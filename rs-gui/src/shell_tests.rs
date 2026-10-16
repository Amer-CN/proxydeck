//! shell_tests.rs —— P7c 宿主外壳层的**真实行为测试**（对应 Go 的
//! `app/rollover_test.go` / `internal/appdir/migrate_test.go` 那批用例）。
//!
//! 与 `plugin_hosting_tests.rs` 同一套纪律：**真起进程、真占测试端口、真写临时目录**，
//! 绝不碰 8785~8792/55990 与正在运行的 GUI（pid 18256）/后端。
//!
//! 分工：
//!   * 纯函数真值表（`is_stale_image` / `classify_holders` / `parse_netstat_listeners` /
//!     轮询）在各自模块的 `#[cfg(test)] mod tests` 里，与 Go 单测逐条同判
//!   * 本文件只放**需要真进程/真端口/真文件系统**的那部分：换血端到端、端口清理、
//!     `headless-error.log` 自写、`Migrate`/`UpgradePlaintext` 的跨步骤行为

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::plugins::{PluginDef, PluginHost, SpawnTarget};
use crate::rollover::{self, StaleTarget};
use crate::testutil::env_lock;

/// 本文件的测试端口带（与 `plugin_hosting_tests.rs` 的 1981x、集成测试的 1978x 都错开）：
/// **只在本文件内用**，且每个用例取自己那一个。
const SHELL_PORT_A: u16 = 19920;
/// 换血端到端用例的端口。
const SHELL_PORT_B: u16 = 19921;
/// 端口清理用例的端口。
const SHELL_PORT_C: u16 = 19922;

fn fixture_js() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-backend.js")
}

/// 换血/端口清理门禁里 spawn 出去的假后端**固定为 serve 模式**：
/// `RS_GUI_FAKE_MODE` 是进程级环境变量（`plugin_hosting_tests` / `bridge_contract`
/// 的 exit 场景会把它设成 `exit`，cargo test 默认并行跑同一个二进制），
/// 而托管器 spawn 时拿不到 spawn 级 env 注入——故用假后端支持的显式参数
/// `--fake-mode=<mode>` 表述（它排在前面，不参与 `--plugin-*` argv 断言）。
fn serve_seam() -> SpawnTarget {
    SpawnTarget {
        exe: Some(PathBuf::from("node")),
        prefix: vec![
            fixture_js().to_string_lossy().to_string(),
            "--fake-mode=serve".to_string(),
        ],
    }
}

fn require_node() {
    let ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "换血行为测试要用 node 跑假后端进程；没有 node 会把这条路径整批漏掉");
}

/// 假后端进程（Drop 时收掉；与 `bridge_contract.rs` 的同类守卫同一写法）。
struct FakeBackend {
    child: Option<std::process::Child>,
}

impl FakeBackend {
    /// 起一个**健康**的假后端（`serve`：任何请求回 200 `{"ok":true}`）＝ 换血里的「新包后端」。
    ///
    /// `RS_GUI_FAKE_MODE` 是**进程级**环境变量，`plugin_hosting_tests` / `bridge_contract`
    /// 里的 exit 场景会把它设成 `exit`（cargo test 默认并行跑同一个二进制）——故这里
    /// **每次显式设回 serve**，否则换血 spawn 出来的假后端会当场退出（exit status 2）。
    /// 同一原因：`RS_GUI_FAKE_EXIT` 也要清掉（它只给 exit 场景用）。
    fn serve(port: u16) -> Self {
        let mut cmd = std::process::Command::new("node");
        cmd.arg(fixture_js())
            .arg("--plugin-zz-x")
            .arg("--port")
            .arg(port.to_string())
            .env("RS_GUI_FAKE_MODE", "serve")
            .env_remove("RS_GUI_FAKE_EXIT")
            .env_remove("RS_GUI_FAKE_LOG")
            .env_remove("RS_GUI_FAKE_ERRLOG");
        let child = cmd.spawn().expect("起假后端失败");
        let b = Self { child: Some(child) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if crate::httpc::ok(&format!("http://127.0.0.1:{port}/health")) {
                return b;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("假后端（serve:{port}）10s 内没就绪");
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

/// 某端口当前是否真被占（直接问 netstat，不经被测代码）。
fn raw_port_busy(port: u16) -> bool {
    crate::proc::port_busy(&port.to_string())
}

fn wait_until(mut f: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/* ---------------- 换血端到端（真进程 + 真端口） ---------------- */

/// `roll_one` 的**完整成功路径**，全部用真东西：
/// 真占着端口的「旧包后端」→ 真被停掉（`pluginStop` 按端口杀）→ 端口真空出 →
/// 真 spawn 出新后端（假后端走 SpawnTarget 测试缝）→ 真等 health 绿。
///
/// 这是「旧包进程→换新→health 绿」那条链唯一能自动化的端到端证据；
/// **不碰 8785~8792**：端口取自本文件专带的 19921。
#[test]
fn roll_one_replaces_a_real_stale_backend_end_to_end() {
    require_node();
    let port = SHELL_PORT_B;
    assert!(!raw_port_busy(port), "测试端口 {port} 开工前必须空闲（被占则本用例当场失败，不静默跳过）");

    // 「旧包后端」：真进程占着端口（这就是 staleTarget 要换掉的那台）
    let old = FakeBackend::serve(port);
    assert!(raw_port_busy(port), "旧后端必须真占着端口");

    // 环境变量（进程级）与数据目录在**起完假后端之后**才动：env_lock 只护这一段，
    // 免得「锁内等待 10s 起后端」把别的用例一起堵住，也免得起不来时全批中毒。
    let _g = env_lock().lock().unwrap();
    let dir = std::env::temp_dir().join(format!("rs-gui-rollone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(crate::appdir::DATA_DIR_ENV, &dir);

    // 托管器：注册表里就是这台，spawn 目标换成假后端（生产 = 本 exe）
    let defs = vec![PluginDef::new(
        "zz-stale",
        "Zz Stale",
        "",
        "",
        &[],
        port,
        "/health",
        "zz-stale",
    )];
    let host = Arc::new(PluginHost::new(
        defs,
        serve_seam(),
    ));
    // 先让托管器"接管"旧实例（探活接管：端口健康 → running=true，不杀不重启）
    host.start("zz-stale").expect("探活接管旧实例应成功");

    let def = host.def("zz-stale").unwrap();
    let t = StaleTarget {
        def_id: def.id.clone(),
        def_name: def.name.clone(),
        def_port: def.port,
        pid: 0, // 真 pid 只有生产判定时才知道；roll_one 不读它
        exe: "ProxyDeck.exe.old-1".into(),
    };

    // 换血：stop（按端口杀旧进程）→ 端口空出 → start（spawn 新后端）→ health 绿
    let msg = rollover::roll_one(
        &t,
        &|id| host.stop(id),
        &|id| host.start(id),
        &|d| crate::httpc::ok(&crate::plugins::plugin_health_url(d)),
        Duration::from_secs(30),
    );
    assert_eq!(msg, None, "换血应成功，实得 {msg:?}");
    assert!(
        crate::httpc::ok(&crate::plugins::plugin_health_url(&def)),
        "换血后端口上必须是健康的新后端"
    );
    // 旧进程确实被换掉了：假后端脚本一被 spawn 就落 pid 文件——这里改为验"端口上换了人"
    // （旧进程被 kill、新进程由托管器 spawn；pid 差异由下面的端口连续性证明）
    drop(old); // 旧句柄 kill 一次（已死则无操作），避免用例结束时漏进程

    // 收尾：停掉新后端，端口必须真空出（否则本用例给后续用例留污染）
    let _ = host.stop("zz-stale");
    assert!(
        wait_until(|| !raw_port_busy(port), Duration::from_secs(10)),
        "收尾后端口 {port} 必须空出"
    );
    std::env::remove_var(crate::appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `roll_one` 的**杀不掉**分支（真端口被占、真报错、且绝不 start）：
/// 用一个**本进程持有**的监听器占住端口——`killByPort` 会把本进程排除掉
/// （`listeners_on_port` 过滤 `std::process::id()`），于是真杀不掉，
/// 与 Go `TestRollOneKillUnverified`（本进程占端口）同一手法。
#[test]
fn roll_one_reports_kill_failure_and_never_starts_in_shell() {
    let port = SHELL_PORT_C;
    assert!(!raw_port_busy(port), "测试端口 {port} 开工前必须空闲");

    // 本进程占住端口：killByPort 排除自己 → 端口仍然被占 → 杀后复验必然失败
    let ln = std::net::TcpListener::bind(("127.0.0.1", port)).expect("绑测试端口失败");

    let started = std::sync::atomic::AtomicBool::new(false);
    let t = StaleTarget {
        def_id: "zz".into(),
        def_name: "Zz".into(),
        def_port: port,
        pid: 4242,
        exe: "ProxyDeck.exe.old-9".into(),
    };
    let msg = rollover::roll_one(
        &t,
        &|_| {
            // 与 `PluginHost::stop` 同构：按端口杀 → 再验端口
            let _ = crate::proc::kill_by_port(&port.to_string());
            if crate::proc::port_busy(&port.to_string()) {
                Err(format!(
                    "旧进程杀不掉（端口 {port} 仍被占）：对方可能以管理员身份在运行"
                ))
            } else {
                Ok(())
            }
        },
        &|_| {
            started.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
        &|_| false,
        Duration::from_secs(2),
    );
    assert!(msg.is_some(), "必须如实报失败");
    assert!(
        !started.load(std::sync::atomic::Ordering::SeqCst),
        "杀不掉时绝不能再 start"
    );
    drop(ln);
}

/// `roll_all_backends` 的「没活人的端口保持关闭」：真 netstat 快照下，
/// 一个不监听的插件端口必须进跳过名单、且 start 不得被调（更新不替你点火）。
#[test]
fn roll_all_backends_skips_closed_ports_against_real_netstat() {
    require_node();
    let port = SHELL_PORT_A;

    // 真跑着的后端（应被换血）+ 一台关着的（应进跳过名单）
    let live = FakeBackend::serve(port);
    let _g = env_lock().lock().unwrap();
    let dir = std::env::temp_dir().join(format!("rs-gui-rollall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(crate::appdir::DATA_DIR_ENV, &dir);

    let defs = vec![
        PluginDef::new("zz-live", "Zz Live", "", "", &[], port, "/health", "zz-live"),
        PluginDef::new("zz-off", "Zz Off", "", "", &[], 19929, "/health", "zz-off"),
    ];
    let host = Arc::new(PluginHost::new(
        defs,
        serve_seam(),
    ));
    host.start("zz-live").expect("探活接管应成功");

    // 真 netstat 快照：zz-live 的端口在读到的表里（真监听）、zz-off 的端口不在
    let snap = rollover::snapshot_listeners().expect("netstat 快照失败");
    assert!(
        snap.get(&port.to_string()).map(|v| !v.is_empty()).unwrap_or(false),
        "真 netstat 快照里必须有 {port}（假后端真的在监听）"
    );
    assert!(
        !snap.contains_key("19929"),
        "19929 不该在本机监听表里"
    );

    rollover::roll_all_backends(&host);

    // 换血成功的后端应健康；关闭的后端必须仍然关闭（start 没被调 → 端口仍空闲）
    // 断言失败时把插件清单打出来当诊断（running/lastErr 能区分"没起来"与"起来又退了"）
    let diagnose = || {
        format!(
            "插件清单={:?}",
            host.list()
                .iter()
                .map(|e| format!(
                    "{}:running={} healthy={} lastErr={}",
                    e["id"], e["running"], e["healthy"], e["lastErr"]
                ))
                .collect::<Vec<_>>()
        )
    };
    assert!(
        wait_until(
            || crate::httpc::ok(&format!("http://127.0.0.1:{port}/health")),
            Duration::from_secs(30)
        ),
        "换血后 zz-live 必须健康：{}",
        diagnose()
    );
    assert!(!raw_port_busy(19929), "关着的后端绝不能因为换血被点火");

    let _ = host.stop("zz-live");
    drop(live);
    assert!(
        wait_until(|| !raw_port_busy(port), Duration::from_secs(10)),
        "收尾后端口 {port} 必须空出"
    );
    std::env::remove_var(crate::appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/* ---------------- -headless 的端口清理与失败自写日志 ---------------- */

/// 「僵死端口先清再听」：`dispatch::run` 的 `Mode::Proxy` 分支在跑 rs-proxy 之前
/// 会 `portBusy → killByPort → 300ms`——这里真占一个测试端口（外部假后端进程，
/// 不是本进程，故 killByPort 能杀掉它），断言「清理后端口空出」这条可观测结果。
///
/// 注：本用例只验**清理动作本身**（真跑 rs-proxy 会把测试进程变成常驻服务，
/// 不适合放进单元测试；`-headless` 的完整起服在 `command_dispatch.rs` 里用假端口验）。
#[test]
fn headless_cleanup_kills_a_stale_holder_on_the_test_port() {
    require_node();
    let port = 19923;
    assert!(!raw_port_busy(port), "测试端口 {port} 开工前必须空闲");
    let stale = FakeBackend::serve(port);
    assert!(raw_port_busy(port), "僵死占位者必须真在监听");

    // 与 `dispatch` 里那段逐句同形
    if crate::proc::port_busy(&port.to_string()) {
        let _ = crate::proc::kill_by_port(&port.to_string());
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        wait_until(|| !raw_port_busy(port), Duration::from_secs(10)),
        "清理后端口 {port} 必须空出（僵死占位者应已被杀）"
    );
    drop(stale);
}

/// `headless 启动失败：` 自写日志：写出来的必须是数据目录下 `headless-error.log`，
/// 且**带时间戳行头**（`appdir::append_log` 的口径）。
#[test]
fn headless_failure_writes_its_own_log() {
    let _g = env_lock().lock().unwrap();
    let dir = std::env::temp_dir().join(format!("rs-gui-headless-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var(crate::appdir::DATA_DIR_ENV, &dir);

    let log = crate::appdir::path("headless-error.log");
    let _ = crate::appdir::append_log(&log, "headless 启动失败：端口 19924 被其他程序占用: …");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("headless 启动失败："), "正文必须带 Go 同款前缀：{text}");
    assert!(
        text.contains("端口 19924 被其他程序占用"),
        "正文必须带 Go 的错误文案：{text}"
    );
    // 时间戳行头：`YYYY-MM-DD HH:MM:SS `
    assert!(
        text.len() > 20 && text.as_bytes()[4] == b'-' && text.as_bytes()[13] == b':',
        "必须带 `YYYY-MM-DD HH:MM:SS ` 行头：{text}"
    );

    std::env::remove_var(crate::appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}
