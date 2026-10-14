//! rs-bai —— ProxyDeck B.AI 插件的 Rust 试点（绞杀者路线第一步）。
//!
//! 目标：复刻 `internal/bai/*.go` 的对外合同，先在测试端口独立跑通，不注册进 `app/plugins.go`、
//! 不动 Go 构建、不碰任何现行服务端口。
//!
//! 本轮实现的端点（简报「允许改动的文件」列举范围）：
//! * `GET  /health`               —— 本地应答，字段与 Go 同形
//! * `GET  /model/matrix`         —— 本地 503（Go 侧要 DPAPI 解密渠道密钥，本轮未迁）
//! * `GET  /v1/stats`             —— 本地 503（同上，统计落盘本轮未迁）
//! * 其余全部路径 —— 透传上游（含 `/v1/models`、`/v1/chat/completions`）
//!
//! `/v1/chat/completions` 上叠加 Go 侧 `adaptQuirks` 的请求改写（developer→system、
//! max_tokens 钳到 8192、reasoning_effort=max→high、超 1M token 截断最早轮次）。
//!
//! 请求体上限：Go 侧 `bufferBody` 是 32MB（`internal/bai/server.go:503`），axum 的 `Bytes`
//! 抽取器默认只收 2MB（超了本地 413，比 Go 少收 2MB~32MB 的请求）——故此处显式对齐 32MB，
//! 见 `MAX_BODY_BYTES` 与 `tests/http_contract.rs` 的 `large_body_reaches_upstream`。

pub mod adapt;
mod capture;
mod transport;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use clap::Parser;
use serde_json::json;

pub use transport::UpstreamClient;

/// 与 Go 侧 `internal/bai/server.go:upstreamBase` 同值。
pub const DEFAULT_UPSTREAM: &str = "https://api.b.ai";

/// 请求体上限，与 Go 侧 `bufferBody` 的 `const limit = 32 << 20` 同值
/// （`internal/bai/server.go:503`）。axum 的 `Bytes` 抽取器默认只收 2MB，不显式设这个
/// 就会在 2MB 处本地 413，而 Go 能一路转发到 32MB。
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(name = "rs-bai", version, about = "ProxyDeck B.AI 插件（Rust 试点）")]
pub struct Args {
    /// 与 Go 侧 `--plugin-bai` 同名，保持 GUI spawn 形态一致（本试点不注册进 GUI，仅占位）。
    #[arg(long = "plugin-bai", default_value_t = false)]
    pub plugin_bai: bool,

    /// 监听端口（默认与 Go 侧 8891 同值；试点请显式指定测试端口，如 18788）。
    #[arg(long, default_value = "8891")]
    pub port: u16,

    /// 监听地址（Go 侧同款默认回环）。
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// 上游 base URL。默认与 Go 侧一致；仅用于黄金回放/本地捕获对照。
    #[arg(long, default_value = DEFAULT_UPSTREAM)]
    pub upstream: String,
}

/// 服务状态（本地端点用）。
pub struct AppState {
    pub started_at: Instant,
    pub client: Arc<UpstreamClient>,
}

impl AppState {
    pub fn new(client: UpstreamClient) -> Self {
        Self {
            started_at: Instant::now(),
            client: Arc::new(client),
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handle_health))
        .route("/model/matrix", any(handle_not_migrated))
        .route("/v1/stats", any(handle_not_migrated))
        .route("/v1/chat/completions", any(handle_chat))
        .route("/v1/models", any(handle_proxy))
        .route("/v1/{*rest}", any(handle_proxy))
        .fallback(any(handle_proxy))
        .layer(axum::middleware::from_fn(cors))
        // 与 Go 侧 bufferBody 的 32MB 对齐（axum 默认 2MB 会本地 413）
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// cors 对应 Go 侧 `corsWith`：所有响应都带 CORS 头，OPTIONS 直接 204。
/// GUI 页面跑在 localhost:随机端口，fetch 127.0.0.1:<port> 属跨域，缺这个头前端全拉不到数据。
async fn cors(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
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

/// `GET /health`：与 Go 侧字段同形（status/service/uptimeSec）。
/// 不转发上游——否则没 key 会 401，健康检查误判。
async fn handle_health(State(st): State<Arc<AppState>>) -> Response {
    let body = json!({
        "status": "ok",
        "service": "bai-go",
        "uptimeSec": st.started_at.elapsed().as_secs() as i64,
    });
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response()
}

/// 本轮未迁移的本地端点：Go 侧这两个由本进程应答（不回上游），
/// 这里如实回 503 而不是漏给上游——漏出去会变成对 api.b.ai 的无效请求。
async fn handle_not_migrated(State(_st): State<Arc<AppState>>, uri: Uri) -> Response {
    let body = json!({
        "error": {
            "message": format!("{} 未在本轮 Rust 试点范围内（Go 侧 8891 仍在服务）", uri.path()),
            "type": "not_implemented_in_pilot",
        }
    });
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response()
}

/// `POST /v1/chat/completions`：先改写请求体，再透传（对应 Go 的 adaptQuirks + statsWith）。
async fn handle_chat(
    State(st): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let adapted = adapt::adapt_quirks_body(&body);
    let url = st.client.upstream_url(&uri);
    // 校验记录：此处是"改写后、出门前"的唯一一点，捕获的就是真实出站体
    capture::dump_outbound(&method, &url, &headers, &adapted);
    st.client.forward(method, &uri, headers, adapted.into()).await
}

/// 其余路径一律透传（`/v1/models` 与 catch-all）。
async fn handle_proxy(
    State(st): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let url = st.client.upstream_url(&uri);
    capture::dump_outbound(&method, &url, &headers, &body);
    st.client.forward(method, &uri, headers, body.into()).await
}

pub async fn run(args: Args) -> Result<(), String> {
    let client = UpstreamClient::new(&args.upstream).await?;
    println!(
        "bai-plugin: [rust pilot] starting on {}:{} (upstream={})",
        args.host, args.port, args.upstream
    );
    let state = Arc::new(AppState::new(client));
    let app = router(state);
    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .map_err(|e| format!("监听地址非法 {}:{}: {}", args.host, args.port, e))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("端口 {} 被占用: {}", args.port, e))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务退出: {}", e))
}
