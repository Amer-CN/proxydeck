//! 关窗合同（v4.0.1 线上 bug 的回归门禁）：
//!
//! 原生关窗（WM_CLOSE：Alt+F4 / 任务栏关闭 / **一键更新替换后的收尾**）必须让
//! GUI 进程退出。此前 tao 事件循环把 `CloseRequested` 丢在地上 → 窗口关不掉、
//! 一键更新后新旧两个实例并存。
//!
//! 判据是真进程 + 真窗口 + 真 `PostMessage`：给主窗发 WM_CLOSE，进程必须在
//! 数秒内退出。窗口起不来（无桌面会话）时**跳过**而不是假绿。

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rs-gui")
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    #[link(name = "user32")]
    extern "system" {
        pub fn PostMessageW(hwnd: *mut c_void, msg: u32, w: usize, l: isize) -> i32;
        pub fn IsWindow(hwnd: *mut c_void) -> i32;
        pub fn FindWindowExW(
            parent: *mut c_void,
            child: *mut c_void,
            cls: *const u16,
            title: *const u16,
        ) -> *mut c_void;
        pub fn GetWindowThreadProcessId(hwnd: *mut c_void, pid: *mut u32) -> u32;
        pub fn GetWindowTextW(hwnd: *mut c_void, buf: *mut u16, max: i32) -> i32;
        pub fn GetClassNameW(hwnd: *mut c_void, buf: *mut u16, max: i32) -> i32;
        pub fn IsWindowVisible(hwnd: *mut c_void) -> i32;
    }

    pub const WM_CLOSE: u32 = 0x0010;

    fn text_of(h: *mut c_void) -> String {
        let mut buf = [0u16; 512];
        let n = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    fn class_of(h: *mut c_void) -> String {
        let mut buf = [0u16; 256];
        let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    /// 该 pid 的全部顶层窗口（标题, 类名, 句柄, 是否可见）。
    pub fn windows_of(pid: u32) -> Vec<(String, String, *mut c_void, bool)> {
        let mut out = Vec::new();
        unsafe {
            let mut cur: *mut c_void = std::ptr::null_mut();
            loop {
                cur = FindWindowExW(
                    std::ptr::null_mut(),
                    cur,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if cur.is_null() {
                    break;
                }
                let mut wpid: u32 = 0;
                GetWindowThreadProcessId(cur, &mut wpid);
                if wpid == pid && IsWindow(cur) != 0 {
                    out.push((
                        text_of(cur),
                        class_of(cur),
                        cur,
                        IsWindowVisible(cur) != 0,
                    ));
                }
            }
        }
        out
    }

    /// 主窗 = 该 pid 里标题含 "ProxyDeck" 的那个（托盘/WebView 辅助窗标题为空）。
    pub fn main_window_of(pid: u32) -> Option<*mut c_void> {
        let ws = windows_of(pid);
        ws.iter()
            .find(|(t, _, _, vis)| *vis && t.contains("ProxyDeck"))
            .map(|(_, _, h, _)| *h)
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn SendMessageTimeoutW(
            hwnd: *mut c_void,
            msg: u32,
            w: usize,
            l: isize,
            flags: u32,
            timeout: u32,
            out: *mut usize,
        ) -> isize;
    }

    /// 窗口线程是否在泵消息（`SendMessageTimeout(WM_NULL)` 成功即活）。
    pub fn probe_alive(h: *mut c_void) -> String {
        const SMTO_ABORTIFHUNG: u32 = 0x0002;
        const WM_NULL: u32 = 0x0000;
        let mut out: usize = 0;
        let r = unsafe { SendMessageTimeoutW(h, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 2000, &mut out) };
        if r == 0 {
            "无响应（线程卡住或窗口已销毁）".to_string()
        } else {
            "正常".to_string()
        }
    }
}

/// 起一个 GUI（无参）→ 找主窗 → 发 WM_CLOSE → 必须退出。
#[test]
#[cfg(windows)]
fn native_close_exits_the_gui() {
    let dir = std::env::temp_dir().join(format!("rs-gui-closegate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut child: Child = Command::new(bin())
        .env("PROXYDECK_DATA_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("起 GUI 进程");
    let pid = child.id();

    // 等主窗出现（最多 30s；无桌面会话时等不到 → 跳过，不假绿）
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut hwnd = None;
    while Instant::now() < deadline {
        if let Some(h) = win::main_window_of(pid) {
            hwnd = Some(h);
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let Some(h) = hwnd else {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("SKIP: 本会话没有可见主窗（窗口未出现），关窗合同跳过");
        return;
    };

    // 静置：窗口出现 ≠ 事件循环在跑（起窗后还有 600ms 自愈 + 托盘 + 后台线程，
    // 之后才 `event_loop.run`）。投早了消息落在队列里没人取，门禁会假红——
    // 手工验证都等了 8~10s，这里给 5s。
    std::thread::sleep(Duration::from_secs(5));

    // 投递 + 重投：真关窗一次就够；重投只为容忍「首投恰好落在循环启动前」的窗口
    let mut posted = 0;
    for _ in 0..5 {
        unsafe {
            let ok = win::PostMessageW(h, win::WM_CLOSE, 0, 0);
            eprintln!("  PostMessageW(WM_CLOSE) → {ok}");
        }
        posted += 1;
        // 每次投完给它 4 秒；期间退出就直接收工
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let alive = unsafe { win::probe_alive(h) };
    eprintln!("  投递 {posted} 次仍未退出；窗口线程响应：{alive}");

    // 预算 60s：新建 WebView2 profile 的首次关窗实测可达数秒（本机杀软 + profile 冷建），
    // 15s 会假红（v4.0.2 实测两次超时）。
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut exited = false;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
            Err(e) => panic!("wait 失败: {e}"),
        }
    }
    if !exited {
        eprintln!("  失败现场：进程 {} 的窗口：", child.id());
        for (t, c, _, vis) in win::windows_of(child.id()) {
            eprintln!("    - 标题={t:?} 类={c:?} 可见={vis}");
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        panic!("WM_CLOSE 之后 GUI 进程没有退出（事件循环把 CloseRequested 丢了）");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
