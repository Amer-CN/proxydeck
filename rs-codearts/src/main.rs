//! rs-codearts 入口：`--plugin-codearts --port 8793`。
//!
//! 与其它插件同形：clap 解析 → `rs_codearts::run(args)`；失败打印并退出 1。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_codearts::Args::parse();
    if !args.plugin_codearts {
        eprintln!("请用 --plugin-codearts 启动（华为云码道 CodeArts 插件服务模式）");
        std::process::exit(2);
    }
    if let Err(e) = rs_codearts::run(args).await {
        eprintln!("codearts-plugin: 启动失败: {e}");
        std::process::exit(1);
    }
}
