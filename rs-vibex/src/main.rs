//! rs-vibex 入口：`--plugin-vibex --host 127.0.0.1 --port 8790`。
//!
//! 与 Go 侧 `app/plugin_modes.go` 的分发同形，但**不注册进 GUI**（本阶段不碰 `plugins.go`）。
use clap::Parser;
use rs_vibex::config::{load_config, LoadInputs};
use rs_vibex::{datadir, logf, Seams, Server};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "rs-vibex", version, about = "VibeX 插件（REST+WS 协议客户端，本地 OpenAI 兼容）")]
struct Args {
    /// 插件服务模式（与 ProxyDeck 的 --plugin-vibex 同名）
    #[arg(long = "plugin-vibex")]
    plugin_vibex: bool,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8790)]
    port: u16,
    /// 单 token（也可用环境变量 RH_ACCESSTOKEN 或配置文件的 tokens 数组）
    #[arg(long = "vibex-token", default_value = "")]
    vibex_token: String,
    /// 配置文件路径（缺省 <数据目录>/vibex-config.json，权限 0600）
    #[arg(long = "vibex-config")]
    vibex_config: Option<PathBuf>,
    /// 便携模式（数据写 exe 同目录）
    #[arg(long)]
    portable: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if !args.plugin_vibex {
        eprintln!("请用 --plugin-vibex 启动（VibeX 插件服务模式）");
        std::process::exit(2);
    }
    datadir::init(args.portable);
    let env_token = std::env::var("RH_ACCESSTOKEN").unwrap_or_default();
    let cfg = load_config(&LoadInputs {
        config_path: args.vibex_config.clone(),
        flag_token: args.vibex_token.clone(),
        env_token: env_token.clone(),
    });
    logf(&format!(
        "配置：base_url={} app_name={} turn_timeout={}s ready_timeout={}s queue_limit={} heartbeat={}s",
        cfg.base_url(),
        cfg.app_name,
        cfg.turn_timeout_sec,
        cfg.ready_timeout_sec,
        cfg.queue_limit,
        cfg.heartbeat_sec
    ));
    let srv = Server::new(cfg, Seams::default());
    if let Err(e) = srv.serve(&args.host, args.port).await {
        eprintln!("vibex-plugin: 启动失败: {e}");
        std::process::exit(1);
    }
}
