//! rollover.rs —— 更新后后端换血（对应 Go `app/rollover.go` 全部）。
//!
//! 背景（与 Go 注释同源）：更新只重启 GUI，后端是**独立常驻进程**，还跑在腾位前的旧镜像上
//! （改名后叫 `ProxyDeck.exe.old-<ts>`）。旧文件被咬住就删不掉，「零残留」无从谈起。
//! 本模块只在新 GUI 启动时后台跑一次：
//!   * 拿到**换血标记**（更新器刚换完包）→ 全量滚（文件改名后运行中进程上报的还是原名，
//!     靠名字永远认不出刚换过包的后端，只能靠标记）
//!   * 没有标记 → 沿用**镜像名判定**（手动腾位/旧残留场景）
//!   * 全部换完再扫一次残留（`update::cleanup_old_exe()`）
//!
//! 判定缺一不可：①已知插件端口（`pluginDefs` 名单，55990 主核心、测试端口一律不动）；
//! ②占位进程镜像命中 `*.old*`（大小写不敏感；现包/外部路径/**读不出的未知一律不动**）。
//! 逐个串行，一路失败只记日志不连累其他路；全程异步，不阻塞界面。

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::plugins::{self, PluginDef, PluginHost};
use crate::proc;
use crate::update;

/// 杀后等端口空出的宽限（Go `rollKillGrace`：生产 5s，单测可调小）。
pub const ROLL_KILL_GRACE: Duration = Duration::from_secs(5);

/// 换血时等后端健康的超时（Go `rollStaleBackends` 传的 90s）。
pub const ROLL_HEALTH_TIMEOUT: Duration = Duration::from_secs(90);

/// health 轮询间隔（Go `rollOne` 里传的 1s）。
pub const ROLL_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// 一台要换血的后端：哪个插件、占位进程是谁、跑在哪个旧镜像上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleTarget {
    /// 插件定义（可测核心 `roll_one` 只需要 id / name / port，故这里给完整定义）
    pub def_id: String,
    pub def_name: String,
    pub def_port: u16,
    pub pid: u32,
    pub exe: String,
}

impl StaleTarget {
    fn of(def: &PluginDef, pid: u32, exe: String) -> Self {
        Self {
            def_id: def.id.clone(),
            def_name: def.name.clone(),
            def_port: def.port,
            pid,
            exe,
        }
    }

    /// 还原成 `PluginDef`（`roll_one` 只需要它去查端口与名字）。
    fn def(&self) -> PluginDef {
        PluginDef::new(
            &self.def_id,
            &self.def_name,
            "",
            "",
            &[],
            self.def_port,
            "/health",
            &self.def_id,
        )
    }
}

/* ---------------- 判定层（纯函数，Go rollover.go:26-120） ---------------- */

/// 判定进程镜像是否为已被 supersede 的旧包。
///
/// 只认 `ProxyDeck.exe.old*` 这个茎（**大小写不敏感**）：更新腾位（唯一名）和手动腾位的
/// 旧文件都长这样。`ProxyDeck_2.exe` 这类改名副本、下载中的 `ProxyDeck.exe.new`、
/// 毫不相干的 `foo.old.exe`、空串一律**不算**——换血只动"确定是旧包"的进程。
pub fn is_stale_image(exe_path: &str) -> bool {
    let base = Path::new(exe_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    base.starts_with("proxydeck.exe.old")
}

/// 解析 `netstat -ano` 输出里 `port -> pids`（仅 LISTENING 的 TCP），与 Go
/// `parseNetstatListeners` 同一套字段语义（`f[1]` 地址后缀、`f[3]` 状态、`f[4]` pid）。
/// 返回 `BTreeMap` 只为输出稳定（查表语义与 Go 的 map 相同）。
pub fn parse_netstat_listeners(out: &str) -> BTreeMap<String, Vec<u32>> {
    let mut holders: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for line in out.split('\n') {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 || !f[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        // f[1] 形如 127.0.0.1:8788 或 [::]:8788：取最后一个冒号后的端口
        let Some(idx) = f[1].rfind(':') else {
            continue;
        };
        let port = &f[1][idx + 1..];
        let Ok(pid) = f[4].parse::<u32>() else {
            continue;
        };
        if pid == 0 {
            continue;
        }
        let entry = holders.entry(port.to_string()).or_default();
        if !entry.contains(&pid) {
            entry.push(pid);
        }
    }
    holders
}

/// 快照一次 TCP 监听表（单次 netstat，供换血判定一次用完）。
pub fn snapshot_listeners() -> Result<BTreeMap<String, Vec<u32>>, String> {
    let out = proc::hidden_cmd("netstat", &["-ano", "-p", "tcp"])
        .output()
        .map_err(|e| format!("netstat 执行失败: {e}"))?;
    Ok(parse_netstat_listeners(&String::from_utf8_lossy(&out.stdout)))
}

/// 决定换血名单：已知插件端口上、跑在旧包镜像上的占位进程。
///
/// `image_of` 由调用方提供（生产走 [`image_path_of_pid`]，单测走假表）；**读不出镜像的一律
/// 按"未知=不动"**；`current_exe` 自身、`self_pid` 自身永远排除；非插件端口的占位进程
/// 看都不看（55990 主核心、测试端口、外部程序全部免疫）。
pub fn classify_holders(
    defs: &[PluginDef],
    snap: &BTreeMap<String, Vec<u32>>,
    image_of: &dyn Fn(u32) -> Option<String>,
    current_exe: &str,
    self_pid: u32,
) -> Vec<StaleTarget> {
    let mut out = Vec::new();
    for d in defs {
        let port = d.port.to_string();
        let Some(pids) = snap.get(&port) else {
            continue;
        };
        for &pid in pids {
            if pid == self_pid {
                continue;
            }
            let Some(exe) = image_of(pid) else {
                continue;
            };
            if exe.is_empty() || exe.eq_ignore_ascii_case(current_exe) {
                continue;
            }
            if !is_stale_image(&exe) {
                continue;
            }
            out.push(StaleTarget::of(d, pid, exe));
            break; // 同一端口只换一次（pluginStop 按端口杀，pluginStart 重拉一个）
        }
    }
    out
}

/// 进程镜像全路径（Go `imagePathOfPID`）。打不开进程、读不出路径一律 `None`，
/// 调用方必须按 "未知=不动" 处理（SYSTEM/受保护进程等本来就读不出来）。
pub fn image_path_of_pid(pid: u32) -> Option<String> {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const MAX_PATH_W: usize = 4096;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
        fn QueryFullProcessImageNameW(
            handle: *mut std::ffi::c_void,
            flags: u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }

    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = [0u16; MAX_PATH_W];
    let mut size = MAX_PATH_W as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    let s: String = String::from_utf16_lossy(&buf[..size as usize]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/* ---------------- 轮询（Go rollover.go:159-184） ---------------- */

/// 等端口空出来（只看 LISTENING，TIME_WAIT 残留不算）。
pub fn wait_port_free(port: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !proc::port_busy(port) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// 轮询直到 `healthy()` 为真（或超时）。
pub fn wait_backend_healthy(
    healthy: &dyn Fn() -> bool,
    timeout: Duration,
    interval: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if healthy() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}

/* ---------------- 换血本体（Go rollover.go:287-311） ---------------- */

/// 给一台后端换血：停旧 → 确认端口空出 → 用新包拉起 → 等 health 绿，返回 `None` 表示成功。
///
/// `stop`/`start`/`healthy` 全部由调用方注入：生产传真实实现，单测传 mock
/// （**绝不碰真实端口**）。任一步失败只返回一行日志文案，由调用方记日志后继续下一台
/// ——失败隔离，不中断整轮。
///
/// 其中杀后复验端口是双保险：`pluginStop` 自身已如实报错（杀不掉回 `Err`），
/// 这里再验一次，防其他 stop 路径的假成功导致「拉杆弹回 running」。
pub fn roll_one(
    t: &StaleTarget,
    stop: &dyn Fn(&str) -> Result<(), String>,
    start: &dyn Fn(&str) -> Result<(), String>,
    healthy: &dyn Fn(&PluginDef) -> bool,
    timeout: Duration,
) -> Option<String> {
    let d = t.def();
    if let Err(e) = stop(&t.def_id) {
        return Some(format!(
            "[rollover] {} 停止旧实例失败，跳过换血：{e}",
            t.def_name
        ));
    }
    if !wait_port_free(&t.def_port.to_string(), ROLL_KILL_GRACE) {
        return Some(format!(
            "[rollover] {} 旧进程杀不掉（端口 {} 仍被占，可能权限不足），跳过换血",
            t.def_name, t.def_port
        ));
    }
    if let Err(e) = start(&t.def_id) {
        return Some(format!(
            "[rollover] {} 用新包拉起失败，请手动拉杆：{e}",
            t.def_name
        ));
    }
    if !wait_backend_healthy(&|| healthy(&d), timeout, ROLL_HEALTH_INTERVAL) {
        return Some(format!(
            "[rollover] {} 拉起后 {}s 仍未健康，请手动检查",
            t.def_name,
            timeout.as_secs()
        ));
    }
    None
}

/// `rollAllWith` 是 `rollAllBackends` 的可测核心：`snap` 快照决定谁有活人；
/// stop/start/healthy 注入（生产真实实现，单测 mock，**绝不碰真实端口**）。
/// 返回（换血成功名单，跳过名单）——名字按 `pluginDefs` 顺序。
pub fn roll_all_with(
    defs: &[PluginDef],
    snap: &BTreeMap<String, Vec<u32>>,
    stop: &dyn Fn(&str) -> Result<(), String>,
    start: &dyn Fn(&str) -> Result<(), String>,
    healthy: &dyn Fn(&PluginDef) -> bool,
    timeout: Duration,
) -> (Vec<String>, Vec<String>) {
    let mut rolled = Vec::new();
    let mut skipped = Vec::new();
    for d in defs {
        let has_holder = snap
            .get(&d.port.to_string())
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if !has_holder {
            skipped.push(d.name.clone());
            continue;
        }
        let t = StaleTarget::of(d, 0, String::new());
        if let Some(msg) = roll_one(&t, stop, start, healthy, timeout) {
            eprintln!("{msg}");
            continue;
        }
        rolled.push(d.name.clone());
    }
    (rolled, skipped)
}

/* ---------------- 接进启动流程（Go rollover.go:196-263） ---------------- */

/// 更新后零残留的关键一步：把还跑在旧包（`*.old*`）上的插件后端逐个换血。
/// **只在新 GUI 启动时后台跑一次**（`dispatch` 的 GUI 分支）。
pub fn roll_stale_backends(host: &PluginHost) {
    // 1) 换血标记（更新器刚换完包）：全量滚，不猜镜像名
    if let (to, true) = update::consume_update_marker() {
        eprintln!("[rollover] 检测到更新换血标记（→ {to}），开始全量换血");
        roll_all_backends(host);
        update::cleanup_old_exe();
        return;
    }
    // 2) 无标记：沿用镜像名判定（手动腾位/旧残留场景）
    let snap = match snapshot_listeners() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[rollover] 监听表快照失败，本次跳过换血检查：{e}");
            return;
        }
    };
    if snap.is_empty() {
        return;
    }
    let self_exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let targets = classify_holders(
        host.defs(),
        &snap,
        &|pid| image_path_of_pid(pid),
        &self_exe,
        std::process::id(),
    );
    if targets.is_empty() {
        return;
    }
    for t in &targets {
        eprintln!(
            "[rollover] {}（端口 {}）还跑在旧包上（pid={}），开始换血",
            t.def_name, t.def_port, t.pid
        );
        let msg = roll_one(
            t,
            &|id| host.stop(id),
            &|id| host.start(id),
            &|d| crate::httpc::ok(&plugins::plugin_health_url(d)),
            ROLL_HEALTH_TIMEOUT,
        );
        match msg {
            Some(m) => eprintln!("{m}"),
            None => eprintln!("[rollover] {} 换血完成（已跑在新包上）", t.def_name),
        }
    }
    update::cleanup_old_exe();
}

/// 把"正跑着"的插件后端逐个换血（更新刚完成时用）。
/// **没活人的端口保持关闭**——更新不替你点火（重启 GUI 本来也不会自动点火，换血不能借机
/// 改变开关状态）。快照失败宁可整轮跳过，也不盲滚。
pub fn roll_all_backends(host: &PluginHost) {
    let snap = match snapshot_listeners() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[rollover] 监听表快照失败，跳过本轮换血（不盲滚，避免点着已关的后端）：{e}");
            return;
        }
    };
    let (rolled, skipped) = roll_all_with(
        host.defs(),
        &snap,
        &|id| host.stop(id),
        &|id| host.start(id),
        &|d| crate::httpc::ok(&plugins::plugin_health_url(d)),
        ROLL_HEALTH_TIMEOUT,
    );
    for n in skipped {
        eprintln!("[rollover] {n} 未在运行，保持关闭（更新不替你点火）");
    }
    for n in rolled {
        eprintln!("[rollover] {n} 换血完成（已跑在新包上）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 Go `TestIsStaleImage` 的 8 条真值表逐个同值（真雷三条 + 不能碰的五条）。
    #[test]
    fn is_stale_image_truth_table_matches_go_test() {
        let cases: [(&str, bool); 8] = [
            // 真雷：用户现场原样（群友那台的腾位残留名）
            (r"F:\AIXM\proxydeck\ProxyDeck.exe.old-1791936628190504400", true),
            (r"D:\桌面\ProxyDeck.exe.old", true),
            (r"C:\x\PROXYDECK.EXE.OLD", true),
            // 不能碰的
            (r"F:\AIXM\proxydeck\ProxyDeck.exe", false),
            (r"D:\桌面\ProxyDeck_2.exe", false),
            (r"F:\AIXM\proxydeck\ProxyDeck.exe.new", false),
            (r"C:\x\foo.old.exe", false),
            ("", false),
        ];
        for (exe, want) in cases {
            assert_eq!(is_stale_image(exe), want, "isStaleImage({exe:?})");
        }
    }

    /// 与 Go `TestParseNetstatListeners` 的 `fakeNetstat` **逐字节同一份输入**、
    /// 同一份期望（TIME_WAIT 与 UDP 行必须被忽略，pid 非数字必须跳过，重复行去重）。
    #[test]
    fn parse_netstat_listeners_matches_go_test_fixture() {
        const FAKE: &str = "Active Connections\n\n\
              Proto  Local Address          Foreign Address        State           PID\n\
              \x20 TCP    127.0.0.1:8788         0.0.0.0:0              LISTENING       1111\n\
              \x20 TCP    127.0.0.1:8788         0.0.0.0:0              LISTENING       1111\n\
              \x20 TCP    0.0.0.0:8787           0.0.0.0:0              LISTENING       2222\n\
              \x20 TCP    [::]:8789              0.0.0.0:0              LISTENING       3333\n\
              \x20 TCP    127.0.0.1:55990        0.0.0.0:0              LISTENING       4444\n\
              \x20 TCP    127.0.0.1:8788         0.0.0.0:0              TIME_WAIT       5555\n\
              \x20 TCP    127.0.0.1:9999         0.0.0.0:0              LISTENING       abc\n\
              \x20 UDP    127.0.0.1:8788         *:*                                    6666\n";
        let got = parse_netstat_listeners(FAKE);
        assert_eq!(got.len(), 4, "端口数：{got:?}");
        assert_eq!(got.get("8788"), Some(&vec![1111]));
        assert_eq!(got.get("8787"), Some(&vec![2222]));
        assert_eq!(got.get("8789"), Some(&vec![3333]));
        assert_eq!(got.get("55990"), Some(&vec![4444]));
        assert!(!got.contains_key("9999"), "pid 非数字必须跳过");
    }

    /// 与 Go `TestClassifyHolders` 同一张快照/镜像表，逐条同判：
    /// 旧包换、现包跳过、非插件端口看都不看、读不出不动、selfPid 排除。
    #[test]
    fn classify_holders_matches_go_test_table() {
        let defs = vec![
            PluginDef::new("tuanjie", "团结", "", "", &[], 8788, "/health", "tuanjie"),
            PluginDef::new("codebuddy", "WB", "", "", &[], 8787, "/health", "codebuddy"),
        ];
        let mut snap: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        snap.insert("8788".into(), vec![1111]); // 跑在旧包 → 换
        snap.insert("8787".into(), vec![2222, 3333]); // 2222 是现包 → 跳过；3333 旧包 → 换
        snap.insert("55990".into(), vec![4444]); // 非插件端口 → 看都不看
        snap.insert("18788".into(), vec![5555]); // 测试端口 → 同上
        let images = |pid: u32| -> Option<String> {
            match pid {
                1111 => Some(r"F:\AIXM\proxydeck\ProxyDeck.exe.old-1".into()),
                2222 => Some(r"F:\AIXM\proxydeck\ProxyDeck.exe".into()), // == currentExe
                3333 => Some(r"F:\AIXM\proxydeck\ProxyDeck.exe.old-2".into()),
                4444 => Some(r"F:\AIXM\proxydeck\ProxyDeck.exe.old-3".into()),
                5555 => Some(r"F:\AIXM\proxydeck\ProxyDeck.exe.old-4".into()),
                _ => None,
            }
        };
        let current = r"F:\AIXM\proxydeck\ProxyDeck.exe";
        let got = classify_holders(&defs, &snap, &images, current, 9999);
        assert_eq!(got.len(), 2, "换血名单应 2 条：{got:?}");
        assert_eq!(got[0].def_id, "tuanjie");
        assert_eq!(got[0].pid, 1111);
        assert_eq!(got[1].def_id, "codebuddy");
        assert_eq!(got[1].pid, 3333, "跳过现包 2222，取 3333");

        // 未知镜像（读不出）→ 不动
        let unknown = |_pid: u32| -> Option<String> { None };
        assert!(classify_holders(&defs, &snap, &unknown, current, 9999).is_empty());

        // **同一端口多 pid**：先命中的是本进程（现包）→ 必须继续往后看，取到旧包那个。
        // （Go 的 `break` 落在"命中旧包"之后；把 currentExe 那条过滤去掉就再也抓不到这一条。）
        let mut multi: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        multi.insert("8788".into(), vec![7777, 1111]);
        let images2 = |pid: u32| -> Option<String> {
            match pid {
                7777 => Some(current.to_string()),
                1111 => Some(r"F:\AIXM\proxydeck\ProxyDeck.exe.old-1".into()),
                _ => None,
            }
        };
        let got2 = classify_holders(&defs, &multi, &images2, current, 9999);
        assert_eq!(got2.len(), 1, "同端口多 pid 时必须跳过现包、取旧包：{got2:?}");
        assert_eq!(got2[0].pid, 1111);

        // **同一个端口全是现包**：一条都不该换
        let mut allcur: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        allcur.insert("8788".into(), vec![7777, 8888]);
        let both_cur = |_pid: u32| -> Option<String> { Some(current.to_string()) };
        assert!(
            classify_holders(&defs, &allcur, &both_cur, current, 9999).is_empty(),
            "端口上全是现包时不该动任何进程"
        );

        // **currentExe 本身就长着 `ProxyDeck.exe.old*`**（用户手动跑了那个腾位残留文件）：
        // 镜像名判定会把它当"旧包"，**只有那句 currentExe 相等判断能挡住它**。
        // 去掉那句上限判断 → 本用例立刻红。
        let stale_looking_current = r"F:\AIXM\proxydeck\ProxyDeck.exe.old-9";
        let mut cur_snap: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        cur_snap.insert("8788".into(), vec![1111]);
        let cur_img = |_pid: u32| -> Option<String> { Some(stale_looking_current.to_string()) };
        assert!(
            classify_holders(&defs, &cur_snap, &cur_img, stale_looking_current, 9999).is_empty(),
            "镜像名与 currentExe 相同时必须不动（哪怕名字长得像旧包）"
        );

        // currentExe 比较必须大小写不敏感（Windows 路径；Go 的 EqualFold 同判）
        let upper_cur = |_pid: u32| -> Option<String> { Some(stale_looking_current.to_uppercase()) };
        assert!(
            classify_holders(&defs, &cur_snap, &upper_cur, stale_looking_current, 9999).is_empty(),
            "currentExe 比较必须大小写不敏感"
        );

        // 自己 → 不动（哪怕监听着插件端口）
        let mut self_snap: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        self_snap.insert("8788".into(), vec![9999]);
        let any_stale = |_pid: u32| -> Option<String> { Some(r"F:\other\ProxyDeck.exe.old-9".into()) };
        assert!(
            classify_holders(&defs, &self_snap, &any_stale, current, 9999).is_empty(),
            "selfPid 必须排除"
        );
    }

    /// 与 Go `TestRollOneOrder` 同判：调用顺序必须是 stop→start→healthy，且轮询能等到绿。
    #[test]
    fn roll_one_call_order_matches_go() {
        use std::sync::Mutex as M;
        let calls: M<Vec<String>> = M::new(Vec::new());
        let n: M<u32> = M::new(0);
        // Port 0 永不被占：rollOne 自带杀后端口复验，真端口会受环境影响（Go 单测同款做法）
        let t = StaleTarget {
            def_id: "tj".into(),
            def_name: "团结".into(),
            def_port: 0,
            pid: 1111,
            exe: String::new(),
        };
        let msg = roll_one(
            &t,
            &|id| {
                calls.lock().unwrap().push(format!("stop:{id}"));
                Ok(())
            },
            &|id| {
                calls.lock().unwrap().push(format!("start:{id}"));
                Ok(())
            },
            &|d| {
                calls.lock().unwrap().push(format!("healthy:{}", d.id));
                let mut g = n.lock().unwrap();
                *g += 1;
                *g >= 2
            },
            Duration::from_secs(2),
        );
        assert_eq!(msg, None, "应成功");
        let seq = calls.lock().unwrap().join(" ");
        assert!(
            seq.starts_with("stop:tj start:tj healthy:tj"),
            "调用顺序应为 stop→start→healthy，实际 {seq:?}"
        );
    }

    /// 与 Go `TestRollOneStopFailsSkipsRest` / `StartFailsSkipsHealth` 同判：
    /// 前一步失败后，后一步**一律不得调用**。
    #[test]
    fn roll_one_short_circuits_on_stop_and_start_failure() {
        use std::sync::Mutex as M;
        let t = StaleTarget {
            def_id: "tj".into(),
            def_name: "团结".into(),
            def_port: 0,
            pid: 1111,
            exe: String::new(),
        };
        // stop 失败 → 只剩 stop
        let calls: M<Vec<String>> = M::new(Vec::new());
        let msg = roll_one(
            &t,
            &|_| {
                calls.lock().unwrap().push("stop".into());
                Err("杀不掉".into())
            },
            &|_| {
                calls.lock().unwrap().push("start".into());
                Ok(())
            },
            &|_| {
                calls.lock().unwrap().push("healthy".into());
                true
            },
            Duration::from_secs(1),
        );
        assert!(msg.is_some(), "必须返回失败文案");
        assert_eq!(calls.lock().unwrap().as_slice(), ["stop"]);
        // start 失败 → stop + start，health 不轮询
        let calls: M<Vec<String>> = M::new(Vec::new());
        let msg = roll_one(
            &t,
            &|_| {
                calls.lock().unwrap().push("stop".into());
                Ok(())
            },
            &|_| {
                calls.lock().unwrap().push("start".into());
                Err("拉不起".into())
            },
            &|_| {
                calls.lock().unwrap().push("healthy".into());
                true
            },
            Duration::from_secs(1),
        );
        assert!(msg.is_some());
        assert_eq!(calls.lock().unwrap().len(), 2, "health 不得被轮询");
    }

    /// 与 Go `TestRollOneUnhealthyReports` 同判：health 一直不绿 → 失败文案，且**确实轮询过**。
    #[test]
    fn roll_one_unhealthy_reports_after_polling() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let t = StaleTarget {
            def_id: "tj".into(),
            def_name: "团结".into(),
            def_port: 0,
            pid: 1111,
            exe: String::new(),
        };
        let msg = roll_one(
            &t,
            &|_| Ok(()),
            &|_| Ok(()),
            &|_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                false
            },
            Duration::from_millis(100),
        );
        assert!(msg.is_some(), "health 超时必须返回失败文案");
        assert!(calls.load(std::sync::atomic::Ordering::SeqCst) > 0, "必须真轮询过");
    }

    /// 与 Go `TestRollAllSkipsIdle` 同判：只换有活人的端口，没活人的保持关闭且 start 不得被调。
    #[test]
    fn roll_all_with_skips_idle_ports_like_go() {
        let defs = vec![
            PluginDef::new("on", "开着", "", "", &[], 0, "/health", "on"), // Port 0 永不被占
            PluginDef::new("off", "关着", "", "", &[], 8786, "/health", "off"),
        ];
        let mut snap: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        snap.insert("0".into(), vec![4242]);
        let stopped: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let started: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let (rolled, skipped) = roll_all_with(
            &defs,
            &snap,
            &|id| {
                stopped.lock().unwrap().push(id.to_string());
                Ok(())
            },
            &|id| {
                started.lock().unwrap().push(id.to_string());
                Ok(())
            },
            &|_| true,
            Duration::from_secs(1),
        );
        assert_eq!(rolled, vec!["开着".to_string()]);
        assert_eq!(skipped, vec!["关着".to_string()]);
        assert_eq!(stopped.lock().unwrap().as_slice(), ["on"]);
        assert!(
            !started.lock().unwrap().iter().any(|s| s == "off"),
            "关着的后端 start 一律不得调用"
        );
    }

    /// `waitPortFree` 的真行为（Go `TestWaitPortFree` 同判）：**端口被占时在超时内必须返回
    /// false**；释放后必须返回 true。
    ///
    /// 为什么单列：`roll_one` 里那个「杀后复验端口」的分支只在「stop 返回 Ok 但端口仍被占」
    /// 时才会走到（真实场景 = 对方以管理员身份运行、`taskkill` 杀不掉），而**门禁里构造不出
    /// 管理员进程**——`stop` 那条路会先报错，测不到复验本身。故把复验依赖的两个原语
    /// （`port_busy` / `wait_port_free`）在真端口上单独锁死。
    #[test]
    fn wait_port_free_is_false_while_the_port_is_really_held() {
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ln.local_addr().unwrap().port().to_string();
        assert!(crate::proc::port_busy(&port), "绑定后端口必须是占用的");
        assert!(
            !wait_port_free(&port, Duration::from_millis(400)),
            "端口被占时 wait_port_free 在超时内必须返回 false"
        );
        drop(ln);
        assert!(
            wait_port_free(&port, Duration::from_secs(5)),
            "端口释放后 wait_port_free 必须返回 true"
        );
    }

    /// **一个真端口**：stop 报错（本进程占着端口，`killByPort` 排除自己）→
    /// 必须如实报「杀不掉」且**绝不 start**（Go `TestRollOneKillUnverified` 同款）。
    #[test]
    fn roll_one_reports_kill_failure_on_a_real_still_busy_port() {
        // 要一个当前空闲的端口（立刻释放；只用它做一次真绑定）
        let port = {
            let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = ln.local_addr().unwrap().port();
            drop(ln);
            p
        };
        // 本进程占住它：`kill_by_port` 会把本进程排除掉 → 杀后端口仍被占
        let _hold = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();

        let calls: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let started = std::sync::atomic::AtomicBool::new(false);
        let t = StaleTarget {
            def_id: "tj".into(),
            def_name: "团结".into(),
            def_port: port,
            pid: 1111,
            exe: String::new(),
        };
        let msg = roll_one(
            &t,
            &|_| {
                // 与 `PluginHost::stop` 同构：按端口杀 → **再验端口**
                let killed = crate::proc::kill_by_port(&port.to_string());
                calls.lock().unwrap().push(format!("stop:{:?}", killed.is_ok()));
                if crate::proc::port_busy(&port.to_string()) {
                    Err(format!("旧进程杀不掉（端口 {port} 仍被占）"))
                } else {
                    Ok(())
                }
            },
            &|_| {
                started.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
            &|_| false,
            Duration::from_secs(1),
        );
        let m = msg.expect("端口仍被占时必须如实报失败");
        assert!(m.contains("杀不掉"), "文案必须点明杀不掉：{m}");
        assert!(
            !started.load(std::sync::atomic::Ordering::SeqCst),
            "杀不掉时绝不能再 start"
        );
        // 本进程是持端口者：kill_by_port 报「未找到占用该端口的进程」（false），复验必失败。
        // 这条序列把「本次失败来自 stop 自己」钉住——**别把它当成复验生效的证据**
        // （复验那条分支需要「stop 返回 Ok 但端口仍被占」，门禁里构造不出管理员进程，
        //   故它由上面的 `wait_port_free_is_false_while_the_port_is_really_held` 单独锁）。
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["stop:false".to_string()],
            "本进程是持端口者：kill_by_port 必然报「未找到占用该端口的进程」（false）"
        );
        assert!(
            crate::proc::port_busy(&port.to_string()),
            "前置条件：这一趟结束时端口仍被本进程占着（否则本用例测不到那条分支）"
        );
    }

    /// 与 Go `TestWaitBackendHealthy` 同判：能轮询到绿；恒假则超时返回 false。
    #[test]
    fn wait_backend_healthy_polls_then_times_out() {
        let n = std::sync::atomic::AtomicU32::new(0);
        assert!(wait_backend_healthy(
            &|| n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2,
            Duration::from_secs(2),
            Duration::from_millis(5)
        ));
        assert!(n.load(std::sync::atomic::Ordering::SeqCst) >= 3);
        assert!(!wait_backend_healthy(
            &|| false,
            Duration::from_millis(50),
            Duration::from_millis(10)
        ));
    }

    /// 与 Go `TestImagePathOfPIDSelf` 同判：读自己进程的镜像必须成功且是个 `.exe`（hermetic）。
    #[test]
    fn image_path_of_self_pid_reads_the_exe() {
        let exe = image_path_of_pid(std::process::id()).expect("读自己镜像失败");
        assert!(!exe.is_empty());
        assert!(
            exe.to_lowercase().ends_with(".exe"),
            "镜像路径应以 .exe 结尾，实得 {exe}"
        );
    }

    /// 读不出镜像的 pid（不存在的 pid）必须返回 None（"未知=不动"的前提）。
    #[test]
    fn image_path_of_bogus_pid_is_none() {
        assert_eq!(image_path_of_pid(0xFFFF_FFFE), None);
    }
}
