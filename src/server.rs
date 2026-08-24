//! API 服务:HTTP 路由 + 权限控制 + Web UI + API 文档
//!
//! 角色与认证:
//!   普通用户(无凭证)      -> 只读:搜索/联想/状态/站点地图/文档/UI
//!   管理员:
//!     - Web UI 登录:账号密码(admin_user/admin_pass) -> 会话 token
//!     - API / MCP:管理员签发的 token(Authorization: Bearer <token>)
//!
//! 路由:
//!   GET  /                          Web UI(Google 风格)
//!   GET  /api/docs                  API 文档
//!   GET  /api/status                引擎状态(公开)
//!   GET  /api/stats                 索引统计(公开)
//!   GET  /api/search                搜索(公开)
//!   GET  /api/suggest               联想(公开)
//!   GET  /api/sitemap               站点地图(公开)
//!   POST /api/auth/login            管理员账号密码登录
//!   POST /api/auth/logout           登出
//!   ---- 以下需管理员(会话 token 或签发 token) ----
//!   GET  /api/admin/status          扫描状态 + 配置概要
//!   GET  /api/admin/scan-status     扫描状态
//!   POST /api/admin/scan            触发穷举遍历
//!   GET  /api/admin/config          读取后缀/DNS 配置
//!   POST /api/admin/config/tld      保存后缀列表
//!   POST /api/admin/config/dns      保存 DNS 列表
//!   POST /api/admin/rebuild         重建索引
//!   GET  /api/admin/tokens          已签发 token 列表
//!   POST /api/admin/tokens          签发新 token
//!   DELETE /api/admin/tokens/<id>   撤销 token

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ai::AiConfig;
use crate::auth::{Sessions, TokenStore};
use crate::http::{Request, Response};
use crate::json::Json;
use crate::search::SearchEngine;

const WEB_UI: &str = include_str!("../web/index.html");
const API_DOCS: &str = include_str!("../web/api_docs.html");

/// 扫描任务状态(跨线程共享)
#[derive(Clone, Debug)]
pub struct ScanState {
    pub running: bool,
    pub finished: bool,
    pub error: Option<String>,
    pub started_ts: u64,
    pub finished_ts: u64,
    pub total: u64,
    pub registered: u64,
    pub available: u64,
    pub errors: u64,
    pub skipped: u64,
    pub sites: u64,
    pub elapsed_secs: f64,
    pub max_len: usize,
    pub grand_total: u128,
}

impl Default for ScanState {
    fn default() -> Self {
        ScanState {
            running: false,
            finished: false,
            error: None,
            started_ts: 0,
            finished_ts: 0,
            total: 0,
            registered: 0,
            available: 0,
            errors: 0,
            skipped: 0,
            sites: 0,
            elapsed_secs: 0.0,
            max_len: 0,
            grand_total: 0,
        }
    }
}

/// 服务上下文
pub struct ServerCtx {
    pub engine: Arc<SearchEngine>,
    pub ai: AiConfig,
    pub admin_user: String,
    pub admin_pass: String,
    /// 前端是否显示管理员入口(engine.conf admin_show=0 隐藏;仍可通过 /admin 直达)
    pub admin_visible: std::sync::atomic::AtomicBool,
    pub tokens: TokenStore,
    pub sessions: Sessions,
    pub scan: Arc<Mutex<ScanState>>,
    /// 爬虫(外链发现抓取)
    pub crawler: Arc<crate::crawler::Crawler>,
    /// 定时任务调度器
    pub tasks: Arc<crate::tasks::TaskScheduler>,
    /// 搜索统计(内存聚合)
    pub stats: Arc<crate::stats::StatsCollector>,
}

impl ServerCtx {
    pub fn new(
        engine: Arc<SearchEngine>,
        ai: AiConfig,
        admin_user: String,
        admin_pass: String,
        admin_visible: bool,
        tokens: TokenStore,
        sessions: Sessions,
        crawler: Arc<crate::crawler::Crawler>,
        tasks: Arc<crate::tasks::TaskScheduler>,
        stats: Arc<crate::stats::StatsCollector>,
    ) -> ServerCtx {
        ServerCtx {
            engine,
            ai,
            admin_user,
            admin_pass,
            admin_visible: std::sync::atomic::AtomicBool::new(admin_visible),
            tokens,
            sessions,
            scan: Arc::new(Mutex::new(ScanState::default())),
            crawler,
            tasks,
            stats,
        }
    }

    pub fn admin_enabled(&self) -> bool {
        !self.admin_user.is_empty() && !self.admin_pass.is_empty()
    }

    /// 管理员校验:Authorization: Bearer <token>,token 为登录会话或管理员签发
    fn is_admin(&self, req: &Request) -> bool {
        let auth = req.header("authorization").unwrap_or("");
        let token = auth.strip_prefix("Bearer ").unwrap_or("").trim();
        if token.is_empty() {
            return false;
        }
        self.sessions.verify(token) || self.tokens.verify(token)
    }
}

/// 管理后台(独立页面,左右布局)
const ADMIN_UI: &str = include_str!("../web/admin.html");

pub fn handle(ctx: &ServerCtx, req: &Request) -> Response {
    let path = req.path.as_str();
    match (req.method.as_str(), path) {
        ("GET", "/search") => Response::html(200, WEB_UI), // 搜索页(前端读 URL q/page 参数自动搜索)
        ("GET", "/") => Response::html(200, WEB_UI),
        ("GET", "/admin") => Response::html(200, ADMIN_UI),
        ("GET", "/api/docs") => Response::html(200, API_DOCS),
        ("GET", "/api/status") => api_status(ctx),
        ("GET", "/api/stats") => api_stats(ctx),
        ("GET", "/api/search") => api_search(ctx, req),
        ("GET", "/api/suggest") => api_suggest(ctx, req),
        ("GET", "/api/sitemap") => api_sitemap(ctx, req),
        // ---- 认证 ----
        ("POST", "/api/auth/login") => api_login(ctx, req),
        ("POST", "/api/auth/logout") => api_logout(ctx, req),
        // ---- 管理员区(会话 token 或签发 token) ----
        ("GET", "/api/admin/status") => admin_guard(ctx, req, |ctx| admin_status(ctx)),
        ("GET", "/api/admin/scan-status") => admin_guard(ctx, req, |ctx| admin_scan_status(ctx)),
        ("POST", "/api/admin/scan") => admin_guard(ctx, req, |ctx| admin_scan(ctx, req)),
        ("GET", "/api/admin/config") => admin_guard(ctx, req, |ctx| admin_config_get(ctx)),
        ("POST", "/api/admin/config/tld") => admin_guard(ctx, req, |ctx| admin_config_save(ctx, req, "tld")),
        ("POST", "/api/admin/config/dns") => admin_guard(ctx, req, |ctx| admin_config_save(ctx, req, "dns")),
        ("POST", "/api/admin/config/admin_show") => admin_guard(ctx, req, |ctx| admin_config_admin_show(ctx, req)),
        // ---- 节日 LOGO(替换搜索页 Logo)----
        ("GET", "/api/logo") => api_logo(ctx),
        ("POST", "/api/admin/logo") => admin_guard(ctx, req, |ctx| admin_logo_upload(ctx, req)),
        ("DELETE", "/api/admin/logo") => admin_guard(ctx, req, |ctx| admin_logo_delete(ctx)),
        ("POST", "/api/admin/rebuild") => admin_guard(ctx, req, |ctx| admin_rebuild(ctx)),
        ("POST", "/api/rebuild") => admin_guard(ctx, req, |ctx| admin_rebuild(ctx)), // 旧路径,现需管理员
        ("GET", "/api/admin/index-status") => admin_guard(ctx, req, |ctx| admin_index_status(ctx)),
        ("GET", "/api/admin/logs") => admin_guard(ctx, req, |ctx| admin_logs(ctx, req)),
        ("GET", "/api/admin/tokens") => admin_guard(ctx, req, |ctx| admin_tokens_list(ctx)),
        ("POST", "/api/admin/tokens") => admin_guard(ctx, req, |ctx| admin_tokens_create(ctx, req)),
        ("DELETE", "/api/admin/tokens/") => Response::json(400, r#"{"error":"缺少 token id"}"#),
        ("DELETE", p) if p.starts_with("/api/admin/tokens/") => {
            let id = &p["/api/admin/tokens/".len()..];
            admin_guard(ctx, req, |ctx| admin_tokens_revoke(ctx, id))
        }
        // ---- 黑名单管理 ----
        ("GET", "/api/admin/blacklist") => admin_guard(ctx, req, |ctx| admin_blacklist_list(ctx, req)),
        ("POST", "/api/admin/blacklist") => admin_guard(ctx, req, |ctx| admin_blacklist_add(ctx, req)),
        ("DELETE", "/api/admin/blacklist/") => Response::json(400, r#"{"error":"缺少域名"}"#),
        ("DELETE", p) if p.starts_with("/api/admin/blacklist/") => {
            let domain = &p["/api/admin/blacklist/".len()..];
            admin_guard(ctx, req, |ctx| admin_blacklist_remove(ctx, domain))
        }
        // ---- 定时任务 ----
        ("GET", "/api/admin/tasks") => admin_guard(ctx, req, |ctx| admin_tasks_list(ctx)),
        ("POST", "/api/admin/tasks") => admin_guard(ctx, req, |ctx| admin_tasks_add(ctx, req)),
        ("POST", "/api/admin/tasks/toggle") => admin_guard(ctx, req, |ctx| admin_tasks_toggle(ctx, req)),
        ("DELETE", "/api/admin/tasks/") => Response::json(400, r#"{"error":"缺少任务 id"}"#),
        ("DELETE", p) if p.starts_with("/api/admin/tasks/") => {
            let id = &p["/api/admin/tasks/".len()..];
            admin_guard(ctx, req, |ctx| admin_tasks_remove(ctx, id))
        }
        // ---- 爬虫 ----
        ("POST", "/api/admin/crawl") => admin_guard(ctx, req, |ctx| admin_crawl(ctx, req)),
        ("GET", "/api/admin/crawl-status") => admin_guard(ctx, req, |ctx| admin_crawl_status(ctx)),
        ("POST", "/api/admin/add-site") => admin_guard(ctx, req, |ctx| admin_add_site(ctx, req)),
        // ---- 重复清理/批量 ----
        ("GET", "/api/admin/duplicates") => admin_guard(ctx, req, |ctx| admin_duplicates(ctx, req)),
        ("POST", "/api/admin/blacklist/batch") => admin_guard(ctx, req, |ctx| admin_blacklist_batch(ctx, req)),
        ("POST", "/api/admin/cleanup") => admin_guard(ctx, req, |ctx| admin_cleanup(ctx, req)),
        // ---- 统计 ----
        ("GET", "/api/admin/stats") => admin_guard(ctx, req, |ctx| admin_stats(ctx)),
        // ---- 聚合搜索(其它搜索引擎)配置 ----
        ("GET", "/api/admin/metasearch") => admin_guard(ctx, req, |ctx| admin_metasearch_get(ctx)),
        ("POST", "/api/admin/metasearch") => admin_guard(ctx, req, |ctx| admin_metasearch_save(ctx, req)),
        // ---- favicon(内存缓存,公开可访问) ----
        ("GET", "/api/favicon/") => Response::json(404, r#"{"error":"缺少域名"}"#),
        ("GET", p) if p.starts_with("/api/favicon/") => {
            let domain = &p["/api/favicon/".len()..];
            api_favicon(ctx, domain)
        }
        _ => page_not_found(path),
    }
}

/// 谷歌风格 404 页(浏览器路径):大标题 + 搜索框 + 提示;API 路径保持 JSON
fn page_not_found(path: &str) -> Response {
    if path.starts_with("/api/") {
        return Response::json(404, r#"{"error":"not found"}"#);
    }
    Response::html(404, PAGE_404)
}

/// 404 美化页(学谷歌:大 404 + 搜索框 + 简洁提示)
const PAGE_404: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>404 - 页面不存在</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { font-family: Arial, "Microsoft YaHei", sans-serif; background: #f8f9fa; color: #202124; display: flex; flex-direction: column; align-items: center; justify-content: center; min-height: 100vh; padding: 24px; }
  .box { text-align: center; max-width: 560px; }
  h1 { font-size: 96px; font-weight: 500; color: #202124; line-height: 1; }
  .tag { font-size: 20px; margin: 16px 0 8px; }
  .desc { font-size: 14px; color: #5f6368; margin-bottom: 24px; }
  form { display: flex; gap: 8px; justify-content: center; }
  input[type="text"] { width: 340px; max-width: 70vw; padding: 12px 16px; border: 1px solid #dfe1e5; border-radius: 24px; font-size: 16px; outline: none; }
  input[type="text"]:focus { box-shadow: 0 1px 6px rgba(32,33,36,0.28); border-color: transparent; }
  button { padding: 0 20px; background: #1a73e8; color: #fff; border: none; border-radius: 24px; font-size: 14px; cursor: pointer; }
  button:hover { background: #1765cc; }
  .back { margin-top: 28px; font-size: 13px; }
  .back a { color: #1a73e8; text-decoration: none; }
  .back a:hover { text-decoration: underline; }
</style>
</head>
<body>
<div class="box">
  <h1>404</h1>
  <div class="tag">此页面不存在</div>
  <div class="desc">您访问的地址不存在或已被移除。试试搜索,或者返回首页。</div>
  <form action="/search" method="get">
    <input type="text" name="q" placeholder="搜索已收录的网站..." autofocus>
    <button type="submit">搜索</button>
  </form>
  <div class="back"><a href="/">← 返回搜索引擎首页</a></div>
</div>
</body>
</html>"##;

/// 管理员守卫:无权限返回 403
fn admin_guard(ctx: &ServerCtx, req: &Request, f: impl Fn(&ServerCtx) -> Response) -> Response {
    if !ctx.is_admin(req) {
        let msg = if !ctx.admin_enabled() {
            r#"{"error":"管理功能未启用:请先在 config/engine.conf 配置 admin_user/admin_pass"}"#
        } else {
            r#"{"error":"无管理员权限:请用账号密码登录(/api/auth/login),或使用管理员签发的 API token"}"#
        };
        return Response::json(403, msg);
    }
    f(ctx)
}

// ---------------- 公开 API ----------------

fn api_status(ctx: &ServerCtx) -> Response {
    let idx = ctx.engine.index().lock().unwrap();
    let (sites, terms, blocks) = idx.stats();
    let loaded = idx.loaded_blocks();
    drop(idx);
    let (hits, misses) = ctx.engine.cache_stats();
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
        ("admin_enabled", Json::Bool(ctx.admin_enabled())),
        ("admin_visible", Json::Bool(ctx.admin_visible.load(std::sync::atomic::Ordering::Relaxed))),
        ("blacklist", Json::num(ctx.engine.blacklist.blocked_count() as f64)),
        ("scan_running", Json::Bool(ctx.scan.lock().unwrap().running)),
    ]);
    Response::json(200, &j.to_string())
}

fn api_stats(ctx: &ServerCtx) -> Response {
    let idx = ctx.engine.index().lock().unwrap();
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

fn api_search(ctx: &ServerCtx, req: &Request) -> Response {
    let q = req.param("q").unwrap_or("").trim().to_string();
    let page = req.param("page").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
    let limit = req.param("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(10).min(100).max(1);
    let meta_page = req.param("meta_page").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
    let want_ai = req.param("ai").map(|v| v == "1" || v == "true").unwrap_or(false);
    if q.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 q"}"#);
    }
    let start = Instant::now();
    let (total, hits) = ctx.engine.search(&q, page, limit);
    let elapsed_ms = start.elapsed().as_millis();
    let pages = (total + limit - 1) / limit;
    // 搜索统计(内存聚合,含响应耗时)
    ctx.stats.record(&q, elapsed_ms as u64);

    // 本地索引搜不到 → 聚合搜索(必应/百度/360/搜狗/谷歌/中国搜索),
    // 结果缓存到本地引擎(下次同词直接命中,不再联网)
    let mut meta_results: Vec<Json> = Vec::new();
    let mut meta_from_cache = false;
    let mut meta_ms = 0u128;
    let mut meta_total = 0usize;
    let mut meta_pages = 1usize;
    if total == 0 {
        let meta_start = Instant::now();
        let (meta, from_cache) = crate::metasearch::search_cached(&q);
        meta_from_cache = from_cache;
        meta_ms = meta_start.elapsed().as_millis();
        // 聚合搜索统计(引擎来源 + 缓存命中)
        let engines: Vec<String> = meta.iter().map(|m| m.engine.clone()).collect();
        ctx.stats.record_meta(&engines, from_cache);
        meta_results = meta
            .iter()
            .map(|m| {
                Json::build(vec![
                    ("title", Json::str(&m.title)),
                    ("url", Json::str(&m.url)),
                    ("snippet", Json::str(&m.snippet)),
                    ("engine", Json::str(&m.engine)),
                ])
            })
            .collect();
        // 乱码/未知字符结果排到最后(质量优先,不影响正常结果)
        meta_results.sort_by_key(|m| {
            let title = m.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let bad = title.contains('\u{FFFD}')
                || title.chars().any(|c| c.is_control() && c != '\n')
                || title.chars().all(|c| !c.is_alphanumeric() && !c.is_whitespace());
            if bad {
                1
            } else {
                0
            }
        });
        // 聚合结果分页(每页 10 条,可翻页查看全部)
        meta_total = meta_results.len();
        meta_pages = (meta_total + 9) / 10;
        let meta_page = meta_page.min(meta_pages.max(1));
        let meta_start = (meta_page - 1) * 10;
        meta_results = meta_results.into_iter().skip(meta_start).take(10).collect();
    }
    let meta_elapsed_ms = meta_ms;

    let results: Vec<Json> = hits
        .iter()
        .map(|h| {
            Json::build(vec![
                ("title", Json::str(&h.title)),
                ("url", Json::str(&h.url)),
                ("domain", Json::str(&h.domain)),
                ("description", Json::str(&h.description)),
                ("score", Json::num(h.score)),
                ("fold_count", Json::num(h.fold_count as f64)),
                ("dup_count", Json::num(h.dup_count as f64)),
            ])
        })
        .collect();

    let mut pairs: Vec<(&str, Json)> = vec![
        ("query", Json::str(&q)),
        ("page", Json::num(page as f64)),
        ("pages", Json::num(pages as f64)),
        ("time_ms", Json::num(elapsed_ms as f64)),
        ("total", Json::num(total as f64)),
        ("results", Json::arr(results)),
        ("meta", Json::arr(meta_results)),
        ("meta_cache", if meta_from_cache { Json::num(1.0) } else { Json::num(0.0) }),
        ("meta_ms", Json::num(meta_elapsed_ms as f64)),
        ("meta_page", Json::num(meta_page as f64)),
        ("meta_pages", Json::num(meta_pages as f64)),
        ("meta_total", Json::num(meta_total as f64)),
    ];

    if want_ai && ctx.ai.enabled {
        let sys = "你是一个本地搜索引擎助手。基于给定的搜索结果,用简洁中文回答用户问题;若结果不足以回答,如实说明。";
        match crate::ai::chat_completion(&ctx.ai, sys, &build_ai_context(&q, &hits)) {
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

fn api_suggest(ctx: &ServerCtx, req: &Request) -> Response {
    let q = req.param("q").unwrap_or("").trim().to_string();
    let limit = req.param("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(10).min(50);
    if q.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 q"}"#);
    }
    let words = ctx.engine.suggest(&q, limit);
    let j = Json::build(vec![
        ("query", Json::str(&q)),
        ("suggestions", Json::arr(words.into_iter().map(Json::str).collect())),
    ]);
    Response::json(200, &j.to_string())
}

fn api_sitemap(ctx: &ServerCtx, req: &Request) -> Response {
    let domain = req.param("domain").unwrap_or("").trim().to_string();
    if domain.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 domain"}"#);
    }
    let site_dir = crate::config::sites_dir().join(&domain);
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
    let _ = ctx;
    let j = Json::build(vec![
        ("domain", Json::str(&domain)),
        ("sitemap_url", Json::str(&format!("/out/sites/{}/sitemap.xml", domain))),
        ("urls", Json::arr(urls.into_iter().map(Json::str).collect())),
    ]);
    Response::json(200, &j.to_string())
}

// ---------------- 认证 ----------------

/// 账号密码登录:成功返回会话 token(管理员 Web UI 用)
fn api_login(ctx: &ServerCtx, req: &Request) -> Response {
    if !ctx.admin_enabled() {
        return Response::json(403, r#"{"error":"管理功能未启用:请先在 config/engine.conf 配置 admin_user/admin_pass"}"#);
    }
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let user = params.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let pass = params.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if user == ctx.admin_user && pass == ctx.admin_pass {
        let token = ctx.sessions.create();
        Response::json(200, &Json::build(vec![
            ("status", Json::str("ok")),
            ("token", Json::str(&token)),
            ("expires_in", Json::num(12.0 * 3600.0)),
            ("user", Json::str(user)),
        ]).to_string())
    } else {
        Response::json(401, r#"{"error":"用户名或密码错误"}"#)
    }
}

/// 登出:销毁会话 token
fn api_logout(ctx: &ServerCtx, req: &Request) -> Response {
    let auth = req.header("authorization").unwrap_or("");
    if let Some(token) = auth.strip_prefix("Bearer ") {
        ctx.sessions.remove(token.trim());
    }
    Response::json(200, r#"{"status":"ok"}"#)
}

// ---------------- Token 管理(管理员签发给 API/MCP) ----------------

/// 索引构建进度(进度条 + 当前站点 + 关键词)
fn admin_index_status(ctx: &ServerCtx) -> Response {
    let s = ctx.engine.index_state().lock().unwrap().clone();
    let keywords: Vec<Json> = s.keywords.iter().map(|k| Json::str(k)).collect();
    Response::json(
        200,
        &Json::build(vec![
            ("phase", Json::str(&s.phase)),
            ("processed", Json::num(s.processed as f64)),
            ("total", Json::num(s.total as f64)),
            ("current_domain", Json::str(&s.current_domain)),
            ("keywords", Json::arr(keywords)),
            ("links_found", Json::num(s.links_found as f64)),
            ("sites", Json::num(s.sites as f64)),
            ("dup", Json::num(s.dup as f64)),
            ("blocked", Json::num(s.blocked as f64)),
            ("running", Json::Bool(s.running)),
            ("finished", Json::Bool(s.finished)),
            ("elapsed_secs", Json::num(s.elapsed_secs)),
            ("error", Json::str(s.error.as_deref().unwrap_or(""))),
        ])
        .to_string(),
    )
}

/// 实时日志流(after 增量拉取)
fn admin_logs(_ctx: &ServerCtx, req: &Request) -> Response {
    let after = req.param("after").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    let (newest, lines) = crate::logger::global().since(after);
    let arr: Vec<Json> = lines.iter().map(|l| Json::str(l)).collect();
    Response::json(
        200,
        &Json::build(vec![
            ("newest", Json::num(newest as f64)),
            ("logs", Json::arr(arr)),
        ])
        .to_string(),
    )
}

fn admin_tokens_list(ctx: &ServerCtx) -> Response {
    let list: Vec<Json> = ctx
        .tokens
        .list()
        .iter()
        .map(|t| {
            Json::build(vec![
                ("id", Json::str(&t.id)),
                ("name", Json::str(&t.name)),
                ("created", Json::num(t.created as f64)),
                ("last_used", Json::num(t.last_used as f64)),
                ("prefix", Json::str(&t.token[..8.min(t.token.len())])),
            ])
        })
        .collect();
    Response::json(200, &Json::build(vec![("tokens", Json::arr(list))]).to_string())
}

fn admin_tokens_create(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 name(token 用途名称)"}"#);
    }
    let token = ctx.tokens.create(&name);
    Response::json(201, &Json::build(vec![
        ("status", Json::str("ok")),
        ("name", Json::str(&name)),
        ("token", Json::str(&token)),
        ("note", Json::str("token 仅此一次完整显示,请立即保存;调用时用 Authorization: Bearer <token>")),
    ]).to_string())
}

fn admin_tokens_revoke(ctx: &ServerCtx, id: &str) -> Response {
    if ctx.tokens.revoke(id) {
        Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("revoked", Json::str(id))]).to_string())
    } else {
        Response::json(404, r#"{"error":"token 不存在"}"#)
    }
}

// ---------------- 黑名单管理 ----------------

fn admin_blacklist_list(ctx: &ServerCtx, req: &Request) -> Response {
    let all = ctx.engine.blacklist.list();
    let page = req.param("page").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
    let limit = req.param("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(50).min(500).max(1);
    let total = all.len();
    let pages = (total + limit - 1) / limit;
    let start = (page - 1) * limit;
    let slice: &[crate::blacklist::BlacklistEntry] = if start < all.len() {
        &all[start..(start + limit).min(all.len())]
    } else {
        &[]
    };
    let entries: Vec<Json> = slice
        .iter()
        .map(|e| {
            Json::build(vec![
                ("domain", Json::str(&e.domain)),
                ("reason", Json::str(&e.reason)),
                ("added_at", Json::num(e.added_at as f64)),
            ])
        })
        .collect();
    Response::json(
        200,
        &Json::build(vec![
            ("count", Json::num(total as f64)),
            ("page", Json::num(page as f64)),
            ("pages", Json::num(pages as f64)),
            ("blacklist", Json::arr(entries)),
        ])
        .to_string(),
    )
}

fn admin_blacklist_add(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let domain = params.get("domain").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if domain.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 domain"}"#);
    }
    ctx.engine.blacklist.add(&domain, "manual");
    ctx.engine.clear_cache(); // 黑名单变更,清缓存立即生效
    Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("domain", Json::str(&domain))]).to_string())
}

fn admin_blacklist_remove(ctx: &ServerCtx, domain: &str) -> Response {
    if ctx.engine.blacklist.remove(domain) {
        ctx.engine.clear_cache(); // 黑名单变更,清缓存立即生效
        Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("removed", Json::str(domain))]).to_string())
    } else {
        Response::json(404, r#"{"error":"域名不在黑名单"}"#)
    }
}

// ---------------- 定时任务与爬虫 ----------------

fn admin_tasks_list(ctx: &ServerCtx) -> Response {
    let tasks: Vec<Json> = ctx
        .tasks
        .list()
        .iter()
        .map(|t| {
            Json::build(vec![
                ("id", Json::str(&t.id)),
                ("name", Json::str(&t.name)),
                ("kind", Json::str(&t.kind)),
                ("interval_secs", Json::num(t.interval_secs as f64)),
                ("params", t.params.clone()),
                ("enabled", Json::Bool(t.enabled)),
                ("last_run", Json::num(t.last_run as f64)),
            ])
        })
        .collect();
    Response::json(200, &Json::build(vec![("tasks", Json::arr(tasks))]).to_string())
}

fn admin_tasks_add(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let kind = params.get("kind").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let interval = params.get("interval_secs").and_then(|v| v.as_u64()).unwrap_or(3600);
    if name.is_empty() || !["scan", "rebuild", "crawl"].contains(&kind.as_str()) {
        return Response::json(400, r#"{"error":"参数缺失: name/kind(scan|rebuild|crawl)/interval_secs"}"#);
    }
    let id = format!("t{}", now_secs());
    let mut task = crate::tasks::Task::new(&id, &name, &kind, interval);
    task.params = params.get("params").cloned().unwrap_or_else(Json::obj);
    ctx.tasks.add(task);
    Response::json(201, &Json::build(vec![("status", Json::str("ok")), ("id", Json::str(&id))]).to_string())
}

fn admin_tasks_toggle(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let id = params.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    if id.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 id"}"#);
    }
    if ctx.tasks.set_enabled(&id, enabled) {
        Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("id", Json::str(&id)), ("enabled", Json::Bool(enabled))]).to_string())
    } else {
        Response::json(404, r#"{"error":"任务不存在"}"#)
    }
}

fn admin_tasks_remove(ctx: &ServerCtx, id: &str) -> Response {
    if ctx.tasks.remove(id) {
        Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("removed", Json::str(id))]).to_string())
    } else {
        Response::json(404, r#"{"error":"任务不存在"}"#)
    }
}

/// 手动触发爬虫(种子 = 白名单 + 外链发现 + 手动附加;参数:深度/页数/并发)
fn admin_crawl(ctx: &ServerCtx, req: &Request) -> Response {
    let mut seeds: Vec<String> = Vec::new();
    // 1. 白名单种子(whitelist.sorl 国内基础名单,链式扩展;no_whitelist=true 跳过)
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let no_whitelist = params.get("no_whitelist").and_then(|v| v.as_bool()).unwrap_or(false);
    if !no_whitelist {
        for d in whitelist_domains() {
            seeds.push(format!("http://{}/", d));
        }
    }
    // 1b. 全站已有域名种子(use_sites=true:把 out/sites 全部域名作为爬虫起点)
    let use_sites = params.get("use_sites").and_then(|v| v.as_bool()).unwrap_or(false);
    if use_sites {
        let sites_dir = crate::config::sites_dir();
        if let Ok(rd) = std::fs::read_dir(&sites_dir) {
            for e in rd.flatten() {
                if let Some(name) = e.file_name().to_str() {
                    let name = name.trim();
                    if !name.is_empty() && !name.starts_with('.') {
                        seeds.push(format!("http://{}/", name));
                    }
                }
            }
        }
    }
    // 2. 外链发现种子
    let disc_path = crate::config::index_dir().join("discovered.txt");
    if let Ok(text) = std::fs::read_to_string(&disc_path) {
        for line in text.lines() {
            let d = line.trim();
            if !d.is_empty() {
                seeds.push(format!("http://{}/", d));
            }
        }
    }
    // 3. 手动附加
    if let Some(extra) = params.get("seeds").and_then(|v| v.as_arr()) {
        for s in extra {
            if let Some(u) = s.as_str() {
                let u = u.trim();
                if !u.is_empty() {
                    seeds.push(if u.starts_with("http") { u.to_string() } else { format!("http://{}/", u) });
                }
            }
        }
    }
    // 去重 + 黑名单过滤
    let mut seen_set = std::collections::HashSet::new();
    seeds.retain(|s| seen_set.insert(s.clone()));
    seeds.retain(|s| {
        let d = s.trim_start_matches("http://").trim_end_matches('/').to_lowercase();
        !ctx.engine.blacklist.contains(&d)
    });
    if seeds.is_empty() {
        return Response::json(400, r#"{"error":"没有种子:配置白名单/重建索引发现外链,或在 seeds 参数提供"}"#);
    }
    // 参数:深度/页数/每域/并发/超时
    let depth = params.get("depth").and_then(|v| v.as_u64()).unwrap_or(3).min(10) as usize;
    let max_pages = params.get("max_pages").and_then(|v| v.as_u64()).unwrap_or(2000).min(200_000) as usize;
    let per_domain = params.get("per_domain").and_then(|v| v.as_u64()).unwrap_or(100).min(5000) as usize;
    let workers = params.get("workers").and_then(|v| v.as_u64()).unwrap_or(0).min(128) as usize;
    let timeout = params.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(5000) as u64;

    let crawler = ctx.crawler.clone();
    let engine = ctx.engine.clone();
    let seeds_len = seeds.len();
    let out_dir = crawl_out_dir();
    std::thread::spawn(move || {
        crawler.set_workers(workers);
        // 共享 stats/favicons:管理面板实时看到进度,favicon 缓存不重复抓
        let c = crate::crawler::Crawler::new_shared(
            depth, max_pages, per_domain, timeout,
            crawler.worker_count(),
            Some((crawler.stats.clone(), crawler.favicons.clone(), crawler.favicon_order.clone())),
        );
        *crawler.stats.lock().unwrap() = crate::crawler::CrawlStats::default();
        let stats = c.crawl(&seeds, &out_dir);
        println!("[crawler] 完成: 抓取 {} 发现 {} 失败 {} ({:.0}s, workers={})", stats.fetched, stats.discovered, stats.failed, stats.elapsed_secs, c.worker_count());
        // 爬完重建索引收录新站
        let _ = engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir());
    });
    Response::json(
        202,
        &Json::build(vec![
            ("status", Json::str("crawl_started")),
            ("seeds", Json::num(seeds_len as f64)),
            ("workers", Json::num(if workers > 0 { workers as f64 } else { std::thread::available_parallelism().map(|n| n.get() as f64).unwrap_or(4.0) })),
        ])
        .to_string(),
    )
}

/// 手动添加域名:抓取(robots+sitemap)+ 落盘 + 重建索引,自动完成
fn admin_add_site(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let domain = params.get("domain").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if domain.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 domain"}"#);
    }
    let url = if domain.starts_with("http") { domain.clone() } else { format!("http://{}/", domain) };
    let engine = ctx.engine.clone();
    let domain_clone = domain.clone();
    std::thread::spawn(move || {
        let out_dir = crawl_out_dir();
        let c = crate::crawler::Crawler::new(2, 500, 200, 5000, 4);
        let stats = c.crawl(&[url], &out_dir);
        println!("[add-site] {} 抓取完成: {} 页,{} 失败", domain_clone, stats.fetched, stats.failed);
        // 自动重建索引
        let _ = engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir());
    });
    Response::json(202, &Json::build(vec![("status", Json::str("added")), ("domain", Json::str(&domain))]).to_string())
}

/// 重复标题列表(按折叠标题分组,count>1 的组,分页)
fn admin_duplicates(ctx: &ServerCtx, req: &Request) -> Response {
    let page = req.param("page").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
    let limit = req.param("limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(20).min(200).max(1);
    let idx = ctx.engine.index().lock().unwrap();
    // 按折叠标题分组
    let mut groups: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    for (i, d) in idx.docs.iter().enumerate() {
        let key = crate::search::fold_key(&d.title, &d.domain);
        groups.entry(key).or_default().push(i);
    }
    drop(idx);
    let mut dup_groups: Vec<(String, usize, Vec<(String, String)>)> = groups
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(key, ids)| {
            let n = ids.len();
            let ctx2 = ctx.engine.index().lock().unwrap();
            let samples: Vec<(String, String)> = ids.iter().take(10).map(|&i| (ctx2.docs[i].domain.clone(), ctx2.docs[i].url.clone())).collect();
            drop(ctx2);
            (key, n, samples)
        })
        .collect();
    dup_groups.sort_by(|a, b| b.1.cmp(&a.1));
    let total = dup_groups.len();
    let pages = (total + limit - 1) / limit;
    let start = (page - 1) * limit;
    let slice: Vec<&(String, usize, Vec<(String, String)>)> = if start < total {
        dup_groups[start..(start + limit).min(total)].iter().collect()
    } else {
        vec![]
    };
    let groups_json: Vec<Json> = slice
        .iter()
        .map(|(key, n, samples)| {
            Json::build(vec![
                ("title", Json::str(key)),
                ("count", Json::num(*n as f64)),
                ("domains", Json::arr(samples.iter().map(|(d, u)| Json::build(vec![("domain", Json::str(d)), ("url", Json::str(u))])).collect())),
            ])
        })
        .collect();
    Response::json(
        200,
        &Json::build(vec![
            ("count", Json::num(total as f64)),
            ("page", Json::num(page as f64)),
            ("pages", Json::num(pages as f64)),
            ("groups", Json::arr(groups_json)),
        ])
        .to_string(),
    )
}

/// 批量拉黑
fn admin_blacklist_batch(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let mut n = 0usize;
    if let Some(arr) = params.get("domains").and_then(|v| v.as_arr()) {
        for d in arr {
            if let Some(domain) = d.as_str() {
                if !domain.trim().is_empty() {
                    ctx.engine.blacklist.add(domain.trim(), "manual");
                    n += 1;
                }
            }
        }
    }
    ctx.engine.clear_cache();
    Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("added", Json::num(n as f64))]).to_string())
}

/// 一键清理:批量拉黑 + 删除站点目录(计数去重保留的重复站)+ 重建索引
fn admin_cleanup(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let mut removed = 0usize;
    let sites_dir = crate::config::sites_dir();
    let crawled_dir = crawl_out_dir();
    if let Some(arr) = params.get("domains").and_then(|v| v.as_arr()) {
        for d in arr {
            if let Some(domain) = d.as_str() {
                let domain = domain.trim();
                if domain.is_empty() {
                    continue;
                }
                ctx.engine.blacklist.add(domain, "manual");
                // 删除站点目录(主目录 + 爬虫目录)
                for base in [&sites_dir, &crawled_dir] {
                    let dir = base.join(domain);
                    if dir.exists() {
                        let _ = std::fs::remove_dir_all(&dir);
                        removed += 1;
                    }
                }
            }
        }
    }
    ctx.engine.clear_cache();
    // 重建索引
    let sites = ctx.engine.rebuild(&sites_dir, &crate::config::index_dir()).unwrap_or(0);
    Response::json(
        200,
        &Json::build(vec![
            ("status", Json::str("ok")),
            ("removed_dirs", Json::num(removed as f64)),
            ("sites", Json::num(sites as f64)),
        ])
        .to_string(),
    )
}

/// 统计:搜索热词/总量/每日/关键词(索引词条数)
fn admin_stats(ctx: &ServerCtx) -> Response {
    let idx = ctx.engine.index().lock().unwrap();
    let (sites, terms, blocks) = idx.stats();
    drop(idx);
    let (hits, misses) = ctx.engine.cache_stats();
    let top: Vec<Json> = ctx
        .stats
        .top_queries(30)
        .iter()
        .map(|(q, n)| Json::build(vec![("query", Json::str(q)), ("count", Json::num(*n as f64))]))
        .collect();
    let daily: Vec<Json> = ctx
        .stats
        .daily_stats()
        .iter()
        .map(|(d, n)| Json::build(vec![("date", Json::str(d)), ("count", Json::num(*n as f64))]))
        .collect();
    // 性能与聚合搜索统计
    let (avg_ms, meta_searches, meta_hits, meta_misses, latency_samples, meta_engines) = ctx.stats.perf_snapshot();
    let engines: Vec<Json> = meta_engines
        .iter()
        .map(|(e, n)| Json::build(vec![("engine", Json::str(e)), ("count", Json::num(*n as f64))]))
        .collect();
    Response::json(
        200,
        &Json::build(vec![
            ("total_searches", Json::num(ctx.stats.total() as f64)),
            ("top_queries", Json::arr(top)),
            ("daily", Json::arr(daily)),
            ("index_sites", Json::num(sites as f64)),
            ("index_terms", Json::num(terms as f64)),
            ("index_blocks", Json::num(blocks as f64)),
            ("cache_hit_rate", Json::num(if hits + misses > 0 { hits as f64 / (hits + misses) as f64 } else { 0.0 })),
            ("avg_ms", Json::num(avg_ms)),
            ("latency_samples", Json::num(latency_samples as f64)),
            ("meta_searches", Json::num(meta_searches as f64)),
            ("meta_cache_hits", Json::num(meta_hits as f64)),
            ("meta_cache_misses", Json::num(meta_misses as f64)),
            ("meta_cache_rate", Json::num(if meta_hits + meta_misses > 0 { meta_hits as f64 / (meta_hits + meta_misses) as f64 } else { 0.0 })),
            ("meta_engines", Json::arr(engines)),
            ("crawler_workers", Json::num(ctx.crawler.worker_count() as f64)),
        ])
        .to_string(),
    )
}

/// 聚合搜索配置:引擎列表(内置 + 自定义)+ 启停状态
fn admin_metasearch_get(ctx: &ServerCtx) -> Response {
    let _ = ctx;
    let builtin: Vec<Json> = crate::metasearch::PROVIDERS
        .iter()
        .map(|p| {
            Json::build(vec![
                ("id", Json::str(p.id)),
                ("name", Json::str(p.name)),
                ("url", Json::str(p.url)),
                ("enabled", if crate::metasearch::provider_enabled(p.id) { Json::num(1.0) } else { Json::num(0.0) }),
                ("builtin", Json::num(1.0)),
            ])
        })
        .collect();
    let custom: Vec<Json> = crate::metasearch::custom_providers()
        .iter()
        .map(|(n, u)| Json::build(vec![("name", Json::str(n)), ("url", Json::str(u)), ("builtin", Json::num(0.0))]))
        .collect();
    Response::json(
        200,
        &Json::build(vec![("engines", Json::arr(builtin)), ("custom", Json::arr(custom))]).to_string(),
    )
}

/// 聚合搜索配置保存:{"enabled":["bing","baidu",...], "custom":[{"name":"..","url":".."}]}
fn admin_metasearch_save(ctx: &ServerCtx, req: &Request) -> Response {
    let _ = ctx;
    let body = String::from_utf8_lossy(&req.body).to_string();
    let Ok(j) = crate::json::parse(&body) else {
        return Response::json(400, r#"{"error":"JSON 解析失败"}"#);
    };
    let enabled: Vec<String> = j
        .get("enabled")
        .and_then(|v| v.as_arr())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if let Err(e) = crate::metasearch::save_config(&enabled) {
        return Response::json(500, &Json::build(vec![("error", Json::str(&e))]).to_string());
    }
    // 自定义引擎(整体覆盖保存)
    let custom: Vec<(String, String)> = j
        .get("custom")
        .and_then(|v| v.as_arr())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| {
                    let n = x.get("name").and_then(|v| v.as_str())?.to_string();
                    let u = x.get("url").and_then(|v| v.as_str())?.to_string();
                    Some((n, u))
                })
                .collect()
        })
        .unwrap_or_default();
    if let Err(e) = crate::metasearch::save_custom(&custom) {
        return Response::json(500, &Json::build(vec![("error", Json::str(&e))]).to_string());
    }
    crate::logger::push(format!("[admin] 聚合引擎配置已保存: 启用 {} 个, 自定义 {} 个", enabled.len(), custom.len()));
    Response::json(200, r#"{"status":"ok"}"#)
}

/// 保存前端管理入口显示开关(admin_show):仅控制前端是否显示"管理"链接,
/// 管理后台 /admin 与 API 始终可用
fn admin_config_admin_show(ctx: &ServerCtx, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let Ok(j) = crate::json::parse(&body) else {
        return Response::json(400, r#"{"error":"JSON 解析失败"}"#);
    };
    let visible = j.get("visible").and_then(|v| v.as_bool()).unwrap_or(true);
    if let Err(e) = crate::config::save_admin_show(visible) {
        return Response::json(500, &Json::build(vec![("error", Json::str(&e))]).to_string());
    }
    // 运行时立即生效(AtomicBool)
    ctx.admin_visible.store(visible, std::sync::atomic::Ordering::Relaxed);
    crate::logger::push(format!(
        "[admin] 前端管理入口显示: {} (/admin 与 API 不受影响)",
        if visible { "开启" } else { "隐藏" }
    ));
    Response::json(200, r#"{"status":"ok"}"#)
}

/// 节日 LOGO 目录与路径(替换搜索页默认 Logo;SVG/PNG)
fn logo_path() -> Option<std::path::PathBuf> {
    let dir = crate::config::index_dir().join("logo");
    for name in ["logo.png", "logo.svg"] {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// GET /api/logo(公开):返回当前节日 LOGO,未设置则 404(前端回退内置 SVG)
fn api_logo(_ctx: &ServerCtx) -> Response {
    if let Some(p) = logo_path() {
        if let Ok(bytes) = std::fs::read(&p) {
            let is_png = p.extension().map(|e| e == "png").unwrap_or(false);
            return Response {
                status: 200,
                content_type: if is_png { "image/png" } else { "image/svg+xml" },
                body: bytes,
                extra_headers: vec![],
            };
        }
    }
    Response::json(404, r#"{"error":"未设置节日 LOGO"}"#)
}

/// POST /api/admin/logo?ext=png|svg(管理):上传节日 LOGO,body 为原始文件字节
fn admin_logo_upload(_ctx: &ServerCtx, req: &Request) -> Response {
    let ext = req.param("ext").unwrap_or("svg").to_lowercase();
    if ext != "png" && ext != "svg" {
        return Response::json(400, r#"{"error":"仅支持 svg / png"}"#);
    }
    let body = &req.body;
    if body.is_empty() || body.len() > 2 * 1024 * 1024 {
        return Response::json(400, r#"{"error":"文件为空或过大(限 2MB)"}"#);
    }
    // 格式校验(防注入/伪装):PNG 魔数;SVG 必须以 <svg 开头
    if ext == "png" {
        if body.len() < 8 || &body[..8] != &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
            return Response::json(400, r#"{"error":"PNG 格式无效(魔数不匹配)"}"#);
        }
    } else {
        let s = String::from_utf8_lossy(body);
        if !s.trim_start().starts_with("<svg") {
            return Response::json(400, r#"{"error":"SVG 格式无效(需以 <svg 开头)"}"#);
        }
    }
    let dir = crate::config::index_dir().join("logo");
    let _ = std::fs::create_dir_all(&dir);
    // 替换旧文件(同 ext 覆盖,异 ext 互斥)
    if ext == "png" {
        let _ = std::fs::remove_file(dir.join("logo.svg"));
    } else {
        let _ = std::fs::remove_file(dir.join("logo.png"));
    }
    let path = dir.join(format!("logo.{}", ext));
    if let Err(e) = std::fs::write(&path, body) {
        return Response::json(500, &Json::build(vec![("error", Json::str(&format!("写入失败: {}", e)))]).to_string());
    }
    crate::logger::push(format!("[admin] 节日 LOGO 已上传: logo.{} ({} 字节)", ext, body.len()));
    Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("ext", Json::str(ext))]).to_string())
}

/// DELETE /api/admin/logo(管理):删除节日 LOGO,恢复默认
fn admin_logo_delete(_ctx: &ServerCtx) -> Response {
    if let Some(p) = logo_path() {
        let _ = std::fs::remove_file(&p);
    }
    crate::logger::push("[admin] 节日 LOGO 已删除,恢复默认".to_string());
    Response::json(200, r#"{"status":"ok"}"#)
}

/// favicon:现场获取——favicon.ico 优先,失败解析页面 <link rel="icon"> 抓真实路径,
/// 再失败返回域名首字母动态图标(每个站不同,不再统一蓝色 P)
fn api_favicon(ctx: &ServerCtx, domain: &str) -> Response {
    let domain = domain.trim().to_lowercase();
    if domain.is_empty() {
        return Response::json(404, r#"{"error":"缺少域名"}"#);
    }
    // 懒加载:未缓存则尝试抓取 favicon.ico
    if ctx.crawler.favicon(&domain).is_none() {
        ctx.crawler.ensure_favicon(&domain);
    }
    let mut bytes = ctx.crawler.favicon(&domain);
    if bytes.is_none() {
        // 现场获取:解析页面 <link rel="icon"> 抓真实 favicon(很多站不在根目录)
        bytes = fetch_favicon_from_page(&domain);
        if let Some(b) = &bytes {
            ctx.crawler.favicons.lock().unwrap().insert(domain.clone(), b.clone());
            ctx.crawler.favicon_order.lock().unwrap().push_back(domain.clone());
        }
    }
    if let Some(bytes) = bytes {
        crate::http::Response {
            status: 200,
            content_type: "image/x-icon",
            body: bytes,
            extra_headers: vec![("Cache-Control".to_string(), "public, max-age=3600".to_string())],
        }
    } else {
        // 兜底:域名首字母动态图标(现场生成,每站不同)
        crate::http::Response {
            status: 200,
            content_type: "image/svg+xml",
            body: crate::http::letter_icon_svg(&domain).into_bytes(),
            extra_headers: vec![("Cache-Control".to_string(), "public, max-age=3600".to_string())],
        }
    }
}

/// 现场解析页面 <link rel="icon"> 抓取真实 favicon 路径
fn fetch_favicon_from_page(domain: &str) -> Option<Vec<u8>> {
    let url = format!("http://{}/", domain);
    let (status, html) = crate::http::http_get(&url, 3000, crate::crawler::CRAWLER_UA).ok()?;
    if status != 200 {
        return None;
    }
    let lower = html.to_lowercase();
    for rel in ["rel=\"icon\"", "rel=\"shortcut icon\""] {
        let mut pos = 0;
        while let Some(i) = lower[pos..].find(rel) {
            let tag_start = pos + i;
            let link_start = lower[..tag_start].rfind("<link").unwrap_or(0);
            let tag_end = lower[tag_start..].find('>').map(|e| tag_start + e).unwrap_or(lower.len());
            let tag = &lower[link_start..tag_end];
            if let Some(hi) = tag.find("href=\"") {
                let hs = hi + 6;
                let he = tag[hs..].find('"').map(|e| hs + e).unwrap_or(tag.len());
                let href = tag[hs..he].trim();
                if !href.is_empty() {
                    let abs = if href.starts_with("http") {
                        href.to_string()
                    } else if href.starts_with("//") {
                        format!("http:{}", href)
                    } else if href.starts_with('/') {
                        format!("http://{}{}", domain, href)
                    } else {
                        format!("http://{}/{}", domain, href)
                    };
                    if abs.starts_with("http://") || abs.starts_with("https://") {
                        if let Ok((s, body)) = crate::http::http_get(&abs, 3000, crate::crawler::CRAWLER_UA) {
                            if s == 200 && !body.is_empty() && body.len() <= 256 * 1024 {
                                return Some(body.into_bytes());
                            }
                        }
                    }
                }
            }
            pos = tag_end + 1;
        }
    }
    None
}

/// 爬虫输出目录
fn crawl_out_dir() -> std::path::PathBuf {
    crate::config::sites_dir()
        .parent()
        .map(|p| p.join("crawled"))
        .unwrap_or_else(|| std::path::PathBuf::from("out/crawled"))
}

/// 白名单域名(whitelist.sorl 国内基础名单;桌面文件缺失时返回空)
fn whitelist_domains() -> Vec<String> {
    let candidates = [
        "C:\\Users\\Admin\\OneDrive\\Desktop\\whitelist.sorl",
        "whitelist.sorl",
        "config\\whitelist.sorl",
    ];
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(path) {
            let domains = crate::crawler::parse_whitelist(&text);
            if !domains.is_empty() {
                return domains;
            }
        }
    }
    Vec::new()
}

fn admin_crawl_status(ctx: &ServerCtx) -> Response {
    let st = ctx.crawler.stats.lock().unwrap().clone();
    Response::json(
        200,
        &Json::build(vec![
            ("fetched", Json::num(st.fetched as f64)),
            ("discovered", Json::num(st.discovered as f64)),
            ("failed", Json::num(st.failed as f64)),
            ("redirects", Json::num(st.redirects as f64)),
            ("skipped_robots", Json::num(st.skipped_robots as f64)),
            ("elapsed_secs", Json::num(st.elapsed_secs)),
        ])
        .to_string(),
    )
}

/// 执行定时任务(调度线程调用,独立线程运行)
fn run_task(ctx: &Arc<ServerCtx>, task: &crate::tasks::Task) {
    match task.kind.as_str() {
        "scan" => {
            let max_len = task.params.get("max_len").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
            let workers = task.params.get("workers").and_then(|v| v.as_u64()).unwrap_or(64) as usize;
            let max_len = max_len.clamp(1, 20);
            {
                let mut scan = ctx.scan.lock().unwrap();
                if scan.running {
                    crate::logger::push("[task] 定时扫描跳过(已有扫描在运行)");
                    return;
                }
                *scan = ScanState::default();
                scan.running = true;
                scan.started_ts = now_secs();
                scan.max_len = max_len;
            }
            start_scan_thread(ctx, 1, max_len, workers);
            crate::logger::push(format!("[task] 定时扫描启动: max_len={} workers={}", max_len, workers));
        }
        "rebuild" => {
            match ctx.engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir()) {
                Ok(n) => crate::logger::push(format!("[task] 定时重建完成: {} 站点", n)),
                Err(e) => crate::logger::push(format!("[task] 定时重建跳过/失败: {}", e)),
            }
        }
        "crawl" => {
            let disc_path = crate::config::index_dir().join("discovered.txt");
            let seeds: Vec<String> = std::fs::read_to_string(&disc_path)
                .map(|text| text.lines().map(|l| format!("http://{}/", l.trim())).filter(|s| s.len() > 8).collect())
                .unwrap_or_default();
            if seeds.is_empty() {
                crate::logger::push("[task] 定时爬虫跳过(无种子,先重建索引发现外链)");
            } else {
                let crawled_dir = crawl_out_dir();
                let stats = ctx.crawler.crawl(&seeds, &crawled_dir);
                crate::logger::push(format!("[task] 定时爬虫完成: 抓取 {} 发现 {} 失败 {} ({:.0}s)", stats.fetched, stats.discovered, stats.failed, stats.elapsed_secs));
                // 爬完重建索引收录新站
                match ctx.engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir()) {
                    Ok(n) => crate::logger::push(format!("[task] 爬虫后重建完成: {} 站", n)),
                    Err(e) => crate::logger::push(format!("[task] 爬虫后重建跳过: {}", e)),
                }
            }
        }
        _ => {}
    }
    ctx.tasks.mark_run(&task.id);
}

/// 启动定时任务调度线程(每 30 秒检查一次)
pub fn spawn_scheduler(ctx: &Arc<ServerCtx>) {
    let ctx_sched = ctx.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        let due = ctx_sched.tasks.due_tasks();
        if due.is_empty() {
            continue;
        }
        for task in due {
            // 触发时立即标记运行,防止长任务(rebuild 数分钟)期间重复触发
            ctx_sched.tasks.mark_run(&task.id);
            let ctx_task = ctx_sched.clone();
            std::thread::spawn(move || run_task(&ctx_task, &task));
        }
    });
}

// ---------------- 管理员 API ----------------

fn scan_state_json(s: &ScanState) -> Json {
    Json::build(vec![
        ("running", Json::Bool(s.running)),
        ("finished", Json::Bool(s.finished)),
        ("error", match &s.error {
            Some(e) => Json::str(e),
            None => Json::Null,
        }),
        ("started_ts", Json::num(s.started_ts as f64)),
        ("finished_ts", Json::num(s.finished_ts as f64)),
        ("max_len", Json::num(s.max_len as f64)),
        ("grand_total", Json::str(s.grand_total.to_string())),
        ("total", Json::num(s.total as f64)),
        ("registered", Json::num(s.registered as f64)),
        ("available", Json::num(s.available as f64)),
        ("errors", Json::num(s.errors as f64)),
        ("skipped", Json::num(s.skipped as f64)),
        ("sites", Json::num(s.sites as f64)),
        ("elapsed_secs", Json::num(s.elapsed_secs)),
    ])
}

fn admin_status(ctx: &ServerCtx) -> Response {
    let scan = ctx.scan.lock().unwrap().clone();
    let tld = read_file_text("config/tld.list");
    let dns = read_file_text("config/dns.list");
    let j = Json::build(vec![
        ("role", Json::str("admin")),
        ("scan", scan_state_json(&scan)),
        ("config", Json::build(vec![
            ("tld_file", Json::str(&tld)),
            ("dns_file", Json::str(&dns)),
            ("tld_count", Json::num(count_words(&tld) as f64)),
            ("dns_count", Json::num(count_words(&dns) as f64)),
        ])),
    ]);
    Response::json(200, &j.to_string())
}

fn admin_scan_status(ctx: &ServerCtx) -> Response {
    let scan = ctx.scan.lock().unwrap().clone();
    Response::json(200, &scan_state_json(&scan).to_string())
}

fn admin_scan(ctx: &ServerCtx, req: &Request) -> Response {
    // 解析 body: {"max_len":2, "min_len":1, "workers":64}
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let max_len = params
        .get("max_len")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .filter(|v| (1..=20).contains(v))
        .ok_or_json("参数 max_len 缺失或非法(范围 1-20)");
    let max_len = match max_len {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let min_len = params.get("min_len").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(1).min(max_len);
    let workers = params
        .get("workers")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .filter(|v| (1..=512).contains(v))
        .unwrap_or(64);

    // 防重复触发
    {
        let mut scan = ctx.scan.lock().unwrap();
        if scan.running {
            return Response::json(409, r#"{"error":"已有扫描任务在运行"}"#);
        }
        *scan = ScanState::default();
        scan.running = true;
        scan.started_ts = now_secs();
        scan.max_len = max_len;
    }
    start_scan_thread(ctx, min_len, max_len, workers);

    Response::json(202, &Json::build(vec![("status", Json::str("scan_started")), ("max_len", Json::num(max_len as f64))]).to_string())
}

/// 启动后台穷举扫描线程(管理 API 与定时任务共用)
fn start_scan_thread(ctx: &ServerCtx, min_len: usize, max_len: usize, workers: usize) {
    let engine = ctx.engine.clone();
    let scan_state = ctx.scan.clone();
    std::thread::spawn(move || {
        let result = run_scan_job(engine.clone(), scan_state.clone(), min_len, max_len, workers);
        let mut s = scan_state.lock().unwrap();
        s.running = false;
        s.finished = true;
        s.finished_ts = now_secs();
        s.elapsed_secs = (s.finished_ts - s.started_ts) as f64;
        match result {
            Ok(stats) => {
                s.total = stats.total;
                s.registered = stats.registered;
                s.available = stats.available;
                s.errors = stats.errors;
                s.skipped = stats.skipped;
                s.sites = stats.sites;
            }
            Err(e) => s.error = Some(e),
        }
        // 扫描完成后自动重建索引,让新站点立即可搜
        let _ = engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir());
    });
}

/// 在后台线程执行穷举扫描(独立读配置,仅覆盖 min/max_len 与 workers)
fn run_scan_job(
    _engine: Arc<SearchEngine>,
    scan_state: Arc<Mutex<ScanState>>,
    min_len: usize,
    max_len: usize,
    workers: usize,
) -> Result<crate::engine::Stats, String> {
    let mut cfg = crate::config::load_config(Path::new("config/engine.conf"))?;
    cfg.min_len = min_len;
    cfg.max_len = max_len;
    cfg.workers = workers;
    println!("[admin] 管理员触发穷举: 位数=[{},{}] workers={}", min_len, max_len, workers);
    let eng = crate::engine::Engine::load(cfg)?;
    // 设置任务总数(供进度条百分比)
    {
        let mut s = scan_state.lock().unwrap();
        s.grand_total = eng.grand_total();
    }
    // 进度回调:周期性把统计写入共享状态(Web UI 轮询展示)
    let mut progress = |st: &crate::engine::Stats| {
        let mut s = scan_state.lock().unwrap();
        s.total = st.total;
        s.registered = st.registered;
        s.available = st.available;
        s.errors = st.errors;
        s.skipped = st.skipped;
        s.sites = st.sites;
    };
    let stats = eng.run(false, Some(&mut progress))?;
    println!(
        "[admin] 穷举完成: 总数={} 可用={} 已注册={} 失败={} 建站={}",
        stats.total, stats.available, stats.registered, stats.errors, stats.sites
    );
    Ok(stats)
}

fn admin_config_get(ctx: &ServerCtx) -> Response {
    let _ = ctx;
    let j = Json::build(vec![
        ("tld", Json::str(&read_file_text("config/tld.list"))),
        ("dns", Json::str(&read_file_text("config/dns.list"))),
    ]);
    Response::json(200, &j.to_string())
}

fn admin_config_save(_ctx: &ServerCtx, req: &Request, kind: &str) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
    let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let content = content.trim();
    if content.is_empty() {
        return Response::json(400, r#"{"error":"content 不能为空"}"#);
    }
    // 校验:内容必须是合法条目(去注释后至少 1 个词)
    let words = content
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .flat_map(|l| l.split_whitespace())
        .filter(|t| !t.starts_with('#'))
        .count();
    if words == 0 {
        return Response::json(400, r#"{"error":"内容中没有任何有效条目"}"#);
    }

    let path = match kind {
        "tld" => "config/tld.list",
        "dns" => "config/dns.list",
        _ => return Response::json(400, r#"{"error":"未知配置类型"}"#),
    };
    let text = if kind == "dns" {
        // DNS 列表:每行一个,规范化
        let mut out = String::from("# PILSEOCORE DNS 服务器列表(每行一个,随机负载均衡)\n");
        for w in content.split_whitespace() {
            out.push_str(w);
            out.push('\n');
        }
        out
    } else {
        content.to_string() + "\n"
    };
    if let Err(e) = std::fs::write(path, text) {
        return Response::json(500, &Json::build(vec![("status", Json::str("error")), ("message", Json::str(e.to_string()))]).to_string());
    }
    Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("file", Json::str(path)), ("entries", Json::num(words as f64))]).to_string())
}

fn admin_rebuild(ctx: &ServerCtx) -> Response {
    if ctx.engine.is_rebuilding() {
        crate::logger::push("[admin] 重建请求跳过(已有重建进行中)");
        return Response::json(409, r#"{"status":"skipped","message":"已有索引重建在进行中,本次跳过"}"#);
    }
    crate::logger::push("[admin] 重建索引开始...");
    match ctx.engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir()) {
        Ok(n) => Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("sites", Json::num(n as f64))]).to_string()),
        Err(e) => {
            crate::logger::push(format!("[admin] 重建失败: {}", e));
            Response::json(500, &Json::build(vec![("status", Json::str("error")), ("message", Json::str(&e))]).to_string())
        }
    }
}

// ---------------- 工具 ----------------

fn read_file_text(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn count_words(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .flat_map(|l| l.split_whitespace())
        .filter(|t| !t.starts_with('#'))
        .count()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 便捷 Result 转换:None -> 400 Response
trait JsonErr {
    fn ok_or_json(self, msg: &'static str) -> Result<usize, Response>;
}
impl JsonErr for Option<usize> {
    fn ok_or_json(self, msg: &'static str) -> Result<usize, Response> {
        match self {
            Some(v) => Ok(v),
            None => Err(Response::json(400, &format!(r#"{{"error":"{}"}}"#, msg))),
        }
    }
}
