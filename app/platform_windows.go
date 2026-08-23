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
		z, _, _ := user32.NewProc("IsZoomed").Call(hwnd)
		const WM_SYSCOMMAND = 0x112
		if z != 0 {
			const SC_RESTORE = 0xF120
			user32.NewProc("PostMessageW").Call(hwnd, WM_SYSCOMMAND, SC_RESTORE, 0)
		} else {
			const SC_MAXIMIZE = 0xF030
			user32.NewProc("PostMessageW").Call(hwnd, WM_SYSCOMMAND, SC_MAXIMIZE, 0)
		}
	case "close":
		const WM_CLOSE = 0x10
		user32.NewProc("PostMessageW").Call(hwnd, WM_CLOSE, 0, 0)
	}
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
