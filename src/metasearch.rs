//! 聚合搜索(元搜索):本地索引搜不到时,借助外部搜索引擎
//! (必应/百度/360搜索/搜狗/谷歌/中国搜索),结果缓存到本地引擎。
//! 纯 Rust 标准库,零第三方依赖;https 抓取走系统 curl。

use std::path::PathBuf;
use std::sync::Mutex;

/// 单条聚合结果
#[derive(Clone, Debug)]
pub struct MetaResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
}

/// 引擎配置:url 模板用 {q} 占位查询词
pub struct MetaProvider {
    pub name: &'static str,
    pub url: &'static str,
    /// 引擎自身功能页特征(排除搜索页/登录/帮助等噪声链接)
    pub noise: &'static [&'static str],
}

/// 内置聚合引擎(顺序即优先级):必应 / 百度 / 360搜索 / 搜狗 / 谷歌 / 中国搜索
pub const PROVIDERS: &[MetaProvider] = &[
    MetaProvider {
        name: "必应",
        url: "https://www.bing.com/search?q={q}&mkt=zh-CN&count=10",
        noise: &["bing.com/search", "bing.com/images", "bing.com/videos", "go.microsoft.com"],
    },
    MetaProvider {
        name: "百度",
        url: "https://www.baidu.com/s?wd={q}&rn=10",
        noise: &["baidu.com/s?", "baidu.com/sug", "baidu.com/cse", "top.baidu.com"],
    },
    MetaProvider {
        name: "360搜索",
        url: "https://www.so.com/s?q={q}",
        noise: &["so.com/s?", "so.com/so", "360.cn", "zhushou.360.cn"],
    },
    MetaProvider {
        name: "搜狗",
        url: "https://www.sogou.com/web?query={q}",
        noise: &["sogou.com/web?", "sogou.com/sogou", "sogou.com/s?query"],
    },
    MetaProvider {
        name: "谷歌",
        url: "https://www.google.com/search?q={q}&hl=zh-CN&num=10",
        noise: &["google.com/search", "google.com/maps", "google.com/images", "support.google.com"],
    },
    MetaProvider {
        name: "中国搜索",
        url: "https://www.chinaso.com/newssearch/all?q={q}",
        noise: &["chinaso.com/newssearch", "chinaso.com/so", "chinaso.com/search"],
    },
];

/// URL 编码(查询词):保留字母数字,其余 percent 编码(UTF-8 字节)
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 去 HTML 标签(标题/摘要内联标签)
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// HTML 实体解码(标题/摘要常见实体)
fn decode_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

/// 标题质量过滤:属性残留/JSON 残留/标签实体残留 → 丢弃
fn title_bad(title: &str) -> bool {
    title.contains('<')
        || title.contains('>')
        || title.contains('&')
        || title.contains('=')
        || title.contains('"')
        || title.contains('{')
        || title.starts_with("ss=")
        || title.starts_with("href=")
        || title.starts_with("class=")
        || title.starts_with("style=")
}

/// 通用结果解析:提取 <h2>/<h3> 内的 <a href> 作为搜索结果(主流引擎均用 h2/h3 包裹标题),
/// 排除引擎自身功能链接;标题块后取摘要文本(去标签截断)
fn parse(html: &str, provider: &MetaProvider) -> Vec<MetaResult> {
    let mut out: Vec<MetaResult> = Vec::new();
    let mut pos = 0usize;
    while out.len() < 10 && pos < html.len() {
        // 定位下一个 <h2 或 <h3(块级标题)
        let rel2 = html[pos..].find("<h2");
        let rel3 = html[pos..].find("<h3");
        let rel = match (rel2, rel3) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        let h_start = pos + rel;
        // 标题标签结束
        let Some(rel_tag_end) = html[h_start..].find('>') else { break };
        let tag_end = h_start + rel_tag_end;
        // 闭合 </h2> 或 </h3>
        let block_end = html[tag_end..]
            .find("</h")
            .map(|e| tag_end + e)
            .unwrap_or_else(|| html.len().min(tag_end + 4000));
        // tag_end+1 可能落在多字节字符中间(如 '>' 后紧跟中文):对齐字符边界
        let block_start = html.floor_char_boundary(tag_end + 1);
        let block = &html[block_start..block_end];
        // 块内 <a href="...">
        if let Some(hi) = block.find("href=\"") {
            let hs = hi + 6;
            let he = block[hs..].find('"').map(|e| hs + e).unwrap_or(block.len());
            let raw_url = block[hs..he].trim();
            if !raw_url.is_empty() && !raw_url.starts_with('#') && !raw_url.starts_with("javascript:") {
                // 排除引擎自身功能链接
                let noise_hit = provider.noise.iter().any(|n| raw_url.contains(n));
                if !noise_hit {
                    // 相对协议补全
                    let url = if raw_url.starts_with("//") {
                        format!("https:{}", raw_url)
                    } else {
                        raw_url.to_string()
                    };
                    // 标题:a 标签结束(第一个 >)后的文本直到 </a>(href 后可能还有其他属性)
                    // 注意:find('>') 返回相对 he 的偏移,map 已换算成 block 内绝对位置
                    let a_tag_end = block[he..].find('>').map(|e| he + e).unwrap_or(block.len());
                    let text_start = block.floor_char_boundary(a_tag_end + 1);
                    let a_end = block[text_start..].find("</a>").map(|e| text_start + e).unwrap_or(block.len());
                    let title = decode_entities(&strip_tags(&block[text_start..a_end]));
                    if !title_bad(&title) && !title.is_empty() {
                        // 摘要:标题块全部文本去标题(截 180 字)
                        let all_text = decode_entities(&strip_tags(block));
                        let snippet = all_text
                            .replace(&title, "")
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        let snippet: String = snippet.chars().take(180).collect();
                        out.push(MetaResult {
                            title,
                            url,
                            snippet,
                            engine: provider.name.to_string(),
                        });
                    }
                }
            }
        }
        pos = block_end + 4;
    }
    // 按 url 去重(跨引擎合并同一结果)
    let mut seen = std::collections::HashSet::new();
    out.retain(|r| seen.insert(r.url.clone()));
    out.truncate(5); // 每引擎最多 5 条
    out
}

/// 缓存目录:data/search_cache/(index_dir 下)
pub fn cache_dir() -> PathBuf {
    crate::config::index_dir().join("search_cache")
}

fn cache_file(q: &str) -> PathBuf {
    let h = crate::blacklist::fnv1a64(q);
    cache_dir().join(format!("{:016x}.tsv", h))
}

/// 读缓存:命中返回 Some(TSV 格式:每行 title\turl\tsnippet\tengine)
pub fn load_cache(q: &str) -> Option<Vec<MetaResult>> {
    let path = cache_file(q);
    let text = std::fs::read_to_string(&path).ok()?;
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() == 4 {
            out.push(MetaResult {
                title: f[0].to_string(),
                url: f[1].to_string(),
                snippet: f[2].to_string(),
                engine: f[3].to_string(),
            });
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 写缓存(TSV:字段内制表符/换行替换为空格)
pub fn save_cache(q: &str, results: &[MetaResult]) {
    if results.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(cache_dir());
    let mut text = String::new();
    for r in results {
        text.push_str(&clean_tsv(&r.title));
        text.push('\t');
        text.push_str(&clean_tsv(&r.url));
        text.push('\t');
        text.push_str(&clean_tsv(&r.snippet));
        text.push('\t');
        text.push_str(&clean_tsv(&r.engine));
        text.push('\n');
    }
    let _ = std::fs::write(cache_file(q), text);
}

fn clean_tsv(s: &str) -> String {
    s.replace('\t', " ").replace('\n', " ").replace('\r', " ")
}

/// 聚合搜索入口:缓存优先,未命中则并发抓取全部引擎,结果缓存
pub fn search_cached(q: &str) -> (Vec<MetaResult>, bool) {
    if let Some(cached) = load_cache(q) {
        return (cached, true);
    }
    let results = search_live(q);
    if !results.is_empty() {
        save_cache(q, &results);
    }
    (results, false)
}

/// 并发抓取全部引擎(每引擎独立线程,互不影响;整体耗时 ≈ 最慢引擎的超时)
/// 用浏览器 UA(聚合搜索 = 代替用户搜索,非爬站点;带爬虫后缀会被引擎拦截)
pub fn search_live(q: &str) -> Vec<MetaResult> {
    const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
    let q_owned = q.to_string();
    let mut handles = Vec::new();
    for p in PROVIDERS {
        let qq = q_owned.clone();
        let url = p.url.replace("{q}", &urlencode(&qq));
        handles.push(std::thread::spawn(move || {
            match crate::http::http_get(&url, 6000, BROWSER_UA) {
                Ok((200, html)) => parse(&html, &p),
                _ => Vec::new(),
            }
        }));
    }
    let mut all: Vec<MetaResult> = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap_or_default());
    }
    // 全局 url 去重 + 截断 20 条
    let mut seen = std::collections::HashSet::new();
    all.retain(|r| seen.insert(r.url.clone()));
    all.truncate(20);
    all
}

/// 聚合缓存统计(空实现占位,供面板后续使用)
pub static CACHE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_entities_works() {
        assert_eq!(decode_entities("a&amp;b&quot;c"), "a&b\"c");
        assert_eq!(decode_entities("&lt;em&gt;x&lt;/em&gt;"), "<em>x</em>");
    }

    #[test]
    fn title_bad_filters_junk() {
        assert!(title_bad("ss=\"mh-refresh-btn\""));
        assert!(title_bad("quot;F3&quot;:&quot;54E&quot;"));
        assert!(title_bad("href=\"/x\""));
        assert!(!title_bad("量子纠缠 百度百科"));
        assert!(!title_bad("是可控核聚变成功的关键"));
    }

    #[test]
    fn parse_extracts_clean_results() {
        // 模拟必应结果页:h3 内 a(href 后带属性)+ 正常标题;噪声块应被过滤
        let html = r#"<ol><li class="b_algo"><h2><a href="https://example.com/a" h="ID=SERP,1">量子纠缠 百科</a></h2><p>描述文本</p></li>
        <li><h3><a href="javascript:void(0)" class="btn">刷新</a></h3></li>
        <li><h3><a href="https://bad.com" class="x">ss="残留"垃圾标题</a></h3></li></ol>"#;
        let p = MetaProvider { name: "测试", url: "", noise: &["bing.com/search"] };
        let res = parse(html, &p);
        assert!(res.iter().any(|r| r.title == "量子纠缠 百科" && r.url == "https://example.com/a"), "got: {:?}", res.iter().map(|r| (&r.title, &r.url)).collect::<Vec<_>>());
        assert!(!res.iter().any(|r| r.title.contains("刷新") || r.title.contains("垃圾")), "got: {:?}", res.iter().map(|r| &r.title).collect::<Vec<_>>());
    }

    #[test]
    fn parse_no_panic_unicode() {
        // 变长 Unicode(İ)与中文混排:切片安全,不 panic
        let html = "<h2><a href=\"https://x.com/1\">İSTANBUL 中文标题</a></h2><h3><a href=\"https://y.com/2\">第二个</a></h3>";
        let p = MetaProvider { name: "测试", url: "", noise: &[] };
        let res = parse(html, &p);
        assert!(res.len() >= 1);
    }
}
