//! rs-vibex 入口：`--plugin-vibex --host 127.0.0.1 --port 8790`。
//!
//! 与 Go 侧 `app/plugin_modes.go` 的分发同形，但**不注册进 GUI**（本阶段不碰 `plugins.go`）。
//!
//! P7a 起启动逻辑搬进 `lib.rs` 的 `rs_vibex::run(args)`（`rs-gui` 单 exe 分发要能复用它），
//! 本文件只剩「解析参数 → 调用 run → 按原有文案报错」这一层，行为逐字未变。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_vibex::Args::parse();
    if !args.plugin_vibex {
        eprintln!("请用 --plugin-vibex 启动（VibeX 插件服务模式）");
        std::process::exit(2);
    }
    if let Err(e) = rs_vibex::run(args).await {
        eprintln!("vibex-plugin: 启动失败: {e}");
        std::process::exit(1);
    }
}
