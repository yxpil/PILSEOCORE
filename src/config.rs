//! 配置加载:解析 engine.conf(key = value 格式)与各列表文件

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Config {
    pub charset: String,
    pub min_len: usize,
    pub max_len: usize,
    pub tld_file: PathBuf,
    pub dns_file: PathBuf,
    pub keywords_file: PathBuf,
    pub block_file: PathBuf,
    pub workers: usize,
    pub dns_per_domain: usize,
    pub dns_timeout_ms: u64,
    pub qtypes: Vec<u16>,
    pub out_dir: PathBuf,
    pub build_sites: bool,
    // 本地搜索引擎
    pub server_port: u16,
    pub hot_cache_size: usize,
    pub hot_cache_ttl: u64,
    pub ai_enabled: bool,
    pub ai_endpoint: String,
    pub ai_model: String,
    pub ai_api_key: String,
    // 管理员控制:账号密码登录 Web UI;token 由管理员签发给 API/MCP
    pub admin_user: String,
    pub admin_pass: String,
    /// 前端是否显示管理员入口(0=隐藏,仍可通过 /admin 直达;默认 1 显示)
    pub admin_show: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            charset: "1234567890abcdefghijklmnopqrstuvwxyz".into(),
            min_len: 1,
            max_len: 2,
            tld_file: "config/tld.list".into(),
            dns_file: "config/dns.list".into(),
            keywords_file: "config/keywords.txt".into(),
            block_file: "config/back.list".into(),
            workers: 64,
            dns_per_domain: 5,
            dns_timeout_ms: 2000,
            qtypes: vec![1, 28, 2], // A AAAA NS
            out_dir: "out".into(),
            build_sites: true,
            server_port: 8896,
            hot_cache_size: 1000,
            hot_cache_ttl: 60,
            ai_enabled: false,
            ai_endpoint: "http://127.0.0.1:11434/v1/chat/completions".into(),
            ai_model: "qwen3:8b".into(),
            ai_api_key: String::new(),
            admin_user: "admin".into(),
            admin_pass: String::new(),
            admin_show: true,
        }
    }
}

/// 解析 key = value 配置文件,支持 # 注释与空行
pub fn parse_key_value(path: &Path) -> Result<HashMap<String, String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("无法读取配置文件 {}: {}", path.display(), e))?;
    let mut map = HashMap::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else {
            return Err(format!("{}:{} 格式错误(应为 key = value): {}", path.display(), lineno + 1, line));
        };
        let key = line[..eq].trim().to_string();
        let val = line[eq + 1..].trim().to_string();
        map.insert(key, val);
    }
    Ok(map)
}

/// 读取列表文件:按空白(空格/制表/换行)分割,支持 # 注释行,保留文件内顺序
pub fn load_space_list(path: &Path) -> Result<Vec<String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("无法读取列表文件 {}: {}", path.display(), e))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for tok in line.split_whitespace() {
            let tok = tok.trim();
            if !tok.is_empty() && !tok.starts_with('#') {
                out.push(tok.to_string());
            }
        }
    }
    Ok(out)
}

/// 读取黑名单文件:每行一个片段(空行与 # 注释忽略)
pub fn load_block_list(path: &Path) -> Result<Vec<String>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .map_err(|e| format!("无法读取黑名单文件 {}: {}", path.display(), e))?;
    Ok(text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_lowercase())
        .collect())
}

pub fn load_config(path: &Path) -> Result<Config, String> {
    let mut cfg = Config::default();
    if !path.exists() {
        return Err(format!("配置文件不存在: {}", path.display()));
    }
    let kv = parse_key_value(path)?;
    for (k, v) in &kv {
        match k.as_str() {
            "charset" => cfg.charset = v.clone(),
            "min_len" => cfg.min_len = v.parse().map_err(|_| format!("min_len 非法: {}", v))?,
            "max_len" => cfg.max_len = v.parse().map_err(|_| format!("max_len 非法: {}", v))?,
            "tld_file" => cfg.tld_file = PathBuf::from(v),
            "dns_file" => cfg.dns_file = PathBuf::from(v),
            "keywords_file" => cfg.keywords_file = PathBuf::from(v),
            "block_file" => cfg.block_file = PathBuf::from(v),
            "workers" => cfg.workers = v.parse().map_err(|_| format!("workers 非法: {}", v))?,
            "dns_per_domain" => cfg.dns_per_domain = v.parse().map_err(|_| format!("dns_per_domain 非法: {}", v))?,
            "dns_timeout_ms" => cfg.dns_timeout_ms = v.parse().map_err(|_| format!("dns_timeout_ms 非法: {}", v))?,
            "qtypes" => {
                let mut qs = Vec::new();
                for t in v.split_whitespace() {
                    let code = match t.to_uppercase().as_str() {
                        "A" => crate::dns::QTYPE_A,
                        "NS" => crate::dns::QTYPE_NS,
                        "CNAME" => crate::dns::QTYPE_CNAME,
                        "MX" => crate::dns::QTYPE_MX,
                        "TXT" => crate::dns::QTYPE_TXT,
                        "AAAA" => crate::dns::QTYPE_AAAA,
                        other => return Err(format!("未知 DNS 记录类型: {}", other)),
                    };
                    qs.push(code);
                }
                if !qs.is_empty() {
                    cfg.qtypes = qs;
                }
            }
            "out_dir" => cfg.out_dir = PathBuf::from(v),
            "build_sites" => cfg.build_sites = parse_bool(v).map_err(|_| format!("build_sites 应为 true/false: {}", v))?,
            "server_port" => cfg.server_port = v.parse().map_err(|_| format!("server_port 非法: {}", v))?,
            "hot_cache_size" => cfg.hot_cache_size = v.parse().map_err(|_| format!("hot_cache_size 非法: {}", v))?,
            "hot_cache_ttl" => cfg.hot_cache_ttl = v.parse().map_err(|_| format!("hot_cache_ttl 非法: {}", v))?,
            "ai_enabled" => cfg.ai_enabled = parse_bool(v).map_err(|_| format!("ai_enabled 应为 true/false: {}", v))?,
            "ai_endpoint" => cfg.ai_endpoint = v.clone(),
            "ai_model" => cfg.ai_model = v.clone(),
            "ai_api_key" => cfg.ai_api_key = v.clone(),
            "admin_user" => cfg.admin_user = v.clone(),
            "admin_pass" => cfg.admin_pass = v.clone(),
            "admin_show" => cfg.admin_show = parse_bool(v).map_err(|_| format!("admin_show 应为 true/false: {}", v))?,
            _ => eprintln!("[warn] 忽略未知配置项: {}", k),
        }
    }
    if cfg.min_len == 0 {
        cfg.min_len = 1;
    }
    if cfg.max_len < cfg.min_len {
        return Err(format!("max_len({}) 不能小于 min_len({})", cfg.max_len, cfg.min_len));
    }
    if cfg.charset.is_empty() {
        return Err("charset 不能为空".into());
    }
    if cfg.workers == 0 {
        cfg.workers = 1;
    }
    if cfg.dns_per_domain == 0 {
        cfg.dns_per_domain = 1;
    }
    Ok(cfg)
}

/// 保存 admin_show 到 engine.conf(运行时开关,保留其它配置)
pub fn save_admin_show(visible: bool) -> Result<(), String> {
    let path = std::path::Path::new("config/engine.conf");
    let text = fs::read_to_string(path).map_err(|e| format!("读取配置失败: {}", e))?;
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let key = "admin_show";
    let mut found = false;
    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(eq) = trimmed.find('=') {
            if trimmed[..eq].trim() == key {
                *line = format!("{} = {}", key, if visible { "true" } else { "false" });
                found = true;
                break;
            }
        }
    }
    if !found {
        lines.push(format!("{} = {}", key, if visible { "true" } else { "false" }));
    }
    fs::write(path, lines.join("\n") + "\n").map_err(|e| format!("写入配置失败: {}", e))
}

fn parse_bool(s: &str) -> Result<bool, ()> {
    match s.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" | "" => Ok(false),
        _ => Err(()),
    }
}

/// 站点输出目录(支持 PILSEO_SITES_DIR 环境变量覆盖,便于嵌入式/测试)
pub fn sites_dir() -> std::path::PathBuf {
    std::env::var("PILSEO_SITES_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("out/sites"))
}

/// 索引数据目录(支持 PILSEO_INDEX_DIR 环境变量覆盖)
pub fn index_dir() -> std::path::PathBuf {
    std::env::var("PILSEO_INDEX_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("data/index"))
}
