//! tray.rs —— 关窗退到托盘（对应 Go `app/tray_windows.go` 全部）。
//!
//! 手写 Win32、零新依赖、不引 systray 类库——与 Go 侧同一形态：
//!   * 一条**独立的 OS 线程**持有隐藏消息窗与消息循环（Go 靠 `runtime.LockOSThread()`；
//!     `GetMessage` 取的是本线程队列，窗口与循环必须同线程）
//!   * `Shell_NotifyIconW` 挂/摘图标；`TaskbarCreated` 广播后自己挂回来（Explorer 重启）
//!   * 单击图标切换隐藏/还原；右键菜单「显示界面 / 退出界面」
//!   * 退出路径先 `NIM_DELETE` 再走原有关窗流程（`PostMessage WM_CLOSE`），不留孤儿图标
//!   * **图标常驻**：GUI 进程起来就挂，窗口可见时也在，进程退出前不摘
//!
//! 对外只有三条入口（与 Go 同名同语义）：[`tray_startup`] / [`tray_hide`] / [`tray_quit`]。
//!
//! `NOTIFYICONDATAW` / `WNDCLASSEXW` / `MSG` 三个结构的字段顺序与对齐**必须与 SDK 一致**
//! （`Shell_NotifyIconW` 按 `cbSize` 认结构版本）——`#[repr(C)]` 在 x86_64-pc-windows-msvc
//! 下的对齐规则与 MSVC 相同，与 Go amd64 亦同。

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::win32;

/* ---------------- 常量（逐字对齐 Go tray_windows.go:23-52） ---------------- */

const TRAY_ICON_ID: u32 = 1;
const WM_APP: u32 = 0x8000;
const TRAY_MSG_CALLBACK: u32 = WM_APP + 1000; // 托盘鼠标回调（NOTIFYICONDATAW.uCallbackMessage）
const TRAY_MSG_INIT: u32 = WM_APP + 1001; // 自投递：消息循环跑起来后再挂图标
const TRAY_MSG_ADD_ICON: u32 = WM_APP + 1002; // 幂等补挂（藏窗前保险）
const TRAY_MSG_QUIT: u32 = WM_APP + 1003; // 摘图标再关主窗
const TRAY_MENU_SHOW: usize = 1;
const TRAY_MENU_QUIT: usize = 2;

const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_NULL: u32 = 0x0000;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONUP: u32 = 0x0205;

const SW_HIDE: i32 = 0;
const SW_RESTORE: i32 = 9;

const NIM_ADD: usize = 0;
const NIM_DELETE: usize = 2;
const NIF_MESSAGE: u32 = 0x1;
const NIF_ICON: u32 = 0x2;
const NIF_TIP: u32 = 0x4;

const IMAGE_ICON: u32 = 1;
const MF_STRING: usize = 0;
const TPM_RIGHTBUTTON: usize = 0x2;
const TPM_RETURNCMD: usize = 0x100;

/* ---------------- FFI ---------------- */

#[link(name = "user32")]
extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: *mut c_void,
        menu: *mut c_void,
        instance: *mut c_void,
        param: *mut c_void,
    ) -> *mut c_void;
    fn RegisterWindowMessageW(name: *const u16) -> u32;
    fn GetMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn DestroyWindow(hwnd: *mut c_void) -> i32;
    fn DefWindowProcW(hwnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> isize;
    fn PostMessageW(hwnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> i32;
    fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    fn IsWindowVisible(hwnd: *mut c_void) -> i32;
    fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
    fn LoadImageW(
        instance: *mut c_void,
        name: *const u16,
        kind: u32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> *mut c_void;
    fn LoadIconW(instance: *mut c_void, name: *const u16) -> *mut c_void;
    fn CreatePopupMenu() -> *mut c_void;
    fn DestroyMenu(menu: *mut c_void) -> i32;
    fn AppendMenuW(menu: *mut c_void, flags: usize, id: usize, item: *const u16) -> i32;
    fn GetCursorPos(pt: *mut Point) -> i32;
    fn TrackPopupMenu(
        menu: *mut c_void,
        flags: usize,
        x: i32,
        y: i32,
        reserved: i32,
        hwnd: *mut c_void,
        rect: *const c_void,
    ) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(message: usize, data: *const NotifyIconDataW) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> *mut c_void;
}

/* ---------------- 结构（字段顺序/对齐与 SDK 一致） ---------------- */

/// `NOTIFYICONDATAW`（Win7+ 全尺寸 976 字节）。
#[repr(C)]
struct NotifyIconDataW {
    cb_size: u32,
    hwnd: *mut c_void,
    u_id: u32,
    u_flags: u32,
    u_callback_message: u32,
    h_icon: *mut c_void,
    sz_tip: [u16; 128],
    dw_state: u32,
    dw_state_mask: u32,
    sz_info: [u16; 256],
    u_version: u32,
    sz_info_title: [u16; 64],
    dw_info_flags: u32,
    guid_item: [u8; 16],
    h_balloon_icon: *mut c_void,
}

/// `WNDCLASSEXW`（`RegisterClassExW` 用）。
#[repr(C)]
struct WndClassExW {
    cb_size: u32,
    style: u32,
    lpfn_wnd_proc: isize,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: *mut c_void,
    h_icon: *mut c_void,
    h_cursor: *mut c_void,
    hbr_background: *mut c_void,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
    h_icon_sm: *mut c_void,
}

/// `POINT`。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
}

/// `MSG`（`GetMessageW` / `DispatchMessageW` 用）。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Msg {
    hwnd: *mut c_void,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: Point,
}

/* ---------------- 包级状态（Go 的 var 块） ---------------- */

struct TrayState {
    started: bool,
    err: Option<String>,
    /// 启动完成信号：**只发不收**（`recv_timeout` 自己排干队列），发过即置 None。
    /// Go 侧是一个 `chan struct{}` 的 close 语义——这里用 mpsc + 收侧排干达到同一效果。
    ready: Option<std::sync::mpsc::Sender<()>>,
    hwnd: usize,      // 隐藏消息窗
    main_hwnd: usize, // WebView 主窗
    icon_visible: bool,
    taskbar_msg: u32, // RegisterWindowMessageW("TaskbarCreated")
}

fn state() -> &'static Mutex<TrayState> {
    static S: OnceLock<Mutex<TrayState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(TrayState {
            started: false,
            err: None,
            ready: None,
            hwnd: 0,
            main_hwnd: 0,
            icon_visible: false,
            taskbar_msg: 0,
        })
    })
}

/// WNDPROC 回调地址（`syscall.NewCallback` 的对应物：进程内取一次、长期有效）。
static TRAY_WNDPROC_CB: AtomicIsize = AtomicIsize::new(0);
/// 托盘图标句柄（进程内复用，不 `DestroyIcon`——托盘要一直用着它）。
static TRAY_ICON_H: AtomicIsize = AtomicIsize::new(0);
/// 任务栏重建消息号（`trayWndProc` 每条消息都要读，用原子免锁；Go 用 mutex）。
static TASKBAR_MSG: AtomicU32 = AtomicU32::new(0);

/* ---------------- 对外入口（bridge.rs 调用） ---------------- */

/// 退到托盘：确保托盘就绪（图标已挂）后隐藏主窗。
/// **托盘起不来就不藏窗**——藏了没图标等于把用户关在门外，此时把错误交回调用方。
pub fn tray_hide(main_hwnd: usize) -> Result<(), String> {
    if main_hwnd == 0 {
        return Err("窗口句柄不可用".into());
    }
    tray_start(main_hwnd)?;
    let hwnd = state().lock().unwrap().hwnd;
    if hwnd == 0 {
        return Err("托盘消息窗未就绪".into());
    }
    // 图标已由常驻启动挂好，这里再自投递一次幂等补挂（Explorer 重建后漏挂的保险；
    // 挂图标一律在托盘线程做）
    unsafe { PostMessageW(hwnd as *mut c_void, TRAY_MSG_ADD_ICON, 0, 0) };
    unsafe { ShowWindow(main_hwnd as *mut c_void, SW_HIDE) };
    Ok(())
}

/// 退出界面：先摘托盘图标再走原有关窗流程（`WM_CLOSE` → 消息循环干净返回 → 正常退出）。
/// 托盘线程没起过就直接关窗（那时压根没有图标）。
pub fn tray_quit(main_hwnd: usize) {
    let (hwnd, started) = {
        let s = state().lock().unwrap();
        (s.hwnd, s.started)
    };
    if started && hwnd != 0 {
        // 交给托盘线程：Shell_NotifyIcon 与它建的窗同线程，摘图标最稳
        if unsafe { PostMessageW(hwnd as *mut c_void, TRAY_MSG_QUIT, 0, 0) } != 0 {
            return;
        }
    }
    tray_close_main(main_hwnd);
}

/// 常拉托盘线程（GUI 启动时调用一次）：图标从进程起来就挂着。
/// 启动失败只写日志——降级为「没有托盘图标」，界面照常可用；用户真要选「退到托盘」时
/// [`tray_hide`] 会拿到同一个错误如实回前端，不会出现藏了窗却没图标的情况。
pub fn tray_startup(main_hwnd: usize) {
    if main_hwnd == 0 {
        eprintln!("[tray] 主窗句柄不可用，托盘图标未挂载");
        return;
    }
    if let Err(e) = tray_start(main_hwnd) {
        eprintln!("[tray] 托盘启动失败，本次运行无托盘图标：{e}");
    }
}

/// 发 `WM_CLOSE` 走原有关窗流程（`ccWindowCmd("close")` 同款语义）。
pub fn tray_close_main(main_hwnd: usize) {
    if main_hwnd != 0 {
        unsafe { PostMessageW(main_hwnd as *mut c_void, WM_CLOSE, 0, 0) };
    }
}

/// 托盘线程是否已经起来（测试用；生产不经过它）。
pub fn tray_started() -> bool {
    state().lock().unwrap().started
}

/// 当前是否挂着图标（测试用）。
pub fn tray_icon_visible() -> bool {
    state().lock().unwrap().icon_visible
}

/// 上一次启动的失败原因（测试用）。
pub fn tray_last_error() -> Option<String> {
    state().lock().unwrap().err.clone()
}

/* ---------------- 常驻启动 ---------------- */

/// 拉起托盘线程（幂等）：隐藏消息窗与消息循环跑在同一条锁定线程上，
/// 图标挂好（或明确失败）后才返回。
pub fn tray_start(main_hwnd: usize) -> Result<(), String> {
    let rx: Option<std::sync::mpsc::Receiver<()>> = {
        let mut s = state().lock().unwrap();
        s.main_hwnd = main_hwnd;
        if s.started {
            None
        } else {
            s.started = true;
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            s.ready = Some(tx);
            Some(rx)
        }
    };

    if let Some(rx) = rx {
        std::thread::spawn(move || {
            // 窗口与消息循环必须同线程（GetMessage 取的是本线程队列）——Go 用
            // runtime.LockOSThread()，Rust 的独立线程天然满足。
            if let Err(e) = tray_create_window() {
                tray_finish_start(Some(e));
                return;
            }
            let hwnd = state().lock().unwrap().hwnd;
            // 先往队列塞一条自投递：wndProc 里挂图标时消息循环已在跑（Shell_NotifyIcon
            // 若向本窗发消息，同线程可重入处理，不会僵在没人泵消息的线程上）
            if unsafe { PostMessageW(hwnd as *mut c_void, TRAY_MSG_INIT, 0, 0) } == 0 {
                tray_finish_start(Some("托盘初始化消息投递失败: PostMessageW 返回 0".into()));
                return;
            }
            tray_loop();
        });
        // 等本条线程收敛（Go 的 `<-ready`）。超时上限只是保险：正常路径不会走到。
        let _ = rx.recv_timeout(std::time::Duration::from_secs(30));
    } else {
        // 已有启动过程/已收敛：Go 的 `<-ch` 语义是「等第一条路径收敛」。
        // ready 非 nil（启动中）→ 短等；已取走 → 直接读结果。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let done = state().lock().unwrap().ready.is_none();
            if done || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let s = state().lock().unwrap();
    match s.err.clone() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// 交回启动结果（只生效一次；失败路径也要放行调用方，不能让它干等）。
fn tray_finish_start(err: Option<String>) {
    let sender = {
        let mut s = state().lock().unwrap();
        s.err = err;
        s.ready.take() // 幂等：第二次起就不再发（Go 的 close 也只能 close 一次）
    };
    if let Some(tx) = sender {
        let _ = tx.send(());
    }
}

fn tray_create_window() -> Result<(), String> {
    let class_name = win32::wide("ProxyDeckTrayWnd");
    let wc = WndClassExW {
        cb_size: std::mem::size_of::<WndClassExW>() as u32,
        style: 0,
        lpfn_wnd_proc: tray_wndproc_callback(),
        cb_cls_extra: 0,
        cb_wnd_extra: 0,
        h_instance: unsafe { GetModuleHandleW(std::ptr::null()) },
        h_icon: std::ptr::null_mut(),
        h_cursor: std::ptr::null_mut(),
        hbr_background: std::ptr::null_mut(),
        lpsz_menu_name: std::ptr::null(),
        lpsz_class_name: class_name.as_ptr(),
        h_icon_sm: std::ptr::null_mut(),
    };
    let atom = unsafe { RegisterClassExW(&wc) };
    if atom == 0 {
        return Err("RegisterClassExW 失败: 返回 0".into());
    }
    let title = win32::wide(crate::APP_TITLE);
    // WS_OVERLAPPED(0) + 全零尺寸：只收消息、永不可见
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            wc.h_instance,
            std::ptr::null_mut(),
        )
    };
    if hwnd.is_null() {
        return Err("CreateWindowExW 失败: 返回 0".into());
    }
    // Explorer 重启会广播 TaskbarCreated：图标得自己挂回去（消息号运行时注册，非固定值）
    let name = win32::wide("TaskbarCreated");
    let msg = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    {
        let mut s = state().lock().unwrap();
        s.hwnd = hwnd as usize;
        s.taskbar_msg = msg;
    }
    TASKBAR_MSG.store(msg, Ordering::SeqCst);
    Ok(())
}

fn tray_loop() {
    loop {
        let mut msg = Msg::default();
        // 0 = WM_QUIT，-1 = 出错 → 两种都退出循环（Go 的 `int32(r) <= 0`）
        if unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } <= 0 {
            return;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/* ---------------- 消息处理 ---------------- */

/// WNDPROC 回调地址（惰性取一次）。
fn tray_wndproc_callback() -> isize {
    let cur = TRAY_WNDPROC_CB.load(Ordering::SeqCst);
    if cur != 0 {
        return cur;
    }
    let p = tray_wndproc as *const () as usize as isize;
    TRAY_WNDPROC_CB.store(p, Ordering::SeqCst);
    p
}

/// 隐藏消息窗的窗口过程：托盘回调、挂图标、任务栏重建、退出四件事，
/// 其余原样回系统。回调内不做重活（在托盘线程的消息循环里跑）。
unsafe extern "system" fn tray_wndproc(
    hwnd: *mut c_void,
    msg: u32,
    w_param: usize,
    l_param: isize,
) -> isize {
    let taskbar_msg = TASKBAR_MSG.load(Ordering::SeqCst);
    if msg == TRAY_MSG_CALLBACK {
        // 未调 NIM_SETVERSION（版本 0 语义）：lParam 就是鼠标消息
        match (l_param as u32) & 0xFFFF {
            WM_LBUTTONUP => tray_toggle(),
            WM_RBUTTONUP => tray_menu(),
            _ => {}
        }
        return 0;
    }
    if msg == TRAY_MSG_INIT {
        let err = tray_add_icon().err();
        if let Some(ref e) = err {
            eprintln!("[tray] 托盘图标挂载失败：{e}");
            DestroyWindow(hwnd); // → WM_DESTROY → PostQuitMessage
        }
        tray_finish_start(err);
        return 0;
    }
    if msg == TRAY_MSG_ADD_ICON {
        if let Err(e) = tray_add_icon() {
            eprintln!("[tray] 托盘图标重挂失败：{e}");
        }
        return 0;
    }
    if msg == TRAY_MSG_QUIT {
        tray_delete_icon();
        let main = state().lock().unwrap().main_hwnd;
        tray_close_main(main);
        return 0;
    }
    if taskbar_msg != 0 && msg == taskbar_msg {
        // 任务栏/Explorer 重建：本来挂着图标才需要挂回来（窗口藏着的场景）
        let need = {
            let mut s = state().lock().unwrap();
            let need = s.icon_visible;
            s.icon_visible = false; // 壳里已没了，让 tray_add_icon 重新挂
            need
        };
        if need {
            if let Err(e) = tray_add_icon() {
                eprintln!("[tray] 任务栏重建后挂图标失败：{e}");
            }
        }
        return 0;
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return 0;
    }
    DefWindowProcW(hwnd, msg, w_param, l_param)
}

/// 挂托盘图标（幂等；必须在托盘线程调用——消息循环已在跑）。
fn tray_add_icon() -> Result<(), String> {
    let hwnd = {
        let s = state().lock().unwrap();
        if s.icon_visible {
            return Ok(());
        }
        s.hwnd
    };
    if hwnd == 0 {
        return Err("托盘消息窗未就绪".into());
    }
    let mut nid = NotifyIconDataW {
        cb_size: std::mem::size_of::<NotifyIconDataW>() as u32,
        hwnd: hwnd as *mut c_void,
        u_id: TRAY_ICON_ID,
        u_flags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        u_callback_message: TRAY_MSG_CALLBACK,
        h_icon: tray_icon_handle(),
        sz_tip: [0; 128],
        dw_state: 0,
        dw_state_mask: 0,
        sz_info: [0; 256],
        u_version: 0,
        sz_info_title: [0; 64],
        dw_info_flags: 0,
        guid_item: [0; 16],
        h_balloon_icon: std::ptr::null_mut(),
    };
    let tip = win32::wide(crate::APP_TITLE);
    let n = tip.len().min(128);
    nid.sz_tip[..n].copy_from_slice(&tip[..n]);
    let r = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    if r == 0 {
        return Err("Shell_NotifyIcon(NIM_ADD) 失败: 返回 0".into());
    }
    state().lock().unwrap().icon_visible = true;
    Ok(())
}

/// 摘托盘图标（幂等；托盘线程调用）。
fn tray_delete_icon() {
    let (hwnd, visible) = {
        let s = state().lock().unwrap();
        (s.hwnd, s.icon_visible)
    };
    if !visible || hwnd == 0 {
        return;
    }
    let nid = NotifyIconDataW {
        cb_size: std::mem::size_of::<NotifyIconDataW>() as u32,
        hwnd: hwnd as *mut c_void,
        u_id: TRAY_ICON_ID,
        u_flags: 0,
        u_callback_message: 0,
        h_icon: std::ptr::null_mut(),
        sz_tip: [0; 128],
        dw_state: 0,
        dw_state_mask: 0,
        sz_info: [0; 256],
        u_version: 0,
        sz_info_title: [0; 64],
        dw_info_flags: 0,
        guid_item: [0; 16],
        h_balloon_icon: std::ptr::null_mut(),
    };
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    state().lock().unwrap().icon_visible = false;
}

/// 还原主窗：`SW_RESTORE` 带出并置前。图标不摘——常驻图标在进程退出前一直在。
fn tray_restore() {
    let hwnd = state().lock().unwrap().main_hwnd;
    if hwnd == 0 {
        return;
    }
    unsafe {
        ShowWindow(hwnd as *mut c_void, SW_RESTORE);
        SetForegroundWindow(hwnd as *mut c_void);
    }
}

/// 单击托盘图标：窗口可见则藏进托盘，已藏则还原带出。
/// `IsWindowVisible` 是实时状态，不用自己记窗口可见性（还原后用户手动最小化等情形也不会错判）。
fn tray_toggle() {
    let hwnd = state().lock().unwrap().main_hwnd;
    if hwnd == 0 {
        return;
    }
    if unsafe { IsWindowVisible(hwnd as *mut c_void) } != 0 {
        unsafe { ShowWindow(hwnd as *mut c_void, SW_HIDE) };
        return;
    }
    tray_restore();
}

/// 右键菜单：显示界面 / 退出界面。`TrackPopupMenu` 前必须置前本窗，否则点菜单外部
/// 菜单不消失（Win32 老规矩）；用 `TPM_RETURNCMD` 自取命令号，不依赖 `WM_COMMAND` 回路。
fn tray_menu() {
    unsafe {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let show = win32::wide("显示界面");
        let quit = win32::wide("退出界面");
        AppendMenuW(menu, MF_STRING, TRAY_MENU_SHOW, show.as_ptr());
        AppendMenuW(menu, MF_STRING, TRAY_MENU_QUIT, quit.as_ptr());

        let mut pt = Point::default();
        GetCursorPos(&mut pt);
        let (hwnd, main) = {
            let s = state().lock().unwrap();
            (s.hwnd, s.main_hwnd)
        };
        SetForegroundWindow(hwnd as *mut c_void);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            0,
            hwnd as *mut c_void,
            std::ptr::null(),
        );
        PostMessageW(hwnd as *mut c_void, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match cmd as usize {
            TRAY_MENU_SHOW => tray_restore(),
            TRAY_MENU_QUIT => {
                tray_delete_icon();
                tray_close_main(main);
            }
            _ => {}
        }
    }
}

/// 取 exe 图标资源（ID = 1，与 `win32::set_window_icon` 同源）。
/// 托盘按 16×16 取小图，取不到退回默认尺寸。句柄进程内复用、不 `DestroyIcon`。
fn tray_icon_handle() -> *mut c_void {
    let cur = TRAY_ICON_H.load(Ordering::SeqCst);
    if cur != 0 {
        return cur as *mut c_void;
    }
    unsafe {
        let h_inst = GetModuleHandleW(std::ptr::null());
        let mut h = LoadImageW(h_inst, 1usize as *const u16, IMAGE_ICON, 16, 16, 0);
        if h.is_null() {
            h = LoadIconW(h_inst, 1usize as *const u16);
        }
        TRAY_ICON_H.store(h as isize, Ordering::SeqCst);
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NOTIFYICONDATAW` 必须是 Win7+ 全尺寸 976 字节：`Shell_NotifyIconW` 按 `cbSize`
    /// 认结构版本，尺寸不对 = 结构被按老版本解读（托盘图标直接挂不上）。
    #[test]
    fn notify_icon_data_is_win7_full_size() {
        assert_eq!(std::mem::size_of::<NotifyIconDataW>(), 976);
        assert_eq!(std::mem::size_of::<WndClassExW>(), 80);
        // MSG：hwnd(8) + message(4) + pad(4) + wParam(8) + lParam(8) + time(4) + pt(8)
        assert_eq!(std::mem::size_of::<Msg>(), 48);
    }

    /// 托盘消息号必须落在应用私有段（`WM_APP + 1000` 起），与 Go 的常量逐个同值。
    #[test]
    fn tray_message_ids_match_go_constants() {
        assert_eq!(TRAY_MSG_CALLBACK, 0x8000 + 1000);
        assert_eq!(TRAY_MSG_INIT, 0x8000 + 1001);
        assert_eq!(TRAY_MSG_ADD_ICON, 0x8000 + 1002);
        assert_eq!(TRAY_MSG_QUIT, 0x8000 + 1003);
    }

    /// 句柄为 0 时 `tray_hide` 如实报错（Go `trayHide` 第一句就是这条），
    /// 且**绝不藏窗**（返回 Err 由桥接层原样回前端）。
    #[test]
    fn tray_hide_without_handle_reports_like_go() {
        assert_eq!(tray_hide(0).unwrap_err(), "窗口句柄不可用");
    }

    /// 未起托盘时 `tray_quit` 直接走关窗（Go：`started` 为假就 `trayCloseMain`）——
    /// 这里用 0 句柄验证不 panic（真句柄的关窗由 webview 侧锁）。
    #[test]
    fn tray_quit_without_start_does_not_panic() {
        tray_quit(0);
        assert!(!tray_started(), "本用例不该把托盘线程拉起来");
    }

    /// 托盘线程真拉起来（真 `RegisterClassExW` + `CreateWindowExW` + 真挂图标 + 真消息循环）。
    ///
    /// **为什么在子进程里跑**：托盘状态是进程级单例（Go 侧同样是一组包级 var），
    /// 而 `cargo test` 默认多线程——若在本进程拉起托盘线程，别的用例的断言会被这次启动
    /// 影响（`tray_started()` 恒真），反过来也会干扰它自己。故本用例自己 re-exec
    /// **同一个测试二进制、只跑这一条**（`--test-threads=1`），在隔离进程里做端到端验证：
    /// 子进程退出即回收托盘线程，**不会在用户桌面留下孤儿图标**。
    /// 主窗句柄用进程内新建的隐藏窗（真 Win32 句柄，不是用户正在用的窗口）。
    #[test]
    fn tray_thread_starts_and_mounts_icon_in_child_process() {
        if std::env::var("RS_GUI_TRAY_CHILD").is_ok() {
            let (_el, window) = hidden_test_window("rs-gui-tray-test-host");
            let hwnd = hwnd_of_window(&window);
            tray_startup(hwnd);
            assert!(tray_started(), "托盘线程必须起来");
            assert_eq!(tray_last_error(), None, "挂图标不该失败");
            assert!(
                tray_icon_visible(),
                "图标必须真挂上（Shell_NotifyIconW 返回非 0）"
            );
            // 二次调用幂等
            tray_startup(hwnd);
            assert!(tray_started());
            assert!(tray_icon_visible());
            // 退到托盘：托盘就绪 + 句柄可用 → Ok（真句柄路径，不是「句柄不可用」那条）
            assert!(tray_hide(hwnd).is_ok(), "托盘就绪后 trayHide 必须成功");
            std::process::exit(0);
        }
        run_in_child_process("tray::tests::tray_thread_starts_and_mounts_icon_in_child_process");
    }

    /// `set_frameless` 的**可观测效果**：对一条真窗口去掉 `WS_CAPTION`（无边框），
    /// 并让变更立即生效（`SWP_FRAMECHANGED`）。关掉 captioned 那条分支就会让
    /// 「样式里不该有 WS_CAPTION」这条断言失败——这是本文件里唯一能自动化的窗口外观判据，
    /// 也是「窗口看起来有没有原生标题栏」这个人工项的唯一机械证据。
    #[test]
    fn set_frameless_clears_the_caption_bit() {
        if std::env::var("RS_GUI_TRAY_CHILD").is_ok() {
            let (_el, window) = hidden_test_window("rs-gui-frameless-test-host");
            let hwnd = hwnd_of_window(&window);
            let before = style_of(hwnd);
            const WS_CAPTION: isize = 0x00C0_0000;
            assert_eq!(
                before & WS_CAPTION,
                WS_CAPTION,
                "建出来的窗应当带标题栏样式（前置条件）：style={before:#x}"
            );
            win32::set_frameless(hwnd);
            let after = style_of(hwnd);
            assert_eq!(
                after & WS_CAPTION,
                0,
                "setFrameless 之后样式里不该再有 WS_CAPTION：{after:#x}"
            );
            std::process::exit(0);
        }
        run_in_child_process("tray::tests::set_frameless_clears_the_caption_bit");
    }

    /* ---------------- 子进程测试夹具 ---------------- */

    /// 起一条**本进程私有**的隐藏窗（真 Win32 句柄，不碰用户正在用的窗口）。
    /// `EventLoop` 必须与窗口同活，故一起返回。
    fn hidden_test_window(title: &str) -> (tao::event_loop::EventLoop<()>, tao::window::Window) {
        use tao::platform::windows::EventLoopBuilderExtWindows;
        let event_loop = tao::event_loop::EventLoopBuilder::new()
            .with_any_thread(true)
            .build();
        let window = tao::window::WindowBuilder::new()
            .with_title(title)
            .with_visible(false)
            .build(&event_loop)
            .expect("建测试窗失败");
        (event_loop, window)
    }

    fn hwnd_of_window(window: &tao::window::Window) -> usize {
        use tao::rwh_06::HasWindowHandle;
        match window.window_handle().unwrap().as_raw() {
            tao::rwh_06::RawWindowHandle::Win32(h) => h.hwnd.get() as usize,
            _ => panic!("测试窗拿不到 Win32 句柄"),
        }
    }

    fn style_of(hwnd: usize) -> isize {
        #[link(name = "user32")]
        extern "system" {
            fn GetWindowLongPtrW(hwnd: *mut c_void, index: i32) -> isize;
        }
        unsafe { GetWindowLongPtrW(hwnd as *mut c_void, win32::GWL_STYLE) }
    }

    /// 把 `name` 这条用例放到**独立子进程**里跑（`--exact --test-threads=1`），
    /// 退出码 0 = 子进程里全过。子进程拿到同一个测试二进制，故不会跑偏到别的用例。
    fn run_in_child_process(name: &str) {
        let exe = std::env::current_exe().expect("取测试二进制路径失败");
        let out = std::process::Command::new(exe)
            .args([name, "--exact", "--test-threads=1", "--nocapture"])
            .env("RS_GUI_TRAY_CHILD", "1")
            .output()
            .expect("起子进程失败");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        // 退出码是本用例唯一需要的信号：子进程的正常路径以 exit(0) 收尾，
        // 任何 `assert!` 失败都会让 libtest 以非 0 退出。stdout 只在失败时当诊断打印
        // （libtest 把 stdout 接管道时不写那行 `test result: …` 汇总，故不拿它当判据）。
        assert!(
            out.status.success(),
            "子进程里的端到端用例 {name} 失败（退出码 {:?}）：\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            out.status.code()
        );
        assert!(
            stdout.contains(name),
            "子进程必须真匹配到那一条用例（防止名字被改后静默跑 0 条）：\n{stdout}"
        );
    }
}
