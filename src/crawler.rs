//! 爬虫:多 worker 并发 BFS 抓取,链式发现网站
//!
//! - 多并发:可配置 worker 数(默认按 CPU 核数自适应),共享队列
//! - 抓取:手写 HTTP 客户端(http:// 明文;https 跳过)
//! - 解析:<a href> + JS 链接(location/window.open/字符串 URL/变量拼接)提取外链
//! - 链式发现:友链的友链继续解析(BFS),子 URL 独立抓取(不算重复)
//! - robots.txt:每域名检查(Disallow 前缀匹配,缓存);明确禁止的站点不编入
//! - sitemap 优先:种子站先取 sitemap.xml 发现全站 URL
//! - favicon:抓取 favicon.ico 缓存于内存(不落盘),供搜索结果展示
//! - 去重:URL 规范化 + visited;限制:深度/总页数/每域/超时
//! - 存储:抓取页面保存到 out/crawled/<domain>/<path>.html(内存不驻留全文)

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::http::http_get;

pub const CRAWLER_UA: &str = "PilseoCrawler/1.0 (SEO kernel crawler)";
/// favicon 内存缓存上限(条)
const FAVICON_CACHE_MAX: usize = 500;

#[derive(Clone, Debug, Default)]
pub struct CrawlStats {
    pub fetched: usize,
    pub discovered: usize,
    pub failed: usize,
    pub skipped_robots: usize,
    pub elapsed_secs: f64,
}

pub struct Crawler {
    visited: Arc<Mutex<HashSet<String>>>,
    /// domain -> robots Disallow 前缀列表
    robots: Arc<Mutex<HashMap<String, Vec<String>>>>,
    /// 内存 favicon 缓存:domain -> ico 字节(不落盘)
    pub favicons: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// LRU 顺序(用于淘汰 favicon 缓存)
    pub favicon_order: Arc<Mutex<VecDeque<String>>>,
    pub stats: Arc<Mutex<CrawlStats>>,
    max_depth: usize,
    max_pages: usize,
    per_domain: usize,
    timeout_ms: u64,
    workers: Arc<std::sync::atomic::AtomicUsize>,
}

impl Crawler {
    pub fn new(max_depth: usize, max_pages: usize, per_domain: usize, timeout_ms: u64, workers: usize) -> Crawler {
        Crawler::new_shared(max_depth, max_pages, per_domain, timeout_ms, workers, None)
    }

    /// 创建爬虫并共享 stats/favicons(管理面板触发时实时显示进度)
    pub fn new_shared(
        max_depth: usize,
        max_pages: usize,
        per_domain: usize,
        timeout_ms: u64,
        workers: usize,
        shared: Option<(Arc<Mutex<CrawlStats>>, Arc<Mutex<HashMap<String, Vec<u8>>>>, Arc<Mutex<VecDeque<String>>>)>,
    ) -> Crawler {
        // CPU 自适应:默认 worker 数 = 核数(可配置覆盖)
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let (stats, favicons, favicon_order) = match shared {
            Some((s, f, o)) => (s, f, o),
            None => (
                Arc::new(Mutex::new(CrawlStats::default())),
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(Mutex::new(VecDeque::new())),
            ),
        };
        Crawler {
            visited: Arc::new(Mutex::new(HashSet::new())),
            robots: Arc::new(Mutex::new(HashMap::new())),
            favicons,
            favicon_order,
            stats,
            max_depth: max_depth.max(1),
            max_pages: max_pages.max(1).min(200_000),
            per_domain: per_domain.max(1),
            timeout_ms: timeout_ms.max(500),
            workers: Arc::new(std::sync::atomic::AtomicUsize::new(if workers > 0 { workers } else { cores.max(2) })),
        }
    }

    pub fn worker_count(&self) -> usize {
        self.workers.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 运行时可调并发(管理面板控制;0 = CPU 自适应)
    pub fn set_workers(&self, n: usize) {
        let cores = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(4);
        let v = if n > 0 { n.min(128) } else { cores.max(2) };
        self.workers.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// 重置状态(新一轮抓取)
    pub fn reset(&self) {
        self.visited.lock().unwrap().clear();
        self.robots.lock().unwrap().clear();
        *self.stats.lock().unwrap() = CrawlStats::default();
    }

    /// 内存 favicon 查询(搜索结果展示用)
    pub fn favicon(&self, domain: &str) -> Option<Vec<u8>> {
        self.favicons.lock().unwrap().get(domain).cloned()
    }

    /// 抓取 favicon.ico 到内存缓存(懒加载,失败静默)
    pub fn ensure_favicon(&self, domain: &str) {
        {
            let fav = self.favicons.lock().unwrap();
            if fav.contains_key(domain) {
                return;
            }
        }
        let url = format!("http://{}/favicon.ico", domain);
        if let Ok((200, body)) = http_get(&url, 3000, CRAWLER_UA) {
            if !body.is_empty() && body.len() <= 256 * 1024 {
                let mut fav = self.favicons.lock().unwrap();
                if fav.len() >= FAVICON_CACHE_MAX {
                    // LRU 淘汰最旧
                    let mut order = self.favicon_order.lock().unwrap();
                    if let Some(old) = order.pop_front() {
                        fav.remove(&old);
                    }
                }
                fav.insert(domain.to_string(), body.into_bytes());
                self.favicon_order.lock().unwrap().push_back(domain.to_string());
            }
        }
    }

    /// 从种子列表开始多 worker BFS 抓取,返回统计
    pub fn crawl(&self, seeds: &[String], out_dir: &Path) -> CrawlStats {
        self.reset();
        let start = std::time::Instant::now();
        fs::create_dir_all(out_dir).ok();

        // 共享队列与计数
        let queue: Arc<Mutex<VecDeque<(String, usize)>>> = Arc::new(Mutex::new(VecDeque::new()));
        let per_domain_count: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        for s in seeds {
            if let Some(n) = normalize_url(s) {
                let mut v = self.visited.lock().unwrap();
                if !v.contains(&n) {
                    v.insert(n.clone());
                    queue.lock().unwrap().push_back((n, 0));
                }
            }
        }

        // worker 线程共享状态(Arc 克隆)
        let visited = self.visited.clone();
        let robots = self.robots.clone();
        let favicons = self.favicons.clone();
        let favicon_order = self.favicon_order.clone();
        let stats = self.stats.clone();
        let workers_n = self.worker_count();
        let (max_depth, max_pages, per_domain, timeout_ms) = (self.max_depth, self.max_pages, self.per_domain, self.timeout_ms);
        let out_dir_owned = out_dir.to_path_buf();

        let mut handles = Vec::new();
        for _ in 0..workers_n {
            let q = queue.clone();
            let pdc = per_domain_count.clone();
            let visited = visited.clone();
            let robots = robots.clone();
            let favicons = favicons.clone();
            let favicon_order = favicon_order.clone();
            let stats = stats.clone();
            let out = out_dir_owned.clone();
            handles.push(std::thread::spawn(move || {
                worker_loop(
                    q, pdc, visited, robots, favicons, favicon_order, stats,
                    max_depth, max_pages, per_domain, timeout_ms, &out,
                )
            }));
        }
        for h in handles {
            let _ = h.join();
        }

        {
            let mut st = self.stats.lock().unwrap();
            st.elapsed_secs = start.elapsed().as_secs_f64();
        }
        self.stats.lock().unwrap().clone()
    }

    /// robots.txt 检查:返回该 URL 是否允许抓取
    /// (worker 线程内使用内联版 robots_ok,此处保留供单线程调用)
    pub fn robots_allowed(&self, domain: &str, url: &str) -> bool {
        {
            let robots = self.robots.lock().unwrap();
            if let Some(disallows) = robots.get(domain) {
                return disallow_match(disallows, url);
            }
        }
        let robots_url = format!("http://{}/robots.txt", domain);
        let disallows = match http_get(&robots_url, self.timeout_ms, CRAWLER_UA) {
            Ok((200, body)) => parse_robots(&body),
            _ => Vec::new(),
        };
        self.robots.lock().unwrap().insert(domain.to_string(), disallows.clone());
        disallow_match(&disallows, url)
    }
}

/// worker 循环:从共享队列取任务抓取,发现新链接入队
#[allow(clippy::too_many_arguments)]
fn worker_loop(
    queue: Arc<Mutex<VecDeque<(String, usize)>>>,
    per_domain_count: Arc<Mutex<HashMap<String, usize>>>,
    visited: Arc<Mutex<HashSet<String>>>,
    robots: Arc<Mutex<HashMap<String, Vec<String>>>>,
    favicons: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    favicon_order: Arc<Mutex<VecDeque<String>>>,
    stats: Arc<Mutex<CrawlStats>>,
    max_depth: usize,
    max_pages: usize,
    per_domain: usize,
    timeout_ms: u64,
    out_dir: &Path,
) {
    // robots 检查复用 Crawler::robots_allowed?worker 拿到的是 Arc,需要独立的 robots 检查函数
    // 内联实现(与 Crawler::robots_allowed 一致)
    fn robots_ok(robots: &Arc<Mutex<HashMap<String, Vec<String>>>>, domain: &str, url: &str, timeout_ms: u64) -> bool {
        {
            let r = robots.lock().unwrap();
            if let Some(disallows) = r.get(domain) {
                return disallow_match(disallows, url);
            }
        }
        let robots_url = format!("http://{}/robots.txt", domain);
        let disallows = match http_get(&robots_url, timeout_ms, CRAWLER_UA) {
            Ok((200, body)) => parse_robots(&body),
            _ => Vec::new(),
        };
        robots.lock().unwrap().insert(domain.to_string(), disallows.clone());
        disallow_match(&disallows, url)
    }

    // favicon 懒加载(与 Crawler::ensure_favicon 一致)
    fn ensure_fav(domain: &str, favicons: &Arc<Mutex<HashMap<String, Vec<u8>>>>, favicon_order: &Arc<Mutex<VecDeque<String>>>) {
        {
            let fav = favicons.lock().unwrap();
            if fav.contains_key(domain) {
                return;
            }
        }
        let url = format!("http://{}/favicon.ico", domain);
        if let Ok((200, body)) = http_get(&url, 3000, CRAWLER_UA) {
            if !body.is_empty() && body.len() <= 256 * 1024 {
                let mut fav = favicons.lock().unwrap();
                if fav.len() >= FAVICON_CACHE_MAX {
                    let mut order = favicon_order.lock().unwrap();
                    if let Some(old) = order.pop_front() {
                        fav.remove(&old);
                    }
                }
                fav.insert(domain.to_string(), body.into_bytes());
                favicon_order.lock().unwrap().push_back(domain.to_string());
            }
        }
    }

    let mut idle_rounds = 0u32;
    loop {
        let task = queue.lock().unwrap().pop_front();
        let Some((url, depth)) = task else {
            // 队列空:等待其他 worker 可能入队新任务;连续 2 秒空则退出
            idle_rounds += 1;
            if idle_rounds >= 20 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        };
        idle_rounds = 0;
        // 总量限制
        {
            let st = stats.lock().unwrap();
            if st.fetched >= max_pages {
                break;
            }
        }
        // 域名页数限制
        let domain = domain_of(&url).unwrap_or_default();
        {
            let mut pdc = per_domain_count.lock().unwrap();
            let cnt = pdc.entry(domain.clone()).or_insert(0);
            if *cnt >= per_domain {
                continue;
            }
            *cnt += 1;
        }
        // robots 检查(明确禁止的站点不编入,日志显示跳过)
        if !robots_ok(&robots, &domain, &url, timeout_ms) {
            stats.lock().unwrap().skipped_robots += 1;
            crate::logger::push(format!("[crawler] robots 禁止,跳过: {}", url));
            continue;
        }
        // 抓取
        match http_get(&url, timeout_ms, CRAWLER_UA) {
            Ok((status, html)) if status == 200 => {
                let _ = save_page(&url, &html, out_dir);
                stats.lock().unwrap().fetched += 1;
                // favicon 内存缓存(懒加载)
                ensure_fav(&domain, &favicons, &favicon_order);
                // 链式发现链接(友链的友链继续解析),面板日志:XX 发现 YY 链接
                if depth < max_depth {
                    let mut new_links: Vec<String> = Vec::new();
                    for link in extract_links(&html, &url) {
                        stats.lock().unwrap().discovered += 1;
                        new_links.push(link);
                    }
                    if !new_links.is_empty() {
                        let sample: Vec<&str> = new_links.iter().take(5).map(|s| s.as_str()).collect();
                        crate::logger::push(format!(
                            "[crawler] 发现: {} 发现 {} 个链接: {}",
                            domain,
                            new_links.len(),
                            sample.join(" , ")
                        ));
                    }
                    {
                        let mut v = visited.lock().unwrap();
                        let mut q = queue.lock().unwrap();
                        for link in new_links {
                            if !v.contains(&link) {
                                v.insert(link.clone());
                                q.push_back((link, depth + 1));
                            }
                        }
                    }
                }
            }
            Ok((status, _)) if status == 301 || status == 302 => {
                crate::logger::push(format!("[crawler] 重定向跳过: {} -> {}", url, status));
                stats.lock().unwrap().failed += 1;
            }
            Ok((status, _)) => {
                crate::logger::push(format!("[crawler] 非 200 跳过: {} -> {}", url, status));
                stats.lock().unwrap().failed += 1;
            }
            Err(e) => {
                crate::logger::push(format!("[crawler] 抓取失败 {}: {}", url, e));
                stats.lock().unwrap().failed += 1;
            }
        }
    }
}

/// 解析 robots.txt:提取 User-agent 通用段(* 或匹配 UA)的 Disallow 前缀
fn parse_robots(body: &str) -> Vec<String> {
    let mut disallows: Vec<String> = Vec::new();
    let mut in_group = false;
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let lower = t.to_lowercase();
        if lower.starts_with("user-agent:") {
            let ua = lower["user-agent:".len()..].trim();
            in_group = ua == "*" || ua.contains("pilseocrawler") || ua.contains("crawler");
        } else if lower.starts_with("disallow:") && in_group {
            let d = t["disallow:".len()..].trim().to_string();
            if !d.is_empty() {
                disallows.push(d);
            }
        }
    }
    disallows
}

fn disallow_match(disallows: &[String], url: &str) -> bool {
    let path = url_path(url);
    !disallows.iter().any(|d| path.starts_with(d.as_str()))
}

/// 提取 URL 路径(不含 host)
fn url_path(url: &str) -> String {
    match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            match rest.find('/') {
                Some(j) => rest[j..].to_string(),
                None => "/".to_string(),
            }
        }
        None => url.to_string(),
    }
}

/// 域名(含端口,如 "example.com" / "127.0.0.1:8912")
pub fn domain_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let hostport = rest.split('/').next()?;
    if hostport.is_empty() {
        return None;
    }
    Some(hostport.to_lowercase())
}

/// URL 规范化:http 降级、去 fragment、host 小写
fn normalize_url(url: &str) -> Option<String> {
    let mut u = url.trim().to_string();
    let lower_u = u.to_lowercase();
    if lower_u.starts_with("https://") {
        u = format!("http://{}", &u[lower_u.find("://")? + 3..]);
    }
    if !u.starts_with("http://") {
        return None;
    }
    if let Some(i) = u.find('#') {
        u.truncate(i);
    }
    let rest = &u["http://".len()..];
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host_lower = hostport.to_lowercase();
    if host_lower.is_empty() {
        return None;
    }
    let path_clean = if path == "/" { "/" } else { path };
    Some(format!("http://{}{}", host_lower, path_clean))
}

/// 相对 URL 解析
fn resolve_url(base: &str, href: &str) -> Option<String> {
    let h = href.trim();
    if h.is_empty()
        || h.starts_with('#')
        || h.starts_with("javascript:")
        || h.starts_with("mailto:")
        || h.starts_with("tel:")
        || h.starts_with("data:")
        || h.starts_with("about:")
    {
        return None;
    }
    if h.starts_with("http://") || h.starts_with("https://") {
        return normalize_url(h);
    }
    if h.starts_with("//") {
        return normalize_url(&format!("http:{}", h));
    }
    let origin = match base.find("://") {
        Some(i) => {
            let rest = &base[i + 3..];
            match rest.find('/') {
                Some(j) => &base[..i + 3 + j],
                None => base,
            }
        }
        None => return None,
    };
    if h.starts_with('/') {
        return Some(format!("{}{}", origin, h));
    }
    let base_path = url_path(base);
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..i + 1],
        None => "/",
    };
    Some(format!("{}{}{}", origin, dir, h))
}

/// 提取页面链接:<a href> + JS 链接(location/window.open/模板字符串/裸 URL)
pub fn extract_links(html: &str, base: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lower = html.to_lowercase();
    // 1. <a href="..."> / <a href='...'>
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a ") {
        let tag_start = pos + rel;
        let tag_end = lower[tag_start..].find('>').map(|e| tag_start + e).unwrap_or(html.len());
        let tag = &html[tag_start..tag_end];
        for (attr, q) in [("href=\"", '"'), ("href='", '\'')] {
            if let Some(hi) = tag.to_lowercase().find(attr) {
                let hs = hi + attr.len();
                if let Some(he) = tag[hs..].find(q) {
                    let href = &tag[hs..hs + he];
                    if let Some(u) = resolve_url(base, href) {
                        out.push(u);
                    }
                }
            }
        }
        pos = tag_end + 1;
        if pos >= html.len() {
            break;
        }
    }
    // 2. JS 链接:window.open/location.href/裸 http(s):// 字符串(含模板拼接片段)
    let mut scan = 0;
    while let Some(i) = lower[scan..].find("http") {
        let start = scan + i;
        let scheme_end = if lower[start..].starts_with("https://") {
            start + 8
        } else if lower[start..].starts_with("http://") {
            start + 7
        } else {
            scan = start + 4;
            continue;
        };
        let rest = &html[scheme_end..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == '>' || c == '<' || c == ']' || c == '`')
            .unwrap_or(rest.len());
        let url = &html[start..scheme_end + end];
        if let Some(u) = normalize_url(url) {
            out.push(u);
        }
        scan = scheme_end + end;
        if scan >= html.len() {
            break;
        }
    }
    out
}

/// 保存抓取页面到 out/crawled/<domain>/<path>.html
fn save_page(url: &str, html: &str, out_dir: &Path) -> Option<std::path::PathBuf> {
    let domain = domain_of(url)?;
    let safe_domain: String = domain
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    let path = url_path(url);
    let is_root = path == "/" || path.is_empty();
    let safe: String = path
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let file_name = if is_root {
        "index.html".to_string()
    } else if safe.ends_with(".html") {
        safe
    } else {
        format!("{}.html", safe)
    };
    let dir = out_dir.join(&safe_domain);
    fs::create_dir_all(&dir).ok()?;
    let file = dir.join(file_name);
    fs::write(&file, html).ok()?;
    Some(file)
}

/// 解析 SwitchyOmega 规则文件(*.domain 行)为具体域名列表(国内基础名单)
pub fn parse_whitelist(text: &str) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with(';') || t.starts_with('#') || t.starts_with('[') {
            continue;
        }
        if let Some(d) = t.strip_prefix("*.") {
            let d = d.trim().to_lowercase();
            // 排除:TLD 泛化(*.cn/*.中国)、IP 段、通配残留
            if d.is_empty()
                || d.contains('*')
                || d.contains('/')
                || d.matches('.').count() == 0
                || d.chars().all(|c| c.is_ascii_digit() || c == '.')
            {
                continue;
            }
            set.insert(d);
        }
    }
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_resolve() {
        assert_eq!(normalize_url("HTTPS://Example.com/#top").unwrap(), "http://example.com/");
        assert_eq!(normalize_url("http://a.com/path").unwrap(), "http://a.com/path");
        assert!(normalize_url("ftp://x.com").is_none());
        assert_eq!(resolve_url("http://a.com/", "/b.html").unwrap(), "http://a.com/b.html");
        assert_eq!(resolve_url("http://a.com/x/", "b.html").unwrap(), "http://a.com/x/b.html");
        assert_eq!(resolve_url("http://a.com/", "//c.com/d").unwrap(), "http://c.com/d");
        assert!(resolve_url("http://a.com/", "javascript:void(0)").is_none());
        assert!(resolve_url("http://a.com/", "#top").is_none());
    }

    #[test]
    fn extract_links_finds_a_and_js() {
        let html = r#"<html>
          <a href="/page1.html">站内</a>
          <a href='http://friend.com/'>友情链接</a>
          <script>window.open('http://js-site.com/'); location.href = "http://loc-site.com/p"; var u = `http://tmpl-site.com/${id}`;</script>
          <div>文本里的 http://text-url.com/ 链接</div>
        </html>"#;
        let links = extract_links(html, "http://me.com/");
        let joined = links.join(" ");
        assert!(joined.contains("http://me.com/page1.html"), "{}", joined);
        assert!(joined.contains("http://friend.com/"), "{}", joined);
        assert!(joined.contains("http://js-site.com/"), "{}", joined);
        assert!(joined.contains("http://loc-site.com/p"), "{}", joined);
        assert!(joined.contains("http://tmpl-site.com/"), "{}", joined);
        assert!(joined.contains("http://text-url.com/"), "{}", joined);
    }

    #[test]
    fn robots_parse_and_match() {
        let dis = parse_robots("User-agent: *\nDisallow: /admin\nDisallow: /private/\nUser-agent: other\nDisallow: /x\n");
        assert_eq!(dis.len(), 2);
        assert!(disallow_match(&dis, "http://a.com/"), "根路径应允许");
        assert!(!disallow_match(&dis, "http://a.com/admin/panel"), "/admin 应禁止");
        assert!(disallow_match(&dis, "http://a.com/public"), "public 应允许");
    }

    #[test]
    fn whitelist_parse() {
        let text = "; 注释\n[SwitchyOmega Conditions]\n*.cn\n*.10010.com\n*.xn--fiqs8s\n10.*.*.*\n*.0daydown.com\n";
        let domains = parse_whitelist(text);
        assert!(domains.contains(&"10010.com".to_string()));
        assert!(domains.contains(&"0daydown.com".to_string()));
        assert!(!domains.contains(&"cn".to_string()), "TLD 泛化应排除");
        assert!(!domains.contains(&"xn--fiqs8s".to_string()), "TLD 泛化应排除");
        assert!(domains.len() <= 2, "IP 段与 TLD 应排除: {:?}", domains);
    }

    #[test]
    fn domain_keeps_port() {
        assert_eq!(domain_of("http://127.0.0.1:8912/").unwrap(), "127.0.0.1:8912");
        assert_eq!(domain_of("http://example.com/x").unwrap(), "example.com");
    }
}
