//! rs-opencode 入口：`--plugin-opencode --host 127.0.0.1 --port 8784`。
//!
//! 与其它插件同形：参数解析 → `rs_opencode::run(args)`，失败按原有文案打印并退出 1。
//! 本阶段**不注册进 `rs-gui`**（dispatch/plugins 放第二版）。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_opencode::Args::parse();
    if !args.plugin_opencode {
        eprintln!("请用 --plugin-opencode 启动（OpenCode 插件服务模式）");
        std::process::exit(2);
    }
    if let Err(e) = rs_opencode::run(args).await {
        eprintln!("opencode-plugin: 启动失败: {e}");
        std::process::exit(1);
    }
}
