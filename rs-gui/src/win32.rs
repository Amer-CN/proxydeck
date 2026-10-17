//! win32.rs —— 宿主外壳的 Win32 底座（对应 Go `app/platform_windows.go` 全部）。
//!
//! 逐条对齐，**不做多余的事**：Go 侧每个函数都靠 `defer recover()` 兜住 panic 并吞掉
//! 失败（调用方拿到的是「尽力而为」的副作用），Rust 侧用 `unsafe { }` 直接调，
//! 不包装错误类型、不重试、不补日志——多写的每一层都是新的行为分歧面。
//!
//! 对应关系（Go 文件行号 → 本文件函数）：
//!   `setDPIAware`（:11）→ [`set_dpi_aware`]
//!   `setWindowIcon`（:63）→ [`set_window_icon`]
//!   `enableDarkTitleBar`（:81）→ [`enable_dark_title_bar`]
//!   `setFrameless`（:94）→ [`set_frameless`]
//!   `windowCmd`（:122）→ [`window_cmd`]
//!   `framelessFullClient`（:178）→ [`frameless_full_client`]
//!   `setClientSize`（:194）→ [`set_client_size`]
//!   `fatalBox`（:204）→ [`fatal_box`]
//!   `findFaxPopup`（:214）→ [`find_fax_popup`]
//!   `foregroundWindow`（:222）→ [`foreground_window`]
//!   `showWindowIfHidden`（:233）→ [`show_window_if_hidden`]

use std::ffi::c_void;

/// 窗口标题（浮窗按标题找；与 Go `runFaxWindow` 的 `w.SetTitle(...)` 逐字同源）。
pub const FAX_TITLE: &str = "注水专线 · WATER PROBE FX-01";

/* ---------------- user32 / kernel32 / dwmapi 声明 ---------------- */

#[link(name = "user32")]
extern "system" {
    fn SetProcessDpiAwarenessContext(value: isize) -> i32;
    fn SetProcessDPIAware() -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn LoadIconW(instance: *mut c_void, name: *const u16) -> *mut c_void;
    fn SendMessageW(hwnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> isize;
    fn PostMessageW(hwnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> i32;
    fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: *mut c_void, index: i32, value: isize) -> isize;
    fn SetWindowPos(
        hwnd: *mut c_void,
        insert_after: *mut c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    fn IsZoomed(hwnd: *mut c_void) -> i32;
    fn IsWindowVisible(hwnd: *mut c_void) -> i32;
    fn FindWindowW(class: *const u16, window: *const u16) -> *mut c_void;
    fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
    fn ReleaseCapture() -> i32;
    fn MessageBoxW(hwnd: *mut c_void, text: *const u16, caption: *const u16, flags: u32) -> i32;
    fn CallWindowProcW(
        prev: isize,
        hwnd: *mut c_void,
        msg: u32,
        w_param: usize,
        l_param: isize,
    ) -> isize;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: *mut c_void, attr: u32, value: *const c_void, size: u32) -> i32;
}

/// UTF-16 加 NUL 结尾（`UTF16PtrFromString` 的对应物）。
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 把 `usize` 句柄当指针（句柄都是非 0 的整数值；0 由调用方拦掉）。
fn hwnd_of(hwnd: usize) -> *mut c_void {
    hwnd as *mut c_void
}

/* ---------------- DPI 感知（Go platform_windows.go:11） ---------------- */

/// `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4)`；
/// 返回 0 时（旧系统没有这个入口）回退 `SetProcessDPIAware()`——与 Go 逐行同源，
/// **必须在建窗之前调用**（Go `main()` 的 `setDPIAware()` 也是 flag.Parse 之前）。
pub fn set_dpi_aware() {
    // Go 的 ^uintptr(3) = -4；这里按 isize 给同一个值
    let ok = unsafe { SetProcessDpiAwarenessContext(-4) };
    if ok != 0 {
        return;
    }
    unsafe {
        SetProcessDPIAware();
    }
}

/// 主屏分辨率（Go `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`）。
pub fn screen_size() -> (i32, i32) {
    unsafe { (GetSystemMetrics(0), GetSystemMetrics(1)) }
}

/* ---------------- 窗口图标（Go platform_windows.go:63） ---------------- */

/// 把 exe 自带的图标设置到窗口（`WM_SETICON`，ICON_BIG + ICON_SMALL）。
///
/// Go 侧 `LoadIconW(hInst, 1)`：图标资源 ID = 1（rsrc 生成）。
/// P7d-2 起 Rust 侧 exe **也有**图标资源了（`rs-gui/winres.rc` 的 `1 ICON "icon.ico"`，
/// 那份 .ico 是 `app/icon.ico` 的逐字节副本），ID 与 Go 逐字对齐 ⇒ 这里能拿到真句柄；
/// 资源缺失时 `LoadIconW` 返回 0，本函数原样返回（与 Go 在同样缺资源时的行为一致）。
pub fn set_window_icon(hwnd: usize) {
    const WM_SETICON: u32 = 0x0080;
    const ICON_BIG: usize = 1;
    const ICON_SMALL: usize = 0;
    let h = hwnd_of(hwnd);
    unsafe {
        let h_inst = GetModuleHandleW(std::ptr::null());
        // MAKEINTRESOURCE(1) = 低位整数当指针
        let h_icon = LoadIconW(h_inst, 1usize as *const u16);
        if h_icon.is_null() {
            return;
        }
        SendMessageW(h, WM_SETICON, ICON_BIG, h_icon as isize);
        SendMessageW(h, WM_SETICON, ICON_SMALL, h_icon as isize);
    }
}

/// 原生标题栏切深色（Win10 1809+ / Win11）。Go 侧定义在 `platform_windows.go:81`，
/// 但**没有任何调用点**（主窗已无边框）；照抄保留，避免「缺一个函数」。
pub fn enable_dark_title_bar(hwnd: usize) {
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    let dark: i32 = 1;
    unsafe {
        DwmSetWindowAttribute(
            hwnd_of(hwnd),
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const i32 as *const c_void,
            4,
        );
    }
}

/* ---------------- 无边框（Go platform_windows.go:94） ---------------- */

/// `GWL_STYLE` = -16、`GWL_WNDPROC` = -4（Go 用 `^uintptr(15)` / `^uintptr(3)`）。
pub const GWL_STYLE: i32 = -16;
/// `GWL_WNDPROC`。
pub const GWL_WNDPROC: i32 = -4;

/// 去掉原生标题栏与可调边框（`WS_CAPTION|WS_THICKFRAME`），并让变更立即生效，
/// 再通过 DWM 显式开启 Win11 圆角（`DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND`）。
/// 保留 `WS_SYSMENU` 与 MIN/MAXBOX（Alt+F4 / 任务栏右键 / 系统级最大化动画照旧）。
pub fn set_frameless(hwnd: usize) {
    const WS_CAPTION: isize = 0x00C0_0000;
    const WS_THICKFRAME: isize = 0x0004_0000;
    // SWP_NOSIZE|SWP_NOMOVE|SWP_NOZORDER|SWP_FRAMECHANGED
    const SWP_FLAGS: u32 = 0x1 | 0x2 | 0x4 | 0x20;
    let h = hwnd_of(hwnd);
    unsafe {
        let style = GetWindowLongPtrW(h, GWL_STYLE);
        SetWindowLongPtrW(h, GWL_STYLE, style & !(WS_CAPTION | WS_THICKFRAME));
        SetWindowPos(h, std::ptr::null_mut(), 0, 0, 0, 0, SWP_FLAGS);
        let pref: i32 = 2; // DWMWCP_ROUND
        DwmSetWindowAttribute(h, 33, &pref as *const i32 as *const c_void, 4);
    }
}

/* ---------------- 自绘窗口控制（Go platform_windows.go:122） ---------------- */

/// `windowCmd` 的**命令 → 动作**映射（纯函数，可测）：`None` = 不认这个命令。
///
/// 抽出来的唯一目的是让「命令字认不认」这件事能被门禁驱动——Go 的 `switch` 无 default，
/// 不认识的命令**什么都不做**，这条语义在真窗口上没法自动化（`move` 会进系统拖拽模态循环、
/// `close` 会关窗，拿真窗口当夹具会动到用户桌面）。
pub fn window_cmd_action(cmd: &str) -> Option<&'static str> {
    match cmd {
        "move" => Some("move"),
        "min" => Some("min"),
        "max" => Some("max"),
        "close" => Some("close"),
        _ => None,
    }
}

/// `move` / `min` / `max` / `close` 四条，逐条对齐 Go `windowCmd`：
///   * `move`：先 `ReleaseCapture()` 再 `SendMessage(WM_NCLBUTTONDOWN, HTCAPTION)` 进系统
///     移动模态循环（WebView2 子窗持有鼠标捕获时漏掉 ReleaseCapture 就拖不动）
///   * `min`：`PostMessage(WM_SYSCOMMAND, SC_MINIMIZE)`
///   * `max`：`IsZoomed` 判定后 `ShowWindow(SW_RESTORE/SW_MAXIMIZE)`
///   * `close`：`PostMessage(WM_CLOSE)`
///   * 其它值：**什么都不做**（Go 的 switch 无 default）
pub fn window_cmd(hwnd: usize, cmd: &str) {
    if hwnd == 0 {
        return;
    }
    let h = hwnd_of(hwnd);
    match window_cmd_action(cmd) {
        Some("move") => unsafe {
            const WM_NCLBUTTONDOWN: u32 = 0x00A1;
            const HTCAPTION: usize = 2;
            ReleaseCapture();
            SendMessageW(h, WM_NCLBUTTONDOWN, HTCAPTION, 0);
        },
        Some("min") => unsafe {
            const WM_SYSCOMMAND: u32 = 0x0112;
            const SC_MINIMIZE: usize = 0xF020;
            PostMessageW(h, WM_SYSCOMMAND, SC_MINIMIZE, 0);
        },
        Some("max") => unsafe {
            const SW_RESTORE: i32 = 9;
            const SW_MAXIMIZE: i32 = 3;
            if IsZoomed(h) != 0 {
                ShowWindow(h, SW_RESTORE);
            } else {
                ShowWindow(h, SW_MAXIMIZE);
            }
        },
        Some("close") => unsafe {
            const WM_CLOSE: u32 = 0x0010;
            PostMessageW(h, WM_CLOSE, 0, 0);
        },
        _ => {}
    }
}

/* ---------------- 浮窗客户区铺满（Go platform_windows.go:152-201） ---------------- */

static FAX_PREV_WNDPROC: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);
static FAX_WNDPROC_CB: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(0);

/// 子类化用的 WndProc 回调：只拦 `WM_NCCALCSIZE(0x0083)` 且 `wParam != 0` 时返回 0
/// （整个窗口 = 客户区），其余消息原样 `CallWindowProcW` 回原链。
///
/// 与 Go 的 `faxFullClientWndProc` 同一形态。Go 靠**包级变量**防 GC 回收回调指针，
/// Rust 侧用 `static` 的 `AtomicIsize` 存同一份地址——语义等价（进程内一次性）。
unsafe extern "system" fn fax_full_client_wndproc(
    hwnd: *mut c_void,
    msg: u32,
    w_param: usize,
    l_param: isize,
) -> isize {
    const WM_NCCALCSIZE: u32 = 0x0083;
    if msg == WM_NCCALCSIZE && w_param != 0 {
        return 0;
    }
    let prev = FAX_PREV_WNDPROC.load(std::sync::atomic::Ordering::SeqCst);
    CallWindowProcW(prev, hwnd, msg, w_param, l_param)
}

/// 回调地址（惰性取一次，同 Go 的 `syscall.NewCallback`）。
fn fax_wndproc_callback() -> isize {
    let cur = FAX_WNDPROC_CB.load(std::sync::atomic::Ordering::SeqCst);
    if cur != 0 {
        return cur;
    }
    let p = fax_full_client_wndproc as *const () as usize as isize;
    FAX_WNDPROC_CB.store(p, std::sync::atomic::Ordering::SeqCst);
    p
}

/// 子类化窗口 WndProc，吃掉 `setFrameless` 后 DWM 仍保留的 ~8px 不可见边框
/// （窗口物理 596×823 vs 内容 580×784 的根因）。**重复调用安全**（已子类化即返回，
/// 防回调链自环）；浮窗进程短命，不做卸载——与 Go 逐字同源。
pub fn frameless_full_client(hwnd: usize) {
    if hwnd == 0 {
        return;
    }
    let cb = fax_wndproc_callback();
    unsafe {
        let h = hwnd_of(hwnd);
        let cur = GetWindowLongPtrW(h, GWL_WNDPROC);
        if cur == 0 || cur == cb {
            return;
        }
        FAX_PREV_WNDPROC.store(cur, std::sync::atomic::Ordering::SeqCst);
        SetWindowLongPtrW(h, GWL_WNDPROC, cb);
    }
}

/// 子类化生效后把窗口外廓直接钉到目标尺寸（webview 的 `SetSize` 会按标题栏 metrics
/// 外扩，请求 580×784 实得 596×823）。须在 [`frameless_full_client`] 之后调用。
pub fn set_client_size(hwnd: usize, w: i32, h: i32) {
    const SWP_NOMOVE: u32 = 0x2;
    const SWP_NOZORDER: u32 = 0x4;
    const SWP_NOACTIVATE: u32 = 0x10;
    unsafe {
        SetWindowPos(
            hwnd_of(hwnd),
            std::ptr::null_mut(),
            0,
            0,
            w,
            h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/* ---------------- 浮窗查找 / 置前 / 可见性自愈 ---------------- */

/// 按窗口标题**精确匹配**找注水专线浮窗（Go `findFaxPopup`）；没有则返回 0。
pub fn find_fax_popup() -> usize {
    let title = wide(FAX_TITLE);
    unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) as usize }
}

/// 还原并置前（最小化的浮窗被再次召唤时先 `SW_RESTORE` 再 `SetForegroundWindow`）。
pub fn foreground_window(hwnd: usize) {
    const SW_RESTORE: i32 = 9;
    unsafe {
        let h = hwnd_of(hwnd);
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
    }
}

/// 自愈窗口可见性：**不可见则带出**（`SW_SHOW` + 置前），可见则一个字不动
/// （最小化状态不惊扰）。一键更新重启时新进程起窗、旧窗仍持前台，前台锁定规则
/// 会让新窗停在不可见——这是第 47 轮那条 600ms 自愈的落点。
pub fn show_window_if_hidden(hwnd: usize) {
    const SW_SHOW: i32 = 5;
    unsafe {
        let h = hwnd_of(hwnd);
        if IsWindowVisible(h) == 0 {
            ShowWindow(h, SW_SHOW);
            SetForegroundWindow(h);
        }
    }
}

/* ---------------- 致命错误弹窗（Go platform_windows.go:204） ---------------- */

/// 启动期致命错误弹原生框（WebView2 运行时缺失等）。
pub fn fatal_box(title: &str, msg: &str) {
    const MB_OK_ICONWARNING: u32 = 0x30;
    let t = wide(msg);
    let c = wide(title);
    unsafe {
        MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK_ICONWARNING);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `wide()` 就是 `UTF16PtrFromString`：UTF-16 编解码 + 一个 NUL 结尾。
    #[test]
    fn wide_appends_nul_and_encodes_utf16() {
        let w = wide("注水A");
        assert_eq!(w.last(), Some(&0));
        assert_eq!(&w[..w.len() - 1], &[0x6CE8, 0x6C34, 0x0041]);
        assert_eq!(wide("").len(), 1, "空串也只有一个 NUL");
    }

    /// 浮窗标题必须与 Go `runFaxWindow` 的 `w.SetTitle(...)` 逐字相同（找窗按键名精确匹配）。
    #[test]
    fn fax_title_matches_go_window_title() {
        assert_eq!(FAX_TITLE, "注水专线 · WATER PROBE FX-01");
    }

    /// 找不到浮窗时返回 0（本机没有该窗口即无浮窗）；顺带证明这条查询真的打到了 user32
    /// （假标题必定找不到）。
    #[test]
    fn find_fax_popup_returns_zero_for_absent_title() {
        assert_eq!(find_fax_popup(), 0, "测试机不该有注水浮窗；有则说明采集前必须先收掉");
    }

    /// 句柄为 0 时窗口命令一条都不做（真 windowCmd 的 move 也不碰——否则会
    /// `SendMessage(0, ...)`）。这条锁的是「先判 0」的顺序。
    #[test]
    fn window_cmd_on_zero_handle_is_a_noop() {
        for cmd in ["move", "min", "max", "close", "nope"] {
            window_cmd(0, cmd);
        }
    }

    /// 命令字 → 动作的映射逐条对齐 Go `windowCmd` 的 `switch`（四个 case + 无 default）。
    /// 门禁靠这条锁「命令字认不认」——真窗口上没法自动化（`move` 会进拖拽模态循环）。
    #[test]
    fn window_cmd_action_matches_go_switch() {
        assert_eq!(window_cmd_action("move"), Some("move"));
        assert_eq!(window_cmd_action("min"), Some("min"));
        assert_eq!(window_cmd_action("max"), Some("max"));
        assert_eq!(window_cmd_action("close"), Some("close"));
        // Go 的 switch 无 default：不认识的一律什么都不做
        for nope in ["", "min ", "MIN", "Close", "nope", "0"] {
            assert_eq!(window_cmd_action(nope), None, "{nope:?} 不该被认作窗口命令");
        }
    }

    /// DPI 感知必须在建窗之前就能调（幂等；重复调不炸）。
    #[test]
    fn set_dpi_aware_is_idempotent() {
        set_dpi_aware();
        set_dpi_aware();
    }

    /// `show_window_if_hidden` 对无效句柄不得把进程搞崩（Go 靠 recover，Rust 靠 Win32 自身的失败返回）。
    #[test]
    fn visibility_helpers_tolerate_invalid_handle() {
        show_window_if_hidden(1);
        foreground_window(1);
        set_window_icon(1);
        set_frameless(1);
        frameless_full_client(1);
        set_client_size(1, 10, 10);
        enable_dark_title_bar(1);
    }
}
