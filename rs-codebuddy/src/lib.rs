//! rs-codebuddy —— ProxyDeck CodeBuddy/WorkBuddy 插件（8787 CN + 8789 INTL）的 Rust 重写，
//! 绞杀者路线。**P5a：合同与请求侧**；**P5b：账号池 / 凭据 / 额度 / failover**；
//! **P5c：脱敏 / SSE 清理 / 非流式聚合 / 统计 / 路径清洗 / 499 取消源**。
//!
//! 目标：等价替换 `internal/codebuddy`，先在测试端口独立跑通，不注册进 `app/plugins.go`、
//! 不动 Go 构建、不碰任何现行服务端口（尤其 **8787/8789 上跑着的 Go 版**）。
//!
//! 端点（与 Go `http.ServeMux` 注册的路径同形；**除 chat 外都不看方法**）：
//! * `GET /health`              —— 诊断（service/backend/region/mode/auth_search_dirs/uptimeSec
//!   + `credential` 块或 status=error + 有窗口时的 `hy4_limit`）
//! * `GET /v1/models`           —— 候选池 + 每小时在线探测过滤（TTL 1h）
//! * `GET /model/info`          —— 模型元数据（只填实测值；INTL 空 meta）
//! * `GET /quota`               —— 官方积分快照（`?uid=` 指定账号、`?refresh=1` 旁路缓存）
//! * `GET|POST /v1/failover`    —— hy4 兜底开关与 fallback 模型读写
//! * `GET|POST /v1/accounts`    —— 账号池管理（rescan / clear_cool / clear_exhausted /
//!   toggle / remove / restore）
//! * `GET|POST /v1/stats`       —— 按模型消耗统计 + 运行时长（**P5c 起为真实应答**）
//! * `POST /v1/chat/completions`—— 入站解析 + 出站体重建 + 池换号 + 11133 退避重试 +
//!   脱敏 + 流式清洗/usage 入账 + 非流式聚合
//! * 路径清洗 —— `//v1/models` 这类非规范路径按 Go `ServeMux` 回 **307**（见 `pathclean`）
//! * 其余路径 —— Go `ServeMux` 语义的 404（`404 page not found`）
//!
//! 测试缝（生产默认值就是 Go 的常量/环境变量）：
//! * `upstream_base`（默认区域常量 base URL）——黄金回放靠它指向假上游；
//! * `diag_dir`（默认 `appdir.data_dir()`）——11133 现场落盘目录与 stats 落盘目录，临时目录驱动；
//! * `cred_dir`（默认 `$CODEBUDDY_AUTH_DIR` 或本机候选目录）——凭据层用；
//! * `cred`（`CredSource`）——测试可喂 `StubCredential`（`--cred-stub`）。

pub mod atrest;
pub mod chat;
pub mod collect;
pub mod conn;
pub mod cred;
pub mod credential;
pub mod datadir;
pub mod desensitize;
pub mod diag;
pub mod failover;
pub mod gjson;
pub mod godecode;
pub mod gotime;
pub mod models;
pub mod pathclean;
pub mod pool;
pub mod quota;
pub mod region;
pub mod sse;
pub mod stats;
pub mod toolrefs;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{extract::DefaultBodyLimit, middleware, Router};
use clap::Parser;
use serde_json::{Map, Value};

use crate::cred::{CredSource, CredSummary, StubCredential};
use crate::credential::RealCredential;
use crate::failover::Failover;
use crate::pool::AccountPool;
use crate::quota::QuotaCache;
use crate::region::Region;
use crate::stats::Stats;

/// 默认端口（与 Go 侧 CN 8787 同值；对照请显式指定测试端口，如 18787）。
pub const DEFAULT_PORT: u16 = 8787;

/// 插件子模式开关名（与 Go 侧 `--plugin-codebuddy` 同名，保持 GUI spawn 形态一致）。
#[derive(Parser, Debug, Clone)]
#[command(name = "rs-codebuddy", about = "ProxyDeck CodeBuddy 插件（Rust）")]
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

    /// 全角色脱敏（P5c 起真实落地；生产 `app/plugins.go` 对 CN/INTL 都传 `--desensitize`）。
    #[arg(long, default_value_t = false)]
    pub desensitize: bool,

    /// 上游 base URL 覆盖（默认区域常量）。仅供对照/黄金回放。
    #[arg(long = "upstream-base", default_value = "")]
    pub upstream_base: String,

    /// 凭据缝的固定值（JSON 文件）。仅供活体对照：`{"auth_dirs":[…],"summary":{…},"headers":{…}}`。
    #[arg(long = "cred-stub", default_value = "")]
    pub cred_stub: String,

    /// auth 目录覆盖（默认 `CODEBUDDY_AUTH_DIR` 或本机候选目录）。仅供对照。
    #[arg(long = "auth-dir", default_value = "")]
    pub auth_dir: String,

    /// 11133 现场落盘目录（默认 `appdir.data_dir()`）。仅供对照。
    #[arg(long = "diag-dir", default_value = "")]
    pub diag_dir: String,

    /// 账号池状态文件覆盖（默认 `appdir.data_dir()/codebuddy-pool(-intl).json`）。
    #[arg(long = "pool-dir", default_value = "")]
    pub pool_dir: String,
}

/// 探测缓存（Go `probeAt` / `probeList`）。
#[derive(Default)]
pub struct ProbeCache {
    pub at: Option<Instant>,
    pub list: Vec<String>,
}

/// 进程级环境变量锁（**lib 单测专用**）。
///
/// libtest 默认**多线程并行**跑同一个二进制里的用例，而 `WORKBUDDY_AT_REST_SECRET` /
/// `PROXYDECK_DATA_DIR` / `CODEBUDDY_POOL_DIR` 都是**进程级**变量：一个用例 set/remove
/// 会影响并行跑着的另一个用例（README 差异 14 那一族；集成测试已用 `common::EnvGuard`
/// 加锁，lib 单测此前没加，曾造成一次未能复现的单点红）。
///
/// 纪律：**凡是要改这些变量的 lib 单测一律先取这把锁**——只锁一部分等于没锁。
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub fn lock() -> MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

/// 服务状态（对应 Go `Server`；P5b 补齐池 / 额度 / failover / 凭据）。
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
    /// 账号池（Go `s.pool`）；`None` = 零值 Server 语义（换号分支一律不走）。
    pub pool: Option<AccountPool>,
    /// 积分缓存（按区 + 号分键）。
    pub quota: QuotaCache,
    /// hy4 兜底配置 + 限流窗口。
    pub failover: Failover,
    /// 全角色脱敏开关（Go `Server.desensitize`）。
    pub desensitize: bool,
    /// 按模型消耗统计（Go `Server.stats` + `codebuddy-stats(-intl).json`）。
    pub stats: Arc<Stats>,
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
            pool: None,
            quota: QuotaCache::new(),
            failover: Failover::load(""),
            desensitize: false,
            stats: Arc::new(Stats::new(PathBuf::new())),
        }
    }

    /// P5b：按区域装配生产状态（真凭据 + 真池 + 真 failover 配置），对应 Go
    /// `NewServerForRegion`。测试用 `cred_override` / `pool_dir_override` 等参数驱动临时目录。
    ///
    /// 返回 `Err` 时给的是 Go 的同款文案（`missing` 的两种形态）——不许跨区域回退。
    pub fn production(
        region: Region,
        desensitize: bool,
        diag_dir: Option<PathBuf>,
        pool_dir_override: Option<String>,
        upstream_base_override: Option<String>,
    ) -> Result<(AppState, Arc<dyn CredSource>), String> {
        let dir = diag_dir.unwrap_or_else(datadir::data_dir);
        Self::production_with_data_dir(
            region,
            dir,
            pool_dir_override.unwrap_or_else(|| datadir::data_dir().to_string_lossy().to_string()),
            upstream_base_override,
            desensitize,
        )
    }

    /// 生产装配的实现体：调用方显式给数据目录（工具/测试用），**只读 `CODEBUDDY_AUTH_DIR`**。
    ///
    /// 与 Go 的差异（登记在 README）：Go 的 `poolStatePath` 优先读 `CODEBUDDY_POOL_DIR`
    /// 环境变量；本实现在**工具路径**上直接接收 `pool_dir` 参数（同名语义、同样的优先级），
    /// 避免测试为了一处路径去改进程级环境变量。
    pub fn production_with_data_dir(
        region: Region,
        data_dir: PathBuf,
        pool_dir: String,
        upstream_base_override: Option<String>,
        desensitize: bool,
    ) -> Result<(AppState, Arc<dyn CredSource>), String> {
        if !pool_dir.is_empty() {
            std::env::set_var("CODEBUDDY_POOL_DIR", &pool_dir);
        }
        let dirs = credential::find_auth_dirs().join("; ");
        let mut missing = format!(
            "未找到 {} 区域的 CodeBuddy/WorkBuddy 登录文件，请先在桌面端完成登录（已搜索：{}）",
            region.as_str(),
            dirs
        );
        let n = credential::count_encrypted_files(region);
        if n > 0 {
            missing = format!(
                "找到 {} 区域 {} 个登录文件但 token 均为新版加密形态（桌面端升级后加密存储，代理无法解密），已搜索：{}",
                region.as_str(),
                n,
                dirs
            );
        }
        let pool = AccountPool::new(region).ok_or_else(|| missing.clone())?;
        let base_url = match upstream_base_override {
            Some(b) if !b.is_empty() => b,
            _ => region.cfg().base_url.to_string(),
        };
        let cred = RealCredential::with_base_url("", region, &base_url);
        if cred.path().is_empty() {
            return Err(missing);
        }
        // 凭据文件内容也可能与期望区域不符（极端：目录里只有一个异域文件）——
        // 读取 domain 复核一次，不符即报错，不许跨区域回退。
        let dom = credential::credential_domain(&cred.path());
        if !crate::region::contains_domain(&dom, region.cfg().domain_suffixes) {
            return Err(format!(
                "未找到 {} 区域凭据（需要 auth.domain 含 {} 的登录文件）",
                region.as_str(),
                region.cfg().domain_suffixes.join(" 或 ")
            ));
        }
        let suffix = if region == Region::Intl { "-intl" } else { "" };
        let fail_path = data_dir
            .join(format!("codebuddy-failover{suffix}.json"))
            .to_string_lossy()
            .to_string();
        let stats_path = data_dir.join(format!("codebuddy-stats{suffix}.json"));
        let cred_arc: Arc<dyn CredSource> = Arc::new(cred);
        let state = AppState {
            region,
            cred: cred_arc.clone(),
            http: build_client(),
            base_url,
            probe_cache: Mutex::new(ProbeCache::default()),
            started_at: Instant::now(),
            diag_dir: data_dir,
            pool: Some(pool),
            quota: QuotaCache::new(),
            failover: Failover::load(&fail_path),
            desensitize,
            stats: Arc::new(Stats::load(stats_path)),
        };
        Ok((state, cred_arc))
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
        // 以下路由都挂 `any`：Go 的 handler 自己不判方法（只有 chat 与 failover/accounts
        // 在 handler 内判），挂 `get()`/`post()` 会在路由层先变 405，与 Go 分歧。
        .route("/health", any(handle_health))
        .route("/v1/models", any(models::handle_models))
        .route("/model/info", any(models::handle_model_info))
        .route("/v1/chat/completions", any(chat::handle_chat))
        .route("/quota", any(handle_quota))
        .route("/v1/failover", any(handle_failover))
        .route("/v1/accounts", any(handle_accounts))
        .route("/v1/stats", any(handle_stats))
        .fallback(any(handle_fallback))
        // chat 的读体口径是 Go 的 `io.LimitReader(64<<20)`（**截断**，不是拒绝），
        // 所以这里显式关掉 axum 的 2MB 默认上限——否则 >2MB 的请求会在本地被挡，
        // 而 Go 会照常转发（P3/P4 都栽过同一个默认值）。
        .layer(DefaultBodyLimit::disable())
        // Go `ServeMux` 的路径清洗在**路由之前**（差异 12：`//v1/models` → 307）。
        .layer(middleware::from_fn(path_clean_mw))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

/// Go `ServeMux.Handler` 的路径清洗：**匹配之前**先把非规范路径重定向出去。
///
/// 位置与 Go 同：在 `corsWith`（= 本 crate 的 `cors` 层）**之内**、路由**之外**——
/// 所以重定向响应同样带 CORS 头（实测 Go 的 307 确实带三个 CORS 头）。
async fn path_clean_mw(req: axum::extract::Request, next: middleware::Next) -> Response {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| format!("?{q}"));
    if let Some(r) = pathclean::clean_path_redirect(&path, query.as_deref(), req.method().as_str()) {
        let len = r.body.len();
        let mut resp = Response::new(Body::from(r.body));
        *resp.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::TEMPORARY_REDIRECT);
        let h = resp.headers_mut();
        h.insert(
            axum::http::header::LOCATION,
            HeaderValue::from_str(&r.location).unwrap_or_else(|_| HeaderValue::from_static("/")),
        );
        if let Some(ct) = r.content_type {
            h.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static(ct),
            );
        }
        // Go 侧这两种形态实测：GET 46 字节体、POST 0 字节体，Content-Length 都显式给出。
        h.insert(
            axum::http::header::CONTENT_LENGTH,
            HeaderValue::from_str(&len.to_string()).unwrap_or_else(|_| HeaderValue::from_static("0")),
        );
        return resp;
    }
    next.run(req).await
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

/// Go `handleStats`（不看方法）。
pub async fn handle_stats(State(st): State<Arc<AppState>>) -> Response {
    models::json_response(st.stats.response(st.started_at.elapsed().as_secs() as i64))
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
    // 第 43 轮：hy4-preview 限流窗口状态（GUI/脚本可观测；到期解除由 hy4_limited 懒处理）
    if let Some(lim) = st.failover.limit_state() {
        resp.insert("hy4_limit".to_string(), lim);
    }
    models::json_response(Value::Object(resp))
}

/// Go `handleQuota`（`?uid=` 指定账号、`?refresh=1` 强制实时）。
pub async fn handle_quota(
    State(st): State<Arc<AppState>>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let (uid, refresh) = parse_uid_refresh(query.as_deref());
    // 未知 uid 如实 source:none，err 只给 uid 前 8 位（不打完整 uid）。
    if !uid.is_empty() {
        if let Some(p) = &st.pool {
            if p.path_of(&uid).is_none() {
                let q = quota::Quota {
                    source: "none".to_string(),
                    err: format!("账号不在池中: {}", pool::uid_short(&uid)),
                    ..Default::default()
                };
                return models::json_response(q.to_json());
            }
        }    }
    let q = if refresh {
        quota::fetch_quota_force(
            &st.http,
            &st.base_url,
            st.cred.as_ref(),
            &st.quota,
            st.pool.as_ref(),
            &uid,
        )
        .await
    } else {
        quota::fetch_quota(
            &st.http,
            &st.base_url,
            st.cred.as_ref(),
            &st.quota,
            st.pool.as_ref(),
            &uid,
        )
        .await
    };
    models::json_response(q.to_json())
}

/// 极小查询串解析（Go `r.URL.Query()` 的取值子集：`uid` / `refresh`）。
fn parse_uid_refresh(query: Option<&str>) -> (String, bool) {
    let mut uid = String::new();
    let mut refresh = false;
    for pair in query.unwrap_or("").split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let v = percent_decode(v);
        match k {
            "uid" => uid = v,
            "refresh" if v == "1" => refresh = true,
            _ => {}
        }
    }
    (uid, refresh)
}

/// `net/url` 的百分号解码子集（`+` → 空格，`%XX` → 字节）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h * 16 + l);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Go `handleFailover`。
pub async fn handle_failover(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    match req.method() {
        &Method::GET => {
            let cfg = st.failover.config();
            models::json_response(gjson::obj(vec![
                ("enabled", Value::Bool(cfg.enabled)),
                ("fallback", gjson::s(cfg.fallback)),
                ("hy4_limit", failover::limit_state_or_null(&st.failover)),
            ]))
        }
        &Method::POST => {
            let raw = match chat::read_capped(req.into_body(), 1 << 20).await {
                Ok(b) => b,
                Err(e) => return chat::write_err(400, &format!("bad json: {e}")),
            };
            let body: Value = match serde_json::from_slice(&raw) {
                Ok(v) => v,
                Err(e) => return chat::write_err(400, &format!("bad json: {e}")),
            };
            let enabled = body.get("enabled").and_then(|x| x.as_bool());
            let fallback = body
                .get("fallback")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let candidates = st.region.cfg().candidates;
            match st.failover.update(enabled, fallback, candidates) {
                Err(msg) => chat::write_err(400, &msg),
                Ok(cfg) => {
                    st.failover.save();
                    println!(
                        "[codebuddy] failover 配置更新: enabled={} fallback={}",
                        cfg.enabled, cfg.fallback
                    );
                    // 响应体与 GET 同构补 hy4_limit：GUI 切开关后立即用 POST 响应渲染状态字。
                    models::json_response(gjson::obj(vec![
                        ("enabled", Value::Bool(cfg.enabled)),
                        ("fallback", gjson::s(cfg.fallback)),
                        ("hy4_limit", failover::limit_state_or_null(&st.failover)),
                    ]))
                }
            }
        }
        _ => chat::write_err(405, "method not allowed"),
    }
}

/// Go `handleAccounts`。
pub async fn handle_accounts(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    let Some(p) = st.pool.as_ref() else {
        // 零值 Server 守卫（同 handleChat），能起服务必有池
        return chat::write_err(500, "account pool unavailable");
    };
    match req.method() {
        &Method::GET => models::json_response(gjson::obj(vec![
            ("ok", Value::Bool(true)),
            ("accounts", snapshot_json(p)),
            ("region", gjson::s(st.region.as_str())),
        ])),
        &Method::POST => {
            let raw = match chat::read_capped(req.into_body(), 1 << 20).await {
                Ok(b) => b,
                Err(e) => return chat::write_err(400, &format!("bad json: {e}")),
            };
            let body: Value = match serde_json::from_slice(&raw) {
                Ok(v) => v,
                Err(e) => return chat::write_err(400, &format!("bad json: {e}")),
            };
            let action = body.get("action").and_then(|x| x.as_str()).unwrap_or("");
            let uid = body.get("uid").and_then(|x| x.as_str()).unwrap_or("");
            let enabled = body
                .get("enabled")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            match action {
                "rescan" => {
                    let before = p.size();
                    p.rescan(st.region);
                    println!(
                        "[codebuddy] 账号池 rescan region={}: {} → {} 个号",
                        st.region.as_str(),
                        before,
                        p.size()
                    );
                    models::json_response(gjson::obj(vec![
                        ("ok", Value::Bool(true)),
                        ("accounts", snapshot_json(p)),
                        ("region", gjson::s(st.region.as_str())),
                    ]))
                }
                "clear_cool" => {
                    let ok = p.clear_cool(uid);
                    println!(
                        "[codebuddy] 账号池 clear_cool uid={} ok={}",
                        pool::uid_short(uid),
                        ok
                    );
                    models::json_response(gjson::obj(vec![("ok", Value::Bool(ok))]))
                }
                "clear_exhausted" => {
                    let ok = p.clear_exhausted(uid);
                    println!(
                        "[codebuddy] 账号池 clear_exhausted uid={} ok={}",
                        pool::uid_short(uid),
                        ok
                    );
                    models::json_response(gjson::obj(vec![("ok", Value::Bool(ok))]))
                }
                "toggle" => {
                    let ok = p.toggle(uid, enabled);
                    println!(
                        "[codebuddy] 账号池 toggle uid={} enabled={} ok={}",
                        pool::uid_short(uid),
                        enabled,
                        ok
                    );
                    models::json_response(gjson::obj(vec![("ok", Value::Bool(ok))]))
                }
                "remove" => {
                    let ok = p.remove(uid);
                    println!(
                        "[codebuddy] 账号池 remove uid={} ok={}",
                        pool::uid_short(uid),
                        ok
                    );
                    models::json_response(gjson::obj(vec![("ok", Value::Bool(ok))]))
                }
                "restore" => {
                    let ok = p.restore(uid);
                    println!(
                        "[codebuddy] 账号池 restore uid={} ok={}",
                        pool::uid_short(uid),
                        ok
                    );
                    models::json_response(gjson::obj(vec![("ok", Value::Bool(ok))]))
                }
                other => chat::write_err(400, &format!("unknown action: {other}")),
            }
        }
        _ => chat::write_err(405, "method not allowed"),
    }
}

fn snapshot_json(p: &AccountPool) -> Value {
    Value::Array(p.snapshot().iter().map(|v| v.to_json()).collect())
}

// ---------- 入口 ----------

pub async fn run(args: Args) -> Result<(), String> {
    if !args.auth_dir.is_empty() {
        std::env::set_var("CODEBUDDY_AUTH_DIR", &args.auth_dir);
    }
    if !args.pool_dir.is_empty() {
        std::env::set_var("CODEBUDDY_POOL_DIR", &args.pool_dir);
    }
    let region = Region::parse(&args.region);
    let base_override = if args.upstream_base.is_empty() {
        None
    } else {
        Some(args.upstream_base.clone())
    };
    let diag_override = if args.diag_dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(&args.diag_dir))
    };
    let (state, _cred) = if args.cred_stub.is_empty() {
        let (mut st, cred) =
            AppState::production(region, args.desensitize, diag_override, None, base_override)?;
        // 生产装配已校验；这里只把凭据换成同实例的 Arc 以免二次构造。
        st.cred = cred.clone();
        (st, cred)
    } else {
        let text = std::fs::read_to_string(&args.cred_stub)
            .map_err(|e| format!("读取 --cred-stub {} 失败: {e}", args.cred_stub))?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析 --cred-stub {} 失败: {e}", args.cred_stub))?;
        let cred: Arc<dyn CredSource> = Arc::new(StubCredential::from_json(&v)?);
        let base_url = base_override.unwrap_or_else(|| region.cfg().base_url.to_string());
        let diag_dir = diag_override.unwrap_or_else(datadir::data_dir);
        let mut st = AppState::new(region, cred.clone(), base_url, diag_dir);
        st.desensitize = args.desensitize;
        (st, cred)
    };
    let state = Arc::new(state);
    let addr = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("端口 {} 被占用: {e}", args.port))?;
    println!(
        "[codebuddy] rs-codebuddy P5c（脱敏/SSE 清理/聚合/统计）监听 {} region={} base={} desensitize={} diag_dir={} stats={} pool={}",
        addr,
        region.as_str(),
        state.base_url,
        state.desensitize,
        state.diag_dir.display(),
        state.stats.path().display(),
        state.pool.as_ref().map(|p| p.size()).unwrap_or(0)
    );
    serve(listener, state).await.map_err(|e| e.to_string())
}

/// 生产 serve 入口：**自定义 `Listener` + `ConnectInfo`** 把「本连接已断开」的信号
/// 送到 handler（差异 13 的 499 取消源，见 `conn.rs`）。测试脚手架也走这里，
/// 保证被测的 accept 路径与生产完全同一条。
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
) -> std::io::Result<()> {
    let app = router(state).into_make_service_with_connect_info::<conn::ConnClosed>();
    axum::serve(conn::WatchedListener::new(listener), app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing_matches_go_subset() {
        assert_eq!(parse_uid_refresh(None), (String::new(), false));
        assert_eq!(
            parse_uid_refresh(Some("uid=u-1&refresh=1")),
            ("u-1".to_string(), true)
        );
        assert_eq!(
            parse_uid_refresh(Some("refresh=0&uid=u-2")),
            ("u-2".to_string(), false)
        );
        assert_eq!(parse_uid_refresh(Some("uid=a%2Db")), ("a-b".to_string(), false));
        assert_eq!(parse_uid_refresh(Some("uid=a+b")), ("a b".to_string(), false));
        assert_eq!(parse_uid_refresh(Some("refresh=1")), (String::new(), true));
    }
}
