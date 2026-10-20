//! rs-qoder —— ProxyDeck Qoder 插件（8785）的 Rust 重写（绞杀者路线 P3）。
//!
//! 目标：等价替换 `internal/qoder`（阿里 Qoder），先在测试端口独立跑通，不注册进
//! `app/plugins.go`、不动 Go 构建、不碰任何现行服务端口（尤其 **8785 上跑着的 Go 版**）。
//!
//! 传输层与 Go 完全同构：**每请求 spawn 一个官方 agent SDK worker 进程**
//! （`qoder-worker-runtime.obf.mjs`，stdin/stdout stream-json，jobToken 回调鉴权），
//! 全部上游流量出自官方 worker 进程——代理只做协议翻译，**不自己构造任何对上游的请求**
//! （Go `server.go:3-6` 的「零自造请求」原则）。
//!
//! 端点（与 Go `http.ServeMux` 注册的四条同形）：
//! * `/health`              —— 本地应答；无登录态回 `{"status":"no_license"}`；
//!   有则带 `service`/`uptimeSec`/`credits{spent,spentThisSession,quotaExceeded[,realtime]}`。
//!   **不看方法**（POST 也回 200）。
//! * `/credits/set`         —— POST `{"spent":0}` 清零累计（非 POST 回 405 + `Allow: POST`）。
//! * `/v1/models`           —— 静态模型目录 13 条（首条 Auto）。**不看方法**。
//! * `/v1/chat/completions` —— 扁平化 messages → worker prompt → 翻回 OpenAI（流式/非流式）。
//! * 其余路径 —— `ServeMux` 语义的 404/307（路径清洗重定向），见 `handle_fallback`。
//!
//! 测试缝（生产默认值就是 Go 的常量/环境变量）：
//! * `auth_dir`（默认 `%APPDATA%\com.qodercn.app.stable`）、`worker_path`（`find_worker`）、
//!   `credits_path`（`%USERPROFILE%\.qoder-cli-credits.json`）、`usage_url`（官方额度接口）
//!   —— 黄金回放靠它们指向临时目录与假上游，**不碰真实登录态、不碰网络**；
//! * `cwd_env`（Go 每请求读 `QODER_CWD`，这里启动时读一次，见 README 差异清单）。
//!
//! 双区（`--region CN|INTL`，默认 CN）：Qoder 客户端有 CN 版与国际版两个构建，
//! 协议完全一致（同一份代码的两个构建），差异只有三处默认值——登录态目录名、
//! worker 相对路径、额度接口域名。区域只提供「整套默认值」，`--auth-dir` /
//! `--worker` / `--usage-url` 显式给出时一律以覆盖值为准；不带 `--region` 时
//! 三处默认值与改前逐字节一致（见 `RegionCfg`）。

pub mod auth;
pub mod credits;
pub mod crypto;
pub mod cwd;
pub mod gjson;
pub mod godecode;
pub mod usage;
pub mod worker;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use clap::Parser;
use futures_util::StreamExt;
use gjson::{b, f, i, obj, s, J};
use serde_json::Value;
use tokio::sync::mpsc;

use auth::QoderAuth;
use godecode::ChatRequest;
use usage::UsageQuota;
use worker::{WorkerEvent, WorkerResult};

// ---------- 区域（CN / INTL） ----------

/// 服务区域：`--region CN|INTL`，默认 CN（**不带该参数时行为与改前逐字节一致**）。
///
/// 两区协议一致（实测 15 个探针同值、worker 是同一份代码的两个构建），所以不拆端口、
/// 不做账号池，只按区域换三处默认值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Cn,
    Intl,
}

impl Region {
    /// 与 `rs-codebuddy` 的 `Region::parse` 同口径：空/未知 → CN（默认行为不变）。
    pub fn parse(s: &str) -> Region {
        match s {
            "INTL" | "intl" | "international" => Region::Intl,
            _ => Region::Cn,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Region::Cn => "CN",
            Region::Intl => "INTL",
        }
    }

    pub fn cfg(self) -> &'static RegionCfg {
        match self {
            Region::Cn => &REGION_CN,
            Region::Intl => &REGION_INTL,
        }
    }
}

/// 一个区域的全部默认值——本 crate 的区域差异**只有这三条**（协议层完全一致）。
pub struct RegionCfg {
    /// 登录态目录名：`%APPDATA%\<名字>`。
    pub auth_dir_name: &'static str,
    /// worker 相对 `<ProgramFiles>` 的路径（不含文件名 `qoder-worker-runtime.obf.mjs`）。
    pub worker_rel: &'static str,
    /// 实时额度接口（`/health?refresh=1` 才拉）。
    pub usage_url: &'static str,
}

pub static REGION_CN: RegionCfg = RegionCfg {
    auth_dir_name: auth::AUTH_DIR_NAME,
    worker_rel: worker::WORKER_REL,
    usage_url: usage::USAGE_URL,
};

pub static REGION_INTL: RegionCfg = RegionCfg {
    auth_dir_name: auth::AUTH_DIR_NAME_INTL,
    worker_rel: worker::WORKER_REL_INTL,
    usage_url: usage::USAGE_URL_INTL,
};

/// 与 Go 侧 `--plugin-qoder` 的默认监听端口同值（对照一律显式指定测试端口，如 18785）。
pub const DEFAULT_PORT: u16 = 8785;

/// 与 Go `handleChat` 的 `io.LimitReader(r.Body, 32<<20)` 同值。
pub const MAX_BODY_BYTES: usize = 32 << 20;

/// 与 Go `handleCreditsSet` 的 `io.LimitReader(r.Body, 64<<10)` 同值。
pub const CREDITS_BODY_BYTES: usize = 64 << 10;

/// `/health` 展示用的静态模型目录：以用户 2026-08-30 的 Qoder 界面截图为准（13 条全量）。
/// `id` 是传给 worker 的 `--model` 原文（displayName 上游不认——实测）。
pub const QODER_MODELS: [(&str, &str); 13] = [
    ("Auto", "Auto"),
    ("qmodel_38max", "Qwen3.8-Max"),
    ("qfmodel", "Qwen3.8-Flash"),
    ("q37fmodel", "Qwen3.7-Flash"),
    ("qmodel_latest", "Qwen3.7-Max"),
    ("qmodel", "Qwen3.7-Plus"),
    ("dmodel", "DeepSeek-V4-Pro"),
    ("dfmodel", "DeepSeek-V4-Flash"),
    ("gmodel", "GLM-5.3"),
    ("gfmodel", "GLM-5.3-Flash"),
    ("gm51model", "GLM-5.2"),
    ("kmodel", "Kimi-K2.7-Code"),
    ("mmodel", "MiniMax-M3"),
];

#[derive(Parser, Debug)]
#[command(name = "rs-qoder", version, about = "ProxyDeck Qoder 插件（Rust 重写）")]
pub struct Args {
    /// 与 Go 侧 `--plugin-qoder` 同名，保持 GUI spawn 形态一致（本阶段不注册进 GUI，仅占位）。
    #[arg(long = "plugin-qoder", default_value_t = false)]
    pub plugin_qoder: bool,

    /// 监听端口（默认与 Go 侧 8785 同值；对照请显式指定测试端口，如 18785）。
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// 监听地址（Go 侧同款默认回环）。
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// 区域：`CN`（默认）/ `INTL`。只提供「整套默认值」（登录态目录 / worker 路径 / 额度接口），
    /// 显式的 `--auth-dir` / `--worker` / `--usage-url` 一律优先。
    #[arg(long, default_value = "CN")]
    pub region: String,

    /// worker 路径覆盖（默认 `QODER_WORKER` → `D:\Program Files\...` → `%ProgramFiles%\...`）。仅供对照。
    #[arg(long)]
    pub worker: Option<PathBuf>,

    /// 登录态目录覆盖（默认 `%APPDATA%\com.qodercn.app.stable`）。仅供对照。
    #[arg(long)]
    pub auth_dir: Option<PathBuf>,

    /// 积分账本文件覆盖（默认 `%USERPROFILE%\.qoder-cli-credits.json`）。仅供对照。
    #[arg(long)]
    pub credits_file: Option<PathBuf>,

    /// 实时额度接口覆盖（默认按 `--region`：CN `openapi.qoder.com.cn` / INTL `openapi.qoder.sh`）。仅供对照。
    #[arg(long = "usage-url", default_value = usage::USAGE_URL)]
    pub usage_url: String,
}

/// 四处显式覆盖（`--auth-dir` / `--worker` / `--credits-file` / `--usage-url`）。
/// 区域只提供默认值，这里给了就以这里为准（`usage_url` 的默认值歧义见 `resolve_usage_url`）。
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub auth_dir: Option<PathBuf>,
    pub worker: Option<PathBuf>,
    pub credits_file: Option<PathBuf>,
    pub usage_url: Option<String>,
}

/// 进程内可变状态（Go `Server` 的三个 mutex 段）。
#[derive(Default)]
struct CreditsState {
    /// 本次进程内 worker `result.total_credits` 合计。
    spent: f64,
    /// 捕获到服务端配额错误（110/112/113/115/119 等）后置位，`/health` 报警。
    quota_exceeded: bool,
}

/// 服务状态。
pub struct AppState {
    pub started_at: Instant,
    /// `%APPDATA%\<区域目录名>`（Go `authDir()`；CN `com.qodercn.app.stable` /
    /// INTL `com.qoder.app.stable`）。
    pub auth_dir: PathBuf,
    /// 探测到的 worker（Go 每请求 `findWorker()`；这里启动时解析一次，见 README 差异）。
    pub worker_path: Option<PathBuf>,
    /// 账本路径（Go `creditsLedgerPath()`；主目录定位不到时为 `None`）。
    pub credits_path: Option<PathBuf>,
    /// `QODER_CWD`（Go 每请求 `os.Getenv`；这里启动时一次）。
    pub cwd_env: Option<String>,
    /// 实时额度接口（Go 是常量；CN `openapi.qoder.com.cn` / INTL `openapi.qoder.sh`）。
    pub usage_url: String,
    /// 登录态缓存（Go `authCache`）。
    auth_cache: Mutex<Option<QoderAuth>>,
    credits: Mutex<CreditsState>,
    /// 实时额度缓存（Go `usageCache`）；**只在 `refresh=1` 时才去拉**。
    usage_cache: Mutex<Option<UsageQuota>>,
    http: reqwest::Client,
}

/// `--usage-url` 的生效值：`--region` 只提供默认值，**显式给出的值优先**。
///
/// CLI 的 `--usage-url` 保留 `default_value = usage::USAGE_URL`（字段类型与改前一致，
/// 既有合同测试也据此断言默认值），所以这里把「值 == CN 默认值」当作「未显式覆盖」。
/// 换言之：不带 `--usage-url` 时按区域取；带了别的值一律以它为准。
/// （极端情形：`--region INTL --usage-url <CN 的默认值>` 会被当成未覆盖——登记在 README 差异 11。）
pub fn resolve_usage_url(region: Region, cli_value: &str) -> String {
    if cli_value == usage::USAGE_URL {
        region.cfg().usage_url.to_string()
    } else {
        cli_value.to_string()
    }
}

impl AppState {
    /// 显式构造（测试/黄金回放用）。`usage_url` 取 CN 默认值（与改前一致），
    /// 需要别的区域用 `with_usage_url` / `from_env_with`。
    pub fn new(
        auth_dir: PathBuf,
        worker_path: Option<PathBuf>,
        credits_path: Option<PathBuf>,
        cwd_env: Option<String>,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            auth_dir,
            worker_path,
            credits_path,
            cwd_env,
            usage_url: usage::USAGE_URL.to_string(),
            auth_cache: Mutex::new(None),
            credits: Mutex::new(CreditsState::default()),
            usage_cache: Mutex::new(None),
            http: reqwest::Client::builder()
                .build()
                .expect("建 reqwest 客户端"),
        }
    }

    /// 生产构造：与 Go 的 `authDir()` / `findWorker()` / `creditsLedgerPath()` / `os.Getenv`
    /// 同一套判定（环境变量读一次的差异见 README）。默认区域 CN。
    pub fn from_env() -> Self {
        Self::from_env_region(Region::Cn)
    }

    /// 按区域取「整套默认值」的生产构造。
    pub fn from_env_region(region: Region) -> Self {
        let worker = worker::find_worker(
            std::env::var("QODER_WORKER").ok().as_deref(),
            std::env::var("ProgramFiles").ok().as_deref(),
            region,
        );
        let mut st = Self::new(
            auth::auth_dir(region),
            worker,
            credits::ledger_path_in(credits::home_dir().as_deref()),
            std::env::var("QODER_CWD").ok(),
        );
        st.usage_url = region.cfg().usage_url.to_string();
        st
    }

    /// 区域默认值 + 显式覆盖（**覆盖优先**）。`run()` 与单测共用这一条路径。
    pub fn from_env_with(region: Region, ov: &Overrides) -> Self {
        let mut st = Self::from_env_region(region);
        if let Some(p) = &ov.worker {
            st.worker_path = Some(p.clone());
        }
        if let Some(d) = &ov.auth_dir {
            st.auth_dir = d.clone();
        }
        if let Some(f) = &ov.credits_file {
            st.credits_path = Some(f.clone());
        }
        if let Some(u) = ov.usage_url.as_deref() {
            st.usage_url = resolve_usage_url(region, u);
        }
        st
    }

    pub fn with_usage_url(mut self, url: impl Into<String>) -> Self {
        self.usage_url = url.into();
        self
    }

    /// Go `currentAuth()`：内存缓存，`expiresAt` 临近过期（<30s）或解不出时重读文件
    /// （App 在跑时 `auth.v1.dat` 会被轮换刷新，重读自动跟上）。
    pub fn current_auth(&self) -> Result<QoderAuth, String> {
        {
            let guard = self.auth_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(a) = guard.as_ref() {
                if let Some(ms) = a.expires_at_ms {
                    if ms - now_ms() > 30_000 {
                        return Ok(a.clone());
                    }
                }
            }
        }
        let a = auth::load_auth(&self.auth_dir)?;
        let mut guard = self.auth_cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(a.clone());
        Ok(a)
    }

    /// Go `addCredits`：worker `result` 到达后累计消耗（服务端下发的真实口径）。
    pub fn add_credits(&self, total: f64) {
        if total <= 0.0 {
            return;
        }
        let mut guard = self.credits.lock().unwrap_or_else(|e| e.into_inner());
        guard.spent += total;
        let session = guard.spent;
        drop(guard);
        let spent = credits::read_spent(self.credits_path.as_deref()) + total;
        match self.credits_path.as_deref() {
            Some(p) => match credits::write_spent(p, spent, now_ms()) {
                Ok(()) => println!("qoder-plugin: credits +{total:.2} → 累计 {spent:.2}"),
                Err(e) => println!("qoder-plugin: credits 写账本失败: {e}"),
            },
            None => println!("qoder-plugin: credits 写账本失败: 无法定位用户主目录"),
        }
        let _ = session;
    }

    pub fn mark_quota_exceeded(&self) {
        let mut guard = self.credits.lock().unwrap_or_else(|e| e.into_inner());
        guard.quota_exceeded = true;
    }

    pub fn credits_snapshot(&self) -> (f64, bool) {
        let guard = self.credits.lock().unwrap_or_else(|e| e.into_inner());
        (guard.spent, guard.quota_exceeded)
    }

    /// Go `cachedUsage`：`force=false` 只回缓存、从不访问官方接口；
    /// `force=true` 现拉一次并更新缓存，失败回落旧缓存。
    async fn cached_usage(&self, token: &str, force: bool) -> Option<UsageQuota> {
        if !force {
            return self
                .usage_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
        }
        match usage::fetch_usage(&self.http, &self.usage_url, token).await {
            Ok(u) => {
                let mut guard = self.usage_cache.lock().unwrap_or_else(|e| e.into_inner());
                *guard = Some(u.clone());
                Some(u)
            }
            Err(_) => self
                .usage_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }
}

// ---------- 路由 ----------

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // 四条路由都挂 `any`：Go 的 handler 自己不判方法（只有 credits/set 与 chat 判），
        // 挂 `get/post` 会在路由层先变 405，与 Go 分歧。
        .route("/health", any(handle_health))
        .route("/credits/set", any(handle_credits_set))
        .route("/v1/models", any(handle_models))
        .route("/v1/chat/completions", any(handle_chat))
        .fallback(any(handle_fallback))
        .layer(axum::middleware::from_fn(cors))
        .with_state(state)
}

/// 对应 Go `corsWith`：所有响应都带 CORS 头；OPTIONS 直接 204（在路由之前短路）。
async fn cors(req: Request, next: axum::middleware::Next) -> Response {
    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        add_cors(resp.headers_mut());
        return resp;
    }
    let mut resp = next.run(req).await;
    add_cors(resp.headers_mut());
    resp
}

pub fn add_cors(h: &mut HeaderMap) {
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
}

// ---------- 纯函数（黄金门禁直接对着它们比） ----------

/// Go `resolveModel`：`'auto'`/空 → `Auto`；白名单内（忽略大小写，id 与显示名双向匹配）
/// → 目录 id 原文；不认识的返回 `""`（调用方回 502 invalid_model）。
pub fn resolve_model(requested: &str) -> String {
    if requested.is_empty() || requested.eq_ignore_ascii_case("auto") {
        return "Auto".to_string();
    }
    for (id, name) in QODER_MODELS {
        // Go 的 strings.EqualFold 是 Unicode simple fold；这里用 ASCII 折叠（真实模型名全 ASCII）
        if id.eq_ignore_ascii_case(requested) || name.eq_ignore_ascii_case(requested) {
            return id.to_string();
        }
    }
    String::new()
}

/// Go `flattenMessages`：按顺序 `role: content` 拼接，`system→[System]\n`、
/// `user→[User]\n`、`assistant→[Assistant]\n`、其余 `[<role>]\n`；
/// `content` 为数组时取其中 `text` 部分拼接；**忽略 tool_calls 等复杂字段**；
/// 最后 `strings.TrimRight(s, "\n")`。
pub fn flatten_messages(messages: &[Value]) -> String {
    let mut out = String::new();
    for raw in messages {
        let Some(msg) = raw.as_object() else { continue };
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        match role {
            "system" => out.push_str("[System]\n"),
            "user" => out.push_str("[User]\n"),
            "assistant" => out.push_str("[Assistant]\n"),
            other => {
                out.push('[');
                out.push_str(other);
                out.push_str("]\n");
            }
        }
        match msg.get("content") {
            Some(Value::String(c)) => out.push_str(c),
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Some(p) = part.as_object() {
                        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                out.push_str(t);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Go `chunkJSON`：`delta.content` 为增量文本；`finish` 为空时 `finish_reason=null`。
pub fn chunk_json(id: &str, model: &str, delta: &str, finish: &str, created: i64) -> Vec<u8> {
    let mut d: Vec<(&str, J)> = Vec::new();
    if !delta.is_empty() {
        d.push(("content", s(delta)));
    }
    chunk_envelope(id, model, obj(d), finish, created)
}

/// Go `chunkReasoningJSON`：思考增量走 `delta.reasoning_content`（DeepSeek 方言）。
pub fn chunk_reasoning_json(
    id: &str,
    model: &str,
    thinking: &str,
    finish: &str,
    created: i64,
) -> Vec<u8> {
    let mut d: Vec<(&str, J)> = Vec::new();
    if !thinking.is_empty() {
        d.push(("reasoning_content", s(thinking)));
    }
    chunk_envelope(id, model, obj(d), finish, created)
}

fn chunk_envelope(id: &str, model: &str, delta: J, finish: &str, created: i64) -> Vec<u8> {
    let fr = if finish.is_empty() {
        J::Null
    } else {
        s(finish)
    };
    obj(vec![
        ("id", s(id)),
        ("object", s("chat.completion.chunk")),
        ("created", i(created)),
        ("model", s(model)),
        (
            "choices",
            J::Arr(vec![obj(vec![
                ("index", i(0)),
                ("delta", delta),
                ("finish_reason", fr),
            ])]),
        ),
    ])
    .to_go()
    .into_bytes()
}

/// Go `completeJSON`：非流式完整响应（上游无 usage 数据，填 0）。
pub fn complete_json(id: &str, model: &str, content: &str, created: i64) -> Vec<u8> {
    obj(vec![
        ("id", s(id)),
        ("object", s("chat.completion")),
        ("created", i(created)),
        ("model", s(model)),
        (
            "choices",
            J::Arr(vec![obj(vec![
                ("index", i(0)),
                (
                    "message",
                    obj(vec![("role", s("assistant")), ("content", s(content))]),
                ),
                ("finish_reason", s("stop")),
            ])]),
        ),
        (
            "usage",
            obj(vec![
                ("prompt_tokens", i(0)),
                ("completion_tokens", i(0)),
                ("total_tokens", i(0)),
            ]),
        ),
    ])
    .to_go()
    .into_bytes()
}

/// Go `isQuotaError`：服务端配额类错误（error-code-cache.json 实录：110 今日问答上限、
/// 112/113 配额上限、115 轻量模型月上限、119 免费次数用完）。
pub fn is_quota_error(msg: &str) -> bool {
    if msg.is_empty() {
        return false;
    }
    for pat in [
        "ResourceUsageOverlimit",
        "配额",
        "升级专业版",
        "免费限额",
        "auto-free-individual",
    ] {
        if msg.contains(pat) {
            return true;
        }
    }
    for code in ["110", "112", "113", "115", "119"] {
        if msg.contains(&format!("code\":{code}")) || msg.contains(&format!("code={code}")) {
            return true;
        }
    }
    false
}

/// Go `firstErr`：`result.errors` 第一条（跳过空白串）；无则兜底文案。
pub fn first_err(res: Option<&WorkerResult>) -> String {
    if let Some(r) = res {
        for e in &r.errs {
            if !e.trim().is_empty() {
                return e.clone();
            }
        }
    }
    "Qoder worker 返回错误".to_string()
}

// ---------- 工具 ----------

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

static ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// Go 的 `fmt.Sprintf("chatcmpl-qoder-%d", rand.Int63())`——随机 id，门禁按形状归一化。
fn new_chat_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let v = (nanos ^ (seq.wrapping_mul(0x9E37_79B9_7F4A_7C15))) & 0x7FFF_FFFF_FFFF_FFFF;
    format!("chatcmpl-qoder-{v}")
}

/// `io.LimitReader(body, max)` 的等价物：读满 max 字节即停（不多读、不报 413）。
async fn read_capped(body: Body, max: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        let remaining = max.saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if out.len() >= max {
            break;
        }
    }
    out
}

fn json_bytes(status: StatusCode, extra: &[(&str, &str)], body: Vec<u8>) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json");
    for (k, v) in extra {
        builder = builder.header(*k, *v);
    }
    builder.body(Body::from(body)).unwrap_or_else(|_| {
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

/// Go `writeError`：OpenAI 风格错误 JSON（`json.Encoder` 的末尾换行照搬）。
fn write_error(status: StatusCode, message: &str, err_type: &str) -> Response {
    let body = obj(vec![(
        "error",
        obj(vec![("message", s(message)), ("type", s(err_type))]),
    )])
    .to_go()
        + "\n";
    json_bytes(status, &[], body.into_bytes())
}

/// 取 `?refresh=` 的第一个值（Go `r.URL.Query().Get("refresh")`）。
fn query_param(uri: &Uri, key: &str) -> Option<String> {
    let q = uri.query()?;
    for pair in q.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(k) == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// 最小百分号解码（`+` 视作空格，与 Go `url.Query()` 同）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------- /health ----------

/// `GET/POST /health`（Go `handleHealth`，**不看方法**）。
async fn handle_health(State(st): State<Arc<AppState>>, uri: Uri) -> Response {
    let a = match auth::load_auth(&st.auth_dir) {
        Err(_) => {
            return json_bytes(
                StatusCode::OK,
                &[],
                (obj(vec![("status", s("no_license"))]).to_go() + "\n").into_bytes(),
            )
        }
        Ok(a) => a,
    };
    let (spent, exceeded) = st.credits_snapshot();
    let mut credits: Vec<(&str, J)> = vec![
        ("spent", f(credits::read_spent(st.credits_path.as_deref()))),
        ("spentThisSession", f(spent)),
        ("quotaExceeded", b(exceeded)),
    ];
    let refresh = query_param(&uri, "refresh").as_deref() == Some("1");
    if let Some(v) = st
        .cached_usage(&a.token, refresh)
        .await
        .as_ref()
        .and_then(usage::usage_view)
    {
        if let Some(J::Bool(true)) = realtime_flag(&v) {
            credits[2].1 = b(true);
        }
        credits.push(("realtime", v));
    }
    let out = obj(vec![
        ("status", s("ok")),
        ("service", s("qoder")),
        ("uptimeSec", i(st.started_at.elapsed().as_secs() as i64)),
        ("credits", obj(credits)),
    ]);
    json_bytes(StatusCode::OK, &[], (out.to_go() + "\n").into_bytes())
}

/// 从 `credits.realtime` 视图里读 `isQuotaExceeded`。
fn realtime_flag(v: &J) -> Option<J> {
    // 只在这里翻一下 J 的 Obj；视图是本地刚构造的，结构固定
    match v {
        J::Obj(pairs) => pairs
            .iter()
            .find(|(k, _)| k == "isQuotaExceeded")
            .map(|(_, v)| v.clone()),
        _ => None,
    }
}

// ---------- /credits/set ----------

/// `POST /credits/set` body `{"spent":0}`（Go `handleCreditsSet`）。
async fn handle_credits_set(State(st): State<Arc<AppState>>, method: Method, req: Request) -> Response {
    if method != Method::POST {
        return json_bytes(
            StatusCode::METHOD_NOT_ALLOWED,
            &[("allow", "POST")],
            err_json(false, "仅支持 POST").into_bytes(),
        );
    }
    let body = read_capped(req.into_body(), CREDITS_BODY_BYTES).await;
    let spent = match decode_set_body(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_bytes(
                StatusCode::OK,
                &[],
                err_json(false, &format!("请求体解析失败: {e}")).into_bytes(),
            )
        }
    };
    if spent != 0.0 {
        return json_bytes(
            StatusCode::OK,
            &[],
            err_json(false, "只支持清零（spent=0）").into_bytes(),
        );
    }
    {
        let mut guard = st.credits.lock().unwrap_or_else(|e| e.into_inner());
        guard.spent = 0.0;
    }
    match st.credits_path.as_deref() {
        Some(p) => {
            if let Err(e) = credits::write_spent(p, 0.0, now_ms()) {
                return json_bytes(StatusCode::OK, &[], err_json(false, &e).into_bytes());
            }
        }
        None => {
            return json_bytes(
                StatusCode::OK,
                &[],
                err_json(false, "无法定位用户主目录").into_bytes(),
            )
        }
    }
    println!("qoder-plugin: credits 累计清零");
    json_bytes(StatusCode::OK, &[], err_json(true, "").into_bytes())
}

/// `{"ok":<bool>}` / `{"ok":false,"error":<msg>}`（Go 的 `map[string]any` + Encoder 换行）。
fn err_json(ok: bool, err: &str) -> String {
    let mut pairs: Vec<(&str, J)> = vec![("ok", b(ok))];
    if !err.is_empty() {
        pairs.push(("error", s(err)));
    }
    obj(pairs).to_go() + "\n"
}

/// `json.NewDecoder(LimitReader(body,64KB)).Decode(&struct{Spent float64 `json:"spent"`}{})`
/// 的等价物（实现在 `godecode::decode_spent_field`，与 Go 同语义：**只看第一个 JSON 值**，
/// 尾随数据忽略；数字溢出按 Go 的 `ErrRange` 文案报错）。错误串按 Go 原文案构造
/// （黄金门禁的 `credits_set` 段逐字节锁住了这些形状）。
pub fn decode_set_body(body: &[u8]) -> Result<f64, String> {
    godecode::decode_spent_field(body)
}

// ---------- /v1/models ----------

/// Go `handleModels`：静态模型目录（**不看方法**）。
async fn handle_models(State(_st): State<Arc<AppState>>) -> Response {
    let data: Vec<J> = QODER_MODELS
        .iter()
        .map(|(id, name)| {
            obj(vec![
                ("id", s(*id)),
                ("display_name", s(*name)),
                ("object", s("model")),
            ])
        })
        .collect();
    let out = obj(vec![("object", s("list")), ("data", J::Arr(data))]);
    json_bytes(StatusCode::OK, &[], (out.to_go() + "\n").into_bytes())
}

// ---------- /v1/chat/completions ----------

async fn handle_chat(State(st): State<Arc<AppState>>, method: Method, req: Request) -> Response {
    let start = Instant::now();
    let path = req.uri().path().to_string();
    if method != Method::POST {
        return write_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "仅支持 POST",
            "invalid_request",
        );
    }
    let auth = match st.current_auth() {
        Err(_) => {
            println!(
                "qoder-plugin: {} {} model=- 503 no_license {:?}",
                method, path, start.elapsed()
            );
            return write_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "未找到 Qoder 登录态，请先在 Qoder CN App 登录",
                "no_license",
            );
        }
        Ok(a) => a,
    };
    let worker_path = match st.worker_path.clone() {
        None => {
            println!(
                "qoder-plugin: {} {} model=- 503 no_worker {:?}",
                method, path, start.elapsed()
            );
            return write_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "未找到 Qoder 官方 worker，请先安装 Qoder CN（AI IDE）",
                "no_license",
            );
        }
        Some(p) => p,
    };
    let body = read_capped(req.into_body(), MAX_BODY_BYTES).await;
    let parsed: ChatRequest = match godecode::decode_chat_request(&body) {
        Ok(v) => v,
        Err(e) => {
            println!("qoder-plugin: {} {} model=- 400 {:?}", method, path, start.elapsed());
            return write_error(
                StatusCode::BAD_REQUEST,
                &format!("请求体解析失败: {e}"),
                "invalid_request",
            );
        }
    };
    let model = resolve_model(&parsed.model);
    if model.is_empty() {
        println!(
            "qoder-plugin: {} {} model={} 502 invalid_model {:?}",
            method, path, parsed.model, start.elapsed()
        );
        return write_error(
            StatusCode::BAD_GATEWAY,
            &format!(
                "不支持的模型: {}（/v1/models 查看可用列表，或改用 auto 由 Qoder 自动路由）",
                parsed.model
            ),
            "invalid_model",
        );
    }
    let prompt = flatten_messages(&parsed.messages);
    if parsed.stream {
        handle_chat_stream(st, &method, &path, auth, worker_path, prompt, model, start).await
    } else {
        handle_chat_once(st, &method, &path, auth, worker_path, prompt, model, start).await
    }
}

/// Go `handleChatOnce`：等 `result`，`content = result.result`（空则用拼接文本）。
#[allow(clippy::too_many_arguments)]
async fn handle_chat_once(
    st: Arc<AppState>,
    method: &Method,
    path: &str,
    auth: QoderAuth,
    worker_path: PathBuf,
    prompt: String,
    model: String,
    start: Instant,
) -> Response {
    let created = unix_now();
    let id = new_chat_id();
    let res = worker::run_worker(
        auth.token.clone(),
        auth.expires_at_ms,
        worker_path,
        model.clone(),
        prompt,
        st.cwd_env.clone(),
        None,
    )
    .await;
    let res = match res {
        Err(e) => {
            println!(
                "qoder-plugin: {} {} model={} 502 {:?}",
                method, path, model, start.elapsed()
            );
            return write_error(StatusCode::BAD_GATEWAY, &e, "upstream_error");
        }
        Ok(r) => r,
    };
    st.add_credits(res.total_credits); // result 已到达，累计本次消耗
    if res.is_err {
        let msg = first_err(Some(&res));
        if is_quota_error(&msg) {
            st.mark_quota_exceeded();
            println!("qoder-plugin: {} {} 检测到配额错误 → /health 报警", method, path);
        }
        println!(
            "qoder-plugin: {} {} model={} 502 is_error {:?}",
            method, path, model, start.elapsed()
        );
        return write_error(StatusCode::BAD_GATEWAY, &msg, "upstream_error");
    }
    let content = if res.final_text.is_empty() {
        res.text.clone()
    } else {
        res.final_text.clone()
    };
    println!(
        "qoder-plugin: {} {} model={} 200 {:?}",
        method, path, model, start.elapsed()
    );
    json_bytes(
        StatusCode::OK,
        &[],
        complete_json(&id, &model, &content, created),
    )
}

/// Go `handleChatStream`：assistant text 块逐个转 `delta.content`，
/// thinking 块逐个转 `delta.reasoning_content`；`result` 后发 `finish_reason=stop` + `[DONE]`。
/// **首个 chunk 落盘前失败可回 502**（落盘后只能 200 收尾，同 Go）。
#[allow(clippy::too_many_arguments)]
async fn handle_chat_stream(
    st: Arc<AppState>,
    method: &Method,
    path: &str,
    auth: QoderAuth,
    worker_path: PathBuf,
    prompt: String,
    model: String,
    start: Instant,
) -> Response {
    let created = unix_now();
    let id = new_chat_id();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<WorkerEvent>();
    let task = tokio::spawn(worker::run_worker(
        auth.token.clone(),
        auth.expires_at_ms,
        worker_path,
        model.clone(),
        prompt,
        st.cwd_env.clone(),
        Some(ev_tx),
    ));

    let first = ev_rx.recv().await;
    let Some(first_ev) = first else {
        // 一个增量都没有：等 worker 结论——错误回 502，成功回「只有 stop + [DONE]」的 200
        let res = match task.await {
            Ok(r) => r,
            Err(e) => Err(format!("worker 任务异常: {e}")),
        };
        match res {
            Err(e) => {
                if is_quota_error(&e) {
                    st.mark_quota_exceeded();
                    println!("qoder-plugin: {} {} 检测到配额错误 → /health 报警", method, path);
                }
                println!(
                    "qoder-plugin: {} {} model={} stream 502 {:?}",
                    method, path, model, start.elapsed()
                );
                return sse_error(&e);
            }
            Ok(r) => {
                st.add_credits(r.total_credits);
                if r.is_err {
                    let msg = first_err(Some(&r));
                    if is_quota_error(&msg) {
                        st.mark_quota_exceeded();
                        println!("qoder-plugin: {} {} 检测到配额错误 → /health 报警", method, path);
                    }
                    println!(
                        "qoder-plugin: {} {} model={} stream 502 {:?}",
                        method, path, model, start.elapsed()
                    );
                    return sse_error(&msg);
                }
                println!(
                    "qoder-plugin: {} {} model={} stream 200 {:?}",
                    method, path, model, start.elapsed()
                );
            }
        }
        return sse_response(Body::from_stream(futures_util::stream::once(async move {
            Ok::<Bytes, std::io::Error>(Bytes::from(sse_tail(&id, &model, created)))
        })));
    };

    // 已提交（HTTP 200 已定）：把增量、收尾块与 [DONE] 交给后台任务边产边发
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Bytes>();
    let st2 = st.clone();
    let method2 = method.clone();
    let path2 = path.to_string();
    tokio::spawn(async move {
        emit_chunk(&out_tx, first_ev, &id, &model, created);
        while let Some(ev) = ev_rx.recv().await {
            emit_chunk(&out_tx, ev, &id, &model, created);
        }
        let res = match task.await {
            Ok(r) => r,
            Err(e) => Err(format!("worker 任务异常: {e}")),
        };
        match res {
            Err(e) => {
                if is_quota_error(&e) {
                    st2.mark_quota_exceeded();
                }
                println!(
                    "qoder-plugin: {} {} model={} stream 200(部分) 上游错误:{} {:?}",
                    method2, path2, model, e, start.elapsed()
                );
            }
            Ok(r) => {
                st2.add_credits(r.total_credits);
                if r.is_err {
                    let msg = first_err(Some(&r));
                    if is_quota_error(&msg) {
                        st2.mark_quota_exceeded();
                        println!("qoder-plugin: {} {} 检测到配额错误 → /health 报警", method2, path2);
                    }
                    println!(
                        "qoder-plugin: {} {} model={} stream 200(部分) 上游错误:{} {:?}",
                        method2, path2, model, msg, start.elapsed()
                    );
                } else {
                    println!(
                        "qoder-plugin: {} {} model={} stream 200 {:?}",
                        method2, path2, model, start.elapsed()
                    );
                }
            }
        }
        let _ = out_tx.send(Bytes::from(sse_tail(&id, &model, created)));
    });
    // tokio 的 mpsc 接收端本身不实现 `Stream`（tokio 1.53 没有 `stream` feature），
    // 用 `unfold` 包一层，省掉一个 `tokio-stream` 依赖。
    sse_response(Body::from_stream(futures_util::stream::unfold(
        out_rx,
        |mut rx| async move {
            rx.recv()
                .await
                .map(|b| (Ok::<Bytes, std::io::Error>(b), rx))
        },
    )))
}

fn emit_chunk(
    tx: &mpsc::UnboundedSender<Bytes>,
    ev: WorkerEvent,
    id: &str,
    model: &str,
    created: i64,
) {
    let data = match ev {
        WorkerEvent::Text(t) => chunk_json(id, model, &t, "", created),
        WorkerEvent::Thinking(t) => chunk_reasoning_json(id, model, &t, "", created),
    };
    let mut frame = Vec::with_capacity(data.len() + 8);
    frame.extend_from_slice(b"data: ");
    frame.extend_from_slice(&data);
    frame.extend_from_slice(b"\n\n");
    let _ = tx.send(Bytes::from(frame));
}

/// 结束块：`finish_reason=stop` + `[DONE]`（Go 无论成功失败都会发这两帧）。
fn sse_tail(id: &str, model: &str, created: i64) -> Vec<u8> {
    let stop = chunk_json(id, model, "", "stop", created);
    let mut out = Vec::with_capacity(stop.len() + 32);
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&stop);
    out.extend_from_slice(b"\n\n");
    out.extend_from_slice(b"data: [DONE]\n\n");
    out
}

fn sse_headers() -> [(&'static str, &'static str); 3] {
    [
        ("content-type", "text/event-stream"),
        ("cache-control", "no-cache"),
        ("connection", "keep-alive"),
    ]
}

fn sse_response(body: axum::body::Body) -> Response {
    let mut builder = Response::builder().status(StatusCode::OK);
    for (k, v) in sse_headers() {
        builder = builder.header(k, v);
    }
    builder.body(body).unwrap_or_else(|_| {
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

/// 流式路径上的 502：Go 的 `handleChatStream` 在未提交时会先设好 SSE 三个头再走
/// `writeError`（后者只覆盖 Content-Type）——所以 `Cache-Control`/`Connection` 会**留在响应上**。
fn sse_error(message: &str) -> Response {
    let body = obj(vec![(
        "error",
        obj(vec![("message", s(message)), ("type", s("upstream_error"))]),
    )])
    .to_go()
        + "\n";
    json_bytes(
        StatusCode::BAD_GATEWAY,
        &[
            ("cache-control", "no-cache"),
            ("connection", "keep-alive"),
        ],
        body.into_bytes(),
    )
}

// ---------- 兜底路由（Go ServeMux 语义） ----------

/// 对应 Go `path.Clean`（词法清洗）。
fn path_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => match stack.last() {
                Some(last) if *last != ".." => {
                    stack.pop();
                }
                Some(_) => {}
                None => {
                    if !rooted {
                        stack.push("..");
                    }
                }
            },
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// 对应 Go `net/http` 的 `cleanPath`（`path.Clean` + 保留尾部斜杠）。
pub fn clean_path(p: &str) -> String {
    let s = if p.is_empty() {
        "/".to_string()
    } else if !p.starts_with('/') {
        format!("/{p}")
    } else {
        p.to_string()
    };
    let np = path_clean(&s);
    if s.ends_with('/') && np != "/" {
        // Go 的「快路径」：p 就是 np 加一个尾斜杠时，**返回 p 自己**（保留尾斜杠）
        if s.len() == np.len() + 1 && s.starts_with(&np) {
            s
        } else {
            format!("{np}/")
        }
    } else {
        np
    }
}

/// Go `hexEscapeNonASCII`：非 ASCII 字节按 %XX（大写十六进制）转义。
fn hex_escape_non_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b < 0x80 {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Go `net/http` 的 `htmlEscape`（重定向响应体用）。
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// 未匹配的路径：`cleanPath(path) != path` 时 307 重定向到清洗后的路径，否则 404。
async fn handle_fallback(method: Method, uri: Uri) -> Response {
    let raw = uri.path().to_string();
    let cleaned = clean_path(&raw);
    if cleaned != raw {
        let loc = match uri.query() {
            Some(q) => format!("{cleaned}?{q}"),
            None => cleaned.clone(),
        };
        let mut builder = Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(header::LOCATION, hex_escape_non_ascii(&loc));
        if method == Method::GET || method == Method::HEAD {
            builder = builder.header(header::CONTENT_TYPE, "text/html; charset=utf-8");
        }
        let body = if method == Method::GET {
            Body::from(format!(
                "<a href=\"{}\">Temporary Redirect</a>.\n\n",
                html_escape(&loc)
            ))
        } else {
            // Go 对 HEAD 不写 body，也不设 Content-Length
            Body::from_stream(futures_util::stream::empty::<Result<Bytes, std::io::Error>>())
        };
        return builder
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(Body::from("404 page not found\n"))
        .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response())
}

// ---------- 入口 ----------

pub async fn run(args: Args) -> Result<(), String> {
    let region = Region::parse(&args.region);
    let ov = Overrides {
        auth_dir: args.auth_dir.clone(),
        worker: args.worker.clone(),
        credits_file: args.credits_file.clone(),
        // CLI 的 `--usage-url` 带 CN 默认值，交给 `resolve_usage_url` 判「是否显式覆盖」
        usage_url: Some(args.usage_url.clone()),
    };
    let state = AppState::from_env_with(region, &ov);

    println!(
        "qoder-plugin: [rust rewrite] starting on {}:{} (official worker transport, region={})",
        args.host,
        args.port,
        region.as_str()
    );
    // 只报路径与存在性，**绝不打印登录态内容**
    println!(
        "qoder-plugin: 登录态目录 {}（存在={}）；worker {}",
        state.auth_dir.display(),
        state.auth_dir.exists(),
        state
            .worker_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<未找到>".to_string())
    );

    let state = Arc::new(state);
    let app = router(state);
    let addr: std::net::SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(|e| format!("监听地址非法 {}:{}: {e}", args.host, args.port))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {} 被占用: {e}", args.port))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务退出: {e}"))
}

/// 路径辅助：本 crate 只用得到「数据目录拼接」，单独抽出来便于测试。
pub fn join_path(base: &Path, rel: &str) -> PathBuf {
    base.join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_matches_go_catalog() {
        assert_eq!(resolve_model(""), "Auto");
        assert_eq!(resolve_model("auto"), "Auto");
        assert_eq!(resolve_model("AUTO"), "Auto");
        assert_eq!(resolve_model("Qwen3.8-Max"), "qmodel_38max");
        assert_eq!(resolve_model("qmodel_38max"), "qmodel_38max");
        assert_eq!(resolve_model("Qwen3.7-Flash"), "q37fmodel");
        assert_eq!(resolve_model("GLM-5.3-Flash"), "gfmodel");
        assert_eq!(resolve_model("MiniMax-M3"), "mmodel");
        for gone in [
            "performance",
            "Cantus",
            "cmodel",
            "Kimi-K3",
            "kmodel_latest",
            "Ultimate",
            "Qwen3.8-Max-Preview",
            "not-a-model",
        ] {
            assert_eq!(resolve_model(gone), "", "{gone} 不在目录里必须拒绝");
        }
    }

    #[test]
    fn flatten_matches_go_shapes() {
        let msgs: Vec<Value> = serde_json::from_str(
            r#"[{"role":"system","content":"S"},{"role":"user","content":[{"type":"text","text":"A"},{"type":"text","text":"B"},{"type":"image"}]},{"role":"assistant","content":"C"},{"role":"tool","content":"T"}]"#,
        )
        .unwrap();
        assert_eq!(
            flatten_messages(&msgs),
            "[System]\nS\n[User]\nAB\n[Assistant]\nC\n[tool]\nT"
        );
        // 非对象项被跳过；没有 role 的走 default 分支
        let msgs: Vec<Value> = serde_json::from_str(r#"[1,{"content":"x"}]"#).unwrap();
        assert_eq!(flatten_messages(&msgs), "[]\nx");
        // 末尾换行被 TrimRight 掉
        let msgs: Vec<Value> = serde_json::from_str(r#"[{"role":"user","content":""}]"#).unwrap();
        assert_eq!(flatten_messages(&msgs), "[User]");
        assert_eq!(flatten_messages(&[]), "");
    }

    #[test]
    fn quota_error_patterns_match_go() {
        assert!(is_quota_error("ResourceUsageOverlimit"));
        assert!(is_quota_error("本月配额已用完"));
        assert!(is_quota_error("请升级专业版"));
        assert!(is_quota_error("免费限额用尽"));
        assert!(is_quota_error("auto-free-individual"));
        assert!(is_quota_error(r#"{"code":119}"#));
        assert!(is_quota_error("code=112"));
        assert!(!is_quota_error(""));
        assert!(!is_quota_error("普通网络错误"));
        assert!(!is_quota_error("code:111"));
    }

    #[test]
    fn chunk_and_complete_bytes_match_go_marshal() {
        assert_eq!(
            String::from_utf8(chunk_json("id1", "Auto", "hi", "", 7)).unwrap(),
            concat!(
                "{\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null,\"index\":0}],",
                "\"created\":7,\"id\":\"id1\",\"model\":\"Auto\",\"object\":\"chat.completion.chunk\"}"
            )
        );
        assert_eq!(
            String::from_utf8(chunk_reasoning_json("id1", "Auto", "想", "stop", 7)).unwrap(),
            concat!(
                "{\"choices\":[{\"delta\":{\"reasoning_content\":\"想\"},\"finish_reason\":\"stop\",\"index\":0}],",
                "\"created\":7,\"id\":\"id1\",\"model\":\"Auto\",\"object\":\"chat.completion.chunk\"}"
            )
        );
        assert_eq!(
            String::from_utf8(chunk_json("i", "m", "", "stop", 1)).unwrap(),
            concat!(
                "{\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0}],",
                "\"created\":1,\"id\":\"i\",\"model\":\"m\",\"object\":\"chat.completion.chunk\"}"
            )
        );
        assert_eq!(
            String::from_utf8(complete_json("i", "m", "c", 1)).unwrap(),
            concat!(
                "{\"choices\":[{\"finish_reason\":\"stop\",\"index\":0,",
                "\"message\":{\"content\":\"c\",\"role\":\"assistant\"}}],",
                "\"created\":1,\"id\":\"i\",\"model\":\"m\",\"object\":\"chat.completion\",",
                "\"usage\":{\"completion_tokens\":0,\"prompt_tokens\":0,\"total_tokens\":0}}"
            )
        );
    }

    #[test]
    fn write_error_shape_matches_go() {
        let resp = write_error(StatusCode::BAD_GATEWAY, "boom", "upstream_error");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn clean_path_matches_go_examples() {
        assert_eq!(clean_path("/v1/models"), "/v1/models");
        assert_eq!(clean_path("/health/"), "/health/");
        assert_eq!(clean_path("//v1/models"), "/v1/models");
        assert_eq!(clean_path("/a//b/./c/.."), "/a/b");
        assert_eq!(clean_path("/a//b/./c/../"), "/a/b/");
        assert_eq!(clean_path("/health/../v1/models"), "/v1/models");
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path(""), "/");
        assert_eq!(clean_path("/.."), "/");
    }

    #[test]
    fn query_param_takes_first_value() {
        let uri: Uri = "/health?refresh=1&x=2".parse().unwrap();
        assert_eq!(query_param(&uri, "refresh").as_deref(), Some("1"));
        let uri: Uri = "/health?x=2".parse().unwrap();
        assert_eq!(query_param(&uri, "refresh"), None);
        let uri: Uri = "/health?refresh=0&refresh=1".parse().unwrap();
        assert_eq!(query_param(&uri, "refresh").as_deref(), Some("0"));
        let uri: Uri = "/health?refresh=%31".parse().unwrap();
        assert_eq!(query_param(&uri, "refresh").as_deref(), Some("1"));
    }

    #[test]
    fn set_body_decoder_shapes() {
        assert_eq!(decode_set_body(b"{\"spent\":0}").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"{\"spent\":0.0}").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"{}").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"null").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"{\"spent\":null}").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"{\"Spent\":0}").unwrap(), 0.0);
        // Go 实测：后者生效（不是「精确匹配优先」）
        assert_eq!(decode_set_body(b"{\"spent\":1,\"SPENT\":2}").unwrap(), 2.0);
        assert!(decode_set_body(b"").unwrap_err().contains("EOF"));
        assert!(decode_set_body(b"{\"spent\":\"x\"}")
            .unwrap_err()
            .contains("Go struct field .spent of type float64"));
        assert_eq!(
            decode_set_body(b"[]").unwrap_err(),
            "json: cannot unmarshal array into Go value of type struct { Spent float64 \"json:\\\"spent\\\"\" }"
        );
        assert_eq!(
            decode_set_body(b"{not json").unwrap_err(),
            "invalid character 'n' looking for beginning of object key string"
        );
        // json.Decoder 只看第一个值：尾随数据不解析（真 Go 实测 `{"spent":0}x` 成功）
        assert_eq!(decode_set_body(b"{\"spent\":0}x").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"{\"spent\":1}x").unwrap(), 1.0);
        assert_eq!(decode_set_body(b"{\"spent\":0} {\"spent\":5}").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"nullx").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"{}x").unwrap(), 0.0);
        // 顶层类型错误的文案，尾随数据同理不看
        assert_eq!(
            decode_set_body(b"[]x").unwrap_err(),
            "json: cannot unmarshal array into Go value of type struct { Spent float64 \"json:\\\"spent\\\"\" }"
        );
        assert_eq!(
            decode_set_body(b"1x").unwrap_err(),
            "json: cannot unmarshal number into Go value of type struct { Spent float64 \"json:\\\"spent\\\"\" }"
        );
        // 数字溢出：Go 的 ParseFloat 报 ErrRange，文案里带**原字面量**（黄金 credits_set 锁住）
        assert_eq!(
            decode_set_body(b"{\"spent\":1e999}").unwrap_err(),
            "json: cannot unmarshal number 1e999 into Go struct field .spent of type float64"
        );
        assert_eq!(
            decode_set_body(b"{\"spent\":-1e999}").unwrap_err(),
            "json: cannot unmarshal number -1e999 into Go struct field .spent of type float64"
        );
        assert_eq!(
            decode_set_body(b"{\"spent\":1e309}").unwrap_err(),
            "json: cannot unmarshal number 1e309 into Go struct field .spent of type float64"
        );
        // 溢出即便后面还有会覆盖它的键，也先返回第一个错误（Go 实测一致）
        assert!(decode_set_body(b"{\"spent\":1e999,\"spent\":0}").is_err());
        // 下溢不是错误：Go 实测 `1e-999` 得 0 且 err=nil
        assert_eq!(decode_set_body(b"{\"spent\":1e-999}").unwrap(), 0.0);
        assert_eq!(decode_set_body(b"{\"spent\":5e-324}").unwrap(), 5e-324);
        // 未知键不计入（溢出也不会被碰）
        assert_eq!(decode_set_body(b"{\"other\":1e999}").unwrap(), 0.0);
    }

    #[test]
    fn sse_error_keeps_sse_headers_like_go() {
        let resp = sse_error("boom");
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
        assert_eq!(resp.headers().get("connection").unwrap(), "keep-alive");
    }

    // ---------- 区域（CN / INTL） ----------

    #[test]
    fn region_parse_defaults_to_cn() {
        assert_eq!(Region::parse("CN"), Region::Cn);
        assert_eq!(Region::parse(""), Region::Cn);
        assert_eq!(Region::parse("weird"), Region::Cn);
        assert_eq!(Region::parse("INTL"), Region::Intl);
        assert_eq!(Region::parse("intl"), Region::Intl);
        assert_eq!(Region::parse("international"), Region::Intl);
        // 与 rs-codebuddy 的 Region::parse 同口径：大小写以外的写法不认，回落 CN
        assert_eq!(Region::parse("Intl"), Region::Cn);
        assert_eq!(Region::as_str(Region::Cn), "CN");
        assert_eq!(Region::as_str(Region::Intl), "INTL");
    }

    /// 验收标准 3：**不带 `--region` 时三处默认值与改前逐字节一致**。
    #[test]
    fn default_region_is_cn_with_unchanged_defaults() {
        // CLI 默认：`--region` 缺省是 "CN"，`--usage-url` 缺省是 CN 常量
        let args = Args::try_parse_from(["rs-qoder"]).unwrap();
        assert_eq!(args.region, "CN");
        assert_eq!(Region::parse(&args.region), Region::Cn);
        assert_eq!(args.usage_url, "https://openapi.qoder.com.cn/api/v2/quota/usage");

        // 三处默认值：CN 与改前逐字相同（worker_rel 于 2026-10-20 按实测布局修正，
        // 见 `worker::WORKER_REL` 的注释；旧布局 `Qoder\Qoder CN\...` 仍作为兜底候选保留）
        let cn = Region::Cn.cfg();
        assert_eq!(cn.auth_dir_name, "com.qodercn.app.stable");
        assert_eq!(
            cn.worker_rel,
            r"Qoder CN\resources\app.asar.unpacked\node_modules\@qoder-ai\qoder-cn-agent-sdk\dist\_worker"
        );
        assert_eq!(cn.usage_url, "https://openapi.qoder.com.cn/api/v2/quota/usage");
        // 旧常量仍然指向 CN（外部引用/合同测试不受影响）
        assert_eq!(auth::AUTH_DIR_NAME, cn.auth_dir_name);
        assert_eq!(worker::WORKER_REL, cn.worker_rel);
        assert_eq!(usage::USAGE_URL, cn.usage_url);
        assert_eq!(worker::worker_rel(Region::Cn), worker::WORKER_REL);

        // 生产构造（默认 CN）：登录态目录仍是 `%APPDATA%\com.qodercn.app.stable`
        let st = AppState::from_env();
        assert_eq!(st.usage_url, cn.usage_url);
        assert!(st.auth_dir.ends_with("com.qodercn.app.stable"), "{:?}", st.auth_dir);
    }

    /// 验收标准 4：`--region INTL` 必须选对三处值。
    #[test]
    fn intl_region_picks_all_three_values() {
        let intl = Region::Intl.cfg();
        assert_eq!(intl.auth_dir_name, "com.qoder.app.stable");
        assert_eq!(
            intl.worker_rel,
            r"Qoder\resources\app.asar.unpacked\node_modules\@qoder-ai\qoder-agent-sdk\dist\_worker"
        );
        assert_eq!(intl.usage_url, "https://openapi.qoder.sh/api/v2/quota/usage");

        // 走 CLI 解析（`--region INTL` 不带覆盖参数）
        let args = Args::try_parse_from(["rs-qoder", "--region", "INTL"]).unwrap();
        let region = Region::parse(&args.region);
        assert_eq!(region, Region::Intl);
        let st = AppState::from_env_with(
            region,
            &Overrides {
                usage_url: Some(args.usage_url.clone()),
                ..Default::default()
            },
        );
        assert_eq!(st.usage_url, "https://openapi.qoder.sh/api/v2/quota/usage");
        assert!(st.auth_dir.ends_with("com.qoder.app.stable"), "{:?}", st.auth_dir);
        // worker 相对路径按区域换（候选顺序与命中逻辑由 worker.rs 的单测锁）
        assert_eq!(worker::worker_rel(Region::Intl), worker::WORKER_REL_INTL);
        // 两区的相对路径必须真的不同（否则「选对」无从谈起）
        assert_ne!(worker::worker_rel(Region::Cn), worker::worker_rel(Region::Intl));
        assert_ne!(Region::Cn.cfg().auth_dir_name, Region::Intl.cfg().auth_dir_name);
        assert_ne!(Region::Cn.cfg().usage_url, Region::Intl.cfg().usage_url);
    }

    /// 显式覆盖参数优先于 `--region`（四个覆盖口都要验）。
    #[test]
    fn explicit_overrides_beat_region() {
        let ov = Overrides {
            auth_dir: Some(PathBuf::from(r"C:\tmp\auth-override")),
            worker: Some(PathBuf::from(r"C:\tmp\worker-override.mjs")),
            credits_file: Some(PathBuf::from(r"C:\tmp\credits.json")),
            usage_url: Some("https://example.test/usage".to_string()),
        };
        for region in [Region::Cn, Region::Intl] {
            let st = AppState::from_env_with(region, &ov);
            assert_eq!(st.auth_dir, PathBuf::from(r"C:\tmp\auth-override"));
            assert_eq!(st.worker_path, Some(PathBuf::from(r"C:\tmp\worker-override.mjs")));
            assert_eq!(st.credits_path, Some(PathBuf::from(r"C:\tmp\credits.json")));
            assert_eq!(st.usage_url, "https://example.test/usage");
        }
        // 只覆盖一项时，其余两项仍按区域取（覆盖不是「全有或全无」）
        let st = AppState::from_env_with(
            Region::Intl,
            &Overrides {
                worker: Some(PathBuf::from(r"C:\tmp\w.mjs")),
                ..Default::default()
            },
        );
        assert_eq!(st.worker_path, Some(PathBuf::from(r"C:\tmp\w.mjs")));
        assert!(st.auth_dir.ends_with("com.qoder.app.stable"), "{:?}", st.auth_dir);
        assert_eq!(st.usage_url, "https://openapi.qoder.sh/api/v2/quota/usage");
    }

    #[test]
    fn usage_url_override_semantics() {
        // 未显式给 `--usage-url`（CLI 给的是 CN 默认值）⇒ 按区域取
        assert_eq!(
            resolve_usage_url(Region::Cn, usage::USAGE_URL),
            "https://openapi.qoder.com.cn/api/v2/quota/usage"
        );
        assert_eq!(
            resolve_usage_url(Region::Intl, usage::USAGE_URL),
            "https://openapi.qoder.sh/api/v2/quota/usage"
        );
        // 显式给了别的值 ⇒ 一律以它为准（两区都一样）
        for region in [Region::Cn, Region::Intl] {
            assert_eq!(
                resolve_usage_url(region, "https://example.test/u"),
                "https://example.test/u"
            );
        }
    }
}
