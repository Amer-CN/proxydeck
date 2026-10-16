//! command_dispatch.rs —— 分发骨架的**真实进程**门禁（P7a 形态：单 exe 多子模式）。
//!
//! 只跑不产生副作用的那几条：`-version`、没有 Rust 实现的 `--plugin-tuanjie`、
//! `-headless` 的失败自写日志（P7c）、以及失败子模式的诊断落盘（P7b）。
//! **不跑** GUI 分支（会弹窗，干扰正在用界面的用户），也**不跑**任何真插件子模式
//! （会占端口起服务）。
//!
//! ⚠ **`--fax` 刻意不在这条门禁里跑**（P7c 起它已真实现）：`run_fax_window` 会**真的建一个
//! 浮窗**，而且它会先抢 `Local\ProxyDeck-FaxWindow` 这个**进程名互斥体**——真跑一次就可能
//! 把用户正开着的注水浮窗挡在门外（或反过来让本进程的浮窗占住名字）。
//! 故 `--fax` 只做：① 单实例/标题/尺寸常量的差分对照（`tests/shell_contract.rs`）
//! ② 三绑定的差分（同上）③ 人工验收清单（`README.md`）。P7a~P7b 那条
//! 「`--fax` 如实报未实现」的用例已随实现落地而删除，不保留空转断言。

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

/// `-headless` 的**失败路径**必须自写 `headless-error.log`（Go `main.go:236-240`：
/// `appdir.AppendLog(appdir.Path("headless-error.log"), "headless 启动失败："+err)`）。
///
/// 怎么**确定性地**触发失败？用 `-port 99999`：`u16` 放不下 → clap 当场拒绝 →
/// `Mode::Proxy` 的解析失败分支走到 `headless-error.log`（端口被占那条路在 Go 里
/// 是「先杀再听」，正常不会走到失败，故只能拿参数错误这条确定路径做证据）。
/// Rust 的 `u16` 解析拒绝与 Go 的 `net.JoinHostPort` 报错都是「headless 起不来」，
/// 走的是**同一个失败收尾**——这正是本条要锁的东西。
#[test]
fn headless_failure_writes_its_own_log() {
    let dir = std::env::temp_dir().join(format!("rs-gui-p7c-headless-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let out = Command::new(bin())
        .args(["-headless", "-port", "99999"])
        .env(rs_gui::appdir::DATA_DIR_ENV, &dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "headless 起不来必须非零退出");

    let log = dir.join("headless-error.log");
    let body = std::fs::read_to_string(&log)
        .expect("headless 失败必须自写 headless-error.log（Go AppendLog 同源）");
    assert!(
        body.contains("headless 启动失败："),
        "正文必须以 Go 同款前缀开头：{body:?}"
    );
    assert!(body.ends_with('\n'), "AppendLog 每行一个换行：{body:?}");
    // AppendLog 的行头形态（Go 同款 2006-01-02 15:04:05）
    assert_eq!(&body[4..5], "-");
    assert_eq!(&body[7..8], "-");
    assert_eq!(&body[10..11], " ");
    assert_eq!(&body[13..14], ":");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 子模式失败必须写 `<id>-plugin-error.log`（Go `plugin_modes.go` 的 `pluginFailLog`）：
/// GUI 的退出监视就是读这个文件给「启动失败：…」的，缺了它界面只会停在「启动中」。
#[test]
fn plugin_submode_failure_writes_the_diagnostic_log() {
    let dir = std::env::temp_dir().join(format!("rs-gui-p7b-faillog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let out = Command::new(bin())
        .args(["--plugin-tuanjie", "--port", "18788"])
        .env(rs_gui::appdir::DATA_DIR_ENV, &dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "失败子模式必须非零退出");

    let log = dir.join("tuanjie-plugin-error.log");
    let body = std::fs::read_to_string(&log)
        .expect("子模式失败必须写 <id>-plugin-error.log（GUI 的退出监视读它）");
    assert!(
        body.contains(" 启动失败："),
        "AppendLog 的行头是 `YYYY-MM-DD HH:MM:SS `，正文以「启动失败：」开头：{body:?}"
    );
    assert!(body.contains("尚无 Rust 实现"), "诊断里要带上真实原因：{body:?}");
    assert!(body.ends_with('\n'), "AppendLog 每行一个换行：{body:?}");
    // AppendLog 的行头形态（Go 同款 2006-01-02 15:04:05）
    assert_eq!(&body[4..5], "-");
    assert_eq!(&body[7..8], "-");
    assert_eq!(&body[10..11], " ");
    assert_eq!(&body[13..14], ":");
    let _ = std::fs::remove_dir_all(&dir);
}
