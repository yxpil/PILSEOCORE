//! 搜索统计:查询热词、搜索总量、时段统计(内存聚合,周期落盘)
//!
//! - 查询词 -> 次数(内存 HashMap,高频查询即"热词")
//! - 每日搜索量(简单按日期计数)
//! - 周期写盘 data/index/search_stats.json(避免频繁 IO,内存优先)

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::json::Json;

#[derive(Clone, Debug, Default)]
pub struct SearchStats {
    /// 查询词 -> 次数
    pub queries: HashMap<String, u64>,
    /// 日期(YYYY-MM-DD) -> 搜索量
    pub daily: HashMap<String, u64>,
    pub total: u64,
    pub started_at: u64,
    /// 聚合搜索(借外部引擎)次数
    pub meta_searches: u64,
    /// 聚合缓存命中/未命中
    pub meta_cache_hits: u64,
    pub meta_cache_misses: u64,
    /// 各引擎返回结果数(来源占比)
    pub meta_engines: HashMap<String, u64>,
    /// 性能:本地搜索响应时间累计(毫秒)与样本数
    pub latency_sum_ms: u64,
    pub latency_samples: u64,
}

pub struct StatsCollector {
    path: PathBuf,
    stats: Mutex<SearchStats>,
    /// 距上次写盘的秒数(内存优先,周期落盘)
    dirty_since: Mutex<std::time::Instant>,
}

impl StatsCollector {
    pub fn new(data_dir: &Path) -> StatsCollector {
        let path = data_dir.join("search_stats.json");
        let mut stats = SearchStats {
            started_at: now_secs(),
            ..Default::default()
        };
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(j) = crate::json::parse(&text) {
                if let Some(qs) = j.get("queries").and_then(|v| v.as_arr()) {
                    for q in qs {
                        if let (Some(k), Some(v)) = (q.get("q").and_then(|x| x.as_str()), q.get("n").and_then(|x| x.as_u64())) {
                            stats.queries.insert(k.to_string(), v);
                        }
                    }
                }
                if let Some(ds) = j.get("daily").and_then(|v| v.as_arr()) {
                    for d in ds {
                        if let (Some(k), Some(v)) = (d.get("d").and_then(|x| x.as_str()), d.get("n").and_then(|x| x.as_u64())) {
                            stats.daily.insert(k.to_string(), v);
                        }
                    }
                }
                stats.total = j.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                stats.started_at = j.get("started_at").and_then(|v| v.as_u64()).unwrap_or_else(now_secs);
            }
        }
        StatsCollector {
            path,
            stats: Mutex::new(stats),
            dirty_since: Mutex::new(std::time::Instant::now()),
        }
    }

    /// 记录一次搜索(含响应耗时)
    pub fn record(&self, query: &str, latency_ms: u64) {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return;
        }
        let today = date_str();
        {
            let mut s = self.stats.lock().unwrap();
            *s.queries.entry(q.clone()).or_insert(0) += 1;
            *s.daily.entry(today).or_insert(0) += 1;
            s.total += 1;
            s.latency_sum_ms += latency_ms;
            s.latency_samples += 1;
        }
        // 周期落盘(每 60 秒)
        let mut dirty = self.dirty_since.lock().unwrap();
        if dirty.elapsed().as_secs() >= 60 {
            self.save();
            *dirty = std::time::Instant::now();
        }
    }

    /// 记录一次聚合搜索(引擎来源统计 + 缓存命中/未命中)
    pub fn record_meta(&self, engines: &[String], from_cache: bool) {
        let mut s = self.stats.lock().unwrap();
        s.meta_searches += 1;
        if from_cache {
            s.meta_cache_hits += 1;
        } else {
            s.meta_cache_misses += 1;
        }
        for e in engines {
            *s.meta_engines.entry(e.clone()).or_insert(0) += 1;
        }
    }

    /// 性能与聚合统计快照(供管理面板)
    pub fn perf_snapshot(&self) -> (f64, u64, u64, u64, u64, Vec<(String, u64)>) {
        let s = self.stats.lock().unwrap();
        let avg_ms = if s.latency_samples > 0 {
            s.latency_sum_ms as f64 / s.latency_samples as f64
        } else {
            0.0
        };
        let mut engines: Vec<(String, u64)> = s.meta_engines.iter().map(|(k, v)| (k.clone(), *v)).collect();
        engines.sort_by(|a, b| b.1.cmp(&a.1));
        (
            avg_ms,
            s.meta_searches,
            s.meta_cache_hits,
            s.meta_cache_misses,
            s.latency_samples,
            engines,
        )
    }

    /// 热门查询词 top N
    pub fn top_queries(&self, n: usize) -> Vec<(String, u64)> {
        let s = self.stats.lock().unwrap();
        let mut v: Vec<(String, u64)> = s.queries.iter().map(|(k, v)| (k.clone(), *v)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n.max(1));
        v
    }

    pub fn total(&self) -> u64 {
        self.stats.lock().unwrap().total
    }

    pub fn daily_stats(&self) -> Vec<(String, u64)> {
        let s = self.stats.lock().unwrap();
        let mut v: Vec<(String, u64)> = s.daily.iter().map(|(k, v)| (k.clone(), *v)).collect();
        v.sort();
        v
    }

    /// 写盘(内存优先:仅在周期/退出时落盘)
    pub fn save(&self) {
        let s = self.stats.lock().unwrap();
        let queries: Vec<Json> = s
            .queries
            .iter()
            .map(|(k, v)| Json::build(vec![("q", Json::str(k)), ("n", Json::num(*v as f64))]))
            .collect();
        let daily: Vec<Json> = s
            .daily
            .iter()
            .map(|(k, v)| Json::build(vec![("d", Json::str(k)), ("n", Json::num(*v as f64))]))
            .collect();
        let j = Json::build(vec![
            ("queries", Json::arr(queries)),
            ("daily", Json::arr(daily)),
            ("total", Json::num(s.total as f64)),
            ("started_at", Json::num(s.started_at as f64)),
        ]);
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&self.path, j.to_string());
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn date_str() -> String {
    // 简易 UTC 日期(YYYY-MM-DD)
    let secs = now_secs();
    let days = secs / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// 天数 -> 公历日期(Howard Hinnant 算法)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_record_and_top() {
        let dir = std::env::temp_dir().join(format!("pilseo_stats_test_{}", now_secs()));
        let sc = StatsCollector::new(&dir);
        sc.record("智能家居", 3);
        sc.record("智能家居", 3);
        sc.record("人工智能", 5);
        assert_eq!(sc.total(), 3);
        let top = sc.top_queries(5);
        assert_eq!(top[0].0, "智能家居");
        assert_eq!(top[0].1, 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
