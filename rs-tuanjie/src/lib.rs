//! rs-tuanjie —— 团结（Codely）插件 Rust 重写，**P8a 小轮：凭证链与请求整形**。
//!
//! 逐字节复刻 `internal/tuanjie/client.go` + `reqshape.go`（Go 侧只读不改）。八项红线
//! 的落点见 `README.md` 的「八项红线对照表」；每一项都有 Go 行号 + Rust 位置 + 黄金门禁。
//!
//! 本小轮**不含**服务与转发编排（`server.go`/`handlers_extra.go` 等，属 P8b）；
//! 出站 TLS 指纹的取舍见 `README.md`「uTLS 路线取证结论」（P8a 用 rustls，差异已登记）。
//!
//! **P8b 切片（本轮）**已迁入 `pacing`（KIMI-K3 429 兜底）/ `inflight`（进行中注册表 +
//! 实时动态）/ `opencode_session`（`x-opencode-session`）/ `dist`（分布指纹纯函数）/
//! `channels`（注水渠道静态表）/ `accounts`（账号池）/ `quota`（积分）/
//! `stats`（按模型累计 + 落盘）/ `server`（服务骨架与路由表）/ `chat`（**`handleChat`
//! 的池路径与单账号路径**，P8b 第四片）/ `providers`（外部账号 + 三协议分流）/
//! `responses_forward`（`/responses` 转发与翻回）/ `anthropic_forward`（`/v1/messages`）/
//! `media`（多模态能力判定 + schema 修复 + 远程图片预下载；生图/生视频与识图改路由
//! 已于 2026-10-18 按用户裁决删除）/ `probes`（探针集 + 基线内存态）
//! —— 后五个模块属 **P8b 第五片**；
//! 剩余面（`water.go` 的注水链路、`seedcheck.go`、`cdp_browser.go`、`model/info` 聚合）
//! 见 `README.md` 的未覆盖清单。

pub mod accounts;
pub mod anthropic_forward;
pub mod appdir;
pub mod baseline;
pub mod cdp_browser;
pub mod channels;
pub mod chat;
pub mod client;
pub mod credentials;
pub mod crypto;
pub mod dist;
pub mod gojson;
pub mod gziptar;
pub mod headers;
pub mod inflight;
pub mod media;
pub mod opencode_session;
pub mod pacing;
pub mod probes;
pub mod providers;
pub mod quota;
pub mod reqshape;
pub mod responses_forward;
pub mod seedcheck;
pub mod server;
pub mod session;
pub mod sign;
pub mod stats;
pub mod transport;
pub mod ua;
pub mod water;
pub mod water_handler;
pub mod winpaths;
pub mod winreg;

use std::time::Duration;

/// 团结侧的日志行（Go 用标准 `log`，带 `YYYY/MM/DD HH:MM:SS` 行头）。
/// 本 crate 的其余模块也用同一格式，便于与 8788 的日志逐行并排看。
pub fn logf(msg: &str) {
    eprintln!(
        "{} {}",
        chrono::Local::now().format("%Y/%m/%d %H:%M:%S"),
        msg
    );
}

/// 团结账号域（登录、换 key、dashboard、埋点）——`client.go:35`。
pub const CODEY_API_BASE: &str = "https://codely.tuanjie.cn";

/// 推理域（LiteLLM）——`client.go:36`。官方 CLI 的 chat 请求也直连这里。
pub const LITELLM_API_BASE: &str = "https://codely-litellm.tuanjie.cn";

/// 换 key 端点——`client.go:37`。**红线 5**：不换源、不加 `?teamId`。
pub const CLI_API_KEY_URL: &str = "https://codely.tuanjie.cn/api/api-token/cli-api-key";

/// cli_api_key 缓存时长——`client.go:38`。
pub const KEY_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 转发客户端总超时——`client.go:39`。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// 换 key 请求超时——`client.go:40`。
pub const KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// 统一的错误类型。文案尽量与 Go 侧一致（P8b 要把它翻成下游可读的响应）。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("读取团结登录态失败（请先登录团结 Cowork 桌面端）")]
    NoLoginState,
    #[error("读取团结登录态失败（请先登录团结 Cowork 桌面端）: {0}")]
    ReadLoginState(#[source] std::io::Error),
    #[error("解析 oauth_creds.json 失败: {0}")]
    ParseLoginState(#[source] serde_json::Error),
    #[error("oauth_creds.json 里没有 access_token")]
    EmptyAccessToken,
    #[error("换取 cli_api_key 网络失败: {0}")]
    FetchKeyNetwork(#[source] reqwest::Error),
    #[error("换取 cli_api_key 返回 {status}: {body}")]
    FetchKeyStatus { status: u16, body: String },
    #[error("换取 cli_api_key 响应异常: {0}")]
    FetchKeyBody(String),
    #[error("转发请求网络失败: {0}")]
    Forward(#[source] reqwest::Error),
}

/// 结果别名。
pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------- 可编程入口（P7d-1）

/* P7d-1：这把启动逻辑从 `main.rs` 搬进库里，`main` 与 `rs-gui` 的分发器**共用同一份**。
 *
 * 做法与 `rs-vibex` / `rs-proxy` 的先例一致（`Args` + `run(args)`），**行为零变化**：
 * 参数名、默认值、日志文案、启动顺序（数据目录收口 → 种子卫兵 → 监听）逐条未动，
 * 只多了两处**必须**的结构化回报——原来那两条测试缝守卫是 `std::process::exit(2)`，
 * 在库里不能退进程，改为 [`RunError::Refused`]（调用方据此还原退出码 2）。
 */

/// **测试缝**（仅 `RS_TUANJIE_TEST_BASES` 显式存在时生效）：把账号域与推理域一起指向
/// 本机假上游，用于对照（Rust 侧与 Go 侧打同一个假上游）。
///
/// * 生产运行**不带**这个变量，两个域恒为 `client.rs` 里的官方常量（红线 5 的换 key 路径
///   在 `Client::cli_api_key_url`，由 `codey_base` 拼出——同一处 seam，避开即生产形态不变）；
/// * `--port 8788`（生产端口）时**拒绝生效**并退出——防误把生产实例指到假上游。
pub const TEST_BASES_ENV: &str = "RS_TUANJIE_TEST_BASES";

/// **测试缝**（仅 `RS_TUANJIE_TEST_INFLIGHT` 显式存在时生效）：它的值是**触发器文件路径**，
/// 该文件存在时 `GET /inflight` 会预置一条合成进行中请求（详见 `server::maybe_seed_test_inflight`）。
/// 注入时机由对照 runner 在用例之间落文件控制；生产运行**不带**这个变量。
pub const TEST_INFLIGHT_ENV: &str = "RS_TUANJIE_TEST_INFLIGHT";

/// `--port 8788` 时**拒绝生效**的测试缝（`exit 2` 那条守卫）。
pub const TEST_SEAMS: [&str; 2] = [TEST_BASES_ENV, TEST_INFLIGHT_ENV];

/// 启动失败/拒绝启动的原因。
///
/// 退出码两档（与 Go `app/main.go` 的子模式同形）：
/// * `Refused` → **2**：测试缝与生产端口 8788 同用（含 `RS_TUANJIE_TEST_BASES` 形态不合法）；
/// * `Startup` → **1**：其余启动失败（绑定端口失败、构造服务状态失败）。
#[derive(Debug)]
pub enum RunError {
    /// 用法/守卫类拒绝（退出码 2）。
    Refused(String),
    /// 启动失败（退出码 1）。
    Startup(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Refused(msg) | RunError::Startup(msg) => f.write_str(msg),
        }
    }
}

impl RunError {
    /// 还原进程退出码（单独跑 `rs-tuanjie` 与 `rs-gui --plugin-tuanjie` 同一口径）。
    pub fn exit_code(&self) -> i32 {
        match self {
            RunError::Refused(_) => 2,
            RunError::Startup(_) => 1,
        }
    }
}

/// 插件子模式命令行参数（原 `main.rs` 的 `Args`，字段与默认值逐字未改，只加了 `pub`）。
#[derive(clap::Parser, Debug)]
#[command(name = "rs-tuanjie", version, about = "ProxyDeck 团结插件（Rust 重写）")]
pub struct Args {
    /// 与 Go 侧 `--plugin-tuanjie` 同名，保持 GUI spawn 形态一致
    /// （P7d-1 起本 crate 已接进 `rs-gui` 的分发器，`--plugin-tuanjie --port N` 真起服务）。
    #[arg(long = "plugin-tuanjie", default_value_t = false)]
    pub plugin_tuanjie: bool,

    /// 监听端口（默认与 Go 侧 8788 同值；测试请显式指定，如 18788）。
    #[arg(long, default_value = "8788")]
    pub port: u16,

    /// 监听地址（Go 侧同款默认回环）。
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// 数据目录（默认按 appdir 判定链：`PROXYDECK_DATA_DIR` → 便携标记 → `%APPDATA%\ProxyDeck\`）。
    /// 测试实例请指向临时目录，避免碰到真实账号池与统计文件。
    #[arg(long)]
    pub data_dir: Option<String>,

    /// 便携模式：状态文件写 exe 同目录（等价于 exe 同目录存在 `portable` 标记文件）——
    /// 与 Go 侧 `-portable` 同名同义（`app/main.go:54`）。
    #[arg(long, default_value_t = false)]
    pub portable: bool,
}

/// 测试缝：把账号域与推理域一起指向本机假上游（`main.rs` 原 `apply_test_bases` 原样搬入）。
///
/// 不合法的变量形态、以及与生产端口 8788 同用，都返回 [`RunError::Refused`]（退出码 2）。
pub fn apply_test_bases(port: u16) -> std::result::Result<(), RunError> {
    let Ok(v) = std::env::var(TEST_BASES_ENV) else {
        return Ok(());
    };
    let mut it = v.split_whitespace();
    let (Some(codey), Some(litellm)) = (it.next(), it.next()) else {
        return Err(RunError::Refused(format!(
            "{TEST_BASES_ENV} 形态应为 \"<codey_base> <litellm_base>\""
        )));
    };
    if port == 8788 {
        return Err(RunError::Refused(format!(
            "拒绝：{TEST_BASES_ENV} 不允许与生产端口 8788 同时使用"
        )));
    }
    logf(&format!(
        "[tuanjie] 测试缝生效：codey_base={codey} litellm_base={litellm}（仅对照用）"
    ));
    client::set_test_bases(codey.to_string(), litellm.to_string());
    Ok(())
}

/// 生产端口 8788 下**拒绝**两条测试缝（`main.rs` 原 `guard_test_seams` 原样搬入）。
pub fn guard_test_seams(port: u16) -> std::result::Result<(), RunError> {
    if port != 8788 {
        return Ok(());
    }
    for name in TEST_SEAMS {
        if std::env::var(name).is_ok() {
            return Err(RunError::Refused(format!(
                "拒绝：{name} 不允许与生产端口 8788 同时使用"
            )));
        }
    }
    Ok(())
}

/// 启动插件服务（原 `main.rs` 的 `main` 主体，逐行搬入；退出码语义见 [`RunError`]）。
///
/// 启动顺序与单独跑 `rs-tuanjie` 时**逐条相同**：
/// `guard_test_seams` → `--data-dir` 钉进环境 → `apply_test_bases` → `run_data_dir_init`
/// → 构造 `AppState` → 种子卫兵后台循环 → 监听。
pub async fn run(args: Args) -> std::result::Result<(), RunError> {
    guard_test_seams(args.port)?;
    if let Some(dir) = &args.data_dir {
        // 与 appdir.DataDirEnv 同一个变量名——两条路径（Rust/Go）用同一套判定链。
        std::env::set_var(appdir::DATA_DIR_ENV, dir);
    }
    apply_test_bases(args.port)?;
    // 数据目录必须在任何状态文件读写之前定下来（Go 在 `main()` 开头 `runDataDirInit()`）：
    // 便携模式解析 → 历史状态文件迁移 → 明文凭据升级，有事情才写摘要日志。
    let notes = appdir::run_data_dir_init(args.portable);
    for n in &notes {
        logf(&format!("[tuanjie] 数据目录收口：{n}"));
    }
    // **失败形态与搬运前逐字相同**：原 `main` 在这里是 `.expect("构造服务状态失败")`
    // （panic + 退出码 101），不是打印一行后 `exit 1`；搬库时**不顺手改**这个。
    let state = std::sync::Arc::new(server::AppState::new().expect("构造服务状态失败"));
    // 种子卫兵后台循环（Go `SeedGuardLoop`：启动先扫一次，之后每小时一次）。
    //
    // **测试缝**：`RS_TUANJIE_TEST_BASES` 生效时**不起**该循环——它每轮都要查 npm 公网
    // （对照环境不该出网），生产运行不带该变量。
    if std::env::var(TEST_BASES_ENV).is_err() {
        let guard = state.seed_guard.clone();
        let http = state.client.http().clone();
        tokio::spawn(async move {
            loop {
                let (round, flip) = seedcheck::run_seed_guard_check(&guard, &http).await;
                match flip {
                    Some("置位") => logf(&format!(
                        "[tuanjie] seedguard 官方 CLI 签名特征已变化，告警置位：{}",
                        round.signal
                    )),
                    Some("解除") => {
                        logf("[tuanjie] seedguard 告警解除（官方 bundle 签名特征恢复命中）")
                    }
                    _ => {}
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }
    logf(&format!(
        "[tuanjie] rs-tuanjie 监听 {}:{}（数据目录 {}，上游 {}）",
        args.host,
        args.port,
        pacing::data_dir().display(),
        server::default_litellm_base()
    ));
    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port))
        .await
        .map_err(|e| RunError::Startup(format!("监听 {}:{} 失败: {e}", args.host, args.port)))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| RunError::Startup(format!("服务退出: {e}")))
}
