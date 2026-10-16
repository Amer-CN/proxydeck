//! host.rs —— 「宿主动作」缝：窗口命令与托盘（对应 Go `app/bridge.go` 里直接调用的
//! `w.Window()` / `windowCmd` / `trayHide` / `trayQuit`）。
//!
//! 为什么做成 trait：桥接层必须能在**没有窗口**的情况下被门禁逐条驱动（Go 采集器同样
//! 靠 `w.Window() == nil` 的假 WebView 跑通 30 个命令）。真实实现（Win32 的
//! `PostMessageW` / 托盘线程）在 P7c 接上：
//!   - [`NoWindowHost`]：无窗口宿主——所有窗口/托盘动作按 Go 的「句柄不可用」如实报错
//!     （子模式 `--plugin-*` / `-headless` 用它）
//!   - [`WindowHost`]：真实窗口宿主（wry/tao 的窗口句柄 → [`crate::win32`] +
//!     [`crate::tray`]）

/// 宿主动作（窗口命令 + 托盘）。
pub trait Host: Send + Sync {
    /// 当前窗口句柄；`None` = 拿不到句柄（Go 里 `w.Window() == nil`）。
    fn window_handle(&self) -> Option<usize>;

    /// 执行窗口命令：`move` / `min` / `max` / `close`（Go `windowCmd`）。
    fn window_cmd(&self, hwnd: usize, cmd: &str);

    /// 退到托盘（Go `trayHide`：托盘起不来就如实报错，绝不藏窗）。
    fn tray_hide(&self, hwnd: usize) -> Result<(), String>;

    /// 退出界面（Go `trayQuit`：先摘托盘图标再走关窗流程）。
    fn tray_quit(&self, hwnd: usize);
}

/// 无窗口宿主：句柄恒不可用。测试与非 GUI 子模式（`--plugin-*` / `-headless`）用它。
///
/// `tray_hide` 的错误文案与真实托盘层**同源**：Go `trayHide` 第一句就是
/// `if mainHwnd == 0 { return errors.New("窗口句柄不可用") }`，而调用方
/// （`bridge.go` 的 `applyCloseMode`）在此之前已经拦了一次——两条路给出同一句话。
pub struct NoWindowHost;

impl Host for NoWindowHost {
    fn window_handle(&self) -> Option<usize> {
        None
    }
    fn window_cmd(&self, _hwnd: usize, _cmd: &str) {}
    fn tray_hide(&self, _hwnd: usize) -> Result<(), String> {
        Err("窗口句柄不可用".into())
    }
    fn tray_quit(&self, _hwnd: usize) {}
}

/// 有窗口的宿主：句柄由外壳层提供（webview / faxwindow 装配）。
pub struct WindowHost {
    hwnd: usize,
    /// 关窗动作：`PostMessage WM_CLOSE`（Go `trayCloseMain` / `windowCmd(hwnd,"close")` 同款）。
    /// `tray_quit` 的兜底路径用它——Go `trayQuit` 在托盘没起过时也是走
    /// `trayCloseMain(mainHwnd)`（同一个 `WM_CLOSE`）。
    close: Box<dyn Fn() + Send + Sync>,
}

impl WindowHost {
    pub fn new(hwnd: usize, close: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            hwnd,
            close: Box::new(close),
        }
    }
}

impl Host for WindowHost {
    fn window_handle(&self) -> Option<usize> {
        if self.hwnd == 0 {
            None
        } else {
            Some(self.hwnd)
        }
    }

    fn window_cmd(&self, hwnd: usize, cmd: &str) {
        crate::win32::window_cmd(hwnd, cmd);
    }

    fn tray_hide(&self, hwnd: usize) -> Result<(), String> {
        crate::tray::tray_hide(hwnd)
    }

    fn tray_quit(&self, hwnd: usize) {
        // 托盘线程起过 → 交给它（先摘图标再 WM_CLOSE）；没起过 → 走这里的关窗回调
        // （与 Go `trayQuit` 的 `if started && hwnd != 0 { … }` 兜底 `trayCloseMain` 同构）。
        if crate::tray::tray_started() {
            crate::tray::tray_quit(hwnd);
            return;
        }
        (self.close)();
    }
}

/// 起窗时挂托盘（Go `main()` 里 `trayStartup(uintptr(hwnd))` 的位置：`bindAll` 之前）。
/// 托盘起不来只写日志——降级为「没有托盘图标」，界面照常可用。
pub fn startup_tray(hwnd: Option<usize>) {
    match hwnd {
        Some(h) => crate::tray::tray_startup(h),
        None => eprintln!("[tray] 主窗句柄不可用，托盘图标未挂载"),
    }
}

/// 一键更新替换完成后的关窗（`update.rs` 的那一跳）：拿得到句柄就发 `WM_CLOSE`
/// 走正常关闭流程，拿不到就直接退进程（Go `bridge.go:1336-1343` 同款）。
pub fn close_after_update(host: &dyn Host) {
    match host.window_handle() {
        Some(h) => host.window_cmd(h, "close"),
        None => std::process::exit(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无窗口宿主：句柄不可用 + 托盘如实报错（与 Go `trayHide` 的第一句话同字）。
    #[test]
    fn no_window_host_reports_like_go() {
        let h = NoWindowHost;
        assert_eq!(h.window_handle(), None);
        assert_eq!(h.tray_hide(0).unwrap_err(), "窗口句柄不可用");
        // 窗口命令是空操作，不得 panic
        h.window_cmd(0, "close");
        h.tray_quit(0);
    }

    /// 有窗口宿主：句柄原样给出；`tray_quit` 走关窗回调（Go `trayQuit` 的兜底路径）。
    #[test]
    fn window_host_exposes_handle_and_routes_close() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = std::sync::Arc::clone(&called);
        let h = WindowHost::new(0x1234, move || {
            c.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(h.window_handle(), Some(0x1234));
        h.tray_quit(0x1234);
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "tray_quit 必须走关窗回调"
        );
        // 句柄 0 → 拿不到（与 Go `w.Window() == nil` 同判）
        let h0 = WindowHost::new(0, || {});
        assert_eq!(h0.window_handle(), None);
    }
}
