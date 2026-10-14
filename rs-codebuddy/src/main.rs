// main.rs —— rs-codebuddy 可执行入口（Rust 重写，绞杀者路线 P5a：合同与请求侧）。
//
// 形态对齐 Go 侧 `--plugin-codebuddy --port <n> --region CN|INTL`：同一套参数名，
// 便于将来并排对照与换装。本阶段**不注册进 `app/plugins.go`**，GUI 不会 spawn 它；
// 仅手动在测试端口拉起（如 `--port 18787`），**绝不占用 8787/8789**。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_codebuddy::Args::parse();
    if let Err(e) = rs_codebuddy::run(args).await {
        eprintln!("codebuddy-plugin: 启动失败: {e}");
        std::process::exit(1);
    }
}
