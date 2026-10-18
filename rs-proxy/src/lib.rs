//! # rs-proxy —— ProxyDeck COMMAND 代理的 Rust 等价实现（绞杀者路线 P6）
//!
//! 替换对象：`internal/proxy`（9 文件）+ `internal/api` + `internal/server` + `internal/appdir`。
//! 对外是 **55990 上的COMMAND 代理**：CommandCode 路线（`/alpha/generate`）+ 三条入站协议
//! （OpenAI chat / OpenAI Responses / Anthropic Messages）。
//!
//! 逐字节对齐的底线与门禁见 `rs-proxy/README.md` 与 `tests/golden_proxy.rs`：
//! 出站体、下游 SSE/JSON 全部经 [`gjson`] 按 Go `encoding/json` 的规则写出。

pub mod api;
pub mod build;
pub mod chat;
pub mod convert;
pub mod cred_crypt;
pub mod gjson;
pub mod godecode;
pub mod model;
pub mod models;
pub mod probe;
pub mod responses;
pub mod stats;
pub mod stream;
pub mod upstream;
pub mod usage;
pub mod util;
pub mod anthropic;
pub mod http;
pub mod version;

pub use build::{
    build_request, create_upstream_request, CC_CONTEXT_LIMIT, DEFAULT_BASE_URL, DEFAULT_TIMEOUT,
};
pub use model::map_model;
pub use models::{ModelInfo, CATALOG_MODELS};
pub use stats::{UsageStats, UsageStatsSnapshot};
pub use upstream::UpstreamClient;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::Value;

/// `proxy.Proxy` 的等价物：COMMAND 代理的进程级状态。
///
/// 与 Go 的字段一一对应：`APIKey` / `BaseURL` / `Client` / `Debug` / `Stats`，
/// 外加 `/v1/usage` 的三件缓存（Go 里叫 `usageMu/usageData/usageAt`）。
pub struct Proxy {
    pub api_key: String,
    pub base_url: String,
    pub client: UpstreamClient,
    pub debug: bool,
    pub stats: Mutex<UsageStats>,
    /// `/v1/usage` 结果缓存（TTL 见 [`usage::USAGE_TTL`]）。
    pub usage_cache: Mutex<UsageCache>,
    /// 本次请求打到上游的次数（Go 用闭包局部计数；这里是进程级原子计数，门禁读它）。
    /// **每个请求开始前由 handler 重置**。
    pub upstream_hits: AtomicUsize,
    /// 出站 opencode.ai 的会话头（Go 在 `internal/tuanjie` 侧；COMMAND 代理不用，保留字段）。
    pub opencode_session: Option<String>,
}

/// `/v1/usage` 的成功结果缓存。
#[derive(Default)]
pub struct UsageCache {
    pub data: Option<Vec<u8>>,
    pub at: Option<std::time::Instant>,
}

impl Proxy {
    /// `NewProxy`：默认 baseURL 与默认 key。
    pub fn new(api_key: &str) -> Proxy {
        Proxy {
            api_key: api_key.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            client: UpstreamClient::new(),
            debug: false,
            stats: Mutex::new(UsageStats::new("")),
            usage_cache: Mutex::new(UsageCache::default()),
            upstream_hits: AtomicUsize::new(0),
            opencode_session: None,
        }
    }

    /// `SetStatsFile`：设置本地统计落盘路径（空串为 no-op，同 Go）。
    pub fn set_stats_file(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        self.stats = Mutex::new(UsageStats::new(path));
    }

    /// `CallUpstream`：把已构造的上游请求发出去（错误文案 `upstream error: <e>`，同 Go）。
    pub async fn call_upstream(
        &self,
        req: reqwest::Request,
    ) -> Result<reqwest::Response, String> {
        self.client
            .send(req)
            .await
            .map_err(|e| format!("upstream error: {e}"))
    }

    pub fn debugf(&self, msg: &str) {
        if self.debug {
            println!("{msg}");
        }
    }

    /// 取客户端键或服务端默认键（Go 三个 handler 开头那段）。
    ///
    /// 返回 `Err(())` 表示「无可用键」，调用方按各自的错误格式写 401。
    pub fn resolve_api_key(&self, auth_header: Option<&str>) -> Result<String, ()> {
        match auth_header {
            Some(h) if !h.is_empty() => {
                let k = h.strip_prefix("Bearer ").unwrap_or(h).trim().to_string();
                if k.is_empty() {
                    Err(())
                } else {
                    Ok(k)
                }
            }
            _ => {
                if self.api_key.is_empty() {
                    Err(())
                } else {
                    Ok(self.api_key.clone())
                }
            }
        }
    }

    /// 重置并读取上游命中计数（门禁用）。
    pub fn reset_hits(&self) {
        self.upstream_hits.store(0, Ordering::SeqCst);
    }

    pub fn hits(&self) -> usize {
        self.upstream_hits.load(Ordering::SeqCst)
    }

    pub fn add_hit(&self) {
        self.upstream_hits.fetch_add(1, Ordering::SeqCst);
    }
}

/// `truncateLog`：Go 的 debug 日志截断（20000 字节）。
pub fn truncate_log(s: &str) -> String {
    const LIMIT: usize = 20000;
    if s.len() <= LIMIT {
        return s.to_string();
    }
    format!(
        "{}... [truncated {} bytes]",
        &s[..floor_char_boundary(s, LIMIT)],
        s.len() - LIMIT
    )
}

/// 安全地找到不超过 `max` 的字符边界（Go 按字节切；Rust 必须落在合法边界，否则 panic）。
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 便捷：`MapModel` 的 JSON 形态。
pub fn map_model_value(name: &str) -> Value {
    Value::String(model::map_model(name))
}

// ===========================================================================
// HTTP 装配（`internal/server/server.go` 的路由表 + axum）
// ===========================================================================

/// `/health` 的响应体与 Content-Type（逐字节同 Go：Go 写的是裸字节、没设 Content-Type，
/// 由 `net/http` 的 sniff 规则判成 `text/plain; charset=utf-8`）。
pub const HEALTH_BODY: &str = "{\"status\":\"ok\"}";
pub const HEALTH_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_api_key_prefers_client_header() {
        let p = Proxy::new("server-key");
        assert_eq!(p.resolve_api_key(Some("Bearer client-key")).unwrap(), "client-key");
        assert_eq!(p.resolve_api_key(None).unwrap(), "server-key");
        assert_eq!(p.resolve_api_key(Some("")).unwrap(), "server-key");
        assert!(Proxy::new("").resolve_api_key(None).is_err());
        assert!(Proxy::new("").resolve_api_key(Some("  ")).is_err());
    }

    #[test]
    fn truncate_log_marks_byte_count() {
        let s = "x".repeat(20010);
        let t = truncate_log(&s);
        assert!(t.ends_with("... [truncated 10 bytes]"), "{t}");
        assert_eq!(truncate_log("short"), "short");
    }

    #[test]
    fn truncate_log_never_splits_utf8() {
        // 10000 个「中」= 30000 字节：截断点必须落在字符边界
        let s = "中".repeat(10000);
        let t = truncate_log(&s);
        assert!(t.starts_with('中') && t.contains("[truncated"));
    }

    #[test]
    fn health_body_is_byte_exact() {
        assert_eq!(HEALTH_BODY, "{\"status\":\"ok\"}");
        assert_eq!(HEALTH_BODY.len(), 15);
    }
}

/* ---------------- 可编程入口（P7a：`rs-gui` 单 exe 分发用） ----------------
 *
 * 边界例外（简报 P7 形态裁决第 3 条）：把 `main.rs` 的启动逻辑抽成 `pub async fn run(args)`，
 * `main` 改为调用它。**行为、参数名、日志文本、端口默认值全部零变化**：
 * 下面的 `Args`、数据目录判定函数与 `run` 都是从 `main.rs` 原样搬过来的，只加了 `pub`。
 */

/// 命令行参数（原 `main.rs` 的私有 `Args`，字段、默认值、帮助文本逐字未改）。
#[derive(clap::Parser, Debug)]
#[command(
    name = "rs-proxy",
    about = "ProxyDeck COMMAND 代理（Rust 重写，P6；等价 internal/proxy + internal/server）",
    version
)]
pub struct Args {
    /// 代理监听端口
    #[arg(long, default_value = "55990")]
    pub port: u16,
    /// 代理监听地址
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// CommandCode API Key（可选；也可放数据目录的 api-key.txt）
    #[arg(long, default_value = "")]
    pub api_key: String,
    /// 调试模式：打印请求/响应体到日志
    #[arg(long, default_value_t = false)]
    pub debug: bool,
    /// 便携模式：状态文件写 exe 同目录（缺省写 %APPDATA%\ProxyDeck）
    #[arg(long, default_value_t = false)]
    pub portable: bool,
    /// 统计文件路径覆盖（缺省为数据目录下的 stats.json）；测试用
    #[arg(long, default_value = "")]
    pub stats_file: String,
    /// 打印版本并退出
    #[arg(long, default_value_t = false)]
    pub version_only: bool,
}

/// 数据目录判定链（`internal/appdir` 的常用那一半：环境变量 → 便携 → `%APPDATA%`）。
///
/// 便携判定同 Go `appdir.Init`：**显式 `--portable` 或 exe 同目录的 `portable` 标记文件**
/// （`appdir.go:53-55` 的 `markerExists`：`os.Stat(ExeDir()/portable)` 且**不是目录**）。
pub fn data_dir(portable: bool) -> std::path::PathBuf {
    data_dir_with(portable, &exe_dir())
}

/// `data_dir` 的可注入版本（exe 目录由调用方给，便于测试）。
pub fn data_dir_with(portable: bool, exe_dir: &std::path::Path) -> std::path::PathBuf {
    if let Ok(d) = std::env::var("PROXYDECK_DATA_DIR") {
        let d = d.trim().to_string();
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    if portable || portable_marker_exists(exe_dir) {
        return exe_dir.to_path_buf();
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            return std::path::PathBuf::from(appdata).join("ProxyDeck");
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.trim().is_empty() {
            return std::path::PathBuf::from(home)
                .join("AppData")
                .join("Roaming")
                .join("ProxyDeck");
        }
    }
    exe_dir.to_path_buf()
}

/// exe 同目录的便携标记文件（名 `portable`，**必须是文件、不能是目录**——同 Go `markerExists`）。
fn portable_marker_exists(exe_dir: &std::path::Path) -> bool {
    match std::fs::metadata(exe_dir.join("portable")) {
        Ok(md) => !md.is_dir(),
        Err(_) => false,
    }
}

fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 启动COMMAND 代理（原 `main.rs` 里 `Args::parse()` 之后的全部逻辑，逐行原样搬入）。
///
/// 返回 `Err` = 服务启动失败；调用方按原有前缀打印（单独跑 `rs-proxy` 时是
/// `Server failed: {e}` + 退出码 1），文案与退出码零变化。
pub async fn run(args: Args) -> Result<(), String> {
    if args.version_only {
        println!("rs-proxy {} (proxy core P6)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // 版本号后台预热（对应 Go `version` 包的 init goroutine）；失败不阻塞
    crate::version::warm_up();

    let dir = data_dir(args.portable);
    let _ = std::fs::create_dir_all(&dir);

    let mut api_key = args.api_key.trim().to_string();
    if api_key.is_empty() {
        let p = dir.join("api-key.txt");
        // 同 Go：`app/main.go` 经 `appdir.ReadFile` 读——**明文与 `PDAPI1` 密文两形态都支持**
        // （v3.20.2 起 GUI 保存的 key 必为 DPAPI 密文；裸读会把密文当 key）。
        if let Ok(b) = crate::cred_crypt::read_file(&p) {
            api_key = String::from_utf8_lossy(&b).trim().to_string();
        }
    }

    let mut proxy = crate::Proxy::new(&api_key);
    proxy.debug = args.debug;
    let stats = if args.stats_file.is_empty() {
        dir.join("stats.json")
    } else {
        std::path::PathBuf::from(&args.stats_file)
    };
    proxy.set_stats_file(&stats.to_string_lossy());

    // 代理环境变量只报存在性、不打印值（变量值里可能带凭据）
    println!(
        "{}",
        crate::upstream::proxy_log_line(proxy.client.proxy_env_detected)
    );
    if proxy.client.proxy_env_detected {
        eprintln!("[warn] 检测到代理环境变量（值不打印）");
    }

    let proxy = std::sync::Arc::new(proxy);
    crate::http::serve(proxy, &args.host, args.port)
        .await
        .map_err(|e| e.to_string())
}


