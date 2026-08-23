//! 并发引擎:有界队列(Condvar 背压)生产者-消费者模型
//!
//! - 主线程 = 生产者:穷举器按序生成域名主体 x 后缀,送入有界队列
//! - worker 池 = 消费者:多线程并发 DNS 探测;探测为"可用"的域名就地并发建站
//! - 主线程同时回收结果:写结果文件 + 统计 + 进度

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use crate::config::Config;
use crate::dns::{self, DnsStatus};
use crate::enumerate::Enumerator;
use crate::site::SiteBuilder;

/// 极简伪随机数生成器(LCG,线程内使用,零依赖)
struct Lcg(u64);

impl Lcg {
    fn new() -> Self {
        static SEED_CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x9E37_79B9_7F4A_7C15);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let c = SEED_CTR.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        Lcg((t ^ c).max(1))
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// 从 DNS 列表中随机选取 k 个不重复项(部分 Fisher-Yates 洗牌)
fn pick_random<'a>(list: &'a [String], k: usize, rng: &mut Lcg) -> Vec<&'a str> {
    let n = list.len();
    let k = k.min(n);
    let mut idx: Vec<usize> = (0..n).collect();
    for i in 0..k {
        let j = i + (rng.next() as usize % (n - i));
        idx.swap(i, j);
    }
    idx[..k].iter().map(|&i| list[i].as_str()).collect()
}

/// 有界多消费者任务队列(生产快于消费时自动背压)
struct WorkQueue {
    queue: Mutex<VecDeque<String>>,
    cond: Condvar,
    cap: usize,
    done: AtomicBool,
}

impl WorkQueue {
    fn new(cap: usize) -> Self {
        WorkQueue {
            queue: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
            cap: cap.max(1),
            done: AtomicBool::new(false),
        }
    }

    fn push(&self, s: String) {
        let mut q = self.queue.lock().unwrap();
        while q.len() >= self.cap {
            q = self.cond.wait(q).unwrap();
        }
        q.push_back(s);
        self.cond.notify_one();
    }

    fn pop(&self) -> Option<String> {
        let mut q = self.queue.lock().unwrap();
        loop {
            if let Some(s) = q.pop_front() {
                self.cond.notify_one(); // 唤醒等待空间的 push
                return Some(s);
            }
            if self.done.load(Ordering::Acquire) {
                return None;
            }
            q = self.cond.wait(q).unwrap();
        }
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.cond.notify_all();
    }
}

#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub total: u64,
    pub registered: u64,
    pub available: u64,
    pub errors: u64,
    pub skipped: u64,
    pub sites: u64,
    pub elapsed: f64,
}

pub struct Engine {
    cfg: Config,
    dns_servers: Vec<String>,
    tlds: Vec<String>,
    blocks: Vec<String>,
}

impl Engine {
    pub fn load(cfg: Config) -> Result<Self, String> {
        let mut dns_servers = crate::config::load_space_list(&cfg.dns_file)?;
        if dns_servers.is_empty() {
            return Err(format!("DNS 列表为空: {}", cfg.dns_file.display()));
        }
        dns::normalize_dns_servers(&mut dns_servers);
        // 健康预检:剔除不可达/慢速 DNS
        let healthy = dns::health_check(&dns_servers, 1500);
        if healthy.len() < dns_servers.len() {
            println!(
                "[信息] DNS 健康预检:剔除 {} 个不可达服务器,保留 {} 个",
                dns_servers.len() - healthy.len(),
                healthy.len()
            );
        }
        let dns_servers = healthy;
        let tlds = crate::config::load_space_list(&cfg.tld_file)?;
        if tlds.is_empty() {
            return Err(format!("后缀列表为空: {}", cfg.tld_file.display()));
        }
        let blocks = crate::config::load_block_list(&cfg.block_file)?;
        Ok(Engine { cfg, dns_servers, tlds, blocks })
    }

    fn is_blocked(&self, fqdn: &str) -> bool {
        self.blocks.iter().any(|b| fqdn.contains(b.as_str()))
    }

    /// 运行参数摘要(供启动时打印)
    pub fn summary(&self) -> String {
        format!(
            "字符集={} 位数=[{},{}] 后缀={} DNS={}(每域名查{}) workers={} 穷举域名总数={}",
            self.cfg.charset,
            self.cfg.min_len,
            self.cfg.max_len,
            self.tlds.len(),
            self.dns_servers.len(),
            self.cfg.dns_per_domain,
            self.cfg.workers,
            self.grand_total()
        )
    }

    /// 穷举任务总数(字符集组合数 x 后缀数),供进度展示
    pub fn grand_total(&self) -> u128 {
        let en = Enumerator::new(&self.cfg.charset, self.cfg.min_len, self.cfg.max_len);
        en.total().saturating_mul(self.tlds.len() as u128)
    }

    /// 运行穷举引擎。progress 回调(可选)在运行中周期性收到当前统计,供进度展示
    pub fn run(&self, dry_run: bool, mut progress: Option<&mut dyn FnMut(&Stats)>) -> Result<Stats, String> {
        if dry_run {
            return self.run_dry();
        }
        let start = Instant::now();
        fs::create_dir_all(&self.cfg.out_dir)
            .map_err(|e| format!("创建输出目录失败 {}: {}", self.cfg.out_dir.display(), e))?;

        // 结果文件(覆盖旧结果)
        let mut avail_f = open_out(&self.cfg.out_dir, "available.txt")?;
        let mut reg_f = open_out(&self.cfg.out_dir, "registered.txt")?;
        let mut err_f = open_out(&self.cfg.out_dir, "errors.txt")?;
        let mut unc_f = open_out(&self.cfg.out_dir, "uncertain.txt")?;

        // 建站器(跨线程共享,关键词轮流)
        let keywords = crate::config::load_space_list(&self.cfg.keywords_file).unwrap_or_default();
        let site_builder = Arc::new(SiteBuilder::new(keywords, self.cfg.out_dir.clone()));

        let (res_tx, res_rx) = mpsc::channel::<(String, DnsStatus, Option<String>)>();

        let work_queue = Arc::new(WorkQueue::new(self.cfg.workers.saturating_mul(4)));
        let mut workers = Vec::new();
        for _ in 0..self.cfg.workers {
            let wq = work_queue.clone();
            let tx = res_tx.clone();
            let dns_list = self.dns_servers.clone();
            let qts = self.cfg.qtypes.clone();
            let tmo = self.cfg.dns_timeout_ms;
            let sb = site_builder.clone();
            let build = self.cfg.build_sites;
            let per = self.cfg.dns_per_domain.min(dns_list.len()).max(1);
            workers.push(thread::spawn(move || {
                // 每 worker 复用单个 UDP socket(避免每域名创建/销毁的开销)
                let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) else {
                    return;
                };
                // 随机负载均衡:每域名从 DNS 大名单随机抽 per 个,分摊负载防限流
                let mut rng = Lcg::new();
                while let Some(fqdn) = wq.pop() {
                    let subset: Vec<String> =
                        pick_random(&dns_list, per, &mut rng).into_iter().map(String::from).collect();
                    let st = dns::probe(&fqdn, &subset, &qts, tmo, &sock);
                    let st = if st == DnsStatus::Error {
                        // 重新随机抽一组 DNS 重试一次(兜底全超时/全 SERVFAIL 的域名)
                        let subset2: Vec<String> =
                            pick_random(&dns_list, per, &mut rng).into_iter().map(String::from).collect();
                        dns::probe(&fqdn, &subset2, &qts, tmo, &sock)
                    } else {
                        st
                    };
                    // 建站策略:明确已注册(有记录)才不建;NXDOMAIN 与无法判定的都建
                    // (穷举域名绝大多数本就不存在,超时/失败大概率仍可用,注册前再确认)
                    let site = if st != DnsStatus::Registered && build {
                        sb.build(&fqdn).ok()
                    } else {
                        None
                    };
                    let _ = tx.send((fqdn, st, site));
                }
            }));
        }
        drop(res_tx); // 回收端仅主线程持有

        // ---- 生产者:穷举 + 按后缀顺序组合,送入有界队列 ----
        let mut stats = Stats::default();
        let mut last_print = Instant::now();
        let mut en = Enumerator::new(&self.cfg.charset, self.cfg.min_len, self.cfg.max_len);
        let grand_total = en.total().saturating_mul(self.tlds.len() as u128);

        while let Some(name) = en.next() {
            for raw_tld in &self.tlds {
                let tld = raw_tld.trim_start_matches('.');
                let fqdn = format!("{}.{}", name, tld);
                if self.is_blocked(&fqdn) {
                    stats.skipped += 1;
                    continue;
                }
                work_queue.push(fqdn);
                stats.total += 1;
                drain_results(&res_rx, &mut stats, &mut avail_f, &mut reg_f, &mut err_f, &mut unc_f);
                if last_print.elapsed().as_secs() >= 3 {
                    let pct = if grand_total > 0 {
                        stats.total as f64 / grand_total as f64 * 100.0
                    } else {
                        0.0
                    };
                    println!(
                        "[进度] 已处理 {}/{} ({:.2}%) 可用={} 已注册={} 失败={} 已建站={} 耗时={:.0}s",
                        stats.total,
                        grand_total,
                        pct,
                        stats.available,
                        stats.registered,
                        stats.errors,
                        stats.sites,
                        start.elapsed().as_secs_f64()
                    );
                    if let Some(cb) = progress.as_mut() {
                        cb(&stats);
                    }
                    last_print = Instant::now();
                }
            }
        }
        work_queue.finish(); // 通知 worker 收工(队列清空后自然退出)

        // ---- 回收剩余结果 ----
        for handle in workers {
            let _ = handle.join();
        }
        drain_results(&res_rx, &mut stats, &mut avail_f, &mut reg_f, &mut err_f, &mut unc_f);

        let _ = avail_f.flush();
        let _ = reg_f.flush();
        let _ = err_f.flush();
        let _ = unc_f.flush();

        stats.elapsed = start.elapsed().as_secs_f64();
        Ok(stats)
    }

    /// 干跑:不建 worker 池,主线程按穷举顺序直接写入 dryrun.txt(顺序与穷举一致)
    fn run_dry(&self) -> Result<Stats, String> {
        let start = Instant::now();
        fs::create_dir_all(&self.cfg.out_dir)
            .map_err(|e| format!("创建输出目录失败 {}: {}", self.cfg.out_dir.display(), e))?;
        let mut f = open_out(&self.cfg.out_dir, "dryrun.txt")?;
        let mut stats = Stats::default();
        let mut en = Enumerator::new(&self.cfg.charset, self.cfg.min_len, self.cfg.max_len);
        while let Some(name) = en.next() {
            for raw_tld in &self.tlds {
                let tld = raw_tld.trim_start_matches('.');
                let fqdn = format!("{}.{}", name, tld);
                if self.is_blocked(&fqdn) {
                    stats.skipped += 1;
                    continue;
                }
                writeln!(f, "{}", fqdn).ok();
                stats.total += 1;
            }
        }
        let _ = f.flush();
        stats.available = stats.total;
        stats.elapsed = start.elapsed().as_secs_f64();
        Ok(stats)
    }
}

fn open_out(out_dir: &std::path::Path, name: &str) -> Result<File, String> {
    File::create(out_dir.join(name)).map_err(|e| format!("创建 {} 失败: {}", name, e))
}

fn drain_results(
    rx: &Receiver<(String, DnsStatus, Option<String>)>,
    stats: &mut Stats,
    avail_f: &mut File,
    reg_f: &mut File,
    err_f: &mut File,
    unc_f: &mut File,
) {
    while let Ok((fqdn, st, site_kw)) = rx.try_recv() {
        match st {
            DnsStatus::Registered => {
                stats.registered += 1;
                writeln!(reg_f, "{}", fqdn).ok();
            }
            DnsStatus::Available => {
                stats.available += 1;
                writeln!(avail_f, "{}", fqdn).ok();
                if site_kw.is_some() {
                    stats.sites += 1;
                }
            }
            DnsStatus::Error => {
                stats.errors += 1;
                // 无法判定:大概率仍可用(穷举域名大多不存在),同时进 available 与 uncertain 供追溯
                writeln!(avail_f, "{}", fqdn).ok();
                writeln!(err_f, "{}", fqdn).ok();
                writeln!(unc_f, "{}", fqdn).ok();
                if site_kw.is_some() {
                    stats.sites += 1;
                }
            }
        }
    }
}
