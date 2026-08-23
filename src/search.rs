//! 搜索引擎:分词器(BPE 81920 词表)+ 相关度排序 + 同标题折叠 + 分页 + 黑名单过滤
//!
//! 相关度策略:
//!   - 短语匹配:整个查询原文在标题/描述/关键词中完整出现 => 强相关,大权重
//!   - 核心词匹配:BPE 分词后的多字节 token 逐词计分
//!   - 候选收集:精确词表 O(1) 查找(不做全词表编辑距离遍历,避免暴力查找)
//!   - 过滤:短语未命中且核心词零匹配 => 不相关,不返回;黑名单域名直接剔除
//!   - 弱相关降权:只命中部分核心词 => 分数降为 30%
//!   - 同标题折叠:相同标题(去域名后缀)只保留最高分一条,fold_count 记录组大小
//!   - 分页:按页切片返回
//!
//! 基础设施:分块倒排索引(词首字符路由,懒加载)+ 热点 LRU 缓存

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::blacklist::Blacklist;
use crate::index::SiteIndex;
use crate::tokenizer::BpeTokenizer;

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub domain: String,
    pub title: String,
    pub description: String,
    pub url: String,
    pub score: f64,
    /// 相同标题折叠后的组大小(>1 表示还有 N-1 个相同标题站点)
    pub fold_count: usize,
    /// 内容重复计数:相同/雷同内容的站点总数(计数去重,仅保留一条)
    pub dup_count: usize,
}

pub struct SearchEngine {
    index: Mutex<SiteIndex>,
    /// 黑名单(内容去重/雷同自动拉黑 + 手动拉黑),搜索实时过滤
    pub blacklist: Arc<Blacklist>,
    /// 热点缓存:查询词 -> (缓存时间, 总组数, 全部折叠结果)
    hot_cache: Mutex<HotCache>,
    pub cache_hits: Mutex<u64>,
    pub cache_misses: Mutex<u64>,
    /// 索引构建进度(管理面板实时显示)
    index_state: Arc<Mutex<crate::index::IndexState>>,
    /// 重建互斥:同一时刻只允许一个 rebuild(定时任务与手动触发并发会互相踩状态)
    rebuilding: std::sync::atomic::AtomicBool,
}

struct HotCache {
    map: HashMap<String, (Instant, usize, Vec<SearchHit>)>,
    order: VecDeque<String>,
    cap: usize,
    ttl: Duration,
}

impl HotCache {
    fn new(cap: usize, ttl_secs: u64) -> Self {
        HotCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
            ttl: Duration::from_secs(ttl_secs.max(1)),
        }
    }

    fn get(&mut self, key: &str) -> Option<(usize, Vec<SearchHit>)> {
        let (t, total, hits) = self.map.get(key)?;
        if t.elapsed() > self.ttl {
            self.map.remove(key);
            return None;
        }
        Some((*total, hits.clone()))
    }

    fn put(&mut self, key: String, total: usize, hits: Vec<SearchHit>) {
        if self.map.contains_key(&key) {
            return;
        }
        self.map.insert(key.clone(), (Instant::now(), total, hits));
        self.order.push_back(key);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

impl SearchEngine {
    pub fn new(index: SiteIndex, cache_cap: usize, cache_ttl_secs: u64, blacklist: Arc<Blacklist>) -> SearchEngine {
        SearchEngine {
            index: Mutex::new(index),
            blacklist,
            hot_cache: Mutex::new(HotCache::new(cache_cap, cache_ttl_secs)),
            cache_hits: Mutex::new(0),
            cache_misses: Mutex::new(0),
            index_state: Arc::new(Mutex::new(crate::index::IndexState::default())),
            rebuilding: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 是否正在重建索引
    pub fn is_rebuilding(&self) -> bool {
        self.rebuilding.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 索引构建进度状态(管理面板实时查询)
    pub fn index_state(&self) -> Arc<Mutex<crate::index::IndexState>> {
        self.index_state.clone()
    }

    pub fn index(&self) -> &Mutex<SiteIndex> {
        &self.index
    }

    /// 重建索引(全量),返回站点数;内容重复/雷同域名自动拉黑
    /// 进度实时写入 index_state(管理面板进度条)
    /// 并发保护:已有重建进行中时返回 Err(定时任务与手动触发冲突时跳过)
    pub fn rebuild(&self, sites_dir: &std::path::Path, data_dir: &std::path::Path) -> Result<usize, String> {
        if self.rebuilding.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err("已有索引重建在进行中,本次跳过".into());
        }
        let result = (|| {
            let idx = SiteIndex::build_with_progress(sites_dir, data_dir, &self.blacklist, Some(&self.index_state))?;
            let n = idx.docs.len();
            *self.index.lock().unwrap() = idx;
            let mut cache = self.hot_cache.lock().unwrap();
            cache.map.clear();
            cache.order.clear();
            Ok(n)
        })();
        self.rebuilding.store(false, std::sync::atomic::Ordering::SeqCst);
        result
    }

    /// 搜索(分页):返回 (总组数, 当前页结果)
    pub fn search(&self, query: &str, page: usize, page_size: usize) -> (usize, Vec<SearchHit>) {
        let q = query.trim();
        let page = page.max(1);
        let page_size = page_size.max(1);
        if q.is_empty() {
            return (0, Vec::new());
        }
        // 热点缓存(全量折叠结果缓存,分页只是切片)
        {
            let mut cache = self.hot_cache.lock().unwrap();
            if let Some((total, all)) = cache.get(q) {
                *self.cache_hits.lock().unwrap() += 1;
                return slice_page(total, all, page, page_size);
            }
        }
        *self.cache_misses.lock().unwrap() += 1;

        let (total, all) = self.search_all(q);
        // 写入热点缓存
        self.hot_cache.lock().unwrap().put(q.to_string(), total, all.clone());
        slice_page(total, all, page, page_size)
    }

    /// 联想建议:前缀匹配索引词 + 按出现文档数排序;热点查询历史也参与
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<String> {
        let p = prefix.trim().to_lowercase();
        if p.is_empty() {
            return Vec::new();
        }
        let mut counts: HashMap<String, usize> = HashMap::new();
        {
            let cache = self.hot_cache.lock().unwrap();
            for k in cache.order.iter() {
                if k.starts_with(&p) {
                    counts.insert(k.clone(), 100);
                }
            }
        }
        let idx = self.index.lock().unwrap();
        let chunk = crate::index::chunk_of(&p);
        idx.ensure_block(chunk);
        let blocks = idx.blocks.lock().unwrap();
        if let Some(term_map) = blocks.get(&chunk) {
            for (word, ids) in term_map.iter() {
                if word.starts_with(&p) {
                    let e = counts.entry(word.clone()).or_insert(0);
                    *e += ids.len().min(50);
                }
            }
        }
        drop(blocks);
        drop(idx);
        let mut v: Vec<(String, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().take(limit.max(1)).map(|(s, _)| s).collect()
    }

    pub fn cache_stats(&self) -> (u64, u64) {
        (*self.cache_hits.lock().unwrap(), *self.cache_misses.lock().unwrap())
    }

    /// 清空热点缓存(黑名单变更/重建索引后调用,避免旧结果残留)
    pub fn clear_cache(&self) {
        let mut cache = self.hot_cache.lock().unwrap();
        cache.map.clear();
        cache.order.clear();
    }

    /// 全量搜索(折叠后),返回 (总组数, 全部折叠结果按相关度排序)
    fn search_all(&self, q: &str) -> (usize, Vec<SearchHit>) {
        let phrase = q.trim().to_lowercase();
        let idx = self.index.lock().unwrap();
        if idx.docs.is_empty() {
            return (0, Vec::new());
        }
        let tok = &idx.tokenizer;
        let core = core_terms(q, tok);

        // ---- 候选收集:BPE 核心词精确查倒排(O(1) 词表查找,不做全表遍历) ----
        let mut candidates: HashSet<usize> = HashSet::new();
        let terms: Vec<String> = if core.is_empty() {
            // 纯单字节/无核心词查询:仍过滤低信息 token,
            // 避免 UTF-8 字节残片(如 0xE5)命中所有中文文档导致不相关结果刷屏
            tok.tokenize_str(q)
                .into_iter()
                .filter(|t| t.len() >= 2 && !t.contains('\u{FFFD}'))
                .collect()
        } else {
            core.clone()
        };
        // 查询词在语料中无有效匹配(全部 token 均为低信息残片)→ 无结果
        if terms.is_empty() {
            return (0, Vec::new());
        }
        for term in &terms {
            let chunk = crate::index::chunk_of(term);
            idx.ensure_block(chunk);
            let blocks = idx.blocks.lock().unwrap();
            if let Some(term_map) = blocks.get(&chunk) {
                if let Some(ids) = term_map.get(term) {
                    candidates.extend(ids.iter().copied());
                }
            }
        }
        if candidates.is_empty() {
            return (0, Vec::new());
        }

        // ---- 黑名单过滤(自动拉黑 + 手动拉黑) ----
        candidates.retain(|&doc_id| !self.blacklist.is_blocked(&idx.docs[doc_id].domain));

        // ---- 相关度评分 ----
        let mut scored: Vec<(usize, f64)> = Vec::with_capacity(candidates.len());
        for &doc_id in &candidates {
            let doc = &idx.docs[doc_id];
            let score = relevance_score(doc, &phrase, &core);
            if score > 0.0 {
                scored.push((doc_id, score));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // ---- 同标题折叠:相同标题(去域名后缀)只保留最高分,fold_count = 组大小 ----
        // 注意:全程持有 idx 锁,禁止二次 lock(rebuild 并发替换索引会导致
        // 旧 blocks 的 doc_id 访问新 docs 越界)
        let mut groups: Vec<(SearchHit, usize)> = Vec::new(); // (组内最佳, 组大小)
        let mut seen: HashMap<String, usize> = HashMap::new(); // fold_key -> groups 下标
        for (doc_id, score) in scored {
            let doc = &idx.docs[doc_id];
            let key = fold_key(&doc.title, &doc.domain);
            if let Some(&gi) = seen.get(&key) {
                groups[gi].1 += 1;
            } else {
                seen.insert(key, groups.len());
                groups.push((
                    SearchHit {
                        domain: doc.domain.clone(),
                        title: doc.title.clone(),
                        description: doc.description.clone(),
                        url: doc.url.clone(),
                        score,
                        fold_count: 1,
                        dup_count: doc.dup_count,
                    },
                    1,
                ));
            }
        }
        let mut hits: Vec<SearchHit> = groups
            .into_iter()
            .map(|(mut h, n)| {
                h.fold_count = n;
                h
            })
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let total = hits.len();
        (total, hits)
    }
}

/// 按页切片
fn slice_page(total: usize, all: Vec<SearchHit>, page: usize, page_size: usize) -> (usize, Vec<SearchHit>) {
    let start = (page - 1) * page_size;
    if start >= all.len() {
        return (total, Vec::new());
    }
    let end = (start + page_size).min(all.len());
    (total, all[start..end].to_vec())
}

/// 折叠键:标题去掉" - <域名>"后缀后的小写形式。
/// 站群模板标题如 "智能家居 - k.eu" 折叠为 "智能家居",避免同质结果刷屏
pub fn fold_key(title: &str, domain: &str) -> String {
    let t = title.trim();
    let d = domain.trim();
    let lower_t = t.to_lowercase();
    let suffix = format!(" - {}", d);
    let suffix_l = suffix.to_lowercase();
    if lower_t.ends_with(&suffix_l) {
        return t[..t.len() - suffix.len()].trim().to_string();
    }
    // 兼容 title 直接含域名后缀但格式不同(如 "xxx.domain.com")
    if let Some(pos) = lower_t.rfind(d) {
        if pos + d.len() == lower_t.len() {
            return t[..pos].trim().to_string();
        }
    }
    lower_t
}

/// 核心词:BPE 分词后的多字节 token(词/子词),去重,排除单字节噪音与 lossy 替换字符
fn core_terms(q: &str, tok: &BpeTokenizer) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for t in tok.tokenize_str(q) {
        // 跳过:单字节 token、lossy 替换字符(未合并的孤立字节)
        if t.len() < 2 || t.contains('\u{FFFD}') {
            continue;
        }
        if seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out
}

/// 相关度评分:短语命中大权重;核心词逐字段计分;弱相关降权/过滤
fn relevance_score(doc: &crate::index::DocMeta, phrase: &str, core: &[String]) -> f64 {
    let title = doc.title.to_lowercase();
    let desc = doc.description.to_lowercase();
    let domain = doc.domain.to_lowercase();
    let kw: Vec<String> = doc.keywords.iter().map(|k| k.to_lowercase()).collect();

    let mut score = 0.0;
    let mut phrase_hit = false;
    let mut matched_terms = 0usize; // 命中的不同核心词数量

    // 短语完整匹配(最强相关信号)
    if !phrase.is_empty() {
        if title.contains(phrase) {
            score += 30.0;
            phrase_hit = true;
        }
        if desc.contains(phrase) {
            score += 20.0;
            phrase_hit = true;
        }
        if kw.iter().any(|k| k.contains(phrase)) {
            score += 15.0;
        }
        if domain.contains(phrase) {
            score += 10.0;
            phrase_hit = true;
        }
    }

    // 核心词逐字段计分
    for term in core {
        let mut hit = false;
        if title.contains(term) {
            score += 8.0;
            hit = true;
        }
        if desc.contains(term) {
            score += 4.0;
            hit = true;
        }
        if domain.contains(term) {
            score += 2.0;
            hit = true;
        }
        if kw.iter().any(|k| k.contains(term)) {
            score += 2.0;
            hit = true;
        }
        if hit {
            matched_terms += 1;
        }
    }

    // 相关度过滤:短语未命中且核心词零匹配 => 不相关
    if !phrase_hit && matched_terms == 0 {
        // 无核心词时,候选已由多字节有效 token 过滤(单字节残片已被剔除),
        // 保留弱相关(弱相关度),避免结果为空
        if core.is_empty() {
            return 5.0;
        }
        return 0.0;
    }
    // 弱相关降权:短语未命中且只命中部分核心词
    if !phrase_hit && !core.is_empty() && matched_terms < core.len() {
        score *= 0.3;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::DocMeta;

    fn fake_index(docs: Vec<DocMeta>) -> SearchEngine {
        let dir = std::env::temp_dir().join(format!(
            "pilseo_srch_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ));
        let bl = Arc::new(Blacklist::load(&dir));
        // 用文档文本训练 BPE(中文词合并,贴近真实索引)
        let mut corpus: Vec<String> = Vec::new();
        for d in &docs {
            corpus.push(format!("{} {} {} {}", d.title, d.description, d.domain, d.keywords.join(" ")));
        }
        let tok = BpeTokenizer::train(&corpus, 300);
        let idx = SiteIndex::from_docs_with_tokenizer(docs, tok);
        SearchEngine::new(idx, 10, 60, bl)
    }

    fn doc(domain: &str, title: &str, desc: &str, kw: &[&str]) -> DocMeta {
        DocMeta {
            domain: domain.to_string(),
            title: title.to_string(),
            description: desc.to_string(),
            url: format!("https://{}/", domain),
            keywords: kw.iter().map(|s| s.to_string()).collect(),
            dup_count: 1,
        }
    }

    #[test]
    fn relevance_filters_irrelevant() {
        let engine = fake_index(vec![
            doc("abc.com", "智能家居 - abc.com", "智能家居 相关资讯", &["智能家居"]),
            doc("xyz.com", "人工智能 - xyz.com", "人工智能 相关资讯", &["人工智能"]),
            doc("123.net", "宠物用品 - 123.net", "宠物用品 介绍", &["宠物"]),
        ]);
        // 搜"智能家居":abc 强相关(短语命中);宠物完全不相关被过滤
        // 注:极小语料下 BPE 整词合并,xyz("人工智能")与"智能家居"无共享子词,
        // 不进候选(真实大语料下会因共享"智能"子词弱相关命中)
        let (total, hits) = engine.search("智能家居", 1, 10);
        assert_eq!(total, 1, "宠物站点应被过滤,xyz 在极小语料下不共享子词");
        assert_eq!(hits[0].domain, "abc.com", "abc.com 应排第一,实际: {:?}", hits.iter().map(|h| &h.domain).collect::<Vec<_>>());
    }

    #[test]
    fn out_of_corpus_query_returns_empty() {
        // 语料只有中文模板站;查询词不在语料(BPE 分词为 UTF-8 字节残片)
        // 必须返回空,而不是让单字节残片命中所有中文文档刷屏
        let engine = fake_index(vec![
            doc("abc.com", "智能家居 - abc.com", "智能家居 相关资讯", &["智能家居"]),
            doc("xyz.com", "人工智能 - xyz.com", "人工智能 相关资讯", &["人工智能"]),
        ]);
        let (total, hits) = engine.search("哔哩哔哩", 1, 10);
        assert_eq!(total, 0, "语料外的查询应返回空,实际 total={}", total);
        assert!(hits.is_empty());
        // 语料内查询不受影响
        let (total2, hits2) = engine.search("智能家居", 1, 10);
        assert!(total2 >= 1, "语料内查询应正常命中, total={}", total2);
        assert!(!hits2.is_empty());
    }

    #[test]
    fn fold_same_title() {
        let engine = fake_index(vec![
            doc("a.com", "智能家居 - a.com", "智能家居", &["智能家居"]),
            doc("b.com", "智能家居 - b.com", "智能家居", &["智能家居"]),
            doc("c.com", "智能家居 - c.com", "智能家居", &["智能家居"]),
        ]);
        {
            let idx = engine.index().lock().unwrap();
            println!("D2 vocab len: {}", idx.tokenizer.vocab_size());
            let toks = idx.tokenizer.tokenize_str("智能家居");
            println!("D2 tokenize: {:?}", toks.iter().map(|t| (t.clone(), t.len())).collect::<Vec<_>>());
            drop(idx);
        }
        let (total, hits) = engine.search("智能家居", 1, 10);
        assert_eq!(total, 1, "相同标题应折叠为一组");
        assert_eq!(hits[0].fold_count, 3);
    }

    #[test]
    fn pagination_slices() {
        let engine = fake_index(vec![
            doc("a.com", "智能家居 - a.com", "智能家居", &["智能家居"]),
            doc("b.com", "智能家居 - b.com", "智能家居", &["智能家居"]),
            doc("c.com", "智能家居 - c.com", "智能家居", &["智能家居"]),
        ]);
        // 折叠后 1 组,分页第 2 页应为空
        let (total, hits) = engine.search("智能家居", 2, 10);
        assert_eq!(total, 1);
        assert!(hits.is_empty());
    }

    #[test]
    fn blacklist_filters_search() {
        let engine = fake_index(vec![
            doc("good.com", "智能家居 - good.com", "智能家居", &["智能家居"]),
            doc("bad.com", "智能家居 - bad.com", "智能家居", &["智能家居"]),
        ]);
        engine.blacklist.add("bad.com", "manual");
        let (total, hits) = engine.search("智能家居", 1, 10);
        assert_eq!(total, 1);
        assert_eq!(hits[0].domain, "good.com");
    }
}
