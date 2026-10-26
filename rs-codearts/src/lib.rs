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
pub mod signer;
pub mod sse;
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
}

struct ModelsCache {
    at: Instant,
    access_key: String,
    list: Vec<ModelInfo>,
}

pub struct Server {
    up: Upstream,
    accounts_path: PathBuf,
    allow_local: bool,
    started: Instant,
    models: Mutex<Option<ModelsCache>>,
    local_cache: Mutex<Option<import::Imported>>,
}

impl Server {
    pub fn new(up: Upstream, accounts_path: PathBuf, allow_local: bool) -> Arc<Self> {
        Arc::new(Self {
            up,
            accounts_path,
            allow_local,
            started: Instant::now(),
            models: Mutex::new(None),
            local_cache: Mutex::new(None),
        })
    }

    /// 读本机登录态；失败时退回上次成功的结果（桌面端短暂写文件时不至于瞬间掉线）。
    fn local_session(&self) -> Option<import::Imported> {
        if !self.allow_local {
            return None;
        }
        match import::read_local_session() {
            Ok(s) => {
                if let Ok(mut c) = self.local_cache.lock() {
                    *c = Some(s.clone());
                }
                Some(s)
            }
            Err(_) => self.local_cache.lock().ok().and_then(|c| c.clone()),
        }
    }

    fn load_config(&self) -> Result<Config, String> {
        Config::load(&self.accounts_path)
    }

    /// 选账号：本机登录态优先，其次账号文件。
    fn resolve_account(&self) -> Result<(Account, &'static str), String> {
        if let Some(local) = self.local_session() {
            if local.account.has_credentials() {
                return Ok((local.account, "local"));
            }
        }
        let cfg = self.load_config()?;
        cfg.pick()
            .cloned()
            .map(|a| (a, "file"))
            .ok_or_else(|| {
                format!(
                    "没有可用账号：本机未登录码道，且 {} 不存在或为空",
                    self.accounts_path.display()
                )
            })
    }
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
    let local = st.local_session();
    let cfg = st.load_config();
    let file_pick = cfg.as_ref().ok().and_then(|c| c.pick().cloned());
    let (account, source) = match (&local, file_pick) {
        (Some(l), _) => (Some(l.account.clone()), "local"),
        (None, Some(a)) => (Some(a), "file"),
        (None, None) => (None, "none"),
    };
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
        "ready": account.is_some(),
        "source": source,
        "channel": cfg.as_ref().map(|c| c.channel.clone()).unwrap_or_else(|_| config::DEFAULT_CHANNEL.to_string()),
        "models": models,
        "version": env!("CARGO_PKG_VERSION"),
    });
    match account {
        Some(a) => {
            body["account"] = Value::String(a.display_name());
            body["expiresAt"] = a
                .expires_at
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null);
            body["expired"] = Value::Bool(a.is_expired());
        }
        None => {
            body["reason"] = Value::String(
                cfg.as_ref().err().cloned()
                    .or_else(|| local.as_ref().map(|l| l.source.clone()))
                    .unwrap_or_else(|| {
                        format!("未找到登录态，也没有 {}", st.accounts_path.display())
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
    let (account, _) = match st.resolve_account() {
        Ok(v) => v,
        Err(e) => return error_response(503, &e, "no_account"),
    };
    if let Ok(cache) = st.models.lock() {
        if let Some(c) = cache.as_ref() {
            if c.access_key == account.access_key && c.at.elapsed() < MODELS_TTL {
                return models_response(&c.list);
            }
        }
    }
    match st.up.builtin_models(&account).await {
        Ok(list) => {
            if let Ok(mut cache) = st.models.lock() {
                *cache = Some(ModelsCache {
                    at: Instant::now(),
                    access_key: account.access_key.clone(),
                    list: list.clone(),
                });
            }
            models_response(&list)
        }
        Err(e) => up_error_response(&e),
    }
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
    let wants_stream = req
        .get("stream")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let (account, _) = match st.resolve_account() {
        Ok(v) => v,
        Err(e) => return error_response(503, &e, "no_account"),
    };
    let cfg = st.load_config().unwrap_or_default();
    let session_id = upstream::session_id_for(&account);

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

    let resp = match st
        .up
        .chat(&account, &model, payload, cfg.is_benefit(), &session_id)
        .await
    {
        Ok(r) => r,
        Err(e) => return up_error_response(&e),
    };

    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if status >= 400 {
        let body = resp.bytes().await.unwrap_or_default();
        let e = upstream::error_from_body(status, &body);
        return up_error_response(&e);
    }
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
        json_response(StatusCode::OK, &agg.into_response(&model))
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
    let (account, source) = match st.resolve_account() {
        Ok(v) => v,
        Err(e) => return error_response(503, &e, "no_account"),
    };
    match st.up.statistics(&account).await {
        Ok(raw) => {
            let (total, used, remain, percent) = parse_credit(&raw);
            json_response(
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
            )
        }
        Err(e) => up_error_response(&e),
    }
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
        "codearts-plugin: 账号文件 {}（存在={}）；本机登录态自动导入={}",
        accounts_path.display(),
        accounts_path.exists(),
        if args.no_local { "关" } else { "开" }
    );
    let state = Server::new(up, accounts_path, !args.no_local);
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
