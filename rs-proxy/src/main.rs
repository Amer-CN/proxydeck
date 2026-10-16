//! `rs-proxy` 入口：对应 `ProxyDeck.exe -headless`（主代理核心）。
//!
//! 参数口径与 `app/main.go` 一致：`--port`（缺省 55990）/ `--host`（缺省 127.0.0.1）/
//! `--api-key`（缺省从数据目录的 `api-key.txt` 读）/ `--debug` / `--portable`。
//!
//! ⚠ 数据目录判定链与 `internal/appdir` 同源（`PROXYDECK_DATA_DIR` → 便携标记 →
//! `%APPDATA%\ProxyDeck\`）；本 crate 目前只实现「读 api-key.txt」这一步，
//! DPAPI/ACL/迁移/日志轮转登记在 README 的剩余差距里。

use std::sync::Arc;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "rs-proxy",
    about = "ProxyDeck 主代理（Rust 重写，P6；等价 internal/proxy + internal/server）",
    version
)]
struct Args {
    /// 代理监听端口
    #[arg(long, default_value = "55990")]
    port: u16,
    /// 代理监听地址
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// CommandCode API Key（可选；也可放数据目录的 api-key.txt）
    #[arg(long, default_value = "")]
    api_key: String,
    /// 调试模式：打印请求/响应体到日志
    #[arg(long, default_value_t = false)]
    debug: bool,
    /// 便携模式：状态文件写 exe 同目录（缺省写 %APPDATA%\ProxyDeck）
    #[arg(long, default_value_t = false)]
    portable: bool,
    /// 统计文件路径覆盖（缺省为数据目录下的 stats.json）；测试用
    #[arg(long, default_value = "")]
    stats_file: String,
    /// 打印版本并退出
    #[arg(long, default_value_t = false)]
    version_only: bool,
}

/// 数据目录判定链（`internal/appdir` 的常用那一半：环境变量 → 便携 → `%APPDATA%`）。
///
/// 便携判定同 Go `appdir.Init`：**显式 `--portable` 或 exe 同目录的 `portable` 标记文件**
/// （`appdir.go:53-55` 的 `markerExists`：`os.Stat(ExeDir()/portable)` 且**不是目录**）。
pub fn data_dir(portable: bool) -> std::path::PathBuf {
    data_dir_with(portable, &exe_dir())
}

/// `data_dir` 的可注入版本（exe 目录由调用方给，便于测试）。
pub fn data_dir_with(portable: bool, exe_dir: &std::path::Path) -> std::path::PathBuf {
    if let Ok(d) = std::env::var("PROXYDECK_DATA_DIR") {
        let d = d.trim().to_string();
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    if portable || portable_marker_exists(exe_dir) {
        return exe_dir.to_path_buf();
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            return std::path::PathBuf::from(appdata).join("ProxyDeck");
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.trim().is_empty() {
            return std::path::PathBuf::from(home)
                .join("AppData")
                .join("Roaming")
                .join("ProxyDeck");
        }
    }
    exe_dir.to_path_buf()
}

/// exe 同目录的便携标记文件（名 `portable`，**必须是文件、不能是目录**——同 Go `markerExists`）。
fn portable_marker_exists(exe_dir: &std::path::Path) -> bool {
    match std::fs::metadata(exe_dir.join("portable")) {
        Ok(md) => !md.is_dir(),
        Err(_) => false,
    }
}

fn exe_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if args.version_only {
        println!("rs-proxy {} (proxy core P6)", env!("CARGO_PKG_VERSION"));
        return;
    }

    // 版本号后台预热（对应 Go `version` 包的 init goroutine）；失败不阻塞
    rs_proxy::version::warm_up();

    let dir = data_dir(args.portable);
    let _ = std::fs::create_dir_all(&dir);

    let mut api_key = args.api_key.trim().to_string();
    if api_key.is_empty() {
        let p = dir.join("api-key.txt");
        // 同 Go：`app/main.go` 经 `appdir.ReadFile` 读——**明文与 `PDAPI1` 密文两形态都支持**
        // （v3.20.2 起 GUI 保存的 key 必为 DPAPI 密文；裸读会把密文当 key）。
        match rs_proxy::cred_crypt::read_file(&p) {
            Ok(b) => api_key = String::from_utf8_lossy(&b).trim().to_string(),
            Err(_) => {}
        }
    }

    let mut proxy = rs_proxy::Proxy::new(&api_key);
    proxy.debug = args.debug;
    let stats = if args.stats_file.is_empty() {
        dir.join("stats.json")
    } else {
        std::path::PathBuf::from(&args.stats_file)
    };
    proxy.set_stats_file(&stats.to_string_lossy());

    // 代理环境变量只报存在性、不打印值（变量值里可能带凭据）
    println!(
        "{}",
        rs_proxy::upstream::proxy_log_line(proxy.client.proxy_env_detected)
    );
    if proxy.client.proxy_env_detected {
        eprintln!("[warn] 检测到代理环境变量（值不打印）");
    }

    let proxy = Arc::new(proxy);
    if let Err(e) = rs_proxy::http::serve(proxy, &args.host, args.port).await {
        eprintln!("Server failed: {e}");
        std::process::exit(1);
    }
}
