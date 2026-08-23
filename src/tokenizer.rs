//! BPE(Byte Pair Encoding)分词器:字节级标准 BPE,零第三方依赖
//!
//! - 训练:从语料统计字节对频率,迭代合并最高频对,词表可达 81920 tokens
//! - 编码:贪心按合并优先级(rank)合并,O(1) 词表查找,避免全表遍历
//! - 持久化:vocab.json(vocab 数组 + 有序 merges),加载后直接使用
//!
//! 用途:替代简单的字符/bigram 分词,词表驱动,索引与查询共享同一词表,
//! 分词后词表查找 O(1),查询速度显著提升。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// 目标词表大小(用户指定)
pub const VOCAB_SIZE: usize = 81920;
/// 保留 id 0 为未知
pub const UNK: u32 = 0;

pub struct BpeTokenizer {
    /// token 原始字节(0=UNK,1..=256=单字节)
    vocab: Vec<Vec<u8>>,
    token_of: HashMap<Vec<u8>, u32>,
    merges_by_rank: Vec<(u32, u32, u32)>,   // rank -> (left, right, merged)
    merge_rank: HashMap<(u32, u32), usize>, // (left, right) -> rank
    unk_id: u32,
}

impl BpeTokenizer {
    /// 从语料训练词表
    pub fn train(corpus: &[String], vocab_size: usize) -> BpeTokenizer {
        let target = vocab_size.max(300).min(1_000_000);
        // 初始词表:UNK + 256 个单字节(id = 字节值 + 1,vocab[1]=[0x00] ... vocab[256]=[0xFF])
        let mut vocab: Vec<Vec<u8>> = Vec::with_capacity(300);
        vocab.push(b"<UNK>".to_vec());
        for b in 0..=255u16 {
            vocab.push(vec![b as u8]);
        }
        let mut token_of: HashMap<Vec<u8>, u32> = HashMap::with_capacity(300);
        for (i, v) in vocab.iter().enumerate() {
            token_of.insert(v.clone(), i as u32);
        }

        // 语料 -> 按空格切词,每词一个字节 id 序列(词边界保护:
        // 只在词内合并,避免跨词/跨上下文合并导致查询与索引分词不一致)
        let mut seqs: Vec<Vec<u32>> = Vec::new();
        let mut seen: HashSet<Vec<u32>> = HashSet::new();
        for text in corpus {
            for word in text.split_whitespace() {
                let seq: Vec<u32> = word.as_bytes().iter().map(|&b| b as u32 + 1).collect();
                if seq.len() >= 2 && seq.len() <= 64 && seen.insert(seq.clone()) {
                    seqs.push(seq);
                }
                if seqs.len() >= 50_000 {
                    break;
                }
            }
            if seqs.len() >= 50_000 {
                break;
            }
        }

        // pair 频率
        let mut pair_counts: HashMap<(u32, u32), u64> = HashMap::new();
        for seq in &seqs {
            for w in seq.windows(2) {
                *pair_counts.entry((w[0], w[1])).or_insert(0) += 1;
            }
        }

        // 迭代合并直到词表达标或无可合并
        let mut merges_by_rank: Vec<(u32, u32, u32)> = Vec::new();
        let mut merge_rank: HashMap<(u32, u32), usize> = HashMap::new();
        let mut last_progress = 0usize;
        while vocab.len() < target {
            let Some(&(l, r)) = pair_counts.iter().max_by_key(|(_, &c)| c).map(|(k, _)| k) else {
                break;
            };
            let cnt = pair_counts[&(l, r)];
            if cnt < 2 {
                break; // 所有 pair 仅出现一次,继续合并无意义
            }
            let merged_id = vocab.len() as u32;
            let mut merged_bytes = vocab[l as usize].clone();
            merged_bytes.extend_from_slice(&vocab[r as usize]);
            vocab.push(merged_bytes);
            token_of.insert(vocab.last().unwrap().clone(), merged_id);
            merge_rank.insert((l, r), merges_by_rank.len());
            merges_by_rank.push((l, r, merged_id));
            pair_counts.remove(&(l, r));

            // 更新所有序列
            for seq in seqs.iter_mut() {
                let mut i = 0;
                while i + 1 < seq.len() {
                    if seq[i] == l && seq[i + 1] == r {
                        if i > 0 {
                            dec_pair(&mut pair_counts, (seq[i - 1], seq[i]));
                        }
                        if i + 2 < seq.len() {
                            dec_pair(&mut pair_counts, (seq[i + 1], seq[i + 2]));
                        }
                        seq[i] = merged_id;
                        seq.remove(i + 1);
                        if i > 0 {
                            inc_pair(&mut pair_counts, (seq[i - 1], seq[i]));
                        }
                        if i + 1 < seq.len() {
                            inc_pair(&mut pair_counts, (seq[i], seq[i + 1]));
                        }
                    }
                    i += 1;
                }
            }

            // 进度(每 10%)
            let pct = (vocab.len() * 100) / target;
            if pct / 10 > last_progress {
                last_progress = pct / 10;
                eprintln!("[tokenizer] 训练中: 词表 {}/{} ({:>3}%)", vocab.len(), target, pct);
            }
            if seqs.is_empty() {
                break;
            }
        }

        BpeTokenizer {
            vocab,
            token_of,
            merges_by_rank,
            merge_rank,
            unk_id: UNK,
        }
    }

    /// 编码:文本 -> token id 序列(按空格切词,词内贪心按 rank 合并;
    /// 与训练一致,保证查询与索引分词稳定一致)
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for word in text.split_whitespace() {
            let mut seq: Vec<u32> = word.as_bytes().iter().map(|&b| b as u32 + 1).collect();
            if seq.len() >= 2 {
                loop {
                    let mut best_pos: Option<usize> = None;
                    let mut best_rank = usize::MAX;
                    for i in 0..seq.len() - 1 {
                        if let Some(&rank) = self.merge_rank.get(&(seq[i], seq[i + 1])) {
                            if rank < best_rank {
                                best_rank = rank;
                                best_pos = Some(i);
                            }
                        }
                    }
                    match best_pos {
                        Some(i) => {
                            let merged = self.merges_by_rank[best_rank].2;
                            seq[i] = merged;
                            seq.remove(i + 1);
                        }
                        None => break,
                    }
                }
            }
            out.extend(seq);
        }
        out
    }

    /// 编码为 token 字符串列表(索引/查询词表;lossy 显示)
    pub fn tokenize_str(&self, text: &str) -> Vec<String> {
        self.encode(text).into_iter().map(|id| self.token_str(id)).collect()
    }

    /// id -> token 字符串(lossy)
    pub fn token_str(&self, id: u32) -> String {
        self.token_bytes(id).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|| "<UNK>".to_string())
    }

    /// id -> 原始字节
    pub fn token_bytes(&self, id: u32) -> Option<Vec<u8>> {
        self.vocab.get(id as usize).cloned()
    }

    /// 解码:token id 序列 -> 文本(拼接原始字节)
    pub fn decode(&self, ids: &[u32]) -> String {
        let bytes: Vec<u8> = ids
            .iter()
            .filter(|&&id| id != self.unk_id)
            .flat_map(|&id| self.vocab.get(id as usize).cloned().unwrap_or_default())
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn vocab_size(&self) -> usize {
        self.vocab.len()
    }

    /// 保存词表到 JSON(vocab 为字节数组,无损)
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let vocab_json: Vec<crate::json::Json> = self
            .vocab
            .iter()
            .map(|b| crate::json::Json::arr(b.iter().map(|&x| crate::json::Json::num(x as f64)).collect()))
            .collect();
        let merges_json: Vec<crate::json::Json> = self
            .merges_by_rank
            .iter()
            .map(|&(l, r, m)| {
                crate::json::Json::arr(vec![
                    crate::json::Json::num(l as f64),
                    crate::json::Json::num(r as f64),
                    crate::json::Json::num(m as f64),
                ])
            })
            .collect();
        let j = crate::json::Json::build(vec![
            ("vocab", crate::json::Json::arr(vocab_json)),
            ("merges", crate::json::Json::arr(merges_json)),
            ("unk", crate::json::Json::num(UNK as f64)),
        ]);
        fs::write(path, j.to_string()).map_err(|e| format!("写入词表 {} 失败: {}", path.display(), e))
    }

    /// 加载词表
    pub fn load(path: &Path) -> Option<BpeTokenizer> {
        let text = fs::read_to_string(path).ok()?;
        let j = crate::json::parse(&text).ok()?;
        let vocab = j.get("vocab")?.as_arr()?;
        let mut vocab_vec: Vec<Vec<u8>> = Vec::with_capacity(vocab.len());
        let mut token_of: HashMap<Vec<u8>, u32> = HashMap::with_capacity(vocab.len());
        for (i, v) in vocab.iter().enumerate() {
            let bytes: Vec<u8> = v
                .as_arr()
                .map(|a| a.iter().filter_map(|x| x.as_u64()).map(|x| x as u8).collect())
                .unwrap_or_default();
            token_of.insert(bytes.clone(), i as u32);
            vocab_vec.push(bytes);
        }
        let mut merges_by_rank: Vec<(u32, u32, u32)> = Vec::new();
        let mut merge_rank: HashMap<(u32, u32), usize> = HashMap::new();
        if let Some(ms) = j.get("merges").and_then(|m| m.as_arr()) {
            for m in ms {
                if let Some(arr) = m.as_arr() {
                    if arr.len() >= 3 {
                        let l = arr[0].as_u64().unwrap_or(0) as u32;
                        let r = arr[1].as_u64().unwrap_or(0) as u32;
                        let mid = arr[2].as_u64().unwrap_or(0) as u32;
                        merge_rank.insert((l, r), merges_by_rank.len());
                        merges_by_rank.push((l, r, mid));
                    }
                }
            }
        }
        Some(BpeTokenizer {
            vocab: vocab_vec,
            token_of,
            merges_by_rank,
            merge_rank,
            unk_id: UNK,
        })
    }

    /// 直接查 token id(O(1) 词表查找;按字符串 lossy 匹配)
    pub fn id_of(&self, token: &str) -> Option<u32> {
        self.token_of.get(token.as_bytes()).copied()
    }
}

fn inc_pair(map: &mut HashMap<(u32, u32), u64>, k: (u32, u32)) {
    *map.entry(k).or_insert(0) += 1;
}

fn dec_pair(map: &mut HashMap<(u32, u32), u64>, k: (u32, u32)) {
    if let Some(v) = map.get_mut(&k) {
        *v = v.saturating_sub(1);
        if *v == 0 {
            map.remove(&k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn train_and_encode_roundtrip() {
        let corpus: Vec<String> = vec![
            "智能家居 物联网 人工智能 大数据 云计算".into(),
            "智能家居 智能门锁 智能灯光 智能窗帘".into(),
            "人工智能 机器学习 深度学习 神经网络".into(),
            "数字营销 网络科技 区块链 云计算".into(),
        ];
        let tok = BpeTokenizer::train(&corpus, 300);
        assert!(tok.vocab_size() >= 260, "词表应包含 256 字节 + 合并 token,实际 {}", tok.vocab_size());
        let ids = tok.encode("智能家居");
        let decoded = tok.decode(&ids);
        assert!(decoded.contains("智能家居"), "解码应还原文本: {:?}", decoded);
        // 高频词应合并成单个 token
        let single = tok.encode("智能家居");
        assert!(single.len() < "智能家居".as_bytes().len(), "高频词应被合并压缩");
    }

    #[test]
    fn save_load_roundtrip() {
        let corpus: Vec<String> = vec!["abcabcabc 测试 测试 测试".into(), "abc 测试测试".into()];
        let tok = BpeTokenizer::train(&corpus, 300);
        let path = std::env::temp_dir().join("pilseo_vocab_test.json");
        tok.save(&path).unwrap();
        let loaded = BpeTokenizer::load(&path).unwrap();
        assert_eq!(loaded.vocab_size(), tok.vocab_size());
        assert_eq!(loaded.encode("测试"), tok.encode("测试"));
        let _ = fs::remove_file(&path);
    }
}
