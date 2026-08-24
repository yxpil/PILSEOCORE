//! 索引器:扫描站点目录,提取标题/描述/关键词,生成 sitemap.xml,
//! 构建分块倒排索引并持久化(每块一个文件,查询时懒加载相关块)

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::blacklist::Blacklist;
use crate::json::Json;
use crate::tokenizer::BpeTokenizer;

#[derive(Clone, Debug)]
pub struct DocMeta {
    pub domain: String,
    pub title: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub url: String,
    /// 内容重复计数:相同/雷同内容的站点数(仅保留一条,此字段记录总数)
    pub dup_count: usize,
}

/// 分块倒排索引(按词首字符分块: 0-9 a-z _)+ BPE 分词器
pub struct SiteIndex {
    pub docs: Vec<DocMeta>,
    pub blocks: Mutex<HashMap<u8, HashMap<String, Vec<usize>>>>,
    data_dir: PathBuf,
    /// BPE 分词器(索引与查询共享同一词表)
    pub tokenizer: BpeTokenizer,
}

/// 文档元数据分片大小(每片 N 个文档,避免单文件过大)
const DOCS_PER_CHUNK: usize = 256;

/// 索引构建进度(管理面板实时显示)
#[derive(Clone, Debug, Default)]
pub struct IndexState {
    pub phase: String,          // 当前阶段:分词训练 / 扫描站点 / 计数去重 / 写盘
    pub processed: usize,       // 已处理站点
    pub total: usize,           // 总站点数
    pub current_domain: String, // 当前处理站点
    pub keywords: Vec<String>,  // 当前站点提取的关键词
    pub links_found: usize,     // 当前站点发现的外链数
    pub sites: usize,           // 索引站点数(页面数)
    pub dup: usize,             // 计数去重合并数
    pub blocked: usize,         // 黑名单跳过数
    pub running: bool,
    pub finished: bool,
    pub error: Option<String>,
    pub started_ts: u64,
    pub elapsed_secs: f64,
}

impl SiteIndex {
    /// 从内存文档直接构造(测试/嵌入式使用,不落盘;同步构建倒排索引)
    pub fn from_docs(docs: Vec<DocMeta>) -> SiteIndex {
        SiteIndex::from_docs_with_tokenizer(docs, BpeTokenizer::train(&[], 300))
    }

    /// 从内存文档 + 指定分词器构造(测试用,语料训练的 BPE 更接近真实)
    pub fn from_docs_with_tokenizer(docs: Vec<DocMeta>, tokenizer: BpeTokenizer) -> SiteIndex {
        let mut blocks: HashMap<u8, HashMap<String, Vec<usize>>> = HashMap::new();
        for (doc_id, doc) in docs.iter().enumerate() {
            let mut text = String::new();
            text.push_str(&doc.title);
            text.push(' ');
            text.push_str(&doc.description);
            text.push(' ');
            text.push_str(&doc.domain);
            for kw in &doc.keywords {
                text.push(' ');
                text.push_str(kw);
            }
            let mut seen = HashSet::new();
            for term in tokenizer.tokenize_str(&text) {
                if !seen.insert(term.clone()) {
                    continue;
                }
                let chunk = chunk_of(&term);
                blocks.entry(chunk).or_default().entry(term).or_default().push(doc_id);
            }
        }
        SiteIndex {
            docs,
            blocks: Mutex::new(blocks),
            data_dir: PathBuf::from(""),
            tokenizer,
        }
    }

    /// 全量重建索引:
    /// 1. 训练/加载 BPE 分词器(81920 词表)
    /// 2. 扫描站点,通过 sitemap + <a> 标签发现站内全部页面(不只 index.html)
    /// 3. 内容指纹去重:**重复/雷同内容计数去重**(只保留一条,dup_count 记录总数,
    ///    不拉黑、不删除结果;手动黑名单域名跳过)
    /// 4. 提取标题/meta,构建分块倒排索引,生成 sitemap.xml
    pub fn build(sites_dir: &Path, data_dir: &Path, blacklist: &Blacklist) -> Result<SiteIndex, String> {
        SiteIndex::build_with_progress(sites_dir, data_dir, blacklist, None)
    }

    /// 带进度回调的构建(progress 每处理一个站点更新一次,供面板实时显示)
    pub fn build_with_progress(
        sites_dir: &Path,
        data_dir: &Path,
        blacklist: &Blacklist,
        progress: Option<&Mutex<IndexState>>,
    ) -> Result<SiteIndex, String> {
        let started = std::time::Instant::now();
        if let Some(st) = progress {
            let mut s = st.lock().unwrap();
            s.running = true;
            s.finished = false;
            s.error = None;
            s.phase = "分词训练".into();
            s.processed = 0;
            s.started_ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            drop(s);
        }
        // ---- 1. 训练/加载分词器 ----
        let vocab_path = data_dir.join("tokenizer").join("vocab.json");
        let tokenizer = if vocab_path.exists() {
            match BpeTokenizer::load(&vocab_path) {
                Some(t) => t,
                None => {
                    let corpus = collect_corpus(sites_dir);
                    let t = BpeTokenizer::train(&corpus, crate::tokenizer::VOCAB_SIZE);
                    let _ = t.save(&vocab_path);
                    t
                }
            }
        } else {
            let corpus = collect_corpus(sites_dir);
            let t = BpeTokenizer::train(&corpus, crate::tokenizer::VOCAB_SIZE);
            let _ = t.save(&vocab_path);
            t
        };
        println!("[index] 分词器: {} tokens", tokenizer.vocab_size());

        // ---- 2/3/4. 扫描站点建索引(内容指纹计数去重) ----
        let mut docs: Vec<DocMeta> = Vec::new();
        let mut blocks: HashMap<u8, HashMap<String, Vec<usize>>> = HashMap::new();
        let mut blocked = 0usize; // 手动黑名单跳过
        let mut deduped = 0usize; // 计数去重数量
        // 指纹库:fnv 精确重复索引 + simhash 雷同索引 (fnv -> (simhash, len, doc_id))
        let mut fingerprints: HashMap<u64, (u64, usize, usize)> = HashMap::new();
        // 外链发现:本站之外的新网站(爬虫种子)
        let mut discovered: HashSet<String> = HashSet::new();

        // 扫描目录:主站点目录 + 爬虫抓取目录(同级的 crawled)
        let mut scan_dirs: Vec<std::path::PathBuf> = vec![sites_dir.to_path_buf()];
        if let Some(parent) = sites_dir.parent() {
            let crawled = parent.join("crawled");
            if crawled.exists() {
                scan_dirs.push(crawled);
            }
        }
        // 站点总数(进度条分母)
        let mut total_sites = 0usize;
        for sd in &scan_dirs {
            if let Ok(rd) = fs::read_dir(sd) {
                total_sites += rd.flatten().filter(|e| e.path().is_dir()).count();
            }
        }
        if let Some(st) = progress {
            let mut s = st.lock().unwrap();
            s.total = total_sites;
            s.phase = "扫描站点".into();
            drop(s);
        }
        crate::logger::push(format!("[index] 开始扫描 {} 个站点目录(共 {} 站点)", scan_dirs.len(), total_sites));
        for sd in &scan_dirs {
        if sd.exists() {
            let entries = fs::read_dir(sd)
                .map_err(|e| format!("读取站点目录失败 {}: {}", sd.display(), e))?;
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let domain = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
                // 进度:当前站点 + 已处理计数
                if let Some(st) = progress {
                    let mut s = st.lock().unwrap();
                    s.processed += 1;
                    s.current_domain = domain.clone();
                    s.keywords.clear();
                    s.links_found = 0;
                    drop(s);
                }
                // 手动黑名单域名:跳过(不索引不计数)
                if blacklist.is_blocked(&domain) {
                    blocked += 1;
                    if let Some(st) = progress {
                        st.lock().unwrap().blocked = blocked;
                    }
                    continue;
                }
                // ---- crawled 目录(爬虫抓取):页面级独立收录 ----
                // 不依赖 index.html(首页没抓到也收录子页);每页独立指纹去重
                // (首页是模板/重复不牵连子页,发现链接真正编入索引)
                let is_crawled = sd.ends_with("crawled");
                if is_crawled {
                    let pages = discover_pages(&dir);
                    if pages.is_empty() {
                        continue;
                    }
                    for (rel, page_html) in &pages {
                        let fp_text = crate::blacklist::fingerprint_text(&domain, &extract_page_text(page_html));
                        let fh = crate::blacklist::fnv1a64(&fp_text);
                        let fp = crate::blacklist::simhash(&fp_text);
                        let fp_len = fp_text.len();
                        let mut dup_target: Option<usize> = None;
                        if let Some(&(_, _, doc_id)) = fingerprints.get(&fh) {
                            dup_target = Some(doc_id);
                        } else if fp_len >= 128 {
                            for (_, (h, len, doc_id)) in fingerprints.iter() {
                                let max_len = fp_len.max(*len).max(1);
                                if fp_len.abs_diff(*len) * 10 > max_len * 3 {
                                    continue;
                                }
                                if crate::blacklist::hamming(fp, *h) <= 6 {
                                    dup_target = Some(*doc_id);
                                    break;
                                }
                            }
                        }
                        if let Some(doc_id) = dup_target {
                            docs[doc_id].dup_count += 1;
                            deduped += 1;
                            if let Some(st) = progress {
                                st.lock().unwrap().dup = deduped;
                            }
                            continue;
                        }
                        // 建文档(每页一条)
                        let title = extract_title(page_html).unwrap_or_else(|| domain.clone());
                        let description = extract_meta(page_html, "description").unwrap_or_default();
                        let keywords = extract_keywords(page_html);
                        let url = if rel.is_empty() {
                            format!("http://{}/", domain)
                        } else {
                            format!("http://{}/{}", domain, rel)
                        };
                        docs.push(DocMeta {
                            domain: domain.clone(),
                            title,
                            description,
                            keywords: keywords.clone(),
                            url: url.clone(),
                            dup_count: 1,
                        });
                        fingerprints.insert(fh, (fp, fp_len, docs.len() - 1));
                        // 外链发现(爬虫种子)
                        for link in crate::crawler::extract_links(page_html, &url) {
                            if let Some(d) = crate::crawler::domain_of(&link) {
                                if d != domain {
                                    discovered.insert(d);
                                }
                            }
                        }
                        // 进度 + 实时日志(什么网站、什么页面、什么关键词)
                        if let Some(st) = progress {
                            let mut s = st.lock().unwrap();
                            s.keywords = keywords.clone();
                            drop(s);
                        }
                        let kw_str = if keywords.is_empty() { "无".to_string() } else { keywords.join("、") };
                        crate::logger::push(format!("[index] 页面 {}/{}: 关键词[{}]", domain, rel, kw_str));
                    }
                    continue; // crawled 站已页面级处理完
                }
                let html_path = dir.join("index.html");
                if !html_path.exists() {
                    continue;
                }
                let html = match fs::read_to_string(&html_path) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                // 内容指纹:主页文本(去域名化)
                let fp_text = crate::blacklist::fingerprint_text(&domain, &extract_page_text(&html));
                let fh = crate::blacklist::fnv1a64(&fp_text);
                let fp = crate::blacklist::simhash(&fp_text);
                let fp_len = fp_text.len();
                // 计数去重:精确重复(fnv 相同)或雷同(simhash 近 + 长度差 <=30%)
                let mut dup_target: Option<usize> = None;
                if let Some(&(_, _, doc_id)) = fingerprints.get(&fh) {
                    dup_target = Some(doc_id);
                } else if fp_len >= 128 {
                    // 短文本(<128 字节)只做精确重复:SimHash 特征少,共享短语易误判
                    for (_, (h, len, doc_id)) in fingerprints.iter() {
                        let max_len = fp_len.max(*len).max(1);
                        if fp_len.abs_diff(*len) * 10 > max_len * 3 {
                            continue; // 长度差 > 30% 不算雷同
                        }
                        if crate::blacklist::hamming(fp, *h) <= 6 {
                            dup_target = Some(*doc_id);
                            break;
                        }
                    }
                }
                if let Some(doc_id) = dup_target {
                    // 重复:计数到保留文档,本站不建文档
                    docs[doc_id].dup_count += 1;
                    deduped += 1;
                    if let Some(st) = progress {
                        st.lock().unwrap().dup = deduped;
                    }
                    continue;
                }

                // 发现站内全部页面(.html,含子目录)
                let pages = discover_pages(&dir);
                let mut page_urls: Vec<String> = Vec::new();
                // 解析站点已有 sitemap.xml:<loc> URL 也纳入发现
                if let Ok(sm) = fs::read_to_string(dir.join("sitemap.xml")) {
                    page_urls.extend(extract_sitemap_locs(&sm));
                }
                // 外链/友链/JS 链接发现:本站之外的新网站(写入爬虫种子)
                let base_url = format!("https://{}/", domain);
                let mut links_found = 0usize;
                for link in crate::crawler::extract_links(&html, &base_url) {
                    if let Some(d) = crate::crawler::domain_of(&link) {
                        if d != domain {
                            discovered.insert(d);
                            links_found += 1;
                        }
                    }
                }
                // 当前站点关键词(主页 meta)
                let site_keywords = extract_keywords(&html);
                if let Some(st) = progress {
                    let mut s = st.lock().unwrap();
                    s.keywords = site_keywords.clone();
                    s.links_found = links_found;
                    drop(s);
                }
                // 面板实时日志:什么网站、什么关键词、发现什么链接
                let kw_str = if site_keywords.is_empty() { "无".to_string() } else { site_keywords.join("、") };
                if links_found > 0 {
                    crate::logger::push(format!("[index] 站点 {}: 关键词[{}], 发现 {} 个外链", domain, kw_str, links_found));
                } else {
                    crate::logger::push(format!("[index] 站点 {}: 关键词[{}], 无外链", domain, kw_str));
                }
                for (rel, page_html) in &pages {
                    let title = extract_title(page_html).unwrap_or_else(|| domain.clone());
                    let description = extract_meta(page_html, "description").unwrap_or_default();
                    let keywords = extract_keywords(page_html);
                    let url = if rel.is_empty() {
                        format!("https://{}/", domain)
                    } else {
                        format!("https://{}/{}", domain, rel)
                    };
                    page_urls.push(url.clone());
                    let doc_id = docs.len();
                    docs.push(DocMeta {
                        domain: domain.clone(),
                        title,
                        description,
                        keywords: keywords.clone(),
                        url,
                        dup_count: 1,
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
                    for term in tokenizer.tokenize_str(&text) {
                        if !seen.insert(term.clone()) {
                            continue;
                        }
                        let chunk = chunk_of(&term);
                        blocks.entry(chunk).or_default().entry(term).or_default().push(doc_id);
                    }
                }
                // 记录指纹(指向站点首页文档)
                fingerprints.insert(fh, (fp, fp_len, docs.len() - pages.len()));
                // 生成 sitemap.xml(主页 + <a> 发现 + 发现的页面)
                let _ = gen_sitemap(&dir, &domain, &html, &page_urls);
            }
        }
        } // for sd
        if deduped > 0 {
            println!("[index] 内容计数去重: {} 个站点与已有内容重复/雷同(计数合并)", deduped);
        }
        if blocked > 0 {
            println!("[index] 跳过手动黑名单域名: {} 个", blocked);
        }
        // 外链发现结果 -> 爬虫种子文件
        if !discovered.is_empty() {
            let mut seeds: Vec<String> = discovered.into_iter().collect();
            seeds.sort();
            let disc_path = data_dir.join("discovered.txt");
            let _ = fs::write(&disc_path, seeds.join("\n"));
            println!("[index] 外链发现新网站 {} 个(爬虫种子已写入 {})", seeds.len(), disc_path.display());
        }

        fs::create_dir_all(data_dir).map_err(|e| format!("创建索引目录失败: {}", e))?;
        fs::create_dir_all(data_dir.join("blocks")).map_err(|e| format!("创建分块目录失败: {}", e))?;
        if let Some(st) = progress {
            let mut s = st.lock().unwrap();
            s.phase = "写盘".into();
            s.sites = docs.len();
            drop(s);
        }
        let index = SiteIndex {
            docs,
            blocks: Mutex::new(blocks),
            data_dir: data_dir.to_path_buf(),
            tokenizer,
        };
        index.save()?;
        // 构建完成:进度状态收尾
        if let Some(st) = progress {
            let mut s = st.lock().unwrap();
            s.running = false;
            s.finished = true;
            s.phase = "完成".into();
            s.sites = index.docs.len();
            s.elapsed_secs = started.elapsed().as_secs_f64();
            drop(s);
        }
        crate::logger::push(format!(
            "[index] 构建完成: {} 站点(页面), 去重合并 {} , 黑名单跳过 {}, 耗时 {:.1}s",
            index.docs.len(),
            deduped,
            blocked,
            started.elapsed().as_secs_f64()
        ));
        Ok(index)
    }

    /// 从磁盘加载(懒加载:先读文档元数据分片,分块文件按需读入)
    pub fn load(data_dir: &Path) -> Result<SiteIndex, String> {
        let docs = load_docs(data_dir);
        let tokenizer = BpeTokenizer::load(&data_dir.join("tokenizer").join("vocab.json"))
            .unwrap_or_else(|| BpeTokenizer::train(&[], 300));
        Ok(SiteIndex {
            docs,
            blocks: Mutex::new(HashMap::new()),
            data_dir: data_dir.to_path_buf(),
            tokenizer,
        })
    }

    /// 持久化索引到磁盘(文档元数据分片存储,每片 DOCS_PER_CHUNK 个)
    pub fn save(&self) -> Result<(), String> {
        // docs 分片:docs_000.json / docs_001.json ...(避免单文件过大)
        let mut chunk: Vec<Json> = Vec::with_capacity(DOCS_PER_CHUNK);
        let mut chunk_idx: usize = 0;
        for d in &self.docs {
            chunk.push(Json::build(vec![
                ("domain", Json::str(&d.domain)),
                ("title", Json::str(&d.title)),
                ("description", Json::str(&d.description)),
                ("keywords", Json::arr(d.keywords.iter().map(|k| Json::str(k)).collect())),
                ("url", Json::str(&d.url)),
                ("dup_count", Json::num(d.dup_count as f64)),
            ]));
            if chunk.len() >= DOCS_PER_CHUNK {
                write_docs_chunk(&self.data_dir, chunk_idx, &chunk)?;
                chunk.clear();
                chunk_idx += 1;
            }
        }
        if !chunk.is_empty() {
            write_docs_chunk(&self.data_dir, chunk_idx, &chunk)?;
        }
        // 清理旧式单文件(如果存在)
        let _ = fs::remove_file(self.data_dir.join("docs.json"));
        // 每块一个文件
        let blocks = self.blocks.lock().unwrap();
        let dir = self.data_dir.join("blocks");
        for (chunk_b, terms) in blocks.iter() {
            let mut m = BTreeMap::new();
            for (term, ids) in terms.iter() {
                m.insert(term.clone(), Json::arr(ids.iter().map(|&i| Json::num(i as f64)).collect()));
            }
            fs::write(dir.join(format!("block_{:02}.json", chunk_name(*chunk_b))), Json::Obj(m).to_string())
                .map_err(|e| format!("写入分块失败: {}", e))?;
        }
        // meta.json
        let meta = Json::build(vec![
            ("version", Json::num(1.0)),
            ("sites", Json::num(self.docs.len() as f64)),
            ("blocks", Json::num(blocks.len() as f64)),
            ("docs_chunks", Json::num(chunk_idx as f64)),
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

/// 写入一个文档分片文件 docs_XXX.json
fn write_docs_chunk(data_dir: &Path, idx: usize, chunk: &[Json]) -> Result<(), String> {
    let path = data_dir.join(format!("docs_{:03}.json", idx));
    fs::write(path, Json::arr(chunk.to_vec()).to_string())
        .map_err(|e| format!("写入文档分片失败: {}", e))
}

/// 从磁盘加载全部文档元数据(支持分片 docs_XXX.json 与旧式单文件 docs.json)
fn load_docs(data_dir: &Path) -> Vec<DocMeta> {
    let mut docs = Vec::new();
    // 优先读分片文件
    let mut any_chunk = false;
    let mut idx = 0usize;
    loop {
        let path = data_dir.join(format!("docs_{:03}.json", idx));
        if !path.exists() {
            break;
        }
        any_chunk = true;
        if let Some(mut chunk) = parse_docs_file(&path) {
            docs.append(&mut chunk);
        }
        idx += 1;
    }
    if !any_chunk {
        // 兼容旧式单文件 docs.json
        let legacy = data_dir.join("docs.json");
        if legacy.exists() {
            if let Some(mut chunk) = parse_docs_file(&legacy) {
                docs.append(&mut chunk);
            }
        }
    }
    docs
}

/// 解析一个文档 JSON 文件(数组)为 DocMeta 列表
fn parse_docs_file(path: &Path) -> Option<Vec<DocMeta>> {
    let text = fs::read_to_string(path).ok()?;
    let j = crate::json::parse(&text).ok()?;
    let arr = j.as_arr()?;
    Some(
        arr.iter()
            .filter_map(|item| {
                let dup = item.get("dup_count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                Some(DocMeta {
                    domain: item.get("domain")?.as_str()?.to_string(),
                    title: item.get("title")?.as_str()?.to_string(),
                    description: item.get("description")?.as_str()?.to_string(),
                    url: item.get("url")?.as_str()?.to_string(),
                    keywords: item
                        .get("keywords")?
                        .as_arr()?
                        .iter()
                        .filter_map(|k| k.as_str().map(|s| s.to_string()))
                        .collect(),
                    dup_count: dup.max(1),
                })
            })
            .collect(),
    )
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

/// 从 HTML 提取 <title>(全 lower 提取,索引自洽:to_lowercase 变长 Unicode 如 İ
/// 会导致 lower 索引与 html 错位 panic;标题小写化,中文不受影响)
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title>")? + 7;
    let end = lower[start..].find("</title>")? + start;
    Some(lower[start..end].trim().to_string())
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
                    // tag 是 lower 子串,索引自洽(全 lower 提取,避免变长 Unicode 错位)
                    return Some(tag[cs..ce].trim().to_string());
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

/// 收集语料(用于 BPE 训练):站点主页文本 + 关键词,去重由训练器完成
pub fn collect_corpus(sites_dir: &Path) -> Vec<String> {
    let mut corpus: Vec<String> = Vec::new();
    if sites_dir.exists() {
        let mut count = 0;
        if let Ok(entries) = fs::read_dir(sites_dir) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let html_path = dir.join("index.html");
                if let Ok(html) = fs::read_to_string(&html_path) {
                    corpus.push(extract_page_text(&html));
                    count += 1;
                    if count >= 10_000 {
                        break;
                    }
                }
            }
        }
    }
    // 关键词补充(保证核心词在词表中)
    if let Ok(kws) = crate::config::load_space_list(Path::new("config/keywords.txt")) {
        corpus.extend(kws);
    }
    corpus
}

/// 发现站内全部页面:递归遍历 .html 文件,返回 (相对路径, HTML 内容)
/// 相对路径为空表示 index.html(主页)
fn discover_pages(dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, String)>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let sub = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let next_rel = if rel.is_empty() { sub } else { format!("{}/{}", rel, sub) };
                walk(&p, &next_rel, out);
            } else if p.extension().map(|e| e == "html").unwrap_or(false) {
                let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let rel_path = if rel.is_empty() { name } else { format!("{}/{}", rel, name) };
                if let Ok(h) = fs::read_to_string(&p) {
                    out.push((rel_path, h));
                }
            }
        }
    }
    walk(dir, "", &mut out);
    out
}

/// 提取页面文本(标题+描述+关键词),用于内容指纹
fn extract_page_text(html: &str) -> String {
    let mut s = String::new();
    if let Some(t) = extract_title(html) {
        s.push_str(&t);
    }
    if let Some(d) = extract_meta(html, "description") {
        s.push(' ');
        s.push_str(&d);
    }
    for kw in extract_keywords(html) {
        s.push(' ');
        s.push_str(&kw);
    }
    s
}

/// 解析 sitemap.xml 的 <loc> URL 列表
pub fn extract_sitemap_locs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = xml.to_lowercase();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<loc>") {
        let start = pos + rel + 5;
        if let Some(end_rel) = lower[start..].find("</loc>") {
            let end = start + end_rel;
            // 全 lower 提取(索引自洽;URL 小写无害)
            let loc = lower[start..end].trim().to_string();
            if !loc.is_empty() {
                out.push(loc);
            }
            pos = end + 6;
        } else {
            break;
        }
    }
    out
}

/// 为站点生成 sitemap.xml(主页 + <a> 链接发现 + 发现的页面)
fn gen_sitemap(site_dir: &Path, domain: &str, html: &str, discovered: &[String]) -> Result<(), String> {
    // 从 <a href> 提取站内链接
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
    // 加入发现的页面
    urls.extend(discovered.iter().cloned());
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

#[cfg(test)]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_title_meta_no_panic_unicode() {
        // İ (U+0130) 小写化变长:旧代码 lower 索引切 html panic
        let html = "<html><head><title>İSTANBUL 中文站</title><meta name=\"description\" content=\"移动互联网 İ 安全\"></head><body>x</body></html>";
        // 全 lower 提取:标题小写化(İ→i̇ 带组合符),但绝不 panic
        let t = extract_title(html).unwrap();
        assert!(t.contains("stanbul") && t.contains("中文站"), "got: {}", t);
        let d = extract_meta(html, "description").unwrap();
        assert!(d.contains("移动互联网") && d.contains("安全"), "got: {}", d);
    }

    #[test]
    fn sitemap_locs_no_panic_unicode() {
        let xml = "<?xml version=\"1.0\"?><urlset><url><loc>https://İstanbul.com/page1</loc></url></urlset>";
        let locs = extract_sitemap_locs(xml);
        assert!(locs.iter().any(|l| l.contains("stanbul.com")), "got {:?}", locs);
    }

    fn test_doc(i: usize) -> DocMeta {
        DocMeta {
            domain: format!("s{}.com", i),
            title: format!("测试站点 {}", i),
            description: "描述内容".into(),
            keywords: vec![],
            url: format!("https://s{}.com/", i),
            dup_count: 1,
        }
    }

    /// 文档分片 roundtrip:600 文档 -> 3 片文件 -> 加载回 600,旧 docs.json 移除
    #[test]
    fn docs_chunk_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pilseo_idx_test_{}", now_secs()));
        fs::create_dir_all(&dir).unwrap();
        let docs: Vec<DocMeta> = (0..600).map(test_doc).collect();
        let idx = SiteIndex {
            docs,
            blocks: Mutex::new(HashMap::new()),
            data_dir: dir.clone(),
            tokenizer: BpeTokenizer::train(&[], 300),
        };
        idx.save().unwrap();
        let chunk_files = fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("docs_"))
            .count();
        assert_eq!(chunk_files, 3, "600 文档应分 3 片(256/片)");
        assert!(!dir.join("docs.json").exists(), "旧式单文件应被移除");
        // 每片应小于 256 条 JSON
        let loaded = SiteIndex::load(&dir).unwrap();
        assert_eq!(loaded.docs.len(), 600);
        assert_eq!(loaded.docs[599].domain, "s599.com");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 旧式单文件 docs.json 兼容加载
    #[test]
    fn legacy_docs_json_compat() {
        let dir = std::env::temp_dir().join(format!("pilseo_idx_legacy_{}", now_secs()));
        fs::create_dir_all(&dir).unwrap();
        let arr = Json::arr(vec![
            Json::build(vec![
                ("domain", Json::str("a.com")),
                ("title", Json::str("A 站")),
                ("description", Json::str("描述")),
                ("keywords", Json::arr(vec![])),
                ("url", Json::str("https://a.com/")),
            ]),
            Json::build(vec![
                ("domain", Json::str("b.com")),
                ("title", Json::str("B 站")),
                ("description", Json::str("描述2")),
                ("keywords", Json::arr(vec![Json::str("b")])),
                ("url", Json::str("https://b.com/")),
            ]),
        ]);
        fs::write(dir.join("docs.json"), arr.to_string()).unwrap();
        let loaded = SiteIndex::load(&dir).unwrap();
        assert_eq!(loaded.docs.len(), 2);
        assert_eq!(loaded.docs[1].keywords, vec!["b".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_sitemap_locs_parses() {
        let xml = r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
          <url><loc>https://a.com/</loc></url>
          <url><loc>https://a.com/about.html</loc></url>
        </urlset>"#;
        let locs = extract_sitemap_locs(xml);
        assert_eq!(locs.len(), 2);
        assert_eq!(locs[1], "https://a.com/about.html");
    }

    /// 折叠键:站群模板标题去掉域名后缀
    #[test]
    fn fold_key_strips_domain() {
        assert_eq!(crate::search::fold_key("智能家居 - k.eu", "k.eu"), "智能家居");
        assert_eq!(crate::search::fold_key("智能家居 - k.eu", "k.eu").len(), "智能家居".len());
        assert_eq!(crate::search::fold_key("宠物 - a.com", "a.com"), "宠物");
        // 不同标题不折叠
        assert_ne!(crate::search::fold_key("智能家居 - k.eu", "k.eu"), crate::search::fold_key("人工智能 - k.eu", "k.eu"));
    }
}
