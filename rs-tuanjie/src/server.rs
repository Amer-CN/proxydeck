//! server.rs —— 团结服务的**骨架**：路由表 / CORS / `writeJSON` / `writeErr` 形态，
//! 以及各小轮已迁的本地端点
//! （`internal/tuanjie/server.go` + `handlers_extra.go` + `inflight.go` 的读出口）。
//!
//! ## 交付到哪一步了
//!
//! Go 侧路由表（`server.go:119-138`）共 19 条注册；逐条状态登记在
//! `README.md` 的「P8b 第三片路由表对照」里：
//!
//! * **已注册且已实现（每条有真 Go 对照门禁）**：`/health`、`/v1/models`（上游 200 透传
//!   与 502 两条）、`/v1/stats`、`/quota`、`/kimi-pacing`、`/accounts`（GET + `remove`/
//!   `toggle`/`setglm`/`setmodels`/未知 action 五条 POST 分支）、`/activity`、`/inflight`、
//!   **`/v1/chat/completions`**（P8b 第四片：池路径 + 单账号路径，见 [`crate::chat`]）、
//!   **`/account-models`**（逐账号模型矩阵）、**`/v1/models?full=true`**（全池取模型最多的
//!   那份列表）、**`/model/info`**（别名 × 部署元数据聚合）——后三条照
//!   `server.go:523-870` 移植，缓存口径同 Go（5 分钟 / 5 分钟 / 1 小时）。
//! * **已注册但显式 501**（属后续小轮，**不假装已通**）：`/accounts` 的 `action=add|auto`。
//! * **本版有意删除（2026-10-18 用户裁决）**：`/vision-config`、`/media-config`、
//!   `/v1/images/generations`、`/v1/videos*`、`/agnesapi` 不再注册 ⇒ 一律 404
//!   （生图/生视频与识图改路由整块下线）。识图改为：目标模型不在内置多模态清单而请求
//!   含图片/音频时直接回 400，不再改写模型。
//!
//! ## 骨架本身的合同（与 Go 逐条对齐）
//!
//! 1. **CORS**（`corsWith`，`server.go:104-115`）：所有响应带三个 `Access-Control-*` 头；
//!    `OPTIONS` 直接 204 且**不进 handler**。
//! 2. **`writeJSON`**（`server.go:2207-2210`）：`Content-Type: application/json`（无 charset）、
//!    `json.Encoder` 的**末尾换行**、键序按 Go 的 `json.Marshal` 口径（map 键按字节序、
//!    struct 按声明序）、浮点走 Go 的 `strconv` 口径（`2` 而不是 `2.0`）。
//! 3. **`writeErr`**（`server.go:2268-2274`）：`{"error":{"message":…,"type":"api_error"}}`
//!    + 指定状态码 + 末尾换行。
//! 4. **405 形态**：Go 的 `http.Error(w,"method not allowed",405)` 回纯文本
//!    `method not allowed\n` + `Content-Type: text/plain; charset=utf-8` +
//!    `X-Content-Type-Options: nosniff`（Go 1.26.5 的 `http.Error` 自带）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use crate::accounts::{AccountPool, AccountStatus};
use crate::gojson::format_go_float;
use crate::inflight::{ActivityEvent, ActivityLog, InflightReq, RequestRegistry};
use crate::pacing::Pacer;
use crate::quota::{
    fetch_quota_cached, Quota, QuotaCache, QuotaEndpoint, QuotaView, BILLING_SUMMARY_URL,
};
use crate::stats::Stats;
use crate::Error;
use crate::LITELLM_API_BASE;

/// Go `server.go:2207-2210` 的 `writeJSON`：`Content-Type` 无 charset、末尾带换行。
pub const JSON_CONTENT_TYPE: &str = "application/json";

/// Go `http.Error` 写的 405 体（`net/http/server.go` 的 `Error`）。
pub const METHOD_NOT_ALLOWED_BODY: &str = "method not allowed\n";
/// 同上：`http.Error` 的 Content-Type。
pub const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// 请求体上限：Go 的 `handleChat` 逐处用 `io.LimitReader` 自控；axum 默认 2MB 会先截住。
/// 取与 rs-bai / rs-comate 同口径的 32MB（Go `bufferBody`）。
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// `/health` 里 `cli_api_key` 的前缀长度 —— `server.go:258` 的 `key[:10]`。
pub const HEALTH_KEY_PREFIX: usize = 10;

/// 三个模型端点的内存缓存 —— Go 侧是三个包级变量（`server.go:330-345` 的
/// `accountModelsCache`/`fullModelsCache`、`:698-705` 的 `infoCache`）。
///
/// 值 = `(响应字节, 写入时刻)`，`None` = **从未成功拉取过**（与 Go 的
/// `len(cache)==0` / `infoCache == nil` 同义，失败时据此决定是否回缓存）。
/// 缓存的字节就是将要写出的响应体：`/account-models`、`?full=true` 存的是
/// `w.Write` 的原始字节（无末尾换行），`/model/info` 存的是 `writeJSON` 的完整行
/// （含末尾换行）。
#[derive(Default)]
pub struct ModelEndpointCache {
    pub account_models: std::sync::Mutex<Option<(Vec<u8>, Instant)>>,
    pub full_models: std::sync::Mutex<Option<(Vec<u8>, Instant)>>,
    pub model_info: std::sync::Mutex<Option<(Vec<u8>, Instant)>>,
}

impl ModelEndpointCache {
    /// 命中且未过期 → 克隆响应体。锁只在同步段内持有，**绝不跨 await**。
    fn fresh(
        slot: &std::sync::Mutex<Option<(Vec<u8>, Instant)>>,
        ttl: std::time::Duration,
    ) -> Option<Vec<u8>> {
        let g = slot.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some((body, at)) if at.elapsed() < ttl => Some(body.clone()),
            _ => None,
        }
    }

    /// 不论年龄，只要有上一次成功结果就取（Go 的 `cached != nil` 回退）。
    fn cached(slot: &std::sync::Mutex<Option<(Vec<u8>, Instant)>>) -> Option<Vec<u8>> {
        let g = slot.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|(body, _)| body.clone())
    }

    fn store(slot: &std::sync::Mutex<Option<(Vec<u8>, Instant)>>, body: Vec<u8>) {
        let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some((body, Instant::now()));
    }
}

/// 服务状态。
pub struct AppState {
    pub started_at: Instant,
    pub client: Arc<crate::client::Client>,
    /// 账号池：`Arc` 是因为 `handleChat` 的 402 分支要把它移进异步任务去查官网额度
    /// （对应 Go `go func(){ … s.pool.MarkBudgetExceededAt(…) }()`）。池内部本就是
    /// `Mutex<Inner>`，多持有一份句柄不改变并发语义。
    pub pool: Arc<AccountPool>,
    pub stats: Stats,
    pub pacer: Pacer,
    pub registry: Arc<RequestRegistry>,
    pub activity: Arc<ActivityLog>,
    pub quota_cache: Arc<QuotaCache>,
    /// 历史字段：媒体改路由已删（2026-10-18），该值恒 0；仅保留 `/accounts` 的既有
    /// JSON 合同（`media_reroutes` 键）不破坏下游。
    pub media_reroutes: AtomicI64,
    /// chat 路径的可注入时序（池路径 429 兜底的换号退避）。
    pub chat_timing: crate::chat::ChatTiming,
    /// 绝对基准库（`server.go:47` 的 `baselines`）——**启动时加载进内存**，
    /// 改/删条目必须重启才生效（CLAUDE.md 坑 15）。
    pub baselines: Arc<crate::probes::BaselineStore>,
    /// 远程图片下载客户端（`media.go:434` 的 `fetchImgClient`：15s 超时）。
    pub fetch_img_client: reqwest::Client,
    /// 注水检测器（被动观测 + 别名解析 + 金丝雀基线）—— `server.go:43` 的 `water`。
    pub water: Arc<crate::water::WaterCheck>,
    /// 探针成本/限流计数器 —— `probes.go:28-52` 的四个包级 atomic。
    pub probe_cost: crate::probes::ProbeCost,
    /// 疑似官方判定变更告警 —— `server.go:49` 的 `judgeAlert`（`noteUpstreamError/OK` 更新）。
    pub judge_alert: crate::chat::JudgeAlert,
    /// 官方 CLI 签名种子轮换告警 —— `server.go:50` 的 `seedAlert`（种子卫兵更新）。
    pub seed_guard: Arc<crate::seedcheck::SeedGuard>,
    /// 注水检测历史（内存环形 20 条）—— `server.go:53` 的 `waterHist`。
    pub water_history: crate::water_handler::WaterHistory,
    /// 三个模型端点的缓存（`/account-models`、`?full=true`、`/model/info`）。
    pub model_cache: ModelEndpointCache,
}

impl AppState {
    /// 生产构造 —— `NewServer()`（`server.go:75-84`）的等价物。
    pub fn new() -> crate::Result<Self> {
        Ok(AppState {
            started_at: Instant::now(),
            client: Arc::new(crate::client::Client::new()?),
            pool: Arc::new(AccountPool::new()),
            stats: Stats::new(),
            pacer: Pacer::new(),
            registry: Arc::new(RequestRegistry::new()),
            activity: Arc::new(ActivityLog::new()),
            quota_cache: Arc::new(QuotaCache::new()),
            media_reroutes: AtomicI64::new(0),
            chat_timing: crate::chat::ChatTiming::default(),
            baselines: Arc::new(crate::probes::BaselineStore::new()),
            fetch_img_client: reqwest::Client::builder()
                .timeout(crate::media::FETCH_IMG_TIMEOUT)
                .build()
                .map_err(crate::Error::Forward)?,
            water: Arc::new(crate::water::WaterCheck::new()),
            probe_cost: crate::probes::ProbeCost::new(),
            judge_alert: crate::chat::JudgeAlert::new(),
            seed_guard: Arc::new(crate::seedcheck::SeedGuard::new()),
            water_history: crate::water_handler::WaterHistory::new(),
            model_cache: ModelEndpointCache::default(),
        })
    }
}

// ---------------------------------------------------------------- 路由表

/// 路由表 —— `server.go:119-138`（已迁的那几条）。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // 与 Go 同形：Go 的 `http.ServeMux` 注册**不带方法**，方法判定全在 handler 里。
        // 挂成 `get(...)` 会让 POST 变 axum 的 405（体为空、无 nosniff），与 Go 分歧。
        .route("/health", any(handle_health))
        .route("/v1/models", any(handle_models))
        // 账号矩阵：逐账号拉上游模型清单（5 分钟缓存 / `?refresh=1` 旁路）。
        .route("/account-models", any(handle_account_models))
        // chat 转发（P8b 第四片）：池路径 + 单账号路径。
        .route("/v1/chat/completions", any(crate::chat::handle_chat))
        .route("/model/info", any(handle_model_info))
        .route("/v1/stats", any(handle_stats))
        .route("/quota", any(handle_quota))
        .route("/kimi-pacing", any(handle_kimi_pacing))
        .route("/accounts", any(handle_accounts))
        .route("/activity", any(handle_activity))
        .route("/inflight", any(handle_inflight))
        // 注水检测（P8c）：history/channels/quick 已实现，其余动作显式 501。
        .route("/water-probe", any(crate::water_handler::handle_water_probe))
        // 自定义服务商（`/providers`）与生图/生视频、识图改路由及三路媒体配置均已按
        // 用户裁决删除（2026-10-18）：不注册即 404（见 README 差异 #46 / #47）。
        // 未匹配路径：Go `http.ServeMux` 的 404 体（`http.NotFound` → `http.Error` 的
        // `"404 page not found\n"` + `text/plain; charset=utf-8` + nosniff）。
        // axum 默认回**空体**，不补就对不上。**未迁**：ServeMux 的路径清洗 307
        // 重定向（`//a` 一类 axum 回 404；见 README 已知差异 16）。
        .fallback(any(handle_not_found))
        .layer(axum::middleware::from_fn(cors))
        .layer(axum::middleware::from_fn(method_not_allowed_text))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// `http.NotFound` 的响应体 —— `net/http/server.go` 的 `Error(w, "404 page not found", 404)`。
pub const NOT_FOUND_BODY: &str = "404 page not found\n";

/// 未匹配路径的 404（Go `http.NotFound` → `http.Error`，后者**无条件**设
/// `text/plain; charset=utf-8` + `X-Content-Type-Options: nosniff`，
/// Go 1.26.5 `net/http/server.go:2315-2316`）。
async fn handle_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, TEXT_CONTENT_TYPE),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        NOT_FOUND_BODY,
    )
        .into_response()
}

/// `corsWith`（`server.go:104-115`）：三个头 + `OPTIONS` 直接 204。
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

/// 三个 CORS 头（`server.go:106-108`）。
pub fn add_cors(h: &mut HeaderMap) {
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

/// 补 axum 默认 405 的响应体，使形态与 Go 的 `http.Error(w,"method not allowed",405)` 一致。
pub async fn method_not_allowed_text(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(TEXT_CONTENT_TYPE),
        );
        resp.headers_mut()
            .insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
        *resp.body_mut() = Body::from(METHOD_NOT_ALLOWED_BODY);
    }
    resp
}

// ---------------------------------------------------------------- 响应写出

/// Go `encoding/json` 口径的 JSON 值（键序与数字形态都由这里定死）。
///
/// * `GJ::Obj` 用 `BTreeMap` —— 键按**字节序**写出，与 `json.Marshal(map[string]any)` 一致；
/// * `GJ::Float` 走 [`format_go_float`]（`2` 而非 `2.0`，`1e+21` 一类也同口径）；
/// * `GJ::Null` 写 `null`（Go 里 `Any(nil)`、`[]string(nil)` 都是这个形态）；
/// * 字符串走 `gojson::write_go_string`（含 `<`/`>`/`&` 的 HTML 转义）。
#[derive(Debug, Clone, PartialEq)]
pub enum GJ {
    Null,
    Bool(bool),
    /// 整数（Go 的 int/int64 原生形态）。
    Int(i64),
    /// 浮点（Go 的 float64 —— `strconv.AppendFloat` 口径）。
    Float(f64),
    Str(String),
    Arr(Vec<GJ>),
    /// Go `map[string]any` 形态：键按**字节序**写出（`json.Marshal` 对 map 的排序）。
    Obj(BTreeMap<String, GJ>),
    /// Go **结构体**形态：字段按**声明序**写出（`InflightReq`/`ActivityEvent` 一类）。
    /// 用错了顺序就会与 Go 差字节 —— 本片踩过一次（`/inflight` 的 rid 被排到 bytes 之后）。
    ObjOrdered(Vec<(String, GJ)>),
    /// 已经是 Go 口径的 JSON 片段（`Quota::to_go_json` 一类**逐字节复刻过**的写出器产物）。
    Raw(String),
}

impl GJ {
    /// 写进字节缓冲（无缩进）。
    pub fn write(&self, out: &mut Vec<u8>) {
        match self {
            GJ::Null => out.extend_from_slice(b"null"),
            GJ::Bool(true) => out.extend_from_slice(b"true"),
            GJ::Bool(false) => out.extend_from_slice(b"false"),
            GJ::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
            GJ::Float(f) => out.extend_from_slice(format_go_float(*f).as_bytes()),
            GJ::Str(s) => crate::gojson::write_go_string(out, s),
            GJ::Arr(items) => {
                out.push(b'[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    it.write(out);
                }
                out.push(b']');
            }
            GJ::Obj(map) => {
                out.push(b'{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    crate::gojson::write_go_string(out, k);
                    out.push(b':');
                    v.write(out);
                }
                out.push(b'}');
            }
            GJ::ObjOrdered(pairs) => {
                out.push(b'{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    crate::gojson::write_go_string(out, k);
                    out.push(b':');
                    v.write(out);
                }
                out.push(b'}');
            }
            GJ::Raw(s) => out.extend_from_slice(s.as_bytes()),
        }
    }

    /// 序列化成 `writeJSON` 的完整响应体（末尾补 `json.Encoder` 的换行）。
    pub fn to_go_json_line(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out.push(b'\n');
        out
    }
}

/// 建一个键序为字节序的对象（Go `map[string]any` 口径）。
pub fn gobj(pairs: Vec<(&str, GJ)>) -> GJ {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    GJ::Obj(m)
}

/// 建一个**按给定顺序**写出的对象（Go **结构体**口径：字段按声明序）。
pub fn gobj_ordered(pairs: Vec<(&str, GJ)>) -> GJ {
    GJ::ObjOrdered(
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// `writeJSON`（`server.go:2207-2210`）。
pub fn write_json(v: &GJ) -> Response {
    (
        [(header::CONTENT_TYPE, JSON_CONTENT_TYPE)],
        v.to_go_json_line(),
    )
        .into_response()
}

/// `writeErr`（`server.go:2268-2274`）。
pub fn write_err(status: StatusCode, msg: &str) -> Response {
    let body = gobj(vec![(
        "error",
        gobj(vec![
            ("message", GJ::Str(msg.to_string())),
            ("type", GJ::Str("api_error".to_string())),
        ]),
    )]);
    (
        status,
        [(header::CONTENT_TYPE, JSON_CONTENT_TYPE)],
        body.to_go_json_line(),
    )
        .into_response()
}

/// `http.Error(w,"method not allowed",405)`。
///
/// `allow`：Go 的 `handleChat` 在 405 前设 `Allow: POST`（`server.go:940`），
/// `handleAccounts` 不设（`handlers_extra.go:31-33`）——故按调用方区分，
/// 不得写死在共用函数里。
pub fn method_not_allowed(allow: Option<&str>) -> Response {
    let mut resp = (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::CONTENT_TYPE, TEXT_CONTENT_TYPE)],
        METHOD_NOT_ALLOWED_BODY,
    )
        .into_response();
    resp.headers_mut()
        .insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    if let Some(a) = allow {
        if let Ok(v) = HeaderValue::from_str(a) {
            resp.headers_mut().insert(header::ALLOW, v);
        }
    }
    resp
}

// ---------------------------------------------------------------- 端点

/// 拉一次 cli_api_key（`apiKeyCached`）——`/health` 的前置。
pub async fn api_key_cached(st: &AppState) -> Result<String, String> {
    st.client.api_key_cached().await.map_err(|e| go_err_text(&e))
}

/// Go 侧错误文案 = `err.Error()`。Rust 的 [`Error`] 用 `Display`，逐条与 Go 的文案对应
/// （见 `lib.rs` 的 `Error` 定义与其 `#[error(...)]` 文本）。
pub fn go_err_text(e: &Error) -> String {
    e.to_string()
}

/// `GET /health`（`server.go:248-278`）。
async fn handle_health(State(st): State<Arc<AppState>>) -> Response {
    let mut status = "ok".to_string();
    let mut message: Option<String> = None;
    let mut cli_api_key: Option<String> = None;
    match api_key_cached(&st).await {
        Ok(key) => cli_api_key = Some(health_key_prefix(&key)),
        Err(msg) => {
            status = "error".to_string();
            message = Some(msg);
        }
    }
    // judge/seed 告警（`server.go:261-275`）：judge 由 `noteUpstreamError/OK` 更新
    // （P8c 已接线），seed 由种子卫兵更新（P8c 已接线）。
    let judge = st.judge_alert.snapshot();
    let seed = st.seed_guard.alert.snapshot();
    let mut pairs: Vec<(&str, GJ)> = vec![
        ("status", GJ::Str(status)),
        ("service", GJ::Str("tuanjie-go".to_string())),
        ("uptimeSec", GJ::Int(st.started_at.elapsed().as_secs() as i64)),
    ];
    if let Some(msg) = message {
        pairs.push(("message", GJ::Str(msg)));
    }
    if let Some(key) = cli_api_key {
        pairs.push(("cli_api_key", GJ::Str(key)));
        // `backend` 取**产物常量**而非 client 的域：Go 侧是包级常量 `litellmAPIBase`
        // （`server.go:259`），与测试缝注入的 base 无关 —— 生产恒为
        // `https://codely-litellm.tuanjie.cn`，对照时也不该随假上游漂移。
        pairs.push(("backend", GJ::Str(LITELLM_API_BASE.to_string())));
    }
    pairs.push(("judgment_alert", GJ::Bool(judge.active)));
    pairs.push(("judgment_signal", GJ::Str(judge.signal)));
    pairs.push(("judgment_since", GJ::Int(judge.since_unix)));
    pairs.push(("seed_alert", GJ::Bool(seed.active)));
    pairs.push(("seed_signal", GJ::Str(seed.signal)));
    pairs.push(("seed_latest", GJ::Str(seed.latest)));
    pairs.push(("seed_online", GJ::Str(seed.online)));
    write_json(&gobj(pairs))
}

/// `key[:10] + "..."`（`server.go:258`）。
///
/// Go 对短于 10 字节的 key 会 panic（slice bounds out of range）——线上 key 恒长于 10
/// （JWT 形态），故不复制那个 panic，Rust 侧按字符取前 10。
pub fn health_key_prefix(key: &str) -> String {
    let head: String = key.chars().take(HEALTH_KEY_PREFIX).collect();
    format!("{head}...")
}

/// `GET /v1/models`（`handleModels` + `serveModels` 的**已迁部分**）。
///
/// 无 `?full=true` 时行为**逐字节保持原样**（上游 200 透传 / 502）；`?full=true`
/// 走 [`handle_models_full`]（Go `handleModelsFull`，`server.go:523-593`）。
async fn handle_models(State(st): State<Arc<AppState>>, uri: Uri) -> Response {
    let q = uri.query().unwrap_or("");
    if query_param(q, "full").as_deref() == Some("true") {
        let refresh = query_param(q, "refresh").as_deref() == Some("1");
        return handle_models_full(&st, refresh).await;
    }
    serve_models_plain(&st).await
}

/// 普通 `/v1/models`（`serveModels` 的 200/502 两条；与迁移时逐字节一致）。
async fn serve_models_plain(st: &AppState) -> Response {
    match st.client.forward_get("/v1/models").await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.bytes().await.unwrap_or_default();
            if status == reqwest::StatusCode::OK {
                return raw_ok(body.to_vec());
            }
            write_err(
                StatusCode::BAD_GATEWAY,
                &format!("上游 /v1/models 返回 {}", status.as_u16()),
            )
        }
        Err(e) => write_err(StatusCode::BAD_GATEWAY, &go_err_text(&e)),
    }
}

/// `w.Write(body)` 形态的 200：`application/json` + **原始字节**（不补换行）。
fn raw_ok(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, JSON_CONTENT_TYPE),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------- 模型三端点

/// `/account-models` 与 `?full=true` 的缓存时长 —— Go 的 `5 * time.Minute`
/// （`server.go:611`、`:548`）。
const MODELS_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// `/model/info` 的缓存时长 —— Go 的 `time.Hour`（`server.go:726`）。
const MODEL_INFO_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// 上游模型清单读取上限 —— Go 两处 `io.LimitReader(resp.Body, 2<<20)`。
const MODELS_BODY_LIMIT: usize = 2 << 20;

/// `ForwardDirect(ctx, http.MethodGet, path, nil, token, nil)`（`handlers_extra.go:211-226`）
/// 的 **GET 形态**：按指定账号的 access_token 现换 key，再用**同一套**
/// [`crate::headers::litellm_headers`]（签名凭证链）打 LiteLLM。
///
/// 与 chat 的 [`crate::chat::forward_direct`] 只差一点：后者写死 POST + 请求体（聊天路径
/// 专用），这里只做读路径。**签名算法/头集合仍是同一个函数产出的**，没有第二份实现。
async fn forward_direct_get(
    st: &AppState,
    path: &str,
    access_token: &str,
) -> Result<reqwest::Response, String> {
    let key = st
        .client
        .fetch_key_with_token(access_token)
        .await
        .map_err(|e| go_err_text(&e))?;
    let url = format!("{}{}", st.client.litellm_base(), path);
    let sess = crate::session::new_uuid_v4();
    let mut req = st.client.http().get(url);
    for (k, v) in crate::headers::litellm_headers(
        path,
        &key,
        &sess,
        crate::client::now_unix(),
        st.client.user_agent(),
    ) {
        req = req.header(k, v);
    }
    req.send()
        .await
        .map_err(|e| go_err_text(&crate::Error::Forward(e)))
}

/// `json.Unmarshal` 到结构体的 Go 语义（根是 `null` 时不报错、留零值）。
fn decode_go_struct<T: serde::de::DeserializeOwned + Default>(
    body: &[u8],
) -> Result<T, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_slice(body)?;
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v)
}

/// `GET /account-models`（`handleAccountModels`，`server.go:596-676`）。
///
/// 逐账号换 key 打官网 = N 账号 × 1 次官方请求，只在用户手势（开弹窗/展开/刷新）时
/// 触发；结果缓存 5 分钟，`?refresh=1` 旁路缓存。
///
/// 响应字段与 Go 的 `accountModelsEntry` **声明序逐字一致**：
/// `user_id/username(omitempty)/source(omitempty)/enabled/budget_exceeded/count/models/err(omitempty)`。
/// `count` 恒存在（GUI 矩阵读它显示「可用模型 N 个」），`models` 恒为数组（空时为 `[]`）。
async fn handle_account_models(State(st): State<Arc<AppState>>, uri: Uri) -> Response {
    let refresh = query_param(uri.query().unwrap_or(""), "refresh").as_deref() == Some("1");
    if !refresh {
        if let Some(body) =
            ModelEndpointCache::fresh(&st.model_cache.account_models, MODELS_CACHE_TTL)
        {
            return raw_ok(body);
        }
    }

    let ids = st.pool.sorted_user_ids();
    let mut entries: Vec<GJ> = Vec::with_capacity(ids.len());
    for uid in ids {
        let Some(acc) = st.pool.get(&uid) else {
            continue;
        };
        let mut models: Vec<String> = Vec::new();
        let mut err: Option<String> = None;
        if !acc.enabled || acc.budget_exceeded {
            err = Some("未参与调度（停用或配额用尽）".to_string());
        } else {
            let tok = st
                .pool
                .effective_access_token(&acc)
                .ok()
                .filter(|t| !t.is_empty());
            match tok {
                None => err = Some("取 token 失败".to_string()),
                Some(tok) => match forward_direct_get(&st, "/v1/models", &tok).await {
                    Err(e) => err = Some(e),
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        if status != 200 {
                            err = Some(format!("上游返回 {status}"));
                        } else {
                            let body =
                                crate::chat::read_body_bounded(resp, MODELS_BODY_LIMIT).await;
                            match decode_go_struct::<ModelsIdDoc>(&body) {
                                Err(_) => err = Some("上游响应解析失败".to_string()),
                                Ok(doc) => {
                                    for row in doc.data.unwrap_or_default().into_iter().flatten() {
                                        if !row.id.is_empty() {
                                            models.push(row.id);
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }

        // Go 的 struct 声明序（不是 map 的字节序）。
        let mut pairs: Vec<(&str, GJ)> = vec![("user_id", GJ::Str(acc.user_id.clone()))];
        if !acc.username.is_empty() {
            pairs.push(("username", GJ::Str(acc.username.clone())));
        }
        if !acc.source.is_empty() {
            pairs.push(("source", GJ::Str(acc.source.clone())));
        }
        pairs.push(("enabled", GJ::Bool(acc.enabled)));
        pairs.push(("budget_exceeded", GJ::Bool(acc.budget_exceeded)));
        pairs.push(("count", GJ::Int(models.len() as i64)));
        pairs.push((
            "models",
            GJ::Arr(models.into_iter().map(GJ::Str).collect()),
        ));
        if let Some(e) = err {
            pairs.push(("err", GJ::Str(e)));
        }
        entries.push(gobj_ordered(pairs));
    }

    // `json.Marshal(map[string]any{...})` —— 键按字节序：accounts / fetched_at / ok。
    let out = gobj(vec![
        ("ok", GJ::Bool(true)),
        ("fetched_at", GJ::Int(crate::client::now_unix())),
        ("accounts", GJ::Arr(entries)),
    ]);
    let mut body = Vec::new();
    out.write(&mut body); // `w.Write(out)`：无末尾换行
    ModelEndpointCache::store(&st.model_cache.account_models, body.clone());
    raw_ok(body)
}

/// `/account-models` 的 `json.Unmarshal` 目标（Go 的匿名 struct，只取 `data[].id`）。
#[derive(Default, serde::Deserialize)]
struct ModelsIdDoc {
    #[serde(default)]
    data: Option<Vec<Option<ModelIdRow>>>,
}

#[derive(Default, serde::Deserialize)]
struct ModelIdRow {
    #[serde(default)]
    id: String,
}

/// `GET /v1/models?full=true`（`handleModelsFull`，`server.go:523-593`）。
///
/// 遍历账号池，用各账号 token 拉 `/v1/models`，**取模型数最多的那份原始响应体**；
/// 5 分钟缓存，`?refresh=1` 旁路。所有账号都失败时回退到普通 `/v1/models` 路径
/// （Go 是 `s.serveModels(w, r, false)`；本片只迁了 200/502 两条，见 [`serve_models_plain`]）。
async fn handle_models_full(st: &AppState, refresh: bool) -> Response {
    if !refresh {
        if let Some(body) = ModelEndpointCache::fresh(&st.model_cache.full_models, MODELS_CACHE_TTL)
        {
            return raw_ok(body);
        }
    }

    let ids = st.pool.sorted_user_ids();
    let mut best: Option<Vec<u8>> = None;
    let mut best_count: i64 = -1;
    for uid in ids {
        let Some(acc) = st.pool.get(&uid) else {
            continue;
        };
        if !acc.enabled || acc.budget_exceeded {
            continue;
        }
        let Ok(tok) = st.pool.effective_access_token(&acc) else {
            continue;
        };
        if tok.is_empty() {
            continue;
        }
        let Ok(resp) = forward_direct_get(st, "/v1/models", &tok).await else {
            continue;
        };
        if resp.status().as_u16() != 200 {
            continue;
        }
        let body = crate::chat::read_body_bounded(resp, MODELS_BODY_LIMIT).await;
        let count = models_count(&body);
        if count > best_count {
            best_count = count;
            best = Some(body);
        }
    }

    if let Some(body) = best {
        ModelEndpointCache::store(&st.model_cache.full_models, body.clone());
        return raw_ok(body);
    }
    // 所有账号都失败 → 同 Go 的回退（普通拉取）。
    serve_models_plain(st).await
}

/// `json.Unmarshal(body, &struct{Data []json.RawMessage})` 后 `len(Data)`：
/// 解析失败/没有 `data` 数组时 Go 得 0，这里同样返回 0（0 > -1 仍会选中首份 body）。
fn models_count(body: &[u8]) -> i64 {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(v) => v
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| a.len() as i64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// `GET /model/info`（`handleModelInfo`，`server.go:709-870`）。
///
/// 1. 实时拉上游 `/v1/models`（用户可调用别名）；2. 实时拉上游 `/model/info`
/// （部署元数据）；3. 按去 `-internal`/`-public` 后缀的键把费率/上限/后端映射到别名。
///
/// **上游 `/model/info` 早已 403**（CLAUDE.md 记载）。Go 的处理是：只看传输错误，
/// 不看状态码；`/model/info` 的 403 HTML 会在 JSON 解析处失败 → **有旧缓存用旧缓存，
/// 否则 `writeErr(502, "解析 /model/info 失败: …")`**。这里逐条照搬——**绝不编造费率**。
async fn handle_model_info(State(st): State<Arc<AppState>>) -> Response {
    if let Some(body) = ModelEndpointCache::fresh(&st.model_cache.model_info, MODEL_INFO_CACHE_TTL)
    {
        return raw_ok(body);
    }

    // 1. 用户可调用别名（实时）
    let mresp = match st.client.forward_get("/v1/models").await {
        Ok(r) => r,
        Err(e) => return model_info_cached_or_502(&st, go_err_text(&e)),
    };
    let mbody = match mresp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => return model_info_cached_or_502(&st, format!("解析 /v1/models 失败: {e}")),
    };
    let ml: ModelsForInfoDoc = match decode_go_struct(&mbody) {
        Ok(v) => v,
        Err(e) => return model_info_cached_or_502(&st, format!("解析 /v1/models 失败: {e}")),
    };

    // 2. `/model/info` 部署元数据（按去后缀名索引）
    //    注意：与 Go 相同，**不检查状态码**——403 HTML 会在下面的解析处失败。
    let iresp = match st.client.forward_get("/model/info").await {
        Ok(r) => r,
        Err(e) => return model_info_cached_or_502(&st, go_err_text(&e)),
    };
    let ibody = match iresp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => return model_info_cached_or_502(&st, format!("解析 /model/info 失败: {e}")),
    };
    let raw: ModelInfoDoc = match decode_go_struct(&ibody) {
        Ok(v) => v,
        Err(e) => return model_info_cached_or_502(&st, format!("解析 /model/info 失败: {e}")),
    };

    // 聚合：部署变体去掉 -internal/-public 后作为键，同名合并、字段取非空、后端去重。
    let mut aggs: BTreeMap<String, InfoAgg> = BTreeMap::new();
    for dep in raw.data.unwrap_or_default().into_iter().flatten() {
        let key = strip_model_variant(&dep.model_name);
        let a = aggs.entry(key).or_default();
        let mi = dep.model_info.unwrap_or_default();
        if a.max_in.is_none() {
            a.max_in = mi.max_input_tokens;
        }
        if a.max_out.is_none() {
            a.max_out = mi.max_tokens;
        }
        if a.input_cost.is_none() {
            a.input_cost = mi.input_cost_per_token;
        }
        if a.output_cost.is_none() {
            a.output_cost = mi.output_cost_per_token;
        }
        if a.cache_cost.is_none() {
            a.cache_cost = mi.cache_read_input_token_cost;
        }
        if let Some(b) = dep.litellm_params.map(|p| p.model) {
            if !b.is_empty() && !a.backends.contains(&b) {
                a.backends.push(b);
            }
        }
    }

    // 3. 组装：别名（/v1/models）× 元数据（映射）+ max_model_len。
    let mut list: Vec<GJ> = Vec::new();
    for row in ml.data.unwrap_or_default().into_iter().flatten() {
        let id = row.id;
        let mut max_in = row.max_model_len;
        let mut max_out = None;
        let mut input_cost = None;
        let mut output_cost = None;
        let mut cache_cost = None;
        let mut backends: Vec<String> = Vec::new();
        if let Some(a) = aggs.get(&id) {
            if max_in.is_none() {
                max_in = a.max_in;
            }
            max_out = a.max_out;
            input_cost = a.input_cost;
            output_cost = a.output_cost;
            cache_cost = a.cache_cost;
            backends = a.backends.clone();
        }
        // `ModelInfo` 的 struct 声明序。
        let mut pairs: Vec<(&str, GJ)> = vec![("name", GJ::Str(id.clone()))];
        if let Some(v) = max_in {
            pairs.push(("maxInput", GJ::Int(v)));
        }
        if let Some(v) = max_out {
            pairs.push(("maxOutput", GJ::Int(v)));
        }
        if let Some(v) = input_cost {
            pairs.push(("inputCost", GJ::Float(v * 1e6)));
        }
        if let Some(v) = output_cost {
            pairs.push(("outputCost", GJ::Float(v * 1e6)));
        }
        if let Some(v) = cache_cost {
            pairs.push(("cacheReadCost", GJ::Float(v * 1e6)));
        }
        if !backends.is_empty() {
            pairs.push((
                "backends",
                GJ::Arr(backends.into_iter().map(GJ::Str).collect()),
            ));
        }
        let reasoning = reasoning_tiers_for(&id);
        if !reasoning.is_empty() {
            pairs.push(("reasoning", GJ::Str(reasoning.to_string())));
        }
        let note = alias_note(&id);
        if !note.is_empty() {
            pairs.push(("note", GJ::Str(note.to_string())));
        }
        list.push(gobj_ordered(pairs));
    }

    let body = GJ::Arr(list).to_go_json_line(); // writeJSON：末尾带换行
    ModelEndpointCache::store(&st.model_cache.model_info, body.clone());
    raw_ok(body)
}

/// `/model/info` 失败时的 Go 口径：`if cached != nil` 用旧缓存，否则 502 `writeErr`。
fn model_info_cached_or_502(st: &AppState, msg: String) -> Response {
    match ModelEndpointCache::cached(&st.model_cache.model_info) {
        Some(body) => raw_ok(body),
        None => write_err(StatusCode::BAD_GATEWAY, &msg),
    }
}

/// `/v1/models`（handleModelInfo 用）：`data[].{id,max_model_len}`。
#[derive(Default, serde::Deserialize)]
struct ModelsForInfoDoc {
    #[serde(default)]
    data: Option<Vec<Option<ModelAliasRow>>>,
}

#[derive(Default, serde::Deserialize)]
struct ModelAliasRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    max_model_len: Option<i64>,
}

/// 上游 `/model/info` 的 `{"data":[…]}`。
#[derive(Default, serde::Deserialize)]
struct ModelInfoDoc {
    #[serde(default)]
    data: Option<Vec<Option<DeploymentRow>>>,
}

#[derive(Default, serde::Deserialize)]
struct DeploymentRow {
    #[serde(default)]
    model_name: String,
    #[serde(default)]
    litellm_params: Option<LiteParamsRow>,
    #[serde(default)]
    model_info: Option<ModelInfoFields>,
}

#[derive(Default, serde::Deserialize)]
struct LiteParamsRow {
    #[serde(default)]
    model: String,
}

#[derive(Default, serde::Deserialize)]
struct ModelInfoFields {
    #[serde(default)]
    max_tokens: Option<i64>,
    #[serde(default)]
    max_input_tokens: Option<i64>,
    #[serde(default)]
    input_cost_per_token: Option<f64>,
    #[serde(default)]
    output_cost_per_token: Option<f64>,
    #[serde(default)]
    cache_read_input_token_cost: Option<f64>,
}

/// 一个去后缀名的聚合桶（Go 的匿名 `agg`）。
#[derive(Default)]
struct InfoAgg {
    max_in: Option<i64>,
    max_out: Option<i64>,
    input_cost: Option<f64>,
    output_cost: Option<f64>,
    cache_cost: Option<f64>,
    backends: Vec<String>,
}

/// `strings.TrimSuffix(strings.TrimSuffix(name, "-internal"), "-public")`。
fn strip_model_variant(name: &str) -> String {
    let s = name.strip_suffix("-internal").unwrap_or(name);
    s.strip_suffix("-public").unwrap_or(s).to_string()
}

/// `reasoningTiersFor`（`server.go:878-887`）：仅这三个模型声明 `max/high/low`。
fn reasoning_tiers_for(model: &str) -> &'static str {
    match model {
        "codely-core" | "KIMI-K3" | "GLM-5.3-FLASH" => "max/high/low",
        _ => "",
    }
}

/// `aliasNote`（`server.go:890-908`）——**逐字**照搬 Go 的实测说明。
fn alias_note(name: &str) -> &'static str {
    match name {
        "codely-air" => "✓可用 · 后端: DeepSeek-V4.1-Flash · ~75 积分/1M token",
        "codely-basic" => "✓可用 · 后端: DeepSeek-V4.1-Flash · ~64 积分/1M token",
        "codely-flash" => "✓可用 · 后端: DeepSeek-V4.1-Flash · ~129 积分/1M token",
        "codely-core" => {
            "✓可用 · 后端: glm-5-2-260617 · ~91 积分/1M token（浅推理，单日波动 47-233）"
        }
        "codely-vl" => "✓可用 · 后端: qwen3.5-397b-a17b · ~405 积分/1M token · 视觉模型",
        "GLM-5.2" => "✗锁定 · 被团结后端锁定，改用 codely-core",
        "GLM-5.3" => "✓可用 · 输入3.2/输出11.2 积分/1M · 默认推理档",
        "KIMI-K3" => "✓可用 · ⚠高价 · 16积分/1M输入 · 输出80 · flash的10倍 · 慎用于长上下文",
        "codely-air-public" => "✗锁定 · 公开变体被团结后端锁定",
        "codely-basic-public" => "✗锁定 · 公开变体被团结后端锁定",
        "codely-core-public" => "✗锁定 · 公开变体被团结后端锁定",
        "codely-flash-public" => "✗锁定 · 公开变体被团结后端锁定",
        "codely-vl-public" => "✗锁定 · 公开变体被团结后端锁定",
        "deepseek-v4-flash-0731-baidu" => "✗锁定 · 百度变体被团结后端锁定",
        _ => "",
    }
}

/// `GET /v1/stats`（`handleStats`，`server.go:1852-1864`）——不看方法。
async fn handle_stats(State(st): State<Arc<AppState>>) -> Response {
    let models = st.stats.snapshot_obj();
    write_json(&gobj(vec![
        ("models", GJ::Obj(models)),
        (
            "uptimeSec",
            GJ::Int(st.started_at.elapsed().as_secs() as i64),
        ),
    ]))
}

/// `GET|POST /quota`（`handleQuota`，`server.go:1869-1895`）。
///
/// **参数名以 Go 现行实现为准**：读的是 `user_id`（不是 `uid`）；`?refresh=1` 旁路缓存。
/// 三个分支与三条文案逐条对应（账号不在池中 / 读取 token 失败 / 正常拉取）。
async fn handle_quota(State(st): State<Arc<AppState>>, uri: Uri) -> Response {
    let q = uri.query().unwrap_or("");
    let uid = query_param(q, "user_id").unwrap_or_default();
    let refresh = query_param(q, "refresh").as_deref() == Some("1");
    let quota = quota_for(&st, &uid, refresh).await;
    write_json(&GJ::Raw(quota.to_go_json()))
}

/// `/quota` 的取数口径（`server.go:1870-1894`）。
pub async fn quota_for(st: &AppState, uid: &str, refresh: bool) -> Quota {
    let ep = QuotaEndpoint {
        url: quota_endpoint_url(&st.client),
        ..QuotaEndpoint::default()
    };
    if uid.is_empty() {
        let at = match crate::credentials::load_access_token() {
            Ok(t) => t,
            Err(e) => return Quota::none(go_err_text(&e)),
        };
        return fetch_quota_cached(&st.quota_cache, st.client.http(), &ep, &at, refresh).await;
    }
    let Some(acc) = st.pool.get(uid) else {
        return QuotaView::not_in_pool();
    };
    let tok = match st.pool.effective_access_token(&acc) {
        Ok(t) => t,
        Err(e) => return QuotaView::token_error(&e),
    };
    fetch_quota_cached(&st.quota_cache, st.client.http(), &ep, &tok, refresh).await
}

/// 计费端点 URL：测试缝（假上游）走 client 的 codey base，生产恒为 [`BILLING_SUMMARY_URL`]。
pub fn quota_endpoint_url(client: &crate::client::Client) -> String {
    let base = client.codey_base();
    if base == crate::CODEY_API_BASE {
        BILLING_SUMMARY_URL.to_string()
    } else {
        format!("{base}/api/user/billing/usage/summary")
    }
}

/// `GET|POST /kimi-pacing`（`handleKimiPacing`，`server.go:1900-1916`）。
async fn handle_kimi_pacing(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if req.method() == Method::POST {
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap_or_default();
        let Some(enabled) = parse_enabled(&body) else {
            return write_err(StatusCode::BAD_REQUEST, "body 需为 {\"enabled\":true|false}");
        };
        st.pacer.set_enabled(enabled);
        crate::logf(&format!("[tuanjie] pacing 开关切换 enabled={enabled}"));
    }
    write_json(&gobj(vec![
        ("enabled", GJ::Bool(st.pacer.enabled())),
        ("pending", GJ::Int(st.pacer.pending())),
    ]))
}

/// `json.Decoder.Decode` 到 `struct{Enabled *bool}` 的等价判据（`server.go:1902-1907`）：
/// 只读第一个 JSON 值（尾随数据容忍——Go 的 `Decode` 读完一个值就停），
/// 再要 `enabled` 为 JSON bool（缺失/非 bool/`null` → `None` → 400，与 Go
/// 解到 `*bool` 保持 nil 的语义一致）。
pub fn parse_enabled(body: &[u8]) -> Option<bool> {
    let v = first_json_value(body)?;
    v.get("enabled")?.as_bool()
}

/// Go `json.Decoder.Decode` 的"读一个值"语义：`serde_json::from_slice` 要求整个
/// 输入恰好是一个值（尾随 `x` 即错），而 Go 读完第一个值就停、后面不管。
/// 用流式迭代器只取第一个值，其余字节原样忽略。
pub(crate) fn first_json_value(body: &[u8]) -> Option<serde_json::Value> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()
}

/// `handleAccounts` 的请求体判据 —— Go `json.NewDecoder(r.Body).Decode(&req struct{…})`
/// （`handlers_extra.go:35-49`）的等价实现：读第一个值（尾随容忍）；顶层须是对象
/// （`null` 视为全缺省——Go 解 null 进非指针 struct 是无操作；数字/串/数组/布尔
/// 则解码失败）；8 个已知字段出现时必须类型对得上（string / bool / []string，
/// 显式 `null` 视为缺省——Go 对 null 字段不报错），未知字段忽略（Go 同）。
/// 任一不满足 → `None`（调用方回 `{"ok":false,"msg":"请求体解析失败"}`）。
fn decode_accounts_body(body: &[u8]) -> Option<serde_json::Value> {
    let v = first_json_value(body)?;
    let o = match &v {
        serde_json::Value::Null => return Some(v),
        serde_json::Value::Object(o) => o,
        _ => return None,
    };
    for k in ["action", "user_id", "token", "username", "org_id"] {
        if let Some(f) = o.get(k) {
            if !f.is_string() && !f.is_null() {
                return None;
            }
        }
    }
    for k in ["enabled", "has_glm53"] {
        if let Some(f) = o.get(k) {
            if !f.is_boolean() && !f.is_null() {
                return None;
            }
        }
    }
    if let Some(f) = o.get("models") {
        let ok = f.is_null()
            || f.as_array()
                .map(|a| a.iter().all(|x| x.is_string() || x.is_null()))
                .unwrap_or(false);
        if !ok {
            return None;
        }
    }
    Some(v)
}

/// `GET|POST /accounts`（`handleAccounts`，`handlers_extra.go:19-86`）。
async fn handle_accounts(State(st): State<Arc<AppState>>, req: axum::extract::Request) -> Response {
    let method = req.method().clone();
    if method == Method::GET {
        st.pool.ensure_local_account();
        let accounts: Vec<GJ> = st.pool.status().iter().map(status_to_gj).collect();
        let mode = if st.pool.size() > 0 { "pool" } else { "single" };
        // passive：注水被动事件（`handlers_extra.go:26` 的 `s.water.PassiveEvents()`）——
        // 结构体声明序 `at/model/returned/user_id` 与 Go 逐个对应。
        let passive: Vec<GJ> = st
            .water
            .passive_events()
            .iter()
            .map(|e| {
                gobj_ordered(vec![
                    ("at", GJ::Str(e.at.clone())),
                    ("model", GJ::Str(e.model.clone())),
                    ("returned", GJ::Str(e.returned.clone())),
                    ("user_id", GJ::Str(e.user_id.clone())),
                ])
            })
            .collect();
        return write_json(&gobj(vec![
            ("ok", GJ::Bool(true)),
            ("accounts", GJ::Arr(accounts)),
            ("mode", GJ::Str(mode.to_string())),
            ("passive", GJ::Arr(passive)),
            (
                "media_reroutes",
                GJ::Int(st.media_reroutes.load(Ordering::SeqCst)),
            ),
        ]));
    }
    if method != Method::POST {
        return method_not_allowed(None);
    }
    let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap_or_default();
    let Some(reqj) = decode_accounts_body(&body) else {
        return write_json(&gobj(vec![
            ("ok", GJ::Bool(false)),
            ("msg", GJ::Str("请求体解析失败".to_string())),
        ]));
    };
    let action = reqj.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let user_id = reqj
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match action {
        "remove" => {
            // Go `handlers_extra.go:64-68`：删本地账号先拦。读的是**实时**登录态的
            // sub（未登录返回空串——此时空 user_id 同样被拦，与 Go 的 `==` 语义一致）。
            let local_sub = crate::credentials::load_access_token()
                .ok()
                .map(|t| crate::accounts::jwt_sub(&t))
                .unwrap_or_default();
            if user_id == local_sub {
                return write_json(&gobj(vec![
                    ("ok", GJ::Bool(false)),
                    ("msg", GJ::Str("本地账号不可删除".to_string())),
                ]));
            }
            write_json(&gobj(vec![("ok", GJ::Bool(st.pool.remove(&user_id)))]))
        }
        "toggle" => {
            let enabled = reqj
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            write_json(&gobj(vec![(
                "ok",
                GJ::Bool(st.pool.toggle(&user_id, enabled)),
            )]))
        }
        "setglm" => {
            let has = reqj
                .get("has_glm53")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            write_json(&gobj(vec![("ok", GJ::Bool(st.pool.set_glm(&user_id, has)))]))
        }
        "setmodels" => {
            if user_id.is_empty() {
                return write_json(&gobj(vec![
                    ("ok", GJ::Bool(false)),
                    ("msg", GJ::Str("缺少 user_id".to_string())),
                ]));
            }
            // `decode_accounts_body` 已保证数组元素只会是 string 或 null；
            // null 映射成 ""（Go 解 null 进 string 切片元素是零值、**不报错**）。
            let models: Vec<String> = reqj
                .get("models")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .map(|x| x.as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default();
            write_json(&gobj(vec![(
                "ok",
                GJ::Bool(st.pool.set_models(&user_id, &models)),
            )]))
        }
        // add / auto：都要换 key 预验证（`accountTokenInvalid`）或 CDP 探测，属后续小轮。
        "add" | "auto" => write_err(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented_in_pilot: action=add/auto 属后续小轮（换 key 预验证 / CDP 探测）",
        ),
        _ => write_json(&gobj(vec![
            ("ok", GJ::Bool(false)),
            ("msg", GJ::Str("未知 action".to_string())),
        ])),
    }
}

/// `AccountStatus` → `/accounts` 条目（`accountList`，`handlers_extra.go:163-192`）。
///
/// 本片点名要求的「经 gojson 口径写出」就落在这里：`token_remaining_hours` 是 float64，
/// Go 写 `2` 而不是 `2.0`；键序按 Go `map[string]any` 的字节序；本地账号的 `username`
/// 被改写成「本地客户端」、`source` 空值补成 `pool`、`models` 恒为数组（永不 `null`）。
pub fn status_to_gj(s: &AccountStatus) -> GJ {
    let source = if s.source.is_empty() {
        "pool".to_string()
    } else {
        s.source.clone()
    };
    let username = if source == "local" {
        "本地客户端".to_string()
    } else {
        s.username.clone()
    };
    let models = if s.models.is_empty() {
        // Go：`func() []string { if st.Models == nil { return []string{} }; return st.Models }()`
        GJ::Arr(Vec::new())
    } else {
        GJ::Arr(s.models.iter().map(|m| GJ::Str(m.clone())).collect())
    };
    gobj(vec![
        ("user_id", GJ::Str(s.user_id.clone())),
        ("username", GJ::Str(username)),
        ("org_id", GJ::Str(s.org_id.clone())),
        ("enabled", GJ::Bool(s.enabled)),
        ("use_count", GJ::Int(s.use_count)),
        ("last_used", GJ::Str(s.last_used.clone())),
        ("token_expires", GJ::Str(s.token_expires.clone())),
        (
            "token_remaining_hours",
            GJ::Float(s.token_remaining_hours),
        ),
        ("has_glm53", GJ::Bool(s.has_glm53)),
        ("budget_exceeded", GJ::Bool(s.budget_exceeded)),
        ("inflight", GJ::Int(s.inflight)),
        ("source", GJ::Str(source)),
        ("models", models),
    ])
}

/// `GET /activity`（`handleActivity`，`handlers_extra.go:377-385`）。
async fn handle_activity(State(st): State<Arc<AppState>>, uri: Uri) -> Response {
    let mut limit = 60usize;
    if let Some(l) = query_param(uri.query().unwrap_or(""), "limit") {
        if let Ok(n) = l.parse::<i64>() {
            if n > 0 && n <= 200 {
                limit = n as usize;
            }
        }
    }
    let events: Vec<GJ> = st.activity.list(limit).iter().map(activity_to_gj).collect();
    write_json(&gobj(vec![
        ("ok", GJ::Bool(true)),
        ("events", GJ::Arr(events)),
    ]))
}

/// 一条动态事件 → Go `ActivityEvent` 的**声明序** + `omitempty` 口径
/// （`inflight.go:144-154`：seq / time / kind / message / model? / user_id? /
/// latency_ms? / tokens? / status?）。
pub fn activity_to_gj(e: &ActivityEvent) -> GJ {
    let mut pairs: Vec<(&str, GJ)> = vec![
        ("seq", GJ::Int(e.seq)),
        ("time", GJ::Str(e.time.clone())),
        ("kind", GJ::Str(e.kind.clone())),
        ("message", GJ::Str(e.message.clone())),
    ];
    if !e.model.is_empty() {
        pairs.push(("model", GJ::Str(e.model.clone())));
    }
    if !e.user_id.is_empty() {
        pairs.push(("user_id", GJ::Str(e.user_id.clone())));
    }
    if e.latency_ms != 0 {
        pairs.push(("latency_ms", GJ::Int(e.latency_ms)));
    }
    if e.tokens != 0 {
        pairs.push(("tokens", GJ::Int(e.tokens)));
    }
    if e.status != 0 {
        pairs.push(("status", GJ::Int(e.status as i64)));
    }
    gobj_ordered(pairs)
}

/// `GET /inflight`（`handleInflight`，`handlers_extra.go:388-390`）。
async fn handle_inflight(State(st): State<Arc<AppState>>) -> Response {
    maybe_seed_test_inflight(&st);
    let reqs: Vec<GJ> = st.registry.inflight().iter().map(inflight_to_gj).collect();
    write_json(&gobj(vec![
        ("ok", GJ::Bool(true)),
        ("requests", GJ::Arr(reqs)),
    ]))
}

/// **测试缝**（`RS_TUANJIE_TEST_INFLIGHT` 点名一个**触发器文件**；文件存在才塞）：
/// 给 `/inflight` 预置一条合成进行中请求，让 `elapsed`/`idle` 两个 float64 字段在
/// 差分对照里有非空取值（空数组时字段形状差异看不出来 —— 踩过一次：把 elapsed
/// 改成字符串的突变没被抓到）。
///
/// 用「文件触发器」而不是启动时一次性注入：对照序列里 `/inflight` 有两条用例
/// （空 → 有一条），注入时机由 runner 在用例之间落文件控制；rid 固定
/// `000000000001`（`rid_from_n(1)`），Go 侧采集器塞同一条。生产不带该变量。
fn maybe_seed_test_inflight(st: &AppState) {
    let Ok(trigger) = std::env::var("RS_TUANJIE_TEST_INFLIGHT") else {
        return;
    };
    if trigger.is_empty() || !std::path::Path::new(&trigger).exists() {
        return;
    }
    st.registry.insert_for_test(
        &crate::inflight::rid_from_n(1),
        "codely-core",
        "uid-p8b3-synthetic",
        true,
        7,
    );
}

/// 一条进行中请求 → Go `InflightReq` 的**声明序**（`inflight.go:12-20`：
/// rid / model / user_id / stream / elapsed / idle / bytes）。
pub fn inflight_to_gj(r: &InflightReq) -> GJ {
    gobj_ordered(vec![
        ("rid", GJ::Str(r.rid.clone())),
        ("model", GJ::Str(r.model.clone())),
        ("user_id", GJ::Str(r.user_id.clone())),
        ("stream", GJ::Bool(r.stream)),
        ("elapsed", GJ::Float(r.elapsed)),
        ("idle", GJ::Float(r.idle)),
        ("bytes", GJ::Int(r.bytes)),
    ])
}

// ---------------------------------------------------------------- 查询串

/// 取一个查询参数（Go `r.URL.Query().Get(k)`：同名取第一个，无则空串）。
/// `+` 解码成空格、`%XX` 解码 —— 与 `net/url` 的 `Query()` 一致。
pub fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
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
                let h = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                match (h(b[i + 1]), h(b[i + 2])) {
                    (Some(a), Some(c)) => {
                        out.push(a * 16 + c);
                        i += 3;
                    }
                    _ => {
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
    String::from_utf8_lossy(&out).into_owned()
}

/// 生产上游 base（`main.rs` 的日志与默认值用）。
pub fn default_litellm_base() -> &'static str {
    LITELLM_API_BASE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountStatus;

    #[test]
    fn write_json_line_has_trailing_newline_like_go_encoder() {
        // Go `json.NewEncoder(w).Encode(v)` 会在 JSON 后补一个 '\n'
        let v = gobj(vec![("ok", GJ::Bool(true))]);
        assert_eq!(v.to_go_json_line(), b"{\"ok\":true}\n");
    }

    #[test]
    fn map_keys_are_byte_sorted_like_go_map() {
        // `json.Marshal(map[string]any)`：键按字节序
        let v = gobj(vec![
            ("media_reroutes", GJ::Int(0)),
            ("ok", GJ::Bool(true)),
            ("mode", GJ::Str("pool".into())),
            ("accounts", GJ::Arr(vec![])),
            ("passive", GJ::Arr(vec![])),
        ]);
        assert_eq!(
            String::from_utf8(v.to_go_json_line()).unwrap(),
            "{\"accounts\":[],\"media_reroutes\":0,\"mode\":\"pool\",\"ok\":true,\"passive\":[]}\n"
        );
    }

    #[test]
    fn struct_fields_keep_declaration_order() {
        // `json.Marshal(struct)`：字段按**声明序**（不是字节序）——inflight_to_gj 用它
        let r = InflightReq {
            rid: "000000000001".into(),
            model: "codely-core".into(),
            user_id: "uid-x".into(),
            stream: true,
            elapsed: 1.5,
            idle: 0.25,
            bytes: 7,
        };
        assert_eq!(
            String::from_utf8(inflight_to_gj(&r).to_go_json_line()).unwrap(),
            "{\"rid\":\"000000000001\",\"model\":\"codely-core\",\"user_id\":\"uid-x\",\
             \"stream\":true,\"elapsed\":1.5,\"idle\":0.25,\"bytes\":7}\n"
        );
    }

    #[test]
    fn account_status_float_is_written_without_trailing_zero() {
        // 简报点名：f64 写 `2` 不写 `2.0`（Go 的 strconv 口径）
        let s = AccountStatus {
            user_id: "uid".into(),
            username: "u".into(),
            org_id: String::new(),
            enabled: true,
            use_count: 0,
            last_used: String::new(),
            token_expires: "未知".into(),
            token_remaining_hours: 2.0,
            has_glm53: true,
            budget_exceeded: false,
            inflight: 0,
            source: "local".into(),
            models: vec![],
        };
        let out = String::from_utf8(status_to_gj(&s).to_go_json_line()).unwrap();
        assert!(out.contains("\"token_remaining_hours\":2,"), "{out}");
        assert!(!out.contains("2.0"), "{out}");
        // `source == "local"` ⇒ username 改写成「本地客户端」（`handlers_extra.go:171-174`）
        assert!(out.contains("\"username\":\"本地客户端\""), "{out}");
        // `models` 恒为数组，永不 null
        assert!(out.contains("\"models\":[]"), "{out}");
        // 键按字节序（Go 那边是 map[string]any）
        assert!(out.starts_with("{\"budget_exceeded\":"), "{out}");
    }

    #[test]
    fn activity_omits_empty_optional_fields_in_declaration_order() {
        let e = ActivityEvent {
            seq: 1,
            time: "10:00:00".into(),
            kind: "ok".into(),
            message: "done".into(),
            model: "codely-core".into(),
            user_id: String::new(),
            latency_ms: 12,
            tokens: 0,
            status: 200,
        };
        assert_eq!(
            String::from_utf8(activity_to_gj(&e).to_go_json_line()).unwrap(),
            "{\"seq\":1,\"time\":\"10:00:00\",\"kind\":\"ok\",\"message\":\"done\",\
             \"model\":\"codely-core\",\"latency_ms\":12,\"status\":200}\n"
        );
    }

    #[test]
    fn query_param_parses_like_go_url_query() {
        assert_eq!(query_param("a=1&b=2", "b").as_deref(), Some("2"));
        assert_eq!(query_param("uid=x", "user_id"), None);
        assert_eq!(query_param("user_id=", "user_id").as_deref(), Some(""));
        // `+` 解成空格、`%XX` 解字节（net/url 口径）
        assert_eq!(query_param("k=a+b", "k").as_deref(), Some("a b"));
        assert_eq!(query_param("k=%E7%A7%AF", "k").as_deref(), Some("积"));
        // 同名取第一个
        assert_eq!(query_param("k=1&k=2", "k").as_deref(), Some("1"));
    }

    #[test]
    fn parse_enabled_rejects_missing_or_non_bool_like_go_pointer_decode() {
        assert_eq!(parse_enabled(br#"{"enabled":true}"#), Some(true));
        assert_eq!(parse_enabled(br#"{"enabled":false}"#), Some(false));
        assert_eq!(parse_enabled(br#"{"enabled":"yes"}"#), None);
        assert_eq!(parse_enabled(br#"{}"#), None);
        assert_eq!(parse_enabled(b"{not json"), None);
        assert_eq!(parse_enabled(b""), None);
        // Go `Decoder.Decode` 读完第一个值就停：尾随垃圾不算错（P8b 审查 R3）。
        assert_eq!(parse_enabled(br#"{"enabled":true}x"#), Some(true));
        assert_eq!(parse_enabled(br#"{"enabled":true} {"enabled":false}"#), Some(true));
    }

    #[test]
    fn health_key_prefix_matches_go_slice() {
        assert_eq!(health_key_prefix("sk-rs-tuanjie-pilot-key"), "sk-rs-tuan...");
        assert_eq!(health_key_prefix("short"), "short...");
    }

    #[test]
    fn quota_param_name_is_user_id_not_uid() {
        // 简报写的是 `?uid=`，Go 现行实现读的是 `user_id`（`server.go:1870`）——
        // 这条单测把「以 Go 为准」固化下来，避免以后有人照简报改回去。
        let q = "uid=uid-a&refresh=1";
        assert_eq!(query_param(q, "user_id"), None);
        assert_eq!(query_param(q, "uid").as_deref(), Some("uid-a"));
        assert_eq!(query_param(q, "refresh").as_deref(), Some("1"));
    }
}
