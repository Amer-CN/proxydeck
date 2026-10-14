//! rs-comate —— ProxyDeck Comate 插件（8786）的 Rust 重写（绞杀者路线 P2）。
//!
//! 目标：等价替换 `internal/comate`（百度文心快码），先在测试端口独立跑通，不注册进
//! `app/plugins.go`、不动 Go 构建、不碰任何现行服务端口（尤其 **8792 上跑着的 zulu**）。
//!
//! 端点（与 Go 的 `http.ServeMux` 三条同形）：
//! * `/health`               —— 本地应答，无登录态回 `{"status":"no_license"}`；**不看方法**
//! * `/v1/models`            —— 本地应答，目录来自 `zulu list-model`（10 分钟缓存）或硬编码回退；**不看方法**
//! * `/v1/chat/completions`  —— 扁平化 query → POST `/api/v1/conversations/init` → 翻回 OpenAI
//!   （流式与非流式两条；见 `translate`）
//! * 其余路径 —— `ServeMux` 语义的 404/307（路径清洗重定向），见 `handle_fallback`
//!
//! 测试缝（生产默认值就是 Go 的常量）：
//! * `AppState::from_env()` 读 `COMATE_CWD` / `COMATE_MODE` / `COMATE_ZULU` 与
//!   `%USERPROFILE%\.comate\cli-login.json`，与 Go 同一套判定；
//! * `upstream_base` / `zulu_health_url` / `zulu_serve_port` 三个字段供黄金回放指向假上游
//!   与假健康端点——**生产构造里它们等于 `internal/comate` 的常量 8792**。

pub mod cwd;
pub mod gjson;
pub mod godecode;
pub mod translate;
pub mod zulu;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use clap::Parser;
use futures_util::StreamExt;
use gjson::{i, obj, s, J};
use serde_json::Value;
use tokio::sync::Mutex;

use translate::{chunk_json, chunk_reasoning_json, complete_json, LineScanner, MAX_TOKEN};

/// 与 Go `internal/comate/upstreamBase` 同值（内部 zulu serve 固定 8792）。
pub const DEFAULT_UPSTREAM: &str = "http://127.0.0.1:8792";

/// 与 Go `internal/comate/zuluPort` 同值。
pub const ZULU_PORT: &str = "8792";

/// 与 Go `modelInfo` 同形的一条模型。
pub const MODELS_CACHE_TTL: Duration = Duration::from_secs(600); // Go: 10*time.Minute

/// 与 Go `handleChat` 的 `io.LimitReader(r.Body, 32<<20)` 同值。
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Go `fallbackModels` 的 id 列表（顺序逐条保持；`/v1/models` 展示去后缀短名）。
pub const FALLBACK_MODELS: [&str; 15] = [
    "deepseek-v4-flash_a8668ea86c3d40e7ad1f8ee45715d447",
    "deepseek-v4-pro_4ba5277b630243be9eeb19fe48f853fa",
    "glm-4.7-fc_0b87452a03394418b5c8d5468deff6c7",
    "glm-5-turbo-fc_6748c832242b4bcb8f715c7c7902d1ad",
    "glm-5.0-fc_c0e11dc07fbb4516a6b687014d9b4fa8",
    "glm-5.1_a231cc317baf4eaaa0b5afdd4de5f1d9",
    "glm-5.2_50c584482e9d47edbbe47938a88b56b1",
    "glm-5.3_37c550fc6d5b4fb2825abc2c1c88a6b3",
    "glm-5.3-flash_805131a9381a4940ba0f2d90cad0e7eb",
    "glm-5v-turbo_ea9e4f222f0a40b281953ebcb6981dda",
    "kimi-k2.5-oneapi_c047f2503bd648269ac7d6fb447d574c",
    "kimi-k2.6-oneapi_123e4567e89b12d3a456426614174000",
    "minimax-m2.5-fc_c99ea14f526548a8bc28717d4e854123",
    "minimax-m3_a8f465fbedb942efadace88370ad7ff1",
    "minimax-m2.7-fc_047f123f661c414bb253c05b605c391e",
];

#[derive(Parser, Debug)]
#[command(name = "rs-comate", version, about = "ProxyDeck Comate 插件（Rust 重写）")]
pub struct Args {
    /// 与 Go 侧 `--plugin-comate` 同名，保持 GUI spawn 形态一致（本阶段不注册进 GUI，仅占位）。
    #[arg(long = "plugin-comate", default_value_t = false)]
    pub plugin_comate: bool,

    /// 监听端口（默认与 Go 侧 8786 同值；对照请显式指定测试端口，如 18786）。
    #[arg(long, default_value = "8786")]
    pub port: u16,

    /// 监听地址（Go 侧同款默认回环）。
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// 上游 base URL。默认与 Go 侧 `upstreamBase` 一致；仅供黄金回放/本地对照。
    #[arg(long, default_value = DEFAULT_UPSTREAM)]
    pub upstream: String,

    /// 登录态文件路径覆盖（默认 `%USERPROFILE%\.comate\cli-login.json`）。仅供对照。
    #[arg(long)]
    pub license_file: Option<PathBuf>,

    /// zulu CLI 路径覆盖（默认 `COMATE_ZULU` → ProgramFiles → D: 候选探测）。仅供对照。
    #[arg(long)]
    pub zulu: Option<PathBuf>,
}

/// 模型目录缓存（对应 Go `Server.modelsCache` / `modelsAt`）。
///
/// `list: Option<Vec<_>>` 里 **`None` 表示 `list-model` 输出是 JSON `null`**：Go 的
/// `json.Unmarshal("null", &list)` 把切片留成 nil，而 `cachedModels` 的门槛是
/// `s.modelsCache != nil` ⇒ nil 永远不算命中缓存，会反复调 CLI。空数组 `[]` 则是
/// **非 nil 空切片**、算命中（黄金基准的 `models-empty-list` 锁了这条：zulu 只被调 1 次）。
#[derive(Default)]
pub struct ModelsCache {
    pub list: Option<Vec<zulu::ModelInfo>>,
    pub at: Option<Instant>,
}

/// 服务状态（本地端点用）。
pub struct AppState {
    pub started_at: Instant,
    /// 登录态文件（Go `licenseFile()`）。
    pub license_path: PathBuf,
    /// 探测到的 zulu CLI（Go 每请求 `findZulu()`，这里启动时解析一次；进程内等价）。
    pub zulu_path: Option<PathBuf>,
    /// `COMATE_CWD`（Go 每请求 `os.Getenv`，同样是「启动时一次」）。
    pub cwd_env: Option<String>,
    /// `COMATE_MODE`（同上）。
    pub mode_env: Option<String>,
    /// 上游 base（Go 常量 `upstreamBase`）。
    pub upstream_base: String,
    /// zulu 探活 URL（Go `zuluHealth()` 里的 `http://127.0.0.1:8792/health`）。
    pub zulu_health_url: String,
    /// 拉起 zulu serve 时传的 `--port`（Go `zuluPort`）。
    pub zulu_serve_port: String,
    /// zulu serve 就绪等待上限（Go 20s）。
    pub zulu_serve_timeout: Duration,
    pub models: Mutex<ModelsCache>,
    /// 自己 spawn 的 zulu serve（Go `Server.zulu`；复用的外部实例不记）。
    pub zulu_proc: Mutex<Option<std::process::Child>>,
}

impl AppState {
    /// 显式构造（测试/黄金回放用，不读环境变量）。
    pub fn new(
        license_path: PathBuf,
        zulu_path: Option<PathBuf>,
        cwd_env: Option<String>,
        mode_env: Option<String>,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            license_path,
            zulu_path,
            cwd_env,
            mode_env,
            upstream_base: DEFAULT_UPSTREAM.to_string(),
            zulu_health_url: format!("http://127.0.0.1:{ZULU_PORT}/health"),
            zulu_serve_port: ZULU_PORT.to_string(),
            zulu_serve_timeout: Duration::from_secs(20),
            models: Mutex::new(ModelsCache::default()),
            zulu_proc: Mutex::new(None),
        }
    }

    /// 生产构造：与 Go 的 `licenseFile()` / `findZulu()` / `os.Getenv` 同一套判定。
    pub fn from_env() -> Self {
        Self::new(
            zulu::license_path(),
            zulu::discover_zulu(std::env::var("COMATE_ZULU").ok().as_deref()),
            std::env::var("COMATE_CWD").ok(),
            std::env::var("COMATE_MODE").ok(),
        )
    }

    /// 覆盖上游 base（假上游对照用）。
    pub fn with_upstream(mut self, base: impl Into<String>) -> Self {
        self.upstream_base = base.into();
        self
    }

    /// 覆盖 zulu 探活端点（假健康端点对照用；生产恒为 8792）。
    pub fn with_zulu_health(mut self, url: impl Into<String>, serve_port: impl Into<String>) -> Self {
        self.zulu_health_url = url.into();
        self.zulu_serve_port = serve_port.into();
        self
    }
}

// ---------- 路由 ----------

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // 三条路由都挂 `any`：Go 的 handler 自己不判方法（只有 handleChat 判），
        // 挂 `get/post` 会在路由层先变 405，与 Go 分歧。
        .route("/health", any(handle_health))
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

// ---------- /health ----------

/// `GET/POST /health`：无登录态 → `{"status":"no_license"}`；否则带 service/uptimeSec。
/// 末尾换行对齐 Go 的 `json.NewEncoder(w).Encode(...)`；方法不限（Go 的 handler 不判方法）。
async fn handle_health(State(st): State<Arc<AppState>>) -> Response {
    let license = zulu::read_license_at(&st.license_path);
    let body = if license.is_empty() {
        obj(vec![("status", s("no_license"))])
    } else {
        obj(vec![
            ("status", s("ok")),
            ("service", s("comate")),
            ("uptimeSec", i(st.started_at.elapsed().as_secs() as i64)),
        ])
    }
    .to_go()
        + "\n";
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

// ---------- /v1/models ----------

/// 去服务端内部后缀：`deepseek-v4-flash_a866...` → `deepseek-v4-flash`（Go `shortModelID`）。
pub fn short_model_id(id: &str) -> String {
    match id.find('_') {
        Some(0) | None => id.to_string(),
        Some(i) => id[..i].to_string(),
    }
}

/// 硬编码目录的去后缀形态（Go `fallbackShortModels`）。
pub fn fallback_short_models() -> Vec<J> {
    FALLBACK_MODELS
        .iter()
        .map(|id| obj(vec![("id", s(short_model_id(id))), ("object", s("model"))]))
        .collect()
}

/// `GET/POST /v1/models`：首条 auto + 实时目录（失败回退硬编码）。方法不限（Go 同）。
async fn handle_models(State(st): State<Arc<AppState>>) -> Response {
    let license = zulu::read_license_at(&st.license_path);
    let mut data: Vec<J> = vec![obj(vec![
        ("id", s("auto")),
        ("display_name", s("Auto-Free")),
        ("object", s("model")),
    ])];
    let use_live = !license.is_empty() && st.zulu_path.is_some();
    if use_live {
        let zulu_path = st.zulu_path.clone().unwrap_or_default();
        match cached_models(&st, &zulu_path, &license).await {
            Ok(list) => {
                for m in list.unwrap_or_default() {
                    let mut entry: Vec<(String, J)> = vec![
                        ("id".to_string(), s(short_model_id(m.id()))),
                        ("object".to_string(), s("model")),
                    ];
                    if !m.display().is_empty() {
                        entry.push(("display_name".to_string(), s(m.display())));
                    }
                    data.push(J::Obj(entry));
                }
            }
            Err(e) => {
                eprintln!("comate-plugin: list-model 失败，回退硬编码目录: {e}");
                data.extend(fallback_short_models());
            }
        }
    } else {
        data.extend(fallback_short_models());
    }
    let body = obj(vec![("data", J::Arr(data)), ("object", s("list"))]).to_go() + "\n";
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// 10 分钟模型目录缓存（Go `cachedModels`）；`None` = `list-model` 输出 JSON `null`。
pub async fn cached_models(
    st: &AppState,
    zulu_path: &Path,
    license: &str,
) -> Result<Option<Vec<zulu::ModelInfo>>, String> {
    let mut cache = st.models.lock().await;
    if let Some(list) = cache.list.as_ref() {
        if let Some(at) = cache.at {
            if at.elapsed() < MODELS_CACHE_TTL {
                return Ok(Some(list.clone()));
            }
        }
    }
    let list = zulu::list_models(zulu_path, license).await?;
    cache.list = list.clone();
    cache.at = Some(Instant::now());
    Ok(list)
}

/// 当前可用完整 modelId 列表（Go `fullModelList`：实时优先，失败回退硬编码）。
async fn full_model_list(st: &AppState, zulu_path: &Path, license: &str) -> Vec<String> {
    if !license.is_empty() {
        if let Ok(list) = cached_models(st, zulu_path, license).await {
            return list
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.id().to_string())
                .collect();
        }
    }
    FALLBACK_MODELS.iter().map(|s| s.to_string()).collect()
}

/// 短名/完整 id 解析（Go `resolveModel`）。
async fn resolve_model(st: &AppState, zulu_path: &Path, license: &str, requested: &str) -> String {
    if requested.is_empty() || requested == "auto" || requested == "auto-free" {
        return requested.to_string();
    }
    let list = full_model_list(st, zulu_path, license).await;
    for id in &list {
        if id == requested {
            return id.clone();
        }
    }
    for id in &list {
        if short_model_id(id).eq_ignore_ascii_case(requested) {
            return id.clone();
        }
    }
    requested.to_string()
}

// ---------- /v1/chat/completions ----------

fn write_error(status: StatusCode, message: &str, err_type: &str) -> Response {
    let body = obj(vec![(
        "error",
        obj(vec![("message", s(message)), ("type", s(err_type))]),
    )])
    .to_go()
        + "\n";
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn handle_chat(State(st): State<Arc<AppState>>, method: Method, req: Request) -> Response {
    let start = Instant::now();
    let path = req.uri().path().to_string();
    if method != Method::POST {
        return write_error(StatusCode::METHOD_NOT_ALLOWED, "仅支持 POST", "invalid_request");
    }
    let license = zulu::read_license_at(&st.license_path);
    if license.is_empty() {
        eprintln!(
            "comate-plugin: {} {} model=- 503 no_license {:?}",
            method,
            path,
            start.elapsed()
        );
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "未找到 Comate 登录态，请先在 Comate IDE 或 zulu CLI 登录",
            "no_license",
        );
    }
    let zulu_path = match st.zulu_path.clone() {
        Some(p) => p,
        None => {
            eprintln!(
                "comate-plugin: {} {} model=- 503 no_zulu {:?}",
                method,
                path,
                start.elapsed()
            );
            return write_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "未找到 zulu CLI，请先安装 Comate（文心快码 AI IDE）",
                "no_license",
            );
        }
    };

    let raw = read_capped_body(req.into_body(), MAX_BODY_BYTES).await;
    let parsed = match godecode::decode_chat_request(&raw) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("comate-plugin: {} {} model=- 400 {:?}", method, path, start.elapsed());
            return write_error(
                StatusCode::BAD_REQUEST,
                &format!("请求体解析失败: {e}"),
                "invalid_request",
            );
        }
    };

    if let Err(e) = ensure_zulu(&st, &zulu_path, &license).await {
        eprintln!(
            "comate-plugin: {} {} model={} 503 {:?}",
            method,
            path,
            parsed.model,
            start.elapsed()
        );
        return write_error(StatusCode::SERVICE_UNAVAILABLE, &e, "upstream_error");
    }

    let upstream_body =
        build_session_body(&st, &zulu_path, &license, &parsed.messages, &parsed.model).await;
    let model_label = if parsed.model.is_empty() {
        "auto".to_string()
    } else {
        parsed.model.clone()
    };

    if parsed.stream {
        handle_chat_stream(&st, upstream_body, model_label, start).await
    } else {
        handle_chat_once(&st, upstream_body, model_label, start).await
    }
}

/// 会话请求体的构造（Go `handleChat` 里 `body := map[string]any{"query":..., "cwd":..., "license":...}`
/// 加 mode / model 那一段）。抽成 pub 是为了让黄金门禁能逐步重放
/// 「同一批 messages → 同一串出站字节」（`golden-comate-go.json` 的 `out`/`wire` 两段）。
pub async fn build_session_body(
    st: &AppState,
    zulu_path: &Path,
    license: &str,
    messages: &[Value],
    model_requested: &str,
) -> String {
    let query = translate::flatten_messages(messages);
    let mut fields: Vec<(String, J)> = vec![
        ("query".to_string(), s(&query)),
        ("cwd".to_string(), s(cwd::dir(&query, st.cwd_env.as_deref()))),
        ("license".to_string(), s(license)),
    ];
    let mode = st
        .mode_env
        .as_deref()
        .map(translate::trim_space)
        .unwrap_or("");
    if !mode.is_empty() {
        fields.push(("mode".to_string(), s(mode)));
    }
    let model = resolve_model(st, zulu_path, license, model_requested).await;
    if !model.is_empty() && model != "auto" && model != "auto-free" {
        fields.push(("model".to_string(), s(&model)));
    }
    J::Obj(fields).to_go()
}

/// 带上限的请求体读取：Go 用 `io.LimitReader(r.Body, 32<<20)`（**截断**，不是 413）。
async fn read_capped_body(body: Body, max: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => break,
        };
        let remain = max.saturating_sub(out.len());
        if remain == 0 {
            break;
        }
        if chunk.len() > remain {
            out.extend_from_slice(&chunk[..remain]);
            break;
        }
        out.extend_from_slice(&chunk);
        if out.len() >= max {
            break;
        }
    }
    out
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Go `fmt.Sprintf("chatcmpl-comate-%d", rand.Int63())`：63 位非负随机数。
/// 不引入 rand 依赖，用时间 + 自增序号走一次 splitmix 混合。
fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut x = nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    format!("chatcmpl-comate-{}", (x >> 1) as i64)
}

/// `postUpstream`：POST `{upstream}/api/v1/conversations/init`，超时 300s。
async fn post_upstream(st: &AppState, body: &str) -> Result<reqwest::Response, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!(
        "{}/api/v1/conversations/init",
        st.upstream_base.trim_end_matches('/')
    );
    client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())
}

/// 上游非 200 时的错误文案（Go 读前 4096 字节并 TrimSpace）。
/// 读取方式也对齐 Go 的 `io.LimitReader(resp.Body, 4096)`：只读到 4096 字节就停，
/// 不把整个响应体读进内存。
async fn upstream_http_error(resp: reqwest::Response) -> String {
    let code = resp.status().as_u16();
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(c) => c,
            Err(_) => break,
        };
        let remain = 4096usize.saturating_sub(buf.len());
        if remain == 0 {
            break;
        }
        if chunk.len() > remain {
            buf.extend_from_slice(&chunk[..remain]);
            break;
        }
        buf.extend_from_slice(&chunk);
        if buf.len() >= 4096 {
            break;
        }
    }
    let msg = translate::trim_space(&String::from_utf8_lossy(&buf)).to_string();
    format!("上游 HTTP {code}: {msg}")
}

async fn handle_chat_once(
    st: &AppState,
    body: String,
    model: String,
    start: Instant,
) -> Response {
    let created = unix_now();
    let id = new_id();
    let resp = match post_upstream(st, &body).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("comate-plugin: POST {} model={} 502 {:?}", st.upstream_base, model, start.elapsed());
            return write_error(StatusCode::BAD_GATEWAY, &format!("上游连接失败: {e}"), "upstream_error");
        }
    };
    if resp.status() != StatusCode::OK {
        let msg = upstream_http_error(resp).await;
        eprintln!("comate-plugin: POST {} model={} 502 {msg} {:?}", st.upstream_base, model, start.elapsed());
        return write_error(StatusCode::BAD_GATEWAY, &msg, "upstream_error");
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    let (content, err_msg) = translate::read_full_answer(&bytes);
    if !err_msg.is_empty() {
        eprintln!("comate-plugin: model={model} 502 {err_msg} {:?}", start.elapsed());
        return write_error(StatusCode::BAD_GATEWAY, &err_msg, "upstream_error");
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        complete_json(&id, &model, &content, created),
    )
        .into_response()
}

/// 流式路径的可变状态（emit/emitThinking 闭包与读循环的合并体）。
struct StreamRun {
    id: String,
    model: String,
    created: i64,
    committed: bool,
    ok_done: bool,
    upstream_err: String,
    out: Vec<u8>,
}

impl StreamRun {
    fn new(model: String) -> Self {
        Self {
            id: new_id(),
            model,
            created: unix_now(),
            committed: false,
            ok_done: false,
            upstream_err: String::new(),
            out: Vec::new(),
        }
    }

    fn push_frame(&mut self, chunk: String) {
        self.out.extend_from_slice(b"data: ");
        self.out.extend_from_slice(chunk.as_bytes());
        self.out.extend_from_slice(b"\n\n");
        self.committed = true;
    }

    /// 单条 SSE 行的处理（`handleChatStream` 读循环的 switch 段）。
    fn feed(&mut self, line: &[u8]) {
        let ev = match translate::event_from_line(line) {
            Some(ev) => ev,
            None => return,
        };
        match ev.kind.as_str() {
            "delta-batch" => {
                for c in &ev.chunks {
                    if c.delta.is_empty() {
                        continue;
                    }
                    match c.kind.as_str() {
                        "text-delta" => {
                            let chunk = chunk_json(&self.id, &self.model, &c.delta, "", self.created);
                            self.push_frame(chunk);
                        }
                        "thinking-delta" => {
                            let chunk =
                                chunk_reasoning_json(&self.id, &self.model, &c.delta, "", self.created);
                            self.push_frame(chunk);
                        }
                        _ => {}
                    }
                }
            }
            "element-add" => {
                if let Some(el) = &ev.element {
                    if el.typ == "TEXT" && !el.content.is_empty() {
                        let chunk = chunk_json(&self.id, &self.model, &el.content, "", self.created);
                        self.push_frame(chunk);
                    }
                }
            }
            "completed" => {
                if ev.status == "ok" {
                    self.ok_done = true;
                } else {
                    self.upstream_err = if ev.content.is_empty() {
                        ev.status.clone()
                    } else {
                        ev.content.clone()
                    };
                }
            }
            _ => {}
        }
    }

    /// 结束块：finish_reason=stop + `data: [DONE]`。
    fn finish_frames(&mut self) {
        let stop = chunk_json(&self.id, &self.model, "", "stop", self.created);
        self.out.extend_from_slice(b"data: ");
        self.out.extend_from_slice(stop.as_bytes());
        self.out.extend_from_slice(b"\n\n");
        self.out.extend_from_slice(b"data: [DONE]\n\n");
    }
}

/// 无 Content-Length 的 SSE 响应（Go 侧因为有 Flush 走的是 chunked，没有长度头）。
fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// 上游连接/HTTP 错误时的 502：Go 在写 SSE 头之后才走到这里，**Cache-Control 与
/// Connection 头会留在响应上**（黄金基准 `stream-error-before-output` 锁住了这两条）。
fn sse_error(status: StatusCode, message: &str) -> Response {
    let mut resp = write_error(status, message, "upstream_error");
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp.headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    resp
}

async fn handle_chat_stream(
    st: &AppState,
    body: String,
    model: String,
    start: Instant,
) -> Response {
    let mut run = StreamRun::new(model.clone());
    let resp = match post_upstream(st, &body).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("comate-plugin: kind=stream model={model} 502 {:?}", start.elapsed());
            return sse_error(
                StatusCode::BAD_GATEWAY,
                &format!("上游连接失败: {e}"),
            );
        }
    };
    if resp.status() != StatusCode::OK {
        let msg = upstream_http_error(resp).await;
        eprintln!("comate-plugin: kind=stream model={model} 502 {msg} {:?}", start.elapsed());
        return sse_error(StatusCode::BAD_GATEWAY, &msg);
    }

    let mut scanner = LineScanner::new(MAX_TOKEN);
    let mut stream = resp.bytes_stream();

    // 阶段一：读到「首个 chunk 落盘」或上游结束为止——在此之前若上游就报错，仍然可以回 502。
    while !run.committed {
        match stream.next().await {
            None => break,
            Some(Err(_)) => break, // 传输错误：交给下面按 !okDone 判 502（与 Go 同）
            Some(Ok(chunk)) => {
                for line in scanner.push(&chunk) {
                    run.feed(&line);
                }
            }
        }
    }

    if run.committed {
        // 阶段二：已提交 200，继续把上游读完并逐帧下发。
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        let first = std::mem::take(&mut run.out);
        let _ = tx.send(Ok(Bytes::from(first))).await;
        tokio::spawn(async move {
            let mut run = run;
            let mut scanner = scanner;
            let mut stream = stream;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(chunk) => {
                        for line in scanner.push(&chunk) {
                            run.feed(&line);
                        }
                        if !run.out.is_empty() {
                            let out = std::mem::take(&mut run.out);
                            if tx.send(Ok(Bytes::from(out))).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            if let Some(tail) = scanner.finish() {
                run.feed(&tail);
            }
            run.finish_frames();
            if !run.out.is_empty() {
                let out = std::mem::take(&mut run.out);
                let _ = tx.send(Ok(Bytes::from(out))).await;
            }
        });
        let body = Body::from_stream(tokio_stream_from(rx));
        return sse_response(body);
    }

    // 上游在首个 chunk 之前结束：把尾部残留行也算上再判。
    if let Some(tail) = scanner.finish() {
        run.feed(&tail);
    }
    if !run.committed {
        if !run.upstream_err.is_empty() {
            eprintln!(
                "comate-plugin: kind=stream model={model} 502 {} {:?}",
                run.upstream_err,
                start.elapsed()
            );
            return sse_error(StatusCode::BAD_GATEWAY, &run.upstream_err);
        }
        if !run.ok_done {
            eprintln!("comate-plugin: kind=stream model={model} 502 SSE中断 {:?}", start.elapsed());
            return sse_error(StatusCode::BAD_GATEWAY, "上游 SSE 中断");
        }
    }
    run.finish_frames();
    let out = std::mem::take(&mut run.out);
    sse_response(Body::from_stream(tokio_stream_from_bytes(out)))
}

type ByteStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

fn tokio_stream_from(
    rx: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> ByteStream {
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

fn tokio_stream_from_bytes(out: Vec<u8>) -> ByteStream {
    Box::pin(futures_util::stream::once(async move {
        Ok::<Bytes, std::io::Error>(Bytes::from(out))
    }))
}

// ---------- zulu 托管 ----------

async fn zulu_health(url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(url).send().await {
        Ok(r) => r.status() == StatusCode::OK,
        Err(_) => false,
    }
}

/// 保证内部 zulu serve 在线（复用或 spawn）。并发安全（Go `ensureZulu`）。
async fn ensure_zulu(st: &AppState, zulu_path: &Path, license: &str) -> Result<(), String> {
    let mut guard = st.zulu_proc.lock().await;
    if zulu_health(&st.zulu_health_url).await {
        return Ok(()); // 已在线：复用自己 spawn 的或外部实例，不 kill
    }
    if let Some(mut child) = guard.take() {
        let _ = child.kill(); // 自己 spawn 的已死：清掉记录再重拉
    }
    match zulu::ensure_zulu_serve(
        zulu_path,
        license,
        &st.zulu_serve_port,
        &st.zulu_health_url,
        st.zulu_serve_timeout,
    )
    .await
    {
        Ok(child) => {
            *guard = child;
            Ok(())
        }
        Err(e) => Err(e),
    }
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
/// 两条形状都按黄金基准（`golden-comate-go.json` 的 routes 段）逐字节复刻。
async fn handle_fallback(method: Method, uri: Uri) -> Response {
    let raw = uri.path();
    let cleaned = clean_path(raw);
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
        return builder.body(body).unwrap_or_else(|_| {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        });
    }
    let body = if method == Method::HEAD {
        // Go 的 HEAD 404 仍带 Content-Length: 19（handler 写了 19 字节、服务器丢掉 body）
        Body::from("404 page not found\n")
    } else {
        Body::from("404 page not found\n")
    };
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("x-content-type-options", "nosniff")
        .body(body)
        .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response())
}

// ---------- 入口 ----------

pub async fn run(args: Args) -> Result<(), String> {
    let mut state = AppState::from_env();
    if let Some(p) = args.license_file.clone() {
        state.license_path = p;
    }
    if let Some(z) = args.zulu.clone() {
        state.zulu_path = Some(z);
    }
    state.upstream_base = args.upstream.clone();

    println!(
        "comate-plugin: [rust rewrite] starting on {}:{} (upstream={})",
        args.host, args.port, args.upstream
    );
    // 只报路径与存在性，**绝不打印登录态内容**
    println!(
        "comate-plugin: 登录态文件 {}（存在={}）；zulu CLI {}",
        state.license_path.display(),
        state.license_path.exists(),
        state
            .zulu_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<未找到>".to_string())
    );

    let state = Arc::new(state);
    let app = router(state);
    let addr: std::net::SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(|e| format!("监听地址非法 {}:{}: {}", args.host, args.port, e))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {} 被占用: {}", args.port, e))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务退出: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_matches_go_examples() {
        // 期望值全部来自真 Go `net/http` 的实测：前四条是黄金基准 routes 段的响应
        // （`/health/` 是 404 ⇒ 没被重定向；`//v1/models` 是 307 Location `/v1/models`），
        // 后三条是 routes 段里 `/a//b/./c/..`、`/a//b/./c/../`、`/health/../v1/models` 三条的 Location。
        assert_eq!(clean_path("/v1/models"), "/v1/models");
        assert_eq!(clean_path("/health/"), "/health/");
        assert_eq!(clean_path("//v1/models"), "/v1/models");
        assert_eq!(clean_path("/a//b/./c/.."), "/a/b");
        assert_eq!(clean_path("/a//b/./c/../"), "/a/b/");
        assert_eq!(clean_path("/health/../v1/models"), "/v1/models");
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path(""), "/");
        assert_eq!(clean_path("/.."), "/");
        assert_eq!(clean_path("/a/../.."), "/");
    }

    #[test]
    fn short_model_id_matches_go() {
        assert_eq!(short_model_id("deepseek-v4-flash_a866"), "deepseek-v4-flash");
        assert_eq!(short_model_id("_leading-underscore"), "_leading-underscore");
        assert_eq!(short_model_id("no-suffix-model"), "no-suffix-model");
        assert_eq!(short_model_id(""), "");
    }

    #[test]
    fn production_defaults_are_the_go_constants() {
        let st = AppState::new(PathBuf::from("x"), None, None, None);
        assert_eq!(st.upstream_base, "http://127.0.0.1:8792");
        assert_eq!(st.zulu_health_url, "http://127.0.0.1:8792/health");
        assert_eq!(st.zulu_serve_port, "8792");
        assert_eq!(st.zulu_serve_timeout, Duration::from_secs(20));
    }
}
