//! MCP(Model Context Protocol)Server:JSON-RPC 2.0 over stdio
//!
//! 协议:MCP stdio 传输(LSP 风格帧: Content-Length 头 + JSON 体)
//! 工具: search / suggest / status / sitemap / rebuild

use std::io::{BufRead, BufReader, Write};

use crate::json::Json;
use crate::search::SearchEngine;

const SERVER_NAME: &str = "pilseocore-mcp";

/// 从 stdin 读取一帧(LSP 风格)
fn read_frame(reader: &mut impl BufRead) -> Option<String> {
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok()?;
        }
    }
    if content_length == 0 {
        return None;
    }
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// 写一帧到 stdout
fn write_frame(stream: &mut impl Write, json: &str) {
    let _ = write!(stream, "Content-Length: {}\r\n\r\n{}", json.len(), json);
    let _ = stream.flush();
}

fn rpc_response(id: &Json, result: Json) -> Json {
    Json::build(vec![
        ("jsonrpc", Json::str("2.0")),
        ("id", id.clone()),
        ("result", result),
    ])
}

fn rpc_error(id: &Json, code: i64, message: &str) -> Json {
    Json::build(vec![
        ("jsonrpc", Json::str("2.0")),
        ("id", id.clone()),
        (
            "error",
            Json::build(vec![("code", Json::num(code as f64)), ("message", Json::str(message))]),
        ),
    ])
}

fn tool_definitions() -> Json {
    let tools = vec![
        Json::build(vec![
            ("name", Json::str("search")),
            ("description", Json::str("在本地搜索引擎中搜索站点,支持模糊匹配;返回标题/URL/描述/评分")),
            (
                "inputSchema",
                Json::build(vec![
                    ("type", Json::str("object")),
                    (
                        "properties",
                        Json::build(vec![
                            (
                                "query",
                                Json::build(vec![("type", Json::str("string")), ("description", Json::str("搜索关键词"))]),
                            ),
                            (
                                "limit",
                                Json::build(vec![("type", Json::str("number")), ("description", Json::str("返回条数,默认 10"))]),
                            ),
                        ]),
                    ),
                    (
                        "required",
                        Json::arr(vec![Json::str("query")]),
                    ),
                ]),
            ),
        ]),
        Json::build(vec![
            ("name", Json::str("suggest")),
            ("description", Json::str("搜索联想:输入前缀返回建议词")),
            (
                "inputSchema",
                Json::build(vec![
                    ("type", Json::str("object")),
                    (
                        "properties",
                        Json::build(vec![
                            (
                                "query",
                                Json::build(vec![("type", Json::str("string")), ("description", Json::str("前缀"))]),
                            ),
                            (
                                "limit",
                                Json::build(vec![("type", Json::str("number")), ("description", Json::str("返回条数,默认 10"))]),
                            ),
                        ]),
                    ),
                    ("required", Json::arr(vec![Json::str("query")])),
                ]),
            ),
        ]),
        Json::build(vec![
            ("name", Json::str("status")),
            ("description", Json::str("引擎状态:索引站点数/词数/分块数/缓存命中")),
            ("inputSchema", Json::build(vec![("type", Json::str("object")), ("properties", Json::Obj(Default::default()))])),
        ]),
        Json::build(vec![
            ("name", Json::str("sitemap")),
            ("description", Json::str("查看指定域名的站点地图(URL 列表)")),
            (
                "inputSchema",
                Json::build(vec![
                    ("type", Json::str("object")),
                    (
                        "properties",
                        Json::build(vec![(
                            "domain",
                            Json::build(vec![
                                ("type", Json::str("string")),
                                ("description", Json::str("域名,如 abc.com")),
                            ]),
                        )]),
                    ),
                    ("required", Json::arr(vec![Json::str("domain")])),
                ]),
            ),
        ]),
        Json::build(vec![
            ("name", Json::str("rebuild")),
            ("description", Json::str("重新扫描站点目录并重建索引(返回站点数)")),
            ("inputSchema", Json::build(vec![("type", Json::str("object")), ("properties", Json::Obj(Default::default()))])),
        ]),
    ];
    Json::build(vec![("tools", Json::arr(tools))])
}

fn call_tool(engine: &SearchEngine, name: &str, args: &Json) -> Json {
    let text = match name {
        "search" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let (total, hits) = engine.search(q, limit);
            let arr: Vec<Json> = hits
                .iter()
                .map(|h| {
                    Json::build(vec![
                        ("title", Json::str(&h.title)),
                        ("url", Json::str(&h.url)),
                        ("domain", Json::str(&h.domain)),
                        ("description", Json::str(&h.description)),
                        ("score", Json::num(h.score)),
                    ])
                })
                .collect();
            Json::build(vec![("count", Json::num(total as f64)), ("results", Json::arr(arr))]).to_string()
        }
        "suggest" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let words = engine.suggest(q, limit);
            Json::build(vec![
                ("query", Json::str(q)),
                ("suggestions", Json::arr(words.into_iter().map(Json::str).collect())),
            ])
            .to_string()
        }
        "status" => {
            let idx = engine.index().lock().unwrap();
            let (sites, terms, blocks) = idx.stats();
            let loaded = idx.loaded_blocks();
            drop(idx);
            let (hits, misses) = engine.cache_stats();
            Json::build(vec![
                ("sites", Json::num(sites as f64)),
                ("terms", Json::num(terms as f64)),
                ("blocks", Json::num(blocks as f64)),
                ("loaded_blocks", Json::num(loaded as f64)),
                ("cache_hits", Json::num(hits as f64)),
                ("cache_misses", Json::num(misses as f64)),
            ])
            .to_string()
        }
        "sitemap" => {
            let domain = args.get("domain").and_then(|v| v.as_str()).unwrap_or("");
            let url = format!("https://{}/", domain);
            let urls = vec![Json::str(url)];
            Json::build(vec![("domain", Json::str(domain)), ("urls", Json::arr(urls))]).to_string()
        }
        "rebuild" => match engine.rebuild(
            &std::path::Path::new("out/sites"),
            &std::path::Path::new("data/index"),
        ) {
            Ok(n) => Json::build(vec![("sites", Json::num(n as f64)), ("status", Json::str("ok"))]).to_string(),
            Err(e) => Json::build(vec![("status", Json::str("error")), ("message", Json::str(&e))]).to_string(),
        },
        _ => {
            return Json::build(vec![
                ("content", Json::arr(vec![Json::build(vec![
                    ("type", Json::str("text")),
                    ("text", Json::str(format!("未知工具: {}", name))),
                ])])),
                ("isError", Json::Bool(true)),
            ])
        }
    };
    Json::build(vec![(
        "content",
        Json::arr(vec![Json::build(vec![("type", Json::str("text")), ("text", Json::str(text))])]),
    )])
}

/// 运行 MCP 服务器(stdio),阻塞直到 stdin 关闭
pub fn serve(engine: &SearchEngine) -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    while let Some(frame) = read_frame(&mut reader) {
        let msg = match crate::json::parse(&frame) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let id = msg.get("id").cloned().unwrap_or(Json::Null);
        let is_notification = msg.get("id").is_none();

        match method.as_str() {
            "initialize" => {
                let result = Json::build(vec![
                    (
                        "protocolVersion",
                        msg.get("protocolVersion").cloned().unwrap_or(Json::str("2024-11-05")),
                    ),
                    (
                        "capabilities",
                        Json::build(vec![("tools", Json::build(vec![("listChanged", Json::Bool(false))]))]),
                    ),
                    (
                        "serverInfo",
                        Json::build(vec![
                            ("name", Json::str(SERVER_NAME)),
                            ("version", Json::str(env!("CARGO_PKG_VERSION"))),
                        ]),
                    ),
                ]);
                write_frame(&mut writer, &rpc_response(&id, result).to_string());
            }
            "notifications/initialized" | "notifications/cancelled" => {
                // 通知:无需响应
            }
            "ping" => {
                write_frame(&mut writer, &rpc_response(&id, Json::build(vec![])).to_string());
            }
            "tools/list" => {
                write_frame(&mut writer, &rpc_response(&id, tool_definitions()).to_string());
            }
            "tools/call" => {
                let name = msg.get("params").and_then(|p| p.get("name")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let args = msg.get("params").and_then(|p| p.get("arguments")).cloned().unwrap_or(Json::obj());
                let result = call_tool(engine, &name, &args);
                write_frame(&mut writer, &rpc_response(&id, result).to_string());
            }
            _ => {
                if !is_notification {
                    write_frame(&mut writer, &rpc_error(&id, -32601, &format!("未知方法: {}", method)).to_string());
                }
            }
        }
    }
    Ok(())
}
