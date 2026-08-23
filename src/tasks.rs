//! 定时任务调度器:定时执行穷举扫描 / 重建索引 / 爬虫抓取
//!
//! 任务配置持久化在 data/index/tasks.json:
//!   {"tasks": [{"id": "t1", "name": "每日重建", "kind": "rebuild",
//!               "interval_secs": 3600, "params": {"max_len": 2, "workers": 64},
//!               "enabled": true, "last_run": 0}]}
//!
//! 调度线程(serve 启动时 spawn)每 30 秒 tick 一次,到期任务在独立线程执行。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::json::Json;

#[derive(Clone, Debug)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub kind: String, // scan / rebuild / crawl
    pub interval_secs: u64,
    pub params: Json,
    pub enabled: bool,
    pub last_run: u64,
}

impl Task {
    pub fn new(id: &str, name: &str, kind: &str, interval_secs: u64) -> Task {
        Task {
            id: id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            interval_secs: interval_secs.max(30),
            params: Json::obj(),
            enabled: true,
            last_run: 0,
        }
    }
}

pub struct TaskScheduler {
    path: PathBuf,
    tasks: Mutex<Vec<Task>>,
}

impl TaskScheduler {
    pub fn load(data_dir: &Path) -> TaskScheduler {
        let path = data_dir.join("tasks.json");
        let mut tasks = Vec::new();
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(j) = crate::json::parse(&text) {
                if let Some(arr) = j.get("tasks").and_then(|v| v.as_arr()) {
                    for item in arr {
                        tasks.push(Task {
                            id: item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            name: item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            kind: item.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            interval_secs: item.get("interval_secs").and_then(|v| v.as_u64()).unwrap_or(3600).max(30),
                            params: item.get("params").cloned().unwrap_or_else(Json::obj),
                            enabled: item.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
                            last_run: item.get("last_run").and_then(|v| v.as_u64()).unwrap_or(0),
                        });
                    }
                }
            }
        }
        TaskScheduler {
            path,
            tasks: Mutex::new(tasks),
        }
    }

    fn save(&self) {
        let tasks: Vec<Json> = self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .map(|t| {
                Json::build(vec![
                    ("id", Json::str(&t.id)),
                    ("name", Json::str(&t.name)),
                    ("kind", Json::str(&t.kind)),
                    ("interval_secs", Json::num(t.interval_secs as f64)),
                    ("params", t.params.clone()),
                    ("enabled", Json::Bool(t.enabled)),
                    ("last_run", Json::num(t.last_run as f64)),
                ])
            })
            .collect();
        let j = Json::build(vec![("tasks", Json::arr(tasks))]);
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&self.path, j.to_string());
    }

    pub fn list(&self) -> Vec<Task> {
        self.tasks.lock().unwrap().clone()
    }

    pub fn add(&self, task: Task) {
        self.tasks.lock().unwrap().push(task);
        self.save();
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut tasks = self.tasks.lock().unwrap();
        let before = tasks.len();
        tasks.retain(|t| t.id != id);
        let removed = tasks.len() != before;
        drop(tasks);
        if removed {
            self.save();
        }
        removed
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        let mut tasks = self.tasks.lock().unwrap();
        let mut found = false;
        for t in tasks.iter_mut() {
            if t.id == id {
                t.enabled = enabled;
                found = true;
                break;
            }
        }
        drop(tasks);
        if found {
            self.save();
        }
        found
    }

    pub fn mark_run(&self, id: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut tasks = self.tasks.lock().unwrap();
        for t in tasks.iter_mut() {
            if t.id == id {
                t.last_run = now;
                break;
            }
        }
        drop(tasks);
        self.save();
    }

    /// tick:返回本次到期的任务列表(已标记运行)
    pub fn due_tasks(&self) -> Vec<Task> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let tasks = self.tasks.lock().unwrap();
        tasks
            .iter()
            .filter(|t| t.enabled && now.saturating_sub(t.last_run) >= t.interval_secs)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_add_remove_enable() {
        let dir = std::env::temp_dir().join(format!("pilseo_tasks_test_{}", now_secs()));
        let sched = TaskScheduler::load(&dir);
        let t = Task::new("t1", "测试任务", "rebuild", 60);
        sched.add(t);
        assert_eq!(sched.list().len(), 1);
        assert!(sched.set_enabled("t1", false));
        assert!(!sched.list()[0].enabled);
        assert!(sched.remove("t1"));
        assert_eq!(sched.list().len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}
