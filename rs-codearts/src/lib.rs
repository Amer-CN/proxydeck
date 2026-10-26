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

pub mod config;
pub mod datadir;
pub mod import;
pub mod protocol;
pub mod refresh;
pub mod signer;
pub mod sse;
pub mod store;
pub mod upstream;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use clap::Parser;
use futures_util::StreamExt;
use serde_json::{json, Value};

use config::{Account, Config};
use upstream::{ModelInfo, UpError, Upstream};

/// 请求体上限（与其它 crate 的 32 MiB 口径一致）。
pub const MAX_BODY_BYTES: usize = 32 << 20;
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
    cooldown_until: Option<Instant>,
    failures: u32,
}

pub struct Server {
    up: Upstream,
    accounts_path: PathBuf,
    overlay_path: PathBuf,
    sts_url: String,
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
}

impl Server {
    pub fn new(
        up: Upstream,
        accounts_path: PathBuf,
        allow_local: bool,
        sts_url: String,
        allow_local_refresh: bool,
        max_attempts: usize,
    ) -> Arc<Self> {
        let overlay_path = accounts_path
            .parent()
            .map(|p| p.join("codearts-refreshed.json"))
            .unwrap_or_else(|| PathBuf::from("codearts-refreshed.json"));
        let refreshed = store::Store::load(&overlay_path);
        Arc::new(Self {
            up,
            accounts_path,
            overlay_path,
            sts_url,
            allow_local,
            allow_local_refresh,
            max_attempts: max_attempts.max(1),
            started: Instant::now(),
            models: Mutex::new(None),
            local_cache: Mutex::new(None),
            refreshed: Mutex::new(refreshed),
            health: Mutex::new(std::collections::HashMap::new()),
            archived: Mutex::new(std::collections::HashSet::new()),
        })
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
        let id = account.identity();
        if let Ok(done) = self.archived.lock() {
            if done.contains(&id) {
                return;
            }
        }
        let mut cfg = match self.load_config() {
            Ok(c) => c,
            Err(_) => Config {
                accounts: Vec::new(),
                channel: config::DEFAULT_CHANNEL.to_string(),
                removed: Vec::new(),
                path: self.accounts_path.clone(),
            },
        };
        if cfg.removed.iter().any(|x| x == &id) {
            if let Ok(mut done) = self.archived.lock() {
                done.insert(id);
            }
            return;
        }
        let has = cfg
            .accounts
            .iter()
            .any(|a| a.identity() == id || (a.display_name() == account.display_name() && a.domain_id == account.domain_id));
        if has {
            if let Ok(mut done) = self.archived.lock() {
                done.insert(id);
            }
            return;
        }
        let mut snap = account.clone();
        if snap.name.trim().is_empty() {
            snap.name = snap.display_name();
        }
        cfg.upsert(snap);
        if cfg.save().is_ok() {
            println!(
                "codearts-plugin: 本机登录态已自动归档（{}），账号池 +1",
                account.display_name()
            );
            if let Ok(mut done) = self.archived.lock() {
                done.insert(id);
            }
        }
    }

    fn load_config(&self) -> Result<Config, String> {
        Config::load(&self.accounts_path)
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

    fn mark_ok(&self, id: &str) {
        if let Ok(mut h) = self.health.lock() {
            let e = h.entry(id.to_string()).or_default();
            e.failures = 0;
            e.cooldown_until = None;
        }
    }

    fn mark_cooling(&self, id: &str, secs: u64) {
        if let Ok(mut h) = self.health.lock() {
            let e = h.entry(id.to_string()).or_default();
            e.failures += 1;
            e.cooldown_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

    fn mark_dead(&self, id: &str) {
        if let Ok(mut h) = self.health.lock() {
            let e = h.entry(id.to_string()).or_default();
            e.dead = true;
            e.failures += 1;
        }
    }

    /// 凭据失效的处置：本机登录态在「桌面端运行中、且本插件不主动续期」时只冷却，
    /// 因为桌面端随后会自己刷新凭据；文件账号才标死（需要用户更新文件/重新导入）。
    fn note_credential_failure(&self, account: &Account, source: &str) {
        if source == "local" && !self.allow_local_refresh && self.codearts_running() {
            self.mark_cooling(&account.identity(), 120);
        } else {
            self.mark_dead(&account.identity());
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
        let removed: &[String] = cfg
            .as_ref()
            .map(|c| c.removed.as_slice())
            .unwrap_or(&[]);
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, (Option<Account>, Option<Account>)> =
            std::collections::HashMap::new();
        // 文件快照（应用 overlay）。
        if let Some(c) = cfg.as_ref() {
            for a in c.accounts.iter().filter(|a| a.has_credentials()) {
                let mut a = a.clone();
                if removed.iter().any(|x| x == &a.identity()) {
                    continue;
                }
                if let Some(o) = overlay.overlay(&a) {
                    a = o;
                }
                let id = a.identity();
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
        // 本机实时登录态。
        if let Some(local) = self.local_session() {
            if local.account.has_credentials() {
                let mut a = local.account;
                if !removed.iter().any(|x| x == &a.identity()) {
                    if let Some(o) = overlay.overlay(&a) {
                        a = o;
                    }
                    let id = a.identity();
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

    /// 账号候选表：剔除已判死账号；本机优先、未冷却优先、过期晚的优先。
    fn candidates(&self) -> Result<Vec<(Account, &'static str)>, String> {
        let mut out: Vec<(Account, &'static str, bool)> = Vec::new();
        for (a, source) in self.all_accounts() {
            if !a.enabled {
                continue;
            }
            let id = a.identity();
            let h = self.health_of(&id);
            if h.dead {
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
        if !account.is_expired() {
            return (account.clone(), None);
        }
        if !account.can_refresh() {
            return (account.clone(), Some("已过期且缺少续期材料".to_string()));
        }
        if source == "local" && !self.allow_local_refresh && self.codearts_running() {
            return (
                account.clone(),
                Some("已过期；码道桌面端在运行，跳过主动续期以免踢掉桌面端".to_string()),
            );
        }
        match refresh::refresh_token(self.up.client(), &self.sts_url, account).await {
            Ok(r) => {
                let new = self.apply_refresh(account, &r);
                println!(
                    "codearts-plugin: 账号 {} 续期成功（新过期 {}）",
                    account.display_name(),
                    r.expires_at
                );
                (new, Some("已自动续期".to_string()))
            }
            Err(e) => {
                eprintln!(
                    "codearts-plugin: 账号 {} 续期失败: {e}",
                    account.display_name()
                );
                (account.clone(), Some(format!("续期失败: {e}")))
            }
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
        let mut cfg = self.load_config().unwrap_or(Config {
            accounts: Vec::new(),
            channel: config::DEFAULT_CHANNEL.to_string(),
            removed: Vec::new(),
            path: self.accounts_path.clone(),
        });
        for mut a in accounts {
            if a.name.trim().is_empty() {
                a.name = a.display_name();
            }
            if !a.enabled {
                a.enabled = true;
            }
            let id = a.identity();
            cfg.removed.retain(|x| x != &id);
            cfg.upsert(a);
        }
        cfg.save()?;
        Ok(self
            .all_accounts()
            .iter()
            .map(|(a, s)| self.account_view(a, s))
            .collect())
    }

    /// 强制续期（忽略是否过期）。本机登录态且桌面运行中且未显式允许 → 拒绝。
    async fn force_refresh(&self, account: &Account, source: &str) -> Result<Account, String> {
        if !account.can_refresh() {
            return Err("该账号缺少续期材料（refresh_token / DPoP 私钥 / PKCE）".to_string());
        }
        if source == "local" && !self.allow_local_refresh && self.codearts_running() {
            return Err("码道桌面端在运行：为避免把桌面端踢下线，默认不主动续期本机登录态（可用 --allow-local-refresh 覆盖）".to_string());
        }
        let r = refresh::refresh_token(self.up.client(), &self.sts_url, account).await?;
        let new = self.apply_refresh(account, &r);
        println!(
            "codearts-plugin: 账号 {} 手动续期成功（新过期 {}）",
            account.display_name(),
            r.expires_at
        );
        Ok(new)
    }

    /// 把续期结果应用到账号并落盘 overlay。
    /// 同时把新凭据写回账号文件（无 domainId 的账号 identity 取自 AK 尾段，
    /// AK 轮转后 identity 会变；不写回的话启用/停用/删除就找不到它了）。
    fn apply_refresh(&self, account: &Account, r: &refresh::Refreshed) -> Account {
        let mut new = account.clone();
        new.access_key = r.access_key.clone();
        new.secret_key = r.secret_key.clone();
        new.security_token = r.security_token.clone();
        new.expires_at = if r.expires_at.is_empty() {
            None
        } else {
            Some(r.expires_at.clone())
        };
        new.refresh_token = Some(r.refresh_token.clone());
        if let Ok(mut st) = self.refreshed.lock() {
            st.put(account, r);
            if let Err(e) = st.save(&self.overlay_path) {
                eprintln!("codearts-plugin: 续期落盘失败: {e}");
            }
        }
        // 同步写回账号文件：让文件里的凭据保持可用状态，
        // 续期后 identity 变化也不会让启用/停用/删除找不到账号。
        let old_id = account.identity();
        let old_name = account.display_name();
        let old_domain = account.domain_id.clone();
        if let Ok(mut cfg) = self.load_config() {
            let mut changed = false;
            for e in cfg.accounts.iter_mut() {
                let same_id = e.identity() == old_id;
                let same_name =
                    e.display_name() == old_name && e.domain_id == old_domain;
                if same_id || same_name {
                    e.access_key = r.access_key.clone();
                    e.secret_key = r.secret_key.clone();
                    e.security_token = r.security_token.clone();
                    e.expires_at = if r.expires_at.is_empty() {
                        None
                    } else {
                        Some(r.expires_at.clone())
                    };
                    e.refresh_token = Some(r.refresh_token.clone());
                    changed = true;
                }
            }
            if changed {
                if let Err(e) = cfg.save() {
                    eprintln!("codearts-plugin: 账号文件回写失败: {e}");
                }
            }
        }
        new
    }

    /// 账号视图（绝不输出密钥类字段）。
    fn account_view(&self, a: &Account, source: &str) -> Value {
        let id = a.identity();
        let h = self.health_of(&id);
        json!({
            "identity": id,
            "name": a.display_name(),
            "source": source,
            "enabled": a.enabled,
            "akTail": Config::ak_tail(a),
            "expiresAt": a.expires_at,
            "expired": a.is_expired(),
            "refreshable": a.can_refresh(),
            "dead": h.dead,
            "cooling": h.cooldown_until.map(|t| t > Instant::now()).unwrap_or(false),
            "failures": h.failures,
            "domainId": a.domain_id,
        })
    }
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
        .route("/accounts/import", any(handle_import))
        .route("/accounts/import-local", any(handle_import_local))
        .route("/accounts/refresh", any(handle_account_refresh))
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
    let mut first: Option<(Account, &'static str)> = None;
    for (a, source) in &all {
        let id = a.identity();
        let h = st.health_of(&id);
        let cooling = h.cooldown_until.map(|t| t > Instant::now()).unwrap_or(false);
        if first.is_none() && a.enabled && !h.dead {
            first = Some((a.clone(), *source));
        }
        pool.push(json!({
            "account": a.display_name(),
            "source": source,
            "expired": a.is_expired(),
            "refreshable": a.can_refresh(),
            "dead": h.dead,
            "cooling": cooling,
            "failures": h.failures,
        }));
    }
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
        let (account, _note) = st.ensure_fresh(&account, source).await;
        if let Ok(cache) = st.models.lock() {
            if let Some(c) = cache.as_ref() {
                if c.access_key == account.access_key && c.at.elapsed() < MODELS_TTL {
                    return models_response(&c.list);
                }
            }
        }
        match st.up.builtin_models(&account).await {
            Ok(list) => {
                st.mark_ok(&account.identity());
                if let Ok(mut cache) = st.models.lock() {
                    *cache = Some(ModelsCache {
                        at: Instant::now(),
                        access_key: account.access_key.clone(),
                        list: list.clone(),
                    });
                }
                return models_response(&list);
            }
            Err(e) => {
                if e.credential {
                    st.note_credential_failure(&account, source);
                } else if retryable(&e) {
                    st.mark_cooling(&account.identity(), 30);
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
    let data: Vec<Value> = list
        .iter()
        .filter(|m| m.enable)
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": created,
                "owned_by": "codearts",
                "name": m.name,
                "description": m.desc,
                "context_window": m.context_window,
                "max_tokens": m.max_tokens,
                "supports_images": m.supports_images,
            })
        })
        .collect();
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
    let wants_stream = req
        .get("stream")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let cfg = st.load_config().unwrap_or_default();

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
    for (account, source) in accounts.into_iter().take(st.max_attempts.max(1)) {
        let (account, _note) = st.ensure_fresh(&account, source).await;
        let session_id = upstream::session_id_for(&account);
        match st
            .up
            .chat(
                &account,
                &model,
                payload.clone(),
                cfg.is_benefit(),
                &session_id,
            )
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status < 400 {
                    st.mark_ok(&account.identity());
                    return finish_chat(resp, &model, wants_stream).await;
                }
                let body = resp.bytes().await.unwrap_or_default();
                let e = upstream::error_from_body(status, &body);
                if e.credential {
                    // 账号失效：本轮直接标死，换下一个
                    st.note_credential_failure(&account, source);
                    last = Some(e);
                    continue;
                }
                if retryable(&e) {
                    st.mark_cooling(&account.identity(), 30);
                    last = Some(e);
                    continue;
                }
                return up_error_response(&e);
            }
            Err(e) => {
                if e.credential {
                    st.note_credential_failure(&account, source);
                } else if retryable(&e) {
                    st.mark_cooling(&account.identity(), 30);
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

/// 上游已成功（HTTP < 400）后的出站处理：SSE 透传 / 非流式聚合。
async fn finish_chat(resp: reqwest::Response, model: &str, wants_stream: bool) -> Response {
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // 非 SSE 的 200：可能是内嵌业务错误，也可能上游直接回了整包 JSON。
    if !ctype.contains("text/event-stream") {
        let body = resp.bytes().await.unwrap_or_default();
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            if let Some(e) = upstream::embedded_error(&v) {
                return up_error_response(&e);
            }
        }
        let ct = if ctype.is_empty() {
            "application/json; charset=utf-8".to_string()
        } else {
            ctype
        };
        return ([(header::CONTENT_TYPE, ct)], body).into_response();
    }

    if wants_stream {
        match preflight_stream(resp).await {
            Ok(stream) => (
                [
                    (header::CONTENT_TYPE, "text/event-stream; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-cache"),
                    (header::CONNECTION, "keep-alive"),
                ],
                Body::from_stream(stream),
            )
                .into_response(),
            Err(e) => up_error_response(&e),
        }
    } else {
        let text = match collect_stream(resp).await {
            Ok(t) => t,
            Err(e) => return up_error_response(&e),
        };
        let mut agg = sse::Aggregate::default();
        agg.feed(&text);
        json_response(StatusCode::OK, &agg.into_response(model))
    }
}

type ByteStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static>,
>;

/// 读首个 SSE 事件做预检；正常就把「已读前缀 + 剩余流」接回去。
async fn preflight_stream(resp: reqwest::Response) -> Result<ByteStream, UpError> {
    let mut source = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + STREAM_PRELUDE;
    loop {
        let complete = sse::first_data_event(&buf).is_some()
            && buf.windows(2).any(|w| w == &b"\n\n"[..]);
        if complete || buf.len() > 128 * 1024 {
            break;
        }
        match tokio::time::timeout_at(deadline, source.next()).await {
            Err(_) => {
                return Err(UpError::new(504, "timeout", "等待上游首个事件超时"));
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                return Err(UpError::new(502, "network", format!("读取上游流失败: {e}")))
            }
            Ok(Some(Ok(chunk))) => buf.extend_from_slice(&chunk),
        }
    }
    if let Some(e) = sse::preflight_error(&buf) {
        return Err(e);
    }
    let head = futures_util::stream::once(async move { Ok::<Bytes, std::io::Error>(Bytes::from(buf)) });
    let rest = source.map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
    Ok(Box::pin(head.chain(rest)))
}

/// 收集整条 SSE（非流式聚合用），上限 64 MiB。
async fn collect_stream(resp: reqwest::Response) -> Result<String, UpError> {
    let mut source = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|e| UpError::new(502, "network", format!("读取上游流失败: {e}")))?;
        if buf.len() + chunk.len() > 64 << 20 {
            return Err(UpError::new(502, "too_large", "上游响应超过 64 MiB"));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
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
        let (account, _note) = st.ensure_fresh(&account, source).await;
        match st.up.statistics(&account).await {
            Ok(raw) => {
                st.mark_ok(&account.identity());
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
                    st.mark_cooling(&account.identity(), 30);
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
    let Some((account, source)) = st
        .all_accounts()
        .into_iter()
        .find(|(a, _)| a.identity() == identity)
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
    let mut cfg = match st.load_config() {
        Ok(c) => c,
        Err(e) => return error_response(500, &e, "load_failed"),
    };
    let Some(slot) = cfg.accounts.iter_mut().find(|a| a.identity() == identity) else {
        return error_response(
            404,
            "账号文件里没有该账号（本机登录态只能整体用 --no-local 关闭）",
            "not_found",
        );
    };
    slot.enabled = enabled;
    if let Err(e) = cfg.save() {
        return error_response(500, &e, "save_failed");
    }
    json_response(StatusCode::OK, &json!({ "identity": identity, "enabled": enabled }))
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
    let mut cfg = match st.load_config() {
        Ok(c) => c,
        Err(e) => return error_response(500, &e, "load_failed"),
    };
    let before = cfg.accounts.len();
    cfg.accounts.retain(|a| a.identity() != identity);
    if cfg.accounts.len() == before {
        return error_response(404, "账号文件里没有该账号", "not_found");
    }
    if !cfg.removed.iter().any(|x| x == identity) {
        cfg.removed.push(identity.to_string());
    }
    if let Err(e) = cfg.save() {
        return error_response(500, &e, "save_failed");
    }
    json_response(StatusCode::OK, &json!({ "identity": identity, "removed": true }))
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
    );
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
