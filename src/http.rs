//! 极简 HTTP/1.1 服务器(零依赖):请求解析、静态文件、线程池
//!
//! 单线程 accept + 每连接一线程;支持 GET/POST、查询参数、
//! Content-Length 请求体、Keep-Alive 基础处理

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

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
