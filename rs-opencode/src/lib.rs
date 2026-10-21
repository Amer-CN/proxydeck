//! rs-opencode —— ProxyDeck OpenCode 插件（8784）的协议翻译层。
//!
//! 把本机真实的 `opencode serve`（headless HTTP API）翻成 OpenAI 兼容接口：
//!
//! ```text
//! GET  /health
//! GET  /v1/models
//! POST /v1/chat/completions      （stream / 非 stream）
//! ```
//!
//! 设计要点（与任务简报逐条对齐）：
//! * **不打包 opencode**：只 spawn/attach 用户机器上的 `opencode serve`，Rust 侧零模型知识。
//! * **每个请求一个真实 opencode 会话**：建会话 → 设模型 → 订阅 `/api/event` → 发 prompt →
//!   把 `session.text.delta` / `session.reasoning.delta` / `session.step.ended` 翻成
//!   `chat.completion[.chunk]`。
//! * **权限询问一律 reject**：没有 approve 分支（见 `upstream::reply_permission`）。
//! * **串行化**：全局 `Semaphore(1)`，opencode 免费档吃不了并发。
//! * **429 明确报错**：撞限流的模型回 429 + retry-after，不会自动换模型（见 `models::ModelPool`）。
//! * **第一版不做工具调用**：入站 `tools` 忽略；`permission.asked` 只拒绝。
pub mod config;
pub mod datadir;
pub mod discover;
pub mod install;
pub mod mcp;
pub mod mcp_child;
pub mod models;
pub mod server;
pub mod translate;
pub mod upstream;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::time::{timeout_at, Instant};

use config::Config;
use models::{ModelEntry, ModelPool, Pick};
use translate::Usage;
use upstream::{ErrorKind, UpError, Upstream};

/// 请求体读取上限（沿用其它 crate 的 32 MiB 口径）。
pub const MAX_BODY_BYTES: usize = 32 << 20;
/// 模型列表内存缓存时长（客户端会高频 poll）。
pub const MODELS_TTL: Duration = Duration::from_secs(60);
/// 流式响应的预热窗口：等第一个"有意义"的事件再决定 HTTP 状态码。
/// 这样上游 429 / 国家限制在发头之前就能如实回 429/400，而不是先 200 再在流里报错。
pub const STREAM_PRELUDE: Duration = Duration::from_secs(15);
/// `/api/info` 健康探活超时。
const INFO_TIMEOUT: Duration = Duration::from_secs(3);
/// `/health` 里的模型数探活超时（复用上游请求的默认 20s 太久，这里只等 2s）。
const HEALTH_MODELS_TIMEOUT: Duration = Duration::from_secs(2);

/// 统一日志前缀（其它 `rs-*` 插件同形）。
pub fn logf(msg: &str) {
    println!("opencode-plugin: {msg}");
}

/// 运行态：就绪（有 opencode base+password）或未就绪（带原因）。
#[derive(Debug, Clone)]
enum Runtime {
    Ready {
        base: String,
        password: String,
        version: String,
    },
    NotReady {
        reason: String,
    },
}

/// 全局状态（axum `State<Arc<Server>>`）。
pub struct Server {
    cfg: Config,
    runtime: RwLock<Runtime>,
    client: reqwest::Client,
    /// 全局最多 1 个上游会话（免费档吃并发）。
    sem: Arc<Semaphore>,
    /// `/api/model` 的内存缓存。
    cache: Mutex<Option<(StdInstant, Vec<ModelEntry>)>>,
    /// 429 冷却表。
    pool: ModelPool,
    /// 自己 spawn 的 `opencode serve`；attach 模式为 None。
    managed: tokio::sync::Mutex<Option<server::ManagedServer>>,
    /// 启动参数 `--attach`（安装完成后重试初始化时沿用同一模式）。
    attach_mode: AtomicBool,
    /// 是否已有安装完成监视任务（避免重复 POST /install/start 叠加 watcher）。
    install_watch: AtomicBool,
    /// 初始化串行化：启动初始化与「安装完成后的重试」不会各 spawn 一个 serve。
    init_lock: tokio::sync::Mutex<()>,
    /// 测试/诊断：MCP 桥子进程用的可执行文件路径覆盖（默认 `current_exe`）。
    bridge_exe: Mutex<Option<PathBuf>>,
}

impl Server {
    /// 生产入口：先起 HTTP 服务，后台再初始化 opencode（避免启动时端口半天不监听）。
    pub fn new(cfg: Config) -> Arc<Self> {
        Self::build(
            cfg,
            Runtime::NotReady {
                reason: "starting".to_string(),
            },
        )
    }

    /// 测试/诊断：直接标记为未就绪（验证 `opencode_not_installed` 的 503 路径）。
    pub fn not_ready(cfg: Config, reason: &str) -> Arc<Self> {
        Self::build(
            cfg,
            Runtime::NotReady {
                reason: reason.to_string(),
            },
        )
    }

    /// 测试/诊断：直接指向一个已经存在的 opencode（或假上游）。
    pub fn with_upstream(cfg: Config, base: &str, password: &str, version: &str) -> Arc<Self> {
        Self::build(
            cfg,
            Runtime::Ready {
                base: base.trim_end_matches('/').to_string(),
                password: password.to_string(),
                version: version.to_string(),
            },
        )
    }

    fn build(cfg: Config, runtime: Runtime) -> Arc<Self> {
        let cooldown = Duration::from_secs(cfg.cooldown_seconds.max(1));
        let client = reqwest::Client::builder()
            // 本机回环绝不走系统代理（WebView2/Clash 环境下 127.0.0.1 被代理劫持是常见坑）。
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            cfg,
            runtime: RwLock::new(runtime),
            client,
            sem: Arc::new(Semaphore::new(1)),
            cache: Mutex::new(None),
            pool: ModelPool::new(cooldown),
            managed: tokio::sync::Mutex::new(None),
            attach_mode: AtomicBool::new(false),
            install_watch: AtomicBool::new(false),
            init_lock: tokio::sync::Mutex::new(()),
            bridge_exe: Mutex::new(None),
        })
    }

    async fn runtime(&self) -> Runtime {
        self.runtime.read().await.clone()
    }

    async fn set_ready(&self, base: &str, password: &str, version: &str) {
        *self.runtime.write().await = Runtime::Ready {
            base: base.trim_end_matches('/').to_string(),
            password: password.to_string(),
            version: version.to_string(),
        };
    }

    async fn set_not_ready(&self, reason: &str) {
        *self.runtime.write().await = Runtime::NotReady {
            reason: reason.to_string(),
        };
    }

    fn cache_get(&self) -> Option<Vec<ModelEntry>> {
        let guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|(at, entries)| {
            if at.elapsed() < MODELS_TTL {
                Some(entries.clone())
            } else {
                None
            }
        })
    }

    fn cache_put(&self, entries: Vec<ModelEntry>) {
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some((StdInstant::now(), entries));
    }

    /// 拉模型列表（过滤后）。`force` 用于 `/v1/models`；刷新失败时尽量沿用旧缓存。
    async fn refresh_models(&self, up: &Upstream, force: bool) -> Result<Vec<ModelEntry>, UpError> {
        if !force {
            if let Some(v) = self.cache_get() {
                return Ok(v);
            }
        }
        match up.models().await {
            Ok(raw) => {
                let entries = models::filter_models(
                    &models::parse_models(&raw),
                    &self.cfg.providers,
                    &self.cfg.model_denylist,
                );
                self.cache_put(entries.clone());
                Ok(entries)
            }
            Err(e) => {
                if let Some(v) = self.cache_get() {
                    logf(&format!("模型列表刷新失败（{}），沿用缓存", e.message));
                    Ok(v)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// 后台初始化：attach（仅 `--attach`）或自己 spawn。
    async fn initialize(self: Arc<Self>, attach: bool) {
        // 启动初始化与「安装完成后重试」可能撞车：串行化，且已就绪就不再 spawn 第二个 serve。
        let _init_guard = self.init_lock.lock().await;
        {
            let rt = self.runtime.read().await;
            if matches!(&*rt, Runtime::Ready { .. }) {
                return;
            }
        }
        if attach {
            if let Some(reg) = discover::read_service_registration() {
                let up = Upstream::new(self.client.clone(), &reg.url, &reg.password);
                match up.info(INFO_TIMEOUT).await {
                    Ok(info) => {
                        let version = info_version(&info)
                            .or(reg.version.clone())
                            .unwrap_or_else(|| "unknown".to_string());
                        self.set_ready(&reg.url, &reg.password, &version).await;
                        logf(&format!(
                            "attach：复用 opencode {version} @ {}（pid {:?}）",
                            reg.url, reg.pid
                        ));
                        return;
                    }
                    Err(e) => logf(&format!(
                        "attach：service.json 指向的 opencode 不可达（{}），改为自己 spawn",
                        e.message
                    )),
                }
            } else {
                logf("attach：未找到 service.json，改为自己 spawn");
            }
        }

        match discover::find_opencode_exe() {
            None => {
                let st = install::status().await;
                if st.downloading {
                    logf("opencode 正在安装中，等待完成后重试初始化");
                    self.set_not_ready("opencode_installing").await;
                    self.clone().spawn_install_watcher();
                } else if self.cfg.auto_install {
                    logf(&format!(
                        "未找到 opencode：autoInstall=true，自动安装 {}",
                        self.cfg.opencode_version
                    ));
                    match install::start(&self.cfg.opencode_version).await {
                        Ok(()) => {
                            self.set_not_ready("opencode_installing").await;
                            self.clone().spawn_install_watcher();
                        }
                        Err(e) => {
                            self.set_not_ready(&format!("opencode_install_failed: {e}"))
                                .await;
                        }
                    }
                } else {
                    self.set_not_ready("opencode_not_installed").await;
                    logf(
                        "未找到 opencode：请安装 opencode、设置 PROXYDECK_OPENCODE_EXE，或 POST /install/start 一键安装",
                    );
                }
            }
            Some(found) => {
                logf(&format!(
                    "使用 opencode：{}（来源 {}）",
                    found.path.display(),
                    found.source
                ));
                match server::spawn(&found.path, server::STARTUP_TIMEOUT).await {
                    Ok(managed) => {
                        let base = managed.base().to_string();
                        let password = managed.password().to_string();
                        let port = managed.port();
                        let up = Upstream::new(self.client.clone(), &base, &password);
                        let version = match up.info(INFO_TIMEOUT).await {
                            Ok(info) => {
                                info_version(&info).unwrap_or_else(|| "unknown".to_string())
                            }
                            Err(_) => "unknown".to_string(),
                        };
                        *self.managed.lock().await = Some(managed);
                        self.set_ready(&base, &password, &version).await;
                        logf(&format!(
                            "opencode serve 就绪：127.0.0.1:{port}（version {version}）"
                        ));
                    }
                    Err(e) => {
                        self.set_not_ready(&format!("opencode_start_failed: {e}"))
                            .await;
                        logf(&format!("opencode serve 启动失败：{e}"));
                    }
                }
            }
        }
    }

    /// 停掉托管的 opencode serve（进程退出/收到 Ctrl+C 时调用）。
    pub async fn shutdown(&self) {
        let mut guard = self.managed.lock().await;
        if let Some(mut managed) = guard.take() {
            managed.stop();
            logf("已停止托管的 opencode serve");
        }
    }

    /// 记录启动参数 `--attach`（安装完成后的重试初始化沿用同一模式）。
    pub fn set_attach_mode(&self, attach: bool) {
        self.attach_mode.store(attach, Ordering::Relaxed);
    }

    /// 测试/诊断：覆盖 MCP 桥子进程的可执行文件路径。
    ///
    /// 生产不用调：默认取 `std::env::current_exe()`（就是本插件 exe）。
    /// 集成测试里当前进程是 test harness，必须指向 `CARGO_BIN_EXE_rs-opencode`。
    pub fn set_bridge_exe(&self, path: PathBuf) {
        *self.bridge_exe.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }

    fn bridge_exe_path(&self) -> Option<PathBuf> {
        if let Some(path) = self
            .bridge_exe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(path);
        }
        std::env::current_exe().ok()
    }

    /// 盯安装任务：完成后自动重新初始化，让 `/health` 变成 `ready:true`。
    ///
    /// 重复 `POST /install/start` 不会叠加 watcher（`install_watch` 去重）；
    /// 每 400ms 读一次状态，直到 `downloading` 变 false。
    fn spawn_install_watcher(self: &Arc<Self>) {
        if self.install_watch.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let st = install::status().await;
                if st.downloading {
                    continue;
                }
                me.install_watch.store(false, Ordering::Release);
                if st.installed {
                    if let Some(path) = st.path.as_deref() {
                        logf(&format!("opencode 安装完成：{path}，重新初始化"));
                    } else {
                        logf("opencode 安装完成，重新初始化");
                    }
                    let attach = me.attach_mode.load(Ordering::Relaxed);
                    me.clone().initialize(attach).await;
                } else if let Some(e) = st.error {
                    logf(&format!("opencode 安装失败：{e}"));
                    me.set_not_ready(&format!("opencode_install_failed: {e}"))
                        .await;
                } else {
                    // 取消（或任务什么都没做）：回到未安装状态，界面可以再点一次安装。
                    me.set_not_ready("opencode_not_installed").await;
                }
                return;
            }
        });
    }
}

// ------------------------------ 路由 ------------------------------

/// 组装路由（与其它插件同一套 CORS / 404 口径）。
pub fn router(state: Arc<Server>) -> Router {
    Router::new()
        .route("/health", any(handle_health))
        .route("/install/status", any(handle_install_status))
        .route("/install/start", any(handle_install_start))
        .route("/install/cancel", any(handle_install_cancel))
        .route("/v1/models", any(handle_models))
        .route("/v1/chat/completions", any(handle_chat))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(cors_layer))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// CORS：GUI 从 localhost:随机端口 fetch 本端口属跨域。
async fn cors_layer(req: Request, next: axum::middleware::Next) -> Response {
    let is_options = req.method() == Method::OPTIONS;
    let mut resp = if is_options {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = resp.headers_mut();
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
    resp
}

/// Go `http.NotFound` 同款文案（其它插件一致）。
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        "404 page not found\n",
    )
        .into_response()
}

fn json_response(status: StatusCode, body: Value) -> Response {
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        text,
    )
        .into_response()
}

/// OpenAI 错误体：`{"error":{"message":...,"type":...}}`（简报给的形状）。
fn oai_error(status: StatusCode, message: &str, ty: &str) -> Response {
    json_response(status, json!({"error": {"message": message, "type": ty}}))
}

/// 上游错误 → HTTP 状态。国家限制按简报回 400，其它非 429 失败回 502。
fn map_up_error(e: &UpError) -> Response {
    let status = match e.kind {
        ErrorKind::RateLimit => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::Country => StatusCode::BAD_REQUEST,
        ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ErrorKind::Http | ErrorKind::Network => StatusCode::BAD_GATEWAY,
    };
    oai_error(status, &e.message, e.error_type())
}

/// 本轮错误（选模之后）：消息里点名是哪个模型出的错，方便多模型时定位。
/// 调用方保证只在已选定模型后用；选模前（建会话/拉列表失败）走 `map_up_error` 原样透出。
fn map_turn_error(model: &str, e: &UpError) -> Response {
    let named = UpError {
        kind: e.kind,
        status: e.status,
        message: format!("模型 {model}：{}", e.message),
    };
    map_up_error(&named)
}

// ------------------------------ /health ------------------------------

async fn handle_health(State(s): State<Arc<Server>>, _req: Request) -> Response {
    match s.runtime().await {
        Runtime::NotReady { reason } => {
            let mut body = json!({"ok": false, "reason": reason});
            if body["reason"] == "opencode_not_installed" {
                // 界面据此显示「一键安装」按钮；装不装由用户决定，插件绝不偷偷下 95 MB。
                body["can_install"] = json!(true);
                body["auto_install"] = json!(s.cfg.auto_install);
            }
            json_response(StatusCode::OK, body)
        }
        Runtime::Ready {
            base,
            password,
            version,
        } => {
            let up = Upstream::new(s.client.clone(), &base, &password);
            match up.info(INFO_TIMEOUT).await {
                Ok(info) => {
                    let opencode = info_version(&info).unwrap_or(version);
                    let models = s
                        .refresh_models_quick(&up)
                        .await
                        .map(|m| m.len())
                        .unwrap_or(0);
                    json_response(
                        StatusCode::OK,
                        json!({
                            "ok": true,
                            "opencode": opencode,
                            "models": models,
                            "ready": true,
                        }),
                    )
                }
                Err(_) => json_response(
                    StatusCode::OK,
                    json!({"ok": false, "reason": "opencode_unreachable"}),
                ),
            }
        }
    }
}

impl Server {
    /// `/health` 用：只等 2 秒的模型数探活；失败不把健康检查拖成 false。
    async fn refresh_models_quick(&self, up: &Upstream) -> Result<Vec<ModelEntry>, UpError> {
        if let Some(v) = self.cache_get() {
            return Ok(v);
        }
        match tokio::time::timeout(HEALTH_MODELS_TIMEOUT, self.refresh_models(up, false)).await {
            Ok(r) => r,
            Err(_) => Err(UpError {
                kind: ErrorKind::Network,
                status: None,
                message: "模型列表探活超时".to_string(),
            }),
        }
    }
}

// ------------------------------ /install/* ------------------------------

/// 安装进度（`installed` 以磁盘为准，完成后自动变 true）。
async fn handle_install_status(State(_s): State<Arc<Server>>) -> Response {
    match serde_json::to_value(install::status().await) {
        Ok(v) => json_response(StatusCode::OK, v),
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": format!("serialize install status: {e}")}),
        ),
    }
}

/// 启动一键安装（后台下载 95 MB，立刻返回 202）。重复调用不重复下载。
async fn handle_install_start(State(s): State<Arc<Server>>) -> Response {
    let version = s.cfg.opencode_version.clone();
    match install::start(&version).await {
        Ok(()) => {
            logf(&format!(
                "安装：已启动（版本 {version}）——下载完成会自动重试初始化"
            ));
            s.spawn_install_watcher();
            json_response(StatusCode::ACCEPTED, json!({"started": true}))
        }
        Err(e) if e == "already running" => {
            json_response(StatusCode::CONFLICT, json!({"started": false, "error": e}))
        }
        Err(e) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"started": false, "error": e}),
        ),
    }
}

/// 取消正在进行的下载；下载循环下一轮退出并清理临时文件。
async fn handle_install_cancel(State(_s): State<Arc<Server>>) -> Response {
    install::cancel().await;
    json_response(StatusCode::ACCEPTED, json!({"cancelled": true}))
}

// ------------------------------ /v1/models ------------------------------

async fn handle_models(State(s): State<Arc<Server>>, _req: Request) -> Response {
    let (base, password) = match s.runtime().await {
        Runtime::Ready { base, password, .. } => (base, password),
        Runtime::NotReady { reason } => {
            return oai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("opencode 未就绪：{reason}"),
                "service_unavailable",
            )
        }
    };
    let up = Upstream::new(s.client.clone(), &base, &password);
    match s.refresh_models(&up, true).await {
        Ok(entries) => json_response(StatusCode::OK, models::openai_models(&entries)),
        Err(e) => map_up_error(&e),
    }
}

// ------------------------------ /v1/chat/completions ------------------------------

async fn handle_chat(State(s): State<Arc<Server>>, req: Request) -> Response {
    let (base, password) = match s.runtime().await {
        Runtime::Ready { base, password, .. } => (base, password),
        Runtime::NotReady { reason } => {
            let msg = format!(
                "opencode is not ready: {reason}. 安装 opencode，或把 PROXYDECK_OPENCODE_EXE 指向 opencode.exe"
            );
            return oai_error(StatusCode::SERVICE_UNAVAILABLE, &msg, "service_unavailable");
        }
    };

    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return oai_error(
                StatusCode::BAD_REQUEST,
                "request body is too large",
                "invalid_request_error",
            )
        }
    };
    let input: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return oai_error(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    let messages = match input.get("messages").and_then(Value::as_array) {
        Some(m) => m,
        None => {
            return oai_error(
                StatusCode::BAD_REQUEST,
                "messages must be an array",
                "invalid_request_error",
            )
        }
    };
    let prompt = translate::flatten_messages(messages);
    if prompt.trim().is_empty() {
        return oai_error(
            StatusCode::BAD_REQUEST,
            "request contains no text content",
            "invalid_request_error",
        );
    }

    let stream = input
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // 第一版只读这两个字段，不转发：opencode 的会话发送接口没有 temperature/max_tokens 入口。
    let _max_tokens = input.get("max_tokens").and_then(Value::as_u64);
    let _temperature = input.get("temperature").and_then(Value::as_f64);
    let requested_raw = input
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let requested = requested_raw.map(models::normalize_key);
    if let Some(r) = requested.as_deref() {
        if models::parse_ref(r).is_none() {
            return oai_error(
                StatusCode::BAD_REQUEST,
                "invalid model id",
                "invalid_request_error",
            );
        }
    }

    let up = Upstream::new(s.client.clone(), &base, &password);
    // 串行化：全局同一时间只允许一个上游会话。排队超过单轮预算就明确回 503。
    // 先拿许可再拉模型/选模型，避免排队期间别的请求已经把某模型打成冷却、自己却仍选它。
    let permit = match tokio::time::timeout(
        Duration::from_secs(s.cfg.timeout_seconds.max(1)),
        s.sem.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => {
            return oai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "opencode request queue is closed",
                "service_unavailable",
            )
        }
        Err(_) => {
            return oai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "opencode is busy with another request; try again later",
                "service_unavailable",
            )
        }
    };
    // 顺手刷新模型缓存（给 /v1/models 用）；失败时：未指定模型就直接报错，
    // 指定了模型则沿用入站 model 让上游如实回错误（不因此改选别的模型）。
    // refresh 的条目同时用来把裸模型名还原成 `<provider>/<model>`（见 models::resolve_ref）。
    let entries: Vec<ModelEntry> = match s.refresh_models(&up, false).await {
        Ok(entries) => entries,
        Err(e) => {
            if requested.is_none() {
                return map_up_error(&e);
            }
            logf(&format!("模型列表拉取失败，沿用入站 model：{}", e.message));
            Vec::new()
        }
    };
    let selected =
        match s
            .pool
            .pick(
                requested.as_deref(),
                &entries,
                &s.cfg.providers,
                &s.cfg.default_model,
            )
        {
            Pick::Selected(m) => m,
            Pick::RequestedCooling { key, retry_after } => {
                let msg = format!(
                    "模型 {key} 正在限流冷却，还剩约 {retry_after} 秒；未自动切换其它模型（显式指定的模型不会被替换）"
                );
                let mut resp = oai_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    &msg,
                    "rate_limit_error",
                );
                if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
                    resp.headers_mut().insert("retry-after", v);
                }
                return resp;
            }
            Pick::Empty => return oai_error(
                StatusCode::BAD_REQUEST,
                "no model specified and no default model configured (set opencode.json defaultModel)",
                "invalid_request_error",
            ),
        };
    let (provider, model_id) = match models::parse_ref(&selected) {
        Some(v) => v,
        None => {
            return oai_error(
                StatusCode::BAD_REQUEST,
                "invalid model id",
                "invalid_request_error",
            )
        }
    };
    logf(&format!(
        "chat：requested={} selected={} stream={}",
        requested_raw.unwrap_or("-"),
        selected,
        stream
    ));

    // ---- 工具桥接（只有带合法 tools 的请求才走；无 tools 时零行为变化）----
    let tool_specs = mcp::parse_tools(input.get("tools"));
    let mut bridge: Option<mcp::Bridge> = None;
    if !tool_specs.is_empty() {
        match s.bridge_exe_path() {
            None => logf("工具桥接：无法定位插件 exe，本轮退回纯文本"),
            Some(exe) => {
                let turn_timeout = Duration::from_secs(s.cfg.timeout_seconds.max(1));
                match mcp::Bridge::create(tool_specs, turn_timeout) {
                    Err(e) => logf(&format!("工具桥接：建临时目录失败（{e}），本轮退回纯文本")),
                    Ok(b) => match b.register(&up.base, &up.password, &exe).await {
                        Ok(()) => {
                            logf(&format!(
                                "工具桥接：已注册 {}（{} 个工具）",
                                b.name(),
                                b.tool_count()
                            ));
                            bridge = Some(b);
                        }
                        Err(e) => logf(&format!(
                            "工具桥接：注册失败（{}），本轮退回纯文本",
                            e.message
                        )),
                    },
                }
            }
        }
    }

    let session_id = match up.create_session().await {
        Ok(id) => id,
        Err(e) => {
            close_bridge(&up, bridge).await;
            return map_up_error(&e);
        }
    };
    if let Err(e) = up.set_model(&session_id, &provider, &model_id).await {
        up.delete_session(&session_id).await;
        close_bridge(&up, bridge).await;
        return map_up_error(&e);
    }
    // 先订阅全局事件流，再发 prompt：避免漏掉 execution.started 之前的帧。
    // 只给"拿到 SSE 响应头"设超时；响应体是长连接，不能套总超时。
    let event_resp = match tokio::time::timeout(upstream::SETUP_TIMEOUT, up.event_stream()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            up.delete_session(&session_id).await;
            close_bridge(&up, bridge).await;
            return map_up_error(&e);
        }
        Err(_) => {
            up.delete_session(&session_id).await;
            close_bridge(&up, bridge).await;
            return oai_error(
                StatusCode::GATEWAY_TIMEOUT,
                "opencode /api/event did not start in time",
                "timeout_error",
            );
        }
    };
    if let Err(e) = up.prompt(&session_id, &prompt).await {
        up.delete_session(&session_id).await;
        close_bridge(&up, bridge).await;
        return map_up_error(&e);
    }

    let deadline = Instant::now() + Duration::from_secs(s.cfg.timeout_seconds.max(1));
    let created = now_secs();
    let completion_id = format!("chatcmpl-{}", random_hex(16));
    let model_echo = requested_raw
        .map(str::to_string)
        .unwrap_or_else(|| selected.clone());

    if stream {
        stream_chat(
            &s,
            &up,
            &session_id,
            event_resp,
            deadline,
            &completion_id,
            &model_echo,
            &selected,
            bridge,
            permit,
        )
        .await
    } else {
        let event_stream: EventStream = Box::pin(event_resp.bytes_stream());
        let outcome = drive_turn(
            &up,
            &session_id,
            event_stream,
            translate::SseDecoder::new(),
            deadline,
            None,
            &completion_id,
            &model_echo,
            Vec::new(),
            bridge.as_ref(),
        )
        .await;
        if !outcome.tool_calls.is_empty() {
            finish_bridge_turn(&up, &session_id, bridge).await;
            drop(permit);
            return json_response(
                StatusCode::OK,
                completion_json(&completion_id, &model_echo, created, &outcome),
            );
        }
        up.delete_session(&session_id).await;
        close_bridge(&up, bridge).await;
        if let Some(e) = &outcome.error {
            if e.kind == ErrorKind::RateLimit {
                s.pool.mark_cooling(&selected);
            }
            return map_turn_error(&selected, e);
        }
        drop(permit);
        json_response(
            StatusCode::OK,
            completion_json(&completion_id, &model_echo, created, &outcome),
        )
    }
}

/// 本轮没有工具调用时的桥清理：只注销 + 交给 Drop 删目录。
async fn close_bridge(up: &Upstream, bridge: Option<mcp::Bridge>) {
    if let Some(b) = bridge {
        b.unregister(&up.base, &up.password).await;
        b.remove_dir();
    }
}

/// 本轮已产出 tool_calls：按简报顺序 interrupt → delete session → delete MCP → 清目录。
async fn finish_bridge_turn(up: &Upstream, session_id: &str, bridge: Option<mcp::Bridge>) {
    if let Err(e) = mcp::interrupt_session(&up.base, &up.password, session_id).await {
        logf(&format!(
            "工具桥接：interrupt 会话失败（{}），继续清理",
            e.message
        ));
    }
    up.delete_session(session_id).await;
    if let Some(b) = bridge {
        b.unregister(&up.base, &up.password).await;
        b.remove_dir();
    }
}

/// 流式：先读最多 15 秒，若第一帧就是 429/国家限制，则在发 HTTP 头之前回真实状态码。
#[allow(clippy::too_many_arguments)]
async fn stream_chat(
    s: &Arc<Server>,
    up: &Upstream,
    session_id: &str,
    event_resp: reqwest::Response,
    deadline: Instant,
    completion_id: &str,
    model: &str,
    selected: &str,
    mut bridge: Option<mcp::Bridge>,
    permit: OwnedSemaphorePermit,
) -> Response {
    let mut stream: EventStream = Box::pin(event_resp.bytes_stream());
    let mut decoder = translate::SseDecoder::new();
    let mut pending: Vec<translate::Event> = Vec::new();
    let mut early_error: Option<UpError> = None;
    let mut step_error: Option<UpError> = None;
    let mut stream_ended = false;

    let prelude_deadline = {
        let cap = Instant::now() + STREAM_PRELUDE;
        if cap < deadline {
            cap
        } else {
            deadline
        }
    };

    loop {
        if Instant::now() >= prelude_deadline {
            break;
        }
        match timeout_at(prelude_deadline, stream.next()).await {
            Err(_) => break,
            Ok(None) => {
                stream_ended = true;
                break;
            }
            Ok(Some(Err(e))) => {
                early_error = Some(UpError::network(e));
                break;
            }
            Ok(Some(Ok(chunk))) => {
                let events = decoder.push(&chunk);
                let mut meaningful = false;
                for ev in events {
                    if !event_for_session(&ev, session_id) {
                        continue;
                    }
                    if ev.ty == "permission.asked" {
                        reply_permission(up, session_id, &ev).await;
                        continue;
                    }
                    if ev.ty == "session.step.failed" && step_error.is_none() {
                        step_error = ev.data.get("error").map(UpError::from_event_error);
                    }
                    if ev.ty == "session.execution.failed" {
                        early_error = Some(
                            ev.data
                                .get("error")
                                .map(UpError::from_event_error)
                                .unwrap_or_else(|| UpError::http(502, "opencode execution failed")),
                        );
                    }
                    if is_meaningful(&ev) || (bridge.is_some() && is_tool_event(&ev)) {
                        meaningful = true;
                    }
                    pending.push(ev);
                }
                if early_error.is_some() || meaningful {
                    break;
                }
            }
        }
    }

    if let Some(e) = early_error {
        up.delete_session(session_id).await;
        close_bridge(up, bridge.take()).await;
        if e.kind == ErrorKind::RateLimit {
            s.pool.mark_cooling(selected);
        }
        return map_turn_error(selected, &e);
    }
    if stream_ended && !pending.iter().any(is_terminal) && !pending.iter().any(is_delta) {
        up.delete_session(session_id).await;
        close_bridge(up, bridge.take()).await;
        if let Some(e) = step_error {
            if e.kind == ErrorKind::RateLimit {
                s.pool.mark_cooling(selected);
            }
            return map_turn_error(selected, &e);
        }
        return oai_error(
            StatusCode::BAD_GATEWAY,
            "opencode event stream ended before the execution completed",
            "upstream_error",
        );
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let role = translate::completion_chunk(
        completion_id,
        model,
        now_secs(),
        json!({"role": "assistant"}),
        None,
    );
    if send_json_frame(&tx, &role).await.is_err() {
        up.delete_session(session_id).await;
        close_bridge(up, bridge.take()).await;
        return empty_sse_response();
    }

    let srv = s.clone();
    let up2 = up.clone();
    let sid = session_id.to_string();
    let id2 = completion_id.to_string();
    let model2 = model.to_string();
    let selected2 = selected.to_string();
    let bridge2 = bridge.take();
    tokio::spawn(async move {
        // permit 移到任务里：客户端读到 [DONE] 或断开之前，都算这一轮还占着上游。
        let _permit = permit;
        let outcome = drive_turn(
            &up2,
            &sid,
            stream,
            decoder,
            deadline,
            Some(&tx),
            &id2,
            &model2,
            pending,
            bridge2.as_ref(),
        )
        .await;
        if !outcome.tool_calls.is_empty() {
            finish_bridge_turn(&up2, &sid, bridge2).await;
            let delta = json!({"tool_calls": translate::tool_call_delta(&outcome.tool_calls)});
            let chunk = translate::completion_chunk(&id2, &model2, now_secs(), delta, None);
            let _ = send_json_frame(&tx, &chunk).await;
            let finish = translate::completion_chunk(
                &id2,
                &model2,
                now_secs(),
                json!({}),
                Some("tool_calls"),
            );
            let _ = send_json_frame(&tx, &finish).await;
        } else {
            up2.delete_session(&sid).await;
            close_bridge(&up2, bridge2).await;
            if let Some(e) = &outcome.error {
                if e.kind == ErrorKind::RateLimit {
                    srv.pool.mark_cooling(&selected2);
                }
                let frame = json!({"error": {"message": format!("模型 {selected2}：{}", e.message), "type": e.error_type()}});
                let _ = send_json_frame(&tx, &frame).await;
            } else {
                let finish =
                    translate::completion_chunk(&id2, &model2, now_secs(), json!({}), Some("stop"));
                let _ = send_json_frame(&tx, &finish).await;
            }
        }
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
    });

    sse_response(rx)
}

// ------------------------------ 事件驱动 ------------------------------

/// boxed 的 reqwest 字节流（预热与正式消费共用同一个流，不能重新开连接）。
type EventStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

#[derive(Default)]
struct DriveOutcome {
    text: String,
    reasoning: String,
    usage: Option<Usage>,
    error: Option<UpError>,
    interrupted: bool,
    /// 非空时本轮返回 `finish_reason:"tool_calls"`，文本/错误都让位。
    tool_calls: Vec<translate::OpenAiToolCall>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventAction {
    Continue,
    Done,
    /// 客户端已断开（流式写不出去）——立刻停手并清理会话。
    ClientGone,
}

/// 消费事件流直到 `execution.succeeded / failed / interrupted`、工具收集窗结束或 deadline。
#[allow(clippy::too_many_arguments)]
async fn drive_turn(
    up: &Upstream,
    session_id: &str,
    mut stream: EventStream,
    mut decoder: translate::SseDecoder,
    deadline: Instant,
    sink: Option<&mpsc::Sender<Result<Bytes, std::io::Error>>>,
    completion_id: &str,
    model: &str,
    mut pending: Vec<translate::Event>,
    bridge: Option<&mcp::Bridge>,
) -> DriveOutcome {
    let mut out = DriveOutcome::default();
    let mut tools = bridge.map(mcp::Bridge::runtime);
    // 第一次识别到外部工具调用后开始 1500ms 收集窗；窗口内继续收并行调用。
    let mut collect_until: Option<Instant> = None;
    'outer: loop {
        for ev in pending.drain(..) {
            if !event_for_session(&ev, session_id) {
                continue;
            }
            if let Some(rt) = tools.as_mut() {
                rt.observe(&ev);
                if collect_until.is_none() && rt.has_ready_calls() {
                    collect_until = Some(Instant::now() + mcp::TOOL_COLLECT_WINDOW);
                }
            }
            match handle_event(up, session_id, &ev, sink, completion_id, model, &mut out).await {
                EventAction::Continue => {}
                EventAction::Done => break 'outer,
                EventAction::ClientGone => {
                    out.error = Some(UpError::client_disconnected());
                    break 'outer;
                }
            }
        }
        if let Some(until) = collect_until {
            if Instant::now() >= until {
                break 'outer;
            }
        }
        let wait_deadline = match collect_until {
            Some(until) if until < deadline => until,
            _ => deadline,
        };
        // 流式时同时盯客户端是否断开：没有下游接收方就立刻停手，
        // 不要空等到 300s deadline 才清理会话。
        let next = match sink {
            Some(tx) => {
                tokio::select! {
                    _ = tx.closed() => {
                        out.error = Some(UpError::client_disconnected());
                        break;
                    }
                    r = timeout_at(wait_deadline, stream.next()) => r,
                }
            }
            None => timeout_at(wait_deadline, stream.next()).await,
        };
        match next {
            Err(_) => {
                // 有可返回的调用时，超时点无论是收集窗还是总 deadline 都优先返回 tool_calls。
                if tools.as_ref().map(|t| t.has_ready_calls()).unwrap_or(false) {
                    break;
                }
                if out.error.is_none() {
                    out.error = Some(UpError::timeout());
                }
                break;
            }
            Ok(None) => {
                if tools.as_ref().map(|t| t.has_ready_calls()).unwrap_or(false) {
                    break;
                }
                if out.error.is_none() {
                    out.error = Some(UpError::stream_ended());
                }
                break;
            }
            Ok(Some(Err(e))) => {
                if tools.as_ref().map(|t| t.has_ready_calls()).unwrap_or(false) {
                    break;
                }
                if out.error.is_none() {
                    out.error = Some(UpError::network(e));
                }
                break;
            }
            Ok(Some(Ok(chunk))) => {
                pending = decoder.push(&chunk);
            }
        }
    }

    if let Some(rt) = tools.as_ref() {
        out.tool_calls = rt
            .ready_calls()
            .iter()
            .map(|c| translate::openai_tool_call(&c.name, &c.arguments))
            .collect();
    }
    if !out.tool_calls.is_empty() {
        // 工具调用优先：权限拒绝 / 打断导致的 execution.failed 不算本轮错误。
        out.error = None;
    }

    // 成功但没收到任何文本（例如事件流只给了 execution.succeeded）时，用最终消息兜底；
    // 错误信息缺失时也从消息接口补。都只是 best-effort，拿不到不改变已有结果。
    if out.error.is_none() && out.text.is_empty() && out.tool_calls.is_empty() {
        if let Ok(messages) = up.final_messages(session_id).await {
            if let Some(text) = final_assistant_text(&messages) {
                out.text = text;
            }
            if let Some(err) = final_error(&messages) {
                out.error = Some(err);
            }
        }
    } else if out.tool_calls.is_empty() {
        if let Some(err) = &out.error {
            if err.message.trim().is_empty() {
                if let Ok(messages) = up.final_messages(session_id).await {
                    if let Some(e) = final_error(&messages) {
                        out.error = Some(e);
                    }
                }
            }
        }
    }
    out
}

async fn handle_event(
    up: &Upstream,
    session_id: &str,
    ev: &translate::Event,
    sink: Option<&mpsc::Sender<Result<Bytes, std::io::Error>>>,
    completion_id: &str,
    model: &str,
    out: &mut DriveOutcome,
) -> EventAction {
    match ev.ty.as_str() {
        "permission.asked" => {
            reply_permission(up, session_id, ev).await;
            EventAction::Continue
        }
        "session.text.delta" => {
            if let Some(delta) = ev.data.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    out.text.push_str(delta);
                    if let Some(tx) = sink {
                        let chunk = translate::completion_chunk(
                            completion_id,
                            model,
                            now_secs(),
                            json!({"content": delta}),
                            None,
                        );
                        if send_json_frame(tx, &chunk).await.is_err() {
                            return EventAction::ClientGone;
                        }
                    }
                }
            }
            EventAction::Continue
        }
        "session.reasoning.delta" => {
            if let Some(delta) = ev.data.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    out.reasoning.push_str(delta);
                    if let Some(tx) = sink {
                        let chunk = translate::completion_chunk(
                            completion_id,
                            model,
                            now_secs(),
                            json!({"reasoning_content": delta}),
                            None,
                        );
                        if send_json_frame(tx, &chunk).await.is_err() {
                            return EventAction::ClientGone;
                        }
                    }
                }
            }
            EventAction::Continue
        }
        "session.step.ended" => {
            if let Some(tokens) = ev.data.get("tokens") {
                if let Some(u) = translate::usage_from(tokens) {
                    out.usage = Some(u);
                }
            }
            EventAction::Continue
        }
        "session.step.failed" => {
            if out.error.is_none() {
                if let Some(err) = ev.data.get("error") {
                    out.error = Some(UpError::from_event_error(err));
                }
            }
            // 不在这里终止：紧随其后的 session.execution.failed 才是本轮终点。
            EventAction::Continue
        }
        "session.execution.failed" => {
            out.error = Some(
                ev.data
                    .get("error")
                    .map(UpError::from_event_error)
                    .unwrap_or_else(|| UpError::http(502, "opencode execution failed")),
            );
            EventAction::Done
        }
        "session.execution.succeeded" => EventAction::Done,
        "session.execution.interrupted" => {
            out.interrupted = true;
            EventAction::Done
        }
        _ => EventAction::Continue,
    }
}

async fn reply_permission(up: &Upstream, session_id: &str, ev: &translate::Event) {
    let Some(permission_id) = ev.data.get("id").and_then(Value::as_str) else {
        return;
    };
    if permission_id.is_empty() {
        return;
    }
    match up.reply_permission(session_id, permission_id).await {
        Ok(()) => logf("permission.asked -> reject（默认拒绝，不允许模型动用户机器）"),
        Err(e) => logf(&format!("回复 permission.asked 失败：{}", e.message)),
    }
}

fn event_for_session(ev: &translate::Event, session_id: &str) -> bool {
    ev.data.get("sessionID").and_then(Value::as_str) == Some(session_id)
}

fn is_meaningful(ev: &translate::Event) -> bool {
    matches!(
        ev.ty.as_str(),
        "session.text.delta"
            | "session.reasoning.delta"
            | "session.step.ended"
            | "session.execution.succeeded"
            | "session.execution.failed"
            | "session.execution.interrupted"
    )
}

/// 带桥接的请求里，工具事件也算“有意义”：否则流式预热会一直等到文本/终点才发头，
/// 工具调用会卡在 15s 窗口里。无桥接时不看这些（保持旧行为）。
fn is_tool_event(ev: &translate::Event) -> bool {
    matches!(
        ev.ty.as_str(),
        "session.tool.input.started"
            | "session.tool.called"
            | "session.tool.progress"
            | "session.tool.success"
            | "session.tool.failed"
    )
}

fn is_terminal(ev: &translate::Event) -> bool {
    matches!(
        ev.ty.as_str(),
        "session.execution.succeeded"
            | "session.execution.failed"
            | "session.execution.interrupted"
    )
}

fn is_delta(ev: &translate::Event) -> bool {
    matches!(
        ev.ty.as_str(),
        "session.text.delta" | "session.reasoning.delta"
    )
}

// ------------------------------ 输出构造 ------------------------------

fn completion_json(id: &str, model: &str, created: i64, out: &DriveOutcome) -> Value {
    let (message, finish_reason) = if out.tool_calls.is_empty() {
        let mut message = json!({"role": "assistant", "content": out.text});
        if !out.reasoning.is_empty() {
            message["reasoning_content"] = Value::String(out.reasoning.clone());
        }
        (message, "stop")
    } else {
        let mut message = json!({"role": "assistant", "content": Value::Null});
        message["tool_calls"] = translate::tool_calls_json(&out.tool_calls);
        (message, "tool_calls")
    };
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": out.usage.unwrap_or_default().to_json(),
    })
}

fn final_assistant_text(messages: &Value) -> Option<String> {
    let arr = messages.get("data").and_then(Value::as_array)?;
    for m in arr {
        if m.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(content) = m.get("content") {
            let text = translate::message_text(content);
            if !text.is_empty() {
                return Some(text);
            }
        }
        if let Some(text) = m.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

fn final_error(messages: &Value) -> Option<UpError> {
    let arr = messages.get("data").and_then(Value::as_array)?;
    for m in arr {
        if let Some(err) = m.get("error") {
            if err.is_object() {
                return Some(UpError::from_event_error(err));
            }
        }
        if m.get("finish").and_then(Value::as_str) == Some("error") {
            return Some(UpError::http(502, "opencode execution failed"));
        }
    }
    None
}

/// 发一帧 `data: <json>\n\n`；接收方（客户端）断开时返回 Err，由调用方停手。
async fn send_json_frame(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    value: &Value,
) -> Result<(), ()> {
    let text = serde_json::to_string(value).map_err(|_| ())?;
    let frame = format!("data: {text}\n\n");
    tx.send(Ok(Bytes::from(frame))).await.map_err(|_| ())
}

fn sse_response(rx: mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Response {
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
    {
        Ok(r) => r,
        Err(_) => oai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build streaming response",
            "server_error",
        ),
    }
}

fn empty_sse_response() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        "",
    )
        .into_response()
}

// ------------------------------ 小工具 ------------------------------

fn info_version(info: &Value) -> Option<String> {
    info.get("version")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if getrandom::fill(&mut buf).is_err() {
        let mut x = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ ((std::process::id() as u64) << 32)
            ^ 0x9e37_79b9_7f4a_7c15;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x & 0xff) as u8;
        }
    }
    use std::fmt::Write as _;
    let mut s = String::with_capacity(n.saturating_mul(2));
    for b in buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

// ------------------------------ 可编程入口 ------------------------------

/// 插件子模式命令行参数（与 `rs-vibex` 的 `Args` 同形，外加 `--attach`）。
#[derive(clap::Parser, Debug, Clone)]
#[command(
    name = "rs-opencode",
    version,
    about = "OpenCode 插件（本机 opencode serve → 本地 OpenAI 兼容接口）"
)]
pub struct Args {
    /// 插件服务模式
    #[arg(long = "plugin-opencode")]
    pub plugin_opencode: bool,
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 8784)]
    pub port: u16,
    /// 附着到 service.json 里已在运行的 opencode serve（默认自己 spawn）
    #[arg(long)]
    pub attach: bool,
    /// MCP stdio 桥接子进程（由 opencode 拉起；值是本轮临时桥目录）
    #[arg(long = "mcp-bridge", value_name = "DIR", hide = true)]
    pub mcp_bridge: Option<PathBuf>,
}

/// 启动插件服务：先绑端口（让 `/health` 立刻可连），后台初始化 opencode 后再服务。
pub async fn run(args: Args) -> Result<(), String> {
    // 桥接子进程模式：必须在加载配置/打印日志之前切走。
    // `logf` 会写 stdout，而 stdout 是 MCP 的 JSON-RPC 通道，绝不能混入日志。
    if let Some(dir) = args.mcp_bridge.clone() {
        let code = tokio::task::spawn_blocking(move || mcp_child::serve(&dir))
            .await
            .unwrap_or(2);
        return if code == 0 {
            Ok(())
        } else {
            Err(format!("mcp-bridge 退出码 {code}"))
        };
    }
    let cfg = config::load(None);
    logf(&format!(
        "配置：providers={:?} modelDenylist={:?} defaultModel={} timeout={}s cooldown={}s autoInstall={} opencodeVersion={}",
        cfg.providers,
        cfg.model_denylist,
        cfg.default_model,
        cfg.timeout_seconds,
        cfg.cooldown_seconds,
        cfg.auto_install,
        cfg.opencode_version
    ));
    let srv = Server::new(cfg);
    srv.set_attach_mode(args.attach);
    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("端口 {} 被占用: {e}", args.port))?;
    logf(&format!("listening on {addr}"));

    let boot = srv.clone();
    let attach = args.attach;
    tokio::spawn(async move {
        boot.initialize(attach).await;
    });

    let serve =
        axum::serve(listener, router(srv.clone())).with_graceful_shutdown(shutdown_signal());
    if let Err(e) = serve.await {
        srv.shutdown().await;
        return Err(e.to_string());
    }
    srv.shutdown().await;
    Ok(())
}
