//! DNS 探测:手写 DNS 报文构造/解析,零第三方依赖
//!
//! 对一个 FQDN,向多个 DNS 服务器并发发出多种记录类型查询(A/AAAA/NS/CNAME/MX),
//! 根据响应判定:
//!   - 任一 DNS 返回了记录(ANCOUNT>0 或 NOERROR)      => 已注册 Registered
//!   - 所有已响应的 DNS 均为 NXDOMAIN(RCODE=3)          => 可用 Available
//!   - 无任何响应 / 混合 SERVFAIL                        => 无法判定 Error

use std::net::UdpSocket;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// 全局查询 ID 计数器:保证同一 socket 上跨域名的 qid 不重复,
// 防止上一域名迟到的响应被误匹配为当前域名的结果(响应串扰)
static QID_COUNTER: AtomicU16 = AtomicU16::new(0);

pub const QTYPE_A: u16 = 1;
pub const QTYPE_NS: u16 = 2;
pub const QTYPE_CNAME: u16 = 5;
pub const QTYPE_MX: u16 = 15;
pub const QTYPE_TXT: u16 = 16;
pub const QTYPE_AAAA: u16 = 28;

/// 规范化 DNS 服务器地址:补默认端口 53
/// 支持: 8.8.8.8 / 8.8.8.8:5353 / ::1 / [::1]:5353 / dns.example.com
pub fn normalize_dns_servers(servers: &mut Vec<String>) {
    for s in servers.iter_mut() {
        let t = s.trim().to_string();
        if t.starts_with('[') {
            *s = t; // [v6]:port 完整形式
        } else if t.matches(':').count() >= 2 {
            *s = format!("[{}]:53", t); // 裸 IPv6
        } else if !t.contains(':') {
            *s = format!("{}:53", t); // IPv4 / 主机名
        } else {
            *s = t; // ipv4:port
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DnsStatus {
    Registered,
    Available,
    Error,
}

impl DnsStatus {
    pub fn label(&self) -> &'static str {
        match self {
            DnsStatus::Registered => "registered",
            DnsStatus::Available => "available",
            DnsStatus::Error => "error",
        }
    }
}

/// 构造标准 DNS 查询报文(ID + RD 标志 + 单 Question)
fn build_query(fqdn: &str, qtype: u16, id: u16) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(64);
    pkt.extend_from_slice(&id.to_be_bytes()); // ID
    pkt.extend_from_slice(&0x0100u16.to_be_bytes()); // Flags: RD=1
    pkt.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    pkt.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
    pkt.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    pkt.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    // QNAME: 逐 label 编码
    for label in fqdn.split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0); // 根
    pkt.extend_from_slice(&qtype.to_be_bytes());
    pkt.extend_from_slice(&1u16.to_be_bytes()); // QCLASS=IN
    pkt
}

/// 跳过报文中的(可能压缩的)名称,返回新的偏移;失败返回 None
fn skip_name(buf: &[u8], mut off: usize) -> Option<usize> {
    loop {
        let len = *buf.get(off)?;
        if len == 0 {
            return Some(off + 1);
        }
        if len & 0xC0 == 0xC0 {
            // 压缩指针(2 字节),名称到此为止
            return Some(off + 2);
        }
        if len & 0xC0 != 0 {
            return None; // 非法标签
        }
        off += 1 + len as usize;
        if off >= buf.len() {
            return None;
        }
    }
}

/// 解析响应头与 Answer 区:返回 (rcode, ancount, truncated)
fn parse_response(buf: &[u8]) -> Option<(u8, u16, bool)> {
    if buf.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    let rcode = (flags & 0x000F) as u8;
    let truncated = flags & 0x0200 != 0;
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);
    let ancount = u16::from_be_bytes([buf[6], buf[7]]);
    let mut off = 12usize;
    // 跳过 Question 区
    for _ in 0..qdcount {
        off = skip_name(buf, off)?;
        off += 4; // QTYPE + QCLASS
    }
    // 跳过 Answer 区(只需计数,逐个 RR 跳过,含压缩名称与 RDATA)
    for _ in 0..ancount {
        off = skip_name(buf, off)?;
        if off + 10 > buf.len() {
            return None;
        }
        let rdlen = u16::from_be_bytes([buf[off + 8], buf[off + 9]]) as usize;
        off += 10 + rdlen;
        if off > buf.len() {
            return None;
        }
    }
    Some((rcode, ancount, truncated))
}

/// 健康预检:并行探测每个 DNS 的真实递归能力
///
/// 用冷门域名 zzz999.com(几乎必未注册,任何公共 DNS 都需递归解析)实测:
/// 返回 NXDOMAIN(3)或记录(0) => 递归能力正常,保留
/// 超时 / SERVFAIL / REFUSED   => 剔除(只能解析缓存域名的"残废"DNS)
pub fn health_check(dns_servers: &[String], timeout_ms: u64) -> Vec<String> {
    if dns_servers.is_empty() {
        return dns_servers.to_vec();
    }
    let n = dns_servers.len();
    let nthreads = 32usize.min(n);
    let alive: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for t in 0..nthreads {
            let servers = dns_servers;
            let alive = &alive;
            s.spawn(move || {
                let sock = match UdpSocket::bind(("0.0.0.0", 0)) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                for (i, dns) in servers.iter().enumerate() {
                    if i % nthreads != t {
                        continue;
                    }
                    if dns_recursive_ok(&sock, dns, timeout_ms, 0xBEE0 + i as u16) {
                        alive.lock().unwrap().push(dns.clone());
                    }
                }
            });
        }
    });
    let mut v = alive.into_inner().unwrap_or_default();
    if v.is_empty() {
        dns_servers.to_vec()
    } else {
        v.sort();
        v
    }
}

/// 单个 DNS 递归能力探测:查询冷门域名,rcode ∈ {0(有记录), 3(NXDOMAIN)} 即合格
fn dns_recursive_ok(sock: &UdpSocket, dns: &str, timeout_ms: u64, qid: u16) -> bool {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
    let pkt = build_query("zzz999.com", QTYPE_A, qid);
    if sock.send_to(&pkt, dns).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buf = [0u8; 512];
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Some((rcode, _, _)) = parse_response(&buf[..n]) {
                    return rcode == 0 || rcode == 3;
                }
            }
            Err(_) => return false,
        }
    }
    false
}

/// 多 DNS 并发探测一个 FQDN(复用调用方提供的 socket),返回判定结果
///
/// 提前终止优化:
///   - 任一 DNS 返回记录/NOERROR   => 立即判定 Registered,不再等待其余响应
///   - 所有 DNS 均已响应且全 NXDOMAIN(无 SERVFAIL)=> 立即判定 Available
pub fn probe(
    fqdn: &str,
    dns_servers: &[String],
    qtypes: &[u16],
    timeout_ms: u64,
    sock: &UdpSocket,
) -> DnsStatus {
    if dns_servers.is_empty() {
        return DnsStatus::Error;
    }
    let _ = sock.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.saturating_mul(2).max(1500));

    // 清空 socket 中上一轮残留响应(非阻塞),彻底杜绝跨域名串扰
    let _ = sock.set_nonblocking(true);
    let mut drain_buf = [0u8; 2048];
    while sock.recv_from(&mut drain_buf).is_ok() {}
    let _ = sock.set_nonblocking(false);

    // 发出全部查询:(qid -> dns 下标);qid 取自全局计数器,本域名单次内唯一
    let qid_base = QID_COUNTER.fetch_add(64, Ordering::Relaxed);
    let mut sent: Vec<(u16, usize)> = Vec::new();
    let mut seq: u16 = 0;
    for (di, dns) in dns_servers.iter().enumerate() {
        for &qt in qtypes {
            let qid = qid_base.wrapping_add(seq);
            seq = seq.wrapping_add(1);
            let pkt = build_query(fqdn, qt, qid);
            if sock.send_to(&pkt, dns).is_ok() {
                sent.push((qid, di));
            }
        }
    }
    if sent.is_empty() {
        return DnsStatus::Error;
    }

    // 收集响应
    let mut any_record = false; // 任一响应含记录或 NOERROR
    let mut responded = vec![false; dns_servers.len()];
    let mut nxdomain = vec![false; dns_servers.len()];
    let mut got = 0usize;
    let mut buf = [0u8; 4096];
    while got < sent.len() && Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, _src)) => {
                if let Some((rcode, ancount, _tc)) = parse_response(&buf[..n]) {
                    let qid = u16::from_be_bytes([buf[0], buf[1]]);
                    if let Some(pos) = sent.iter().position(|(id, _)| *id == qid) {
                        let (_, di) = sent.remove(pos);
                        got += 1;
                        responded[di] = true;
                        if ancount > 0 || rcode == 0 {
                            any_record = true;
                        }
                        if rcode == 3 {
                            nxdomain[di] = true;
                        }
                    }
                }
                // 提前终止:已有记录
                if any_record {
                    return DnsStatus::Registered;
                }
                // 提前终止:所有 DNS 均已响应且其中有明确 NXDOMAIN
                if responded.iter().all(|&r| r)
                    && responded.iter().zip(nxdomain.iter()).any(|(&r, &n)| r && n)
                {
                    return DnsStatus::Available;
                }
            }
            Err(_) => break, // 超时
        }
    }

    // 判定(务实的 SEO 规则):
    // - 任一 DNS 返回记录/NOERROR        => 已注册
    // - 至少一个 DNS 明确 NXDOMAIN 且无记录 => 可用(SERVFAIL/超时视为无意见,不否决)
    // - 否则(全超时/全 SERVFAIL)         => 无法判定
    if any_record {
        DnsStatus::Registered
    } else {
        let has_nx = responded.iter().zip(nxdomain.iter()).any(|(&r, &n)| r && n);
        if has_nx {
            DnsStatus::Available
        } else {
            DnsStatus::Error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造最小响应报文(1 个 question + 2 个 answer)验证解析
    fn fake_response(id: u16, rcode: u8, ancount: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(&(0x8000u16 | rcode as u16).to_be_bytes()); // QR + rcode
        b.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        b.extend_from_slice(&ancount.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        b.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
        b.extend_from_slice(&[1, b'a', 3, b'c', b'o', b'm', 0]); // a.com
        b.extend_from_slice(&[0, 1, 0, 1]); // QTYPE A, QCLASS IN
        // answer1: 压缩指针 + A 记录
        b.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 2, 3, 4]);
        // answer2: 压缩指针 + AAAA 记录(16 字节 RDATA)
        b.extend_from_slice(&[
            0xC0, 0x0C, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]);
        b
    }

    #[test]
    fn parse_counts_answers() {
        let pkt = fake_response(0x1234, 0, 2);
        assert_eq!(parse_response(&pkt), Some((0, 2, false)));
    }

    #[test]
    fn parse_sees_nxdomain() {
        let pkt = fake_response(0x5678, 3, 0);
        assert_eq!(parse_response(&pkt), Some((3, 0, false)));
    }

    #[test]
    fn normalize_adds_port() {
        let mut servers = vec!["8.8.8.8".into(), "::1".into(), "[::1]:5353".into(), "dns.local".into()];
        normalize_dns_servers(&mut servers);
        assert_eq!(
            servers,
            vec!["8.8.8.8:53", "[::1]:53", "[::1]:5353", "dns.local:53"]
        );
    }

    /// 复用 socket 连续探测多个域名,观察响应率与限流情况(需网络,默认忽略)
    #[test]
    #[ignore]
    fn probe_stress() {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let dns = vec!["8.8.8.8:53".into(), "223.5.5.5:53".into()];
        let qts = vec![1u16, 28, 2];
        let names = ["1.com", "2.com", "3.com", "4.com", "5.com", "6.com", "7.com", "8.com", "9.com", "0.com"];
        for n in names {
            let t0 = Instant::now();
            let st = probe(n, &dns, &qts, 2000, &sock);
            println!("{} -> {:?} ({}ms)", n, st, t0.elapsed().as_millis());
        }
    }

    /// 对未缓存域名连续探测,观察递归查询耗时(需网络,默认忽略)
    #[test]
    #[ignore]
    fn probe_timing() {
        use std::net::UdpSocket;
        let sock = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let dns_all: Vec<String> = [
            "223.5.5.5:53", "119.29.29.29:53", "114.114.114.114:53", "180.76.76.76:53",
            "1.2.4.8:53", "8.8.8.8:53", "1.1.1.1:53", "208.67.222.222:53",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let qts = vec![1u16, 28, 2];
        for round in 0..8 {
            let t0 = Instant::now();
            let st = probe("zzz999.com", &dns_all[..5].to_vec(), &qts, 2000, &sock);
            println!("round {}: {:?} ({}ms)", round, st, t0.elapsed().as_millis());
        }
    }
}
