//! 黑名单管理(手动):管理员拉黑/解除域名,搜索实时过滤
//!
//! 注意:内容重复/雷同的域名**不自动拉黑**——索引层做计数去重
//! (相同内容只保留一条,dup_count 记录重复数量),黑名单仅用于手动管理。
//!
//! 另提供内容指纹工具函数(FNV-1a / SimHash),供索引层去重使用。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::json::Json;

/// FNV-1a 64 确定性哈希(跨进程稳定)
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// SimHash:字符 4-gram 特征,输出 64 位指纹
pub fn simhash(text: &str) -> u64 {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return 0;
    }
    let mut v = [0i64; 64];
    if chars.len() < 4 {
        // 短文本:整体作为特征
        let h = fnv1a64(text);
        for i in 0..64 {
            if (h >> i) & 1 == 1 {
                v[i] += 1;
            } else {
                v[i] -= 1;
            }
        }
    } else {
        for w in chars.windows(4) {
            let gram: String = w.iter().collect();
            let h = fnv1a64(&gram);
            for i in 0..64 {
                if (h >> i) & 1 == 1 {
                    v[i] += 1;
                } else {
                    v[i] -= 1;
                }
            }
        }
    }
    let mut out: u64 = 0;
    for i in 0..64 {
        if v[i] > 0 {
            out |= 1u64 << i;
        }
    }
    out
}

/// 海明距离
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// 指纹文本:去掉域名与分隔符,模板站指纹一致、原创站区分明显
pub fn fingerprint_text(domain: &str, text: &str) -> String {
    let mut s = text.replace(domain, "");
    s = s.replace(" - ", " ").replace('-', " ");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug)]
pub struct BlacklistEntry {
    pub domain: String,
    pub reason: String, // manual
    pub added_at: u64,
}

/// 黑名单(手动管理):拉黑域名,搜索实时过滤
pub struct Blacklist {
    path: PathBuf,
    entries: Mutex<Vec<BlacklistEntry>>,
}

impl Blacklist {
    pub fn load(data_dir: &Path) -> Blacklist {
        let path = data_dir.join("blacklist.json");
        let mut entries = Vec::new();
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(j) = crate::json::parse(&text) {
                if let Some(arr) = j.get("blacklisted").and_then(|v| v.as_arr()) {
                    for item in arr {
                        entries.push(BlacklistEntry {
                            domain: item.get("domain").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            reason: item.get("reason").and_then(|v| v.as_str()).unwrap_or("manual").to_string(),
                            added_at: item.get("added_at").and_then(|v| v.as_u64()).unwrap_or(0),
                        });
                    }
                }
            }
        }
        Blacklist {
            path,
            entries: Mutex::new(entries),
        }
    }

    fn save(&self) {
        let entries: Vec<Json> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|e| {
                Json::build(vec![
                    ("domain", Json::str(&e.domain)),
                    ("reason", Json::str(&e.reason)),
                    ("added_at", Json::num(e.added_at as f64)),
                ])
            })
            .collect();
        let j = Json::build(vec![("blacklisted", Json::arr(entries))]);
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&self.path, j.to_string());
    }

    pub fn is_blocked(&self, domain: &str) -> bool {
        self.entries.lock().unwrap().iter().any(|e| e.domain == domain)
    }

    pub fn add(&self, domain: &str, reason: &str) {
        let mut entries = self.entries.lock().unwrap();
        if !entries.iter().any(|e| e.domain == domain) {
            entries.push(BlacklistEntry {
                domain: domain.to_string(),
                reason: reason.to_string(),
                added_at: now_secs(),
            });
        }
        drop(entries);
        self.save();
    }

    pub fn remove(&self, domain: &str) -> bool {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|e| e.domain != domain);
        let removed = entries.len() != before;
        drop(entries);
        if removed {
            self.save();
        }
        removed
    }

    pub fn list(&self) -> Vec<BlacklistEntry> {
        self.entries.lock().unwrap().clone()
    }

    pub fn blocked_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

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
    fn simhash_detects_similar() {
        // 真实规模的页面文本(标题+描述+关键词);雷同仅微小差异(如标点/个别词)
        let a = simhash("智能家居 智能家居相关资讯、资源与最新动态,尽在本站,持续更新优质内容,欢迎访问,更多精彩等你发现。");
        let b = simhash("智能家居 智能家居相关资讯、资源与最新动态,尽在本站,持续更新优质内容,欢迎访问,更多精彩等你发现!");
        let c = simhash("宠物用品 猫粮狗粮 宠物美容 宠物医疗 宠物训练 宠物寄养 宠物商店 宠物医院 宠物摄影。");
        assert!(hamming(a, b) <= 6, "雷同内容距离应小: {}", hamming(a, b));
        assert!(hamming(a, c) > 6, "不同内容距离应大: {}", hamming(a, c));
    }

    #[test]
    fn fingerprint_text_strips_domain() {
        let t = fingerprint_text("bad2.com", "智能家居 - bad2.com 智能家居相关资讯、资源与最新动态");
        assert!(!t.contains("bad2"), "域名应被移除: {}", t);
        assert!(t.contains("智能家居"), "内容应保留: {}", t);
    }

    #[test]
    fn blacklist_manual_manage() {
        let dir = std::env::temp_dir().join(format!("pilseo_bl_test_{}", now_secs()));
        let bl = Blacklist::load(&dir);
        assert!(!bl.is_blocked("a.com"));
        bl.add("a.com", "manual");
        assert!(bl.is_blocked("a.com"));
        assert_eq!(bl.list().len(), 1);
        assert!(bl.remove("a.com"));
        assert!(!bl.is_blocked("a.com"));
        let _ = fs::remove_dir_all(&dir);
    }
}
