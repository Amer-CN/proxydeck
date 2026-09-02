//go:build windows

package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/atotto/clipboard"
	webview "github.com/webview/webview_go"
)

// app 是 GUI 与代理核心之间的控制器：代理直接运行在本进程内。
// 结构体定义见 app.go（公开版 / 完整版各一份：完整版多插件状态字段）。

func newApp(host, port, key string) *app {
	a := &app{host: host, port: port, apiKey: key}
	if a.apiKey == "" {
		if b, err := os.ReadFile(a.keyFile()); err == nil {
			a.apiKey = strings.TrimSpace(string(b))
		}
	}
	return a
}

func (a *app) keyFile() string       { return filepath.Join(exeDir(), "api-key.txt") }
func (a *app) statsFile() string     { return filepath.Join(exeDir(), "stats.json") }
func (a *app) noticeFile() string    { return filepath.Join(exeDir(), "notice_dismissed.flag") }
func (a *app) closeHintFile() string { return filepath.Join(exeDir(), "close_hint_dismissed.flag") }
func (a *app) baseURL() string       { return "http://" + net.JoinHostPort(a.host, a.port) }
func (a *app) healthURL() string     { return a.baseURL() + "/health" }

func fileExists(p string) bool { _, err := os.Stat(p); return err == nil }

// migrateLegacyStats 把旧版（bin\stats.json）的统计数据迁移到新位置，仅首次生效。
func (a *app) migrateLegacyStats() {
	dst := a.statsFile()
	if fileExists(dst) {
		return
	}
	src := filepath.Join(exeDir(), "bin", "stats.json")
	in, err := os.Open(src)
	if err != nil {
		return
	}
	defer in.Close()
	out, err := os.Create(dst)
	if err != nil {
		return
	}
	defer out.Close()
	_, _ = io.Copy(out, in)
}

// start 启动代理。代理以 headless 子进程常驻（独立于本 GUI 进程），
// 因此关闭窗口不影响代理；再次打开 GUI 时探活识别为"运行中"。
func (a *app) start(key string) (string, error) {
	a.mu.Lock()
	defer a.mu.Unlock()

	if key == "" {
		key = a.apiKey
	}
	if key != "" {
		a.apiKey = key
		_ = os.WriteFile(a.keyFile(), []byte(key), 0o600)
	}

	// 端口已有健康代理（可能是上次遗留的 headless 子进程）→ 直接接管观察。
	if httpOK(a.healthURL()) {
		a.running = true
		a.external = false
		a.started = time.Now()
		a.lastErr = ""
		return "检测到 " + a.baseURL() + " 代理已在运行，已接入", nil
	}

	// 端口被非健康进程占用（僵死/残留）→ 先清理，再启动。
	if portBusy(a.port) {
		if err := killByPort(a.port); err != nil {
			// 清理失败不阻塞：子进程绑定端口时若仍冲突会再报错
			_ = err
		}
		time.Sleep(300 * time.Millisecond)
	}

	// 启动 headless 子进程（同一 exe，-headless 参数），代理独立常驻。
	exe, err := os.Executable()
	if err != nil {
		return "", fmt.Errorf("无法定位自身可执行文件: %v", err)
	}
	cmd := hiddenCmd(exe, "-headless")
	if key != "" {
		cmd.Args = append(cmd.Args, "-api-key", key)
	}
	// headless 的 stdout/stderr 落盘（headless-error.log，与 exe 同目录），
	// 后台异常时可查原因；日志文件已加入 .gitignore。
	if lf, err := os.OpenFile(filepath.Join(exeDir(), "headless-error.log"),
		os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644); err == nil {
		cmd.Stdout = lf
		cmd.Stderr = lf
	}
	if err := cmd.Start(); err != nil {
		return "", fmt.Errorf("启动后台代理失败: %v", err)
	}

	// 等待代理就绪（最多 ~5s）
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if httpOK(a.healthURL()) {
			a.running = true
			a.external = false
			a.started = time.Now()
			a.lastErr = ""
			return "点火成功 · " + a.baseURL() + " 已就绪（后台常驻，关窗不影响）", nil
		}
		time.Sleep(200 * time.Millisecond)
	}
	return "", fmt.Errorf("后台代理启动超时，请检查端口 %s 是否被占用", a.port)
}

// stop 停堆：探活 → 若有代理在跑（无论是不是本进程起的 headless 子进程），
// 结束占用端口的进程。本 GUI 进程自身不持有代理，停堆只影响后台代理。
func (a *app) stop() (string, error) {
	a.mu.Lock()
	wasExternal := a.external
	a.mu.Unlock()

	if !httpOK(a.healthURL()) {
		a.mu.Lock()
		a.running = false
		a.external = false
		a.mu.Unlock()
		return "代理未在运行", nil
	}

	if err := killByPort(a.port); err != nil {
		return "", err
	}
	a.mu.Lock()
	a.running = false
	a.external = false
	a.mu.Unlock()
	if wasExternal {
		return "后台代理已停止", nil
	}
	return "代理核心已停堆（后台进程已结束）", nil
}

// stateMsg 是发给界面的完整状态。
type stateMsg struct {
	Running         bool   `json:"running"`
	External        bool   `json:"external"`
	Phase           string `json:"phase"` // idle | running | external | error
	Host            string `json:"host"`
	Port            string `json:"port"`
	BaseURL         string `json:"baseUrl"`
	Uptime          int64  `json:"uptime"`
	Autostart       bool   `json:"autostart"`
	APIKey          string `json:"apiKey"`
	NoticeDismissed bool   `json:"noticeDismissed"`
	CloseHintDone   bool   `json:"closeHintDismissed"`
	Version         string `json:"version"`
	StartedOnce     bool   `json:"startedOnce"` // stats.json 存在且 started>0：代理曾成功运行过
	Core            string `json:"core"`
	PID             int    `json:"pid"`
	LastErr         string `json:"lastErr,omitempty"`
}

func (a *app) state() stateMsg {
	a.mu.Lock()
	running, external, started, lastErr, key := a.running, a.external, a.started, a.lastErr, a.apiKey
	a.mu.Unlock()

	// 探活：端口上有健康代理即视为运行中（可能是本 GUI 起的 headless 子进程，
	// 也可能是开机自启/上次遗留的后台实例）。GUI 关闭不影响代理运行。
	alive := httpOK(a.healthURL())
	if alive && !running {
		running = true
		a.mu.Lock()
		a.running = true
		if a.started.IsZero() {
			a.started = time.Now()
		}
		a.mu.Unlock()
		started = a.started
	}

	phase := "idle"
	switch {
	case running:
		phase = "running"
	case lastErr != "":
		phase = "error"
	}
	var up int64
	if running {
		up = int64(time.Since(started).Seconds())
	}
	// 曾成功运行过？读 stats.json 的 started 字段（>0 即曾运行）。
	startedOnce := false
	if b, err := os.ReadFile(a.statsFile()); err == nil {
		var st struct {
			Started int64 `json:"started"`
		}
		if json.Unmarshal(b, &st) == nil && st.Started > 0 {
			startedOnce = true
		}
	}
	return stateMsg{
		Running: running, External: external, Phase: phase,
		Host: a.host, Port: a.port, BaseURL: a.baseURL() + "/v1",
		Uptime: up, Autostart: autostartInstalled(), APIKey: key,
		NoticeDismissed: fileExists(a.noticeFile()),
		CloseHintDone:   fileExists(a.closeHintFile()),
		Version:         appVersion, Core: coreVersion, PID: os.Getpid(), LastErr: lastErr,
		StartedOnce: startedOnce,
	}
}

/* ---------------- 开机自启 ---------------- */

func autostartVBS() string {
	return filepath.Join(os.Getenv("APPDATA"),
		`Microsoft\Windows\Start Menu\Programs\Startup`, "command-code-proxy-autostart.vbs")
}

func autostartInstalled() bool { return fileExists(autostartVBS()) }

func setAutostart(on bool, port string) (string, error) {
	vbs := autostartVBS()
	if !on {
		if fileExists(vbs) {
			if err := os.Remove(vbs); err != nil {
				return "", err
			}
		}
		return "开机自启已取消", nil
	}
	exe, err := os.Executable()
	if err != nil {
		return "", err
	}
	if rp, err := filepath.EvalSymlinks(exe); err == nil {
		exe = rp
	}
	// VBS 字符串内以两个双引号转义一个双引号
	content := "Set sh = CreateObject(\"WScript.Shell\")\r\n" +
		"sh.Run \"\"\"" + exe + "\"\" -headless -port " + port + "\", 0, False\r\n"
	if err := os.WriteFile(vbs, []byte(content), 0o600); err != nil {
		return "", fmt.Errorf("写入启动文件夹失败（被杀软拦截？）: %v", err)
	}
	return "开机自启已设置 · 登录 Windows 后将以无窗口模式自动点火", nil
}

/* ---------------- 工具函数 ---------------- */

// hiddenCmd 创建隐藏窗口的命令：无控制台的 GUI 进程 spawn 控制台子进程
// （netstat/taskkill/rundll32 等）会弹黑窗，必须隐藏。
func hiddenCmd(name string, args ...string) *exec.Cmd {
	cmd := exec.Command(name, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{
		HideWindow:    true,
		CreationFlags: 0x08000000, // CREATE_NO_WINDOW
	}
	return cmd
}

var httpClient = &http.Client{Timeout: 1500 * time.Millisecond}

// httpClientSlow 给官网统计接口用：/v1/usage 要串行调 4 个上游接口，
// 1.5s 的默认超时不够（实测 ~3s），单独用 10s 超时。
var httpClientSlow = &http.Client{Timeout: 10 * time.Second}

// 更新检查缓存：bind 立即返回缓存，网络刷新在后台 goroutine（不阻塞 bind 队列）。
var (
	updateCacheMu sync.Mutex
	updateCache   string
)

// 一键更新流程状态：bind 只读写这几个字段并瞬时返回，真正的下载/替换在
// runOneKeyUpdateFlow 的 goroutine 里。updFlowActive 保证同一时间只跑一个流程。
var (
	updFlowMu     sync.Mutex
	updFlowActive atomic.Bool
	updPhase      = "idle" // idle | downloading | downloaded | replaced | failed
	updPct        = -1     // 0-100；-1 = 服务器没给 Content-Length，前端显示不定进度
	updMsg        = ""
)

// 官网用量缓存：bind 读缓存，后台 goroutine 只在前端触发（usageRefreshCh）时
// 刷新一次——拉杆启动/手动刷新各触发一次，不做周期轮询（官网 429 限流教训）。
var (
	usageCacheMu   sync.Mutex
	usageCache     string
	usageRefreshCh = make(chan struct{}, 1)
)

// httpClientProbe 延迟测试专用：6s 超时 + 走环境代理（HTTP_PROXY/HTTPS_PROXY/ALL_PROXY）。
// 梯子链路抖动大（实测 CommandCode 经代理 0.6-1.6s 波动），短超时易误判超时。
// 注意：Go 默认读环境变量代理，不读 Windows 系统代理设置；用户若用 Clash 等
// 系统级代理，请把 "打开系统代理" 与 "设置环境变量" 都打开，或手动设 ALL_PROXY。
var httpClientProbe = &http.Client{Timeout: 6 * time.Second}

func httpGetSlow(url string) (string, bool) {
	resp, err := httpClientSlow.Get(url)
	if err != nil {
		return "", false
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", false
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		return "", false
	}
	return string(b), true
}

func httpGet(url string) (string, bool) {
	resp, err := httpClient.Get(url)
	if err != nil {
		return "", false
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", false
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		return "", false
	}
	return string(b), true
}

func httpOK(url string) bool { _, ok := httpGet(url); return ok }

// portBusy 检查端口是否有进程监听（不论是否健康）。
func portBusy(port string) bool {
	out, err := hiddenCmd("netstat", "-ano", "-p", "tcp").Output()
	if err != nil {
		return false
	}
	for _, line := range strings.Split(string(out), "\n") {
		f := strings.Fields(line)
		if len(f) >= 5 && strings.EqualFold(f[3], "LISTENING") && strings.HasSuffix(f[1], ":"+port) {
			return true
		}
	}
	return false
}

// killByPort 结束监听指定端口的进程（排除自己），用于停止外部/遗留实例。
func killByPort(port string) error {
	out, err := hiddenCmd("netstat", "-ano", "-p", "tcp").Output()
	if err != nil {
		return fmt.Errorf("netstat 执行失败: %v", err)
	}
	self := os.Getpid()
	killed := false
	for _, line := range strings.Split(string(out), "\n") {
		f := strings.Fields(line)
		// TCP  127.0.0.1:55990  0.0.0.0:0  LISTENING  12345
		if len(f) >= 5 && strings.EqualFold(f[3], "LISTENING") && strings.HasSuffix(f[1], ":"+port) {
			pid, _ := strconv.Atoi(f[4])
			if pid > 0 && pid != self {
				_ = hiddenCmd("taskkill", "/F", "/PID", strconv.Itoa(pid)).Run()
				killed = true
			}
		}
	}
	if !killed {
		return errors.New("未找到占用该端口的进程（可能已自行退出）")
	}
	return nil
}

func openURL(u string) error {
	if !strings.HasPrefix(u, "http://") && !strings.HasPrefix(u, "https://") {
		return errors.New("仅允许打开 http(s) 链接")
	}
	return hiddenCmd("rundll32", "url.dll,FileProtocolHandler", u).Start()
}

/* ---------------- JS 桥接绑定 ---------------- */

func jsonOK(msg string) string {
	b, _ := json.Marshal(map[string]any{"ok": true, "msg": msg})
	return string(b)
}

func jsonErr(err error) string {
	b, _ := json.Marshal(map[string]any{"ok": false, "msg": err.Error()})
	return string(b)
}

func (a *app) bindAll(w webview.WebView) {
	_ = w.Bind("ccGetState", func() string {
		b, _ := json.Marshal(a.state())
		return string(b)
	})
	// ccLiveSnapshot 并发实探核心与五插件端口健康，返回键角蓝灯用的 live 表
	//（第 25 轮用户裁决：探活下沉 Go 侧——UI 不自发网络请求感知服务，那是管理进程的职责）。
	// 55990 核心走 /health，五插件复用 pluginHealthURL；单端口失败即 false，不报错。
	// goroutine 并发探 6 端口，总耗时 ≈ 最慢一个端口的探测时间（~毫秒级）。
	_ = w.Bind("ccLiveSnapshot", func() string {
		targets := []struct{ name, url string }{{"core", a.healthURL()}}
		for _, d := range pluginDefs {
			targets = append(targets, struct{ name, url string }{d.ID, a.pluginHealthURL(d)})
		}
		var mu sync.Mutex
		live := make(map[string]bool, len(targets))
		var wg sync.WaitGroup
		for _, t := range targets {
			wg.Add(1)
			go func(name, u string) {
				defer wg.Done()
				ok := httpOK(u)
				mu.Lock()
				live[name] = ok
				mu.Unlock()
			}(t.name, t.url)
		}
		wg.Wait()
		b, _ := json.Marshal(map[string]any{"ok": true, "live": live})
		return string(b)
	})
	_ = w.Bind("ccStart", func(key string) string {
		msg, err := a.start(strings.TrimSpace(key))
		if err != nil {
			return jsonErr(err)
		}
		return jsonOK(msg)
	})
	_ = w.Bind("ccStop", func() string {
		msg, err := a.stop()
		if err != nil {
			return jsonErr(err)
		}
		return jsonOK(msg)
	})
	// 透传代理的统计数据（原始 JSON，前端直接解析 total/today/models）
	_ = w.Bind("ccStats", func() string {
		if s, ok := httpGet(a.baseURL() + "/v1/stats"); ok {
			return s
		}
		// 代理未运行时兜底读本地 stats.json——历史消耗离线可见
		if b, err := os.ReadFile(a.statsFile()); err == nil && len(b) > 0 {
			return string(b)
		}
		return `{"ok":false,"msg":"代理未运行或无法连接"}`
	})
	// 官网权威统计（/v1/usage）：金额、总 token、运行次数、额度、按模型明细。
	// 上游 4 个接口耗时较长，用独立慢速客户端（10s 超时）。
	_ = w.Bind("ccUsage", func() string {
		// 只读缓存（瞬时返回，不阻塞 bind 队列）；官网 4 接口在后台 goroutine 刷新
		usageCacheMu.Lock()
		cached := usageCache
		usageCacheMu.Unlock()
		if cached != "" {
			return cached
		}
		return `{"ok":false,"msg":"统计加载中"}`
	})
	// 后台异步刷新官网用量（失败静默，前端下次手动触发重试）。
	// 事件驱动：只在收到 usageRefreshCh 信号（拉杆启动成功/手动刷新按钮）时
	// 拉一轮官网 4 接口，不做周期轮询——GUI 未启动 COMMAND 也会频繁访问官网
	// 曾触发限流（429 事故）。
	go func() {
		for range usageRefreshCh {
			if s, ok := httpGetSlow(a.baseURL() + "/v1/usage"); ok {
				usageCacheMu.Lock()
				usageCache = s
				usageCacheMu.Unlock()
			}
		}
	}()
	_ = w.Bind("ccUsageRefresh", func() string {
		select {
		case usageRefreshCh <- struct{}{}:
		default: // 已有待处理刷新，跳过
		}
		return `{"ok":true}`
	})
	// 立即保存 API Key 到 api-key.txt（无需等点火）。
	_ = w.Bind("ccSaveKey", func(key string) string {
		key = strings.TrimSpace(key)
		if key == "" {
			if err := os.Remove(a.keyFile()); err != nil && !os.IsNotExist(err) {
				return jsonErr(err)
			}
			a.apiKey = ""
			return jsonOK("已清除保存的 Key")
		}
		if err := os.WriteFile(a.keyFile(), []byte(key), 0o600); err != nil {
			return jsonErr(err)
		}
		a.apiKey = key
		return jsonOK("API Key 已保存")
	})
	// 版本更新检查 + 下载统计：查 GitHub Releases 全部版本，汇总所有 assets 的
	// 累计下载量（匿名：GitHub 官方计数，不含任何用户信息）。
	// 注意：不能只看 releases/latest——每次发布新版本 latest 会指向新版，
	// 旧版下载量就"消失"了，用户会误以为没人用。累计下载才是真实反馈。
	_ = w.Bind("ccCheckUpdate", func() string {
		// 只读缓存（瞬时返回，不阻塞 WebView2 的串行 bind 队列）；
		// 网络刷新由后台 goroutine 异步完成（见 startUpdateRefresher）。
		updateCacheMu.Lock()
		cached := updateCache
		updateCacheMu.Unlock()
		if cached != "" {
			return cached
		}
		return `{"ok":false,"msg":"checking"}`
	})
	// 后台异步拉取 GitHub releases（不占 bind 队列；成功写缓存供前端重试读取）
	go func() {
		const url = "https://api.github.com/repos/Amer-CN/proxydeck/releases?per_page=100"
		type asset struct {
			Name               string `json:"name"`
			Download           int    `json:"download_count"`
			BrowserDownloadURL string `json:"browser_download_url"`
			Size               int64  `json:"size"`
		}
		type rel struct {
			TagName string  `json:"tag_name"`
			HTMLURL string  `json:"html_url"`
			Assets  []asset `json:"assets"`
		}
		resp, err := httpClientSlow.Get(url)
		if err != nil {
			return
		}
		defer resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			return
		}
		var releases []rel
		if json.NewDecoder(resp.Body).Decode(&releases) != nil || len(releases) == 0 {
			return
		}
		totalDownloads := 0
		latest := releases[0] // GitHub 按时间倒序，第一个即最新
		for _, rl := range releases {
			for _, a := range rl.Assets {
				totalDownloads += a.Download
			}
		}
		// 一键更新用的直链：只认最新 release 里名字为 ProxyDeck.exe 的 asset
		//（实测形态 https://github.com/Amer-CN/proxydeck/releases/download/vX.Y.Z/ProxyDeck.exe）。
		// 老 release 没有该 asset 时留空串 → 前端「立即更新」退回打开网页的旧行为。
		dlURL, dlSize := "", int64(0)
		for _, as := range latest.Assets {
			if as.Name == "ProxyDeck.exe" {
				dlURL, dlSize = as.BrowserDownloadURL, as.Size
				break
			}
		}
		b, _ := json.Marshal(map[string]any{
			"ok": true, "latest": latest.TagName, "url": latest.HTMLURL,
			"downloads": totalDownloads, // 所有版本累计下载量
			"dl_url":    dlURL,          // 新增：exe 直链（老前端忽略新字段，兼容）
			"dl_size":   dlSize,         // 新增：exe 字节数（下载后校验用；0 = 未知不校验）
		})
		updateCacheMu.Lock()
		updateCache = string(b)
		updateCacheMu.Unlock()
	}()
	// 一键更新：触发（action="start"）启动后台 goroutine 立即返回；查询（action="status"）
	// 返回进度。webview_go 的 Bind 按位置把 JS 参数 JSON 反序列化进 Go 参数（个数必须相等），
	// JS 对象映射不进 string 参数，故 action 用纯字符串（前端恒传 'start'/'status'，空串按 start 处理）。
	// bind 体内只碰缓存/互斥锁（微秒级），下载/替换全程在 goroutine——绝不阻塞串行 bind 队列。
	_ = w.Bind("ccOneKeyUpdate", func(action string) string {
		if action == "status" {
			updFlowMu.Lock()
			defer updFlowMu.Unlock()
			b, _ := json.Marshal(map[string]any{"ok": true, "phase": updPhase, "pct": updPct, "msg": updMsg})
			return string(b)
		}
		if action == "" || action == "start" {
			return a.startOneKeyUpdateFlow(w)
		}
		b, _ := json.Marshal(map[string]any{"ok": false, "msg": "未知 action: " + action})
		return string(b)
	})
	// 插件绑定（ccPlugin*）：实现见 plugin_bindings.go。
	a.bindPluginBindings(w)
	_ = w.Bind("ccCalib", func(model, v string) string {
		v = strings.TrimSpace(v)
		model = strings.TrimSpace(model)
		if model == "" {
			return jsonErr(fmt.Errorf("缺少模型名"))
		}
		if v != "" {
			if _, err := strconv.ParseInt(v, 10, 64); err != nil {
				return jsonErr(fmt.Errorf("请输入数字（官网该模型总 Token 数）"))
			}
		}
		// 通过代理核心写入校准（stats.json 同目录）
		// GUI 与代理是独立进程，这里直接调用代理的 /v1/calibration 接口
		resp, err := http.PostForm(a.baseURL()+"/v1/calibration",
			map[string][]string{"model": {model}, "tokens": {v}})
		if err != nil {
			return jsonErr(err)
		}
		defer resp.Body.Close()
		body, _ := io.ReadAll(resp.Body)
		if resp.StatusCode != http.StatusOK {
			return jsonErr(fmt.Errorf("保存失败: %s", string(body)))
		}
		return string(body)
	})
	_ = w.Bind("ccModels", func(plan string) string {
		u := a.baseURL() + "/v1/models"
		if plan == "go" {
			u += "?plan=go"
		}
		if s, ok := httpGet(u); ok {
			return s
		}
		return `{"ok":false,"msg":"代理未运行或无法连接"}`
	})
	_ = w.Bind("ccAutostart", func(on bool) string {
		msg, err := setAutostart(on, a.port)
		if err != nil {
			return jsonErr(err)
		}
		return jsonOK(msg)
	})
	_ = w.Bind("ccOpen", func(u string) string {
		if err := openURL(u); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已打开")
	})
	_ = w.Bind("ccCopy", func(s string) string {
		if err := clipboard.WriteAll(s); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已复制")
	})
	// HTTP/HTTPS 延迟测试：GET 请求计时（走代理链路，非 ICMP ping）。
	// proxyAddr 可选：填 http://127.0.0.1:7890 这类地址时强制走该代理；留空时
	// 先探测常见本地代理端口，找到可用的自动使用（并返回 detected），否则走环境代理/直连。
	_ = w.Bind("ccLatencyTest", func(proxyAddr string) string {
		type probe struct{ name, url string }
		targets := []probe{
			{"CommandCode API", "https://api.commandcode.ai"},
			{"GitHub", "https://api.github.com"},
			{"国内 · 百度", "https://www.baidu.com"},
		}
		type result struct {
			Name string `json:"name"`
			URL  string `json:"url,omitempty"`
			MS   int    `json:"ms"`
			OK   bool   `json:"ok"`
			Err  string `json:"err,omitempty"`
		}
		// 探测可用代理：显式地址优先；否则尝试常见 Clash/梯子端口
		detected := ""
		var client *http.Client
		if addr := strings.TrimSpace(proxyAddr); addr != "" {
			pu, err := url.Parse(addr)
			if err != nil {
				b, _ := json.Marshal(map[string]any{"ok": false, "msg": "代理地址格式错误: " + err.Error()})
				return string(b)
			}
			client = &http.Client{
				Timeout:   2000 * time.Millisecond,
				Transport: &http.Transport{Proxy: http.ProxyURL(pu)},
			}
		} else {
			// 候选端口：Clash 混合端口 7897/7890、V2Ray 10808/10809、通用 8888/1080
			// 按"已验证优先"排序：把当前探测到的端口放最前，减少首次失败浪费
			candidates := []string{
				"http://127.0.0.1:7897", "http://127.0.0.1:7890",
				"http://127.0.0.1:10809", "http://127.0.0.1:10808",
				"http://127.0.0.1:8888", "http://127.0.0.1:1080",
			}
			for _, c := range candidates {
				pu, err := url.Parse(c)
				if err != nil {
					continue
				}
				probe := &http.Client{
					Timeout:   4 * time.Second, // 梯子链路抖动大，留足余量
					Transport: &http.Transport{Proxy: http.ProxyURL(pu)},
				}
				// 探测目标用国内站（百度）：链路抖动下，百度通 = 代理本身可用；
				// CommandCode 慢/超时只是该链路慢，不代表代理不可用
				resp, err := probe.Get("https://www.baidu.com")
				if err == nil {
					resp.Body.Close()
					detected = c
					client = &http.Client{
						Timeout:   6 * time.Second, // 探测到的代理，测目标时也留足超时
						Transport: &http.Transport{Proxy: http.ProxyURL(pu)},
					}
					break
				}
			}
			if client == nil {
				client = httpClientProbe // 无可用代理 → 环境代理/直连
			}
		}
		results := make([]result, 0, len(targets))
		for _, t := range targets {
			r := result{Name: t.name, URL: t.url}
			best := -1
			var lastErr string
			for attempt := 0; attempt < 2; attempt++ {
				start := time.Now()
				resp, err := client.Get(t.url)
				if err == nil {
					io.Copy(io.Discard, io.LimitReader(resp.Body, 1024))
					resp.Body.Close()
					ms := int(time.Since(start).Milliseconds())
					if best < 0 || ms < best {
						best = ms
					}
				} else {
					lastErr = err.Error()
				}
			}
			if best >= 0 {
				r.MS, r.OK = best, true
			} else {
				r.Err = lastErr
			}
			results = append(results, r)
		}
		// 推理延迟（模型"卡不卡"的真实指标）：经本地代理 /v1/chat/completions 发 1-token 最小请求，
		// 测首 token 时间（TTFB）。仅代理运行时可测。
		a.mu.Lock()
		running := a.running
		a.mu.Unlock()
		var inference *result
		if running {
			inf := result{Name: "模型推理", URL: a.baseURL() + "/v1/chat/completions"}
			ms, err := measureFirstToken(a.baseURL(), a.apiKey)
			if err == nil {
				inf.MS, inf.OK = ms, true
			} else {
				inf.Err = err.Error()
			}
			inference = &inf
		}
		b, _ := json.Marshal(map[string]any{"ok": true, "targets": results, "detected": detected, "inference": inference})
		return string(b)
	})
	_ = w.Bind("ccDismiss", func() string {
		if err := os.WriteFile(a.noticeFile(), []byte("1"), 0o600); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已记录")
	})
	_ = w.Bind("ccDismissCloseHint", func() string {
		if err := os.WriteFile(a.closeHintFile(), []byte("1"), 0o600); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已记录")
	})
	// 自绘窗口控制（无边框窗口的标题栏交互）：move=台肩拖拽 / min / max / close。
	// 实现见 platform_windows.go 的 windowCmd。
	_ = w.Bind("ccWindowCmd", func(cmd string) string {
		h := w.Window()
		if h == nil {
			return jsonErr(errors.New("窗口句柄不可用"))
		}
		windowCmd(uintptr(h), strings.TrimSpace(cmd))
		return jsonOK("")
	})
}

/* ---------------- 一键更新流程（下载 → 校验 → 腾位替换 → 重启） ---------------- */

// startOneKeyUpdateFlow 触发一键更新：CAS 抢占单流程名额后立即返回（下载在 goroutine）。
// 下载地址只从后端 updateCache 取（GitHub Releases 数据），不信任前端传参——
// 这个文件落地后要被执行，URL 来源必须后端权威。
func (a *app) startOneKeyUpdateFlow(w webview.WebView) string {
	if !updFlowActive.CompareAndSwap(false, true) {
		return `{"ok":false,"msg":"更新流程已在进行中"}`
	}
	updateCacheMu.Lock()
	cached := updateCache
	updateCacheMu.Unlock()
	var info struct {
		Latest string `json:"latest"`
		DlURL  string `json:"dl_url"`
		DlSize int64  `json:"dl_size"`
	}
	_ = json.Unmarshal([]byte(cached), &info)
	if info.DlURL == "" {
		updFlowActive.Store(false)
		return `{"ok":false,"msg":"当前版本信息没有 exe 直链（老 Release 无 ProxyDeck.exe asset），请到仓库下载页手动更新"}`
	}
	updFlowMu.Lock()
	updPhase, updPct, updMsg = "downloading", -1, "连接下载源…"
	updFlowMu.Unlock()
	go a.runOneKeyUpdateFlow(w, info.Latest, info.DlURL, info.DlSize)
	return `{"ok":true,"msg":"started"}`
}

// runOneKeyUpdateFlow 一键更新主流程（goroutine 内跑，与 bind 队列完全隔离）。
func (a *app) runOneKeyUpdateFlow(w webview.WebView, latest, dlURL string, dlSize int64) {
	// 流程结束（无论成败）释放名额；失败后允许再次触发重试。
	defer updFlowActive.Store(false)
	setPhase := func(phase string, pct int, msg string) {
		updFlowMu.Lock()
		updPhase, updPct, updMsg = phase, pct, msg
		updFlowMu.Unlock()
	}
	// fail：phase=failed + msg 指路。.new/.old 残留故意不删（用户可手动救）。
	fail := func(msg string) {
		setPhase("failed", -1, msg)
		log.Printf("[update] 失败：%s", msg)
	}

	exe, err := os.Executable()
	if err != nil {
		fail("无法定位自身可执行文件: " + err.Error())
		return
	}
	if rp, err := filepath.EvalSymlinks(exe); err == nil {
		exe = rp
	}
	dir := filepath.Dir(exe)
	newFile := filepath.Join(dir, "ProxyDeck.exe.new")
	oldFile := filepath.Join(dir, "ProxyDeck.exe.old")

	log.Printf("[update] 开始下载 %s ← %s", latest, dlURL)
	// 30 分钟专用 client：exe 包体积大（GitHub → objects.githubusercontent 会 302，
	// http.Client 默认跟随重定向），1.5s/10s 的既有 client 都撑不住整包下载。
	dlClient := &http.Client{Timeout: 30 * time.Minute}
	resp, err := dlClient.Get(dlURL)
	if err != nil {
		fail("下载失败: " + err.Error())
		return
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		fail(fmt.Sprintf("下载失败: HTTP %d", resp.StatusCode))
		return
	}
	out, err := os.Create(newFile)
	if err != nil {
		fail("创建临时文件失败（" + newFile + "）: " + err.Error())
		return
	}
	var written int64
	total := resp.ContentLength // -1 = 服务器未给长度 → pct 恒 -1，前端显示不定进度
	buf := make([]byte, 256*1024)
	for {
		n, rerr := resp.Body.Read(buf)
		if n > 0 {
			if _, werr := out.Write(buf[:n]); werr != nil {
				out.Close()
				fail("写入临时文件失败（" + newFile + "）: " + werr.Error())
				return
			}
			written += int64(n)
			if total > 0 {
				pct := int(written * 100 / total)
				if pct > 100 {
					pct = 100
				}
				setPhase("downloading", pct, fmt.Sprintf("下载中 %d%%（%.1f MB）", pct, float64(written)/1048576))
			}
		}
		if rerr != nil {
			if rerr == io.EOF {
				break
			}
			out.Close()
			fail("下载中断: " + rerr.Error())
			return
		}
	}
	out.Close()
	// 下载完成校验：字节数必须等于 Releases 里登记的 size（有 size 才校验）。
	if dlSize > 0 && written != dlSize {
		fail(fmt.Sprintf("校验失败：下载 %d 字节，应为 %d 字节（文件留在 %s，可手动核查）", written, dlSize, newFile))
		return
	}
	log.Printf("[update] 下载完成 %.1f MB，校验通过", float64(written)/1048576)
	setPhase("downloaded", 100, "下载完成 · 校验通过")

	// 腾位法替换（Windows 允许 rename 运行中的 exe，本会话构建腾位实证多次）：
	// 1) 运行中的旧 exe 改名 .old 腾出原路径 → 2) .new 改名顶替原路径。
	if err := os.Rename(exe, oldFile); err != nil {
		// .old 残留挡路的两种报错都要接住（审查 P2 勘误）：Permission=残留被占用；
		// Windows 上 rename 到已存在目标最常报 ErrExist（ERROR_ALREADY_EXISTS）而非
		// Permission——只接后者会让「首轮失败→二次更新」必挂
		if (os.IsPermission(err) || os.IsExist(err)) && os.Remove(oldFile) == nil {
			err = os.Rename(exe, oldFile)
		}
		if err != nil {
			fail(fmt.Sprintf("替换失败（旧程序改名 .old 不成）: %v", err))
			return
		}
	}
	if err := os.Rename(newFile, exe); err != nil {
		fail(fmt.Sprintf("替换失败（新包就位不成）: %v（旧程序在 %s，新包在 %s，可手动放回）", err, oldFile, newFile))
		return
	}
	log.Printf("[update] 替换完成，拉起新进程并关闭旧窗口")

	// 重启：先拉起新 exe（无参数 = 主界面，与现状启动模式一致；工作目录 = exe 目录，
	// 保证 api-key.txt / stats.json 等数据文件定位不变），再关旧窗。
	cmd := hiddenCmd(exe)
	cmd.Dir = dir
	if err := cmd.Start(); err != nil {
		fail(fmt.Sprintf("新程序启动失败（文件已替换，可手动运行 %s）: %v", exe, err))
		return
	}
	log.Printf("[update] 新进程已启动 pid=%d，旧窗口即将关闭", cmd.Process.Pid)
	setPhase("replaced", 100, "已替换，正在重启…")
	// 关窗路径选择（自定理由在案）：不走 w.Dispatch(w.Destroy)——main.go 133 行审查记录：
	// Destroy 在 Run 消息循环里整窗销毁曾致 0xc0000005 崩溃，主 GUI 的 w.Destroy 挂在
	// defer（Run 返回后才执行）。故沿用 ccWindowCmd/ccFaxClose 的「关闭」同款语义：
	// PostMessage WM_CLOSE → Run() 干净返回 → defer w.Destroy() 自然执行 → 进程正常退出。
	// 兜底：窗口句柄拿不到时（理论上不该发生）os.Exit(0) 直接退出，新进程已在前一步起好。
	w.Dispatch(func() {
		if h := w.Window(); h != nil {
			windowCmd(uintptr(h), "close")
		} else {
			os.Exit(0)
		}
	})
}

// measureFirstToken 经本地代理发一个 1-token 最小推理请求，测首 token 时间（TTFB）。
// 这才是"模型卡不卡"的真实指标——不是网络 RTT，是实际开始吐 token 的快慢。
func measureFirstToken(baseURL, apiKey string) (int, error) {
	body := map[string]any{
		"model":    "deepseek/deepseek-v4-flash", // 用最便宜的模型测推理链路，省额度
		"messages": []map[string]any{{"role": "user", "content": "hi"}},
		"stream":   true, // 流式才能测 TTFB
	}
	b, _ := json.Marshal(body)
	req, err := http.NewRequest(http.MethodPost, baseURL+"/v1/chat/completions", bytes.NewReader(b))
	if err != nil {
		return 0, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "text/event-stream")
	if apiKey != "" {
		req.Header.Set("Authorization", "Bearer "+apiKey)
	}
	client := &http.Client{Timeout: 15 * time.Second} // 推理可能慢，给足超时
	start := time.Now()
	resp, err := client.Do(req)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return 0, fmt.Errorf("HTTP %d", resp.StatusCode)
	}
	// 读到第一个非空 SSE data 行 = 首 token 到达
	reader := bufio.NewReader(resp.Body)
	for {
		line, err := reader.ReadString('\n')
		if strings.HasPrefix(line, "data:") && strings.TrimSpace(strings.TrimPrefix(line, "data:")) != "" &&
			!strings.Contains(line, "[DONE]") {
			return int(time.Since(start).Milliseconds()), nil
		}
		if err != nil {
			return 0, err
		}
	}
}
