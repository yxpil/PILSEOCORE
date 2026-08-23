//! 索引器:扫描站点目录,提取标题/描述/关键词,生成 sitemap.xml,
//! 构建分块倒排索引并持久化(每块一个文件,查询时懒加载相关块)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::json::Json;

#[derive(Clone, Debug)]
pub struct DocMeta {
    pub domain: String,
    pub title: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub url: String,
}

/// 分块倒排索引(按词首字符分块: 0-9 a-z _)
pub struct SiteIndex {
    pub docs: Vec<DocMeta>,
    pub blocks: Mutex<HashMap<u8, HashMap<String, Vec<usize>>>>,
    data_dir: PathBuf,
}

impl SiteIndex {
    /// 全量重建索引:扫描 sites_dir 下所有站点,提取内容并生成 sitemap.xml
    pub fn build(sites_dir: &Path, data_dir: &Path) -> Result<SiteIndex, String> {
        let mut docs = Vec::new();
        let mut blocks: HashMap<u8, HashMap<String, Vec<usize>>> = HashMap::new();

        if sites_dir.exists() {
            let entries = fs::read_dir(sites_dir)
                .map_err(|e| format!("读取站点目录失败 {}: {}", sites_dir.display(), e))?;
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let domain = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let html_path = dir.join("index.html");
                if !html_path.exists() {
                    continue;
                }
                let html = match fs::read_to_string(&html_path) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                let title = extract_title(&html).unwrap_or_else(|| domain.clone());
                let description = extract_meta(&html, "description").unwrap_or_default();
                let keywords = extract_keywords(&html);
                let url = format!("https://{}/", domain);
                let doc_id = docs.len();
                docs.push(DocMeta {
                    domain: domain.clone(),
                    title,
                    description,
                    keywords: keywords.clone(),
                    url,
                });
                // 分词入倒排
                let mut text = String::new();
                text.push_str(&docs[doc_id].title);
                text.push(' ');
                text.push_str(&docs[doc_id].description);
                text.push(' ');
                text.push_str(&domain);
                for kw in &keywords {
                    text.push(' ');
                    text.push_str(kw);
                }
                let mut seen: HashSet<String> = HashSet::new();
                for term in tokenize(&text) {
                    if !seen.insert(term.clone()) {
                        continue;
                    }
                    let chunk = chunk_of(&term);
                    blocks.entry(chunk).or_default().entry(term).or_default().push(doc_id);
                }
                // 生成 sitemap.xml
                let _ = gen_sitemap(&dir, &domain, &html);
            }
        }

        fs::create_dir_all(data_dir).map_err(|e| format!("创建索引目录失败: {}", e))?;
        fs::create_dir_all(data_dir.join("blocks")).map_err(|e| format!("创建分块目录失败: {}", e))?;
        let index = SiteIndex {
            docs,
            blocks: Mutex::new(blocks),
            data_dir: data_dir.to_path_buf(),
        };
        index.save()?;
        Ok(index)
    }

    /// 从磁盘加载(懒加载:先读文档元数据,分块文件按需读入)
    pub fn load(data_dir: &Path) -> Result<SiteIndex, String> {
        let meta_path = data_dir.join("meta.json");
        let docs_path = data_dir.join("docs.json");
        let docs = if docs_path.exists() {
            let text = fs::read_to_string(&docs_path).map_err(|e| format!("读取 {} 失败: {}", docs_path.display(), e))?;
            let j = crate::json::parse(&text).map_err(|e| format!("解析 {} 失败: {}", docs_path.display(), e))?;
            let mut docs = Vec::new();
            if let Some(arr) = j.as_arr() {
                for item in arr {
                    let domain = item.get("domain").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let description = item.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let mut keywords = Vec::new();
                    if let Some(ks) = item.get("keywords").and_then(|v| v.as_arr()) {
                        for k in ks {
                            if let Some(s) = k.as_str() {
                                keywords.push(s.to_string());
                            }
                        }
                    }
                    docs.push(DocMeta { domain, title, description, keywords, url });
                }
            }
            docs
        } else {
            Vec::new()
        };
        let _ = meta_path; // meta 仅供展示,加载时忽略
        Ok(SiteIndex {
            docs,
            blocks: Mutex::new(HashMap::new()),
            data_dir: data_dir.to_path_buf(),
        })
    }

    /// 持久化索引到磁盘
    pub fn save(&self) -> Result<(), String> {
        // docs.json
        let arr: Vec<Json> = self
            .docs
            .iter()
            .map(|d| {
                Json::build(vec![
                    ("domain", Json::str(&d.domain)),
                    ("title", Json::str(&d.title)),
                    ("description", Json::str(&d.description)),
                    ("keywords", Json::arr(d.keywords.iter().map(|k| Json::str(k)).collect())),
                    ("url", Json::str(&d.url)),
                ])
            })
            .collect();
        fs::write(self.data_dir.join("docs.json"), Json::arr(arr).to_string())
            .map_err(|e| format!("写入 docs.json 失败: {}", e))?;
        // 每块一个文件
        let blocks = self.blocks.lock().unwrap();
        let dir = self.data_dir.join("blocks");
        for (chunk, terms) in blocks.iter() {
            let mut m = BTreeMap::new();
            for (term, ids) in terms.iter() {
                m.insert(term.clone(), Json::arr(ids.iter().map(|&i| Json::num(i as f64)).collect()));
            }
            fs::write(dir.join(format!("block_{:02}.json", chunk_name(*chunk))), Json::Obj(m).to_string())
                .map_err(|e| format!("写入分块失败: {}", e))?;
        }
        // meta.json
        let meta = Json::build(vec![
            ("version", Json::num(1.0)),
            ("sites", Json::num(self.docs.len() as f64)),
            ("blocks", Json::num(blocks.len() as f64)),
            ("built_at", Json::str(now_rfc3339())),
        ]);
        fs::write(self.data_dir.join("meta.json"), meta.to_string())
            .map_err(|e| format!("写入 meta.json 失败: {}", e))?;
        Ok(())
    }

    /// 确保某块已加载到内存
    pub fn ensure_block(&self, chunk: u8) {
        let mut blocks = self.blocks.lock().unwrap();
        if blocks.contains_key(&chunk) {
            return;
        }
        let path = self.data_dir.join("blocks").join(format!("block_{:02}.json", chunk_name(chunk)));
        let mut map = HashMap::new();
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(j) = crate::json::parse(&text) {
                if let Json::Obj(m) = j {
                    for (term, v) in m {
                        if let Json::Arr(ids) = v {
                            let ids: Vec<usize> = ids.iter().filter_map(|x| x.as_u64()).map(|x| x as usize).collect();
                            if !ids.is_empty() {
                                map.insert(term, ids);
                            }
                        }
                    }
                }
            }
        }
        blocks.insert(chunk, map);
    }

    /// 已加载块数
    pub fn loaded_blocks(&self) -> usize {
        self.blocks.lock().unwrap().len()
    }

    pub fn stats(&self) -> (usize, usize, usize) {
        let blocks = self.blocks.lock().unwrap();
        let terms = blocks.values().map(|m| m.len()).sum::<usize>();
        (self.docs.len(), terms, blocks.len())
    }
}

fn chunk_name(c: u8) -> String {
    if c == b'_' {
        "other".to_string()
    } else {
        (c as char).to_string()
    }
}

pub fn chunk_of(word: &str) -> u8 {
    match word.chars().next() {
        Some(c) if c.is_ascii_digit() || c.is_ascii_lowercase() => c as u8,
        _ => b'_',
    }
}

/// 判断是否 CJK 汉字
fn is_cjk(c: char) -> bool {
    let u = c as u32;
    (0x4E00..=0x9FFF).contains(&u) || (0x3400..=0x4DBF).contains(&u)
}

/// 分词:ASCII 词(字母数字,>=2 字符)+ 中文单字与相邻双字
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = text.to_lowercase();
    // ASCII 词
    let mut cur = String::new();
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            cur.push(c);
        } else {
            if cur.len() >= 2 {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }
    if cur.len() >= 2 {
        out.push(cur);
    }
    // 中文单字 + bigram
    let cjk: Vec<char> = text.chars().filter(|&c| is_cjk(c)).collect();
    for &ch in &cjk {
        out.push(ch.to_string());
    }
    for w in cjk.windows(2) {
        let mut s = String::with_capacity(6);
        s.push(w[0]);
        s.push(w[1]);
        out.push(s);
    }
    out
}

/// 简单编辑距离(Levenshtein)
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 3 {
        return 4;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            let v = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
            cur.push(v);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// 从 HTML 提取 <title>
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title>")? + 7;
    let end = lower[start..].find("</title>")? + start;
    Some(html[start..end].trim().to_string())
}

/// 提取 meta 标签(content 属性),name 大小写不敏感
fn extract_meta(html: &str, name: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<meta") {
        let tag_start = search_from + rel;
        let tag_end = lower[tag_start..].find('>').map(|e| tag_start + e).unwrap_or(html.len());
        let tag = &lower[tag_start..tag_end];
        if let Some(ni) = tag.find("name=\"") {
            let ns = ni + 6;
            let ne = tag[ns..].find('"').map(|e| ns + e).unwrap_or(tag.len());
            if &tag[ns..ne] == name {
                // 找 content
                if let Some(ci) = tag.find("content=\"") {
                    let cs = ci + 9;
                    let ce = tag[cs..].find('"').map(|e| cs + e).unwrap_or(tag.len());
                    return Some(html[tag_start + cs..tag_start + ce].trim().to_string());
                }
            }
        }
        search_from = tag_end + 1;
        if search_from >= html.len() {
            break;
        }
    }
    None
}

/// 提取 keywords(逗号/空格/中文逗号分隔)
fn extract_keywords(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(kws) = extract_meta(html, "keywords") {
        for part in kws.split(|c: char| c == ',' || c == '，' || c.is_whitespace()) {
            let p = part.trim();
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// 为站点生成 sitemap.xml
fn gen_sitemap(site_dir: &Path, domain: &str, html: &str) -> Result<(), String> {
    // 提取站内链接(本地 SEO 站通常只有主页,也扫描 <a href>)
    let mut urls: Vec<String> = vec![format!("https://{}/", domain)];
    let lower = html.to_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("<a ") {
        let tag_start = search_from + rel;
        let tag_end = lower[tag_start..].find('>').map(|e| tag_start + e).unwrap_or(html.len());
        let tag = &lower[tag_start..tag_end];
        if let Some(hi) = tag.find("href=\"") {
            let hs = hi + 6;
            let he = tag[hs..].find('"').map(|e| hs + e).unwrap_or(tag.len());
            let href = &tag[hs..he];
            if href.starts_with('/') {
                urls.push(format!("https://{}{}", domain, href));
            } else if href.starts_with("https://") || href.starts_with("http://") {
                urls.push(href.to_string());
            }
        }
        search_from = tag_end + 1;
        if search_from >= html.len() {
            break;
        }
    }
    // 去重
    urls.sort();
    urls.dedup();
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for u in &urls {
        xml.push_str(&format!("  <url><loc>{}</loc></url>\n", u));
    }
    xml.push_str("</urlset>\n");
    fs::write(site_dir.join("sitemap.xml"), xml)
        .map_err(|e| format!("写入 sitemap.xml 失败 {}: {}", site_dir.display(), e))
}

fn now_rfc3339() -> String {
    // 无第三方库的简单时间格式
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}", secs)
}
