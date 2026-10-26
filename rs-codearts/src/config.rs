//! 账号池配置：`codearts-accounts.json`（数据目录，已 gitignore）。
//!
//! 兼容三种写法，都是参考实现里出现过的真实形态：
//!
//! 1. 码道桌面端 `state.vscdb` 解出来的 **session JSON**（单账号，含
//!    `accessKey/secretKey/securitytoken/expires_at/refresh_token/loginContext`）；
//! 2. OAuth `TokenResponse`（`credentials.access_key_id/secret_access_key/...`）；
//! 3. 本项目自己的账号池：`{"channel":"credit","accounts":[...]}`。
//!
//! 字段一律 `serde(alias)` 放宽，不做严格 schema —— 这是「导入别人的 session」用的，
//! 苛求字段名只会让用户反复贴不进来。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 默认通道：`credit`（积分）。`benefit`（每日免费 Token）2026-10 已下线，
/// 只在配置显式写 `"channel": "benefit"` 时才发 `maas_type` 头。
pub const DEFAULT_CHANNEL: &str = "credit";

fn default_true() -> bool {
    true
}

/// 单账号。
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Account {
    /// 显示名；没写就用 `account.label` / `account.id` / AK 尾段。
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,

    #[serde(default, alias = "accessKey", alias = "access_key_id", alias = "accessKeyId", alias = "access")]
    pub access_key: String,
    #[serde(default, alias = "secret_access_key", alias = "secretKey", alias = "secret")]
    pub secret_key: String,
    #[serde(default, alias = "security_token", alias = "securityToken", alias = "securitytoken", alias = "token")]
    pub security_token: String,

    /// ISO8601；桌面端写 `expires_at`，OAuth 写 `expiration`。
    #[serde(default, alias = "expiration", alias = "expiresAt")]
    pub expires_at: Option<String>,
    #[serde(default, alias = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(default, alias = "codeVerifier")]
    pub code_verifier: Option<String>,
    /// DPoP 私钥 JWK（续期用；v1 只保存不消费）。
    #[serde(default, alias = "dpopPrivateJwk", alias = "dpop_private_key")]
    pub dpop_private_jwk: Option<Value>,
    #[serde(default, alias = "domainId")]
    pub domain_id: Option<String>,
    /// OAuth client_id（这版码道是 `codearts`；旧 VS Code 版是 `codearts-agent`）。
    #[serde(default, alias = "clientId")]
    pub client_id: Option<String>,

    /// 身份头覆盖（上游换版本时可改配置，不必改代码）。
    #[serde(default)]
    pub plugin_name: Option<String>,
    #[serde(default)]
    pub plugin_version: Option<String>,
    #[serde(default)]
    pub ide_name: Option<String>,
    #[serde(default)]
    pub ide_version: Option<String>,

    /// 桌面端 session 的嵌套账号信息。
    #[serde(default)]
    pub account: Option<SessionAccount>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionAccount {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
}

impl Account {
    /// 有效显示名：name → account.label → account.id → AK 尾 6 位。
    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_string();
        }
        if let Some(a) = &self.account {
            if !a.label.trim().is_empty() {
                return a.label.trim().to_string();
            }
            if !a.id.trim().is_empty() {
                return a.id.trim().to_string();
            }
        }
        let ak = self.access_key.trim();
        if ak.len() > 6 {
            return format!("…{}", &ak[ak.len() - 6..]);
        }
        if ak.is_empty() {
            "未命名账号".to_string()
        } else {
            ak.to_string()
        }
    }

    /// 是否填了可用的三件套。
    pub fn has_credentials(&self) -> bool {
        !self.access_key.trim().is_empty() && !self.secret_key.trim().is_empty()
    }

    /// 过期时间（Unix 秒）。解析不了按 None（视为未知，仍然可用）。
    pub fn expires_unix(&self) -> Option<i64> {
        self.expires_at.as_deref().and_then(parse_rfc3339)
    }

    /// 已过期（带 60 秒余量）。
    pub fn is_expired(&self) -> bool {
        match self.expires_unix() {
            Some(t) => t <= now_unix() + 60,
            None => false,
        }
    }

    /// 账号稳定身份（续期 overlay 的键）：`domainId|显示名`，缺 domain 用 AK 尾段。
    pub fn identity(&self) -> String {
        let domain = self.domain_id.clone().unwrap_or_default();
        let name = self.display_name();
        if domain.is_empty() {
            let ak = self.access_key.trim();
            let tail = if ak.len() > 8 { &ak[ak.len() - 8..] } else { ak };
            format!("ak|{tail}|{name}")
        } else {
            format!("{domain}|{name}")
        }
    }

    /// 是否具备续期三件套：refresh_token + DPoP 私钥 + PKCE verifier。
    pub fn can_refresh(&self) -> bool {
        self.refresh_token.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
            && self.dpop_private_jwk.is_some()
            && self.code_verifier.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
    }
}

/// 账号池。
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub channel: String,
    pub path: PathBuf,
    /// 已删除的账号身份（删了就不再自动归档回来）。
    pub removed: Vec<String>,
}

impl Config {
    /// 从磁盘读（文件不存在 → 空池，不报错：插件仍应能启动并如实 /health）。
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self {
                accounts: Vec::new(),
                channel: DEFAULT_CHANNEL.to_string(),
                removed: Vec::new(),
                path: path.to_path_buf(),
            });
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("读 {} 失败: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("解析 {} 失败: {e}", path.display()))?;
        Self::from_value(v, path)
    }

    /// 支持数组 / `{accounts:[...]}` / 单个 session 对象。
    pub fn from_value(v: Value, path: &Path) -> Result<Self, String> {
        let (accounts, channel, removed) = match v {
            Value::Array(items) => (items, DEFAULT_CHANNEL.to_string(), Vec::new()),
            Value::Object(mut map) => {
                let channel = map
                    .get("channel")
                    .and_then(|c| c.as_str())
                    .unwrap_or(DEFAULT_CHANNEL)
                    .to_string();
                let removed: Vec<String> = map
                    .get("removed")
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(Value::Array(items)) = map.remove("accounts") {
                    (items, channel, removed)
                } else if map.contains_key("accessKey")
                    || map.contains_key("access_key")
                    || map.contains_key("access_key_id")
                    || map.contains_key("credentials")
                {
                    // 单个 session / TokenResponse：兼容 `credentials` 嵌套。
                    let mut acct = map.clone();
                    if let Some(Value::Object(creds)) = map.remove("credentials") {
                        for (k, val) in creds {
                            acct.entry(k).or_insert(val);
                        }
                    }
                    (vec![Value::Object(acct)], channel, removed)
                } else {
                    return Err("配置文件里既没有 accounts 数组，也不像单个 session".to_string());
                }
            }
            _ => return Err("配置文件顶层必须是对象或数组".to_string()),
        };

        let mut out = Vec::with_capacity(accounts.len());
        for item in accounts {
            let acct: Account = serde_json::from_value(item)
                .map_err(|e| format!("账号条目解析失败: {e}"))?;
            if acct.has_credentials() {
                out.push(acct);
            }
        }
        Ok(Self {
            accounts: out,
            channel,
            removed,
            path: path.to_path_buf(),
        })
    }

    /// 选一个账号：启用 + 有三件套 + 未过期。优先过期时间最晚的。
    pub fn pick(&self) -> Option<&Account> {
        let mut candidates: Vec<&Account> = self
            .accounts
            .iter()
            .filter(|a| a.enabled && a.has_credentials())
            .collect();
        // 未过期的优先；全过期时仍返回一个，让上游如实报凭据错误（便于排查）。
        candidates.sort_by_key(|a| a.expires_unix().unwrap_or(i64::MAX));
        if let Some(pos) = candidates.iter().position(|a| !a.is_expired()) {
            return candidates.get(pos).copied();
        }
        candidates.last().copied()
    }

    /// 有效账号数（启用 + 有三件套 + 未过期）。
    pub fn healthy_count(&self) -> usize {
        self.accounts
            .iter()
            .filter(|a| a.enabled && a.has_credentials() && !a.is_expired())
            .count()
    }

    /// 是否发 `maas_type: benefit`。
    pub fn is_benefit(&self) -> bool {
        self.channel.eq_ignore_ascii_case("benefit")
    }

    /// 落盘（创建我们自己的 `{channel, accounts}` 结构）。
    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("创建目录 {} 失败: {e}", dir.display()))?;
            }
        }
        let v = serde_json::json!({
            "channel": self.channel,
            "accounts": self.accounts,
            "removed": self.removed,
        });
        let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("落盘 {} 失败: {e}", self.path.display()))
    }

    /// 按 identity 合并（同一个人/同 AK 尾段 → 替换，否则追加）。
    /// 另外按「显示名 + domainId」也视为同一账号：桌面端重登后 AK 变了，
    /// 但 domainId 和显示名不变，旧条目才不会越积越多。
    pub fn upsert(&mut self, account: Account) -> Account {
        let id = account.identity();
        let named = !account.display_name().is_empty();
        if let Some(slot) = self.accounts.iter_mut().find(|a| {
            a.identity() == id
                || (named
                    && a.display_name() == account.display_name()
                    && a.domain_id == account.domain_id)
        }) {
            *slot = account.clone();
        } else {
            self.accounts.push(account.clone());
        }
        account
    }

    /// AK 尾 8 位（界面展示用，不泄完整密钥）。
    pub fn ak_tail(a: &Account) -> String {
        let ak = a.access_key.trim();
        if ak.len() > 8 {
            format!("…{}", &ak[ak.len() - 8..])
        } else {
            ak.to_string()
        }
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// RFC3339 → Unix 秒。支持 `Z` 与 `+08:00`/`-05:00` 两种时区写法。
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse::<i64>().ok() };
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let h = num(11, 13)?;
    let mi = num(14, 16)?;
    let sec = num(17, 19)?;
    let mut t = days_from_civil(y, mo as u32, d as u32) * 86_400 + h * 3600 + mi * 60 + sec;
    // 时区：从第 19 位开始，可能是 `.123Z` / `Z` / `+08:00`
    let tail = &s[19..];
    if let Some(off) = tail.strip_prefix('+').or_else(|| tail.strip_prefix('-')) {
        let sign = if tail.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off.split_once(':')?;
        let oh: i64 = oh.parse().ok()?;
        let om: i64 = om.parse().ok()?;
        t -= sign * (oh * 3600 + om * 60);
    }
    Some(t)
}

/// 公历 → 距 1970-01-01 的天数（Howard Hinnant）。
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// ISO8601（UTC，秒级）用于 `created-time` 头与响应。
pub fn iso8601_utc(unix: i64) -> String {
    let (y, mo, d, h, mi, s) = crate::signer::civil_from_unix(unix);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_variants() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-26T12:00:00.123Z"), Some(1_793_016_000));
        assert_eq!(parse_rfc3339("2026-10-26T20:00:00+08:00"), Some(1_793_016_000));
        assert_eq!(parse_rfc3339("bad"), None);
    }

    #[test]
    fn picks_latest_unexpired_account() {
        let v: Value = serde_json::from_str(
            r#"{"channel":"credit","accounts":[
                 {"name":"old","accessKey":"a","secretKey":"b","expires_at":"2020-01-01T00:00:00Z"},
                 {"name":"new","accessKey":"c","secretKey":"d","expires_at":"2099-01-01T00:00:00Z"}
               ]}"#,
        )
        .unwrap();
        let cfg = Config::from_value(v, Path::new("x")).unwrap();
        assert_eq!(cfg.pick().unwrap().display_name(), "new");
        assert_eq!(cfg.healthy_count(), 1);
    }

    #[test]
    fn accepts_single_session_shape() {
        let v: Value = serde_json::from_str(
            r#"{"account":{"id":"1","label":"hid_abc"},"accessKey":"AK","secretKey":"SK",
                "securitytoken":"T","expires_at":"2099-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let cfg = Config::from_value(v, Path::new("x")).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].display_name(), "hid_abc");
    }
}
