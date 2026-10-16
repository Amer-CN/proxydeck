//! host.rs —— 「宿主动作」缝：窗口命令与托盘（对应 Go `app/bridge.go` 里直接调用的
//! `w.Window()` / `windowCmd` / `trayHide` / `trayQuit`）。
//!
//! 为什么做成 trait：桥接层必须能在**没有窗口**的情况下被门禁逐条驱动（Go 采集器同样
//! 靠 `w.Window() == nil` 的假 WebView 跑通 26 个命令）。真实实现（Win32 的
//! `PostMessageW` / 托盘线程）属 P7c，本阶段提供：
//!   - `NoWindowHost`：无窗口宿主——所有窗口/托盘动作按 Go 的「句柄不可用」如实报错
//!   - `WindowHost`：真实窗口宿主（P7c 接上 tao/wry 的窗口句柄后由 webview 模块装配）

/// 宿主动作（窗口命令 + 托盘）。
pub trait Host: Send + Sync {
    /// 当前窗口句柄；`None` = 拿不到句柄（Go 里 `w.Window() == nil`）。
    fn window_handle(&self) -> Option<usize>;

    /// 执行窗口命令：`move` / `min` / `max` / `close`（Go `windowCmd`，P7c 落地细节）。
    fn window_cmd(&self, hwnd: usize, cmd: &str);

    /// 退到托盘（Go `trayHide`：托盘起不来就如实报错，绝不藏窗）。
    fn tray_hide(&self, hwnd: usize) -> Result<(), String>;

    /// 退出界面（Go `trayQuit`：先摘托盘图标再走关窗流程）。
    fn tray_quit(&self, hwnd: usize);
}

/// 无窗口宿主：句柄恒不可用。测试与非 GUI 子模式（`--plugin-*` / `-headless`）用它。
pub struct NoWindowHost;

impl Host for NoWindowHost {
    fn window_handle(&self) -> Option<usize> {
        None
    }
    fn window_cmd(&self, _hwnd: usize, _cmd: &str) {}
    fn tray_hide(&self, _hwnd: usize) -> Result<(), String> {
        // 真实文案由托盘层给（Go `trayHide` 先 trayStart，失败回自己的错；
        // 消息窗没起来时是「托盘消息窗未就绪」）。P7a 没接托盘，如实回这一条。
        Err("托盘消息窗未就绪".into())
    }
    fn tray_quit(&self, _hwnd: usize) {}
}

/// 有窗口的宿主：句柄由外壳层提供（webview 模块装配；P7c 补齐托盘与无边框细节）。
pub struct WindowHost {
    hwnd: usize,
    /// 关窗动作：PostMessage WM_CLOSE（Go `trayCloseMain` / `windowCmd(hwnd,"close")` 同款）。
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
        // P7c：把 tao 窗口的 minimize/maximize/drag 与 PostMessage 接上来。
        // 本阶段只保证「关窗」这条主干路径可用（一键更新与红灯都走它）。
        if cmd == "close" {
            (self.close)();
        }
        let _ = hwnd;
    }

    fn tray_hide(&self, _hwnd: usize) -> Result<(), String> {
        Err("托盘消息窗未就绪".into())
    }

    fn tray_quit(&self, _hwnd: usize) {
        (self.close)();
    }
}
