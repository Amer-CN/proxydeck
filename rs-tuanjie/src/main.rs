//! rs-tuanjie 服务入口 —— 与 Go 侧 `--plugin-tuanjie --port 8788` 同形的命令行。
//!
//! 本 crate 仍是绞杀者路线的**测试端口实例**（默认端口与 Go 同值只为形态一致，
//! 真实运行请显式给测试端口，如 `--port 18788`）：`internal/tuanjie` 的 Go 实现仍
//! 在 8788 上跑，**没有**被本 crate 接管。
//!
//! P7d-1 起启动逻辑搬进 `lib.rs` 的 [`rs_tuanjie::run`]（`rs-gui` 单 exe 分发要复用它），
//! 本文件只剩「解析参数 → 调用 `run` → 按原退出码报错」这一层，行为逐字未变。
use clap::Parser;

#[tokio::main]
async fn main() {
    let args = rs_tuanjie::Args::parse();
    if let Err(e) = rs_tuanjie::run(args).await {
        eprintln!("{e}");
        std::process::exit(e.exit_code());
    }
}
