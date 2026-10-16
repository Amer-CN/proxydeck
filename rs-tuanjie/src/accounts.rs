//! accounts.rs —— 团结多账号池，逐条对应 `internal/tuanjie/accounts.go`（584 行）。
//!
//! 本模块是 **P8b 第二片**：把池本体（选号 / 402 预算标记 / 负载计数 / 状态摘要 /
//! 增删启停 / 本地账号 / 落盘）搬过来，并补上服务层换号要用的三样内存态语义：
//!
//! * **Dead（401 标死 / 换活号）** —— `mark_dead`，`pick` 永不再选，复活只靠 `rescan`
//!   （语义源：`internal/codebuddy/pool.go:304-318` 的 `MarkDead` + `Pick` 的跳过位；
//!   Go 团结侧**没有**这个标记，见 README「已知差异」第 1 条）。
//! * **冷却（429 限流窗口）** —— `mark_limited(uid, until)` + `clear_cool`，到期自动可选
//!   （懒判定，无后台循环）。
//! * **调用计数** —— `use_count` / `last_used` / `inflight` 全是内存态；其中
//!   `use_count`/`last_used` 与 Go 一样在 `pick` 内自增并**随池落盘**，
//!   `inflight`（`loads`）与 Dead/冷却一样只在内存里（重启清零，`accounts.go:58` 原样如此）。
//!
//! DPAPI 面走 [`crate::crypto`]（明文旧文件读兼容、写侧一律加密），与 Go
//! `appdir.ReadFile/WriteFile`（`accounts.go:77,110`）同款。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{Datelike, Local, NaiveDateTime, TimeZone, Timelike};
use serde_json::Value;

use crate::gojson::{format_go_float, go_string};

/// 账号池文件名 —— `accounts.go:62` 的 `tuanjie-accounts.json`。
pub const ACCOUNTS_FILE: &str = "tuanjie-accounts.json";

/// 402 复查顺延窗口 —— `accounts.go:325,330` 的 `now.Add(5 * time.Minute)`。
pub const RESUME_RETRY_SECS: i64 = 5 * 60;

// ---------------------------------------------------------------- 类型

/// 账号池里的一个团结账号 —— `accounts.go:20-33` 的 `Account`。
///
/// 字段名与 JSON tag 逐条对应；`budget_reset_at` 是 Go `time.Time`（**零值不会被
/// `omitempty` 省掉**，Go 1.26.5 实测，见 README 差异 3）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Account {
    pub user_id: String,
    pub username: String,
    pub org_id: String,
    pub access_token: String,
    /// `"pool"` = 入池账号（默认）；`"local"` = 本地客户端（token 恒空，实时读登录态）。
    pub source: String,
    pub last_used: String,
    pub use_count: i64,
    pub enabled: bool,
    pub has_glm53: bool,
    pub budget_exceeded: bool,
    /// 402 时记录的 5h 窗口重置时刻；`None` = Go 的 `time.Time{}`（零值）。
    pub budget_reset_at: Option<i64>,
    pub models: Vec<String>,
    // ------- 以下三项是 Go 团结侧没有的（服务层换号所需，见模块头注释）-------
    /// 凭据失效（上游 401/403）：无限期，`pick` 永不再选；复活只靠 [`AccountPool::rescan`]。
    pub dead: bool,
    /// 限流冷却到期时刻（epoch 秒）；`None` = 可用。
    pub limited_until: Option<i64>,
}

impl Account {
    /// 建一个 enabled 的池内账号（对应 `AccountPool.Add` 的缺省：`Enabled/HasGLM53 = true`）。
    pub fn new(user_id: &str, token: &str, username: &str, org_id: &str) -> Self {
        Account {
            user_id: user_id.to_string(),
            access_token: token.to_string(),
            username: username.to_string(),
            org_id: org_id.to_string(),
            enabled: true,
            has_glm53: true,
            ..Default::default()
        }
    }
}

/// 给前端的账号摘要（不含 token 本体）—— `accounts.go:36-50` 的 `AccountStatus`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AccountStatus {
    pub user_id: String,
    pub username: String,
    pub org_id: String,
    pub enabled: bool,
    pub use_count: i64,
    pub last_used: String,
    pub token_expires: String,
    pub token_remaining_hours: f64,
    pub has_glm53: bool,
    pub budget_exceeded: bool,
    pub inflight: i64,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub source: String,
    pub models: Vec<String>,
}

/// 取 token 的缝 —— 对应 `Account::effectiveAccessToken`（`accounts.go:220-225`）：
/// 本地账号（`source == "local"`）实时读 `oauth_creds.json`，池账号用入池快照。
/// 生产装配传 [`crate::credentials::load_access_token`]（按 `user_id` 忽略）；
/// 测试传合成串，**绝不读真实 JWT**。
pub type TokenProvider<'a> = dyn Fn(&Account) -> Result<String, String> + Send + Sync + 'a;

/// 生产 token 缝：对齐 Go `effectiveAccessToken`（`accounts.go:220-225`）——
/// 本地账号实时读登录态（桌面端切号自动跟随），池账号用入池快照。
/// 单独成函数以便门禁用合成账号锁定分支（`new()` 直接引用它，改缝不断线）；
/// `#[doc(hidden)]`：测试缝，不算公开 API。
#[doc(hidden)]
pub fn production_token_provider(a: &Account) -> Result<String, String> {
    if a.source == "local" {
        crate::credentials::load_access_token().map_err(|e| e.to_string())
    } else {
        Ok(a.access_token.clone())
    }
}

// ---------------------------------------------------------------- 池

/// 多账号池 —— `accounts.go:53-59` 的 `AccountPool`。单锁 + 切片 + 轮询指针（同款并发模型）。
pub struct AccountPool {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
    provider: Box<dyn for<'a> Fn(&'a Account) -> Result<String, String> + Send + Sync>,
}

struct Inner {
    accounts: Vec<Account>,
    index: usize,
    /// `user_id → 进行中请求数`（负载感知选号用）—— `accounts.go:58` 的 `loads`。
    loads: HashMap<String, i64>,
}

impl AccountPool {
    /// 生产构造：从数据目录的 `tuanjie-accounts.json` 加载 + 并入本地账号 ——
    /// `NewAccountPool()`（`accounts.go:65-70`）。token 缝对齐 Go 的
    /// `effectiveAccessToken`（`accounts.go:220-225`）：本地账号实时读登录态，
    /// 池账号用入池快照——否则池账号会拿错号发请求，无登录态的纯池部署全军覆没。
    pub fn new() -> Self {
        Self::with_path(
            Some(crate::pacing::data_path(ACCOUNTS_FILE)),
            production_token_provider,
        )
    }

    /// 显式路径 + token 缝（测试缝：临时目录 + 合成 token，绝不碰真实凭据）。
    ///
    /// `provider` 只在「取 token」时被调用；Go 侧 `effectiveAccessToken` 是方法，
    /// 这里显式传参是为了让门禁能在**不读真实登录态**的前提下驱动同一条路径。
    pub fn with_path<F>(path: Option<PathBuf>, provider: F) -> Self
    where
        F: for<'a> Fn(&'a Account) -> Result<String, String> + Send + Sync + 'static,
    {
        let p = AccountPool {
            inner: Mutex::new(Inner {
                accounts: Vec::new(),
                index: 0,
                loads: HashMap::new(),
            }),
            path,
            provider: Box::new(provider),
        };
        p.reload();
        p.ensure_local_account();
        p
    }

    /// 空路径（不读不写盘）+ 合成 token 的内存池（单测用）。
    pub fn detached(accounts: Vec<Account>, provider: fn(&Account) -> Result<String, String>) -> Self {
        let p = AccountPool {
            inner: Mutex::new(Inner {
                accounts,
                index: 0,
                loads: HashMap::new(),
            }),
            path: None,
            provider: Box::new(provider),
        };
        p
    }

    /// `reload()`（`accounts.go:72-98`）：读文件 → 解 JSON → 过滤（`user_id` 空、
    /// 非 local 且 token 空的一律不入选）；文件缺失/解析失败 = 空池（不报错）。
    pub fn reload(&self) {
        let mut g = self.lock();
        g.accounts.clear();
        let Some(path) = self.path.clone() else {
            return;
        };
        let Ok(b) = crate::crypto::read_file(&path) else {
            return;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&b) else {
            return;
        };
        let Some(list) = v.get("accounts").and_then(|a| a.as_array()) else {
            return;
        };
        for item in list {
            if item.is_null() {
                continue; // Go: `if a == nil { continue }`
            }
            let mut a = account_from_value(item);
            if a.user_id.is_empty() {
                continue;
            }
            if a.source != "local" && a.access_token.is_empty() {
                continue;
            }
            if a.source.is_empty() {
                a.source = String::new(); // 显式：与 Go 零值一致（omitempty 不写）
            }
            g.accounts.push(a);
        }
    }

    /// `saveLocked()`（`accounts.go:100-113`）：`MarshalIndent(out,"","  ")` + DPAPI 落盘。
    /// 加密失败只留日志（内存池不受影响）；路径为空则不落盘。
    fn save_locked(&self, g: &Inner) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let bytes = marshal_accounts_indent(&g.accounts);
        if let Err(e) = crate::crypto::write_file(&path, &bytes) {
            crate::logf(&format!("[tuanjie] 账号池落盘失败（内存态不受影响）：{e}"));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 账号总数（0 = 单账号模式）—— `Size()`（`accounts.go:116`）。
    pub fn size(&self) -> usize {
        self.lock().accounts.len()
    }

    /// 选号 —— `Pick(model)`（`accounts.go:127-216`）。优先级逐条照搬：
    /// enabled 且非 402 → 认领过滤（精确匹配 `models`，空列表=兜底）→ GLM 资格 →
    /// 负载最低优先 → 轮询推进；**额外**跳过 Dead 与冷却中的号（服务层换号所需）。
    /// 无可用号返回 `None`（调用方如实透传上游响应，不编造）。
    pub fn pick(&self, model: &str) -> Option<Account> {
        let mut g = self.lock();
        if g.accounts.is_empty() {
            return None;
        }
        let now = now_unix();
        let enabled: Vec<usize> = (0..g.accounts.len())
            .filter(|&i| {
                let a = &g.accounts[i];
                a.enabled
                    && !a.budget_exceeded
                    && !a.dead
                    && !is_cooling(a, now)
            })
            .collect();
        if enabled.is_empty() {
            return None;
        }
        // 模型路由：精确匹配 Models；空列表 = 兜底接未认领模型
        let mut enabled = enabled;
        if !model.is_empty() {
            let mut claimers: Vec<usize> = Vec::new();
            let mut fallbacks: Vec<usize> = Vec::new();
            for &i in &enabled {
                let a = &g.accounts[i];
                if a.models.is_empty() {
                    fallbacks.push(i);
                    continue;
                }
                if a.models.iter().any(|m| m == model) {
                    claimers.push(i);
                }
            }
            if !claimers.is_empty() {
                enabled = claimers;
            } else if !fallbacks.is_empty() {
                enabled = fallbacks;
            } else {
                for a in &g.accounts {
                    if !a.models.is_empty() {
                        return None; // 显式配置过的池无人认领该模型：不派发
                    }
                }
            }
        }
        // GLM 资格路由
        if is_glm_model(model) {
            let glm: Vec<usize> = enabled
                .iter()
                .copied()
                .filter(|&i| g.accounts[i].has_glm53)
                .collect();
            if !glm.is_empty() {
                enabled = glm;
            }
        }
        // 负载感知：最低负载优先
        let mut min_load: i64 = -1;
        for &i in &enabled {
            let l = *g.loads.get(&g.accounts[i].user_id).unwrap_or(&0);
            if min_load < 0 || l < min_load {
                min_load = l;
            }
        }
        if min_load > 0 {
            let light: Vec<usize> = enabled
                .iter()
                .copied()
                .filter(|&i| *g.loads.get(&g.accounts[i].user_id).unwrap_or(&0) <= min_load)
                .collect();
            if !light.is_empty() {
                enabled = light;
            }
        }
        // 轮询
        for i in 0..enabled.len() {
            let idx = (g.index + i) % enabled.len();
            let acct = enabled[idx];
            g.index = (idx + 1) % enabled.len();
            g.accounts[acct].last_used = format_go_seconds(Local::now());
            g.accounts[acct].use_count += 1;
            let out = g.accounts[acct].clone();
            self.save_locked(&g);
            return Some(out);
        }
        None
    }

    /// 取 token —— `Account::effectiveAccessToken`。
    pub fn effective_access_token(&self, a: &Account) -> Result<String, String> {
        (self.provider)(a)
    }

    /// 选号并解析可用 token —— `PickWithToken`（`accounts.go:229-243`）：最多试池大小次，
    /// 取 token 失败就换下一个号；全部失败返回 `None`。
    pub fn pick_with_token(&self, model: &str) -> Option<(Account, String)> {
        let n = self.size();
        for _ in 0..n {
            let a = self.pick(model)?;
            match self.effective_access_token(&a) {
                Ok(tok) => return Some((a, tok)),
                Err(e) => {
                    crate::logf(&format!(
                        "[tuanjie] 账号 {} 取 token 失败：{e}，跳过换下一账号",
                        a.user_id
                    ));
                }
            }
        }
        None
    }

    /// 标记 402 并记录预计重置时刻 —— `MarkBudgetExceededAt`（`accounts.go:248-262`）。
    pub fn mark_budget_exceeded_at(&self, user_id: &str, reset_at: Option<i64>) {
        let mut g = self.lock();
        for i in 0..g.accounts.len() {
            if g.accounts[i].user_id == user_id {
                g.accounts[i].budget_exceeded = true;
                g.accounts[i].budget_reset_at = reset_at;
                if g.accounts[i].source == "local" {
                    crate::logf(&format!(
                        "[tuanjie] 本地账号 {user_id} 命中预算超额（BudgetExceeded=true 置 UI 标记，不物理删除）"
                    ));
                }
                self.save_locked(&g);
                return;
            }
        }
    }

    /// 标记账号 402 并持久化（选号自动绕过）—— `MarkBudgetExceeded`（`accounts.go:265`）。
    pub fn mark_budget_exceeded(&self, user_id: &str) {
        self.mark_budget_exceeded_at(user_id, None);
    }

    /// 402 自动恢复的单轮扫描 —— `resumeScanOnce`（`accounts.go:289-338`）。
    ///
    /// `query_quota(uid) -> (remaining, next_reset, ok)`；`now_unix` 由调用方给定
    /// （Go 在每轮循环里各取一次 `time.Now()`，这里收成一个参数，纯函数可测）。
    /// 锁内不调 `query_quota`：先收集候选、解锁再查、再加锁写回。
    pub fn resume_scan_once<F>(&self, now_unix: i64, query_quota: Option<F>)
    where
        F: Fn(&str) -> (f64, Option<i64>, bool),
    {
        let Some(query) = query_quota else {
            return;
        };
        let mut ids: Vec<String> = Vec::new();
        {
            let g = self.lock();
            for a in &g.accounts {
                // 恢复只看 BudgetExceeded（不要求 Enabled）
                if a.budget_exceeded
                    && a.budget_reset_at.is_some()
                    && now_unix > a.budget_reset_at.unwrap_or(i64::MAX)
                {
                    ids.push(a.user_id.clone());
                }
            }
        }
        for uid in ids {
            let (remaining, next_reset, ok) = query(&uid);
            let mut g = self.lock();
            let Some(a) = g.accounts.iter_mut().find(|a| a.user_id == uid) else {
                continue;
            };
            if !a.budget_exceeded {
                continue; // 期间已被 Toggle 清标记：跳过
            }
            if ok && remaining > 0.0 {
                a.budget_exceeded = false;
                a.budget_reset_at = None;
                crate::logf(&format!(
                    "[tuanjie] 账号 {uid} 额度已恢复（5h 窗口重置），自动恢复轮询"
                ));
            } else if ok {
                if let Some(nr) = next_reset {
                    a.budget_reset_at = Some(nr);
                }
                if a.budget_reset_at.is_none() || a.budget_reset_at.unwrap_or(0) <= now_unix {
                    a.budget_reset_at = Some(now_unix + RESUME_RETRY_SECS); // +5min 防死循环快扫
                }
                crate::logf(&format!(
                    "[tuanjie] 账号 {uid} 复查额度仍未恢复（剩余 {remaining:.1}），顺延至 {} 再查",
                    format_go_seconds_local(a.budget_reset_at)
                ));
            } else {
                a.budget_reset_at = Some(now_unix + RESUME_RETRY_SECS);
                crate::logf(&format!("[tuanjie] 账号 {uid} 复查额度查询失败，顺延 5 分钟重试"));
            }
            self.save_locked(&g);
        }
    }

    /// 进行中计数 +1 —— `IncLoad`（`accounts.go:341`）。
    pub fn inc_load(&self, user_id: &str) {
        let mut g = self.lock();
        *g.loads.entry(user_id.to_string()).or_insert(0) += 1;
    }

    /// 进行中计数 −1（钳在 0）—— `DecLoad`（`accounts.go:346`）。
    pub fn dec_load(&self, user_id: &str) {
        let mut g = self.lock();
        if let Some(v) = g.loads.get_mut(user_id) {
            if *v > 0 {
                *v -= 1;
            }
        }
    }

    /// 池内某号的进行中计数 —— `accounts.go:381` 的 `p.loads[a.UserID]`。
    pub fn load_of(&self, user_id: &str) -> i64 {
        let g = self.lock();
        *g.loads.get(user_id).unwrap_or(&0)
    }

    /// 全部账号摘要（含 JWT 过期解析）—— `Status()`（`accounts.go:355-387`）。
    pub fn status(&self) -> Vec<AccountStatus> {
        let g = self.lock();
        g.accounts
            .iter()
            .map(|a| {
                let (expires, remain) = match jwt_expiry(&a.access_token) {
                    Some(exp) => {
                        let now = now_unix();
                        let mut remain = (exp - now) as f64 / 3600.0;
                        if remain < 0.0 {
                            remain = 0.0;
                        }
                        (
                            format_local_minute(exp),
                            ((remain * 10.0) as i64) as f64 / 10.0,
                        )
                    }
                    None => ("未知".to_string(), 0.0),
                };
                AccountStatus {
                    user_id: a.user_id.clone(),
                    username: a.username.clone(),
                    org_id: a.org_id.clone(),
                    enabled: a.enabled,
                    use_count: a.use_count,
                    last_used: a.last_used.clone(),
                    token_expires: expires,
                    token_remaining_hours: remain,
                    has_glm53: a.has_glm53,
                    budget_exceeded: a.budget_exceeded,
                    inflight: *g.loads.get(&a.user_id).unwrap_or(&0),
                    source: a.source.clone(),
                    models: a.models.clone(),
                }
            })
            .collect()
    }

    /// 新增账号（`user_id` 重复返回 false）—— `Add`（`accounts.go:390-404`）。
    pub fn add(&self, user_id: &str, token: &str, username: &str, org_id: &str) -> bool {
        let mut g = self.lock();
        if g.accounts.iter().any(|a| a.user_id == user_id) {
            return false;
        }
        g.accounts
            .push(Account::new(user_id, token, username, org_id));
        self.save_locked(&g);
        true
    }

    /// 把本地客户端登录态并进池 —— `EnsureLocalAccount`（`accounts.go:411-436`）。
    /// 取 token 成功且 `sub` 非空才动手；已在池里则只在 `sub` 变化时更新那条。
    pub fn ensure_local_account(&self) -> bool {
        let probe = Account {
            source: "local".to_string(),
            ..Default::default()
        };
        let Ok(token) = (self.provider)(&probe) else {
            return false;
        };
        let sub = jwt_sub(&token);
        if sub.is_empty() {
            return false;
        }
        let mut g = self.lock();
        for i in 0..g.accounts.len() {
            if g.accounts[i].source == "local" {
                if g.accounts[i].user_id != sub {
                    g.accounts[i].user_id = sub;
                    self.save_locked(&g);
                }
                return true;
            }
        }
        g.accounts.push(Account {
            user_id: sub,
            source: "local".to_string(),
            enabled: true,
            has_glm53: true,
            ..Default::default()
        });
        self.save_locked(&g);
        true
    }

    /// 池内本地账号 —— `LocalAccount`（`accounts.go:440-449`）。
    pub fn local_account(&self) -> Option<Account> {
        let g = self.lock();
        g.accounts.iter().find(|a| a.source == "local").cloned()
    }

    /// 删除账号 —— `Remove`（`accounts.go:452-468`）。
    pub fn remove(&self, user_id: &str) -> bool {
        let mut g = self.lock();
        let before = g.accounts.len();
        g.accounts.retain(|a| a.user_id != user_id);
        let changed = g.accounts.len() != before;
        if changed {
            self.save_locked(&g);
        }
        changed
    }

    /// 启停账号（重新启用时清 402 标记）—— `Toggle`（`accounts.go:471-485`）。
    /// 与 codebuddy 的 Toggle 一样**不碰** Dead/冷却（三态互不越权）。
    pub fn toggle(&self, user_id: &str, enabled: bool) -> bool {
        let mut g = self.lock();
        for i in 0..g.accounts.len() {
            if g.accounts[i].user_id == user_id {
                g.accounts[i].enabled = enabled;
                if enabled {
                    g.accounts[i].budget_exceeded = false;
                }
                self.save_locked(&g);
                return true;
            }
        }
        false
    }

    /// 标记 GLM 资格 —— `SetGLM`（`accounts.go:488-499`）。
    pub fn set_glm(&self, user_id: &str, has: bool) -> bool {
        let mut g = self.lock();
        for i in 0..g.accounts.len() {
            if g.accounts[i].user_id == user_id {
                g.accounts[i].has_glm53 = has;
                self.save_locked(&g);
                return true;
            }
        }
        false
    }

    /// 设置账号认领的模型列表（空切片等价于兜底）—— `SetModels`（`accounts.go:502-517`）。
    pub fn set_models(&self, user_id: &str, models: &[String]) -> bool {
        let mut g = self.lock();
        for i in 0..g.accounts.len() {
            if g.accounts[i].user_id == user_id {
                g.accounts[i].models = models.to_vec();
                self.save_locked(&g);
                return true;
            }
        }
        false
    }

    /// 排序后的 user_id 列表（探针遍历用）—— `SortedUserIDs`（`accounts.go:520-529`）。
    pub fn sorted_user_ids(&self) -> Vec<String> {
        let g = self.lock();
        let mut ids: Vec<String> = g.accounts.iter().map(|a| a.user_id.clone()).collect();
        ids.sort();
        ids
    }

    /// 按 user_id 取账号 —— `Get`（`accounts.go:532-541`）。
    pub fn get(&self, user_id: &str) -> Option<Account> {
        let g = self.lock();
        g.accounts.iter().find(|a| a.user_id == user_id).cloned()
    }

    // ------------------------------------------------------------ 服务层语义

    /// **凭据失效标记（上游 401/403）** —— 语义源 `internal/codebuddy/pool.go:304-318`。
    ///
    /// token 被官方吊销不会自愈（重新登录才有救）：无限期标记，`pick` 永不再选；
    /// 复活只靠 [`AccountPool::rescan`]。找不到 uid 静默（与 `MarkLimited` 同款）。
    pub fn mark_dead(&self, user_id: &str) {
        let mut g = self.lock();
        for a in g.accounts.iter_mut() {
            if a.user_id == user_id {
                a.dead = true;
                crate::logf(&format!(
                    "[tuanjie] 账号 {} 凭据失效(401)，入池但不再轮到它",
                    uid_short(user_id)
                ));
                return;
            }
        }
    }

    /// **限流冷却标记（上游 429 配额族）**：`until` = 上游报文里的重置时点，到期即自动可选
    /// （`pick` 懒判定）。语义源 `codebuddy/pool.go:261-271`。
    pub fn mark_limited(&self, user_id: &str, until: i64) {
        let mut g = self.lock();
        for a in g.accounts.iter_mut() {
            if a.user_id == user_id {
                a.limited_until = Some(until);
                crate::logf(&format!(
                    "[tuanjie] 账号 {} 限流冷却至 {}",
                    uid_short(user_id),
                    format_local_seconds(until)
                ));
                return;
            }
        }
    }

    /// 提前解除冷却 —— `ClearCool`（`codebuddy/pool.go:524-534`）。只清冷却，不清 Dead。
    pub fn clear_cool(&self, user_id: &str) -> bool {
        let mut g = self.lock();
        for a in g.accounts.iter_mut() {
            if a.user_id == user_id {
                a.limited_until = None;
                return true;
            }
        }
        false
    }

    /// 全量重扫 —— `Rescan`（`codebuddy/pool.go:497-519`）：
    /// 用同一套加载逻辑重建池，同 uid 继承冷却与 `enabled`（用户意图不得被 rescan 复活）；
    /// **Dead 不继承 = rescan 即复位**（Dead 是纯内存态）；**402 预算标记经落盘存续**
    /// （`budget_exceeded` 写进池文件，重载后仍在——清标记只靠 Toggle 或额度恢复）；
    /// 文件已删的号出池。
    /// 传入的 `provider` 供新号解析本地 token（本地账号实时读）。
    pub fn rescan<F>(&self, provider: F)
    where
        F: Fn(&Account) -> Result<String, String>,
    {
        let fresh = self.load_accounts_with(&provider);
        let mut g = self.lock();
        struct Keep {
            limited: Option<i64>,
            enabled: bool,
        }
        let mut old: HashMap<String, Keep> = HashMap::new();
        for a in &g.accounts {
            old.insert(
                a.user_id.clone(),
                Keep {
                    limited: a.limited_until,
                    enabled: a.enabled,
                },
            );
        }
        let mut next = fresh;
        for a in next.iter_mut() {
            if let Some(k) = old.get(&a.user_id) {
                a.limited_until = k.limited;
                a.enabled = k.enabled;
            }
        }
        g.accounts = next;
        if g.index >= g.accounts.len() {
            g.index = 0;
        }
    }

    /// 按给定 token 缝重读池（`reload` 的「用什么取 token」版本；rescan 复用同一套加载）。
    fn load_accounts_with<F>(&self, provider: F) -> Vec<Account>
    where
        F: Fn(&Account) -> Result<String, String>,
    {
        let Some(path) = self.path.clone() else {
            return Vec::new();
        };
        let _ = &provider; // 加载本身不取 token（token 是快照），保留参数以标明同一套加载路径
        let Ok(b) = crate::crypto::read_file(&path) else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_slice::<Value>(&b) else {
            return Vec::new();
        };
        let Some(list) = v.get("accounts").and_then(|a| a.as_array()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in list {
            if item.is_null() {
                continue;
            }
            let a = account_from_value(item);
            if a.user_id.is_empty() {
                continue;
            }
            if a.source != "local" && a.access_token.is_empty() {
                continue;
            }
            out.push(a);
        }
        out
    }

    /// 测试缝：直接把池内成员换成给定列表（不经磁盘）。
    #[cfg(test)]
    pub fn replace_accounts_for_test(&self, accounts: Vec<Account>) {
        let mut g = self.lock();
        g.accounts = accounts;
        g.index = 0;
    }

    /// 取锁读取池内成员快照（测试/诊断用）。
    pub fn accounts_snapshot(&self) -> Vec<Account> {
        self.lock().accounts.clone()
    }
}

impl Default for AccountPool {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- 纯函数

/// `isGLMModel`（`accounts.go:544-553`）：**只折 ASCII 大写**，然后看前 3 字节是否 `glm`。
/// Go 逐字节原地小写（多字节 UTF-8 会被打散），这里用同一套字节操作对齐其观察结果。
pub fn is_glm_model(model: &str) -> bool {
    let mut m = model.as_bytes().to_vec();
    for c in m.iter_mut() {
        if c.is_ascii_uppercase() {
            *c += b'a' - b'A';
        }
    }
    m.len() >= 3 && &m[..3] == b"glm"
}

/// `splitJWT`（`accounts.go:574-583`）：按 `.` 切；**没有点也返回一段**（长度恒 ≥ 1）。
pub fn split_jwt(s: &str) -> Vec<&str> {
    s.split('.').collect()
}

/// `jwtExpiry`（`accounts.go:556-572`）：解 payload 的 `exp`（不校验签名）。
/// 段数 ≠ 3 / base64 坏 / JSON 坏 / `exp == 0` → `None`。
pub fn jwt_expiry(token: &str) -> Option<i64> {
    let parts = split_jwt(token);
    if parts.len() != 3 {
        return None;
    }
    let payload = crate::crypto::base64_decode(parts[1].as_bytes()).ok()?;
    let v: Value = serde_json::from_slice(&payload).ok()?;
    let exp = v.get("exp")?.as_i64()?;
    if exp == 0 {
        return None;
    }
    Some(exp)
}

/// `jwtSub`（`cdp_browser.go:168-193`）：解 payload 的 `sub`；与 `jwtExpiry` 同款容忍度。
pub fn jwt_sub(token: &str) -> String {
    let parts = split_jwt(token);
    if parts.len() != 3 {
        return String::new();
    }
    let Ok(payload) = crate::crypto::base64_decode(parts[1].as_bytes()) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&payload) else {
        return String::new();
    };
    v.get("sub")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string()
}

/// 日志用 uid 前 8 位（不打 token、不打完整 uid）—— `uidShort`（`codebuddy/pool.go:537`）。
pub fn uid_short(uid: &str) -> String {
    uid.chars().take(8).collect()
}

fn is_cooling(a: &Account, now_unix: i64) -> bool {
    match a.limited_until {
        Some(until) => now_unix < until,
        None => false,
    }
}

/// 当前 unix 秒（对齐 Go `time.Now()` 在 `now.After(...)` 比较里的用法）。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `time.Now().Format("2006-01-02 15:04:05")`（`accounts.go:210` 的 `LastUsed`）。
pub fn format_go_seconds(t: chrono::DateTime<Local>) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// `exp.Format("2006-01-02 15:04")`（`accounts.go:368` 的 `TokenExpires`）。
pub fn format_local_minute(epoch_secs: i64) -> String {
    match Local.timestamp_opt(epoch_secs, 0) {
        chrono::LocalResult::Single(t) => {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                t.year(),
                t.month(),
                t.day(),
                t.hour(),
                t.minute()
            )
        }
        _ => "未知".to_string(),
    }
}

/// `2006-01-02 15:04:05` 形态（本地时区），epoch 秒输入。
pub fn format_local_seconds(epoch_secs: i64) -> String {
    match Local.timestamp_opt(epoch_secs, 0) {
        chrono::LocalResult::Single(t) => format_go_seconds(t),
        _ => String::new(),
    }
}

fn format_go_seconds_local(v: Option<i64>) -> String {
    v.map(format_local_seconds).unwrap_or_default()
}

/// RFC3339（本地时区偏移、秒精度）—— Go `time.Time.MarshalJSON` 的形态
/// （`time.RFC3339Nano` 再把纳秒裁零；Go 侧 `BudgetResetAt` 由 `time.Time{}` 或
/// `time.Now().Add(...)` 得来，都是秒精度）。
fn format_rfc3339_local(epoch_secs: i64) -> String {
    let t = Local.timestamp_opt(epoch_secs, 0).single();
    match t {
        Some(t) => t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        None => "0001-01-01T00:00:00Z".to_string(),
    }
}

/// 解析 RFC3339（读回磁盘时用；Go 的 `time.Time` 反序列化接受 RFC3339）。
///
/// Go 零值时间（`"0001-01-01T00:00:00Z"` = unix `-62135596800`）映射为 `None`：
/// `MarkBudgetExceededAt` 的注释明言"resetAt 零值 = 未知，不参与自动恢复"
/// （`accounts.go:245-247`）——读成 `Some(-621…)` 会让每条零重置账号被
/// `resume_scan_once` 收为复查候选，每 5 分钟打一次积分接口。
fn parse_rfc3339(s: &str) -> Option<i64> {
    /// Go `time.Time{}` 零值的 unix 秒（`0001-01-01T00:00:00Z`）。
    const GO_ZERO_UNIX: i64 = -62135596800;
    let ts = if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        t.timestamp()
    } else {
        // Go 零值形态（"0001-01-01T00:00:00Z"）也接受
        NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S")
            .ok()?
            .and_utc()
            .timestamp()
    };
    if ts == GO_ZERO_UNIX {
        None
    } else {
        Some(ts)
    }
}

// ---------------------------------------------------------------- JSON 读写

/// 从 `Account` 的 JSON 形态读回（与 Go 的 `encoding/json` 同款字段名/类型容忍度）。
pub fn account_from_value(v: &Value) -> Account {
    let s = |k: &str| -> String {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let b = |k: &str| -> bool { v.get(k).and_then(|x| x.as_bool()).unwrap_or(false) };
    let mut models: Vec<String> = Vec::new();
    if let Some(arr) = v.get("models").and_then(|x| x.as_array()) {
        for m in arr {
            if let Some(s) = m.as_str() {
                models.push(s.to_string());
            }
        }
    }
    Account {
        user_id: s("user_id"),
        username: s("username"),
        org_id: s("org_id"),
        access_token: s("access_token"),
        source: s("source"),
        last_used: s("last_used"),
        use_count: v.get("use_count").and_then(|x| x.as_i64()).unwrap_or(0),
        enabled: b("enabled"),
        has_glm53: b("has_glm53"),
        budget_exceeded: b("budget_exceeded"),
        budget_reset_at: v
            .get("budget_reset_at")
            .and_then(|x| x.as_str())
            .and_then(parse_rfc3339),
        models,
        dead: false,
        limited_until: None,
    }
}

/// `json.MarshalIndent(struct{Accounts []*Account}{...}, "", "  ")` 的逐字节形态
/// （`accounts.go:104`）：字段声明序、`omitempty` 口径、`time.Time` 的 RFC3339。
pub fn marshal_accounts_indent(accounts: &[Account]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"{\n  \"accounts\": ");
    if accounts.is_empty() {
        out.extend_from_slice(b"[]");
    } else {
        out.extend_from_slice(b"[\n");
        for (i, a) in accounts.iter().enumerate() {
            out.extend_from_slice(b"    {\n");
            let mut fields: Vec<(String, String)> = Vec::new();
            fields.push(("user_id".into(), go_string(&a.user_id)));
            if !a.username.is_empty() {
                fields.push(("username".into(), go_string(&a.username)));
            }
            if !a.org_id.is_empty() {
                fields.push(("org_id".into(), go_string(&a.org_id)));
            }
            fields.push(("access_token".into(), go_string(&a.access_token)));
            if !a.source.is_empty() {
                fields.push(("source".into(), go_string(&a.source)));
            }
            if !a.last_used.is_empty() {
                fields.push(("last_used".into(), go_string(&a.last_used)));
            }
            if a.use_count != 0 {
                fields.push(("use_count".into(), a.use_count.to_string()));
            }
            fields.push(("enabled".into(), a.enabled.to_string()));
            fields.push(("has_glm53".into(), a.has_glm53.to_string()));
            fields.push(("budget_exceeded".into(), a.budget_exceeded.to_string()));
            // Go 的 `omitempty` 对 `time.Time` **无效**（结构体永不为「空」）⇒ 零值照样写。
            let reset = a
                .budget_reset_at
                .map(format_rfc3339_local)
                .unwrap_or_else(|| "0001-01-01T00:00:00Z".to_string());
            fields.push(("budget_reset_at".into(), go_string(&reset)));
            if !a.models.is_empty() {
                let items: Vec<String> = a.models.iter().map(|m| go_string(m)).collect();
                fields.push(("models".into(), format!("[{}]", items.join(","))));
            }
            for (k, (name, j)) in fields.iter().enumerate() {
                if k > 0 {
                    out.extend_from_slice(b",\n");
                }
                out.extend_from_slice(b"      ");
                out.extend_from_slice(go_string(name).as_bytes());
                out.push(b':');
                out.push(b' ');
                out.extend_from_slice(j.as_bytes());
            }
            out.push(b'\n');
            out.extend_from_slice(b"    }");
            if i + 1 < accounts.len() {
                out.push(b',');
            }
            out.push(b'\n');
        }
        out.extend_from_slice(b"  ");
        out.extend_from_slice(b"]");
    }
    out.extend_from_slice(b"\n}");
    out
}

/// 供下游（GUI/探针）用的浮点形态（Go `%v` 口径）—— `status()` 的字段由 serde 写出，
/// 这里只是把 `gojson` 的口径引进来以备服务层复用（避免出现第二套浮点规则）。
pub fn go_float(f: f64) -> String {
    format_go_float(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_jwt(sub: &str, exp: i64) -> String {
        let payload = format!("{{\"sub\":\"{sub}\",\"exp\":{exp}}}");
        let enc = crate::crypto::base64_encode(payload.as_bytes());
        format!("eyJhbGciOiJub25lIn0.{}.sig", enc.trim_end_matches('='))
    }

    #[test]
    fn synthetic_jwt_helper_round_trips() {
        // 先锁住测量仪器本身：App 的 payload 由 Rust 侧构造，Go 侧必须解出同一个 sub。
        let tok = synthetic_jwt("user-123", 4_000_000_000);
        assert_eq!(tok, "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyLTEyMyIsImV4cCI6NDAwMDAwMDAwMH0.sig");
        assert_eq!(jwt_sub(&tok), "user-123");
    }

    #[test]
    fn glm_model_check_is_ascii_only_lowercase() {
        assert!(is_glm_model("GLM-5.3"));
        assert!(is_glm_model("glm"));
        assert!(is_glm_model("GlM-5"));
        assert!(!is_glm_model("gl"));
        assert!(!is_glm_model(""));
        assert!(!is_glm_model("kimi-k3"));
        // 全角字符不在 ASCII 折换范围（Go 逐字节折换同理）
        assert!(!is_glm_model("ＧＬＭ"));
    }

    #[test]
    fn jwt_parsing_matches_go_tolerance() {
        let tok = synthetic_jwt("user-123", 4_000_000_000);
        assert_eq!(jwt_sub(&tok), "user-123");
        assert_eq!(jwt_expiry(&tok), Some(4_000_000_000));
        assert_eq!(split_jwt(&tok).len(), 3);
        // 段数不对 / base64 坏 / 无 exp / exp=0
        assert_eq!(jwt_expiry("abc"), None);
        assert_eq!(jwt_sub("abc"), "");
        assert_eq!(jwt_expiry("a.!!!.c"), None);
        let no_exp = {
            let enc = crate::crypto::base64_encode(br#"{"sub":"x"}"#);
            format!("h.{}.s", enc.trim_end_matches('='))
        };
        assert_eq!(jwt_expiry(&no_exp), None);
        let zero_exp = {
            let enc = crate::crypto::base64_encode(br#"{"exp":0}"#);
            format!("h.{}.s", enc.trim_end_matches('='))
        };
        assert_eq!(jwt_expiry(&zero_exp), None);
    }

    #[test]
    fn accounts_file_shape_is_go_marshal_indent() {
        let a1 = Account {
            user_id: "u1".into(),
            username: "小明".into(),
            access_token: "tok1".into(),
            source: "local".into(),
            enabled: true,
            has_glm53: true,
            ..Default::default()
        };
        let a2 = Account {
            user_id: "u2".into(),
            access_token: "tok2".into(),
            use_count: 7,
            enabled: true,
            has_glm53: false,
            budget_exceeded: true,
            budget_reset_at: Some(4_000_000_000),
            models: vec!["gpt-4o".into(), "kimi-k3".into()],
            ..Default::default()
        };
        let got = String::from_utf8(marshal_accounts_indent(&[a1, a2])).unwrap();
        let want = r#"{
  "accounts": [
    {
      "user_id": "u1",
      "username": "小明",
      "access_token": "tok1",
      "source": "local",
      "enabled": true,
      "has_glm53": true,
      "budget_exceeded": false,
      "budget_reset_at": "0001-01-01T00:00:00Z"
    },
    {
      "user_id": "u2",
      "access_token": "tok2",
      "use_count": 7,
      "enabled": true,
      "has_glm53": false,
      "budget_exceeded": true,
      "budget_reset_at": "2096-10-02T15:06:40+08:00",
      "models": ["gpt-4o","kimi-k3"]
    }
  ]
}"#;
        assert_eq!(got, want);
    }

    #[test]
    fn empty_pool_serializes_as_empty_array() {
        assert_eq!(
            String::from_utf8(marshal_accounts_indent(&[])).unwrap(),
            "{\n  \"accounts\": []\n}"
        );
    }

    #[test]
    fn reload_round_trips_and_filters_like_go() {
        let dir = std::env::temp_dir().join("rs-tuanjie-acc-reload");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ACCOUNTS_FILE);
        let body = r#"{"accounts":[
            {"user_id":"pool1","access_token":"t1","enabled":true},
            {"user_id":"local1","source":"local","access_token":"","enabled":true},
            {"user_id":"notoken","access_token":"","enabled":true},
            {"user_id":"","access_token":"t","enabled":true},
            null
        ]}"#;
        std::fs::write(&path, body).unwrap();
        let p = AccountPool::with_path(Some(path.clone()), |_a| {
            Err("no login state".to_string())
        });
        let ids: Vec<String> = p.sorted_user_ids();
        assert_eq!(ids, vec!["local1", "pool1"], "只保留 token 非空或 source=local 的号");
        // 落盘后仍是合法 JSON 且能读回同样成员（DPAPI 密文形态）
        let a = Account {
            user_id: "u9".into(),
            access_token: "tok9".into(),
            enabled: true,
            ..Default::default()
        };
        p.replace_accounts_for_test(vec![a]);
        p.add("u10", "tok10", "", "");
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.starts_with(crate::crypto::CRYPT_PREFIX), "落盘必须是密文");
        let p2 = AccountPool::with_path(Some(path.clone()), |_a| {
            Err("no login state".to_string())
        });
        assert_eq!(p2.size(), 2);
        assert_eq!(p2.get("u9").unwrap().access_token, "tok9");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
