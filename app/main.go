//go:build windows

// ProxyDeck —— 多平台订阅代理的机械风控制台（WebView2 独立 GUI）
//
// 与旧的 HTA 方案的区别：
//   - 代理核心直接在本进程内运行（internal/proxy + internal/server 的 Handler），
//     不再需要释放/调用外部 exe，点火即启动、关窗即优雅停堆。
//   - 界面由系统自带的 WebView2 运行时渲染（Win10/11 通常已预装），
//     编译产物是单个 exe，无控制台窗口（-H windowsgui），无需附带任何 DLL。
//   - 界面文件 ui.html 通过 go:embed 内嵌，并经 127.0.0.1 随机端口的进程内
//     HTTP 服务提供给 WebView2（http://localhost 属于安全上下文，剪贴板等 API 可用）。
package main

import (
	_ "embed"
	"flag"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/Amer-CN/proxydeck/internal/proxy"
	"github.com/Amer-CN/proxydeck/internal/server"
	webview "github.com/webview/webview_go"
)

const (
	appVersion  = "v3.6.2"
	coreVersion = "v1.2.0"
	appTitle    = "ProxyDeck · 多平台代理控制台"
)

//go:embed ui.html
var uiHTML string

var (
	flagPort     = flag.String("port", "55990", "代理监听端口")
	flagHost     = flag.String("host", "127.0.0.1", "代理监听地址")
	flagAPIKey   = flag.String("api-key", "", "CommandCode API Key（可选，也可在界面里填）")
	flagHeadless = flag.Bool("headless", false, "无窗口后台模式（供开机自启使用）")
	flagDebug    = flag.Bool("debug", false, "调试模式：打印请求/响应体到日志（headless-error.log）")
	flagVersion  = flag.Bool("version", false, "打印版本并退出")

	// 插件子模式（--plugin-tuanjie / --plugin-codebuddy / --plugin-bai / --desensitize）
	// 定义在 plugin_modes.go。
)

// runCoreHeadless 在本进程内启动代理核心并阻塞（headless 后台模式）。
// 这是"代理本体"：GUI 的 start 会 spawn 本模式作为子进程。
// 代理核心直接在本进程内运行。
func runCoreHeadless(host, port, apiKey string) error {
	// 未显式传 key 时，从 api-key.txt 读取（GUI 保存的 key 自动生效）。
	if apiKey == "" {
		if b, err := os.ReadFile(filepath.Join(exeDir(), "api-key.txt")); err == nil {
			apiKey = strings.TrimSpace(string(b))
		}
	}
	// 端口被非健康进程占用（僵死/残留）→ 先清理。
	if portBusy(port) {
		_ = killByPort(port)
		time.Sleep(300 * time.Millisecond)
	}
	p := proxy.NewProxy(apiKey)
	p.Debug = *flagDebug
	p.SetStatsFile(filepath.Join(exeDir(), "stats.json"))
	srv := server.NewServer(p)
	srv.SetHost(host)
	srv.SetPort(port)

	ln, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return fmt.Errorf("端口 %s 被其他程序占用: %v", port, err)
	}
	srv.StartWithListener(ln)
	return nil
}

// exeDir 返回 exe 所在目录（go run 时退回工作目录），数据文件都放在这里。
func exeDir() string {
	if p, err := os.Executable(); err == nil {
		if rp, err := filepath.EvalSymlinks(p); err == nil {
			p = rp
		}
		return filepath.Dir(p)
	}
	if wd, err := os.Getwd(); err == nil {
		return wd
	}
	return "."
}

func main() {
	setDPIAware()
	flag.Parse()
	if *flagVersion {
		fmt.Printf("ProxyDeck %s (proxy core %s)\n", appVersion, coreVersion)
		return
	}

	app := newApp(*flagHost, *flagPort, *flagAPIKey)

	// 插件子模式（团结 / CodeBuddy / B.AI / Comate）：实现见 plugin_modes.go
	if *flagPluginTuanjie || *flagPluginCodebuddy || *flagPluginBai || *flagPluginComate {
		os.Exit(runPluginMode())
	}

	// 无窗口后台模式：本进程内嵌代理核心常驻（由 GUI 的 Stop / taskkill 结束）。
	// 注意：headless 是"代理本体"，不再 spawn 子进程；GUI 的 start 才是 spawn 方。
	if *flagHeadless {
		if err := runCoreHeadless(*flagHost, *flagPort, *flagAPIKey); err != nil {
			_ = os.WriteFile(filepath.Join(exeDir(), "headless-error.log"),
				[]byte(err.Error()), 0o600)
			os.Exit(1)
		}
		select {}
	}

	// GUI 模式：进程内静态服务提供内嵌界面（随机空闲端口，避免冲突）
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.Header().Set("Cache-Control", "no-store")
		_, _ = fmt.Fprint(w, uiHTML)
	})
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		fatalBox("无法创建本地界面服务：\r\n" + err.Error())
		os.Exit(1)
	}
	go func() { _ = http.Serve(ln, mux) }()

	w := webview.New(false)
	if w == nil {
		fatalBox("未检测到 WebView2 运行时（Microsoft Edge WebView2 Runtime）。\r\n\r\nWindows 10/11 通常已随系统或 Edge 预装；若被卸载，请点「确定」打开下载页安装后重试。")
		_ = openURL("https://developer.microsoft.com/zh-cn/microsoft-edge/webview2/")
		os.Exit(1)
	}
	w.SetTitle(appTitle)
	// 设备即窗口：机身自绘边框（无原生标题栏），固定 540×760 标准尺寸（小巧）。
	// 可读性由前端统一字号规范承担（.work/ui-src.html「统一字号规范」块），不再整机放大。
	// 关窗不停代理（headless 后台常驻），重开 GUI 随时接管。
	w.SetSize(540, 760, webview.HintNone)
	defer w.Destroy()
	// 注意：代理以独立 headless 子进程常驻，关窗不停止代理。
	// 需要停代理时，在 COMMAND 甲板扳下「代理核心」开关即可。

	// 自绘无边框 + 窗口图标（任务栏）；Dispatch 二次保险（部分环境早期句柄未就绪）
	if hwnd := w.Window(); hwnd != nil {
		setFrameless(uintptr(hwnd))
		setWindowIcon(uintptr(hwnd))
	}
	w.Dispatch(func() {
		if hwnd := w.Window(); hwnd != nil {
			setFrameless(uintptr(hwnd))
			setWindowIcon(uintptr(hwnd))
		}
	})

	app.bindAll(w)
	w.Navigate("http://" + ln.Addr().String() + "/")
	w.Run()
}
