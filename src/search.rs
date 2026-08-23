//! 搜索引擎:分块查找(按词首字符路由到索引块,懒加载)+
//! 模糊匹配(编辑距离)+ 联想 + 热点 LRU 缓存

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::index::{edit_distance, tokenize, SiteIndex};

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub domain: String,
    pub title: String,
    pub description: String,
    pub url: String,
    pub score: f64,
}

pub struct SearchEngine {
    index: Mutex<SiteIndex>,
    /// 热点缓存:查询词 -> (缓存时间, 结果)
    hot_cache: Mutex<HotCache>,
    pub cache_hits: Mutex<u64>,
    pub cache_misses: Mutex<u64>,
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
    pub fn new(index: SiteIndex, cache_cap: usize, cache_ttl_secs: u64) -> SearchEngine {
        SearchEngine {
            index: Mutex::new(index),
            hot_cache: Mutex::new(HotCache::new(cache_cap, cache_ttl_secs)),
            cache_hits: Mutex::new(0),
            cache_misses: Mutex::new(0),
        }
    }

    pub fn index(&self) -> &Mutex<SiteIndex> {
        &self.index
    }

    /// 重建索引(全量),返回站点数
    pub fn rebuild(&self, sites_dir: &std::path::Path, data_dir: &std::path::Path) -> Result<usize, String> {
        let idx = SiteIndex::build(sites_dir, data_dir)?;
        let n = idx.docs.len();
        *self.index.lock().unwrap() = idx;
        let mut cache = self.hot_cache.lock().unwrap();
        cache.map.clear();
        cache.order.clear();
        Ok(n)
    }

    /// 搜索:支持精确词、子串、编辑距离<=2 的模糊匹配,分块懒加载查找
    /// 返回 (总命中数, 排序后截取 limit 条的结果)
    pub fn search(&self, query: &str, limit: usize) -> (usize, Vec<SearchHit>) {
        let q = query.trim();
        if q.is_empty() {
            return (0, Vec::new());
        }
        // 热点缓存
        {
            let mut cache = self.hot_cache.lock().unwrap();
            if let Some((total, hits)) = cache.get(q) {
                *self.cache_hits.lock().unwrap() += 1;
                return (total, hits.into_iter().take(limit.max(1)).collect());
            }
        }
        *self.cache_misses.lock().unwrap() += 1;

        let terms = tokenize(q);
        let idx = self.index.lock().unwrap();
        let mut scores: HashMap<usize, (f64, usize)> = HashMap::new(); // doc_id -> (score, matched)
        let mut term_hits: HashMap<usize, Vec<String>> = HashMap::new();

        for term in &terms {
            let chunk = crate::index::chunk_of(term);
            idx.ensure_block(chunk);
            let blocks = idx.blocks.lock().unwrap();
            let Some(term_map) = blocks.get(&chunk) else { continue };
            // 精确匹配
            if let Some(ids) = term_map.get(term) {
                for &doc in ids {
                    let e = scores.entry(doc).or_insert((0.0, 0));
                    e.0 += 4.0;
                    e.1 += 1;
                    term_hits.entry(doc).or_default().push(term.clone());
                }
            }
            // 模糊匹配:编辑距离 <= 2(只扫长度相近的词)
            let tl = term.chars().count();
            for (word, ids) in term_map.iter() {
                let wl = word.chars().count();
                if wl.abs_diff(tl) > 2 {
                    continue;
                }
                if word == term {
                    continue;
                }
                let d = edit_distance(term, word);
                if d <= 2 {
                    let boost = if d == 1 { 2.0 } else { 1.0 };
                    for &doc in ids {
                        let e = scores.entry(doc).or_insert((0.0, 0));
                        e.0 += boost;
                        e.1 += 1;
                        term_hits.entry(doc).or_default().push(word.clone());
                    }
                }
            }
        }

        // 组装结果 + 字段加权(标题 > 描述 > 域名 > 关键词)
        let mut hits: Vec<SearchHit> = Vec::new();
        for (doc_id, (mut score, _matched)) in scores {
            let doc = &idx.docs[doc_id];
            let mut boost = 0.0;
            for t in &terms {
                if doc.title.to_lowercase().contains(&t.to_lowercase()) {
                    boost += 3.0;
                }
                if doc.domain.contains(t) {
                    boost += 2.0;
                }
                if doc.description.to_lowercase().contains(&t.to_lowercase()) {
                    boost += 1.0;
                }
                if doc.keywords.iter().any(|k| k.to_lowercase().contains(&t.to_lowercase())) {
                    boost += 1.0;
                }
            }
            score += boost;
            hits.push(SearchHit {
                domain: doc.domain.clone(),
                title: doc.title.clone(),
                description: doc.description.clone(),
                url: doc.url.clone(),
                score,
            });
        }
        drop(idx);
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let total = hits.len();
        let hits = hits.into_iter().take(limit.max(1)).collect::<Vec<_>>();
        // 写入热点缓存
        self.hot_cache.lock().unwrap().put(q.to_string(), total, hits.clone());
        (total, hits)
    }

    /// 联想建议:前缀匹配索引词 + 按出现文档数排序;热点查询历史也参与
    pub fn suggest(&self, prefix: &str, limit: usize) -> Vec<String> {
        let p = prefix.trim().to_lowercase();
        if p.is_empty() {
            return Vec::new();
        }
        let mut counts: HashMap<String, usize> = HashMap::new();
        // 从热点缓存取历史查询
        {
            let cache = self.hot_cache.lock().unwrap();
            for k in cache.order.iter() {
                if k.starts_with(&p) {
                    counts.insert(k.clone(), 100);
                }
            }
        }
        // 扫描索引词表(仅已加载块 + 前缀对应块)
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
}
