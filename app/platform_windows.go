//go:build windows

package main

import (
	"syscall"
	"unsafe"
)

// setDPIAware 让 WebView2 窗口在高 DPI 屏幕上清晰渲染（尽力而为）。
func setDPIAware() {
	defer func() { _ = recover() }()
	user32 := syscall.NewLazyDLL("user32.dll")
	// SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4)
	if p := user32.NewProc("SetProcessDpiAwarenessContext"); p != nil {
		if r, _, _ := p.Call(^uintptr(3)); r != 0 {
			return
		}
	}
	// 旧系统回退：SetProcessDPIAware()
	if p := user32.NewProc("SetProcessDPIAware"); p != nil {
		p.Call()
	}
}

// initialWindowSize 按主屏逻辑分辨率自适应初始窗口大小：
// 高分辨率屏（2K/4K）自动放大，普通 1080p 保持 1280x800，且不超出屏幕工作区。
func initialWindowSize() (int, int) {
	user32 := syscall.NewLazyDLL("user32.dll")
	sm := user32.NewProc("GetSystemMetrics")
	sw, _, _ := sm.Call(0) // SM_CXSCREEN
	sh, _, _ := sm.Call(1) // SM_CYSCREEN
	if sw <= 0 || sh <= 0 {
		return 1280, 800
	}
	w := int(sw) * 4 / 5
	h := int(sh) * 4 / 5
	if w > 1600 {
		w = 1600
	}
	if h > 1000 {
		h = 1000
	}
	if w < 1080 {
		w = 1080
	}
	if h < 680 {
		h = 680
	}
	// 兜底：任何情况下不超出屏幕
	if w > int(sw)*95/100 {
		w = int(sw) * 95 / 100
	}
	if h > int(sh)*95/100 {
		h = int(sh) * 95 / 100
	}
	return w, h
}

// setWindowIcon 把 exe 自带的图标设置到窗口（标题栏 + 任务栏）。
// webview 底层注册窗口类时用的是系统默认图标（IDI_APPLICATION），
// 会盖掉 exe 资源里的图标，必须用 WM_SETICON 显式覆盖。
func setWindowIcon(hwnd uintptr) {
	defer func() { _ = recover() }()
	user32 := syscall.NewLazyDLL("user32.dll")
	kernel32 := syscall.NewLazyDLL("kernel32.dll")
	hInst, _, _ := kernel32.NewProc("GetModuleHandleW").Call(0)
	// rsrc 生成的图标资源 ID = 1（MAKEINTRESOURCE(1)）
	hIcon, _, _ := user32.NewProc("LoadIconW").Call(hInst, 1)
	if hIcon == 0 {
		return
	}
	const WM_SETICON = 0x0080
	const ICON_BIG = 1
	const ICON_SMALL = 0
	user32.NewProc("SendMessageW").Call(hwnd, WM_SETICON, ICON_BIG, hIcon)
	user32.NewProc("SendMessageW").Call(hwnd, WM_SETICON, ICON_SMALL, hIcon)
}

// enableDarkTitleBar 把原生标题栏切换为深色沉浸式（Win10 1809+ / Win11）。
func enableDarkTitleBar(hwnd uintptr) {
	defer func() { _ = recover() }()
	const DWMWA_USE_IMMERSIVE_DARK_MODE = 20
	p := syscall.NewLazyDLL("dwmapi.dll").NewProc("DwmSetWindowAttribute")
	var dark int32 = 1
	p.Call(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, uintptr(unsafe.Pointer(&dark)), 4)
}

// setFrameless 去掉原生标题栏和可调边框（WS_CAPTION|WS_THICKFRAME），
// 窗口改为自绘机身样式：台肩红绿灯条即标题栏（三灯功能由前端桥接实现）。
// 并通过 DWM 显式开启 Win11 圆角（剥掉 THICKFRAME 后系统默认不再给圆角；
// Win10 无此属性，保持直角属预期）。保留 WS_SYSMENU（Alt+F4 / 任务栏右键仍可用）
// 与 MIN/MAXBOX 样式（系统级最大化动画）。
func setFrameless(hwnd uintptr) {
	defer func() { _ = recover() }()
	user32 := syscall.NewLazyDLL("user32.dll")
	const GWL_STYLE = ^uintptr(15) // -16
	const (
		WS_CAPTION    = 0x00C00000
		WS_THICKFRAME = 0x00040000
	)
	get := user32.NewProc("GetWindowLongPtrW")
	set := user32.NewProc("SetWindowLongPtrW")
	st, _, _ := get.Call(hwnd, GWL_STYLE)
	st &^= WS_CAPTION | WS_THICKFRAME
	set.Call(hwnd, GWL_STYLE, st)
	// SWP_NOSIZE|SWP_NOMOVE|SWP_NOZORDER|SWP_FRAMECHANGED：让边框变更立即生效
	user32.NewProc("SetWindowPos").Call(hwnd, 0, 0, 0, 0, 0, 0x1|0x2|0x4|0x20)
	// Win11 圆角：DWMWA_WINDOW_CORNER_PREFERENCE(33) = DWMWCP_ROUND(2)
	p := syscall.NewLazyDLL("dwmapi.dll").NewProc("DwmSetWindowAttribute")
	var pref int32 = 2
	p.Call(hwnd, 33, uintptr(unsafe.Pointer(&pref)), 4)
}

// windowCmd 自绘窗口控制：move=进入系统拖拽移动循环 / min / max / close。
// move 必须先 ReleaseCapture() 放掉 WebView2 子窗口持有的鼠标捕获，
// 再发 WM_NCLBUTTONDOWN+HTCAPTION 进入系统移动模态循环——这是无边框
// 窗口接 WebView2 的标准做法（Electron/Tauri 同款），漏掉 ReleaseCapture
// 时捕获仍在子窗口手里，拖动循环拿不到鼠标事件（第一版失败的原因）。
// move 用 SendMessage(WM_NCLBUTTONDOWN, HTCAPTION) 触发系统移动模态循环——
// 与原生标题栏拖拽完全同路径（拖到屏幕顶自动最大化等系统行为全部保留）。
func windowCmd(hwnd uintptr, cmd string) {
	defer func() { _ = recover() }()
	user32 := syscall.NewLazyDLL("user32.dll")
	switch cmd {
	case "move":
		user32.NewProc("ReleaseCapture").Call()
		const WM_NCLBUTTONDOWN = 0xA1
		const HTCAPTION = 2
		user32.NewProc("SendMessageW").Call(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0)
	case "min":
		const WM_SYSCOMMAND = 0x112
		const SC_MINIMIZE = 0xF020
		user32.NewProc("PostMessageW").Call(hwnd, WM_SYSCOMMAND, SC_MINIMIZE, 0)
	case "max":
		// 第 17 轮：frameless 窗上 WM_SYSCOMMAND/SC_MAXIMIZE 语义异常（三灯真机失灵成因之一），
		// 改 ShowWindow 直控：SW_MAXIMIZE(3) 最大化 / SW_RESTORE(9) 还原，IsZoomed 判定切替。
		z, _, _ := user32.NewProc("IsZoomed").Call(hwnd)
		if z != 0 {
			const SW_RESTORE = 9
			user32.NewProc("ShowWindow").Call(hwnd, SW_RESTORE)
		} else {
			const SW_MAXIMIZE = 3
			user32.NewProc("ShowWindow").Call(hwnd, SW_MAXIMIZE)
		}
	case "close":
		const WM_CLOSE = 0x10
		user32.NewProc("PostMessageW").Call(hwnd, WM_CLOSE, 0, 0)
	}
}

// faxUser32 / faxPrevWndProc / faxWndProcCallback 浮窗子类化专用包级状态：
// Go 回调必须存包级变量防 GC（SetWindowLongPtrW 挂进系统后 Windows 长期持有该指针，
// 回调被 GC 回收 = 下一条消息即崩）；faxPrevWndProc 存原 WndProc 供 CallWindowProc 回链。
var (
	faxUser32          = syscall.NewLazyDLL("user32.dll")
	faxPrevWndProc     uintptr
	faxWndProcCallback = syscall.NewCallback(faxFullClientWndProc)
)

// faxFullClientWndProc 浮窗子类化 WndProc：只拦 WM_NCCALCSIZE(0x0083) 且 wParam=1 时
// 返回 0（整个窗口=客户区，无保留边框），其余消息原样 CallWindowProc 回原链。
// 回调内不做任何重活（在 UI 消息循环里执行）。
func faxFullClientWndProc(hwnd uintptr, msg uint32, wParam, lParam uintptr) uintptr {
	const WM_NCCALCSIZE = 0x0083
	if msg == WM_NCCALCSIZE && wParam != 0 {
		return 0
	}
	r, _, _ := faxUser32.NewProc("CallWindowProcW").Call(
		faxPrevWndProc, hwnd, uintptr(msg), wParam, lParam)
	return r
}

// framelessFullClient 子类化窗口 WndProc（GWL_WNDPROC），吃掉 setFrameless 后 DWM 仍保留的
// ~8px 不可见边框（窗口物理 596×823 vs 内容 580×784 的根因）：WM_NCCALCSIZE(wParam=1)
// 返回 0 → 客户区铺满全窗。重复调用安全（已子类化即返回，防回调链自环）；
// 浮窗进程短命，不做卸载。只挂浮窗，主窗不调此函数。
func framelessFullClient(hwnd uintptr) {
	defer func() { _ = recover() }()
	const GWL_WNDPROC = ^uintptr(3) // -4
	cur, _, _ := faxUser32.NewProc("GetWindowLongPtrW").Call(hwnd, GWL_WNDPROC)
	if cur == 0 || cur == faxWndProcCallback {
		return
	}
	faxPrevWndProc = cur
	faxUser32.NewProc("SetWindowLongPtrW").Call(hwnd, GWL_WNDPROC, faxWndProcCallback)
}

// setClientSize 在 framelessFullClient 生效后把窗口外廓直接钉到目标尺寸：
// webview 的 SetSize 内部按 WS_OVERLAPPEDWINDOW 的标题栏 metrics 外扩
// （请求 580×784 → 外廓实得 596×823，实测在案），子类化吃掉保留边框后
// 必须用 SetWindowPos 直接定外廓，客户区才会恰好等于 580×784
// （WM_NCCALCSIZE 返回 0 → client=window）。须在子类化之后调用。
func setClientSize(hwnd uintptr, w, h int) {
	defer func() { _ = recover() }()
	const SWP_NOMOVE = 0x2
	const SWP_NOZORDER = 0x4
	const SWP_NOACTIVATE = 0x10
	faxUser32.NewProc("SetWindowPos").Call(hwnd, 0, 0, 0,
		uintptr(w), uintptr(h), SWP_NOMOVE|SWP_NOZORDER|SWP_NOACTIVATE)
}

// fatalBox 弹出原生消息框（用于 WebView2 运行时缺失等启动期致命错误）。
func fatalBox(msg string) {
	defer func() { _ = recover() }()
	p := syscall.NewLazyDLL("user32.dll").NewProc("MessageBoxW")
	t, _ := syscall.UTF16PtrFromString(msg)
	c, _ := syscall.UTF16PtrFromString(appTitle)
	const MB_OK_ICONWARNING = 0x30
	p.Call(0, uintptr(unsafe.Pointer(t)), uintptr(unsafe.Pointer(c)), MB_OK_ICONWARNING)
}

// findFaxPopup 返回注水专线浮窗句柄（按窗口标题精确匹配；无则 0）。
func findFaxPopup() uintptr {
	user32 := syscall.NewLazyDLL("user32.dll")
	title, _ := syscall.UTF16PtrFromString("注水专线 · WATER PROBE FX-01")
	h, _, _ := user32.NewProc("FindWindowW").Call(0, uintptr(unsafe.Pointer(title)))
	return h
}

// foregroundWindow 还原并置前（最小化的浮窗被再次召唤时先还原再置顶）。
func foregroundWindow(hwnd uintptr) {
	user32 := syscall.NewLazyDLL("user32.dll")
	const SW_RESTORE = 9
	user32.NewProc("ShowWindow").Call(hwnd, SW_RESTORE)
	user32.NewProc("SetForegroundWindow").Call(hwnd)
}

// showWindowIfHidden 自愈窗口可见性（第 47 轮）：一键更新重启的新进程，
// 起窗瞬间旧进程未退、旧窗仍持前台——Windows 前台锁定规则会让新窗创建后停在
// 不可见（实测 EnumWindows vis=False，进程活、窗口藏，用户须手动再运行）。
// 不可见则 ShowWindow 带出（可见则不动，最小化状态不惊扰）。
func showWindowIfHidden(hwnd uintptr) {
	user32 := syscall.NewLazyDLL("user32.dll")
	vis, _, _ := user32.NewProc("IsWindowVisible").Call(hwnd)
	if vis == 0 {
		user32.NewProc("ShowWindow").Call(hwnd, 5) // SW_SHOW
		user32.NewProc("SetForegroundWindow").Call(hwnd)
	}
}

// faxPopupMutexGuard 子进程侧单实例保险：同名互斥体已存在 = 已有浮窗，
// 找到它置前然后本进程自退（覆盖两个入口几乎同时点击的竞态窗口）。
func faxPopupMutexGuard() bool {
	user32 := syscall.NewLazyDLL("kernel32.dll")
	name, _ := syscall.UTF16PtrFromString("Local\\ProxyDeck-FaxWindow")
	_, _, err := user32.NewProc("CreateMutexW").Call(0, 0, uintptr(unsafe.Pointer(name)))
	if err == syscall.ERROR_ALREADY_EXISTS {
		if h := findFaxPopup(); h != 0 {
			foregroundWindow(h)
		}
		return false
	}
	return true
}
