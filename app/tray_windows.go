//go:build windows

package main

/* 关窗退到托盘（手写 win32，零新依赖，不引入 systray 类库）：
   - 独立隐藏消息窗收托盘回调——不子类化 WebView 顶层窗（改别人的 WndProc 风险高）
   - Shell_NotifyIconW 挂/摘图标，TaskbarCreated 广播后自己挂回来（Explorer 重启）
   - ShowWindow(SW_HIDE / SW_RESTORE) 藏窗与还原
   - 退出路径先 NIM_DELETE 再走原有关窗流程（PostMessage WM_CLOSE），不留孤儿图标
   常驻启动：GUI 进程起就把线程拉起来，图标一直在（窗口可见时也在），进程退出前不摘；
   单击图标切换隐藏/还原，右键菜单「显示界面 / 退出界面」不变。 */

import (
	"errors"
	"fmt"
	"log"
	"runtime"
	"sync"
	"syscall"
	"unsafe"
)

const (
	trayIconID      = 1
	trayMsgCallback = wmApp + 1000 // 托盘鼠标回调（NOTIFYICONDATAW.uCallbackMessage）
	trayMsgInit     = wmApp + 1001 // 自投递：消息循环跑起来后再挂图标
	trayMsgAddIcon  = wmApp + 1002 // 请托盘线程幂等补挂图标（藏窗前保险；图标常驻，本不摘）
	trayMsgQuit     = wmApp + 1003 // 请托盘线程摘图标再关主窗
	trayMenuShow    = 1            // 右键菜单：显示界面
	trayMenuQuit    = 2            // 右键菜单：退出界面

	wmApp       = 0x8000 // 应用私有消息起点（自定义窗口类专用）
	wmDestroy   = 0x0002
	wmClose     = 0x0010
	wmNull      = 0x0000
	wmLButtonUp = 0x0202
	wmRButtonUp = 0x0205

	swHide    = 0 // ShowWindow
	swRestore = 9

	nimAdd     = 0   // Shell_NotifyIcon
	nimDelete  = 2   //
	nifMessage = 0x1 // NOTIFYICONDATAW.uFlags
	nifIcon    = 0x2
	nifTip     = 0x4

	imageIcon      = 1   // LoadImageW
	mfString       = 0   // AppendMenuW
	tpmRightButton = 0x2 // TrackPopupMenu
	tpmReturnCmd   = 0x100
)

// trayIconData = NOTIFYICONDATAW（Win7+ 全尺寸 976 字节）。Shell_NotifyIconW 按 cbSize
// 认结构版本，字段顺序/对齐必须与 SDK 一致（Go 在 amd64 上的对齐规则与 MSVC 相同）。
type trayIconData struct {
	cbSize           uint32
	hWnd             uintptr
	uID              uint32
	uFlags           uint32
	uCallbackMessage uint32
	hIcon            uintptr
	szTip            [128]uint16
	dwState          uint32
	dwStateMask      uint32
	szInfo           [256]uint16
	uVersion         uint32
	szInfoTitle      [64]uint16
	dwInfoFlags      uint32
	guidItem         [16]byte
	hBalloonIcon     uintptr
}

// trayWndClass = WNDCLASSEXW（RegisterClassExW 用）。
type trayWndClass struct {
	cbSize        uint32
	style         uint32
	lpfnWndProc   uintptr
	cbClsExtra    int32
	cbWndExtra    int32
	hInstance     uintptr
	hIcon         uintptr
	hCursor       uintptr
	hbrBackground uintptr
	lpszMenuName  *uint16
	lpszClassName *uint16
	hIconSm       uintptr
}

type trayPoint struct{ X, Y int32 }

// trayMsg = MSG（GetMessageW / DispatchMessageW 用）。
type trayMsg struct {
	hwnd    uintptr
	message uint32
	wParam  uintptr
	lParam  uintptr
	time    uint32
	pt      trayPoint
}

var (
	trayUser32   = syscall.NewLazyDLL("user32.dll")
	trayShell32  = syscall.NewLazyDLL("shell32.dll")
	trayKernel32 = syscall.NewLazyDLL("kernel32.dll")

	// Go 回调必须存包级变量防 GC：WNDPROC 挂进系统后由 Windows 长期持有该指针。
	trayWndProcCb = syscall.NewCallback(trayWndProc)

	trayMu          sync.Mutex
	trayStarted     bool
	trayErr         error
	trayReadyCh     chan struct{}
	trayHWND        uintptr // 隐藏消息窗
	trayMainHWND    uintptr // WebView 主窗
	trayIconVisible bool
	trayTaskbarMsg  uint32 // RegisterWindowMessageW("TaskbarCreated")

	trayIconOnce sync.Once
	trayIconH    uintptr
)

/* ---------------- 对外入口（bridge.go 调用） ---------------- */

// trayHide 退到托盘：确保托盘就绪（图标已挂）后隐藏主窗。
// 托盘起不来就不藏窗——藏了没图标等于把用户关在门外，此时把错误交回调用方。
func trayHide(mainHwnd uintptr) error {
	if mainHwnd == 0 {
		return errors.New("窗口句柄不可用")
	}
	if err := trayStart(mainHwnd); err != nil {
		return err
	}
	trayMu.Lock()
	hwnd := trayHWND
	trayMu.Unlock()
	if hwnd == 0 {
		return errors.New("托盘消息窗未就绪")
	}
	// 图标已由常驻启动挂好，这里再自投递一次幂等补挂（Explorer 重建后漏挂的保险；
	// 挂图标一律在托盘线程做）
	trayUser32.NewProc("PostMessageW").Call(hwnd, trayMsgAddIcon, 0, 0)
	trayUser32.NewProc("ShowWindow").Call(mainHwnd, swHide)
	return nil
}

// trayQuit 退出界面：先摘托盘图标再走原有关窗流程（WM_CLOSE → webview Run() 干净返回
// → defer w.Destroy()）。托盘线程没起过就直接关窗（那时压根没有图标）。
func trayQuit(mainHwnd uintptr) {
	trayMu.Lock()
	hwnd, started := trayHWND, trayStarted
	trayMu.Unlock()
	if started && hwnd != 0 {
		// 交给托盘线程：Shell_NotifyIcon 与它建的窗同线程，摘图标最稳
		if r, _, _ := trayUser32.NewProc("PostMessageW").Call(hwnd, trayMsgQuit, 0, 0); r != 0 {
			return
		}
	}
	trayCloseMain(mainHwnd)
}

// trayStartup 常拉托盘线程（GUI 启动时调用一次）：图标从进程起来就挂着，窗口可见时也在，
// 进程退出前不摘。启动失败只写日志——降级为「没有托盘图标」，界面照常可用；用户真要选
// 「退到托盘」时 trayHide 会拿到同一个错误如实回前端，不会出现藏了窗却没图标的情况。
func trayStartup(mainHwnd uintptr) {
	if mainHwnd == 0 {
		log.Printf("[tray] 主窗句柄不可用，托盘图标未挂载")
		return
	}
	if err := trayStart(mainHwnd); err != nil {
		log.Printf("[tray] 托盘启动失败，本次运行无托盘图标：%v", err)
	}
}

// trayCloseMain 发 WM_CLOSE 走原有关窗流程（ccWindowCmd("close") 同款语义）。
func trayCloseMain(mainHwnd uintptr) {
	if mainHwnd != 0 {
		trayUser32.NewProc("PostMessageW").Call(mainHwnd, wmClose, 0, 0)
	}
}

/* ---------------- 常驻启动 ---------------- */

// trayStart 拉起托盘线程（幂等）：隐藏消息窗与消息循环跑在同一条锁定线程上，
// 图标挂好（或明确失败）后才返回。
func trayStart(mainHwnd uintptr) error {
	trayMu.Lock()
	trayMainHWND = mainHwnd
	if trayStarted {
		ch := trayReadyCh
		trayMu.Unlock()
		if ch != nil {
			<-ch
		}
		trayMu.Lock()
		err := trayErr
		trayMu.Unlock()
		return err
	}
	trayStarted = true
	ready := make(chan struct{})
	trayReadyCh = ready
	trayMu.Unlock()

	go func() {
		runtime.LockOSThread() // 窗口与消息循环必须同线程（GetMessage 取的是本线程队列）
		defer runtime.UnlockOSThread()
		if err := trayCreateWindow(); err != nil {
			trayFinishStart(err)
			return
		}
		// 先往队列塞一条自投递：wndProc 里挂图标时消息循环已在跑（Shell_NotifyIcon
		// 若向本窗发消息，同线程可重入处理，不会僵在没人泵消息的线程上）
		if r, _, err := trayUser32.NewProc("PostMessageW").Call(trayHWND, trayMsgInit, 0, 0); r == 0 {
			trayFinishStart(fmt.Errorf("托盘初始化消息投递失败: %v", err))
			return
		}
		trayLoop()
	}()

	<-ready
	trayMu.Lock()
	defer trayMu.Unlock()
	return trayErr
}

// trayFinishStart 交回启动结果（只生效一次；失败路径也要放行调用方，不能让它干等）。
func trayFinishStart(err error) {
	trayMu.Lock()
	trayErr = err
	ch := trayReadyCh
	trayReadyCh = nil
	trayMu.Unlock()
	if ch != nil {
		close(ch)
	}
}

func trayCreateWindow() error {
	hInst, _, _ := trayKernel32.NewProc("GetModuleHandleW").Call(0)
	className, _ := syscall.UTF16PtrFromString("ProxyDeckTrayWnd")
	wc := trayWndClass{
		cbSize:        uint32(unsafe.Sizeof(trayWndClass{})),
		lpfnWndProc:   trayWndProcCb,
		hInstance:     hInst,
		lpszClassName: className,
	}
	if atom, _, err := trayUser32.NewProc("RegisterClassExW").Call(uintptr(unsafe.Pointer(&wc))); atom == 0 {
		return fmt.Errorf("RegisterClassExW 失败: %v", err)
	}
	title, _ := syscall.UTF16PtrFromString(appTitle)
	// WS_OVERLAPPED(0) + 全零尺寸：只收消息、永不可见
	hwnd, _, err := trayUser32.NewProc("CreateWindowExW").Call(
		0, uintptr(unsafe.Pointer(className)), uintptr(unsafe.Pointer(title)),
		0, 0, 0, 0, 0, 0, 0, hInst, 0)
	if hwnd == 0 {
		return fmt.Errorf("CreateWindowExW 失败: %v", err)
	}
	// Explorer 重启会广播 TaskbarCreated：图标得自己挂回去（消息号运行时注册，非固定值）
	name, _ := syscall.UTF16PtrFromString("TaskbarCreated")
	msg, _, _ := trayUser32.NewProc("RegisterWindowMessageW").Call(uintptr(unsafe.Pointer(name)))
	trayMu.Lock()
	trayHWND = hwnd
	trayTaskbarMsg = uint32(msg)
	trayMu.Unlock()
	return nil
}

func trayLoop() {
	var msg trayMsg
	for {
		r, _, _ := trayUser32.NewProc("GetMessageW").Call(uintptr(unsafe.Pointer(&msg)), 0, 0, 0)
		if int32(r) <= 0 { // 0 = WM_QUIT，-1 = 出错
			return
		}
		trayUser32.NewProc("TranslateMessage").Call(uintptr(unsafe.Pointer(&msg)))
		trayUser32.NewProc("DispatchMessageW").Call(uintptr(unsafe.Pointer(&msg)))
	}
}

/* ---------------- 消息处理 ---------------- */

// trayWndProc 隐藏消息窗的窗口过程：托盘回调、挂图标、任务栏重建、退出四件事，
// 其余原样回系统。回调内不做重活（在托盘线程的消息循环里跑）。
func trayWndProc(hwnd uintptr, msg uint32, wParam, lParam uintptr) uintptr {
	trayMu.Lock()
	taskbarMsg := trayTaskbarMsg
	trayMu.Unlock()
	switch {
	case msg == trayMsgCallback:
		// 未调 NIM_SETVERSION（版本 0 语义）：lParam 就是鼠标消息
		switch uint32(lParam) & 0xFFFF {
		case wmLButtonUp:
			trayToggle()
		case wmRButtonUp:
			trayMenu()
		}
		return 0
	case msg == trayMsgInit:
		err := trayAddIcon()
		if err != nil {
			log.Printf("[tray] 托盘图标挂载失败：%v", err)
			trayUser32.NewProc("DestroyWindow").Call(hwnd) // → WM_DESTROY → PostQuitMessage
		}
		trayFinishStart(err)
		return 0
	case msg == trayMsgAddIcon:
		if err := trayAddIcon(); err != nil {
			log.Printf("[tray] 托盘图标重挂失败：%v", err)
		}
		return 0
	case msg == trayMsgQuit:
		trayDeleteIcon()
		trayMu.Lock()
		main := trayMainHWND
		trayMu.Unlock()
		trayCloseMain(main)
		return 0
	case taskbarMsg != 0 && msg == taskbarMsg:
		// 任务栏/Explorer 重建：本来挂着图标才需要挂回来（窗口藏着的场景）
		trayMu.Lock()
		need := trayIconVisible
		trayIconVisible = false // 壳里已没了，让 trayAddIcon 重新挂
		trayMu.Unlock()
		if need {
			if err := trayAddIcon(); err != nil {
				log.Printf("[tray] 任务栏重建后挂图标失败：%v", err)
			}
		}
		return 0
	case msg == wmDestroy:
		trayUser32.NewProc("PostQuitMessage").Call(0)
		return 0
	}
	r, _, _ := trayUser32.NewProc("DefWindowProcW").Call(hwnd, uintptr(msg), wParam, lParam)
	return r
}

// trayAddIcon 挂托盘图标（幂等；必须在托盘线程调用——消息循环已在跑）。
func trayAddIcon() error {
	trayMu.Lock()
	if trayIconVisible {
		trayMu.Unlock()
		return nil
	}
	hwnd := trayHWND
	trayMu.Unlock()
	if hwnd == 0 {
		return errors.New("托盘消息窗未就绪")
	}
	tip, _ := syscall.UTF16FromString(appTitle)
	nid := trayIconData{
		hWnd:             hwnd,
		uID:              trayIconID,
		uFlags:           nifMessage | nifIcon | nifTip,
		uCallbackMessage: trayMsgCallback,
		hIcon:            trayIconHandle(),
	}
	nid.cbSize = uint32(unsafe.Sizeof(nid))
	copy(nid.szTip[:], tip)
	if r, _, err := trayShell32.NewProc("Shell_NotifyIconW").Call(nimAdd, uintptr(unsafe.Pointer(&nid))); r == 0 {
		return fmt.Errorf("Shell_NotifyIcon(NIM_ADD) 失败: %v", err)
	}
	trayMu.Lock()
	trayIconVisible = true
	trayMu.Unlock()
	return nil
}

// trayDeleteIcon 摘托盘图标（幂等；托盘线程调用）。
func trayDeleteIcon() {
	trayMu.Lock()
	hwnd, visible := trayHWND, trayIconVisible
	trayMu.Unlock()
	if !visible || hwnd == 0 {
		return
	}
	nid := trayIconData{hWnd: hwnd, uID: trayIconID}
	nid.cbSize = uint32(unsafe.Sizeof(nid))
	trayShell32.NewProc("Shell_NotifyIconW").Call(nimDelete, uintptr(unsafe.Pointer(&nid)))
	trayMu.Lock()
	trayIconVisible = false
	trayMu.Unlock()
}

// trayRestore 还原主窗：SW_RESTORE 带出并置前。
// 图标不摘——常驻图标在进程退出前一直在（窗口可见时也在）。
func trayRestore() {
	trayMu.Lock()
	hwnd := trayMainHWND
	trayMu.Unlock()
	if hwnd == 0 {
		return
	}
	trayUser32.NewProc("ShowWindow").Call(hwnd, swRestore)
	trayUser32.NewProc("SetForegroundWindow").Call(hwnd)
}

// trayToggle 单击托盘图标：窗口可见则藏进托盘，已藏则还原带出。
// IsWindowVisible 是实时状态，不用自己记窗口可见性（还原后用户手动最小化等情形也不会错判）。
func trayToggle() {
	trayMu.Lock()
	hwnd := trayMainHWND
	trayMu.Unlock()
	if hwnd == 0 {
		return
	}
	if v, _, _ := trayUser32.NewProc("IsWindowVisible").Call(hwnd); v != 0 {
		trayUser32.NewProc("ShowWindow").Call(hwnd, swHide)
		return
	}
	trayRestore()
}

// trayMenu 右键菜单：显示界面 / 退出界面。TrackPopupMenu 前必须置前本窗，
// 否则点菜单外部菜单不消失（Win32 老规矩）；菜单用 TPM_RETURNCMD 自取命令号，
// 不依赖 WM_COMMAND 回路。
func trayMenu() {
	menu, _, _ := trayUser32.NewProc("CreatePopupMenu").Call()
	if menu == 0 {
		return
	}
	defer trayUser32.NewProc("DestroyMenu").Call(menu)
	show, _ := syscall.UTF16PtrFromString("显示界面")
	quit, _ := syscall.UTF16PtrFromString("退出界面")
	appendMenu := trayUser32.NewProc("AppendMenuW")
	appendMenu.Call(menu, mfString, trayMenuShow, uintptr(unsafe.Pointer(show)))
	appendMenu.Call(menu, mfString, trayMenuQuit, uintptr(unsafe.Pointer(quit)))

	var pt trayPoint
	trayUser32.NewProc("GetCursorPos").Call(uintptr(unsafe.Pointer(&pt)))
	trayMu.Lock()
	hwnd, main := trayHWND, trayMainHWND
	trayMu.Unlock()
	trayUser32.NewProc("SetForegroundWindow").Call(hwnd)
	cmd, _, _ := trayUser32.NewProc("TrackPopupMenu").Call(menu, tpmRightButton|tpmReturnCmd,
		uintptr(int(pt.X)), uintptr(int(pt.Y)), 0, hwnd, 0)
	trayUser32.NewProc("PostMessageW").Call(hwnd, wmNull, 0, 0)
	switch cmd {
	case trayMenuShow:
		trayRestore()
	case trayMenuQuit:
		trayDeleteIcon()
		trayCloseMain(main)
	}
}

// trayIconHandle 取 exe 图标资源（rsrc 生成的 ID = 1，与 setWindowIcon 同源）。
// 托盘按 16×16 取小图，取不到退回默认尺寸（LoadIconW 同款调用）。
// 句柄进程内复用、不 DestroyIcon：托盘图标要一直用着它。
func trayIconHandle() uintptr {
	trayIconOnce.Do(func() {
		hInst, _, _ := trayKernel32.NewProc("GetModuleHandleW").Call(0)
		h, _, _ := trayUser32.NewProc("LoadImageW").Call(hInst, 1, imageIcon, 16, 16, 0)
		if h == 0 {
			h, _, _ = trayUser32.NewProc("LoadIconW").Call(hInst, 1)
		}
		trayIconH = h
	})
	return trayIconH
}
