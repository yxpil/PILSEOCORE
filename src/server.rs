//! API 服务:HTTP 路由 + 静态 Web UI + API 文档
//!
//! 路由:
//!   GET /                  Web UI(Google 风格)
//!   GET /api/status        引擎状态
//!   GET /api/stats         索引统计
//!   GET /api/search?q=&limit=&ai=  搜索(可选 AI 摘要)
//!   GET /api/suggest?q=    联想建议
//!   GET /api/sitemap?domain=  站点地图
//!   GET /api/rebuild       重建索引
//!   GET /api/docs          API 文档

use std::sync::Arc;
use std::time::Instant;

use crate::ai::AiConfig;
use crate::http::{Request, Response};
use crate::json::Json;
use crate::search::SearchEngine;

const WEB_UI: &str = include_str!("../web/index.html");
const API_DOCS: &str = include_str!("../web/api_docs.html");

pub fn handle(engine: &Arc<SearchEngine>, ai: &AiConfig, req: &Request) -> Response {
    let path = req.path.as_str();
    match (req.method.as_str(), path) {
        ("GET", "/") => Response::html(200, WEB_UI),
        ("GET", "/api/docs") => Response::html(200, API_DOCS),
        ("GET", "/api/status") => api_status(engine),
        ("GET", "/api/stats") => api_stats(engine),
        ("GET", "/api/search") => api_search(engine, ai, req),
        ("GET", "/api/suggest") => api_suggest(engine, req),
        ("GET", "/api/sitemap") => api_sitemap(engine, req),
        ("GET", "/api/rebuild") => api_rebuild(engine),
        _ => Response::not_found(),
    }
}

fn api_status(engine: &Arc<SearchEngine>) -> Response {
    let idx = engine.index().lock().unwrap();
    let (sites, terms, blocks) = idx.stats();
    let loaded = idx.loaded_blocks();
    drop(idx);
    let (hits, misses) = engine.cache_stats();
    let j = Json::build(vec![
        ("status", Json::str("ok")),
        ("name", Json::str("PILSEOCORE Local Search")),
        ("version", Json::str(env!("CARGO_PKG_VERSION"))),
        ("sites", Json::num(sites as f64)),
        ("terms", Json::num(terms as f64)),
        ("blocks", Json::num(blocks as f64)),
        ("loaded_blocks", Json::num(loaded as f64)),
        ("cache_hits", Json::num(hits as f64)),
        ("cache_misses", Json::num(misses as f64)),
        ("cache_hit_rate", Json::num(if hits + misses > 0 { hits as f64 / (hits + misses) as f64 } else { 0.0 })),
    ]);
    Response::json(200, &j.to_string())
}

fn api_stats(engine: &Arc<SearchEngine>) -> Response {
    let idx = engine.index().lock().unwrap();
    let (sites, terms, blocks) = idx.stats();
    let docs: Vec<Json> = idx
        .docs
        .iter()
        .map(|d| {
            Json::build(vec![
                ("domain", Json::str(&d.domain)),
                ("title", Json::str(&d.title)),
                ("url", Json::str(&d.url)),
            ])
        })
        .collect();
    drop(idx);
    let j = Json::build(vec![
        ("sites", Json::num(sites as f64)),
        ("terms", Json::num(terms as f64)),
        ("blocks", Json::num(blocks as f64)),
        ("docs", Json::arr(docs)),
    ]);
    Response::json(200, &j.to_string())
}

fn api_search(engine: &Arc<SearchEngine>, ai: &AiConfig, req: &Request) -> Response {
    let q = req.param("q").unwrap_or("").trim().to_string();
    let limit = req.param("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(10).min(100);
    let want_ai = req.param("ai").map(|v| v == "1" || v == "true").unwrap_or(false);
    if q.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 q"}"#);
    }
    let start = Instant::now();
    let (total, hits) = engine.search(&q, limit);
    let elapsed_ms = start.elapsed().as_millis();

    let results: Vec<Json> = hits
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

    let mut pairs: Vec<(&str, Json)> = vec![
        ("query", Json::str(&q)),
        ("time_ms", Json::num(elapsed_ms as f64)),
        ("total", Json::num(total as f64)),
        ("results", Json::arr(results)),
    ];

    // 可选 AI 摘要
    if want_ai && ai.enabled {
        let ctx = build_ai_context(&q, &hits);
        let sys = "你是一个本地搜索引擎助手。基于给定的搜索结果,用简洁中文回答用户问题;若结果不足以回答,如实说明。";
        match crate::ai::chat_completion(ai, sys, &ctx) {
            Ok(summary) => pairs.push(("ai_summary", Json::str(&summary))),
            Err(e) => pairs.push(("ai_error", Json::str(&e))),
        }
    }

    Response::json(200, &Json::build(pairs).to_string())
}

fn build_ai_context(q: &str, hits: &[crate::search::SearchHit]) -> String {
    let mut s = format!("用户查询: {}\n本地搜索结果:\n", q);
    for (i, h) in hits.iter().enumerate() {
        s.push_str(&format!("{}. {} ({})\n   {}\n", i + 1, h.title, h.url, h.description));
    }
    s
}

fn api_suggest(engine: &Arc<SearchEngine>, req: &Request) -> Response {
    let q = req.param("q").unwrap_or("").trim().to_string();
    let limit = req.param("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(10).min(50);
    if q.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 q"}"#);
    }
    let words = engine.suggest(&q, limit);
    let j = Json::build(vec![
        ("query", Json::str(&q)),
        ("suggestions", Json::arr(words.into_iter().map(Json::str).collect())),
    ]);
    Response::json(200, &j.to_string())
}

fn api_sitemap(engine: &Arc<SearchEngine>, req: &Request) -> Response {
    let domain = req.param("domain").unwrap_or("").trim().to_string();
    if domain.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 domain"}"#);
    }
    // 从索引找该站点,返回其 sitemap 入口;本地站 sitemap.xml 由索引器生成
    let site_dir = std::path::Path::new("out/sites").join(&domain);
    let sitemap_path = site_dir.join("sitemap.xml");
    let mut urls = vec![format!("https://{}/", domain)];
    if let Ok(xml) = std::fs::read_to_string(&sitemap_path) {
        for line in xml.lines() {
            let t = line.trim();
            if let Some(inner) = t.strip_prefix("<loc>").and_then(|s| s.strip_suffix("</loc>")) {
                urls.push(inner.to_string());
            }
        }
    }
    urls.sort();
    urls.dedup();
    let _ = engine; // 域名列表来自文件系统
    let j = Json::build(vec![
        ("domain", Json::str(&domain)),
        ("sitemap_url", Json::str(&format!("/out/sites/{}/sitemap.xml", domain))),
        ("urls", Json::arr(urls.into_iter().map(Json::str).collect())),
    ]);
    Response::json(200, &j.to_string())
}

fn api_rebuild(engine: &Arc<SearchEngine>) -> Response {
    match engine.rebuild(
        &std::path::Path::new("out/sites"),
        &std::path::Path::new("data/index"),
    ) {
        Ok(n) => {
            let j = Json::build(vec![
                ("status", Json::str("ok")),
                ("sites", Json::num(n as f64)),
            ]);
            Response::json(200, &j.to_string())
        }
        Err(e) => Response::json(500, &Json::build(vec![("status", Json::str("error")), ("message", Json::str(&e))]).to_string()),
    }
}
