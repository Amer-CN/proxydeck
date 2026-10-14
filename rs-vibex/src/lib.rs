//! rs-vibex —— ProxyDeck VibeX 插件（8790）的 Rust 等价实现（绞杀者路线 P4）。
//!
//! 对外合同与 `internal/vibex` 逐条对齐，清单见 `rs-vibex/README.md` 的对齐表（C1~C81）。
//! **只做测试端口实例**：不注册进 `plugins.go`、不碰 8790。
pub mod cdp;
pub mod client;
pub mod config;
pub mod crypto;
pub mod datadir;
pub mod engine;
pub mod gjson;
pub mod godecode;
pub mod jwt;
pub mod pool;
pub mod prompt;
pub mod stats;
pub mod util;

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex, Semaphore};

use client::VibexError;
use config::Config;
use engine::{Cancel, Engine, TurnRequest};
use gjson::J;
use pool::Pool;
use prompt::{fold_sync, oai_usage, usage_to_j, OutEvent};
use stats::Stats;

/// `/v1/models` 的内存缓存时长（客户端会高频 poll，绝不透传上游）。
pub const MODELS_TTL_MS: i64 = 5 * 60 * 1000;
/// 请求体读取上限（Go `io.LimitReader(r.Body, 32<<20)`）。
pub const MAX_BODY_BYTES: usize = 32 << 20;
/// `autoWaitTimeout`：自动探测第③段「等用户登录」的长轮询预算（照抄团结的 150 秒）。
pub const AUTO_WAIT_MS_DEFAULT: i64 = 150_000;
/// 无效 token 的固定报错文案（在 pool 之外，供 handler 复用）。
pub const INVALID_TOKEN_MSG: &str = "该 token 无效或已过期（GET /api/llm-providers 探活失败）\
                                     ——请在浏览器登录 vibex.runninghub.cn 后重新提取 Rh-Accesstoken";

/// 统一日志前缀（argv/头里的 token 一律不打印——坑 21）。
pub fn logf(msg: &str) {
    println!("vibex-plugin: {msg}");
}

/// 测试缝：CDP 端口列表与「拉起浏览器」钩子（生产用真实现）。
pub struct Seams {
    pub cdp_ports: Vec<String>,
    pub auto_wait_ms: i64,
    pub launch_browser: Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>,
}

impl Default for Seams {
    fn default() -> Self {
        Seams {
            cdp_ports: cdp::CDP_BROWSER_PORTS.iter().map(|s| s.to_string()).collect(),
            auto_wait_ms: AUTO_WAIT_MS_DEFAULT,
            launch_browser: Arc::new(|port, dir| cdp::launch_browser_with_cdp(port, dir)),
        }
    }
}

/// 插件服务壳。
pub struct Server {
    /// 可写配置（`persistToken` 要同步内存 `tokens`）。
    pub cfg: Mutex<Config>,
    /// 只读快照（Engine / prompt 用；启动后 base_url/app_name/超时不再变）。
    pub cfg_snapshot: Arc<Config>,
    pub pool: Arc<Pool>,
    pub client: Arc<client::Client>,
    pub engine: Arc<Engine>,
    pub stats: Stats,
    pub seams: Seams,

    queue: Arc<Semaphore>,
    started_at_ms: i64,
    models: std::sync::Mutex<(Option<Vec<J>>, i64)>,
    /// token 入池落盘串行化（配置文件整份读写）。
    token_mu: Mutex<()>,
}

impl Server {
    /// 建服务（不监听）。
    pub fn new(cfg: Config, seams: Seams) -> Arc<Server> {
        let stats_path = datadir::path(datadir::STATS_FILE);
        Server::new_with(cfg, seams, stats_path)
    }

    /// 建服务（显式指定统计文件路径；单测用临时目录，避免进程级环境变量互相踩）。
    pub fn new_with(cfg: Config, seams: Seams, stats_path: std::path::PathBuf) -> Arc<Server> {
        let cfg_snapshot = Arc::new(cfg.clone());
        let pool = Arc::new(Pool::new(
            config::token_sources(&cfg, "", &std::env::var("RH_ACCESSTOKEN").unwrap_or_default()),
            config::TOKEN_COOLDOWN_MS,
        ));
        let client = Arc::new(client::Client::new(&cfg_snapshot, pool.clone()));
        let engine = Arc::new(Engine::new(cfg_snapshot.clone(), client.clone(), pool.clone()));
        let queue_limit = cfg.queue_limit.max(1) as usize;
        let stats = Stats::new(stats_path, pool::now_ms());
        Arc::new(Server {
            cfg_snapshot,
            cfg: Mutex::new(cfg),
            pool,
            client,
            engine,
            stats,
            seams,
            queue: Arc::new(Semaphore::new(queue_limit)),
            started_at_ms: pool::now_ms(),
            models: std::sync::Mutex::new((None, 0)),
            token_mu: Mutex::new(()),
        })
    }

    /// 测试缝：非阻塞占一个排队槽（`queue-full` 场景要先占满再发请求）。
    pub fn queue_try_acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.queue.clone().try_acquire_owned().ok()
    }

    /// 启动日志（不含 token 本体）。
    pub fn startup_log(&self, host: &str, port: &str) {        let cfg = &self.cfg_snapshot;
        logf(&format!(
            "listening on {host}:{port} (base_url={} tokens={} token_configured={})",
            cfg.base_url(),
            self.pool.len(),
            self.pool.len() > 0
        ));
    }

    /// 由 `--host`/`--port` 起 HTTP 服务（阻塞）。
    pub async fn serve(self: Arc<Self>, host: &str, port: u16) -> Result<(), String> {
        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("端口 {port} 被占用: {e}"))?;
        self.startup_log(host, &port.to_string());
        axum::serve(listener, router(self))
            .await
            .map_err(|e| e.to_string())
    }
}

// ------------------------------ 路由 ------------------------------

/// 组装路由（与 Go `Server.handler()` 同一组：5 条精确路径 + CORS）。
pub fn router(state: Arc<Server>) -> Router {
    Router::new()
        .route("/health", any(handle_health))
        .route("/accounts", any(handle_accounts))
        .route("/v1/models", any(handle_models))
        .route("/v1/stats", any(handle_stats))
        .route("/v1/chat/completions", any(handle_chat))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(cors_layer))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// CORS：GUI 从 localhost:随机端口 fetch 本端口属跨域（Go `corsWith`）。
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
        header::HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        header::HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        header::HeaderValue::from_static("Content-Type, Authorization, X-Vibex-Session"),
    );
    resp
}

/// Go `http.NotFound`：`404 page not found\n` + `text/plain; charset=utf-8` + nosniff。
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

/// `writeJSON`：`Content-Type: application/json; charset=utf-8`。
fn json_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

/// `writeOAIError`。
fn oai_error(status: StatusCode, message: &str, code: &str) -> Response {
    json_response(
        status,
        J::Obj(vec![(
            "error".into(),
            J::Obj(vec![
                ("code".into(), J::Str(code.to_string())),
                ("message".into(), J::Str(message.to_string())),
                ("type".into(), J::Str("vibex_error".to_string())),
            ]),
        )])
        .to_go(),
    )
}

/// `writeVibexErr`。
fn vibex_err_response(e: &VibexError) -> Response {
    let status = if e.status == 0 { 500 } else { e.status };
    oai_error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        &e.message,
        &e.code,
    )
}

/// 读请求体（上限 32 MiB，与 Go 的 `LimitReader` 同量级）。
async fn read_body(req: Request) -> Vec<u8> {
    let bytes: Bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => Bytes::new(),
    };
    bytes.to_vec()
}

// ------------------------------ /health ------------------------------

async fn handle_health(State(s): State<Arc<Server>>, _req: Request) -> Response {
    let cfg = &s.cfg_snapshot;
    let tokens = s.pool.snapshot();
    let mut pairs: Vec<(String, J)> = vec![
        ("app_name".into(), J::Str(cfg.app_name.clone())),
        ("base_url".into(), J::Str(cfg.base_url())),
        (
            "model_default".into(),
            if cfg.model.is_empty() {
                J::Null
            } else {
                J::Str(cfg.model.clone())
            },
        ),
        ("service".into(), J::Str("vibex-go".to_string())),
        ("status".into(), J::Str("ok".to_string())),
        ("token_configured".into(), J::Bool(s.pool.len() > 0)),
        ("tokens".into(), J::Arr(tokens)),
        (
            "uptimeSec".into(),
            J::Int((pool::now_ms() - s.started_at_ms) / 1000),
        ),
        ("version".into(), J::Str(config::PLUGIN_VERSION.to_string())),
    ];
    if let Some(d) = s.pool.max_days_left() {
        pairs.push(("days_left_max".into(), J::Int(d)));
        pairs.push(("days_left".into(), J::Int(d)));
    }
    json_response(StatusCode::OK, J::Obj(pairs).to_go())
}

// ------------------------------ /accounts ------------------------------

async fn handle_accounts(State(s): State<Arc<Server>>, req: Request) -> Response {
    if req.method() == Method::GET {
        let mut pairs: Vec<(String, J)> = vec![
            ("action".into(), J::Str("list".to_string())),
            ("ok".into(), J::Bool(true)),
            ("pool_size".into(), J::Int(s.pool.len() as i64)),
            ("token_configured".into(), J::Bool(s.pool.len() > 0)),
            ("tokens".into(), J::Arr(s.pool.snapshot())),
        ];
        if let Some(d) = s.pool.max_days_left() {
            pairs.push(("days_left".into(), J::Int(d)));
        }
        return json_response(StatusCode::OK, J::Obj(pairs).to_go());
    }
    if req.method() != Method::POST {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            J::Obj(vec![
                ("msg".into(), J::Str("只接受 GET/POST".to_string())),
                ("ok".into(), J::Bool(false)),
            ])
            .to_go(),
        );
    }
    let raw = read_body(req).await;
    let (action, token) = match godecode::decode_accounts_body(&raw) {
        Ok(v) => v,
        Err(()) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                J::Obj(vec![
                    ("msg".into(), J::Str("请求体解析失败".to_string())),
                    ("ok".into(), J::Bool(false)),
                ])
                .to_go(),
            )
        }
    };
    match action.as_str() {
        "auto" => handle_account_auto(s).await,
        "add" | "" => add_account(s, token.trim().to_string()).await,
        _ => json_response(
            StatusCode::OK,
            J::Obj(vec![
                ("msg".into(), J::Str("未知 action".to_string())),
                ("ok".into(), J::Bool(false)),
            ])
            .to_go(),
        ),
    }
}

/// `addAccount`：JWT 解码 → providers 探活 → 入池 → 落盘（0600）→ 热加载。
async fn add_account(s: Arc<Server>, token: String) -> Response {
    let info = jwt::decode_jwt(&token);
    let info = match (&token, info) {
        (t, Some(i)) if !t.is_empty() => i,
        _ => {
            return oai_error(
                StatusCode::BAD_REQUEST,
                "不是合法的 JWT（须是 Rh-Accesstoken 整串，形如 aaa.bbb.ccc）",
                "INVALID_TOKEN",
            )
        }
    };
    if let Err(e) = s.client.validate_token(&token).await {
        return oai_error(
            StatusCode::BAD_REQUEST,
            &format!("{INVALID_TOKEN_MSG}（{}）", e.message),
            "INVALID_TOKEN",
        );
    }
    let st = pool::TokenState {
        value: token.clone(),
        dead: false,
        cooldown_until_ms: 0,
        calls: 0,
        exp_ms: info.exp.map(|e| e * 1000),
        has_exp: info.has_exp(),
        // **逐字对齐 Go 的缺陷**：`server.go:206` 的 `addAccount` 建 state 时**没有设 `Sub`**
        // （`&tokenState{Value: token, Exp: info.Exp, HasExp: info.hasExp()}`），
        // 于是手动粘贴路径的内存池**不按同号收敛**（同号新串会被追加，池里留两条旧串）；
        // 落盘路径 `persistToken` 仍按 sub 收敛，重启加载（`NewPool` → `addWithState`）
        // 又收敛回一条。三条路径只有这一条不收敛，Rust 侧如实复刻、不擅自"修好"
        // —— 见 rs-vibex/README.md「已知 Go 侧缺陷」。
        sub: String::new(),
    };
    let (_, existing) = s.pool.add_with_state(st);
    if existing {
        return json_response(
            StatusCode::OK,
            J::Obj(vec![
                ("existing".into(), J::Bool(true)),
                ("msg".into(), J::Str("该 token 已在池里".to_string())),
                ("ok".into(), J::Bool(false)),
                ("pool_size".into(), J::Int(s.pool.len() as i64)),
                ("user".into(), J::Str(info.username.clone())),
            ])
            .to_go(),
        );
    }
    let mut pairs: Vec<(String, J)> = vec![
        ("existing".into(), J::Bool(false)),
        ("msg".into(), J::Str("账号已添加".to_string())),
        ("ok".into(), J::Bool(true)),
        ("pool_size".into(), J::Int(s.pool.len() as i64)),
        ("user".into(), J::Str(info.username.clone())),
    ];
    // tokenMu：落盘串行化（配置文件是整份读写，并发写会互相盖掉）
    let persisted = {
        let _guard = s.token_mu.lock().await;
        let mut cfg = s.cfg.lock().await;
        cfg.persist_token(&token)
    };
    match persisted {
        Ok(()) => {
            pairs.push(("persisted".into(), J::Bool(true)));
            logf(&format!("账号已入池（手动粘贴，池 {} 个，token 本体不打印）", s.pool.len()));
        }
        Err(e) => {
            // 落盘失败不回滚内存态（本次能用），但如实告知——重启后会丢
            pairs.push(("persisted".into(), J::Bool(false)));
            pairs.push((
                "warning".into(),
                J::Str(format!("token 已热加载，但写入配置文件失败: {e}")),
            ));
            logf(&format!("token 落盘失败（重启后会丢，token 本体不打印）: {e}"));
        }
    }
    let now = pool::now_ms();
    if let Some(exp) = info.exp {
        let exp_ms = exp * 1000;
        pairs.push(("days_left".into(), J::Int(pool::days_left(exp_ms, now) as i64)));
        pairs.push(("exp".into(), J::Str(pool::rfc3339_local(exp_ms))));
        pairs.push(("expired".into(), J::Bool(now > exp_ms)));
    }
    json_response(StatusCode::OK, J::Obj(pairs).to_go())
}

/// `persistNewToken`：把刚入池的 token 落盘（失败只记日志，不回滚内存态）。
async fn persist_new_token(s: &Arc<Server>, token: &str) {
    let _guard = s.token_mu.lock().await;
    let mut cfg = s.cfg.lock().await;
    match cfg.persist_token(token) {
        Ok(()) => logf(&format!("token 已落盘（池 {} 个，token 本体不打印）", s.pool.len())),
        Err(e) => logf(&format!("token 落盘失败（重启后会丢，token 本体不打印）: {e}")),
    }
}

/// `handleAccountAuto` 三段式自动探测。
async fn handle_account_auto(s: Arc<Server>) -> Response {
    // 1. 直接探测（浏览器可能已带调试口在跑）
    if let Some(creds) = cdp::probe_cdp_browser(&s.seams.cdp_ports).await {
        match s.client.validate_token(&creds.token).await {
            Err(e) => logf(&format!(
                "自动探测到 VibeX 登录态但探活失败，继续弹窗流程以便用户重新登录: {e}"
            )),
            Ok(()) => {
                let (_, existing) = s
                    .pool
                    .add_with_state(pool::TokenState::from_value(&creds.token));
                if !existing {
                    persist_new_token(&s, &creds.token).await;
                    logf(&format!(
                        "自动探测成功（浏览器已开调试口，来源={} 池 {} 个）",
                        creds.source,
                        s.pool.len()
                    ));
                    return json_response(
                        StatusCode::OK,
                        J::Obj(vec![
                            ("browser".into(), J::Str(creds.browser.clone())),
                            ("msg".into(), J::Str("已从浏览器读取并添加账号".to_string())),
                            ("ok".into(), J::Bool(true)),
                            ("pool_size".into(), J::Int(s.pool.len() as i64)),
                            ("user_id".into(), J::Str(creds.user_id.clone())),
                        ])
                        .to_go(),
                    );
                }
                logf(&format!(
                    "自动探测到 {} 已在池里，继续弹窗流程以便用户登新号",
                    creds.user_id
                ));
            }
        }
    }
    // 2. 探测不到 → 拉起【一次性独立 profile】浏览器带调试口
    cdp::cleanup_stale_probe_profiles();
    let port = cdp::free_cdp_port();
    let dir = cdp::new_probe_session_dir();
    let path = match (s.seams.launch_browser)(&port, &dir) {
        Ok(p) => p,
        Err(e) => {
            return json_response(
                StatusCode::OK,
                J::Obj(vec![
                    ("msg".into(), J::Str(format!("未探测到调试口，且启动浏览器失败: {e}"))),
                    ("ok".into(), J::Bool(false)),
                ])
                .to_go(),
            )
        }
    };
    // 3. 长轮询等登录（每 2 秒查一次，只盯本会话端口）
    let deadline = pool::now_ms() + s.seams.auto_wait_ms;
    while pool::now_ms() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let creds = match cdp::probe_cdp_browser_quiet(&port).await {
            Some(c) => c,
            None => continue,
        };
        if let Err(e) = s.client.validate_token(&creds.token).await {
            logf(&format!("探测窗口读到的登录态探活失败（等下一次轮询）: {e}"));
            continue;
        }
        let (_, existing) = s
            .pool
            .add_with_state(pool::TokenState::from_value(&creds.token));
        if existing {
            return json_response(
                StatusCode::OK,
                J::Obj(vec![
                    ("msg".into(), J::Str(format!("该账号已在池里（user_id {}）", creds.user_id))),
                    ("ok".into(), J::Bool(false)),
                    ("pool_size".into(), J::Int(s.pool.len() as i64)),
                    ("user_id".into(), J::Str(creds.user_id.clone())),
                ])
                .to_go(),
            );
        }
        persist_new_token(&s, &creds.token).await;
        logf(&format!(
            "自动探测成功：探测窗口可以关了（port={} user_id={}）",
            port, creds.user_id
        ));
        return json_response(
            StatusCode::OK,
            J::Obj(vec![
                ("browser".into(), J::Str(path.clone())),
                ("msg".into(), J::Str("已读取登录态并入池（弹出的浏览器窗口可以关了）".to_string())),
                ("ok".into(), J::Bool(true)),
                ("pool_size".into(), J::Int(s.pool.len() as i64)),
                ("user_id".into(), J::Str(creds.user_id.clone())),
            ])
            .to_go(),
        );
    }
    logf(&format!("自动探测超时：探测窗口可以关了（port={port}，150 秒未读到期）"));
    json_response(
        StatusCode::OK,
        J::Obj(vec![
            (
                "msg".into(),
                J::Str(
                    "等待登录超时（150 秒）——请在弹出的浏览器窗口里登录 VibeX 账号后再点一次探测"
                        .to_string(),
                ),
            ),
            ("ok".into(), J::Bool(false)),
        ])
        .to_go(),
    )
}

// ------------------------------ /v1/models ------------------------------

async fn handle_models(State(s): State<Arc<Server>>, req: Request) -> Response {
    if s.pool.len() == 0 {
        return oai_error(
            StatusCode::UNAUTHORIZED,
            client::TOKEN_HINT,
            "TOKEN_NOT_CONFIGURED",
        );
    }
    let refresh = req
        .uri()
        .query()
        .map(|q| q.split('&').any(|kv| kv == "refresh=1"))
        .unwrap_or(false);
    match models_cached(&s, refresh).await {
        Ok(items) => json_response(
            StatusCode::OK,
            J::Obj(vec![
                ("data".into(), J::Arr(items)),
                ("object".into(), J::Str("list".to_string())),
            ])
            .to_go(),
        ),
        Err(e) => vibex_err_response(&e),
    }
}

/// `modelsCached`：命中缓存直接回；否则现拉一次 provider 列表并刷新缓存（错误不写缓存）。
async fn models_cached(s: &Arc<Server>, refresh: bool) -> Result<Vec<J>, VibexError> {
    {
        let cache = s.models.lock().unwrap();
        if !refresh && cache.0.is_some() && pool::now_ms() - cache.1 < MODELS_TTL_MS {
            return Ok(cache.0.clone().unwrap());
        }
    }
    let providers = s.client.providers().await?;
    let providers = match providers {
        Some(p) if !p.is_empty() => p,
        _ => return Err(VibexError::new("provider 列表为空", 502, "")),
    };
    let mut items: Vec<J> = Vec::with_capacity(providers.len());
    for raw in &providers {
        let p = match raw.as_object() {
            Some(p) => p,
            None => continue,
        };
        let id = engine::provider_id(raw);
        if id.is_empty() {
            continue;
        }
        let display_name = util::first_non_empty(&[
            &util::str_of(p.get("display_name")),
            &util::str_of(p.get("label")),
            &util::str_of(p.get("name")),
        ]);
        items.push(J::Struct(vec![
            ("id".into(), J::Str(id)),
            ("object".into(), J::Str("model".to_string())),
            ("created".into(), J::Int(0)),
            ("owned_by".into(), J::Str("vibex".to_string())),
            (
                "vibex".into(),
                J::Obj(vec![
                    (
                        "billing_tier".into(),
                        J::from_value(p.get("billing_tier").unwrap_or(&Value::Null)),
                    ),
                    (
                        "description".into(),
                        J::from_value(p.get("description").unwrap_or(&Value::Null)),
                    ),
                    ("display_name".into(), J::Str(display_name)),
                ]),
            ),
        ]));
    }
    {
        let mut cache = s.models.lock().unwrap();
        cache.0 = Some(items.clone());
        cache.1 = pool::now_ms();
    }
    Ok(items)
}

// ------------------------------ /v1/stats ------------------------------

async fn handle_stats(State(s): State<Arc<Server>>, _req: Request) -> Response {
    json_response(StatusCode::OK, s.stats.body(pool::now_ms()))
}

// ------------------------------ /v1/chat/completions ------------------------------

async fn handle_chat(State(s): State<Arc<Server>>, req: Request) -> Response {
    let session_header = req
        .headers()
        .get("x-vibex-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let raw = read_body(req).await;
    if String::from_utf8_lossy(&raw).trim().is_empty() {
        return oai_error(StatusCode::BAD_REQUEST, "请求体必须是 JSON 对象", "BAD_REQUEST");
    }
    let body = match godecode::decode_chat_body(&raw) {
        Ok(v) => v,
        Err(()) => {
            return oai_error(StatusCode::BAD_REQUEST, "请求体必须是 JSON 对象", "BAD_REQUEST")
        }
    };
    let obj = match body.as_object() {
        Some(o) => o,
        None => {
            return oai_error(StatusCode::BAD_REQUEST, "请求体必须是 JSON 对象", "BAD_REQUEST")
        }
    };
    let messages = match obj.get("messages").and_then(|m| m.as_array()) {
        Some(m) if !m.is_empty() => m.clone(),
        _ => {
            return oai_error(
                StatusCode::BAD_REQUEST,
                "messages 必须是非空数组",
                "BAD_REQUEST",
            )
        }
    };
    let stream = obj.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    if s.pool.len() == 0 {
        return oai_error(
            StatusCode::UNAUTHORIZED,
            client::TOKEN_HINT,
            "TOKEN_NOT_CONFIGURED",
        );
    }
    let requested = util::str_of(obj.get("model"));
    let provider = match s.engine.resolve_model(&requested).await {
        Ok(p) => p,
        Err(e) => return vibex_err_response(&e),
    };
    let model_name = util::first_non_empty(&[&requested, &engine::provider_id(&provider), "vibex"]);

    // 排队上限：带计数的信号量做非阻塞取槽（无先判后取的 TOCTOU）。
    let permit = match s.queue.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return oai_error(
                StatusCode::TOO_MANY_REQUESTS,
                "当前排队已达上限, 请稍后重试",
                "QUEUE_FULL",
            )
        }
    };

    // 会话亲和（P0-2）：有 session_id 且已知 → 跳过 new_session、只发增量 messages。
    let mut session_id = session_header;
    if session_id.is_empty() {
        if let Some(Value::String(v)) = obj.get("session_id") {
            session_id = v.trim().to_string();
        }
    }
    let (start_idx, fresh) = {
        let mut t = s.engine.sessions().lock().unwrap();
        t.begin(&session_id, messages.len(), pool::now_ms())
    };
    let start = if start_idx > messages.len() { 0 } else { start_idx };
    let prompt = prompt::build_prompt(
        &messages[start..],
        &s.cfg_snapshot.chat_instruction,
    );

    let (tx, rx) = mpsc::channel::<OutEvent>(64);
    let state: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
    let cancel = Cancel::new();
    let turn_req = TurnRequest {
        provider,
        prompt,
        first_connect: s.engine.next_first_connect(),
        fresh_session: fresh && s.cfg_snapshot.fresh_session,
        session_id: session_id.clone(),
        total_msgs: messages.len(),
    };
    let engine = s.engine.clone();
    let state2 = state.clone();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        engine.turn(turn_req, tx, state2, cancel2).await;
    });

    if stream {
        stream_out(s.clone(), rx, state, cancel, model_name, permit).await
    } else {
        let resp = sync_out(rx, state, &model_name, &s).await;
        drop(permit);
        resp
    }
}

/// `syncOut`：聚合文本返回标准 completion。
async fn sync_out(
    mut rx: mpsc::Receiver<OutEvent>,
    state: Arc<std::sync::Mutex<String>>,
    model_name: &str,
    s: &Arc<Server>,
) -> Response {
    let mut events: Vec<OutEvent> = Vec::new();
    while let Some(ev) = rx.recv().await {
        if ev.kind.is_empty() {
            break;
        }
        events.push(ev);
    }
    let (content, usage, flags, mut last_err) = fold_sync(&events);
    let st = state.lock().unwrap().clone();
    if !st.is_empty() && last_err.is_empty() {
        last_err = st;
    }
    if content.is_empty() {
        if let Some(f) = &flags {
            if util::truthy(f.get("balance_insufficient").unwrap_or(&Value::Null)) {
                return oai_error(
                    StatusCode::PAYMENT_REQUIRED,
                    "上游余额不足, 无法完成本轮对话",
                    "BALANCE_INSUFFICIENT",
                );
            }
        }
        if !last_err.is_empty() {
            return oai_error(StatusCode::BAD_GATEWAY, &last_err, "UPSTREAM_ERROR");
        }
        return oai_error(StatusCode::BAD_GATEWAY, "本轮未产生任何内容", "EMPTY_TURN");
    }
    // 统计入账：只认 result 事件带来的真 usage（无 usage 则不入账，不估算）。
    s.stats
        .record_usage(model_name, usage.as_ref().and_then(|u| u.as_object()));
    let created = pool::now_ms() / 1000;
    let id = format!("chatcmpl-{}", config::rand_hex(12));
    json_response(
        StatusCode::OK,
        J::Obj(vec![
            (
                "choices".into(),
                J::Arr(vec![J::Obj(vec![
                    ("finish_reason".into(), J::Str("stop".to_string())),
                    ("index".into(), J::Int(0)),
                    (
                        "message".into(),
                        J::Obj(vec![
                            ("content".into(), J::Str(content)),
                            ("role".into(), J::Str("assistant".to_string())),
                        ]),
                    ),
                ])]),
            ),
            ("created".into(), J::Int(created)),
            ("id".into(), J::Str(id)),
            ("model".into(), J::Str(model_name.to_string())),
            ("object".into(), J::Str("chat.completion".to_string())),
            (
                "usage".into(),
                usage_to_j(oai_usage(usage.as_ref().and_then(|u| u.as_object()))),
            ),
        ])
        .to_go(),
    )
}

/// `streamOut`：SSE 输出（首块 role → 增量 → 尾块 finish stop + usage → [DONE]）。
async fn stream_out(
    s: Arc<Server>,
    mut rx: mpsc::Receiver<OutEvent>,
    state: Arc<std::sync::Mutex<String>>,
    cancel: Cancel,
    model_name: String,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let (tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(32);
    let include_thinking = s.cfg_snapshot.include_thinking;
    let hb_ms = s.cfg_snapshot.heartbeat_ms().max(1);
    tokio::spawn(async move {
        let _permit = permit;
        let cid = format!("chatcmpl-{}", config::rand_hex(12));
        let created = pool::now_ms() / 1000;
        let chunk = |delta: Vec<(String, J)>, finish: J| -> String {
            J::Obj(vec![
                (
                    "choices".into(),
                    J::Arr(vec![J::Obj(vec![
                        ("delta".into(), J::Obj(delta)),
                        ("finish_reason".into(), finish),
                        ("index".into(), J::Int(0)),
                    ])]),
                ),
                ("created".into(), J::Int(created)),
                ("id".into(), J::Str(cid.clone())),
                ("model".into(), J::Str(model_name.clone())),
                ("object".into(), J::Str("chat.completion.chunk".to_string())),
            ])
            .to_go()
        };
        let send = |tx: &mpsc::Sender<Result<Bytes, std::io::Error>>, obj: String| {
            let tx = tx.clone();
            async move {
                let line = format!("data: {obj}\n\n");
                tx.send(Ok(Bytes::from(line))).await.is_ok()
            }
        };
        let abort = |cancel: &Cancel| {
            cancel.cancel();
        };

        if !send(&tx, chunk(vec![("role".into(), J::Str("assistant".to_string()))], J::Null)).await {
            abort(&cancel);
            return;
        }

        let mut produced = false;
        let mut assistant_buf: Vec<String> = Vec::new();
        let mut usage: Option<Value> = None;
        let mut flags: Option<Value> = None;
        let mut last_err = String::new();

        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(hb_ms as u64),
            Duration::from_millis(hb_ms as u64),
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let ev = match ev {
                        Some(ev) => ev,
                        None => break,
                    };
                    if ev.kind.is_empty() { break; }
                    let ok = match ev.kind.as_str() {
                        "text" => {
                            if !ev.text.is_empty() {
                                produced = true;
                                send(&tx, chunk(vec![("content".into(), J::Str(ev.text.clone()))], J::Null)).await
                            } else { true }
                        }
                        "assistant_text" => {
                            if !ev.text.is_empty() { assistant_buf.push(ev.text.clone()); }
                            true
                        }
                        "thinking" => {
                            if include_thinking && !ev.text.is_empty() {
                                send(&tx, chunk(vec![("reasoning_content".into(), J::Str(ev.text.clone()))], J::Null)).await
                            } else { true }
                        }
                        "tool" | "note" | "error_text" => {
                            if !ev.text.is_empty() {
                                produced = true;
                                send(&tx, chunk(vec![("content".into(), J::Str(ev.text.clone()))], J::Null)).await
                            } else { true }
                        }
                        "result" => { usage = ev.usage.clone(); true }
                        "done" => { if ev.flags.is_some() { flags = ev.flags.clone(); } true }
                        "error" => { if last_err.is_empty() { last_err = ev.text.clone(); } true }
                        _ => true,
                    };
                    if !ok { abort(&cancel); return; }
                }
                _ = ticker.tick() => {
                    if tx.send(Ok(Bytes::from_static(b": hb\n\n"))).await.is_err() {
                        abort(&cancel);
                        return;
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }

        let st = state.lock().unwrap().clone();
        if !st.is_empty() && last_err.is_empty() {
            last_err = st;
        }
        if !produced && !assistant_buf.is_empty() {
            produced = true;
            if !send(&tx, chunk(vec![("content".into(), J::Str(assistant_buf.join("")))], J::Null)).await {
                return;
            }
        }
        if !produced && !last_err.is_empty() {
            let _ = send(
                &tx,
                J::Obj(vec![(
                    "error".into(),
                    J::Obj(vec![
                        ("code".into(), J::Str("UPSTREAM_ERROR".to_string())),
                        ("message".into(), J::Str(last_err.clone())),
                        ("type".into(), J::Str("vibex_error".to_string())),
                    ]),
                )])
                .to_go(),
            )
            .await;
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
            return;
        }
        if let Some(f) = &flags {
            if util::truthy(f.get("balance_insufficient").unwrap_or(&Value::Null)) {
                if !send(&tx, chunk(vec![("content".into(), J::Str("\n[上游余额不足]".to_string()))], J::Null)).await {
                    return;
                }
            }
        }
        // 统计入账：走完循环即本轮完成，usage 取 result 事件真值（无 usage 则不入账）。
        s.stats
            .record_usage(&model_name, usage.as_ref().and_then(|u| u.as_object()));
        let final_chunk = J::Obj(vec![
            (
                "choices".into(),
                J::Arr(vec![J::Obj(vec![
                    ("delta".into(), J::Obj(vec![])),
                    ("finish_reason".into(), J::Str("stop".to_string())),
                    ("index".into(), J::Int(0)),
                ])]),
            ),
            ("created".into(), J::Int(created)),
            ("id".into(), J::Str(cid.clone())),
            ("model".into(), J::Str(model_name.clone())),
            ("object".into(), J::Str("chat.completion.chunk".to_string())),
            (
                "usage".into(),
                usage_to_j(oai_usage(usage.as_ref().and_then(|u| u.as_object()))),
            ),
        ])
        .to_go();
        if !send(&tx, final_chunk).await {
            return;
        }
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
    });

    let stream = futures_util::stream::unfold(body_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// 便于测试：把 `HeaderMap` 里我们关心的响应头抽出来。
pub fn pick_response_headers(h: &HeaderMap) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for k in [
        "content-type",
        "cache-control",
        "x-accel-buffering",
        "access-control-allow-origin",
        "access-control-allow-methods",
        "access-control-allow-headers",
    ] {
        out.push((
            k.to_string(),
            h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string(),
        ));
    }
    out
}
