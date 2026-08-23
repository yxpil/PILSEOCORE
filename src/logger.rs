//! 内存环形日志:面板实时显示爬虫/索引/服务详细日志(透明化)
//!
//! - 全局单例(OnceLock),任意模块 push 即可
//! - 环形缓冲:最多保留 N 条,避免内存膨胀
//! - 带自增序号:面板按 after 增量拉取新日志
//! - 零落盘(内存优先),日志不写文件

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

/// 日志上限(条)
const MAX_LOGS: usize = 500;

pub struct LogRing {
    logs: Mutex<VecDeque<(usize, String)>>,
    seq: Mutex<usize>,
    max: usize,
}

static GLOBAL: OnceLock<Arc<LogRing>> = OnceLock::new();

/// 初始化全局日志(serve 启动时调用一次)
pub fn init() -> Arc<LogRing> {
    let ring = Arc::new(LogRing::new(MAX_LOGS));
    let _ = GLOBAL.set(ring.clone());
    ring
}

/// 取全局日志环(未初始化时自动创建)
pub fn global() -> Arc<LogRing> {
    GLOBAL.get_or_init(|| Arc::new(LogRing::new(MAX_LOGS))).clone()
}

/// 写一条日志(带时间戳)
pub fn push(line: impl AsRef<str>) {
    let ts = now_ts();
    global().push(format!("[{}] {}", ts, line.as_ref()));
}

impl LogRing {
    pub fn new(max: usize) -> LogRing {
        LogRing {
            logs: Mutex::new(VecDeque::with_capacity(max.min(100))),
            seq: Mutex::new(0),
            max: max.max(10),
        }
    }

    pub fn push(&self, line: String) {
        let mut seq = self.seq.lock().unwrap();
        *seq += 1;
        let n = *seq;
        drop(seq);
        let mut logs = self.logs.lock().unwrap();
        if logs.len() >= self.max {
            logs.pop_front();
        }
        logs.push_back((n, line));
    }

    /// 拉取序号 > after 的新日志,返回 (最新序号, 新增日志)
    pub fn since(&self, after: usize) -> (usize, Vec<String>) {
        let logs = self.logs.lock().unwrap();
        let newest = logs.back().map(|(n, _)| *n).unwrap_or(after);
        let lines: Vec<String> = logs
            .iter()
            .filter(|(n, _)| *n > after)
            .map(|(_, l)| l.clone())
            .collect();
        (newest, lines)
    }

    /// 最近 N 条
    pub fn last_n(&self, n: usize) -> Vec<String> {
        let logs = self.logs.lock().unwrap();
        logs.iter().rev().take(n.max(1)).rev().map(|(_, l)| l.clone()).collect()
    }
}

/// 时间戳 HH:MM:SS
fn now_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffers_and_since() {
        let ring = LogRing::new(10);
        for i in 0..15 {
            ring.push(format!("line {}", i));
        }
        // 环形:只保留最后 10 条
        let recent = ring.last_n(20);
        assert_eq!(recent.len(), 10);
        assert!(recent[0].ends_with("line 5"));
        assert!(recent[9].ends_with("line 14"));
        // since 增量
        let (newest, lines) = ring.since(13);
        assert_eq!(newest, 15);
        assert_eq!(lines.len(), 2); // seq 14, 15 = line 13, 14
        assert!(lines[0].ends_with("line 13"));
        assert!(lines[1].ends_with("line 14"));
    }
}
