//! 续期结果落盘：`codearts-refreshed.json`（数据目录，gitignore）。
//!
//! **为什么不写回码道自己的存储**：Electron 的 `secretStorage/*.enc` 由主进程持有，
//! 外部改写容易被覆盖或让桌面端状态错乱；本项目只维护自己的 overlay，按账号身份
//! （`domainId|显示名`）取「更晚过期」的那份。桌面端下次自己续期失败时，用户重开桌面端
//! 仍能拿到最新登录态，不会把两个进程的凭据链搅在一起。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{now_unix, Account};
use crate::refresh::Refreshed;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Entry {
    #[serde(default)]
    pub access_key: String,
    #[serde(default)]
    pub secret_key: String,
    #[serde(default)]
    pub security_token: String,
    #[serde(default)]
    pub expires_at: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Store {
    #[serde(default)]
    pub entries: BTreeMap<String, Entry>,
}

impl Store {
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("落盘 {} 失败: {e}", path.display()))
    }

    /// 取比账号当前凭据更晚过期的 overlay；没有就返回 None。
    pub fn overlay(&self, account: &Account) -> Option<Account> {
        let e = self.entries.get(&account.identity())?;
        if e.access_key.is_empty() || e.secret_key.is_empty() || e.security_token.is_empty() {
            return None;
        }
        let mut out = account.clone();
        let old = out.expires_unix();
        out.access_key = e.access_key.clone();
        out.secret_key = e.secret_key.clone();
        out.security_token = e.security_token.clone();
        if !e.expires_at.is_empty() {
            out.expires_at = Some(e.expires_at.clone());
        }
        if !e.refresh_token.is_empty() {
            out.refresh_token = Some(e.refresh_token.clone());
        }
        let new = out.expires_unix();
        match (old, new) {
            (Some(o), Some(n)) if n <= o => None,
            _ => Some(out),
        }
    }

    pub fn put(&mut self, account: &Account, r: &Refreshed) {
        self.entries.insert(
            account.identity(),
            Entry {
                access_key: r.access_key.clone(),
                secret_key: r.secret_key.clone(),
                security_token: r.security_token.clone(),
                expires_at: r.expires_at.clone(),
                refresh_token: r.refresh_token.clone(),
                updated_at: now_unix(),
            },
        );
    }
}
