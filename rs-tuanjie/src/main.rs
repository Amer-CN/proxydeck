//! rs-tuanjie 服务入口 —— 与 Go 侧 `--plugin-tuanjie --port 8788` 同形的命令行。
//!
//! 本 crate 仍在绞杀者路线的 **P8b 第三片**（服务骨架与路由表），**未注册进 `app/plugins.go`**、
//! 不参与 Go 构建、也不接管任何现行端口：默认端口与 Go 同值只为形态一致，
//! 真实运行请显式给测试端口（如 `--port 18788`）。

use std::sync::Arc;

use clap::Parser;

use rs_tuanjie::server::AppState;

#[derive(Parser, Debug)]
#[command(name = "rs-tuanjie", version, about = "ProxyDeck 团结插件（Rust 重写）")]
pub struct Args {
    /// 与 Go 侧 `--plugin-tuanjie` 同名，保持 GUI spawn 形态一致（本片不注册进 GUI，仅占位）。
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

/// **测试缝**（仅 `RS_TUANJIE_TEST_BASES` 显式存在时生效）：把账号域与推理域一起指向
/// 本机假上游，用于 P8b 第三片的差分对照（Rust 侧与 Go 侧打同一个假上游）。
///
/// * 生产运行**不带**这个变量，两个域恒为 `client.rs` 里的官方常量（红线 5 的换 key 路径
///   在 `Client::cli_api_key_url`，由 `codey_base` 拼出——同一处 seam，避开即生产形态不变）；
/// * `--port 8788`（生产端口）时**拒绝生效**并退出——防误把生产实例指到假上游。
const TEST_BASES_ENV: &str = "RS_TUANJIE_TEST_BASES";

fn apply_test_bases(port: u16) {
    let Ok(v) = std::env::var(TEST_BASES_ENV) else {
        return;
    };
    let mut it = v.split_whitespace();
    let (Some(codey), Some(litellm)) = (it.next(), it.next()) else {
        eprintln!("{TEST_BASES_ENV} 形态应为 \"<codey_base> <litellm_base>\"");
        std::process::exit(2);
    };
    if port == 8788 {
        eprintln!("拒绝：{TEST_BASES_ENV} 不允许与生产端口 8788 同时使用");
        std::process::exit(2);
    }    rs_tuanjie::logf(&format!(
        "[tuanjie] 测试缝生效：codey_base={codey} litellm_base={litellm}（仅对照用）"
    ));
    rs_tuanjie::client::set_test_bases(codey.to_string(), litellm.to_string());
}

/// **测试缝**（仅 `RS_TUANJIE_TEST_INFLIGHT` 显式存在时生效）：它的值是**触发器文件路径**，
/// 该文件存在时 `GET /inflight` 会预置一条合成进行中请求（详见 `server::maybe_seed_test_inflight`）。
/// 注入时机由对照 runner 在用例之间落文件控制；生产运行**不带**这个变量。
const TEST_INFLIGHT_ENV: &str = "RS_TUANJIE_TEST_INFLIGHT";

fn guard_test_seams(port: u16) {
    if port != 8788 {
        return;
    }
    for name in [TEST_BASES_ENV, TEST_INFLIGHT_ENV] {
        if std::env::var(name).is_ok() {
            eprintln!("拒绝：{name} 不允许与生产端口 8788 同时使用");
            std::process::exit(2);
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    guard_test_seams(args.port);
    if let Some(dir) = &args.data_dir {
        // 与 appdir.DataDirEnv 同一个变量名——两条路径（Rust/Go）用同一套判定链。
        std::env::set_var(rs_tuanjie::pacing::DATA_DIR_ENV, dir);
    }
    apply_test_bases(args.port);
    // 数据目录必须在任何状态文件读写之前定下来（Go 在 `main()` 开头 `runDataDirInit()`）：
    // 便携模式解析 → 历史状态文件迁移 → 明文凭据升级，有事情才写摘要日志。
    let notes = rs_tuanjie::appdir::run_data_dir_init(args.portable);
    for n in &notes {
        rs_tuanjie::logf(&format!("[tuanjie] 数据目录收口：{n}"));
    }
    let state = Arc::new(AppState::new().expect("构造服务状态失败"));
    // 种子卫兵后台循环（Go `SeedGuardLoop`：启动先扫一次，之后每小时一次）。
    //
    // **测试缝**：`RS_TUANJIE_TEST_BASES` 生效时**不起**该循环——它每轮都要查 npm 公网
    // （对照环境不该出网），生产运行不带该变量。
    if std::env::var(TEST_BASES_ENV).is_err() {
        let guard = state.seed_guard.clone();
        let http = state.client.http().clone();
        tokio::spawn(async move {
            loop {
                let (round, flip) = rs_tuanjie::seedcheck::run_seed_guard_check(&guard, &http).await;
                match flip {
                    Some("置位") => rs_tuanjie::logf(&format!(
                        "[tuanjie] seedguard 官方 CLI 签名特征已变化，告警置位：{}",
                        round.signal
                    )),
                    Some("解除") => rs_tuanjie::logf(
                        "[tuanjie] seedguard 告警解除（官方 bundle 签名特征恢复命中）",
                    ),
                    _ => {}
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }
    rs_tuanjie::logf(&format!(
        "[tuanjie] rs-tuanjie 监听 {}:{}（数据目录 {}，上游 {}）",
        args.host,
        args.port,
        rs_tuanjie::pacing::data_dir().display(),
        rs_tuanjie::server::default_litellm_base()
    ));
    let app = rs_tuanjie::server::router(state);
    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    axum::serve(listener, app).await
}
