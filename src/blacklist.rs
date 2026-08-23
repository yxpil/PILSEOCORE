//! 去重与自动拉黑:内容指纹(SimHash)检测重复/雷同域名
//!
//! - 精确重复:内容哈希相同 => 拉黑
//! - 雷同:SimHash 海明距离 <= 4(模板站"换皮"内容)=> 拉黑
//! - 黑名单持久化 data/blacklist.json;手动拉黑/解除由管理员 API 控制
//!
//! 哈希全部为确定性算法(FNV-1a / 自实现 SimHash),跨进程稳定。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::json::Json;

/// 雷同判定阈值(SimHash 海明距离;指纹已去域名化,模板站指纹高度一致)
const SIM_THRESHOLD: u32 = 6;

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
        let h = fnv1a64(&text);
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

#[derive(Clone, Debug)]
pub struct BlacklistEntry {
    pub domain: String,
    pub reason: String, // duplicate / similar / manual
    pub added_at: u64,
}

/// 黑名单:拉黑域名 + 已索引域名指纹库(用于雷同检测)
pub struct Blacklist {
    path: PathBuf,
    entries: Mutex<Vec<BlacklistEntry>>,
    /// 已索引(白)域名 -> (simhash, fnv, 文本长度);长度差 >30% 不判雷同
    fingerprints: Mutex<HashMap<String, (u64, u64, usize)>>,
}

impl Blacklist {
    pub fn load(data_dir: &Path) -> Blacklist {
        let path = data_dir.join("blacklist.json");
        let (mut entries, mut fingerprints) = (Vec::new(), HashMap::new());
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(j) = crate::json::parse(&text) {
                if let Some(arr) = j.get("blacklisted").and_then(|v| v.as_arr()) {
                    for item in arr {
                        entries.push(BlacklistEntry {
                            domain: item.get("domain").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            reason: item.get("reason").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            added_at: item.get("added_at").and_then(|v| v.as_u64()).unwrap_or(0),
                        });
                    }
                }
                if let Some(fp) = j.get("fingerprints").and_then(|v| v.as_arr()) {
                    for item in fp {
                        if let (Some(d), Some(h)) = (item.get("domain").and_then(|v| v.as_str()), item.get("hash").and_then(|v| v.as_u64())) {
                            let l = item.get("len").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            // 兼容旧格式(无 fnv/len):fnv 缺省用 hash 值占位
                            let f = item.get("fnv").and_then(|v| v.as_u64()).unwrap_or(h);
                            fingerprints.insert(d.to_string(), (h, f, l));
                        }
                    }
                }
            }
        }
        Blacklist {
            path,
            entries: Mutex::new(entries),
            fingerprints: Mutex::new(fingerprints),
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
        let fps: Vec<Json> = self
            .fingerprints
            .lock()
            .unwrap()
            .iter()
            .map(|(d, (h, f, l))| {
                Json::build(vec![
                    ("domain", Json::str(d)),
                    ("hash", Json::num(*h as f64)),
                    ("fnv", Json::num(*f as f64)),
                    ("len", Json::num(*l as f64)),
                ])
            })
            .collect();
        let j = Json::build(vec![
            ("blacklisted", Json::arr(entries)),
            ("fingerprints", Json::arr(fps)),
        ]);
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

    /// 索引前检测:该域名内容是否与已索引域名重复/雷同。
    /// 返回 true = 允许索引;false = 自动拉黑(内容雷同/重复)
    pub fn check_before_index(&self, domain: &str, text: &str) -> bool {
        if self.is_blocked(domain) {
            return false;
        }
        // 指纹文本:去域名化(模板站差异主要来自 title 里的域名,去掉后指纹一致)
        let fp_text = fingerprint_text(domain, text);
        let fp = simhash(&fp_text);
        let fp_len = fp_text.len();
        // 短文本(<128 字节)只做精确重复:SimHash 特征少,共享短语易误判雷同
        if fp_len >= 128 {
            let fingerprints = self.fingerprints.lock().unwrap();
            for (_, (h, _, len)) in fingerprints.iter() {
                // 长度差 > 30% 不算雷同(短原创内容与长模板共享关键词是正常现象)
                let max_len = fp_len.max(*len).max(1);
                if fp_len.abs_diff(*len) * 10 > max_len * 3 {
                    continue;
                }
                if hamming(fp, *h) <= SIM_THRESHOLD {
                    // 雷同:拉黑该域名
                    drop(fingerprints);
                    self.add(domain, "similar");
                    return false;
                }
            }
        }
        // 精确重复(内容哈希相同)
        let fh = fnv1a64(&fp_text);
        let fps2 = self.fingerprints.lock().unwrap();
        for (_, (_, f, _)) in fps2.iter() {
            if *f == fh {
                drop(fps2);
                self.add(domain, "duplicate");
                return false;
            }
        }
        drop(fps2);
        // 记录指纹
        self.fingerprints.lock().unwrap().insert(domain.to_string(), (fp, fh, fp_len));
        true
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 指纹文本:去掉域名与分隔符,模板站指纹一致、原创站区分明显
fn fingerprint_text(domain: &str, text: &str) -> String {
    let mut s = text.replace(domain, "");
    s = s.replace(" - ", " ").replace('-', " ");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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
        assert!(hamming(a, b) <= SIM_THRESHOLD, "雷同内容距离应小: {}", hamming(a, b));
        assert!(hamming(a, c) > SIM_THRESHOLD, "不同内容距离应大: {}", hamming(a, c));
    }

    #[test]
    fn blacklist_auto_blocks_similar() {
        let dir = std::env::temp_dir().join(format!("pilseo_bl_test_{}", now_secs()));
        let bl = Blacklist::load(&dir);
        let text = "智能家居 - a.com 智能家居相关资讯、资源与最新动态,尽在本站,持续更新优质内容,欢迎访问,更多精彩等你发现。";
        assert!(bl.check_before_index("a.com", text), "第一个域名应允许索引");
        // 模板站:仅域名不同(title 域名部分),指纹去域名后雷同 -> 自动拉黑
        let similar = "智能家居 - b.com 智能家居相关资讯、资源与最新动态,尽在本站,持续更新优质内容,欢迎访问,更多精彩等你发现。";
        assert!(!bl.check_before_index("b.com", similar), "雷同域名应被拉黑");
        assert!(bl.is_blocked("b.com"));
        assert_eq!(bl.list()[0].reason, "similar");
        // 完全不同内容 -> 允许
        assert!(bl.check_before_index("c.com", "宠物用品 猫粮狗粮 宠物美容 宠物医疗 宠物训练 宠物寄养"));
        // 手动解除
        assert!(bl.remove("b.com"));
        assert!(!bl.is_blocked("b.com"));
        let _ = fs::remove_dir_all(&dir);
    }
}
