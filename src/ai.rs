//! AI 接入:OpenAI 兼容 Chat Completions 客户端(零依赖,手写 HTTP)
//!
//! 支持 http:// 明文端点(如本地 llama-server / Ollama / vLLM),
//! 也支持 https 端点需代理转发;默认指向本地 11434(Ollama/llama-server 兼容)

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::json::Json;

#[derive(Clone, Debug)]
pub struct AiConfig {
    pub enabled: bool,
    pub endpoint: String, // 如 http://127.0.0.1:11434/v1/chat/completions
    pub model: String,
    pub api_key: String,
    pub timeout_ms: u64,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            enabled: false,
            endpoint: "http://127.0.0.1:11434/v1/chat/completions".into(),
            model: "qwen3:8b".into(),
            api_key: String::new(),
            timeout_ms: 30000,
        }
    }
}

/// 解析 http://host:port/path
fn parse_http_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("仅支持 http:// 端点: {}", url))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rfind(':') {
        Some(i) => (&hostport[..i], hostport[i + 1..].parse::<u16>().unwrap_or(80)),
        None => (hostport, 80u16),
    };
    Ok((host.to_string(), port, path.to_string()))
}

/// 发送 Chat Completions 请求,返回助手回复文本
pub fn chat_completion(cfg: &AiConfig, system: &str, user: &str) -> Result<String, String> {
    let (host, port, path) = parse_http_url(&cfg.endpoint)?;
    let body = Json::build(vec![
        ("model", Json::str(&cfg.model)),
        (
            "messages",
            Json::arr(vec![
                Json::build(vec![("role", Json::str("system")), ("content", Json::str(system))]),
                Json::build(vec![("role", Json::str("user")), ("content", Json::str(user))]),
            ]),
        ),
        ("stream", Json::Bool(false)),
        ("temperature", Json::num(0.7)),
    ])
    .to_string();

    let mut sock = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("连接 AI 服务 {}:{} 失败: {}", host, port, e))?;
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(cfg.timeout_ms)));
    let _ = sock.set_write_timeout(Some(std::time::Duration::from_millis(cfg.timeout_ms)));

    let mut req = String::new();
    req.push_str(&format!("POST {} HTTP/1.1\r\n", path));
    req.push_str(&format!("Host: {}:{}\r\n", host, port));
    req.push_str("Content-Type: application/json\r\n");
    if !cfg.api_key.is_empty() {
        req.push_str(&format!("Authorization: Bearer {}\r\n", cfg.api_key));
    }
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("Connection: close\r\n\r\n");
    req.push_str(&body);

    sock.write_all(req.as_bytes())
        .map_err(|e| format!("发送 AI 请求失败: {}", e))?;

    let mut resp = Vec::new();
    sock.read_to_end(&mut resp)
        .map_err(|e| format!("读取 AI 响应失败: {}", e))?;
    let text = String::from_utf8_lossy(&resp).into_owned();

    // 分离头与体
    let body_part = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or(&text);
    // 解析 JSON:跳过可能的 chunked 干扰(取第一个完整 JSON)
    let j = crate::json::parse(body_part).map_err(|e| format!("AI 响应解析失败: {} (响应: {})", e, &text[..text.len().min(300)]))?;
    let content = j
        .get("choices")
        .and_then(|c| c.as_arr())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            let err = j
                .get("error")
                .map(|e| e.to_string())
                .unwrap_or_else(|| "未知响应结构".into());
            format!("AI 服务返回错误: {}", err)
        })?;
    Ok(content.trim().to_string())
}
