//! 码道账号管理边界：稳定 ID、旧 identity 别名、凭据版本与文件事务。
//!
//! `Account::identity()` 仍是旧输入/展示值；运行态统一使用这里的 `AccountId`。
//! 缺 domainId 的旧账号首次加载时分配并持久化随机 ID，后续 AK 轮换只追加别名，
//! 不按显示名猜测合并。升级前已删除且只剩旧 removed 字符串的记录只做精确匹配。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::account_lock::{acquire_config_lock, LockError};
use crate::config::{Account, Config};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountSource {
    Local,
    File,
}

#[derive(Clone, Debug)]
pub struct AccountSnapshot {
    pub id: AccountId,
    pub credentials: Account,
    pub source: AccountSource,
    pub revision: u64,
}

pub struct AccountManager {
    path: PathBuf,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "ca_{:x}_{:x}_{:x}",
        nanos,
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn data_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn normalize_aliases(aliases: &mut Vec<String>) {
    aliases.retain(|s| !s.trim().is_empty());
    aliases.sort();
    aliases.dedup();
}

fn assign_ids(cfg: &mut Config) -> bool {
    let mut changed = false;
    let mut used = std::collections::BTreeSet::new();
    for account in &mut cfg.accounts {
        let mut id = account.account_id.clone();
        if id.as_deref().map(str::trim).map_or(true, |s| s.is_empty())
            || used.contains(id.as_deref().unwrap_or(""))
        {
            id = Some(new_id());
            changed = true;
        }
        let id = id.unwrap();
        if account.account_id.as_deref() != Some(id.as_str()) {
            account.account_id = Some(id.clone());
            changed = true;
        }
        used.insert(id);
        let alias = account.identity();
        if !account.identity_aliases.contains(&alias) {
            account.identity_aliases.push(alias);
            changed = true;
        }
        normalize_aliases(&mut account.identity_aliases);
    }
    changed
}

impl AccountManager {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 取得短期配置锁，重新读盘并迁移稳定 ID；只有持久化成功才返回。
    pub fn load(&self) -> Result<Config, String> {
        let _guard = acquire_config_lock(&data_dir(&self.path)).map_err(|e: LockError| e.to_string())?;
        let mut cfg = Config::load(&self.path)?;
        if assign_ids(&mut cfg) {
            cfg.save()?;
        }
        Ok(cfg)
    }

    pub fn resolve_id(&self, legacy_or_current: &str) -> Option<AccountId> {
        Self::resolve_in(&self.load().ok()?, legacy_or_current)
    }

    pub fn resolve_in(cfg: &Config, legacy_or_current: &str) -> Option<AccountId> {
        let raw = legacy_or_current.trim();
        let stable = raw.strip_prefix("id:").unwrap_or(raw);
        for account in &cfg.accounts {
            if account.account_id.as_deref() == Some(stable)
                || account.identity_aliases.iter().any(|a| a == raw)
                || account.identity() == raw
            {
                return account.account_id.clone().map(AccountId);
            }
        }
        for removed in &cfg.removed {
            if let Some(rest) = removed.strip_prefix("id:") {
                if rest == stable {
                    return Some(AccountId(stable.to_string()));
                }
            }
            if let Some(rest) = removed.strip_prefix("alias-map:") {
                if let Some((id, alias)) = rest.split_once(':') {
                    if alias == raw {
                        return Some(AccountId(id.to_string()));
                    }
                }
            }
        }
        None
    }

    pub fn snapshot(&self, id: &AccountId) -> Option<AccountSnapshot> {
        Self::snapshot_in(&self.load().ok()?, id)
    }

    pub fn snapshot_in(cfg: &Config, id: &AccountId) -> Option<AccountSnapshot> {
        let account = cfg
            .accounts
            .iter()
            .find(|a| a.account_id.as_deref() == Some(id.as_str()))?;
        if Self::account_removed_in(cfg, account) {
            return None;
        }
        Some(AccountSnapshot {
            id: id.clone(),
            credentials: account.clone(),
            source: AccountSource::File,
            revision: account.revision,
        })
    }

    pub fn legacy_removed_in(cfg: &Config, legacy: &str) -> bool {
        cfg.removed.iter().any(|x| x == legacy)
    }

    pub fn account_removed_in(cfg: &Config, account: &Account) -> bool {
        let stable = account.stable_key();
        cfg.removed.iter().any(|x| {
            x == &format!("id:{stable}")
                || account.identity_aliases.iter().any(|a| x == a)
                || x == &account.identity()
        })
    }

    pub fn upsert(&self, mut account: Account) -> Result<AccountSnapshot, String> {
        let _guard = acquire_config_lock(&data_dir(&self.path)).map_err(|e: LockError| e.to_string())?;
        let mut cfg = Config::load(&self.path)?;
        assign_ids(&mut cfg);
        let incoming_identity = account.identity();
        normalize_aliases(&mut account.identity_aliases);
        if !account.identity_aliases.contains(&incoming_identity) {
            account.identity_aliases.push(incoming_identity.clone());
        }
        let matching = cfg.accounts.iter().position(|existing| {
            existing.account_id.is_some()
                && account.account_id.is_some()
                && existing.account_id == account.account_id
                || existing
                    .identity_aliases
                    .iter()
                    .any(|a| account.identity_aliases.contains(a))
                || existing.identity() == incoming_identity
                || (account.domain_id.as_deref().is_some_and(|d| !d.trim().is_empty())
                    && existing.domain_id == account.domain_id)
        });
        let snapshot = if let Some(index) = matching {
            let old = cfg.accounts[index].clone();
            let id = old.account_id.clone().unwrap_or_else(new_id);
            account.account_id = Some(id.clone());
            let mut aliases = old.identity_aliases.clone();
            aliases.push(old.identity());
            aliases.extend(account.identity_aliases.clone());
            normalize_aliases(&mut aliases);
            account.identity_aliases = aliases;
            account.revision = old.revision.saturating_add(1).max(account.revision);
            cfg.accounts[index] = account.clone();
            Self::snapshot_in(&cfg, &AccountId(id)).expect("刚写入的账号必须可解析")
        } else {
            let id = account.account_id.clone().unwrap_or_else(new_id);
            account.account_id = Some(id.clone());
            account.revision = account.revision.max(1);
            cfg.accounts.push(account.clone());
            Self::snapshot_in(&cfg, &AccountId(id)).expect("刚写入的账号必须可解析")
        };
        cfg.removed.retain(|x| {
            x != &format!("id:{}", snapshot.id.as_str())
                && !snapshot.credentials.identity_aliases.contains(x)
                && x != &snapshot.credentials.identity()
        });
        cfg.save()?;
        Ok(snapshot)
    }

    pub fn replace_credentials(&self, id: &AccountId, mut replacement: Account) -> Result<AccountSnapshot, String> {
        let _guard = acquire_config_lock(&data_dir(&self.path)).map_err(|e: LockError| e.to_string())?;
        let mut cfg = Config::load(&self.path)?;
        assign_ids(&mut cfg);
        let index = cfg
            .accounts
            .iter()
            .position(|a| a.account_id.as_deref() == Some(id.as_str()))
            .ok_or_else(|| format!("找不到稳定账号 {}", id.as_str()))?;
        let old = cfg.accounts[index].clone();
        replacement.account_id = Some(id.as_str().to_string());
        replacement.enabled = old.enabled;
        let mut aliases = old.identity_aliases.clone();
        aliases.push(old.identity());
        aliases.push(replacement.identity());
        aliases.extend(replacement.identity_aliases.clone());
        normalize_aliases(&mut aliases);
        replacement.identity_aliases = aliases;
        replacement.revision = old.revision.saturating_add(1).max(replacement.revision);
        cfg.accounts[index] = replacement;
        cfg.save()?;
        Self::snapshot_in(&cfg, id).ok_or_else(|| "凭据替换后账号快照缺失".to_string())
    }

    pub fn set_enabled(&self, id: &AccountId, enabled: bool) -> Result<(), String> {
        let _guard = acquire_config_lock(&data_dir(&self.path)).map_err(|e: LockError| e.to_string())?;
        let mut cfg = Config::load(&self.path)?;
        assign_ids(&mut cfg);
        let account = cfg
            .accounts
            .iter_mut()
            .find(|a| a.account_id.as_deref() == Some(id.as_str()))
            .ok_or_else(|| format!("找不到稳定账号 {}", id.as_str()))?;
        account.enabled = enabled;
        cfg.save()
    }

    pub fn remove(&self, id: &AccountId) -> Result<(), String> {
        let _guard = acquire_config_lock(&data_dir(&self.path)).map_err(|e: LockError| e.to_string())?;
        let mut cfg = Config::load(&self.path)?;
        assign_ids(&mut cfg);
        let account = cfg
            .accounts
            .iter()
            .find(|a| a.account_id.as_deref() == Some(id.as_str()))
            .cloned()
            .ok_or_else(|| format!("找不到稳定账号 {}", id.as_str()))?;
        let stable = format!("id:{}", id.as_str());
        if !cfg.removed.contains(&stable) {
            cfg.removed.push(stable);
        }
        let mut aliases = account.identity_aliases.clone();
        aliases.push(account.identity());
        normalize_aliases(&mut aliases);
        for alias in aliases {
            if !cfg.removed.contains(&alias) {
                cfg.removed.push(alias.clone());
            }
            let mapping = format!("alias-map:{}:{alias}", id.as_str());
            if !cfg.removed.contains(&mapping) {
                cfg.removed.push(mapping);
            }
        }
        cfg.accounts.retain(|a| a.account_id.as_deref() != Some(id.as_str()));
        cfg.save()
    }
}
