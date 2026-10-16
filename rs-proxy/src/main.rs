//! `rs-proxy` 入口：对应 `ProxyDeck.exe -headless`（主代理核心）。
//!
//! 参数口径与 `app/main.go` 一致：`--port`（缺省 55990）/ `--host`（缺省 127.0.0.1）/
//! `--api-key`（缺省从数据目录的 `api-key.txt` 读）/ `--debug` / `--portable`。
//!
//! P7a 起启动逻辑搬进 `lib.rs` 的 `rs_proxy::run(args)`（`rs-gui` 单 exe 分发要能复用它），
//! 本文件只剩「解析参数 → 调用 run → 按原有文案报错」这一层，行为逐字未变。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_proxy::Args::parse();
    if let Err(e) = rs_proxy::run(args).await {
        eprintln!("Server failed: {e}");
        std::process::exit(1);
    }
}
