//! rs-codebuddy —— ProxyDeck CodeBuddy/WorkBuddy 插件（8787 CN + 8789 INTL）的 Rust 重写，
//! **P5a：合同与请求侧**（绞杀者路线）。
//!
//! 目标：等价替换 `internal/codebuddy` 的**请求/合同面**，先在测试端口独立跑通，
//! 不注册进 `app/plugins.go`、不动 Go 构建、不碰任何现行服务端口（尤其 **8787/8789 上跑着的 Go 版**）。
//!
//! 端点（与 Go `http.ServeMux` 注册的路径同形；**除 chat 外都不看方法**）：
//! * `GET /health`              —— 诊断（service/backend/region/mode/auth_search_dirs/uptimeSec
//!   + credential 块或 status=error）。`hy4_limit` 属 P5b（无窗口时不写该键）。
//! * `GET /v1/models`           —— 候选池 + 每小时在线探测过滤（TTL 1h）。
//! * `GET /model/info`          —— 模型元数据（只填实测值；INTL 空 meta）。
//! * `POST /v1/chat/completions`—— 入站解析 + 出站体重建 + 11133 退避重试 + `writeErr` 形状。
//! * 其余路径 —— Go `ServeMux` 语义的 404（`404 page not found`）。
//! * `/quota`、`/v1/failover`、`/v1/accounts`（P5b）、`/v1/stats`（P5c）**本轮显式 501**
//!   ——比假 404 诚实：路由存在但功能未迁，交接清单见 crate README。
//!
//! 测试缝（生产默认值就是 Go 的常量/环境变量）：
//! * `upstream_base`（默认区域常量 base URL）——黄金回放靠它指向假上游；
//! * `diag_dir`（默认 `appdir.data_dir()`）——11133 现场落盘目录，临时目录驱动；
//! * `cred`（`CredSource`）——凭据缝，P5a 默认 `PendingCredential`（如实报未接入）。

pub mod chat;
pub mod cred;
pub mod datadir;
pub mod diag;
pub mod gjson;
pub mod godecode;
pub mod models;
pub mod region;
pub mod toolrefs;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{extract::DefaultBodyLimit, middleware, Router};
use clap::Parser;
use serde_json::{Map, Value};

use crate::cred::{CredSource, CredSummary, PendingCredential, StubCredential};
use crate::region::Region;

/// 默认端口（与 Go 侧 CN 8787 同值；对照请显式指定测试端口，如 18787）。
pub const DEFAULT_PORT: u16 = 8787;

/// 插件子模式开关名（与 Go 侧 `--plugin-codebuddy` 同名，保持 GUI spawn 形态一致）。
#[derive(Parser, Debug, Clone)]
#[command(name = "rs-codebuddy", about = "ProxyDeck CodeBuddy 插件（Rust，P5a）")]
pub struct Args {
    /// 与 Go 侧 `--plugin-codebuddy` 同名（本阶段不注册进 GUI，仅占位）。
    #[arg(long = "plugin-codebuddy", default_value_t = false)]
    pub plugin_codebuddy: bool,

    /// 监听端口（默认 8787；对照请显式用测试端口，如 18787）。
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// 监听地址（Go 侧同款回环）。
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// 区域：CN（默认）/ INTL。
    #[arg(long, default_value = "CN")]
    pub region: String,

    /// 全角色脱敏。**属 P5c**：本轮未迁，传了就拒绝启动（不静默忽略）。
    #[arg(long, default_value_t = false)]
    pub desensitize: bool,

    /// 上游 base URL 覆盖（默认区域常量）。仅供对照/黄金回放。
    #[arg(long = "upstream-base", default_value = "")]
    pub upstream_base: String,

    /// 凭据缝的固定值（JSON 文件）。仅供活体对照：`{"auth_dirs":[…],"summary":{…},"headers":{…}}`。
    #[arg(long = "cred-stub", default_value = "")]
    pub cred_stub: String,

    /// 11133 现场落盘目录（默认 `appdir.data_dir()`）。仅供对照。
    #[arg(long = "diag-dir", default_value = "")]
    pub diag_dir: String,
}

/// 探测缓存（Go `probeAt` / `probeList`）。
#[derive(Default)]
pub struct ProbeCache {
    pub at: Option<Instant>,
    pub list: Vec<String>,
}

/// 服务状态（Go `Server` 的子集：P5a 只带请求侧需要的那几段）。
pub struct AppState {
    pub region: Region,
    pub cred: Arc<dyn CredSource>,
    pub http: reqwest::Client,
    /// 出站 base URL（Go 是区域常量；此处留覆盖缝给黄金回放）。
    pub base_url: String,
    pub probe_cache: Mutex<ProbeCache>,
    pub started_at: Instant,
    /// 11133 现场落盘目录（Go `appdir.DataDir()`；启动时解析一次，见 README 差异）。
    pub diag_dir: PathBuf,
}

impl AppState {
    /// 显式构造（测试/黄金回放用）。
    pub fn new(region: Region, cred: Arc<dyn CredSource>, base_url: String, diag_dir: PathBuf) -> AppState {
        AppState {
            region,
            cred,
            http: build_client(),
            base_url,
            probe_cache: Mutex::new(ProbeCache::default()),
            started_at: Instant::now(),
            diag_dir,
        }
    }
}

/// 与 Go `&http.Client{Timeout: 0}` 同口径：**不设总超时**（流式长连接会被总超时掐断），
/// 只设连接超时。
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ---------- 路由 ----------

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // 四条路由都挂 `any`：Go 的 handler 自己不判方法（只有 chat 判 POST），
        // 挂 `get()`/`post()` 会在路由层先变 405，与 Go 分歧。
        .route("/health", any(handle_health))
        .route("/v1/models", any(models::handle_models))
        .route("/model/info", any(models::handle_model_info))
        .route("/v1/chat/completions", any(chat::handle_chat))
        // P5b/P5c 的路径：显式 501（不是假 404，也不是静默缺路由）
        .route("/quota", any(deferred_p5b))
        .route("/v1/failover", any(deferred_p5b))
        .route("/v1/accounts", any(deferred_p5b))
        .route("/v1/stats", any(deferred_p5c))
        .fallback(any(handle_fallback))
        // chat 的读体口径是 Go 的 `io.LimitReader(64<<20)`（**截断**，不是拒绝），
        // 所以这里显式关掉 axum 的 2MB 默认上限——否则 >2MB 的请求会在本地被挡，
        // 而 Go 会照常转发（P3/P4 都栽过同一个默认值）。
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

/// 对应 Go `corsWith`：所有响应都带 CORS 头；OPTIONS 直接 204（路由之前短路）。
async fn cors(req: axum::extract::Request, next: middleware::Next) -> Response {
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
    h.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
}

/// Go `ServeMux` 的兜底：`404 page not found` + `nosniff`。
async fn handle_fallback() -> Response {
    chat::http_error(404, "404 page not found")
}

async fn deferred_p5b() -> Response {
    chat::write_err(
        501,
        "（P5a）该端点属 P5b（账号池与凭据）：/quota、/v1/failover、/v1/accounts 尚未迁移",
    )
}

async fn deferred_p5c() -> Response {
    chat::write_err(
        501,
        "（P5a）该端点属 P5c（出站与下游）：/v1/stats 与 usage 统计尚未迁移",
    )
}

/// Go `handleHealth`（不看方法）。
pub async fn handle_health(State(st): State<Arc<AppState>>) -> Response {
    let cfg = st.region.cfg();
    let mut resp: Map<String, Value> = Map::new();
    resp.insert("status".to_string(), gjson::s("ok"));
    resp.insert("service".to_string(), gjson::s("codebuddy-go"));
    resp.insert("backend".to_string(), gjson::s(cfg.base_url));
    resp.insert("region".to_string(), gjson::s(st.region.as_str()));
    resp.insert(
        "mode".to_string(),
        gjson::s("direct-proxy (native function calling)"),
    );
    let dirs: Vec<Value> = st.cred.auth_dirs().into_iter().map(gjson::s).collect();
    resp.insert("auth_search_dirs".to_string(), Value::Array(dirs));
    resp.insert(
        "uptimeSec".to_string(),
        Value::Number((st.started_at.elapsed().as_secs() as i64).into()),
    );
    if let Some(sum) = st.cred.summary() {
        match sum {
            CredSummary::Err(msg) => {
                resp.insert("status".to_string(), gjson::s("error"));
                resp.insert("message".to_string(), gjson::s(msg));
            }
            CredSummary::Ok(pairs) => {
                let mut m = Map::new();
                for (k, v) in pairs {
                    m.insert(k, v);
                }
                resp.insert("credential".to_string(), Value::Object(m));
            }
        }
    }
    models::json_response(Value::Object(resp))
}

// ---------- 入口 ----------

pub async fn run(args: Args) -> Result<(), String> {
    if args.desensitize {
        return Err(
            "--desensitize 属 P5c（全角色脱敏 DesensitizeBody 尚未迁移）：本轮拒绝启动而不是静默忽略"
                .to_string(),
        );
    }
    let region = Region::parse(&args.region);
    let cred: Arc<dyn CredSource> = if args.cred_stub.is_empty() {
        Arc::new(PendingCredential)
    } else {
        let text = std::fs::read_to_string(&args.cred_stub)
            .map_err(|e| format!("读取 --cred-stub {} 失败: {e}", args.cred_stub))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析 --cred-stub {} 失败: {e}", args.cred_stub))?;
        Arc::new(StubCredential::from_json(&v)?)
    };
    let base_url = if args.upstream_base.is_empty() {
        region.cfg().base_url.to_string()
    } else {
        args.upstream_base.clone()
    };
    let diag_dir = if args.diag_dir.is_empty() {
        datadir::data_dir()
    } else {
        PathBuf::from(&args.diag_dir)
    };
    let state = Arc::new(AppState::new(region, cred, base_url, diag_dir));
    let app = router(state.clone());
    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("端口 {} 被占用: {e}", args.port))?;
    println!(
        "[codebuddy] rs-codebuddy P5a（合同与请求侧）监听 {} region={} base={} diag_dir={}",
        addr,
        region.as_str(),
        state.base_url,
        state.diag_dir.display()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
