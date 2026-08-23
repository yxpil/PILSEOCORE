//! SEO 网站生成:对探测为"可用"的域名批量生成静态站点

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct SiteBuilder {
    keywords: Vec<String>,
    idx: AtomicUsize,
    out_dir: PathBuf,
}

impl SiteBuilder {
    pub fn new(keywords: Vec<String>, out_dir: PathBuf) -> Self {
        let keywords = if keywords.is_empty() {
            vec!["SEO".to_string()]
        } else {
            keywords
        };
        SiteBuilder {
            keywords,
            idx: AtomicUsize::new(0),
            out_dir,
        }
    }

    /// 为 fqdn 生成站点目录 out/sites/<fqdn>/index.html,返回关键词
    pub fn build(&self, fqdn: &str) -> Result<String, String> {
        let kw = self.next_keyword();
        let dir = self.out_dir.join("sites").join(fqdn);
        fs::create_dir_all(&dir).map_err(|e| format!("创建站点目录失败 {}: {}", dir.display(), e))?;
        let html = self.render_html(&kw, fqdn);
        fs::write(dir.join("index.html"), html)
            .map_err(|e| format!("写入 index.html 失败 {}: {}", dir.display(), e))?;
        Ok(kw)
    }

    fn next_keyword(&self) -> String {
        let i = self.idx.fetch_add(1, Ordering::Relaxed);
        self.keywords[i % self.keywords.len()].clone()
    }

    fn render_html(&self, kw: &str, fqdn: &str) -> String {
        format!(
            r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{kw} - {fqdn}</title>
<meta name="description" content="{kw} 相关资讯、资源与最新动态,尽在 {fqdn}">
<meta name="keywords" content="{kw}, {fqdn}">
<link rel="canonical" href="https://{fqdn}/">
</head>
<body>
<header>
<h1>{kw}</h1>
<p>{fqdn} —— {kw} 专题站点</p>
</header>
<main>
<article>
<h2>关于 {kw}</h2>
<p>{kw} 是一个值得深入探索的领域。本站专注于 {kw} 相关的知识、产品与行业动态,持续更新优质内容。</p>
</article>
<article>
<h2>{kw} 最新动态</h2>
<p>更多 {kw} 相关内容正在建设中,敬请期待。</p>
</article>
</main>
<footer>
<p>&copy; 2026 {fqdn} · {kw}</p>
</footer>
</body>
</html>
"#,
            kw = kw,
            fqdn = fqdn
        )
    }
}
