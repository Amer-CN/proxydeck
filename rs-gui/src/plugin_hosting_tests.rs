//! plugin_hosting_tests.rs —— **插件托管层的真实行为测试**（`#[cfg(test)]`，不进生产二进制）。
//!
//! 为什么单开一个文件：门禁里要真起进程、真占端口，且必须与其余改 `PROXYDECK_DATA_DIR`
//! 的用例串行（用本 crate 既有的 [`crate::testutil::env_lock`]）。
//!
//! 与「Go↔Rust 差分」（`tests/bridge_contract.rs`）的分工：
//!   * 差分锁的是**输出合同**（同输入同形 JSON、文案逐字）；
//!   * 本文件锁的是**行为**——探活接管、spawn 的本 exe 形态与 argv、退出监视的失败原因、
//!     停堆杀后复验、以及「端口还在占着就如实报错」。
//!
//! 三条硬纪律（简报要求）：
//!   1. 测试端口一律用 **1979x** 段，绝不碰 8785/8786/8787/8788/8789/8790/8891 与正在
//!      运行的 GUI/后端进程；
//!   2. 数据目录一律经 `PROXYDECK_DATA_DIR` 指到临时目录，不碰真实 `%APPDATA%\ProxyDeck`；
//!   3. 「假后端进程」是 `tests/fixtures/fake-backend.js`（node），它同时充当
//!      「被 spawn 的插件子进程」与「外部占位进程」两种角色——**没有 node 就当场失败**，
//!      不静默跳过（否则这些行为一条都没测）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use crate::appdir;
use crate::httpc;
use crate::plugins::{PluginDef, PluginHost, SpawnTarget};
use crate::testutil::env_lock;

/* ---------------- 脚手架 ---------------- */

fn fixture_js() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-backend.js")
}

/// 确认 node 可用（不可用就当场失败：这些行为测试没有替代品）。
fn require_node() {
    let ok = Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        ok,
        "本门禁要用 node 跑 tests/fixtures/fake-backend.js（假后端进程）；找不到 node 就把这一层行为全部漏掉，故直接失败"
    );
}

/// 假后端进程句柄（Drop 时收掉，绝不留孤儿占端口）。
struct FakeBackend {
    child: Option<Child>,
    port: u16,
}

impl FakeBackend {
    fn spawn(mode: &str, port: u16, extra: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new("node");
        cmd.arg(fixture_js())
            .arg("--plugin-zz-delta")
            .arg("--port")
            .arg(port.to_string());
        cmd.env("RS_GUI_FAKE_MODE", mode);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("起假后端失败（node 起不来？）");
        let b = Self {
            child: Some(child),
            port,
        };
        match mode {
            "serve" => b.wait_healthy(Duration::from_secs(10)),
            "raw" => b.wait_listening(Duration::from_secs(10)),
            _ => {}
        }
        b
    }

    fn wait_healthy(&self, timeout: Duration) {
        let url = format!("http://127.0.0.1:{}/health", self.port);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if httpc::ok(&url) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("假后端（serve）{} 秒内没在 {} 上健康", timeout.as_secs(), self.port);
    }

    fn wait_listening(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("假后端（raw）{} 秒内没占住端口 {}", timeout.as_secs(), self.port);
    }
}

impl Drop for FakeBackend {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// 进程内 HTTP 应答器：占住端口且 /health 回 200——**本进程自己**占的端口，
/// 于是 Go/Rust 的 `killByPort`（排除自己）杀不掉它，正好用来测「杀后复验如实报错」。
struct InProcHttp {
    _ln: TcpListener,
}

impl InProcHttp {
    fn start(port: u16) -> Self {
        let ln = TcpListener::bind(("127.0.0.1", port))
            .unwrap_or_else(|e| panic!("绑测试端口 {port} 失败（被占？）: {e}"));
        let ln2 = ln.try_clone().expect("克隆监听失败");
        std::thread::spawn(move || {
            for s in ln2.incoming() {
                let Ok(mut sock) = s else { break };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf);
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
                    );
                    let _ = sock.flush();
                });
            }
        });
        Self { _ln: ln }
    }
}

/// 隔离场景：新数据目录 + 新托管器（spawn 目标 = 假后端，生产目标另有断言）。
struct Fixture {
    dir: PathBuf,
    host: PluginHost,
}

impl Fixture {
    fn new(tag: &str, port: u16, native: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rs-gui-p7b-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);
        let def = PluginDef::new(
            "zz-delta",
            "Zz Delta",
            "",
            "",
            &["--desensitize"],
            port,
            "/health",
            native,
        );
        let host = PluginHost::new(
            vec![def],
            SpawnTarget {
                exe: Some(PathBuf::from("node")),
                prefix: vec![fixture_js().to_string_lossy().to_string()],
            },
        );
        Self { dir, host }
    }

    fn listing(&self) -> serde_json::Value {
        let mut v = self.host.list();
        assert_eq!(v.len(), 1, "测试注册表只有一个插件");
        v.remove(0)
    }

    /// 轮询到某个字段满足条件（退出监视是异步的）。
    fn wait_field<F: Fn(&serde_json::Value) -> bool>(&self, what: &str, f: F) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v = self.listing();
            if f(&v) {
                return v;
            }
            if Instant::now() > deadline {
                panic!("等 {what} 超时；最后看到：{v}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn err_log(&self) -> PathBuf {
        self.dir.join("zz-delta-plugin-error.log")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::env::remove_var(appdir::DATA_DIR_ENV);
        for k in [
            "RS_GUI_FAKE_MODE",
            "RS_GUI_FAKE_LOG",
            "RS_GUI_FAKE_EXIT",
            "RS_GUI_FAKE_ERRLOG",
            "RS_GUI_FAKE_ERRLINE",
        ] {
            std::env::remove_var(k);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/* ---------------- 用例 ---------------- */

/// spawn 形态：生产目标 = **本 exe**（`os.Executable()` 对应物）。
#[test]
fn production_spawn_target_is_this_exe() {
    let (exe, prefix) = SpawnTarget::default().resolve().expect("本 exe 必须定位得到");
    assert_eq!(exe, std::env::current_exe().unwrap());
    assert!(prefix.is_empty(), "生产目标不带前置参数");
}

/// spawn 形态：argv 逐位对齐 Go `pluginStart` 的原生分支
/// （`--plugin-<native> --port <n>` + `appdir.PortableArgs()` + `def.Args`），
/// 且子进程的输出真的落进 `<数据目录>/plugins/<id>.log`（带时间戳行头）。
#[test]
fn start_spawns_self_submode_with_go_arg_order() {
    let _g = env_lock().lock().unwrap();
    require_node();
    let fx = Fixture::new("spawn-argv", 19810, "zz-delta");
    let argv_log = fx.dir.join("argv.json");
    std::env::set_var("RS_GUI_FAKE_MODE", "exit");
    std::env::set_var("RS_GUI_FAKE_EXIT", "0");
    std::env::set_var("RS_GUI_FAKE_LOG", argv_log.to_string_lossy().to_string());

    fx.host.start("zz-delta").expect("启动必须成功");

    // 子进程把 argv 记进文件（假后端第一件事）
    let deadline = Instant::now() + Duration::from_secs(10);
    while !argv_log.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let raw = std::fs::read_to_string(&argv_log).expect("假后端应把 argv 落盘");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let args: Vec<String> = v["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        args,
        vec![
            "--plugin-zz-delta".to_string(),
            "--port".to_string(),
            "19810".to_string(),
            "--desensitize".to_string()
        ],
        "被 spawn 的子进程 argv 必须逐位是 --plugin-<native> --port <n> + def.Args"
    );
    assert_eq!(v["port"].as_str().unwrap(), "19810");
}

/// 探活接管：端口上已有健康实例 → 不 spawn、直接接管；陈旧报错自愈。
#[test]
fn start_takes_over_healthy_backend_and_heals_stale_error() {
    let _g = env_lock().lock().unwrap();
    require_node();
    let fx = Fixture::new("takeover", 19811, "zz-delta");

    // 先制造一次失败（子进程退出且不留诊断）→ lastErr 有值
    std::env::set_var("RS_GUI_FAKE_MODE", "exit");
    std::env::set_var("RS_GUI_FAKE_EXIT", "4");
    fx.host.start("zz-delta").unwrap();
    let failed = fx.wait_field("失败原因落位", |v| {
        v["lastErr"].as_str().map(|s| !s.is_empty()).unwrap_or(false)
    });
    assert_eq!(failed["running"], false, "子进程退出后不得还是 running");
    assert_eq!(
        failed["lastErr"].as_str().unwrap(),
        "Zz Delta 启动后退出：exit status 4"
    );
    assert_eq!(failed["healthy"], false);
    assert!(!fx.err_log().exists(), "退出诊断只在子模式失败时写");

    // 端口上出现健康实例（不经过 start）→ pluginList 必须把**存下来的**旧错清掉。
    // 判据要这样取才能区分「自愈」与「碰巧为空」：清掉之后再让端口掉线，
    // 若旧错还在状态里就会重新冒出来（Go: pluginList 里 st.lastErr = ""）。
    let holder = FakeBackend::spawn("serve", 19811, &[]);
    let healed = fx.listing();
    assert_eq!(healed["healthy"], true);
    assert_eq!(
        healed["lastErr"].as_str().unwrap(),
        "",
        "端口已健康 → 陈旧报错必须自愈"
    );
    drop(holder); // 外部实例下线
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::proc::port_busy("19811") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = fx.listing();
    assert_eq!(after["healthy"], false, "外部实例已下线");
    assert_eq!(
        after["lastErr"].as_str().unwrap(),
        "",
        "自愈必须写回状态：端口掉线后旧错不得复活"
    );

    // 再走一次接管路径（端口上重新有健康实例）→ start 直接复用、不起新进程
    let holder2 = FakeBackend::spawn("serve", 19811, &[]);
    std::env::remove_var("RS_GUI_FAKE_MODE");
    fx.host.start("zz-delta").expect("健康实例在，start 必须接管");
    let taken = fx.listing();
    assert_eq!(taken["running"], true, "接管后 running=true（外部实例）");
    assert_eq!(taken["healthy"], true);
    // 再 start 一次：已在运行
    assert_eq!(fx.host.start("zz-delta").unwrap_err(), "Zz Delta 已在运行");
    fx.host.stop("zz-delta").expect("外部实例按端口杀得掉");
    drop(holder2);
}

/// 退出监视：子进程写了诊断文件 → 失败原因逐字取诊断正文（前缀 `启动失败：`）。
#[test]
fn exit_watch_reports_child_diagnostic_verbatim() {
    let _g = env_lock().lock().unwrap();
    require_node();
    let fx = Fixture::new("diag", 19812, "zz-delta");
    let line = "2026-10-16 18:00:00 启动失败：端口 19812 被占用";
    std::env::set_var("RS_GUI_FAKE_MODE", "exit");
    std::env::set_var("RS_GUI_FAKE_EXIT", "0");
    std::env::set_var("RS_GUI_FAKE_ERRLOG", fx.err_log().to_string_lossy().to_string());
    std::env::set_var("RS_GUI_FAKE_ERRLINE", line);

    fx.host.start("zz-delta").expect("启动必须成功");
    let v = fx.wait_field("诊断原因落位", |v| {
        v["lastErr"].as_str().map(|s| !s.is_empty()).unwrap_or(false)
    });
    assert_eq!(v["running"], false);
    assert_eq!(
        v["lastErr"].as_str().unwrap(),
        format!("启动失败：{line}"),
        "退出原因 = '启动失败：' + 诊断正文（TrimSpace 后）"
    );
    // 再 start 一次必须清掉旧诊断（Go 在 Start 前 os.Remove(errLog)）
    std::env::remove_var("RS_GUI_FAKE_ERRLOG");
    std::env::remove_var("RS_GUI_FAKE_ERRLINE");
    std::env::set_var("RS_GUI_FAKE_EXIT", "5");
    fx.host.start("zz-delta").unwrap();
    let v = fx.wait_field("第二次失败原因", |v| {
        v["lastErr"].as_str() == Some("Zz Delta 启动后退出：exit status 5")
    });
    assert_eq!(v["lastErr"].as_str().unwrap(), "Zz Delta 启动后退出：exit status 5");
}

/// 停堆的**按 PID 收尾**那条路径：自 spawn 的子进程活着但**没监听端口**
/// （`idle` 假后端 = 起了没上线的僵尸），按端口杀找不到人，必须按 PID 真收掉它。
#[test]
fn stop_kills_the_spawned_child_by_pid() {
    let _g = env_lock().lock().unwrap();
    require_node();
    let fx = Fixture::new("stop-pid", 19815, "zz-delta");
    let argv_log = fx.dir.join("argv.json");
    std::env::set_var("RS_GUI_FAKE_MODE", "idle");
    std::env::set_var("RS_GUI_FAKE_LOG", argv_log.to_string_lossy().to_string());

    fx.host.start("zz-delta").expect("spawn 必须成功");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !argv_log.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&argv_log).expect("假后端应落盘 pid")).unwrap();
    let pid = v["pid"].as_u64().expect("假后端会记自己的 pid") as u32;
    // 守卫：无论这条用例成功还是失败，退出前都把子进程收掉（见 KillOnDrop 的注释）
    let _guard = KillOnDrop(pid);
    assert!(!crate::proc::port_busy("19815"), "idle 假后端不监听端口");
    assert!(pid_alive(pid), "子进程此刻必须活着（pid={pid}）");
    assert_eq!(fx.listing()["running"], true);

    fx.host.stop("zz-delta").expect("按 PID 也必须收得掉");
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(pid), "stop 必须真的杀掉自 spawn 的子进程（pid={pid}）");
    assert_eq!(fx.listing()["running"], false);
    assert_eq!(fx.listing()["lastErr"].as_str().unwrap(), "");
}

/// 进程是否还活着（`tasklist` 按 PID 查；查不到即已退出）。
fn pid_alive(pid: u32) -> bool {
    crate::proc::hidden_output("tasklist", &["/FI", &format!("PID eq {pid}"), "/NH"])
        .map(|s| s.contains(&pid.to_string()))
        .unwrap_or(false)
}

/// 「用例跑完（无论成败）都把子进程收掉」的守卫。
///
/// 为什么必须有：子进程由生产代码的监视线程持有，**用例断言失败时不会有人替它收尸**；
/// 而它继承了测试进程的 stdout/stderr 句柄——泄漏的进程会让调用方（pwsh 管道）永远
/// 等不到 EOF，整个门禁**挂死**。实测踩到：突变检验把 stop 的按 PID 收尾注掉后，
/// 这条用例把 15 条突变的脚本挂在了第 14 条上（脚本已加超时与清场，这里是根治）。
struct KillOnDrop(u32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = crate::proc::hidden_cmd("taskkill", &["/F", "/PID", &self.0.to_string()]).status();
    }
}

/// 停堆：外部健康实例被按端口杀掉 → 端口空出 → 状态复位（含日志面板的读法）。
#[test]
fn stop_kills_external_backend_and_verifies_port_free() {
    let _g = env_lock().lock().unwrap();
    require_node();
    let fx = Fixture::new("stop-ok", 19813, "zz-delta");
    let holder = FakeBackend::spawn("serve", 19813, &[]);

    fx.host.start("zz-delta").expect("接管健康实例");
    assert_eq!(fx.listing()["running"], true);
    fx.host.stop("zz-delta").expect("外部实例必须杀得掉");
    let after = fx.listing();
    assert_eq!(after["running"], false);
    assert_eq!(after["healthy"], false, "端口必须真空出来");
    assert_eq!(after["lastErr"].as_str().unwrap(), "");
    drop(holder);
}

/// 停堆杀后复验：端口被**杀不掉的**进程占着（本进程自己占）→ 如实报错 + 恢复跟踪。
#[test]
fn stop_reports_failure_when_port_stays_busy() {
    let _g = env_lock().lock().unwrap();
    let fx = Fixture::new("stop-busy", 19814, "zz-delta");
    let _holder = InProcHttp::start(19814); // 本进程占端口：killByPort 排除自己 → 杀不掉

    fx.host.start("zz-delta").expect("健康 → 接管");
    assert_eq!(fx.listing()["running"], true);

    let err = fx.host.stop("zz-delta").expect_err("杀不掉必须报错");
    assert_eq!(
        err,
        "旧进程杀不掉（端口 19814 仍被占）：对方可能以管理员身份在运行，请右键以管理员身份运行本程序后重试，或从任务管理器结束该进程"
    );
    let after = fx.listing();
    assert_eq!(
        after["running"], true,
        "杀失败要恢复跟踪：界面显示失败，不是先假装关掉再弹回 running"
    );
    assert_eq!(after["lastErr"].as_str().unwrap(), "", "恢复跟踪不清旧错");
}

/// F1 回归：停堆失败恢复跟踪时必须连 token 一起恢复（Go 恢复 `st.cmd` 的对等语义）。
///
/// 背景：`stop()` 开头先摘 pid/token（Go：`st.cmd = nil`），杀失败时恢复
/// （Go：`st.cmd = cmd`），进程将来退出时 watcher 按 token 比对才复位。
/// 旧实现只恢复 pid/running、不恢复 token，则 watcher 的世代号永远对不上——
/// 杀不掉的进程将来死亡后 running 恒 true 且无退出原因。
///
/// 为什么不断言"将来复位"本身：复位只发生在"杀失败时子进程还活着"的形态下，
/// 而同用户 taskkill /F 必能杀掉子进程（杀不掉的只有管理员进程，门禁里造不出来）。
/// 这里锁定的是可观测的另一半：**失败恢复后内部 (pid, token) 与失败前逐字相同**
/// （不恢复 token 的旧实现在这条用例必红——恢复后 token 恒 0）。
#[test]
fn stop_failure_restores_pid_and_token() {
    let _g = env_lock().lock().unwrap();
    require_node();
    let fx = Fixture::new("stop-token", 19908, "zz-delta");
    std::env::set_var("RS_GUI_FAKE_MODE", "idle");
    fx.host.start("zz-delta").expect("idle 子进程必须能起");
    let (pid_before, token_before) = fx.host.test_state_snapshot("zz-delta");
    assert!(token_before != 0, "spawn 后应有 watcher 世代号");
    let pid_before = pid_before.expect("spawn 后应有 pid");
    // 守卫：断言失败时也要把子进程收掉（它继承测试进程的 stdio 句柄，泄漏会挂死门禁）
    let _guard = KillOnDrop(pid_before);
    // 占住端口 → stop 里的 taskkill 能杀掉子进程、但 kill_by_port 杀不掉自己 → 失败恢复
    let _holder = InProcHttp::start(19908);
    let err = fx.host.stop("zz-delta").expect_err("杀不掉必须报错");
    assert!(
        err.starts_with("旧进程杀不掉"),
        "报错文案，实得：{err}"
    );
    let (pid_after, token_after) = fx.host.test_state_snapshot("zz-delta");
    assert_eq!(
        (pid_after, token_after),
        (Some(pid_before), token_before),
        "失败恢复必须原样恢复 pid 与 token（否则 watcher 世代号对不上）"
    );
    assert_eq!(fx.listing()["running"], true);
}

/// 未知 id 的三条文案与 Go 逐字同源。
#[test]
fn unknown_plugin_id_messages_match_go() {
    let _g = env_lock().lock().unwrap();
    let fx = Fixture::new("unknown", 19816, "zz-delta");
    assert_eq!(fx.host.start("zz-nope").unwrap_err(), "未知插件: zz-nope");
    assert_eq!(fx.host.stop("zz-nope").unwrap_err(), "未知插件: zz-nope");
    assert_eq!(
        fx.host.log_lines("zz-nope", 200),
        r#"{"ok":false,"msg":"未知插件"}"#
    );
    // log_lines 的 lines 归一：<=0 或 >500 都落 200（Go 同款）
    assert_eq!(fx.host.log_lines("zz-delta", 0), fx.host.log_lines("zz-delta", 200));
    assert_eq!(
        fx.host.log_lines("zz-delta", 200),
        r#"{"ok":false,"msg":"暂无日志"}"#
    );
}

/// 未健康但占着端口的进程（raw 假后端）会被先清理，再拉新实例。
#[test]
fn start_clears_non_healthy_port_holder_first() {
    let _g = env_lock().lock().unwrap();
    require_node();
    let fx = Fixture::new("clear", 19817, "zz-delta");
    let _holder = FakeBackend::spawn("raw", 19817, &[]);
    assert!(crate::proc::port_busy("19817"), "raw 假后端必须占住端口");

    std::env::set_var("RS_GUI_FAKE_MODE", "exit");
    std::env::set_var("RS_GUI_FAKE_EXIT", "6");
    fx.host.start("zz-delta").expect("清理非健康占位后必须拉起");
    let v = fx.wait_field("新实例退出原因", |v| {
        v["lastErr"].as_str() == Some("Zz Delta 启动后退出：exit status 6")
    });
    assert_eq!(v["running"], false);
}

/// 探活是并发做的（串行会把 WebView UI 线程拖死）：7 个不可达端口的总耗时必须
/// 远小于 7 × 1.5s 的超时上限。
#[test]
fn list_probes_are_concurrent() {
    let _g = env_lock().lock().unwrap();
    let dir = std::env::temp_dir().join("rs-gui-p7b-concurrent");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(appdir::DATA_DIR_ENV, &dir);
    // 端口全在保留段（无人监听）→ 每个探活都是「连不上」快速失败
    let defs: Vec<PluginDef> = (0..7)
        .map(|i| {
            PluginDef::new(
                &format!("zz-{i}"),
                "Zz",
                "",
                "",
                &[],
                19900 + i as u16,
                "/health",
                &format!("zz-{i}"),
            )
        })
        .collect();
    let host = PluginHost::new(defs, SpawnTarget::default());
    let t0 = Instant::now();
    let list = host.list();
    let spent = t0.elapsed();
    assert_eq!(list.len(), 7);
    assert!(list.iter().all(|v| v["healthy"] == false));
    assert!(
        spent < Duration::from_secs(3),
        "7 端口并发探活应远快于串行上限；实测 {spent:?}"
    );
    std::env::remove_var(appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 日志尾部读法（ccPluginLog 走的那条）：把数据目录日志换成一份真实文本，
/// 断言 `{"ok":true,"lines":[...]}` 的窗口与尾部换行处理。
#[test]
fn log_lines_reads_the_tail_like_go() {
    let _g = env_lock().lock().unwrap();
    let fx = Fixture::new("loglines", 19818, "zz-delta");
    std::fs::create_dir_all(fx.dir.join("plugins")).unwrap();
    let log = fx.dir.join("plugins").join("zz-delta.log");
    let mut body = String::new();
    for i in 1..=12 {
        body.push_str(&format!("2026-10-16 18:00:00 line{i}\n"));
    }
    body.push('\n'); // 尾部空行：TrimRight 掉
    std::fs::write(&log, body.as_bytes()).unwrap();

    let v: serde_json::Value = serde_json::from_str(&fx.host.log_lines("zz-delta", 2)).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["lines"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["lines"][1].as_str().unwrap(),
        "2026-10-16 18:00:00 line12"
    );
    // Go 的 lines 参数归一：0 / -1 / 501 都落 200
    let big: serde_json::Value = serde_json::from_str(&fx.host.log_lines("zz-delta", 501)).unwrap();
    assert_eq!(big["lines"].as_array().unwrap().len(), 12);
    let zero: serde_json::Value = serde_json::from_str(&fx.host.log_lines("zz-delta", 0)).unwrap();
    assert_eq!(zero["lines"].as_array().unwrap().len(), 12);
}

/// `ccPluginList` 的键集与取值（生产注册表的形态由差分门禁锁，这里锁字段语义）。
#[test]
fn list_entry_shape_matches_go_plugin_list() {
    let _g = env_lock().lock().unwrap();
    let fx = Fixture::new("shape", 19819, "zz-delta");
    let raw = serde_json::to_string(&fx.listing()).unwrap();
    assert_eq!(
        raw,
        concat!(
            r#"{"dir":"plugins","healthy":false,"id":"zz-delta","lastErr":"","log":"zz-delta.log","#,
            r#""name":"Zz Delta","native":true,"port":19819,"present":true,"running":false,"#,
            r#""url":"http://127.0.0.1:19819/v1"}"#
        ),
        "键序 = Go map marshal 的排序结果；字段一个不多一个不少"
    );
}

/// `pluginScript` 的存在性判据与 Go 同义（**目录也算存在**），非原生插件缺脚本时
/// 报「插件目录缺失」并带上 plugins/<dir> 路径。
#[test]
fn non_native_plugin_missing_script_reports_go_message() {
    let _g = env_lock().lock().unwrap();
    let dir = std::env::temp_dir().join("rs-gui-p7b-nonnative");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(appdir::DATA_DIR_ENV, &dir);
    let def = PluginDef::new(
        "zz-py",
        "Zz Py",
        "zz-no-such-dir-p7b",
        "main.py",
        &[],
        19820,
        "/health",
        "",
    );
    let host = PluginHost::new(vec![def], SpawnTarget::default());
    let entry = &host.list()[0];
    assert_eq!(entry["native"], false);
    assert_eq!(entry["present"], false);
    assert_eq!(
        entry["dir"].as_str().unwrap(),
        Path::new("plugins").join("zz-no-such-dir-p7b").to_string_lossy()
    );
    let err = host.start("zz-py").unwrap_err();
    let want_tail = format!(
        "插件目录缺失：{}\n请确认 plugins/zz-no-such-dir-p7b 已随程序放置",
        crate::plugins::plugins_dir()
            .join("zz-no-such-dir-p7b")
            .display()
    );
    assert_eq!(err, want_tail);
    std::env::remove_var(appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}
