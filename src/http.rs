//! 极简 HTTP/1.1 服务器(零依赖):请求解析、静态文件、线程池
//!
//! 单线程 accept + 每连接一线程;支持 GET/POST、查询参数、
//! Content-Length 请求体、Keep-Alive 基础处理
//! 另提供 HTTP 客户端 http_get(爬虫用)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// favicon 占位图(SVG 字母图标,内存返回,不落盘)
pub const FAVICON_PLACEHOLDER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="6" fill="#1a73e8"/><text x="16" y="22" font-size="16" font-family="Arial" font-weight="bold" fill="#fff" text-anchor="middle">P</text></svg>"##;

/// 现场生成域名首字母图标(SVG):favicon 抓不到时每个站显示自己的字母图标,
/// 不再千篇一律蓝色 P。背景色由域名 hash 决定,首字母取域名第一个字符大写
pub fn letter_icon_svg(domain: &str) -> String {
    let d = domain.trim();
    let ch = d
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string());
    // 域名 hash 决定背景色(HSL 色相 0-360,饱和度 55%,亮度 42%)
    let mut h: u64 = 0;
    for b in d.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as u64);
    }
    let hue = (h % 360) as u32;
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="6" fill="hsl({},55%,42%)"/><text x="16" y="23" font-size="17" font-family="Arial" font-weight="bold" fill="#fff" text-anchor="middle">{}</text></svg>"##,
        hue, ch
    )
}

/// HTTP 客户端:GET 请求,返回 (状态码, 响应体)。
/// http:// 用内置明文客户端;https:// 走系统 curl(Windows 自带,零第三方依赖)
pub fn http_get(url: &str, timeout_ms: u64, ua: &str) -> Result<(u16, String), String> {
    let (status, _, body) = http_get_full(url, timeout_ms, ua)?;
    Ok((status, body))
}

/// HTTP 客户端:GET 请求,返回 (状态码, 响应头(location 等), 响应体)
pub fn http_get_full(url: &str, timeout_ms: u64, ua: &str) -> Result<(u16, Vec<(String, String)>, String), String> {
    if url.starts_with("https://") {
        return curl_get_full(url, timeout_ms, ua);
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("不支持的地址(仅 http/https): {}", url))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = if path.is_empty() { "/" } else { path };
    let (host, port) = match hostport.rfind(':') {
        Some(i) => (&hostport[..i], hostport[i + 1..].parse::<u16>().unwrap_or(80)),
        None => (hostport, 80u16),
    };
    if host.is_empty() {
        return Err(format!("地址无效: {}", url));
    }
    // 解析全部地址,IPv4 优先逐个尝试(Windows getaddrinfo 常 IPv6 优先,
    // 若 IPv6 不通且只试第一个地址,IPv4 可达也会失败)
    let mut addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS 解析 {} 失败: {}", host, e))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS 解析 {} 无结果", host));
    }
    addrs.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
    // 连接超时收紧(上限 3 秒):不可达站快速失败,不拖慢整批
    let connect_timeout = Duration::from_millis(timeout_ms.min(3000));
    let mut last_err = String::new();
    let mut stream: Option<TcpStream> = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, connect_timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = format!("{}: {}", a, e),
        }
    }
    let mut stream = stream.ok_or_else(|| format!("连接 {}:{} 失败(全部地址不可达): {}", host, port, last_err))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(timeout_ms)))
        .map_err(|e| format!("设置超时失败: {}", e))?;
    stream
        .set_write_timeout(Some(Duration::from_millis(timeout_ms)))
        .ok();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: text/html,application/xhtml+xml,*/*;q=0.8\r\nConnection: close\r\n\r\n",
        path, hostport, ua
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("发送请求失败: {}", e))?;
    stream.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).map_err(|e| format!("读取响应头失败: {}", e))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // 响应头
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| format!("读取响应头失败: {}", e))? == 0 {
            break;
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        let lower = t.to_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().ok();
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        } else if let Some(v) = lower.strip_prefix("location:") {
            headers.push(("location".to_string(), v.trim().to_string()));
        }
    }
    // 响应体(限 8MB)
    let max_body = 8 * 1024 * 1024;
    let mut body = Vec::new();
    if chunked {
        // 分块传输:逐块读取
        loop {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line).map_err(|e| format!("读取分块失败: {}", e))? == 0 {
                break;
            }
            let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            let mut buf = vec![0u8; size];
            reader.read_exact(&mut buf).map_err(|e| format!("读取分块数据失败: {}", e))?;
            body.extend_from_slice(&buf);
            reader.read_line(&mut String::new()).ok(); // 块尾 CRLF
            if body.len() > max_body {
                break;
            }
        }
    } else if let Some(cl) = content_length {
        if cl > max_body {
            return Err(format!("响应体过大: {} 字节", cl));
        }
        let mut buf = vec![0u8; cl];
        reader.read_exact(&mut buf).map_err(|e| format!("读取响应体失败: {}", e))?;
        body = buf;
    } else {
        reader.read_to_end(&mut body).map_err(|e| format!("读取响应体失败: {}", e))?;
        if body.len() > max_body {
            body.truncate(max_body);
        }
    }
    Ok((status, headers, String::from_utf8_lossy(&body).into_owned()))
}

/// https 客户端:调用系统 curl(Windows 10+ 自带,零第三方依赖),
/// -k 忽略证书验证(爬虫抓取公开内容,避免自签/过期证书拦截);
/// 临时文件在 %TEMP%,用完即删(内存优先,不落盘)
fn curl_get_full(url: &str, timeout_ms: u64, ua: &str) -> Result<(u16, Vec<(String, String)>, String), String> {
    let tag = format!("pilseo_curl_{}_{}", std::process::id(), url.len());
    let hf = std::env::temp_dir().join(format!("{}.h", tag));
    let bf = std::env::temp_dir().join(format!("{}.b", tag));
    let secs = (timeout_ms / 1000).max(1).to_string();
    let out = std::process::Command::new("curl")
        .args(["-s", "-k", "--max-time"])
        .arg(&secs)
        .args(["-A", ua, "-D"])
        .arg(&hf)
        .args(["-o"])
        .arg(&bf)
        .args(["-w", "%{http_code}"])
        .arg(url)
        .output()
        .map_err(|e| format!("调用 curl 失败(https 需系统自带 curl): {}", e))?;
    let code: u16 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
    let body = std::fs::read(&bf)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Ok(h) = std::fs::read_to_string(&hf) {
        for line in h.lines() {
            if let Some(vi) = line.find(':') {
                let key = &line[..vi];
                if key.eq_ignore_ascii_case("location") {
                    headers.push(("location".to_string(), line[vi + 1..].trim().to_string()));
                }
            }
        }
    }
    let _ = std::fs::remove_file(&hf);
    let _ = std::fs::remove_file(&bf);
    if code == 0 {
        // 带上 curl 的具体错误(拒绝/超时/DNS 等),便于诊断
        let err_detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if err_detail.is_empty() { "未知错误".to_string() } else { err_detail };
        return Err(format!("curl 请求失败 {}: {}", url, detail));
    }
    Ok((code, headers, body))
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // headers/body 供 POST 扩展使用
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    /// 查询参数(URL 解码)
    pub fn param(&self, key: &str) -> Option<&str> {
        self.query.get(key).map(|s| s.as_str())
    }

    /// 请求头(小写键)
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers.get(&key.to_lowercase()).map(|s| s.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub extra_headers: Vec<(String, String)>,
}

impl Response {
    pub fn json(status: u16, body: &str) -> Response {
        Response {
            status,
            content_type: "application/json; charset=utf-8",
            body: body.as_bytes().to_vec(),
            extra_headers: vec![],
        }
    }
    pub fn html(status: u16, body: &str) -> Response {
        Response {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
            extra_headers: vec![],
        }
    }
    pub fn text(status: u16, body: &str) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
            extra_headers: vec![],
        }
    }
    pub fn not_found() -> Response {
        Response::text(404, "404 Not Found")
    }
}

/// URL 百分号解码
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(qs: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = url_decode(it.next().unwrap_or(""));
        let v = url_decode(it.next().unwrap_or(""));
        map.insert(k, v);
    }
    map
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    // 请求行
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    // 拆分 path 与 query
    let (path, qs) = match target.find('?') {
        Some(idx) => (&target[..idx], &target[idx + 1..]),
        None => (target.as_str(), ""),
    };
    let query = parse_query(qs);
    // 请求头
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some(idx) = h.find(':') {
            let k = h[..idx].trim().to_lowercase();
            let v = h[idx + 1..].trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
            headers.insert(k, v);
        }
    }
    // 请求体
    let mut body = Vec::new();
    if content_length > 0 && content_length <= 16 * 1024 * 1024 {
        let mut buf = vec![0u8; content_length];
        reader.read_exact(&mut buf).ok()?;
        body = buf;
    }
    Some(Request { method, path: path.to_string(), query, headers, body })
}

fn write_response(stream: &mut TcpStream, resp: &Response) {
    let reason = match resp.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n",
        resp.status,
        reason,
        resp.content_type,
        resp.body.len()
    );
    for (k, v) in &resp.extra_headers {
        head.push_str(&format!("{}: {}\r\n", k, v));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&resp.body);
    let _ = stream.flush();
}

/// 启动 HTTP 服务器,每连接一线程
pub fn serve(addr: &str, handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Result<(), String> {
    let listener = TcpListener::bind(addr).map_err(|e| format!("绑定 {} 失败: {}", addr, e))?;
    println!("[serve] HTTP 服务已启动: http://{}", addr);
    let handler: Arc<dyn Fn(&Request) -> Response + Send + Sync> = Arc::new(handler);
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let h = handler.clone();
                std::thread::spawn(move || {
                    if let Some(req) = read_request(&mut stream) {
                        let resp = h(&req);
                        write_response(&mut stream, &resp);
                    }
                });
            }
            Err(_) => continue,
        }
    }
    Ok(())
}
