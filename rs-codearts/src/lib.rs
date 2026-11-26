//! rs-codearts —— ProxyDeck 华为云码道（CodeArts Doer）插件。
//!
//! 对外（本机回环）：
//!
//! ```text
//! GET  /health                  本地应答（不上游）
//! GET  /v1/models               上游 /v1/model/builtin（10 分钟缓存）
//! POST /v1/chat/completions     流式透传 / 非流式聚合（上游一律 stream=true）
//! GET  /v1/quota                上游 /snap-manager/v1/statistics/plugin
//! ```
//!
//! 账号来源（按优先级）：
//! 1. **本机码道登录态**：`%APPDATA%\codearts\secretStorage\*.enc`
//!    （DPAPI + AES-256-GCM，见 `import.rs`；只读，不续期）；
//! 2. `codearts-accounts.json`（`{accounts:[...]}` 或单个 session JSON）。
//!
//! 设计要点：
//! * 上游是**原生 OpenAI 协议**，请求体基本原样转发；本插件只补 `stream:true`
//!   与 `prompt_cache_key`，并同步 `model` / `model-id` / `model-name` 三处；
//! * 业务错误可能以 HTTP 200 返回（`error_code`），首事件预检在发响应头之前转成真错误；
//! * `glm-5.3` 只支持流式 → 对上游**永远**发 stream=true，客户端要非流式就本地聚合；
//! * 每账号并发上限 3，`x-ot-session-id` 按 AK 派生并复用，不每请求随机。

pub mod account_lock;
pub mod accounts;
pub mod activity;
pub mod config;
pub mod login;
pub mod datadir;
pub mod import;
pub mod protocol;
pub mod refresh;
pub mod protocols;
pub mod response;
pub mod signer;
pub mod sse;
pub mod store;
pub mod toolrefs;
pub mod upstream;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use clap::Parser;
use serde_json::{json, Value};

use accounts::AccountManager;
use config::{Account, Config};
use upstream::{ModelInfo, UpError, Upstream};

/// 请求体上限（与其它 crate 的 32 MiB 口径一致）。
pub const MAX_BODY_BYTES: usize = 32 << 20;
/// 并发上限（`TM.00001041`）时对**同一个号**的退避重试间隔：2/4/6/8/10 秒。
/// 上游槽位是时间租约（约 1 分钟），换号只是把问题搬到别的号上。
pub const CONCURRENCY_RETRY_SECS: [u64; 5] = [2, 4, 6, 8, 10];
/// 网关排队（`429 + queue-status: queued`）的总等待预算，默认 120 秒，
/// `CODEARTS_QUEUE_BUDGET_SECS` 可覆盖（0 = 不等待，直接透传 429）。
fn queue_budget() -> Duration {
    let secs = std::env::var("CODEARTS_QUEUE_BUDGET_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(120);
    Duration::from_secs(secs)
}
/// 排队轮询间隔。
const QUEUE_POLL: Duration = Duration::from_secs(2);
/// 模型清单缓存时长。
pub const MODELS_TTL: Duration = Duration::from_secs(600);
/// 等上游首个 SSE 事件的超时。
pub const STREAM_PRELUDE: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(name = "rs-codearts", version, about = "ProxyDeck 华为云码道 CodeArts 插件")]
pub struct Args {
    /// 插件服务模式开关（由 ProxyDeck 分发器传入）。
    #[arg(long = "plugin-codearts", default_value_t = false)]
    pub plugin_codearts: bool,

    #[arg(long, default_value = "8793")]
    pub port: u16,

    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// 账号池路径覆盖（默认数据目录下 `codearts-accounts.json`）。
    #[arg(long)]
    pub accounts: Option<PathBuf>,

    /// 上游根地址（测试用假上游）。
    #[arg(long, default_value = upstream::DEFAULT_BASE)]
    pub upstream: String,

    /// 关闭本机码道登录态自动导入（只用账号文件）。
    #[arg(long)]
    pub no_local: bool,

    /// STS 令牌端点（测试用假 STS；默认华为云）。
    #[arg(long, default_value = refresh::DEFAULT_STS_URL)]
    pub sts_url: String,

    /// 允许在码道桌面端运行时也主动续期本机登录态。
    /// 默认关：refresh_token 单次轮转，和桌面端抢续期会把桌面端踢下线。
    #[arg(long)]
    pub allow_local_refresh: bool,

    /// 多账号轮转最多尝试几个账号（默认 3）。
    #[arg(long, default_value_t = 3)]
    pub max_attempts: usize,
}

struct ModelsCache {
    at: Instant,
    access_key: String,
    list: Vec<ModelInfo>,
}

/// 单账号健康态（只在内存；重启清零）。
#[derive(Clone, Debug, Default)]
struct AcctHealth {
    dead: bool,
    dead_revision: Option<u64>,
    cooldown_until: Option<Instant>,
    failures: u32,
}

impl AcctHealth {
    fn is_dead(&self, revision: u64) -> bool {
        self.dead && self.dead_revision.map_or(true, |dead| dead == revision)
    }
}

pub struct Server {
    up: Upstream,
    accounts: AccountManager,
    accounts_path: PathBuf,
    overlay_path: PathBuf,
    allow_local: bool,
    allow_local_refresh: bool,
    max_attempts: usize,
    started: Instant,
    models: Mutex<Option<ModelsCache>>,
    local_cache: Mutex<Option<import::Imported>>,
    /// 本轮已自动归档过的本机登录态身份（避免每次请求都写盘）。
    archived: Mutex<std::collections::HashSet<String>>,
    /// 续期结果（`codearts-refreshed.json`）。
    refreshed: Mutex<store::Store>,
    /// 账号健康度（轮转依据）。
    health: Mutex<std::collections::HashMap<String, AcctHealth>>,
    /// 活动积分账本（签到/新手/邀请…；落数据目录 `codearts-activity.json`）。
    ledger: Mutex<activity::Ledger>,
    /// 每账号最近一次活动运行结果（GET /activity 展示）。
    activity_state: Mutex<std::collections::HashMap<String, activity::Entry>>,
    /// 福利模型目录（每账号独立；`benefit=true` 的模型聊天要带 maas_type）。
    benefit: Mutex<std::collections::HashMap<String, BenefitCatalog>>,
    /// 单账号并发闸门（上游每账号并发上限 3）。
    busy: Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>,
    /// 模型不可用学习（账号 → 模型 → 到期时刻）：`002002009 not registered` 等。
    unavailable: Mutex<std::collections::HashMap<String, std::collections::HashMap<String, Instant>>>,
    /// 每账号最近一次上游活动（看门狗保活判断用）。
    last_touch: Mutex<std::collections::HashMap<String, Instant>>,
    /// 待完成的 OAuth 登录（/login/*）。
    logins: Mutex<std::collections::HashMap<String, login::Pending>>,
}

/// 福利模型目录缓存（每账号一份，1 小时）。
pub struct BenefitCatalog {
    at: Instant,
    models: Vec<ModelInfo>,
    ids: std::collections::HashSet<String>,
    /// 上一轮福利来源不可用（4000/超时）：按失败态缓存，不再按未确认路由。
    failed: bool,
}

fn migrate_ledger_keys(ledger: &mut activity::Ledger, cfg: &Config) -> bool {
    let mut changed = false;
    for account in &cfg.accounts {
        let stable = account.stable_key();
        let mut keys = vec![account.identity()];
        keys.extend(account.identity_aliases.iter().cloned());
        for key in keys {
            if key == stable {
                continue;
            }
            if let Some(old) = ledger.entries.remove(&key) {
                let dst = ledger.entries.entry(stable.clone()).or_default();
                if old.last_run >= dst.last_run {
                    dst.last_run = old.last_run;
                    dst.last_message = old.last_message;
                    dst.last_credits = old.last_credits;
                }
                for (campaign, day) in old.settled {
                    match dst.settled.get(&campaign) {
                        Some(previous) if previous >= &day => {}
                        _ => {
                            dst.settled.insert(campaign, day);
                        }
                    }
                }
                changed = true;
            }
        }
    }
    changed
}

fn finish_account_groups(
    order: Vec<String>,
    mut groups: std::collections::HashMap<String, (Option<Account>, Option<Account>)>,
) -> Vec<(Account, &'static str)> {
    fn score(a: &Account) -> i64 {
        a.expires_unix().unwrap_or(-1)
    }
    let mut out: Vec<(Account, &'static str)> = Vec::new();
    for id in order {
        let Some((file, live)) = groups.remove(&id) else {
            continue;
        };
        let enabled = file.as_ref().map(|f| f.enabled).unwrap_or(true);
        let (mut a, source) = match (file, live) {
            (Some(f), Some(l)) => {
                if score(&l) >= score(&f) {
                    (l, "local")
                } else {
                    (f, "file")
                }
            }
            (Some(f), None) => (f, "file"),
            (None, Some(l)) => (l, "local"),
            (None, None) => continue,
        };
        a.enabled = enabled;
        out.push((a, source));
    }
    out
}

impl Server {
    pub fn new(
        up: Upstream,
        accounts_path: PathBuf,
        allow_local: bool,
        sts_url: String,
        allow_local_refresh: bool,
        max_attempts: usize,
    ) -> Result<Arc<Self>, String> {
        let overlay_path = accounts_path
            .parent()
            .map(|p| p.join("codearts-refreshed.json"))
            .unwrap_or_else(|| PathBuf::from("codearts-refreshed.json"));
        let accounts = AccountManager::with_refresh(
            accounts_path.clone(),
            up.client().clone(),
            sts_url.clone(),
        );
        // 启动即完成稳定 ID 迁移；失败不假装服务已就绪。
        let cfg = accounts.load()?;
        let mut refreshed = store::Store::load(&overlay_path);
        if refreshed.migrate_keys(&cfg) {
            refreshed.save(&overlay_path)?;
        }
        let ledger_path = accounts_path
            .parent()
            .map(|p| p.join("codearts-activity.json"))
            .unwrap_or_else(|| PathBuf::from("codearts-activity.json"));
        let mut loaded_ledger = activity::Ledger::load(&ledger_path);
        if migrate_ledger_keys(&mut loaded_ledger, &cfg) {
            loaded_ledger.save();
        }
        Ok(Arc::new(Self {
            up,
            accounts,
            accounts_path,
            overlay_path,
            allow_local,
            allow_local_refresh,
            max_attempts: max_attempts.max(1),
            started: Instant::now(),
            models: Mutex::new(None),
            local_cache: Mutex::new(None),
            refreshed: Mutex::new(refreshed),
            health: Mutex::new(std::collections::HashMap::new()),
            archived: Mutex::new(std::collections::HashSet::new()),
            ledger: Mutex::new(loaded_ledger),
            activity_state: Mutex::new(std::collections::HashMap::new()),
            benefit: Mutex::new(std::collections::HashMap::new()),
            busy: Mutex::new(std::collections::HashMap::new()),
            unavailable: Mutex::new(std::collections::HashMap::new()),
            last_touch: Mutex::new(std::collections::HashMap::new()),
            logins: Mutex::new(std::collections::HashMap::new()),
        }))
    }

    /// 读本机登录态；失败时退回上次成功的结果（桌面端短暂写文件时不至于瞬间掉线）。
    /// 读到有效登录态后自动把快照归档进账号文件（桌面端换号登录即自动攒池，
    /// 与 WorkBuddy 一样零手工；同一身份本轮只写一次）。
    fn local_session(&self) -> Option<import::Imported> {
        if !self.allow_local {
            return None;
        }
        match import::read_local_session() {
            Ok(s) => {
                if let Ok(mut c) = self.local_cache.lock() {
                    *c = Some(s.clone());
                }
                self.maybe_archive_local(&s.account);
                Some(s)
            }
            Err(_) => self.local_cache.lock().ok().and_then(|c| c.clone()),
        }
    }

    /// 本机登录态自动归档：账号文件里还没有该身份（且没被删过）就存一份快照。
    fn maybe_archive_local(&self, account: &Account) {
        if !account.has_credentials() {
            return;
        }
        let legacy = account.identity();
        let cfg = match self.load_config() {
            Ok(c) => c,
            Err(_) => Config {
                accounts: Vec::new(),
                channel: config::DEFAULT_CHANNEL.to_string(),
                removed: Vec::new(),
                path: self.accounts_path.clone(),
            },
        };
        let stable = AccountManager::resolve_in(&cfg, &legacy)
            .map(|id| id.as_str().to_string())
            .unwrap_or_else(|| legacy.clone());
        if let Ok(done) = self.archived.lock() {
            if done.contains(&stable) {
                return;
            }
        }
        if AccountManager::account_removed_in(&cfg, account)
            || cfg
                .accounts
                .iter()
                .any(|a| a.stable_key() == stable)
        {
            if let Ok(mut done) = self.archived.lock() {
                done.insert(stable);
            }
            return;
        }
        let mut snap = account.clone();
        if snap.name.trim().is_empty() {
            snap.name = snap.display_name();
        }
        match self.accounts.upsert(snap) {
            Ok(saved) => {
                println!(
                    "codearts-plugin: 本机登录态已自动归档（{}），账号池 +1",
                    account.display_name()
                );
                if let Ok(mut done) = self.archived.lock() {
                    done.insert(saved.id.as_str().to_string());
                }
            }
            Err(e) => eprintln!("codearts-plugin: 本机登录态归档失败: {e}"),
        }
    }

    fn load_config(&self) -> Result<Config, String> {
        self.accounts.load()
    }

    // ---------------- 活动积分（签到 / 新手 / 邀请…） ----------------

    /// 对单个账号跑一轮活动领取：拉活动列表 → 逐个可领活动 claim（必要时 confirm）→ 记账。
    /// 任何一步失败都只记录、不影响该账号的推理健康（不冷却、不标死）。
    async fn run_activity_for(&self, account: &Account) -> Value {
        let identity = account.stable_key();
        let now = crate::config::now_unix();
        let day = activity::local_day_utc8(now);
        let settled = self
            .ledger
            .lock()
            .map(|l| l.entry(&identity).settled)
            .unwrap_or_default();
        let campaigns = match self.up.campaigns(account).await {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("活动列表失败: {}", e.message);
                self.record_activity(&identity, now, 0.0, &msg);
                return serde_json::json!({
                    "account": account.display_name(),
                    "ok": false,
                    "message": msg,
                    "claimed": []
                });
            }
        };
        let mut claimed = Vec::new();
        let mut credits = 0.0f64;
        let mut msgs: Vec<String> = Vec::new();
        for c in &campaigns {
            // `CLAIMED` = 已到账但**未确认**：只补 confirm（幂等），绝不重复 claim。
            // 上游把「领取」和「确认」拆成两步，只 CLAIMED 不 CONFIRMED 的积分算挂账。
            if c.status.eq_ignore_ascii_case("CLAIMED") {
                if self.already_confirmed(&identity, c) {
                    continue;
                }
                match self.up.confirm_campaign(account, &c.id_json()).await {
                    Ok(()) => {
                        self.mark_confirmed(&identity, c, &day);
                        self.settle_activity(&identity, c, &day);
                        msgs.push(format!("{}: 已补确认", c.title));
                        claimed.push(serde_json::json!({
                            "id": c.id_text(),
                            "type": c.kind,
                            "title": c.title,
                            "credits": 0,
                            "confirmed": true
                        }));
                    }
                    Err(e) => {
                        let m = format!("{}: 补确认失败 {}", c.title, e.message);
                        println!("[codearts] 活动确认失败 {}", m);
                        msgs.push(m);
                    }
                }
                continue;
            }
            if !activity::claimable_now(c, &settled, &day, now) {
                continue;
            }
            match self.up.claim_campaign(account, &c.id_json(), &c.channel).await {
                Ok(v) => {
                    let need_confirm = v
                        .get("data")
                        .and_then(|d| d.get("id"))
                        .map(|x| !x.is_null())
                        .unwrap_or(false);
                    if need_confirm {
                        let _ = self.up.confirm_campaign(account, &c.id_json()).await;
                        self.mark_confirmed(&identity, c, &day);
                    }
                    let got = claim_credits(&v, c);
                    credits += got;
                    self.settle_activity(&identity, c, &day);
                    claimed.push(serde_json::json!({
                        "id": c.id_text(),
                        "type": c.kind,
                        "title": c.title,
                        "credits": got
                    }));
                    msgs.push(if got > 0.0 {
                        format!("{}: +{}", c.title, got)
                    } else {
                        format!("{}: 已领取", c.title)
                    });
                }
                Err(e) => {
                    let m = format!("{}: {}", c.title, e.message);
                    println!("[codearts] 活动领取失败 {}", m);
                    msgs.push(m);
                }
            }
        }
        let msg = if msgs.is_empty() {
            "无可领取活动".to_string()
        } else {
            msgs.join("; ")
        };
        self.record_activity(&identity, now, credits, &msg);
        serde_json::json!({
            "account": account.display_name(),
            "ok": true,
            "message": msg,
            "credits": credits,
            "claimed": claimed
        })
    }

    fn record_activity(&self, identity: &str, now: i64, credits: f64, msg: &str) {
        if let Ok(mut l) = self.ledger.lock() {
            l.update(identity, |e| {
                e.last_run = now;
                e.last_credits = credits;
                e.last_message = msg.to_string();
            });
            l.save();
        }
        if let Ok(mut st) = self.activity_state.lock() {
            st.insert(
                identity.to_string(),
                activity::Entry {
                    last_run: now,
                    last_message: msg.to_string(),
                    last_credits: credits,
                    settled: self
                        .ledger
                        .lock()
                        .map(|l| l.entry(identity).settled)
                        .unwrap_or_default(),
                },
            );
        }
    }

    /// 该活动是否已经补确认过（本地账本记 `confirm|<key>`）。
    fn already_confirmed(&self, identity: &str, c: &activity::Campaign) -> bool {
        self.ledger
            .lock()
            .ok()
            .map(|l| {
                l.entry(identity)
                    .settled
                    .contains_key(&format!("confirm|{}", c.key()))
            })
            .unwrap_or(false)
    }

    fn mark_confirmed(&self, identity: &str, c: &activity::Campaign, day: &str) {
        if let Ok(mut l) = self.ledger.lock() {
            l.update(identity, |e| {
                e.settled
                    .insert(format!("confirm|{}", c.key()), day.to_string());
            });
            l.save();
        }
    }

    fn settle_activity(&self, identity: &str, c: &activity::Campaign, day: &str) {
        if let Ok(mut l) = self.ledger.lock() {
            l.update(identity, |e| {
                e.settled.insert(c.key(), day.to_string());
                e.settled
                    .insert(format!("id:{}", c.id_text()), day.to_string());
            });
            l.save();
        }
    }

    /// 对所有账号（或指定 identity/显示名）跑一轮活动领取。
    pub async fn run_activities(&self, only: Option<String>) -> Vec<Value> {
        let mut out = Vec::new();
        for (a, _src) in self.all_accounts() {
            if !a.enabled || !a.has_credentials() {
                continue;
            }
            if let Some(o) = &only {
                if &a.stable_key() != o && &a.identity() != o && &a.display_name() != o {
                    continue;
                }
            }
            out.push(self.run_activity_for(&a).await);
        }
        out
    }

    /// 今天的每日签到是否已了结（看门狗/面板判断用）。
    pub fn login_settled_today(&self, identity: &str) -> bool {
        let day = activity::local_day_utc8(crate::config::now_unix());
        self.ledger
            .lock()
            .map(|l| {
                let e = l.entry(identity);
                e.settled
                    .iter()
                    .any(|(k, d)| k.starts_with("USER_LOGIN#") && d == &day)
            })
            .unwrap_or(false)
    }

    // ---------------- 限时福利模型 ----------------

    /// 确保某账号的福利模型目录（1 小时缓存）；首次会先做一次幂等 claim。
    async fn ensure_benefit(&self, account: &Account) -> Option<Vec<ModelInfo>> {
        let id = account.stable_key();
        if let Ok(g) = self.benefit.lock() {
            if let Some(c) = g.get(&id) {
                if c.at.elapsed() < Duration::from_secs(if c.failed { 1800 } else { 3600 }) {
                    return Some(c.models.clone());
                }
            }
        }
        let claim_err = self.up.claim_benefit(account).await.err();
        match self.up.benefit_models(account).await {
            Ok(models) => {
                let ids = models
                    .iter()
                    .map(|m| m.id.to_lowercase())
                    .collect::<std::collections::HashSet<_>>();
                if let Ok(mut g) = self.benefit.lock() {
                    g.insert(
                        id,
                        BenefitCatalog {
                            at: Instant::now(),
                            models: models.clone(),
                            ids,
                            failed: false,
                        },
                    );
                }
                Some(models)
            }
            Err(e) => {
                // 福利网关 2026-11 实测回 `4000 invalid channel, group not found`（通道下线）。
                // 失败也写一份空目录（30 分钟 TTL），别让每个 /v1/models 都去打一次。
                println!(
                    "[codearts] 福利目录不可用（{}）：{}；本账号按内置目录路由",
                    e.code, e.message
                );
                if let Some(ce) = claim_err {
                    println!("[codearts] 福利领取同样失败：{}", ce.message);
                }
                if let Ok(mut g) = self.benefit.lock() {
                    g.insert(
                        id,
                        BenefitCatalog {
                            at: Instant::now(),
                            models: Vec::new(),
                            ids: std::collections::HashSet::new(),
                            failed: true,
                        },
                    );
                }
                None
            }
        }
    }

    /// 该账号的该模型是否属于限时福利（聊天要带 `maas_type: benefit`）。
    pub fn is_benefit_model(&self, uid: &str, model: &str) -> bool {
        let key = model.trim().to_lowercase();
        if key.is_empty() {
            return false;
        }
        // 只用「本账号确认过的」福利目录。福利网关 2026-11 实测回 4000（通道下线），
        // 种子兜底会在首次聊天（模型目录还没缓存）时把 glm-5.3-flash 等内置模型
        // 误加 maas_type，实测直接 APIG.0101.404——所以不再用种子路由。
        self.benefit
            .lock()
            .ok()
            .and_then(|g| g.get(uid).map(|c| !c.failed && c.ids.contains(&key)))
            .unwrap_or(false)
    }

    // ---------------- OAuth 登录（/login/*） ----------------

    fn set_login_secret(&self, id: &str, secret: &str) {
        if let Ok(mut g) = self.logins.lock() {
            if let Some(p) = g.get_mut(id) {
                p.secret = Some(secret.to_string());
            }
        }
    }

    fn set_login_code(&self, id: &str, code: &str) {
        if let Ok(mut g) = self.logins.lock() {
            if let Some(p) = g.get_mut(id) {
                p.code = Some(code.to_string());
            }
        }
    }

    fn set_login_error(&self, id: &str, msg: &str) {
        if let Ok(mut g) = self.logins.lock() {
            if let Some(p) = g.get_mut(id) {
                p.done = true;
                p.error = Some(msg.to_string());
            }
        }
    }

    fn set_login_success(&self, id: &str, account: &str) {
        if let Ok(mut g) = self.logins.lock() {
            if let Some(p) = g.get_mut(id) {
                p.done = true;
                p.account = Some(account.to_string());
            }
        }
    }

    /// 取一次登录的 (code, verifier, port, jwk, done)。
    fn login_view(&self, id: &str) -> Option<(Option<String>, String, u16, Value, bool)> {
        self.logins.lock().ok().and_then(|g| {
            g.get(id).map(|p| {
                (
                    p.code.clone(),
                    p.verifier.clone(),
                    p.port,
                    p.jwk.clone(),
                    p.done,
                )
            })
        })
    }

    /// 小写别名 → 上游精确模型 ID（模型目录缓存里能查到就用精确 ID，否则原样返回）。
    pub fn canonical_model(&self, model: &str) -> String {
        let m = model.trim();
        if m.is_empty() {
            return m.to_string();
        }
        if let Ok(cache) = self.models.lock() {
            if let Some(c) = cache.as_ref() {
                if let Some(hit) = c.list.iter().find(|x| x.id.eq_ignore_ascii_case(m)) {
                    return hit.id.clone();
                }
            }
        }
        m.to_string()
    }

    // ---------------- 并发闸门 / 模型可用性学习 ----------------

    /// 等网关排队放行（`429 + queue-status: queued`）：轮询 trace 直到 `promoted`
    /// 或超预算；超时就 DELETE 取消，别把队列项挂在服务端。
    async fn await_queue(&self, account: &Account, trace_id: &str) -> bool {
        let budget = queue_budget();
        if budget.is_zero() {
            return false;
        }
        let deadline = Instant::now() + budget;
        loop {
            if Instant::now() >= deadline {
                let _ = self.up.trace_queue_cancel(account, trace_id).await;
                return false;
            }
            tokio::time::sleep(QUEUE_POLL).await;
            match self.up.trace_queue_state(account, trace_id).await {
                Ok(v) => {
                    if queue_promoted(&v) {
                        return true;
                    }
                }
                // 查询失败不等于没放行：继续等，直到预算耗尽。
                Err(e) => println!("[codearts] 排队状态查询失败（继续等）：{}", e.message),
            }
        }
    }

    /// 单账号并发闸门（上游每账号并发 3；`CODEARTS_MAX_CONCURRENT` 可覆盖）。
    pub fn permit(&self, uid: &str) -> Arc<tokio::sync::Semaphore> {
        let limit = crate::config::max_concurrent();
        let mut g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(uid.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(limit)))
            .clone()
    }

    /// 模型对该账号暂时不可用（`002002009 not registered` / benefit 未领）。
    pub fn mark_model_unavailable(&self, uid: &str, model: &str, secs: u64) {
        if let Ok(mut g) = self.unavailable.lock() {
            g.entry(uid.to_string())
                .or_default()
                .insert(model.trim().to_lowercase(), Instant::now() + Duration::from_secs(secs));
        }
    }

    pub fn model_available(&self, uid: &str, model: &str) -> bool {
        let key = model.trim().to_lowercase();
        if let Ok(g) = self.unavailable.lock() {
            if let Some(m) = g.get(uid).and_then(|m| m.get(&key)) {
                return *m <= Instant::now();
            }
        }
        true
    }

    /// 记录一次成功/失败的上游调用（看门狗保活判断用）。
    pub fn touch_account(&self, uid: &str) {
        if let Ok(mut g) = self.last_touch.lock() {
            g.insert(uid.to_string(), Instant::now());
        }
    }

    pub fn idle_for(&self, uid: &str) -> Option<Duration> {
        self.last_touch
            .lock()
            .ok()
            .and_then(|g| g.get(uid).map(|t| t.elapsed()))
    }

    /// 码道桌面端是否在运行（Electron 用户目录的 lockfile）。
    /// 只用它做「本机登录态要不要主动续期」的保守判断；lockfile 残留时最坏是少续一次。
    fn codearts_running(&self) -> bool {
        crate::import::local_app_dir()
            .map(|d| d.join("lockfile").exists())
            .unwrap_or(false)
    }

    fn health_of(&self, id: &str) -> AcctHealth {
        self.health
            .lock()
            .ok()
            .and_then(|h| h.get(id).cloned())
            .unwrap_or_default()
    }

    fn mark_ok(&self, account: &Account) {
        if let Ok(mut h) = self.health.lock() {
            let id = account.stable_key();
            let e = h.entry(id).or_default();
            e.failures = 0;
            e.cooldown_until = None;
            if e.dead_revision.map_or(true, |revision| revision == account.revision) {
                e.dead = false;
                e.dead_revision = None;
            }
        }
    }

    fn mark_cooling(&self, id: &str, secs: u64) {
        if let Ok(mut h) = self.health.lock() {
            let e = h.entry(id.to_string()).or_default();
            e.failures += 1;
            e.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    fn mark_dead(&self, id: &str, revision: u64) {
        if let Ok(mut h) = self.health.lock() {
            let e = h.entry(id.to_string()).or_default();
            e.dead = true;
            e.dead_revision = Some(revision);
            e.failures += 1;
        }
    }

    /// 凭据失效的处置：本机登录态在「桌面端运行中、且本插件不主动续期」时只冷却，
    /// 因为桌面端随后会自己刷新凭据；文件账号才标死（需要用户更新文件/重新导入）。
    fn note_credential_failure(&self, account: &Account, source: &str) {
        if source == "local" && !self.allow_local_refresh && self.codearts_running() {
            self.mark_cooling(&account.stable_key(), 120);
        } else {
            self.mark_dead(&account.stable_key(), account.revision);
        }
    }

    /// 账号全集：按身份合并为一行。同一身份可能同时存在于
    /// 文件快照、本机实时登录态、续期 overlay 三处——取三者中过期最晚的凭据
    /// （桌面端每小时自己轮转，本机登录态；overlay 存的是插件上次续期的结果）。
    /// `enabled` 开关取文件项（没有文件项默认启用）。已删除（removed 名单）
    /// 的身份不再出现，也不再自动归档。
    fn all_accounts(&self) -> Vec<(Account, &'static str)> {
        let overlay = self.refreshed.lock().ok().map(|s| s.clone()).unwrap_or_default();
        let cfg = self.load_config().ok();
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, (Option<Account>, Option<Account>)> =
            std::collections::HashMap::new();
        // 文件快照（应用 overlay）；稳定 ID 由 AccountManager 在 load 时保证。
        if let Some(c) = cfg.as_ref() {
            for a in c.accounts.iter().filter(|a| a.has_credentials()) {
                let mut a = a.clone();
                if AccountManager::account_removed_in(c, &a) {
                    continue;
                }
                if let Some(o) = overlay.overlay(&a) {
                    a = o;
                }
                let id = a.stable_key();
                groups
                    .entry(id.clone())
                    .and_modify(|g| {
                        if g.0.is_none() {
                            g.0 = Some(a.clone());
                        }
                    })
                    .or_insert_with(|| {
                        order.push(id);
                        (Some(a), None)
                    });
            }
        }
        // 本机实时登录态；能解析到文件稳定 ID 时归同组，否则保持本地身份。
        if let Some(local) = self.local_session() {
            if local.account.has_credentials() {
                let mut a = local.account;
                let removed = cfg
                    .as_ref()
                    .map(|c| AccountManager::account_removed_in(c, &a))
                    .unwrap_or(false);
                if !removed {
                    if let Some(c) = cfg.as_ref() {
                        if let Some(id) = AccountManager::resolve_in(c, &a.identity()) {
                            a.account_id = Some(id.as_str().to_string());
                        }
                    }
                    if let Some(o) = overlay.overlay(&a) {
                        a = o;
                    }
                    let id = a.stable_key();
                    groups
                        .entry(id.clone())
                        .and_modify(|g| {
                            if g.1.is_none() {
                                g.1 = Some(a.clone());
                            }
                        })
                        .or_insert_with(|| {
                            order.push(id);
                            (None, Some(a))
                        });
                }
            }
        }
        finish_account_groups(order, groups)
    }

    /// 账号候选表：剔除已判死账号；本机优先、未冷却优先、过期晚的优先。
    fn candidates(&self) -> Result<Vec<(Account, &'static str)>, String> {
        let mut out: Vec<(Account, &'static str, bool)> = Vec::new();
        for (a, source) in self.all_accounts() {
            if !a.enabled {
                continue;
            }
            let id = a.stable_key();
            let h = self.health_of(&id);
            if h.is_dead(a.revision) {
                continue;
            }
            let cooling = h.cooldown_until.map(|t| t > Instant::now()).unwrap_or(false);
            out.push((a, source, cooling));
        }
        // 本机优先、未冷却优先、过期晚的优先
        out.sort_by_key(|(a, source, cooling)| {
            let src = if *source == "local" { 0 } else { 1 };
            let cool = if *cooling { 1 } else { 0 };
            (src, cool, -a.expires_unix().unwrap_or(0))
        });
        let list: Vec<(Account, &'static str)> =
            out.into_iter().map(|(a, s, _)| (a, s)).collect();
        if list.is_empty() {
            return Err(format!(
                "没有可用账号：本机未登录码道，且 {} 不存在/为空/全部失效",
                self.accounts_path.display()
            ));
        }
        Ok(list)
    }

    /// 过期则尝试续期；返回（可能更新过的）账号 + 续期说明。
    async fn ensure_fresh(&self, account: &Account, source: &str) -> (Account, Option<String>) {
        if account.is_expired() && source == "local" && !self.allow_local_refresh && self.codearts_running() {
            return (
                account.clone(),
                Some("已过期；码道桌面端在运行，跳过主动续期以免踢掉桌面端".to_string()),
            );
        }
        let id = accounts::AccountId(account.stable_key());
        match self
            .accounts
            .ensure_fresh(&id, account.revision, accounts::RefreshMode::IfExpired)
            .await
        {
            Ok(snapshot) => {
                self.reload_refreshed_store();
                let changed = snapshot.revision > account.revision;
                if changed {
                    println!(
                        "codearts-plugin: 账号 {} 续期成功（新过期 {}）",
                        account.display_name(),
                        snapshot.credentials.expires_at.as_deref().unwrap_or("未知")
                    );
                }
                (
                    snapshot.credentials,
                    changed.then(|| "已自动续期".to_string()),
                )
            }
            Err(accounts::RefreshError::Definitive { revision, message }) => {
                self.mark_dead(id.as_str(), revision);
                (account.clone(), Some(message))
            }
            Err(error) => (account.clone(), Some(error.to_string())),
        }
    }

    fn reload_refreshed_store(&self) {
        if let Ok(mut current) = self.refreshed.lock() {
            *current = store::Store::load(&self.overlay_path);
        }
    }

    /// 后台续期周期；与 HTTP 入口共用同一个账号协调器。
    pub async fn refresh_due_accounts(&self) {
        for (account, source) in self.candidates().unwrap_or_default() {
            let _ = self.ensure_fresh(&account, source).await;
        }
    }

    /// 账号导入解析：完整 secretStorage 会话 / 单 session / 普通 Account 三种写法。
    fn parse_import(v: &Value) -> Result<Vec<Account>, String> {
        if v.get("session").is_some() {
            return Ok(vec![import::account_from_value(v)?]);
        }
        let cfg = Config::from_value(v.clone(), std::path::Path::new("import"))?;
        if cfg.accounts.is_empty() {
            return Err("导入内容里没有可用账号".to_string());
        }
        Ok(cfg.accounts)
    }

    /// 把导入的账号合并进账号文件并落盘，返回全量视图。
    fn save_imported(&self, accounts: Vec<Account>) -> Result<Vec<Value>, String> {
        for mut a in accounts {
            if a.name.trim().is_empty() {
                a.name = a.display_name();
            }
            if !a.enabled {
                a.enabled = true;
            }
            self.accounts.upsert(a)?;
        }
        Ok(self
            .all_accounts()
            .iter()
            .map(|(a, s)| self.account_view(a, s))
            .collect())
    }

    /// 强制续期（忽略是否过期）。本机登录态且桌面运行中且未显式允许 → 拒绝。
    async fn force_refresh(&self, account: &Account, source: &str) -> Result<Account, String> {
        if source == "local" && !self.allow_local_refresh && self.codearts_running() {
            return Err("码道桌面端在运行：为避免把桌面端踢下线，默认不主动续期本机登录态（可用 --allow-local-refresh 覆盖）".to_string());
        }
        let id = accounts::AccountId(account.stable_key());
        match self
            .accounts
            .ensure_fresh(&id, account.revision, accounts::RefreshMode::Force)
            .await
        {
            Ok(snapshot) => {
                self.reload_refreshed_store();
                println!(
                    "codearts-plugin: 账号 {} 手动续期成功（新过期 {}）",
                    account.display_name(),
                    snapshot.credentials.expires_at.as_deref().unwrap_or("未知")
                );
                Ok(snapshot.credentials)
            }
            Err(accounts::RefreshError::Definitive { revision, message }) => {
                self.mark_dead(id.as_str(), revision);
                Err(message)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// 账号视图（绝不输出密钥类字段）。
    fn account_view(&self, a: &Account, source: &str) -> Value {
        let id = a.stable_key();
        let legacy = a.identity();
        let h = self.health_of(&id);
        json!({
            "identity": legacy,
            "accountId": id,
            "name": a.display_name(),
            "source": source,
            "enabled": a.enabled,
            "akTail": Config::ak_tail(a),
            "expiresAt": a.expires_at,
            "expired": a.is_expired(),
            "refreshable": a.can_refresh(),
            "dead": h.is_dead(a.revision),
            "cooling": h.cooldown_until.map(|t| t > Instant::now()).unwrap_or(false),
            "failures": h.failures,
            "domainId": a.domain_id,
        })
    }
}

/// 续期失败是否「确定没救」（续期材料作废），而不是网络抖动。
pub(crate) fn is_definitive_refresh_failure(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("http 400")
        || lower.contains("http 401")
        || lower.contains("http 403")
        || lower.contains("invalid_grant")
        || lower.contains("refresh token has been used")
        || lower.contains("invalid refresh token")
}

/// 续期后仍然过期 = 这份凭据发出去必然 400（APIG.0602），跳过它别浪费一次上游调用。
fn expired_account_error(account: &Account, note: &Option<String>) -> UpError {
    UpError::new(
        502,
        "credentials_error",
        format!(
            "账号 {} 登录态已过期且无法续期（{}）",
            account.display_name(),
            note.as_deref().unwrap_or("缺少续期材料")
        ),
    )
}

/// 中国大陆站丢弃 `stop` / `stop_sequences`（上游一个 stop 都不接受），返回是否丢弃了。
/// 国际站（不含 `cn-north-4`）保留原样。
fn strip_unsupported_stop(base: &str, req: &mut Value) -> bool {
    if !base.contains("cn-north-4") {
        return false;
    }
    let Some(obj) = req.as_object_mut() else {
        return false;
    };
    let a = obj.remove("stop").is_some();
    let b = obj.remove("stop_sequences").is_some();
    a || b
}

/// 并发上限错误（`TM.00001041`「并发会话数已达上限(3个)」）：先退避重试同一个号。
fn is_concurrency_error(e: &UpError) -> bool {
    let text = format!("{} {}", e.code, e.message).to_ascii_lowercase();
    text.contains("tm.00001041") || text.contains("并发会话数") || text.contains("concurrent")
}

/// 网关排队应答：`429` + `queue-status: queued` + `trace-id`，取 trace id 供轮询。
fn queued_trace(resp: &reqwest::Response) -> Option<String> {
    let status = resp
        .headers()
        .get("queue-status")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if status != "queued" {
        return None;
    }
    resp.headers()
        .get("trace-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 排队状态是否已放行（`promoted` / `running` / `admitted`）。
pub fn queue_promoted(v: &Value) -> bool {
    for k in ["status", "state", "queueStatus", "queue_status"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            let s = s.trim().to_ascii_lowercase();
            return matches!(s.as_str(), "promoted" | "running" | "admitted" | "active");
        }
    }
    if let Some(d) = v.get("data") {
        return queue_promoted(d);
    }
    v.to_string().to_ascii_lowercase().contains("\"promoted\"")
}

/// 可重试（换号）的上游错误：限流、5xx、并发上限、网络。
fn retryable(e: &UpError) -> bool {
    e.status == 429
        || e.status >= 500
        || e.code.contains("TM.00001041")
        || e.message.contains("并发")
        || e.code == "network"
}

/// 统一 CORS（GUI 页面跑在 localhost:随机端口，fetch 127.0.0.1 属跨域）。
async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        add_cors(resp.headers_mut());
        return resp;
    }
    let mut resp = next.run(req).await;
    add_cors(resp.headers_mut());
    resp
}

fn add_cors(h: &mut HeaderMap) {
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
}

pub fn router(state: Arc<Server>) -> Router {
    Router::new()
        .route("/health", any(handle_health))
        .route("/v1/models", any(handle_models))
        .route("/v1/chat/completions", any(handle_chat))
        .route("/v1/quota", any(handle_quota))
        .route("/accounts", any(handle_accounts))
        .route("/activity", any(handle_activity))
        .route("/activity/claim", any(handle_activity_claim))
        .route("/accounts/import", any(handle_import))
        .route("/models/benefit", any(handle_benefit))
        .route("/accounts/import-local", any(handle_import_local))
        .route("/login/start", any(handle_login_start))
        .route("/login/status", any(handle_login_status))
        .route("/login/complete", any(handle_login_complete))
        .route("/login/list", any(handle_login_list))
        .route("/accounts/refresh", any(handle_account_refresh))
        .route("/v1/messages", any(handle_messages))
        .route("/v1/responses", any(handle_responses))
        .route("/accounts/enable", any(handle_enable))
        .route("/accounts/remove", any(handle_remove))
        .fallback(any(handle_not_found))
        .layer(axum::middleware::from_fn(cors))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

fn error_response(status: u16, message: &str, code: &str) -> Response {
    let (s, body) = upstream::openai_error(status, message, code);
    json_response(StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY), &body)
}

/// 上游错误 → 对外响应。凭据类错误固定 502 并给出可操作提示。
fn up_error_response(e: &UpError) -> Response {
    // 本地预检的 10 MiB 上限（上游回的是误导性的「参数非法」）。
    if e.code == "request_too_large" {
        return error_response(413, &e.message, "request_too_large");
    }
    // 先认「请求体超限」：网关把它伪装成 APIG.0301 签名失败，真拿它当凭据问题
    // 会误导用户去重新登录（实际是上下文/图片太大）。
    if upstream::is_body_too_large(e) {
        return error_response(
            413,
            "请求体过大：码道上游单次请求上限约 15 MiB，当前上下文（通常是历史截图）已超限。\
             请压缩上下文或开新会话；纯文字会话不受影响。",
            "body_too_large",
        );
    }
    if e.credential {
        return error_response(
            502,
            &format!(
                "码道登录态失效，请重新打开码道桌面端登录（{}）",
                e.message
            ),
            "credentials_error",
        );
    }
    error_response(e.status, &e.message, &e.code)
}

// ---------------------------------------------------------------------------
// /health
// ---------------------------------------------------------------------------

async fn handle_health(State(st): State<Arc<Server>>) -> Response {
    let cfg = st.load_config();
    let all = st.all_accounts();
    let mut pool = Vec::new();
    for (a, source) in &all {
        let id = a.stable_key();
        let h = st.health_of(&id);
        let cooling = h.cooldown_until.map(|t| t > Instant::now()).unwrap_or(false);
        pool.push(json!({
            "account": a.display_name(),
            "source": source,
            "expired": a.is_expired(),
            "refreshable": a.can_refresh(),
            "dead": h.is_dead(a.revision),
            "cooling": cooling,
            "failures": h.failures,
        }));
    }
    // 甲板顶部显示的必须是「真正会被拿去发请求的账号」。早先这里按文件顺序挑第一个
    // 未判死账号，结果长期显示一份早已过期的存档快照，与真实在用的本机登录态不符
    // （2026-11-05 实测：顶部写 hw030085737321632/11-04 过期，实际跑的是本机号）。
    let cands = st.candidates().unwrap_or_default();
    let first: Option<(Account, &'static str)> = cands
        .iter()
        .find(|(a, _)| !a.is_expired())
        .cloned()
        .or_else(|| cands.first().cloned());
    let models = st
        .models
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|m| m.list.len()))
        .unwrap_or(0);
    let mut body = json!({
        "status": "ok",
        "service": "codearts",
        "uptimeSec": st.started.elapsed().as_secs(),
        "ready": first.is_some(),
        "source": first.as_ref().map(|(_, s)| *s).unwrap_or("none"),
        "channel": cfg.as_ref().map(|c| c.channel.clone()).unwrap_or_else(|_| config::DEFAULT_CHANNEL.to_string()),
        "models": models,
        "version": env!("CARGO_PKG_VERSION"),
        "codeartsRunning": st.codearts_running(),
        "localRefreshAllowed": st.allow_local_refresh,
        "pool": pool,
    });
    match first {
        Some((a, _)) => {
            body["account"] = Value::String(a.display_name());
            body["expiresAt"] = a
                .expires_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null);
            body["expired"] = Value::Bool(a.is_expired());
            body["refreshable"] = Value::Bool(a.can_refresh());
        }
        None => {
            body["reason"] = Value::String(
                cfg.as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_else(|| {
                        format!(
                            "没有可用（未判死）账号；已发现 {} 个账号但全部失效",
                            all.len()
                        )
                    }),
            );
        }
    }
    json_response(StatusCode::OK, &body)
}

// ---------------------------------------------------------------------------
// /v1/models
// ---------------------------------------------------------------------------

async fn handle_models(State(st): State<Arc<Server>>) -> Response {
    let accounts = match st.candidates() {
        Ok(v) => v,
        Err(e) => return error_response(503, &e, "no_account"),
    };
    let mut last: Option<UpError> = None;
    for (account, source) in accounts.into_iter().take(st.max_attempts.max(1)) {
        let (account, note) = st.ensure_fresh(&account, source).await;
        if account.is_expired() {
            // 续期没救回来：换成可读的 502，别再拿死令牌打上游。
            last = Some(expired_account_error(&account, &note));
            continue;
        }
        if let Ok(cache) = st.models.lock() {
            if let Some(c) = cache.as_ref() {
                if c.access_key == account.access_key && c.at.elapsed() < MODELS_TTL {
                    return models_response(&c.list);
                }
            }
        }
        match st.up.builtin_models(&account).await {
            Ok(list) => {
                st.mark_ok(&account);
                // 合并限时福利模型（每账号独立目录；benefit=true 的条目聊天要带 maas_type）。
                let mut merged = list.clone();
                if let Some(bm) = st.ensure_benefit(&account).await {
                    for m in bm {
                        if !merged.iter().any(|x| x.id.eq_ignore_ascii_case(&m.id)) {
                            merged.push(m);
                        }
                    }
                }
                // 过滤掉本账号已知不可用的模型（002002009 / 未领取福利）。
                merged.retain(|m| st.model_available(&account.stable_key(), &m.id));
                if let Ok(mut cache) = st.models.lock() {
                    *cache = Some(ModelsCache {
                        at: Instant::now(),
                        access_key: account.access_key.clone(),
                        list: merged.clone(),
                    });
                }
                return models_response(&merged);
            }
            Err(e) => {
                if e.credential {
                    st.note_credential_failure(&account, source);
                } else if retryable(&e) {
                    st.mark_cooling(&account.stable_key(), 30);
                } else {
                    return up_error_response(&e);
                }
                last = Some(e);
            }
        }
    }
    up_error_response(
        &last.unwrap_or_else(|| UpError::new(503, "no_account", "没有可用码道账号")),
    )
}

fn models_response(list: &[ModelInfo]) -> Response {
    let created = config::now_unix();
    let mut data: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in list.iter().filter(|m| m.enable) {
        let entry = json!({
            "id": m.id,
            "object": "model",
            "created": created,
            "owned_by": "codearts",
            "name": m.name,
            "description": m.desc,
            "context_window": m.context_window,
            "max_tokens": m.max_tokens,
            "supports_images": m.supports_images,
            "benefit": m.benefit,
        });
        if seen.insert(m.id.clone()) {
            data.push(entry.clone());
        }
        // 官方只给精确 ID；社区实现额外补小写别名（聊天时由 canonical_model 归一回精确 ID）。
        let lower = m.id.to_lowercase();
        if lower != m.id && seen.insert(lower.clone()) {
            let mut alias = entry;
            alias["id"] = Value::String(lower);
            alias["alias_of"] = Value::String(m.id.clone());
            data.push(alias);
        }
    }
    json_response(
        StatusCode::OK,
        &json!({ "object": "list", "data": data }),
    )
}

// ---------------------------------------------------------------------------
// /v1/chat/completions
// ---------------------------------------------------------------------------

async fn handle_chat(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let mut req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json")
        }
    };
    if let Some(m) = req.get("model").and_then(|x| x.as_str()) {
        let canon = st.canonical_model(m.trim());
        req["model"] = Value::String(canon);
    }
    chat_core(st, req).await
}

/// `POST /v1/messages`：Anthropic 协议 → 内部 chat。
async fn handle_messages(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json"),
    };
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let stream = v.get("stream").and_then(|x| x.as_bool()).unwrap_or(false);
    let mut chat = match protocols::anthropic_to_chat(&v) {
        Ok(c) => c,
        Err(e) => return error_response(400, &e, "invalid_request"),
    };
    let canon = st.canonical_model(&model);
    chat["model"] = Value::String(canon.clone());
    chat["stream"] = Value::Bool(stream);
    let resp = chat_core(st, chat).await;
    if !resp.status().is_success() {
        return resp; // 上游错误按 JSON 原样返回，别伪装成 SSE 流
    }
    if !resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false)
    {
        return resp; // 非 SSE（例如上游直接回整包 JSON）也原样透传
    }
    if stream {
        let (mut parts, body) = resp.into_parts();
        parts.headers.insert(
            header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        parts.headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
        return Response::from_parts(parts, protocols::anthropic_stream(body, canon));
    }
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap_or_default();
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v2) if status.is_success() => {
            json_response(StatusCode::OK, &protocols::chat_to_anthropic(&v2, &canon))
        }
        _ => {
            let mut out = Response::new(axum::body::Body::from(bytes));
            *out.status_mut() = status;
            out.headers_mut().insert(
                header::CONTENT_TYPE,
                "application/json; charset=utf-8".parse().unwrap(),
            );
            out
        }
    }
}

/// `POST /v1/responses`：OpenAI Responses 协议 → 内部 chat。
async fn handle_responses(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json"),
    };
    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let stream = v.get("stream").and_then(|x| x.as_bool()).unwrap_or(false);
    let mut chat = match protocols::responses_to_chat(&v) {
        Ok(c) => c,
        Err(e) => return error_response(400, &e, "invalid_request"),
    };
    let canon = st.canonical_model(&model);
    chat["model"] = Value::String(canon.clone());
    chat["stream"] = Value::Bool(stream);
    let resp = chat_core(st, chat).await;
    if !resp.status().is_success() {
        return resp; // 上游错误按 JSON 原样返回，别伪装成 SSE 流
    }
    if !resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false)
    {
        return resp; // 非 SSE（例如上游直接回整包 JSON）也原样透传
    }
    if stream {
        let (mut parts, body) = resp.into_parts();
        parts.headers.insert(
            header::CONTENT_TYPE,
            "text/event-stream; charset=utf-8".parse().unwrap(),
        );
        parts.headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
        return Response::from_parts(parts, protocols::responses_stream(body, canon));
    }
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap_or_default();
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v2) if status.is_success() => {
            json_response(StatusCode::OK, &protocols::chat_to_responses(&v2, &canon))
        }
        _ => {
            let mut out = Response::new(axum::body::Body::from(bytes));
            *out.status_mut() = status;
            out.headers_mut().insert(
                header::CONTENT_TYPE,
                "application/json; charset=utf-8".parse().unwrap(),
            );
            out
        }
    }
}

/// 已规整/校验过的 chat 请求（/v1/chat/completions 与协议适配器共用）。
async fn chat_core(st: Arc<Server>, mut req: Value) -> Response {
    let model = match req.get("model").and_then(|x| x.as_str()) {
        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
        _ => return error_response(400, "缺少 model 字段", "invalid_request"),
    };
    if req.get("messages").and_then(|x| x.as_array()).is_none() {
        return error_response(400, "缺少 messages 数组", "invalid_request");
    }
    // 上游对空 role 直接 400（InferHub.001001044）；对齐参考实现做规整。
    if let Err(e) = protocol::normalize_messages(&mut req) {
        return error_response(400, &e, "invalid_request");
    }
    // 工具参数里的 `$ref`/`$defs` 上游一律不认（Moonshot 风格校验器会判「无限递归」，
    // 整轮 400）。内联掉再发；没有这类工具的请求原样不动。
    let inlined = toolrefs::sanitize_tool_refs(&mut req);
    if inlined > 0 {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            eprintln!(
                "codearts-plugin: 已内联工具 schema 的 $ref（本请求 {inlined} 个工具；上游不认 $defs）"
            );
        });
    }
    let wants_stream = req
        .get("stream")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let cfg = st.load_config().unwrap_or_default();

    // 中国大陆站不接受 `stop`（magpie 0.3.19 实测：mainland 一个 stop 都不收）。
    // 客户端带了就本地剥掉，避免整轮 400；国际站保留。
    strip_unsupported_stop(st.up.base(), &mut req);

    // 上游一律流式（glm-5.3 只支持流式）；非流式在本地聚合。
    req["model"] = Value::String(model.clone());
    req["stream"] = Value::Bool(true);
    if req.get("prompt_cache_key").is_none() {
        req["prompt_cache_key"] = Value::String(format!("codearts-{}", upstream::random_hex(12)));
    }
    let payload = match serde_json::to_vec(&req) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("序列化请求失败: {e}"), "invalid_json"),
    };

    let accounts = match st.candidates() {
        Ok(v) => v,
        Err(e) => return error_response(503, &e, "no_account"),
    };
    let mut last: Option<UpError> = None;
    let mut benefit_claimed_once = false; // 福利未领：本轮只补领一次
    for (account, source) in accounts.into_iter().take(st.max_attempts.max(1)) {
        let (account, note) = st.ensure_fresh(&account, source).await;
        if account.is_expired() {
            // 续期没救回来：换成可读的 502，别再拿死令牌打上游。
            last = Some(expired_account_error(&account, &note));
            continue;
        }
        // 单账号并发闸门：上游按账号统计并发（上限 3），超了回 429/并发错误。
        // 许可在本轮请求返回后释放（流式响应会提前释放，属近似限流，足够压住突发）。
        let sem = st.permit(&account.stable_key());
        let _permit = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            sem.acquire_owned(),
        )
        .await
        {
            Ok(Ok(p)) => p,
            _ => {
                last = Some(UpError::new(429, "busy", "账号并发已满，稍后重试"));
                continue;
            }
        };
        st.touch_account(&account.stable_key());
        let benefit = cfg.is_benefit() || st.is_benefit_model(&account.stable_key(), &model);
        let session_id = upstream::session_id_for(&account);
        // 内层循环：并发上限先退避重试**同一个号**，网关排队先等 promoted 再重发。
        // 都不成就按原规则交给外层换号。
        let mut conc_retry = 0usize;
        let mut queue_waited = false;
        let terminal: Option<UpError> = loop {
            let resp = match st
                .up
                .chat(&account, &model, payload.clone(), benefit, &session_id)
                .await
            {
                Ok(r) => r,
                Err(e) => break Some(e),
            };
            let status = resp.status().as_u16();
            // 网关排队：429 + `queue-status: queued` + `trace-id`，等 promoted 后重发一次。
            if status == 429 && !queue_waited {
                if let Some(trace) = queued_trace(&resp) {
                    queue_waited = true;
                    println!(
                        "[codearts] 账号 {} 被网关排队（trace {trace}），等待放行",
                        account.display_name()
                    );
                    if st.await_queue(&account, &trace).await {
                        println!("[codearts] 已放行（trace {trace}），重发原请求");
                        continue;
                    }
                    println!("[codearts] 排队超时（trace {trace}），放弃");
                }
            }

            // HTTP 状态、HTTP200 JSON、HTTP200 首 SSE/非流式聚合错误都在提交前转成 UpError。
            let error = match response::prepare_response(resp, wants_stream).await {
                Ok(prepared) => {
                    st.mark_ok(&account);
                    return prepared.for_model(&model).into_response();
                }
                Err(error) => error,
            };
            let text = format!("{} {}", error.code, error.message);
            // 模型级不可用（002002009 not registered）：记 30 分钟黑名单，避免反复撞。
            if is_model_missing_body(&text) {
                st.mark_model_unavailable(&account.stable_key(), &model, 1800);
            }
            // 福利模型未领（4004.200 benefit not found）：补一次 claim 后按原错误策略处理。
            if is_benefit_missing_body(&text) && !benefit_claimed_once {
                benefit_claimed_once = true;
                println!(
                    "[codearts] 账号 {} 福利未领（benefit not found），补领后按原错误返回",
                    account.display_name()
                );
                let _ = st.up.claim_benefit(&account).await;
                let _ = st.ensure_benefit(&account).await;
                break Some(error);
            }
            // 并发上限（TM.00001041）：槽位按时间租约释放（约 1 分钟），换号只是把
            // 问题搬到别的号上——先按 2/4/6/8/10 秒退避重试同一个号。
            if is_concurrency_error(&error) && conc_retry < CONCURRENCY_RETRY_SECS.len() {
                let secs = CONCURRENCY_RETRY_SECS[conc_retry];
                conc_retry += 1;
                println!(
                    "[codearts] 账号 {} 并发已满，{secs}s 后重试（第 {conc_retry} 次）",
                    account.display_name()
                );
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                continue;
            }
            break Some(error);
        };
        let Some(e) = terminal else { continue };
        if e.credential {
            // 账号失效：本轮直接标死，换下一个
            st.note_credential_failure(&account, source);
            last = Some(e);
            continue;
        }
        if retryable(&e) {
            st.mark_cooling(&account.stable_key(), 30);
            last = Some(e);
            continue;
        }
        return up_error_response(&e);
    }
    up_error_response(
        &last.unwrap_or_else(|| UpError::new(503, "no_account", "没有可用码道账号")),
    )
}

// ---------------------------------------------------------------------------
// /v1/quota
// ---------------------------------------------------------------------------

async fn handle_quota(State(st): State<Arc<Server>>) -> Response {
    let accounts = match st.candidates() {
        Ok(v) => v,
        Err(e) => return error_response(503, &e, "no_account"),
    };
    let mut last: Option<UpError> = None;
    for (account, source) in accounts.into_iter().take(st.max_attempts.max(1)) {
        let (account, note) = st.ensure_fresh(&account, source).await;
        if account.is_expired() {
            // 续期没救回来：换成可读的 502，别再拿死令牌打上游。
            last = Some(expired_account_error(&account, &note));
            continue;
        }
        match st.up.statistics(&account).await {
            Ok(raw) => {
                st.mark_ok(&account);
                let (total, used, remain, percent) = parse_credit(&raw);
                return json_response(
                    StatusCode::OK,
                    &json!({
                        "account": account.display_name(),
                        "source": source,
                        "credit": {
                            "total": total,
                            "used": used,
                            "remain": remain,
                            "percent": percent,
                        },
                        "raw": raw,
                    }),
                );
            }
            Err(e) => {
                if e.credential {
                    st.note_credential_failure(&account, source);
                } else if retryable(&e) {
                    st.mark_cooling(&account.stable_key(), 30);
                } else {
                    return up_error_response(&e);
                }
                last = Some(e);
            }
        }
    }
    up_error_response(
        &last.unwrap_or_else(|| UpError::new(503, "no_account", "没有可用码道账号")),
    )
}

/// 从 `metrics[name=usageTotalPackageCredit]` 取积分额度。
fn parse_credit(raw: &Value) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    let metric = raw
        .get("metrics")
        .and_then(|m| m.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|m| m.get("name").and_then(|n| n.as_str()) == Some("usageTotalPackageCredit"))
        });
    let Some(m) = metric else {
        return (None, None, None, None);
    };
    let num = |k: &str| m.get(k).and_then(|v| v.as_f64());
    let total = num("package_credit_amount");
    let used = num("package_credit_used");
    let remain = num("package_credit_remain");
    let percent = remain.zip(total).and_then(|(r, t)| {
        if t > 0.0 {
            Some((r / t * 100.0 * 100.0).round() / 100.0)
        } else {
            None
        }
    });
    (total, used, remain, percent)
}

/// `POST /login/start`：起本地回调服务，返回华为云授权链接（PKCE + DPoP）。
async fn handle_login_start(State(st): State<Arc<Server>>) -> Response {
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => return error_response(500, &format!("回调端口绑定失败: {e}"), "login_bind"),
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => return error_response(500, &format!("回调端口读取失败: {e}"), "login_bind"),
    };
    let id = upstream::random_hex(8);
    let ticket_id = upstream::random_hex(16);
    let (verifier, challenge) = login::pkce();
    let jwk = match login::new_dpop_jwk() {
        Ok(j) => j,
        Err(e) => return error_response(500, &e, "login_key"),
    };
    let url = login::authorize_url(&ticket_id, &challenge, port);
    {
        let mut g = match st.logins.lock() {
            Ok(g) => g,
            Err(e) => return error_response(500, &e.to_string(), "login_state"),
        };
        // 清掉过期的登录会话，避免长期堆积。
        g.retain(|_, p| p.created.elapsed() < login::LOGIN_TTL);
        g.insert(
            id.clone(),
            login::Pending {
                url: url.clone(),
                port,
                verifier,
                ticket_id: ticket_id.clone(),
                jwk,
                created: Instant::now(),
                code: None,
                secret: None,
                done: false,
                error: None,
                account: None,
            },
        );
    }
    let st_cb = st.clone();
    let id_cb = id.clone();
    tokio::spawn(async move { login::serve_callback(listener, st_cb, id_cb).await });
    let st_worker = st.clone();
    let id_worker = id.clone();
    tokio::spawn(async move { login::finish_worker(st_worker, id_worker).await });
    json_response(
        StatusCode::OK,
        &login::start_json(&id, &url, port, &challenge, &ticket_id),
    )
}

/// `GET /login/status?id=`：查询登录结果（done/error/account）。
async fn handle_login_status(
    State(st): State<Arc<Server>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let id = query
        .as_deref()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                let mut it = kv.splitn(2, '=');
                match (it.next(), it.next()) {
                    (Some("id"), Some(v)) if !v.is_empty() => Some(v.to_string()),
                    _ => None,
                }
            })
        })
        .unwrap_or_default();
    let guard = match st.logins.lock() {
        Ok(g) => g,
        Err(e) => return error_response(500, &e.to_string(), "login_state"),
    };
    match guard.get(&id) {
        Some(p) => json_response(StatusCode::OK, &p.status_json(&id)),
        None => error_response(404, "没有这个登录会话（可能已过期）", "login_not_found"),
    }
}

/// `GET /login/list`：列出所有待完成/已完成的登录。
async fn handle_login_list(State(st): State<Arc<Server>>) -> Response {
    let guard = match st.logins.lock() {
        Ok(g) => g,
        Err(e) => return error_response(500, &e.to_string(), "login_state"),
    };
    let items: Vec<Value> = guard.iter().map(|(k, v)| v.status_json(k)).collect();
    json_response(StatusCode::OK, &json!({ "logins": items }))
}

/// `POST /login/complete`：远程部署时把浏览器回调 URL 里的 code 手动提交（body: {id, code} 或 {id, url}）。
async fn handle_login_complete(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json"),
    };
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut code = v.get("code").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if code.is_empty() {
        if let Some(u) = v.get("url").and_then(|x| x.as_str()) {
            for kv in u.split('&') {
                let mut it = kv.splitn(2, '=');
                if let (Some("code"), Some(val)) = (it.next(), it.next()) {
                    code = val.to_string();
                }
            }
        }
    }
    if id.is_empty() || code.is_empty() {
        return error_response(400, "缺少 id 或 code", "invalid_request");
    }
    st.set_login_code(&id, &code);
    json_response(StatusCode::OK, &json!({ "ok": true, "id": id }))
}

/// `/models/benefit`：诊断用——对首个可用账号做一次福利领取 + 目录拉取，返回原始结果。
async fn handle_benefit(State(st): State<Arc<Server>>) -> Response {
    let accounts = match st.candidates() {
        Ok(v) => v,
        Err(e) => return error_response(503, &e, "no_account"),
    };
    let Some((account, _src)) = accounts.into_iter().next() else {
        return error_response(503, "没有可用账号", "no_account");
    };
    let claim_error = st.up.claim_benefit(&account).await.err().map(|e| e.message);
    match st.up.benefit_models(&account).await {
        Ok(models) => json_response(
            StatusCode::OK,
            &json!({
                "account": account.display_name(),
                "claim_error": claim_error,
                "count": models.len(),
                "models": models.iter().map(|m| json!({ "id": m.id, "name": m.name, "benefit": m.benefit })).collect::<Vec<_>>(),
            }),
        ),
        Err(e) => json_response(
            StatusCode::BAD_GATEWAY,
            &json!({ "account": account.display_name(), "claim_error": claim_error, "error": e.message, "code": e.code }),
        ),
    }
}

/// `/activity`：活动积分账本（签到/新手/邀请…）与自动领取开关。
async fn handle_activity(
    State(st): State<Arc<Server>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let now = config::now_unix();
    let day = activity::local_day_utc8(now);
    let mut accounts = Vec::new();
    for (a, src) in st.all_accounts() {
        let id = a.stable_key();
        let e = st
            .ledger
            .lock()
            .map(|l| l.entry(&id))
            .unwrap_or_default();
        accounts.push(json!({
            "identity": id,
            "account": a.display_name(),
            "source": src,
            "enabled": a.enabled,
            "expired": a.is_expired(),
            "last_run": e.last_run,
            "last_message": e.last_message,
            "last_credits": e.last_credits,
            "settled": e.settled,
            "login_settled_today": st.login_settled_today(&a.stable_key()),
        }));
    }
    // `?campaigns=1`：只读预览各账号活动列表（不领取，便于排障）。
    let mut campaigns = Vec::new();
    if query.as_deref().map(|q| q.contains("campaigns=1")).unwrap_or(false) {
        for (a, _src) in st.all_accounts() {
            if !a.enabled || !a.has_credentials() {
                continue;
            }
            match st.up.campaigns(&a).await {
                Ok(list) => campaigns.push(json!({
                    "account": a.display_name(),
                    "ok": true,
                    "items": list.iter().map(|c| json!({
                        "id": c.id_text(), "type": c.kind, "title": c.title,
                        "status": c.status, "claimable": c.claimable,
                        "channel": c.channel,
                        "unit": c.unit, "amount": c.amount,
                        "start": c.start, "end": c.end,
                    })).collect::<Vec<_>>(),
                })),
                Err(e) => campaigns.push(json!({
                    "account": a.display_name(), "ok": false, "message": e.message,
                })),
            }
        }
    }

    json_response(
        StatusCode::OK,
        &json!({
            "day": day,
            "auto": activity_auto_enabled(),
            "watchdog": watchdog_enabled(),
            "accounts": accounts,
            "campaigns": campaigns,
        }),
    )
}

/// `POST /activity/claim`：立即对所有账号（或 body.identity 指定一个）跑一轮活动领取。
async fn handle_activity_claim(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let only = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("identity").and_then(|x| x.as_str()).map(str::to_string));
    let results = st.run_activities(only).await;
    json_response(StatusCode::OK, &json!({ "results": results }))
}

fn activity_auto_enabled() -> bool {
    std::env::var("CODEARTS_ACTIVITY_AUTO").map(|v| v != "0").unwrap_or(true)
}

fn watchdog_enabled() -> bool {
    std::env::var("CODEARTS_WATCHDOG").map(|v| v != "0").unwrap_or(true)
}

async fn handle_not_found() -> Response {
    error_response(404, "not found", "not_found")
}

// ---------------------------------------------------------------------------
// 账号管理：GET /accounts，POST /accounts/{import,import-local,refresh,enable}
// ---------------------------------------------------------------------------

/// 账号池清单（密钥一律只露 AK 尾段）。
async fn handle_accounts(State(st): State<Arc<Server>>) -> Response {
    let list: Vec<Value> = st
        .all_accounts()
        .iter()
        .map(|(a, s)| st.account_view(a, s))
        .collect();
    json_response(
        StatusCode::OK,
        &json!({
            "accounts": list,
            "codeartsRunning": st.codearts_running(),
            "localRefreshAllowed": st.allow_local_refresh,
            "accountsPath": st.accounts_path.display().to_string(),
        }),
    )
}

/// 导入账号：接受完整 secretStorage 会话 / 单 session / 普通 Account 三种写法。
async fn handle_import(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json"),
    };
    let accounts = match Server::parse_import(&v) {
        Ok(a) => a,
        Err(e) => return error_response(400, &e, "invalid_request"),
    };
    match st.save_imported(accounts) {
        Ok(list) => json_response(StatusCode::OK, &json!({ "accounts": list })),
        Err(e) => error_response(500, &e, "save_failed"),
    }
}

/// 把当前本机登录态快照一份进账号文件（桌面端可换号登录再导，实现多号池）。
async fn handle_import_local(State(st): State<Arc<Server>>) -> Response {
    if !st.allow_local {
        return error_response(400, "本机导入已用 --no-local 关闭", "local_disabled");
    }
    let Some(local) = st.local_session() else {
        return error_response(503, "未找到本机码道登录态（桌面端尚未登录）", "no_local");
    };
    if !local.account.has_credentials() {
        return error_response(503, "本机登录态不完整", "no_local");
    }
    let mut a = local.account;
    if a.name.trim().is_empty() {
        a.name = a.display_name();
    }
    match st.save_imported(vec![a]) {
        Ok(list) => json_response(StatusCode::OK, &json!({ "accounts": list })),
        Err(e) => error_response(500, &e, "save_failed"),
    }
}

/// 对单个账号强制续期（忽略过期时间）。
async fn handle_account_refresh(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json"),
    };
    let identity = v.get("identity").and_then(|x| x.as_str()).unwrap_or("");
    if identity.is_empty() {
        return error_response(400, "缺少 identity", "invalid_request");
    }
    let Some(account_id) = st.accounts.resolve_id(identity) else {
        return error_response(404, "找不到该账号", "not_found");
    };
    let Some((account, source)) = st
        .all_accounts()
        .into_iter()
        .find(|(a, _)| a.stable_key() == account_id.as_str())
    else {
        return error_response(404, "找不到该账号", "not_found");
    };
    match st.force_refresh(&account, source).await {
        Ok(new) => json_response(
            StatusCode::OK,
            &json!({ "account": st.account_view(&new, source) }),
        ),
        Err(e) => error_response(502, &e, "refresh_failed"),
    }
}

/// 启用/停用账号文件里的账号（本机登录态不受影响，用 --no-local 关）。
async fn handle_enable(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json"),
    };
    let identity = v.get("identity").and_then(|x| x.as_str()).unwrap_or("");
    let enabled = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true);
    if identity.is_empty() {
        return error_response(400, "缺少 identity", "invalid_request");
    }
    let Some(account_id) = st.accounts.resolve_id(identity) else {
        return error_response(
            404,
            "账号文件里没有该账号（本机登录态只能整体用 --no-local 关闭）",
            "not_found",
        );
    };
    if let Err(e) = st.accounts.set_enabled(&account_id, enabled) {
        return error_response(500, &e, "save_failed");
    }
    json_response(StatusCode::OK, &json!({ "identity": identity, "accountId": account_id, "enabled": enabled }))
}

/// 删除账号文件里的账号快照（本机登录态不受影响）。
async fn handle_remove(State(st): State<Arc<Server>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return error_response(400, &format!("请求体不是合法 JSON: {e}"), "invalid_json"),
    };
    let identity = v.get("identity").and_then(|x| x.as_str()).unwrap_or("");
    if identity.is_empty() {
        return error_response(400, "缺少 identity", "invalid_request");
    }
    let Some(account_id) = st.accounts.resolve_id(identity) else {
        return error_response(404, "账号文件里没有该账号", "not_found");
    };
    if let Err(e) = st.accounts.remove(&account_id) {
        return error_response(500, &e, "save_failed");
    }
    json_response(StatusCode::OK, &json!({ "identity": identity, "accountId": account_id, "removed": true }))
}

pub async fn run(args: Args) -> Result<(), String> {
    let accounts_path = args
        .accounts
        .clone()
        .unwrap_or_else(|| datadir::path(datadir::ACCOUNTS_FILE));
    let up = Upstream::new(&args.upstream)?;
    println!(
        "codearts-plugin: starting on {}:{} (upstream={})",
        args.host, args.port, args.upstream
    );
    println!(
        "codearts-plugin: 账号文件 {}（存在={}）；本机登录态自动导入={}；本机主动续期={}",
        accounts_path.display(),
        accounts_path.exists(),
        if args.no_local { "关" } else { "开" },
        if args.allow_local_refresh { "开" } else { "关" }
    );
    let state = Server::new(
        up,
        accounts_path,
        !args.no_local,
        args.sts_url,
        args.allow_local_refresh,
        args.max_attempts,
    )?;
    if watchdog_enabled() {
        let bg = state.clone();
        tokio::spawn(async move { background_loop(bg).await });
    }
    let app = router(state);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(|e| format!("监听地址非法 {}:{}: {e}", args.host, args.port))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {} 被占用: {e}", args.port))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务退出: {e}"))
}

/// 模型级不可用（上游说没注册）：`InferHub.002002009.404 model is not registered`。
fn is_model_missing_body(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    l.contains("002002009") || l.contains("not registered") || l.contains("model_not_found")
}

/// 限时福利未领取：`InferHub.4004.200 benefit not found`。
fn is_benefit_missing_body(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    text.contains("4004.200") || l.contains("benefit not found")
}

/// 从 claim 响应里取本次发放的积分（多字段兜底，最后用活动标称额）。
fn claim_credits(v: &Value, c: &activity::Campaign) -> f64 {
    if let Some(d) = v.get("data") {
        for k in ["benefitAmount", "credit", "credits", "creditAmount", "amount", "pendingTotalAmount"] {
            if let Some(n) = d.get(k).and_then(|x| x.as_f64()) {
                if n > 0.0 {
                    return n;
                }
            }
        }
    }
    if c.pending > 0.0 {
        c.pending
    } else {
        c.amount
    }
}

/// 看门狗：续期、保活、活动领取（每 60 秒一拍；`CODEARTS_WATCHDOG=0` 关闭）。
async fn background_loop(st: Arc<Server>) {
    use tokio::time::{interval, Duration as TD};
    let mut tick = interval(TD::from_secs(60));
    let mut last_activity = Instant::now() - Duration::from_secs(3600);
    loop {
        tick.tick().await;
        let now = config::now_unix();
        let hour = activity::local_hour_utc8(now);
        // 活动：启动即跑一次，之后 30 分钟一轮；9/21 点额外兜底一次。
        if activity_auto_enabled()
            && (last_activity.elapsed() >= Duration::from_secs(1800) || hour == 9 || hour == 21)
        {
            last_activity = Instant::now();
            let results = st.run_activities(None).await;
            let claimed: f64 = results
                .iter()
                .map(|r| r.get("credits").and_then(|x| x.as_f64()).unwrap_or(0.0))
                .sum();
            if claimed > 0.0 {
                println!("[codearts] 活动积分：本轮合计 +{claimed}");
            }
        }
        // 续期：把 30 分钟内到期的号提前续掉（失败按既有规则标死）。
        st.refresh_due_accounts().await;
        // 保活：10 分钟没动静的号打一次积分接口（签名成本极低）。
        for (a, _src) in st.all_accounts() {
            if !a.enabled || !a.has_credentials() {
                continue;
            }
            let idle = st
                .idle_for(&a.stable_key())
                .map(|d| d >= Duration::from_secs(600))
                .unwrap_or(true);
            if idle && st.up.statistics(&a).await.is_ok() {
                st.touch_account(&a.stable_key());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-05 线上原文：四份存档快照的 refresh token 全被用过，每次续期都是这条。
    #[test]
    fn spent_refresh_token_is_definitive() {
        assert!(is_definitive_refresh_failure(
            "续期失败 HTTP 400 Bad Request: invalid refresh token: 'the refresh token has been used'"
        ));
        assert!(is_definitive_refresh_failure("续期失败 HTTP 401 Unauthorized"));
        assert!(is_definitive_refresh_failure("invalid_grant: bad code_verifier"));
    }

    /// 网络抖动 / 上游 5xx 不能把账号标死，下轮还要再试。
    #[test]
    fn transient_refresh_failure_is_not_definitive() {
        assert!(!is_definitive_refresh_failure("请求失败: error sending request for url"));
        assert!(!is_definitive_refresh_failure("续期失败 HTTP 503 Service Unavailable"));
        assert!(!is_definitive_refresh_failure("续期失败 HTTP 502 Bad Gateway"));
    }

    /// 续期没救回来的账号：给客户端 502 + 人话，而不是上游 400 原文。
    #[test]
    fn expired_account_error_is_credential_502() {
        let e = expired_account_error(&Account::default(), &Some("续期失败: HTTP 400".into()));
        assert_eq!(e.status, 502);
        assert!(e.credential);
        assert!(e.message.contains("登录态已过期"), "{}", e.message);
    }

    #[test]
    fn model_error_classifiers() {
        assert!(is_model_missing_body(
            r#"{"error_code":"InferHub.002002009.404","error_msg":"model is not registered"}"#
        ));
        assert!(is_model_missing_body("model_not_found"));
        assert!(!is_model_missing_body("bad request"));
        assert!(is_benefit_missing_body(
            r#"{"error_code":"InferHub.4004.200","error_msg":"benefit not found"}"#
        ));
        assert!(is_benefit_missing_body("benefit not found"));
        assert!(!is_benefit_missing_body("plain error"));
    }

    #[test]
    fn claim_credits_prefers_response_then_campaign_amount() {
        let c = activity::Campaign {
            id: serde_json::json!(1),
            channel: "IDE".into(),
            title: "t".into(),
            kind: "USER_LOGIN".into(),
            status: "ELIGIBLE".into(),
            claimable: true,
            unit: "CREDIT".into(),
            amount: 1000.0,
            pending: 0.0,
            start: None,
            end: None,
        };
        assert_eq!(
            claim_credits(&serde_json::json!({"data":{"benefitAmount":1000}}), &c),
            1000.0
        );
        assert_eq!(
            claim_credits(&serde_json::json!({"data":{"credit":500}}), &c),
            500.0
        );
        assert_eq!(claim_credits(&serde_json::json!({}), &c), 1000.0);
    }


    #[test]
    fn mainland_strips_stop_and_intl_keeps_it() {
        let mut req = serde_json::json!({"model":"glm-5.3","stop":["END"],"stop_sequences":["X"]});
        let base_cn = "https://snap-access.cn-north-4.myhuaweicloud.com";
        assert!(strip_unsupported_stop(base_cn, &mut req));
        assert!(req.get("stop").is_none(), "大陆站必须丢掉 stop");
        assert!(req.get("stop_sequences").is_none());

        let mut req2 = serde_json::json!({"model":"glm-5.3","stop":["END"]});
        let base_intl = "https://snap-access.ap-southeast-1.myhuaweicloud.com";
        assert!(!strip_unsupported_stop(base_intl, &mut req2));
        assert_eq!(req2["stop"], serde_json::json!(["END"]), "国际站保留 stop");

        // 没有 stop 的请求不报错、不改动
        let mut req3 = serde_json::json!({"model":"glm-5.3"});
        assert!(!strip_unsupported_stop(base_cn, &mut req3));
    }

    #[test]
    fn concurrency_errors_are_recognized() {
        assert!(is_concurrency_error(&UpError::new(
            400,
            "TM.00001041",
            "并发会话数已达上限(3个)"
        )));
        assert!(is_concurrency_error(&UpError::new(
            429,
            "",
            "concurrent session limit reached"
        )));
        assert!(!is_concurrency_error(&UpError::new(400, "TM.00001050", "积分已耗尽")));
    }

    #[test]
    fn queue_state_parsing() {
        assert!(queue_promoted(&serde_json::json!({"status":"promoted"})));
        assert!(queue_promoted(&serde_json::json!({"data":{"state":"RUNNING"}})));
        assert!(queue_promoted(&serde_json::json!({"queueStatus":"admitted"})));
        assert!(!queue_promoted(&serde_json::json!({"status":"queued"})));
        assert!(!queue_promoted(&serde_json::json!({"status":"waiting","position":3})));
    }
}
