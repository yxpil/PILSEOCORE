//! PILSEOCORE —— SEO 自动域名穷举引擎 + 本地搜索引擎(CLI 薄壳)
//!
//! 全部核心能力在 pilseocore 库中;本二进制仅提供命令行入口:
//!   (默认)             穷举模式:字符集穷举 x 后缀大全 x 多DNS并发解析 x 自动建站
//!   serve [--port N]   启动本地搜索引擎(Web UI + API,自动加载/重建索引)
//!   index              手动重建索引(扫描 out/sites 提取标题/meta,生成 sitemap.xml)
//!   search <关键词>     CLI 搜索
//!   mcp                启动 MCP Server(stdio,供 AI 客户端调用)

use pilseocore::{ai, auth, blacklist, config, crawler, dns, engine, http, index, logger, mcp, search, server, stats, tasks, tokenizer};

use std::process::exit;
use std::sync::Arc;
use std::time::Instant;

// 默认目录常量(可用环境变量 PILSEO_SITES_DIR / PILSEO_INDEX_DIR 覆盖,见 config::sites_dir/index_dir)

fn usage() {
    println!(
        r#"PILSEOCORE —— SEO 自动域名穷举引擎 + 本地搜索引擎 v0.1.0

穷举模式(默认):
  pilseocore [--config <file>] [--min-len N] [--max-len N] [--workers N]
             [--charset <s>] [--dry-run] [--no-sites] [--check <域名>]

本地搜索引擎:
  pilseocore serve [--port 8896]   启动搜索服务(Web UI + API + 自动索引)
  pilseocore index                 重建索引(扫描站点,生成 sitemap.xml)
  pilseocore search "关键词"        CLI 搜索
  pilseocore mcp                    MCP Server(stdio)

示例:
  pilseocore --max-len 2 --workers 128    # 穷举 2 位域名
  pilseocore serve --port 8896            # 启动搜索引擎
  pilseocore search 智能家居              # 搜索
"#
    );
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("[error] {}", e);
        exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 子命令分发
    if let Some(cmd) = args.first() {
        match cmd.as_str() {
            "-h" | "--help" => {
                usage();
                return Ok(());
            }
            "serve" => return cmd_serve(&args[1..]),
            "index" => return cmd_index(),
            "search" => {
                let q = args.get(1).cloned().unwrap_or_default();
                return cmd_search(&q);
            }
            "mcp" => return cmd_mcp(&args[1..]),
            "tokenizer-train" => return cmd_tokenizer_train(),
            _ => {}
        }
    }

    cmd_enumerate(&args)
}

// ---------------- 穷举模式(原有) ----------------

fn cmd_enumerate(args: &[String]) -> Result<(), String> {
    let mut config_path = std::path::PathBuf::from("config/engine.conf");
    let mut min_len: Option<usize> = None;
    let mut max_len: Option<usize> = None;
    let mut workers: Option<usize> = None;
    let mut charset: Option<String> = None;
    let mut dry_run = false;
    let mut no_sites = false;
    let mut check: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                config_path = args.get(i).ok_or("--config 缺少参数")?.into();
            }
            "--min-len" => {
                i += 1;
                min_len = Some(parse_num(args.get(i).ok_or("--min-len 缺少参数")?, "--min-len")?);
            }
            "--max-len" => {
                i += 1;
                max_len = Some(parse_num(args.get(i).ok_or("--max-len 缺少参数")?, "--max-len")?);
            }
            "--workers" => {
                i += 1;
                workers = Some(parse_num(args.get(i).ok_or("--workers 缺少参数")?, "--workers")?);
            }
            "--charset" => {
                i += 1;
                charset = Some(args.get(i).ok_or("--charset 缺少参数")?.clone());
            }
            "--dry-run" => dry_run = true,
            "--no-sites" => no_sites = true,
            "--check" => {
                i += 1;
                check = Some(args.get(i).ok_or("--check 缺少参数")?.clone());
            }
            "-h" | "--help" => {
                usage();
                return Ok(());
            }
            other => return Err(format!("未知参数: {}", other)),
        }
        i += 1;
    }

    let mut cfg = config::load_config(&config_path)?;
    if let Some(v) = min_len {
        cfg.min_len = v;
    }
    if let Some(v) = max_len {
        cfg.max_len = v;
    }
    if let Some(v) = workers {
        cfg.workers = v;
    }
    if let Some(v) = charset {
        cfg.charset = v;
    }
    if no_sites {
        cfg.build_sites = false;
    }

    if let Some(fqdn) = check {
        return check_one(&cfg, &fqdn);
    }

    let engine = engine::Engine::load(cfg)?;
    println!("[信息] {}", engine.summary());
    if dry_run {
        println!("[信息] 干跑模式:不查询 DNS,仅验证穷举顺序");
    }
    let stats = engine.run(dry_run, None)?;
    println!("\n===== 完成 ===== 耗时 {:.1}s", stats.elapsed);
    println!("穷举总数    : {}", stats.total);
    println!("已注册      : {}", stats.registered);
    println!("可用        : {}", stats.available);
    println!("无法判定    : {}", stats.errors);
    println!("黑名单跳过  : {}", stats.skipped);
    println!("已建站      : {}", stats.sites);
    if stats.available > 0 {
        println!("可用域名列表: out/available.txt");
    }
    if stats.sites > 0 {
        println!("提示: 运行 `pilseocore index` 建立搜索索引,`pilseocore serve` 启动本地搜索引擎");
    }
    Ok(())
}

fn parse_num(s: &str, arg: &str) -> Result<usize, String> {
    s.parse().map_err(|_| format!("{} 参数非法: {}", arg, s))
}

fn check_one(cfg: &config::Config, fqdn: &str) -> Result<(), String> {
    let mut dns_servers = config::load_space_list(&cfg.dns_file)?;
    if dns_servers.is_empty() {
        return Err("DNS 列表为空".into());
    }
    dns::normalize_dns_servers(&mut dns_servers);
    let sock = std::net::UdpSocket::bind(("0.0.0.0", 0)).map_err(|e| format!("创建 socket 失败: {}", e))?;
    println!(
        "[信息] 探测 {}  DNS={} 类型={}",
        fqdn,
        dns_servers.len(),
        cfg.qtypes.len()
    );
    let start = Instant::now();
    let st = dns::probe(fqdn, &dns_servers, &cfg.qtypes, cfg.dns_timeout_ms, &sock);
    println!("结果: {} (耗时 {:.0}ms)", st.label(), start.elapsed().as_millis());
    Ok(())
}

// ---------------- 本地搜索引擎 ----------------

/// 加载或构建索引(含黑名单初始化;目录支持环境变量 PILSEO_SITES_DIR / PILSEO_INDEX_DIR)
fn load_or_build_index(cfg: &config::Config, rebuild: bool) -> Result<Arc<search::SearchEngine>, String> {
    let sites_dir = config::sites_dir();
    let index_dir = config::index_dir();
    let blacklist = Arc::new(blacklist::Blacklist::load(&index_dir));
    let meta_path = index_dir.join("meta.json");
    let need_build = rebuild || !meta_path.exists();
    let idx = if need_build {
        println!("[index] 扫描 {} 并重建索引...", sites_dir.display());
        index::SiteIndex::build(&sites_dir, &index_dir, &blacklist)?
    } else {
        println!("[index] 加载已有索引 {}", index_dir.display());
        index::SiteIndex::load(&index_dir)?
    };
    let (sites, terms, blocks) = idx.stats();
    println!("[index] 站点={} 词={} 分块={} 黑名单={}", sites, terms, blocks, blacklist.blocked_count());
    Ok(Arc::new(search::SearchEngine::new(idx, cfg.hot_cache_size, cfg.hot_cache_ttl, blacklist)))
}

fn cmd_serve(args: &[String]) -> Result<(), String> {
    let mut cfg = config::load_config(&std::path::Path::new("config/engine.conf"))?;
    let mut port = cfg.server_port;
    let mut rebuild = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = parse_num(args.get(i).ok_or("--port 缺少参数")?, "--port")? as u16;
            }
            "--rebuild" => rebuild = true,
            "-h" | "--help" => {
                usage();
                return Ok(());
            }
            other => return Err(format!("serve 未知参数: {}", other)),
        }
        i += 1;
    }
    cfg.server_port = port;

    let engine = load_or_build_index(&cfg, rebuild)?;
    let ai_cfg = ai::AiConfig {
        enabled: cfg.ai_enabled,
        endpoint: cfg.ai_endpoint.clone(),
        model: cfg.ai_model.clone(),
        api_key: cfg.ai_api_key.clone(),
        timeout_ms: 30000,
    };
    if ai_cfg.enabled {
        println!("[ai] 已启用,端点={} 模型={}", ai_cfg.endpoint, ai_cfg.model);
    } else {
        println!("[ai] 未启用(engine.conf 中 ai_enabled = true 可开启 AI 摘要)");
    }
    if cfg.admin_user.is_empty() || cfg.admin_pass.is_empty() {
        println!("[admin] 管理功能未启用(engine.conf 配置 admin_user/admin_pass 后,管理员可登录并签发 API token)");
    } else {
        println!("[admin] 管理账号: {} (登录 Web UI 管理面板签发 API/MCP token)", cfg.admin_user);
    }

    let tokens = auth::TokenStore::load(std::path::Path::new("data"));
    let sessions = auth::Sessions::new(12 * 3600); // 会话 12 小时
    // 爬虫(CPU 自适应并发)、定时任务、搜索统计
    let crawler = Arc::new(crawler::Crawler::new(3, 5000, 200, 5000, 0));
    let tasks = Arc::new(tasks::TaskScheduler::load(&config::index_dir()));
    let stats = Arc::new(stats::StatsCollector::new(&config::index_dir()));
    let ctx = Arc::new(server::ServerCtx::new(
        engine,
        ai_cfg,
        cfg.admin_user.clone(),
        cfg.admin_pass.clone(),
        tokens,
        sessions,
        crawler,
        tasks,
        stats,
    ));
    println!("[tasks] 定时任务调度线程运行中(管理后台'定时任务'可添加)");
    server::spawn_scheduler(&ctx);
    logger::init();
    logger::push(format!("[serve] 服务已启动: http://127.0.0.1:{}(端口 8896 固定)", port));
    let addr = format!("127.0.0.1:{}", port);
    http::serve(&addr, move |req| {
        let ctx = ctx.clone();
        server::handle(&ctx, req)
    })
}

fn cmd_index() -> Result<(), String> {
    let sites_dir = config::sites_dir();
    let index_dir = config::index_dir();
    let blacklist = blacklist::Blacklist::load(&index_dir);
    let start = Instant::now();
    let idx = index::SiteIndex::build(&sites_dir, &index_dir, &blacklist)?;
    let (sites, terms, blocks) = idx.stats();
    println!("[index] 完成: 站点={} 词={} 分块={} 黑名单={} 耗时={:.2}s", sites, terms, blocks, blacklist.blocked_count(), start.elapsed().as_secs_f64());
    println!("[index] 已生成各站点 sitemap.xml,索引位于 {}", index_dir.display());
    Ok(())
}

/// 从站点语料训练 BPE 分词器(81920 词表),保存到 data/index/tokenizer/vocab.json
fn cmd_tokenizer_train() -> Result<(), String> {
    let sites_dir = config::sites_dir();
    let data_dir = config::index_dir();
    let corpus = index::collect_corpus(&sites_dir);
    println!("[tokenizer] 语料样本 {} 条,训练 {} 词表...", corpus.len(), tokenizer::VOCAB_SIZE);
    let start = Instant::now();
    let tok = tokenizer::BpeTokenizer::train(&corpus, tokenizer::VOCAB_SIZE);
    let vocab_path = data_dir.join("tokenizer").join("vocab.json");
    tok.save(&vocab_path)?;
    println!("[tokenizer] 完成: {} tokens,耗时 {:.1}s,已保存 {}", tok.vocab_size(), start.elapsed().as_secs_f64(), vocab_path.display());
    Ok(())
}

fn cmd_search(q: &str) -> Result<(), String> {
    let cfg = config::load_config(&std::path::Path::new("config/engine.conf"))?;
    let engine = load_or_build_index(&cfg, false)?;
    if q.is_empty() {
        return Err("用法: pilseocore search \"关键词\"".into());
    }
    let start = Instant::now();
    let (total, hits) = engine.search(q, 1, 10);
    let ms = start.elapsed().as_millis();
    println!("查询: {}  找到 {} 条 ({}ms)", q, total, ms);
    for (i, h) in hits.iter().enumerate() {
        let fold = if h.fold_count > 1 { format!(" (另有 {} 个相同标题)", h.fold_count - 1) } else { String::new() };
        println!("{}. {} [{}]{}", i + 1, h.title, h.url, fold);
        if !h.description.is_empty() {
            println!("   {}", h.description);
        }
    }
    Ok(())
}

fn cmd_mcp(args: &[String]) -> Result<(), String> {
    // MCP 需要管理员签发的 token(Web UI 管理面板签发):pilseocore mcp --token <token>
    let mut token: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--token" => {
                i += 1;
                token = Some(args.get(i).ok_or("--token 缺少参数")?.clone());
            }
            "-h" | "--help" => {
                usage();
                return Ok(());
            }
            other => return Err(format!("mcp 未知参数: {}", other)),
        }
        i += 1;
    }
    let cfg = config::load_config(&std::path::Path::new("config/engine.conf"))?;
    let engine = load_or_build_index(&cfg, false)?;
    let tokens = auth::TokenStore::load(std::path::Path::new("data"));
    let token = token.or_else(|| std::env::var("PILSEO_TOKEN").ok());
    let Some(token) = token else {
        return Err(
            "MCP 需要管理员签发的 token:\n  pilseocore mcp --token <token>\n(在 Web UI 管理面板签发;也可设环境变量 PILSEO_TOKEN)".into(),
        );
    };
    if !tokens.verify(&token) {
        return Err("MCP token 无效:请在 Web UI 管理面板重新签发".into());
    }
    println!("[mcp] PILSEOCORE MCP Server 已启动(stdio,管理员 token 已验证)");
    mcp::serve(&engine)
}
