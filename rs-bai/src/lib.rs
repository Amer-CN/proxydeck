//! rs-bai —— ProxyDeck B.AI 插件的 Rust 试点（绞杀者路线第一步）。
//!
//! 目标：复刻 `internal/bai/*.go` 的对外合同，先在测试端口独立跑通，不注册进 `app/plugins.go`、
//! 不动 Go 构建、不碰任何现行服务端口。
//!
//! 已实现的端点：
//! * `GET  /health`               —— 本地应答，字段与 Go 同形
//! * `GET  /model/matrix`         —— GUI 专用旁路：读渠道配置的 Bearer 拉上游清单并缓存（`src/matrix.rs`）
//! * `GET  /v1/stats`            —— 本地按模型累计 + `bai-stats.json` 落盘（`src/stats.rs`）
//! * 其余全部路径 —— 透传上游（含 `/v1/models`、`/v1/chat/completions`）
//!
//! `/v1/chat/completions` 上叠加 Go 侧 `adaptQuirks` 的请求改写（developer→system、
//! max_tokens 钳到 8192、reasoning_effort=max→high、超 1M token 截断最早轮次）。
//!
//! 请求体上限：Go 侧 `bufferBody` 是 32MB（`internal/bai/server.go:503`），axum 的 `Bytes`
//! 抽取器默认只收 2MB（超了本地 413，比 Go 少收 2MB~32MB 的请求）——故此处显式对齐 32MB，
//! 见 `MAX_BODY_BYTES` 与 `tests/http_contract.rs` 的 `large_body_reaches_upstream`。
//!
//! 数据目录：`/model/matrix` 与 `/v1/stats` 都落在 Go 侧 `internal/appdir` 的同一目录
//! （默认 `%APPDATA%\ProxyDeck\`，`PROXYDECK_DATA_DIR` 可覆盖），见 `src/datadir.rs`。

pub mod adapt;
mod capture;
pub mod crypto;
pub mod datadir;
pub mod matrix;
pub mod stats;
pub mod transport;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use clap::Parser;
use serde_json::json;

use matrix::MatrixState;
use stats::Stats;
pub use transport::UpstreamClient;

/// 与 Go 侧 `internal/bai/server.go:upstreamBase` 同值。
pub const DEFAULT_UPSTREAM: &str = "https://api.b.ai";

/// 请求体上限，与 Go 侧 `bufferBody` 的 `const limit = 32 << 20` 同值
/// （`internal/bai/server.go:503`）。axum 的 `Bytes` 抽取器默认只收 2MB，不显式设这个
/// 就会在 2MB 处本地 413，而 Go 能一路转发到 32MB。
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// 按 Go `encoding/json` 的规则写一个 JSON 字符串字面量（含 HTML 转义，见
/// `encoding/json` 的 `stringEncoder`：`<`/`>`/`&` 与 U+2028/U+2029 一律转义）。
/// `/model/matrix` 与 `/v1/stats` 的响应体都手写拼装以求与 Go 逐字节一致，共用这一处转义。
pub(crate) fn push_go_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

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

    /// 渠道配置路径覆盖（默认数据目录下的 `tuanjie-water-channels.json`）。
    /// 仅供黄金回放/本地对照——真实运行不要带它，否则与 Go 版取不到同一个文件。
    #[arg(long)]
    pub channels_config: Option<PathBuf>,

    /// 统计文件路径覆盖（默认数据目录下的 `bai-stats.json`）。同 `--channels-config`，仅供对照。
    #[arg(long)]
    pub stats_path: Option<PathBuf>,
}

/// 服务状态（本地端点用）。
pub struct AppState {
    pub started_at: Instant,
    pub client: Arc<UpstreamClient>,
    /// 矩阵缓存（对应 Go `Server.matrix`）。
    pub matrix: MatrixState,
    /// 渠道配置路径（对应 Go `channelsConfigPath`）。
    pub channels_config: PathBuf,
    /// 上游清单 URL（对应 Go `upstreamModelsURL`）。
    pub models_url: String,
    /// 消耗统计（对应 Go `Server.stats` + `statsPath`）。
    pub stats: Stats,
}

impl AppState {
    /// 生产构造：数据目录按 `datadir` 解析（与 Go 同一套判定）。
    pub fn new(client: UpstreamClient) -> Self {
        Self::with_paths(
            client,
            datadir::path(datadir::CHANNELS_FILE),
            datadir::path(datadir::STATS_FILE),
        )
    }

    /// 显式指定路径的构造（测试/对照用，不读环境变量）。
    pub fn with_paths(client: UpstreamClient, channels_config: PathBuf, stats_path: PathBuf) -> Self {
        let models_url = format!("{}/v1/models", client.base_url());
        Self {
            started_at: Instant::now(),
            stats: Stats::load(stats_path),
            client: Arc::new(client),
            matrix: MatrixState::new(),
            channels_config,
            models_url,
        }
    }

    /// 覆盖上游清单 URL（假上游对照用）。
    pub fn with_models_url(mut self, url: impl Into<String>) -> Self {
        self.models_url = url.into();
        self
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // 与 Go 的路由表同形：Go 的 `/health` handler **不看方法**（POST 也回 200 health JSON，
        // `internal/bai/server.go:72`），挂成 `get(...)` 会让 POST 变 405，与 Go 分歧。
        .route("/health", any(handle_health))
        // 方法与 Go 的路由表同形：Go 对 /model/matrix、/v1/stats 都注册在 mux 上**不限方法**
        // （handler 内部各自判定：矩阵端点非 GET 回 405，统计端点不看方法）。所以这里两条都挂
        // `any`，把方法判定留给 handler——挂成 `get(...)` 会让 POST /v1/stats 变 405，
        // 而 Go 回的是 200 + 统计，那就同形不了了。
        .route("/model/matrix", any(handle_model_matrix))
        .route("/v1/stats", any(handle_stats))
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
/// 末尾换行与 Go 的 `json.NewEncoder(w).Encode(...)` 对齐（Encoder 会补一个 `\n`），
/// 方法也不限——Go 的 handler 不判方法，POST 同样回这份 JSON。
async fn handle_health(State(st): State<Arc<AppState>>) -> Response {
    let mut body = json!({
        "status": "ok",
        "service": "bai-go",
        "uptimeSec": st.started_at.elapsed().as_secs() as i64,
    });
    // 走 Go 口径（先递归排键再编字节）：`json!` 的键序是**写下**的顺序，
    // 而 Go 那侧是 `map[string]any` → `encoding/json` 按字典序输出
    // （黄金基准里就是 service/status/uptimeSec）。键序随构建配置飘的话，
    // 与 Go 的逐字节对照会莫名其妙地红——P7d-2 实测踩过一次。
    let mut out = crate::adapt::go_json_bytes(&mut body);
    out.push(b'\n');
    (
        [(header::CONTENT_TYPE, "application/json")],
        out,
    )
        .into_response()
}

/// `/model/matrix[?refresh=1]`（对应 Go `handleModelMatrix`，见 `src/matrix.rs`）。
/// 方法是**透传进来的真方法**：Go 的 handler 自己判非 GET 回 405，路由层不看方法
/// （挂成 `get(...)` 会先被 axum 截住，响应体就不是 Go 那句纯文本了）。
async fn handle_model_matrix(
    State(st): State<Arc<AppState>>,
    method: Method,
    uri: Uri,
) -> Response {
    st.matrix
        .handle(
            &method,
            uri.query(),
            &st.channels_config,
            &st.client.matrix_client(),
            &st.models_url,
        )
        .await
}

/// 补 axum 默认 405 的响应体：Go 侧 `http.Error(w,"method not allowed",405)` 回的是
/// 纯文本 `method not allowed\n`（Content-Type `text/plain; charset=utf-8`），
/// axum 默认是空体——不补就对不上。
///
/// `pub` 是给测试用的：黄金/合同测试**直接用生产这一份**，不再各自抄一份
/// （第 5 轮 R5——此前 `tests/p1_golden.rs` 的自造副本多设了 `nosniff`，
/// 却注释成「照抄生产实现」，两边其实不一样）。
pub async fn method_not_allowed_text(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let mut resp = next.run(req).await;
    if resp.status() == StatusCode::METHOD_NOT_ALLOWED {
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        *resp.body_mut() = axum::body::Body::from("method not allowed\n");
    }
    resp
}

/// `GET /v1/stats`：本地消耗统计 + 运行时长（对应 Go `handleStats`，见 `src/stats.rs`）。
/// Go 侧该 handler 不看方法（POST 也照回统计），故这里不做 405 判定。
async fn handle_stats(State(st): State<Arc<AppState>>, method: Method) -> Response {
    st.stats.handle(&method)
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

/// 黄金门禁用的路径解析：**先 `rs-bai/tests/golden/`（入库），缺则回落 `.work/rust-rewrite/`**。
///
/// 为什么要有它：`.work/` 是 gitignore 的临时目录，被清掉时整批黄金门禁会转红；
/// 门禁必需的基准已放入 `tests/golden/` 入库，`.work/` 只作重新采集时的对照入口。
/// 第 4 轮起入库的是**小号基准**（`*.small.json` + `golden-p1-go.json` + `golden-compact-go.json`），
/// 全量的 `fixtures.json` / `golden-adapt-go.json`（65MB 工作树体积）只留在 `.work/`——
/// 调用方按文件分别处理：必需的缺了**失败**，只有超大用例那一档缺了才**显式跳过并计数**
/// （见 `tests/golden_adapt.rs`），绝不静默。
pub fn golden_path(file: &str) -> std::path::PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rs-bai 应有父目录");
    let in_repo = root.join("rs-bai/tests/golden").join(file);
    if in_repo.exists() {
        return in_repo;
    }
    root.join(".work/rust-rewrite").join(file)
}

pub async fn run(args: Args) -> Result<(), String> {
    let client = UpstreamClient::new(&args.upstream).await?;
    let channels_config = args
        .channels_config
        .clone()
        .unwrap_or_else(|| datadir::path(datadir::CHANNELS_FILE));
    let stats_path = args
        .stats_path
        .clone()
        .unwrap_or_else(|| datadir::path(datadir::STATS_FILE));
    println!(
        "bai-plugin: [rust pilot] starting on {}:{} (upstream={})",
        args.host, args.port, args.upstream
    );
    // 只报路径与存在性，**绝不打印内容**（渠道配置里是 Bearer）
    println!(
        "bai-plugin: 渠道配置 {}（存在={}）；统计文件 {}",
        channels_config.display(),
        channels_config.exists(),
        stats_path.display()
    );
    let state = Arc::new(AppState::with_paths(client, channels_config, stats_path));
    let app = router(state).layer(axum::middleware::from_fn(method_not_allowed_text));
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
