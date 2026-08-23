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
    pub tokens: TokenStore,
    pub sessions: Sessions,
    pub scan: Arc<Mutex<ScanState>>,
    /// 爬虫(外链发现抓取)
    pub crawler: Arc<crate::crawler::Crawler>,
    /// 定时任务调度器
    pub tasks: Arc<crate::tasks::TaskScheduler>,
}

impl ServerCtx {
    pub fn new(
        engine: Arc<SearchEngine>,
        ai: AiConfig,
        admin_user: String,
        admin_pass: String,
        tokens: TokenStore,
        sessions: Sessions,
        crawler: Arc<crate::crawler::Crawler>,
        tasks: Arc<crate::tasks::TaskScheduler>,
    ) -> ServerCtx {
        ServerCtx {
            engine,
            ai,
            admin_user,
            admin_pass,
            tokens,
            sessions,
            scan: Arc::new(Mutex::new(ScanState::default())),
            crawler,
            tasks,
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
        ("POST", "/api/admin/rebuild") => admin_guard(ctx, req, |ctx| admin_rebuild(ctx)),
        ("POST", "/api/rebuild") => admin_guard(ctx, req, |ctx| admin_rebuild(ctx)), // 旧路径,现需管理员
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
        _ => Response::not_found(),
    }
}

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
    let want_ai = req.param("ai").map(|v| v == "1" || v == "true").unwrap_or(false);
    if q.is_empty() {
        return Response::json(400, r#"{"error":"缺少参数 q"}"#);
    }
    let start = Instant::now();
    let (total, hits) = ctx.engine.search(&q, page, limit);
    let elapsed_ms = start.elapsed().as_millis();
    let pages = (total + limit - 1) / limit;

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

/// 手动触发爬虫(种子 = 外链发现文件 + 手动附加域名)
fn admin_crawl(ctx: &ServerCtx, req: &Request) -> Response {
    let mut seeds: Vec<String> = Vec::new();
    // 读外链发现种子
    let disc_path = crate::config::index_dir().join("discovered.txt");
    if let Ok(text) = std::fs::read_to_string(&disc_path) {
        for line in text.lines() {
            let d = line.trim();
            if !d.is_empty() {
                seeds.push(format!("http://{}/", d));
            }
        }
    }
    // 手动附加
    let body = String::from_utf8_lossy(&req.body);
    let params = crate::json::parse(&body).unwrap_or(Json::obj());
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
    if seeds.is_empty() {
        return Response::json(400, r#"{"error":"没有种子:先重建索引发现外链,或在 seeds 参数提供"}"#);
    }
    let seeds_len = seeds.len();
    // 后台抓取
    let crawler = ctx.crawler.clone();
    std::thread::spawn(move || {
        let crawled_dir = crate::config::sites_dir().parent().map(|p| p.join("crawled")).unwrap_or_else(|| std::path::PathBuf::from("out/crawled"));
        crawler.crawl(&seeds, &crawled_dir);
    });
    Response::json(202, &Json::build(vec![("status", Json::str("crawl_started")), ("seeds", Json::num(seeds_len as f64))]).to_string())
}

fn admin_crawl_status(ctx: &ServerCtx) -> Response {
    let st = ctx.crawler.stats.lock().unwrap().clone();
    Response::json(
        200,
        &Json::build(vec![
            ("fetched", Json::num(st.fetched as f64)),
            ("discovered", Json::num(st.discovered as f64)),
            ("failed", Json::num(st.failed as f64)),
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
                    println!("[task] 扫描任务跳过(已有扫描在运行)");
                    return;
                }
                *scan = ScanState::default();
                scan.running = true;
                scan.started_ts = now_secs();
                scan.max_len = max_len;
            }
            start_scan_thread(ctx, 1, max_len, workers);
            println!("[task] 定时扫描启动: max_len={} workers={}", max_len, workers);
        }
        "rebuild" => {
            println!("[task] 定时重建索引...");
            match ctx.engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir()) {
                Ok(n) => println!("[task] 重建完成: {} 站点", n),
                Err(e) => println!("[task] 重建失败: {}", e),
            }
        }
        "crawl" => {
            let disc_path = crate::config::index_dir().join("discovered.txt");
            let seeds: Vec<String> = std::fs::read_to_string(&disc_path)
                .map(|text| text.lines().map(|l| format!("http://{}/", l.trim())).filter(|s| s.len() > 8).collect())
                .unwrap_or_default();
            if seeds.is_empty() {
                println!("[task] 定时爬虫跳过(无种子,先重建索引发现外链)");
            } else {
                let crawled_dir = crate::config::sites_dir().parent().map(|p| p.join("crawled")).unwrap_or_else(|| std::path::PathBuf::from("out/crawled"));
                let stats = ctx.crawler.crawl(&seeds, &crawled_dir);
                println!("[task] 爬虫完成: 抓取 {} 发现 {} 失败 {} ({:.0}s)", stats.fetched, stats.discovered, stats.failed, stats.elapsed_secs);
                // 爬完重建索引收录新站
                let _ = ctx.engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir());
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
    match ctx.engine.rebuild(&crate::config::sites_dir(), &crate::config::index_dir()) {
        Ok(n) => Response::json(200, &Json::build(vec![("status", Json::str("ok")), ("sites", Json::num(n as f64))]).to_string()),
        Err(e) => Response::json(500, &Json::build(vec![("status", Json::str("error")), ("message", Json::str(&e))]).to_string()),
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
