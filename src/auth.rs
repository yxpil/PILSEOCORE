//! 认证模块:API/MCP 令牌签发与校验 + Web 登录会话
//!
//! 模型:
//!   - 管理员用"账号 + 密码"登录 Web UI(见 server.rs /api/auth/login)
//!   - 登录后管理员可"签发"API Token(命名,可撤销),存于 data/tokens.json
//!   - API 客户端 / MCP 使用签发 token(Authorization: Bearer <token>)访问管理能力

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::json::Json;

#[derive(Clone, Debug)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub token: String,
    pub created: u64,
    pub last_used: u64,
}

/// 签发 token 存储(data/tokens.json,本地信任模型明文存储)
pub struct TokenStore {
    path: PathBuf,
    tokens: Mutex<Vec<ApiToken>>,
}

impl TokenStore {
    pub fn load(data_dir: &Path) -> TokenStore {
        let path = data_dir.join("tokens.json");
        let tokens = fs::read_to_string(&path)
            .ok()
            .and_then(|t| crate::json::parse(&t).ok())
            .and_then(|j| parse_tokens(&j))
            .unwrap_or_default();
        TokenStore {
            path,
            tokens: Mutex::new(tokens),
        }
    }

    fn save(&self) {
        let arr: Vec<Json> = self
            .tokens
            .lock()
            .unwrap()
            .iter()
            .map(|t| {
                Json::build(vec![
                    ("id", Json::str(&t.id)),
                    ("name", Json::str(&t.name)),
                    ("token", Json::str(&t.token)),
                    ("created", Json::num(t.created as f64)),
                    ("last_used", Json::num(t.last_used as f64)),
                ])
            })
            .collect();
        let _ = fs::create_dir_all(self.path.parent().unwrap_or(Path::new(".")));
        let _ = fs::write(&self.path, Json::arr(arr).to_string());
    }

    /// 签发新 token,返回完整 token 值(仅此一次可见)
    pub fn create(&self, name: &str) -> String {
        let name = if name.trim().is_empty() { "api".to_string() } else { name.trim().to_string() };
        let token = generate_token();
        let id = format!("{:x}", now_secs()) + &token[..6];
        let t = ApiToken {
            id,
            name,
            token: token.clone(),
            created: now_secs(),
            last_used: now_secs(),
        };
        self.tokens.lock().unwrap().push(t);
        self.save();
        token
    }

    /// 校验 token 是否有效(有效则更新 last_used)
    pub fn verify(&self, token: &str) -> bool {
        let mut tokens = self.tokens.lock().unwrap();
        if let Some(t) = tokens.iter_mut().find(|t| t.token == token) {
            t.last_used = now_secs();
            drop(tokens);
            self.save();
            true
        } else {
            false
        }
    }

    /// 撤销 token(按 id)
    pub fn revoke(&self, id: &str) -> bool {
        let mut tokens = self.tokens.lock().unwrap();
        let before = tokens.len();
        tokens.retain(|t| t.id != id);
        let removed = tokens.len() != before;
        drop(tokens);
        if removed {
            self.save();
        }
        removed
    }

    pub fn list(&self) -> Vec<ApiToken> {
        self.tokens.lock().unwrap().clone()
    }
}

fn parse_tokens(j: &Json) -> Option<Vec<ApiToken>> {
    let arr = j.as_arr()?;
    Some(
        arr.iter()
            .filter_map(|item| {
                Some(ApiToken {
                    id: item.get("id")?.as_str()?.to_string(),
                    name: item.get("name")?.as_str()?.to_string(),
                    token: item.get("token")?.as_str()?.to_string(),
                    created: item.get("created")?.as_u64()?,
                    last_used: item.get("last_used")?.as_u64()?,
                })
            })
            .collect(),
    )
}

/// 登录会话:token -> 过期时间戳
pub struct Sessions {
    map: Mutex<HashMap<String, u64>>,
    ttl_secs: u64,
}

impl Sessions {
    pub fn new(ttl_secs: u64) -> Sessions {
        Sessions {
            map: Mutex::new(HashMap::new()),
            ttl_secs,
        }
    }

    /// 创建会话,返回 token
    pub fn create(&self) -> String {
        let token = generate_token();
        let exp = now_secs() + self.ttl_secs;
        self.map.lock().unwrap().insert(token.clone(), exp);
        token
    }

    pub fn verify(&self, token: &str) -> bool {
        let mut map = self.map.lock().unwrap();
        match map.get(token) {
            Some(&exp) if exp > now_secs() => true,
            _ => {
                map.remove(token);
                false
            }
        }
    }

    pub fn remove(&self, token: &str) {
        self.map.lock().unwrap().remove(token);
    }
}

/// 生成随机 token:时间 + RandomState(OS 随机种子)混合,hex 编码
fn generate_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let rnd = RandomState::new();
    let mut h1 = rnd.build_hasher();
    h1.write_u64(now_secs());
    h1.write_u64(std::process::id() as u64);
    h1.write_u64(randish());
    let a = h1.finish();
    let mut h2 = rnd.build_hasher();
    h2.write_u64(a);
    h2.write_u64(std::time::SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0));
    let b = h2.finish();
    format!("{:016x}{:016x}", a, b)
}

/// 无锁递增 + 时间混合的辅助随机
fn randish() -> u64 {
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let c = CTR.fetch_add(0x9E37_79B9_7F4A_7C15, std::sync::atomic::Ordering::Relaxed);
    c ^ now_secs().rotate_left(17)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_create_verify_revoke() {
        let dir = std::env::temp_dir().join(format!("pilseo_auth_test_{}", now_secs()));
        let store = TokenStore::load(&dir);
        let t = store.create("test-token");
        assert_eq!(t.len(), 32);
        assert!(store.verify(&t));
        assert!(!store.verify("wrong"));
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-token");
        assert!(store.revoke(&list[0].id));
        assert!(!store.verify(&t));
        let _ = fs::remove_dir_all(&dir);
    }
}
