//! pool.rs —— WorkBuddy / CodeBuddy 双区账号池，对应 Go `internal/codebuddy/pool.go`。
//!
//! 要点（逐条照抄 Go，含注释里的纪律）：
//! * 账号来源是桌面端 auth 目录的 `*.info` 文件：扫描入池、同 uid 留 mtime 最新；
//!   **auth 目录全程只读**——不落盘、不回写、不改名、不删除；
//! * 限流信号是上游 429/6004（`frequency limit`），`MarkLimited` 只记冷却到期时点，
//!   到期即自动可选（`Pick` 懒判定，无后台恢复循环）；
//! * 状态三件套：`Enabled` 手动停用 / `Removed` 移出 / `UseCount`·`LastUsed`·`Inflight`
//!   调用统计。**持久化只落 Enabled/Removed**（数据目录 `codebuddy-pool(-intl).json`）；
//!   `Dead`/`Exhausted`/冷却/统计一律**内存态**（重启清零——如实登记，见 crate README）。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::Value;

use crate::credential;
use crate::datadir;
use crate::gjson;
use crate::gotime;
use crate::region::Region;

/// 账号池里的一个登录态（对应 auth 目录里一个 `*.info` 文件）。
#[derive(Clone)]
pub struct Account {
    /// 桌面端账号 uid（`X-User-Id`）。
    pub uid: String,
    /// 显示名（日志用）。
    pub nickname: String,
    /// `*.info` 文件路径（只读）。
    pub auth_path: String,
    /// 6004 冷却到期时点（epoch 毫秒；`i64::MIN` = Go 零值 = 可用）。
    pub limited_until: i64,
    /// 凭据失效（上游 401/403）：无限期标记，`Pick` 永不再选；复活只靠 `Rescan`。
    pub dead: bool,
    /// 额度耗尽（上游 14018）：无限期标记；复活靠 `Rescan` 或 `ClearExhausted`。
    pub exhausted: bool,
    /// 手动停用开关（GUI「停用/启用」）；建池缺省 true。
    pub enabled: bool,
    /// 从池中移出（GUI「移除」）：uid 键持久排除；恢复靠 `Restore`（可撤销）。
    pub removed: bool,
    /// 调用次数（`Pick` 选中后由调用方 `Touch` 计数）。
    pub use_count: i64,
    /// 最近使用时点（epoch 毫秒；`i64::MIN` = Go 零值）。
    pub last_used: i64,
    /// 进行中请求数（`handleChat` Inc/Dec，GUI「进行中 N」）。
    pub inflight: i64,
    /// 文件 mtime（建池去重/排序用；**不**出现在响应里，Go 用局部结构体承载）。
    mtime: SystemTime,
}

impl Account {
    fn is_cooling(&self, now: i64) -> bool {
        self.limited_until != i64::MIN && now < self.limited_until
    }
}

/// 状态文件里单 uid 的持久条目（只存用户意图：停用/移出）。
fn state_entry_json(enabled: bool, removed: bool) -> Value {
    gjson::obj(vec![
        ("enabled", Value::Bool(enabled)),
        ("removed", Value::Bool(removed)),
    ])
}

/// `poolStatePath`：状态文件落数据目录（与 stats/failover 同规矩）。CN 用
/// `codebuddy-pool.json`，INTL 沿 `-intl` 后缀惯例用 `codebuddy-pool-intl.json`。
/// `CODEBUDDY_POOL_DIR` 环境变量可重定向目录（单测隔离与临时实例用）。
pub fn pool_state_path(region: Region) -> String {
    let dir = match std::env::var("CODEBUDDY_POOL_DIR") {
        Ok(d) if !d.is_empty() => d,
        _ => datadir::data_dir().to_string_lossy().to_string(),
    };
    let suffix = if region == Region::Intl { "-intl" } else { "" };
    std::path::Path::new(&dir)
        .join(format!("codebuddy-pool{suffix}.json"))
        .to_string_lossy()
        .to_string()
}

/// `loadPoolState`：读回停用/移出状态；文件缺失/损坏返回空表（建池零障碍，不报错）。
pub fn load_pool_state(path: &str) -> BTreeMap<String, (bool, bool)> {
    let mut out = BTreeMap::new();
    if path.is_empty() {
        return out;
    }
    let Ok(b) = std::fs::read(path) else {
        return out;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&b) else {
        return out;
    };
    let Some(accs) = v.get("accounts").and_then(|a| a.as_object()) else {
        return out;
    };
    for (uid, e) in accs {
        let enabled = e.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let removed = e.get("removed").and_then(|x| x.as_bool()).unwrap_or(false);
        out.insert(uid.clone(), (enabled, removed));
    }
    out
}

/// 多账号轮询：单锁 + 切片 + 轮询指针（照抄 Go 的并发模型）。
pub struct AccountPool {
    inner: Mutex<PoolInner>,
    state_path: String,
}

struct PoolInner {
    accounts: Vec<Account>,
    index: usize,
}

impl AccountPool {
    /// `NewAccountPool`：扫描 auth 目录建池；零可用返回 `None`
    /// （调用方保持「未找到登录文件」的现有报错行为）。
    pub fn new(region: Region) -> Option<AccountPool> {
        let accounts = scan_auth_files(region);
        if accounts.is_empty() {
            return None;
        }
        let state_path = pool_state_path(region);
        let mut inner = PoolInner {
            accounts,
            index: 0,
        };
        if let Ok(st) = std::fs::read(&state_path) {
            if let Ok(v) = serde_json::from_slice::<Value>(&st) {
                if let Some(accs) = v.get("accounts").and_then(|a| a.as_object()) {
                    for a in inner.accounts.iter_mut() {
                        if let Some(e) = accs.get(&a.uid) {
                            a.enabled = e.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
                            a.removed = e.get("removed").and_then(|x| x.as_bool()).unwrap_or(false);
                        }
                    }
                }
            }
        }
        Some(AccountPool {
            inner: Mutex::new(inner),
            state_path,
        })
    }

    pub fn size(&self) -> usize {
        lock(&self.inner).accounts.len()
    }

    /// 全量落盘（须持锁调用）：0600 + tmp+rename 原子写。只写池内成员。
    /// 写失败静默（状态文件是记忆不是命脉）。
    fn save_state_locked(inner: &PoolInner, path: &str) {
        if path.is_empty() {
            return;
        }
        let mut accounts = serde_json::Map::new();
        for a in &inner.accounts {
            accounts.insert(a.uid.clone(), state_entry_json(a.enabled, a.removed));
        }
        let v = gjson::obj(vec![
            ("accounts", Value::Object(accounts)),
            ("version", Value::Number(1.into())),
        ]);
        let body = gjson::to_go_bytes(&v);
        let tmp = format!("{path}.tmp");
        if write_0600(&tmp, &body).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    /// `Pick` 轮询选一个可用号；全池不可用 → `None`（调用方如实透传上游响应，不编造）。
    pub fn pick(&self) -> Option<Account> {
        let mut g = lock(&self.inner);
        let now = gotime::now_millis();
        let n = g.accounts.len();
        for i in 0..n {
            let idx = (g.index + i) % n;
            let a = &g.accounts[idx];
            if a.dead || a.exhausted || !a.enabled || a.removed || a.is_cooling(now) {
                continue;
            }
            let out = a.clone();
            g.index = (idx + 1) % n;
            return Some(out);
        }
        println!(
            "[codebuddy] 账号池 {} 个号全部不可用（冷却/停用/移除/凭据失效/额度耗尽），本次无可用号",
            n
        );
        None
    }

    /// `MarkLimited`：收到 6004/配额 429 时给当前号记冷却（until = 上游报文里的重置时点）。
    pub fn mark_limited(&self, uid: &str, until_ms: i64) {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.limited_until = until_ms;
                println!(
                    "[codebuddy] 账号 {} 限流冷却至 {}",
                    uid_short(uid),
                    crate::failover::format_reset_display(until_ms)
                );
                return;
            }
        }
    }

    /// `MarkExhausted`：额度耗尽标记（上游 14018）。找不到 uid 静默。
    pub fn mark_exhausted(&self, uid: &str) {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.exhausted = true;
                println!(
                    "[codebuddy] 账号 {} 额度耗尽(14018)，入池但不再轮到它",
                    uid_short(uid)
                );
                return;
            }
        }
    }

    /// `ClearExhausted`：解掉额度耗尽标记（GUI「额度恢复」按钮）。只清耗尽，不清 Dead。
    pub fn clear_exhausted(&self, uid: &str) -> bool {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.exhausted = false;
                return true;
            }
        }
        false
    }

    /// `MarkDead`：凭据失效标记（上游 401/403）。找不到 uid 静默。
    pub fn mark_dead(&self, uid: &str) {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.dead = true;
                println!(
                    "[codebuddy] 账号 {} 凭据失效(401)，入池但不再轮到它",
                    uid_short(uid)
                );
                return;
            }
        }
    }

    /// `Toggle` 手动停用/启用：改内存 + 全量落盘，`Pick` 即时生效。
    pub fn toggle(&self, uid: &str, enabled: bool) -> bool {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.enabled = enabled;
                Self::save_state_locked(&g, &self.state_path);
                println!(
                    "[codebuddy] 账号 {} 手动{}",
                    uid_short(uid),
                    if enabled { "启用" } else { "停用" }
                );
                return true;
            }
        }
        false
    }

    /// `Remove` 从池中移出：`removed=true`，uid 键持久排除。**不碰 auth 文件**。
    pub fn remove(&self, uid: &str) -> bool {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.removed = true;
                Self::save_state_locked(&g, &self.state_path);
                println!(
                    "[codebuddy] 账号 {} 已移出账号池（可恢复，不动登录文件）",
                    uid_short(uid)
                );
                return true;
            }
        }
        false
    }

    /// `Restore` 恢复被移出的号（可撤销）：`removed=false`。
    pub fn restore(&self, uid: &str) -> bool {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.removed = false;
                Self::save_state_locked(&g, &self.state_path);
                println!("[codebuddy] 账号 {} 已恢复入池", uid_short(uid));
                return true;
            }
        }
        false
    }

    /// `IncLoad` 进行中计数 +1。
    pub fn inc_load(&self, uid: &str) {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.inflight += 1;
                return;
            }
        }
    }

    /// `DecLoad` 进行中计数 -1（钳在 0，防负数）。
    pub fn dec_load(&self, uid: &str) {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                if a.inflight > 0 {
                    a.inflight -= 1;
                }
                return;
            }
        }
    }

    /// `Touch` 使用计数：`UseCount+1` + `LastUsed=now`。
    pub fn touch(&self, uid: &str) {
        let mut g = lock(&self.inner);
        let now = gotime::now_millis();
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.use_count += 1;
                a.last_used = now;
                return;
            }
        }
    }

    /// `PathOf` 按完整 uid 精确取号（积分面板等按账号查询用）。
    pub fn path_of(&self, uid: &str) -> Option<(String, String, String)> {
        let g = lock(&self.inner);
        for a in &g.accounts {
            if a.uid == uid {
                return Some((a.auth_path.clone(), a.nickname.clone(), a.uid.clone()));
            }
        }
        None
    }

    /// `NewestPath` 池内排序第一号（建池按 mtime 新→旧，第一号 = 当前最新登录）。
    pub fn newest_path(&self) -> Option<(String, String, String)> {
        let g = lock(&self.inner);
        g.accounts
            .first()
            .map(|a| (a.auth_path.clone(), a.nickname.clone(), a.uid.clone()))
    }

    /// `Snapshot` 锁内复制池成员视图（`GET /v1/accounts` 用）。
    pub fn snapshot(&self) -> Vec<AccountView> {
        let g = lock(&self.inner);
        let now = gotime::now_millis();
        let off = gotime::local_offset_secs();
        g.accounts
            .iter()
            .map(|a| AccountView {
                uid: a.uid.clone(),
                uid_short: uid_short(&a.uid),
                nickname: a.nickname.clone(),
                file: base_name(&a.auth_path),
                limited_until: gotime::marshal_time(a.limited_until, off),
                cooling: a.is_cooling(now),
                dead: a.dead,
                exhausted: a.exhausted,
                enabled: a.enabled,
                removed: a.removed,
                use_count: a.use_count,
                last_used: if a.last_used == i64::MIN {
                    String::new()
                } else {
                    gotime::format_rfc3339_seconds(a.last_used, off)
                },
                inflight: a.inflight,
            })
            .collect()
    }

    /// `Rescan`：同套扫描逻辑重扫。同 uid 继承冷却与 `Enabled`/`Removed`；
    /// `Dead`/`Exhausted` 不继承（rescan 即复位）。
    pub fn rescan(&self, region: Region) {
        let fresh = scan_auth_files(region);
        let mut g = lock(&self.inner);
        let mut old: BTreeMap<String, (i64, bool, bool)> = BTreeMap::new();
        for a in &g.accounts {
            old.insert(a.uid.clone(), (a.limited_until, a.enabled, a.removed));
        }
        let mut next: Vec<Account> = Vec::with_capacity(fresh.len());
        for mut a in fresh {
            if let Some((until, enabled, removed)) = old.get(&a.uid) {
                a.limited_until = *until;
                a.enabled = *enabled;
                a.removed = *removed;
            }
            next.push(a);
        }
        g.accounts = next;
        if g.index >= g.accounts.len() {
            g.index = 0;
        }
    }

    /// `ClearCool`：把指定号的 `LimitedUntil` 置零值（GUI「提前恢复」）。
    pub fn clear_cool(&self, uid: &str) -> bool {
        let mut g = lock(&self.inner);
        for a in g.accounts.iter_mut() {
            if a.uid == uid {
                a.limited_until = i64::MIN;
                return true;
            }
        }
        false
    }
}

/// `AccountView`（`GET /v1/accounts` 响应用）：完整 uid 供 GUI 回传；`AuthPath` 只给
/// 文件名；不含 token。`last_used` 为 RFC3339 字符串（未使用给空串）。
pub struct AccountView {
    pub uid: String,
    pub uid_short: String,
    pub nickname: String,
    pub file: String,
    pub limited_until: String,
    pub cooling: bool,
    pub dead: bool,
    pub exhausted: bool,
    pub enabled: bool,
    pub removed: bool,
    pub use_count: i64,
    pub last_used: String,
    pub inflight: i64,
}

impl AccountView {
    /// 按 Go 的 `AccountView` 字段名写出（键排序交给 `gjson`）。
    pub fn to_json(&self) -> Value {
        let v = gjson::obj(vec![
            ("uid", Value::String(self.uid.clone())),
            ("uid_short", Value::String(self.uid_short.clone())),
            ("nickname", Value::String(self.nickname.clone())),
            ("file", Value::String(self.file.clone())),
            (
                "limited_until",
                Value::String(self.limited_until.clone()),
            ),
            ("cooling", Value::Bool(self.cooling)),
            ("dead", Value::Bool(self.dead)),
            ("exhausted", Value::Bool(self.exhausted)),
            ("enabled", Value::Bool(self.enabled)),
            ("removed", Value::Bool(self.removed)),
            ("use_count", Value::Number(self.use_count.into())),
            ("last_used", Value::String(self.last_used.clone())),
            ("inflight", Value::Number(self.inflight.into())),
        ]);
        v
    }
}

/// `scanAuthFiles`：扫描 auth 目录里命中区域 domain 的全部 `*.info` 文件。
/// 解析失败的（读不了 / JSON 坏 / 无 uid）或区域不符的跳过，不整池报错；
/// 同 uid 只留 mtime 最新的一个。返回按 mtime 新→旧排序。
pub fn scan_auth_files(region: Region) -> Vec<Account> {
    let suffixes = region.cfg().domain_suffixes;
    let mut found: Vec<Account> = Vec::new();
    let mut encrypted = 0usize;
    for dir in credential::find_auth_dirs() {
        let Ok(ents) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in ents.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() || !name.ends_with(".info") {
                continue;
            }
            let Ok(info) = e.metadata() else { continue };
            let path = e.path();
            let parsed = parse_auth_info(&path);
            let Some((domain, uid, nick)) = parsed else {
                println!("[codebuddy] 账号池扫描：跳过无法解析或区域不符的文件 {name}");
                continue;
            };
            if !crate::region::contains_domain(&domain, suffixes) {
                println!("[codebuddy] 账号池扫描：跳过无法解析或区域不符的文件 {name}");
                continue;
            }
            // 不可用文件不入池（入池会被 Pick 选中再吃 401 误标死号；原因不带原文）。
            if !credential::auth_token_usable(&path) {
                encrypted += 1;
                println!(
                    "[codebuddy] 账号池扫描：跳过不可用登录文件 {}（{}）",
                    name,
                    credential::auth_token_unusable_reason(&path)
                );
                continue;
            }
            found.push(Account {
                uid,
                nickname: nick,
                auth_path: path.to_string_lossy().to_string(),
                limited_until: i64::MIN,
                dead: false,
                exhausted: false,
                enabled: true,
                removed: false,
                use_count: 0,
                last_used: i64::MIN,
                inflight: 0,
                mtime: info.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
    let mut latest: BTreeMap<String, Account> = BTreeMap::new();
    for f in found {
        match latest.get(&f.uid) {
            Some(old) if old.mtime >= f.mtime => {}
            _ => {
                latest.insert(f.uid.clone(), f);
            }
        }
    }
    let mut uniq: Vec<Account> = latest.into_values().collect();
    // mtime 新→旧；同 mtime 按路径定序（Go 的 sort.Slice 不稳定，同 mtime 场景在真实
    // 目录里不出现——黄金夹具用显式 mtime，故这里定序只为可复现）。
    uniq.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then_with(|| a.auth_path.cmp(&b.auth_path))
    });
    if uniq.is_empty() {
        let dirs = credential::find_auth_dirs().join("; ");
        if encrypted > 0 {
            println!(
                "[codebuddy] 账号池扫描 region={}：{} 个登录文件但 token 均不可用（加密解不出来或已损坏），已搜索目录：{}",
                region.as_str(),
                encrypted,
                dirs
            );
        } else {
            println!(
                "[codebuddy] 账号池扫描 region={} 零命中，已搜索目录：{}",
                region.as_str(),
                dirs
            );
        }
    }
    uniq
}

/// `parseAuthInfo`：读 `*.info` 的 domain/uid/昵称（建池用；token 等其余字段不落输出）。
pub fn parse_auth_info(path: &Path) -> Option<(String, String, String)> {
    let b = std::fs::read(path).ok()?;
    let s = credential::parse_session_info(&b).ok()?;
    if s.uid.is_empty() {
        return None;
    }
    Some((s.domain, s.uid, s.nickname))
}

/// `uidShort` 日志用 uid 前 8 位（不打 token、不打完整 uid）。
pub fn uid_short(uid: &str) -> String {
    uid.chars().take(8).collect()
}

fn base_name(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string())
}

/// Go `os.WriteFile(..., 0o600)`。
fn write_0600(path: &str, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(body)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `IncLoad`/`DecLoad` 必须配对且钳在 0（门禁对 `inflight` 的取值做了归一，
    /// 这条单测是它独立的正面对冲）。
    #[test]
    fn inflight_inc_dec_pairs_and_clamps() {
        let _env = crate::test_env::lock();
        let dir = std::env::temp_dir().join("rs-codebuddy-pool-inflight");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CODEBUDDY_POOL_DIR", dir.to_string_lossy().to_string());
        let p = AccountPool {
            inner: Mutex::new(PoolInner {
                accounts: vec![Account {
                    uid: "u-1".to_string(),
                    nickname: "n".to_string(),
                    auth_path: "x.info".to_string(),
                    limited_until: i64::MIN,
                    dead: false,
                    exhausted: false,
                    enabled: true,
                    removed: false,
                    use_count: 0,
                    last_used: i64::MIN,
                    inflight: 0,
                    mtime: SystemTime::UNIX_EPOCH,
                }],
                index: 0,
            }),
            state_path: String::new(),
        };
        p.inc_load("u-1");
        p.inc_load("u-1");
        assert_eq!(p.snapshot()[0].inflight, 2);
        p.dec_load("u-1");
        assert_eq!(p.snapshot()[0].inflight, 1);
        p.dec_load("u-1");
        p.dec_load("u-1"); // 多减一次：钳在 0，不许变负
        assert_eq!(p.snapshot()[0].inflight, 0);
        // 未知 uid 静默
        p.inc_load("nope");
        p.dec_load("nope");
        std::env::remove_var("CODEBUDDY_POOL_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uid_short_is_first_eight() {
        assert_eq!(uid_short("u-intl-golden"), "u-intl-g");
        assert_eq!(uid_short("abc"), "abc");
        assert_eq!(uid_short(""), "");
    }

    #[test]
    fn pool_state_path_has_intl_suffix() {
        let _env = crate::test_env::lock();
        std::env::set_var("CODEBUDDY_POOL_DIR", "X:\\data");
        assert_eq!(
            pool_state_path(Region::Cn),
            "X:\\data\\codebuddy-pool.json"
        );
        assert_eq!(
            pool_state_path(Region::Intl),
            "X:\\data\\codebuddy-pool-intl.json"
        );
        std::env::remove_var("CODEBUDDY_POOL_DIR");
    }
}
