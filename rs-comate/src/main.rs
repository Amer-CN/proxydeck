// main.rs —— rs-comate 可执行入口（Rust 重写，绞杀者路线 P2）。
//
// 形态对齐 Go 侧 `--plugin-comate --port <n>`：同一套参数名，便于将来并排对照与换装。
// 本阶段**不注册进 `app/plugins.go`**，GUI 不会 spawn 它；仅手动在测试端口拉起。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_comate::Args::parse();
    if let Err(e) = rs_comate::run(args).await {
        eprintln!("comate-plugin: 启动失败: {e}");
        std::process::exit(1);
    }
}
