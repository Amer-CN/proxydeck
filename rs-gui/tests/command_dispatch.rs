//! command_dispatch.rs —— 分发骨架的**真实进程**门禁（P7a 形态：单 exe 多子模式）。
//!
//! 只跑不产生副作用的那几条：`-version`、没有 Rust 实现的 `--plugin-tuanjie`、
//! 未落地的 `--fax`。**不跑** GUI 分支（会弹窗）、也**不跑**任何真插件子模式
//!（会占端口起服务）——那两类交给 P7c 的人工验收清单。

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rs-gui")
}

#[test]
fn version_flag_prints_the_go_compatible_line() {
    let out = Command::new(bin()).arg("-version").output().unwrap();
    assert!(out.status.success(), "-version 必须退出码 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.trim_end(),
        format!("ProxyDeck {} (proxy core {})", rs_gui::APP_VERSION, rs_gui::CORE_VERSION),
        "版本行必须与 Go `main.go:217` 的 `ProxyDeck %s (proxy core %s)` 同形"
    );
}

#[test]
fn plugin_submode_without_a_rust_crate_reports_honestly() {
    // 团结（8788）还没有 Rust crate：分发器必须如实报「尚未实现」，且**不占端口**
    let port = "18788";
    let out = Command::new(bin())
        .args(["--plugin-tuanjie", "--port", port])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "缺 crate 的子模式必须非零退出");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("尚无 Rust 实现"),
        "必须如实说明缺哪一块，不得静默继续：{stderr}"
    );
    // 端口没被占用（子模式根本没起来）
    let ln = std::net::TcpListener::bind(format!("127.0.0.1:{port}"));
    assert!(ln.is_ok(), "缺 crate 的子模式不该占用端口 {port}");
}

#[test]
fn fax_submode_reports_not_implemented() {
    let out = Command::new(bin()).arg("--fax").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("尚未实现"),
        "--fax 属 P7c，必须如实报未实现：{stderr}"
    );
}
