//! 爬虫:从种子 URL 出发,BFS 抓取网页,发现更多网站
//!
//! - 抓取:手写 HTTP 客户端(http_get,仅 http:// 明文;https 跳过)
//! - 解析:<a href> + JS 链接(location/window.open/http:// 字符串)提取外链
//! - 去重:URL 规范化 + visited 集合
//! - robots.txt:每域名抓取前检查(Disallow 前缀匹配,缓存)
//! - 限制:最大深度 / 总页数 / 每域名页数 / 超时
//! - 存储:抓到的页面保存到 out/crawled/<domain>/<path>.html,供索引器发现
//!
//! 种子来源:本地站点外链发现(data/discovered.txt)+ 手动 API 添加

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use crate::http::http_get;

pub const CRAWLER_UA: &str = "PilseoCrawler/1.0 (SEO kernel crawler)";

#[derive(Clone, Debug, Default)]
pub struct CrawlStats {
    pub fetched: usize,
    pub discovered: usize,
    pub failed: usize,
    pub skipped_robots: usize,
    pub elapsed_secs: f64,
}

pub struct Crawler {
    visited: Mutex<HashSet<String>>,
    /// domain -> robots Disallow 前缀列表(None = 未获取/不允许抓取判定跳过)
    robots: Mutex<HashMap<String, Vec<String>>>,
    pub stats: Mutex<CrawlStats>,
    max_depth: usize,
    max_pages: usize,
    per_domain: usize,
    timeout_ms: u64,
}

impl Crawler {
    pub fn new(max_depth: usize, max_pages: usize, per_domain: usize, timeout_ms: u64) -> Crawler {
        Crawler {
            visited: Mutex::new(HashSet::new()),
            robots: Mutex::new(HashMap::new()),
            stats: Mutex::new(CrawlStats::default()),
            max_depth: max_depth.max(1),
            max_pages: max_pages.max(1).min(50_000),
            per_domain: per_domain.max(1),
            timeout_ms: timeout_ms.max(500),
        }
    }

    /// 重置状态(新一轮抓取)
    pub fn reset(&self) {
        self.visited.lock().unwrap().clear();
        self.robots.lock().unwrap().clear();
        *self.stats.lock().unwrap() = CrawlStats::default();
    }

    /// 从种子列表开始 BFS 抓取,返回统计
    pub fn crawl(&self, seeds: &[String], out_dir: &Path) -> CrawlStats {
        self.reset();
        let start = std::time::Instant::now();
        fs::create_dir_all(out_dir).ok();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        for s in seeds {
            if let Some(n) = normalize_url(s) {
                queue.push_back((n, 0));
            }
        }
        let mut per_domain_count: HashMap<String, usize> = HashMap::new();

        while let Some((url, depth)) = queue.pop_front() {
            // 总量限制
            {
                let st = self.stats.lock().unwrap();
                if st.fetched >= self.max_pages {
                    break;
                }
            }
            // 去重
            {
                let mut v = self.visited.lock().unwrap();
                if !v.insert(url.clone()) {
                    continue;
                }
            }
            // 域名页数限制
            let domain = domain_of(&url).unwrap_or_default();
            {
                let c = per_domain_count.entry(domain.clone()).or_insert(0);
                if *c >= self.per_domain {
                    continue;
                }
                *c += 1;
            }
            // robots 检查
            if !self.robots_allowed(&domain, &url) {
                self.stats.lock().unwrap().skipped_robots += 1;
                continue;
            }
            // 抓取
            match http_get(&url, self.timeout_ms, CRAWLER_UA) {
                Ok((status, html)) if status == 200 => {
                    if let Some(save_path) = save_page(&url, &html, out_dir) {
                        let _ = save_path;
                    }
                    self.stats.lock().unwrap().fetched += 1;
                    if depth < self.max_depth {
                        for link in extract_links(&html, &url) {
                            let mut st = self.stats.lock().unwrap();
                            st.discovered += 1;
                            drop(st);
                            let mut v = self.visited.lock().unwrap();
                            if !v.contains(&link) {
                                v.insert(link.clone());
                                drop(v);
                                queue.push_back((link, depth + 1));
                            }
                        }
                    }
                }
                Ok((status, _)) if status == 301 || status == 302 => {
                    println!("[crawler] 重定向跳过: {} -> {}", url, status);
                    self.stats.lock().unwrap().failed += 1;
                }
                Ok((status, _)) => {
                    println!("[crawler] 非 200 跳过: {} -> {}", url, status);
                    self.stats.lock().unwrap().failed += 1;
                }
                Err(e) => {
                    println!("[crawler] 抓取失败 {}: {}", url, e);
                    self.stats.lock().unwrap().failed += 1;
                }
            }
        }
        {
            let mut st = self.stats.lock().unwrap();
            st.elapsed_secs = start.elapsed().as_secs_f64();
        }
        self.stats.lock().unwrap().clone()
    }

    /// robots.txt 检查:返回该 URL 是否允许抓取
    fn robots_allowed(&self, domain: &str, url: &str) -> bool {
        // 已缓存?
        {
            let robots = self.robots.lock().unwrap();
            if let Some(disallows) = robots.get(domain) {
                return disallow_match(disallows, url);
            }
        }
        // 未缓存:获取 robots.txt
        let robots_url = format!("http://{}/robots.txt", domain);
        let disallows = match http_get(&robots_url, self.timeout_ms, CRAWLER_UA) {
            Ok((200, body)) => parse_robots(&body),
            _ => Vec::new(), // 无 robots.txt = 允许
        };
        self.robots.lock().unwrap().insert(domain.to_string(), disallows.clone());
        disallow_match(&disallows, url)
    }
}

/// 解析 robots.txt:提取 User-agent 通用段(* 或匹配 UA)的 Disallow 前缀
fn parse_robots(body: &str) -> Vec<String> {
    let mut disallows: Vec<String> = Vec::new();
    let mut in_group = false;
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let lower = t.to_lowercase();
        if lower.starts_with("user-agent:") {
            let ua = lower["user-agent:".len()..].trim();
            in_group = ua == "*" || ua.contains("pilseocrawler") || ua.contains("crawler");
        } else if lower.starts_with("disallow:") && in_group {
            let d = t["disallow:".len()..].trim().to_string();
            if !d.is_empty() {
                disallows.push(d);
            }
        } else if lower.starts_with("allow:") && in_group {
            // 简化:不处理 Allow 覆盖
        }
    }
    disallows
}

fn disallow_match(disallows: &[String], url: &str) -> bool {
    let path = url_path(url);
    !disallows.iter().any(|d| path.starts_with(d.as_str()))
}

/// 提取 URL 路径(不含 host)
fn url_path(url: &str) -> String {
    match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            match rest.find('/') {
                Some(j) => rest[j..].to_string(),
                None => "/".to_string(),
            }
        }
        None => url.to_string(),
    }
}

/// 域名(含端口,如 "example.com" / "127.0.0.1:8912"),用于 robots 与指纹
pub fn domain_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let hostport = rest.split('/').next()?;
    if hostport.is_empty() {
        return None;
    }
    Some(hostport.to_lowercase())
}

/// URL 规范化:http 降级、去 fragment、去尾部斜杠、host 小写
fn normalize_url(url: &str) -> Option<String> {
    let mut u = url.trim().to_string();
    let lower_u = u.to_lowercase();
    if lower_u.starts_with("https://") {
        u = format!("http://{}", &u[lower_u.find("://")? + 3..]);
    }
    if !u.starts_with("http://") {
        return None;
    }
    // 去 fragment
    if let Some(i) = u.find('#') {
        u.truncate(i);
    }
    // host 小写
    let rest = &u["http://".len()..];
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host_lower = hostport.to_lowercase();
    if host_lower.is_empty() {
        return None;
    }
    let path_clean = if path == "/" { "/" } else { path };
    Some(format!("http://{}{}", host_lower, path_clean))
}

/// 相对 URL 解析
fn resolve_url(base: &str, href: &str) -> Option<String> {
    let h = href.trim();
    if h.is_empty()
        || h.starts_with('#')
        || h.starts_with("javascript:")
        || h.starts_with("mailto:")
        || h.starts_with("tel:")
        || h.starts_with("data:")
        || h.starts_with("about:")
    {
        return None;
    }
    if h.starts_with("http://") || h.starts_with("https://") {
        return normalize_url(h);
    }
    if h.starts_with("//") {
        return normalize_url(&format!("http:{}", h));
    }
    // 相对路径:基于 base
    let origin = match base.find("://") {
        Some(i) => {
            let rest = &base[i + 3..];
            match rest.find('/') {
                Some(j) => &base[..i + 3 + j],
                None => base,
            }
        }
        None => return None,
    };
    if h.starts_with('/') {
        return Some(format!("{}{}", origin, h));
    }
    // 相对:base 目录 + href
    let base_path = url_path(base);
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..i + 1],
        None => "/",
    };
    Some(format!("{}{}{}", origin, dir, h))
}

/// 提取页面链接:<a href> + JS 链接(location/window.open/字符串 URL)
pub fn extract_links(html: &str, base: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let lower = html.to_lowercase();
    // 1. <a href="..."> / <a href='...'>
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<a ") {
        let tag_start = pos + rel;
        let tag_end = lower[tag_start..]
            .find('>')
            .map(|e| tag_start + e)
            .unwrap_or(html.len());
        let tag = &html[tag_start..tag_end];
        for (attr, q) in [("href=\"", '"'), ("href='", '\'')] {
            if let Some(hi) = tag.to_lowercase().find(attr) {
                let hs = hi + attr.len();
                if let Some(he) = tag[hs..].find(q) {
                    let href = &tag[hs..hs + he];
                    if let Some(u) = resolve_url(base, href) {
                        out.push(u);
                    }
                }
            }
        }
        pos = tag_end + 1;
        if pos >= html.len() {
            break;
        }
    }
    // 2. JS 链接:window.open("...") / location.href="..." / 裸 http(s):// 字符串
    let mut scan = 0;
    while let Some(i) = lower[scan..].find("http") {
        let start = scan + i;
        // 检查是否 http:// 或 https://
        let scheme_end = if lower[start..].starts_with("https://") {
            start + 8
        } else if lower[start..].starts_with("http://") {
            start + 7
        } else {
            scan = start + 4;
            continue;
        };
        let rest = &html[scheme_end..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == '>' || c == '<' || c == ']')
            .unwrap_or(rest.len());
        let url = &html[start..scheme_end + end];
        if let Some(u) = normalize_url(url) {
            out.push(u);
        }
        scan = scheme_end + end;
        if scan >= html.len() {
            break;
        }
    }
    out
}

/// 保存抓取页面到 out/crawled/<domain>/<path>.html(文件名/目录名安全化)
fn save_page(url: &str, html: &str, out_dir: &Path) -> Option<std::path::PathBuf> {
    let domain = domain_of(url)?;
    // 目录名安全化(Windows 不允许 ':' 等)
    let safe_domain: String = domain
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    let path = url_path(url);
    let is_root = path == "/" || path.is_empty();
    let safe: String = path
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let file_name = if is_root {
        "index.html".to_string()
    } else if safe.ends_with(".html") {
        safe
    } else {
        format!("{}.html", safe)
    };
    let dir = out_dir.join(&safe_domain);
    fs::create_dir_all(&dir).ok()?;
    let file = dir.join(file_name);
    fs::write(&file, html).ok()?;
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_resolve() {
        assert_eq!(normalize_url("HTTPS://Example.com/#top").unwrap(), "http://example.com/");
        assert_eq!(normalize_url("http://a.com/path").unwrap(), "http://a.com/path");
        assert!(normalize_url("ftp://x.com").is_none());
        assert_eq!(resolve_url("http://a.com/", "/b.html").unwrap(), "http://a.com/b.html");
        assert_eq!(resolve_url("http://a.com/x/", "b.html").unwrap(), "http://a.com/x/b.html");
        assert_eq!(resolve_url("http://a.com/", "//c.com/d").unwrap(), "http://c.com/d");
        assert!(resolve_url("http://a.com/", "javascript:void(0)").is_none());
        assert!(resolve_url("http://a.com/", "#top").is_none());
    }

    #[test]
    fn extract_links_finds_a_and_js() {
        let html = r#"<html>
          <a href="/page1.html">站内</a>
          <a href='http://friend.com/'>友情链接</a>
          <a href="https://secure.com/x">外链</a>
          <script>window.open('http://js-site.com/'); location.href = "http://loc-site.com/p";</script>
          <div>文本里的 http://text-url.com/ 链接</div>
        </html>"#;
        let links = extract_links(html, "http://me.com/");
        let joined = links.join(" ");
        assert!(joined.contains("http://me.com/page1.html"), "{}", joined);
        assert!(joined.contains("http://friend.com/"), "{}", joined);
        assert!(joined.contains("http://secure.com/x"), "{}", joined);
        assert!(joined.contains("http://js-site.com/"), "{}", joined);
        assert!(joined.contains("http://loc-site.com/p"), "{}", joined);
        assert!(joined.contains("http://text-url.com/"), "{}", joined);
    }

    #[test]
    fn robots_parse_and_match() {
        let dis = parse_robots("User-agent: *\nDisallow: /admin\nDisallow: /private/\nUser-agent: other\nDisallow: /x\n");
        assert_eq!(dis.len(), 2);
        assert!(disallow_match(&dis, "http://a.com/"), "根路径应允许");
        assert!(!disallow_match(&dis, "http://a.com/admin/panel"), "/admin 应禁止");
        assert!(!disallow_match(&dis, "http://a.com/private/x"), "/private/ 应禁止");
        assert!(disallow_match(&dis, "http://a.com/public"), "public 应允许");
    }
}
