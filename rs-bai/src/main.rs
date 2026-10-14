// main.rs —— rs-bai 可执行入口（Rust 试点）。
//
// 形态对齐 Go 侧 `--plugin-bai --port <n>`：同一套参数名，便于将来并排对照与换装。
// 本试点**不注册进 `app/plugins.go`**，GUI 不会 spawn 它；仅手动在测试端口拉起。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_bai::Args::parse();
    if let Err(e) = rs_bai::run(args).await {
        eprintln!("bai-plugin: 启动失败: {e}");
        std::process::exit(1);
    }
}
