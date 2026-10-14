//go:build windows

package main

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/Amer-CN/proxydeck/internal/appdir"
	"github.com/Amer-CN/proxydeck/internal/proxy"
	"github.com/atotto/clipboard"
	webview "github.com/webview/webview_go"
)

// app 是 GUI 与代理核心之间的控制器：代理直接运行在本进程内。
// 结构体定义见 app.go（公开版 / 完整版各一份：完整版多插件状态字段）。

func newApp(host, port, key string) *app {
	// 每次进程启动清一次腾位法留下的旧程序残骸（更新成功即删，见下方函数注释）。
	cleanupOldExe()

	a := &app{host: host, port: port, apiKey: key}
	if a.apiKey == "" {
		if b, err := appdir.ReadFile(a.keyFile()); err == nil {
			a.apiKey = strings.TrimSpace(string(b))
		}
	}
	return a
}

// 状态文件一律落 appdir.DataDir()（默认 %APPDATA%\ProxyDeck\，便携模式则 exe 同目录）：
// exe 目录视作只读，桌面/Program Files 里不再长文件。
func (a *app) keyFile() string       { return appdir.Path("api-key.txt") }
func (a *app) statsFile() string     { return appdir.Path("stats.json") }
func (a *app) noticeFile() string    { return appdir.Path("notice_dismissed.flag") }
func (a *app) closeHintFile() string { return appdir.Path("close_hint_dismissed.flag") }
func (a *app) closeModeFile() string { return appdir.Path("close_mode.txt") }
func (a *app) keyMapFile() string    { return appdir.Path("keymap.json") }
func (a *app) baseURL() string       { return "http://" + net.JoinHostPort(a.host, a.port) }
func (a *app) healthURL() string     { return a.baseURL() + "/health" }

func fileExists(p string) bool { _, err := os.Stat(p); return err == nil }

// closeMode 读盘上记住的关窗模式：tray / quit；文件缺席或内容不识别 = ask（每次询问）。
// 落盘先例同 notice_dismissed.flag（flag 式存在性判定的升级版：这里要存三态值）。
func (a *app) closeMode() string {
	b, err := os.ReadFile(a.closeModeFile())
	if err != nil {
		return "ask"
	}
	switch strings.TrimSpace(string(b)) {
	case "tray":
		return "tray"
	case "quit":
		return "quit"
	}
	return "ask"
}

// setCloseMode 记住关窗模式：ask = 删掉记忆文件（回到每次询问），tray/quit = 写盘。
func (a *app) setCloseMode(mode string) error {
	if mode == "ask" {
		if err := os.Remove(a.closeModeFile()); err != nil && !os.IsNotExist(err) {
			return err
		}
		return nil
	}
	if err := os.MkdirAll(filepath.Dir(a.closeModeFile()), 0o700); err != nil {
		return err
	}
	return os.WriteFile(a.closeModeFile(), []byte(mode), 0o600)
}

// keyMapDef 默认键位映射（与 app/ui.html 的 KEYMAP_DEF 同源：B.AI 默认不在任何键下）。
// 改默认映射时两处必须同步，否则首跑（无 keymap.json 时）前后端首屏不一致。
var keyMapDef = [][]string{{"cmd"}, {"tj", "comate"}, {"cb", "qoder"}, {"ext"}}

// keyMapFaceOK 脸名白名单（与 ui.html 的 SUP_FACES 同源）。
var keyMapFaceOK = map[string]bool{
	"cmd": true, "tj": true, "comate": true, "cb": true, "qoder": true, "bai": true, "ext": true,
}

// keyMapValid 键位映射形态校验（与 ui.html 的 keyMapValid 同口径）：
// 1-4 键、每键 ≥1 脸、脸名白名单、一页一键（无重复）。
// 前端子页写盘前已用同一口径验过，这里是后端权威复验（防手改文件与绕过）。
func keyMapValid(m [][]string) bool {
	if len(m) < 1 || len(m) > 4 {
		return false
	}
	seen := make(map[string]bool)
	for _, k := range m {
		if len(k) < 1 {
			return false
		}
		for _, f := range k {
			if !keyMapFaceOK[f] {
				return false
			}
			if seen[f] {
				return false
			}
			seen[f] = true
		}
	}
	return true
}

// keyMapGet 读盘：文件缺席/非法 → 返回默认映射 + saved=false
// （前端此时保持 localStorage 快路值，绝不把快路值回写成文件——
// 首跑无文件是正常态，不是错误）。
func (a *app) keyMapGet() ([][]string, bool) {
	b, err := os.ReadFile(a.keyMapFile())
	if err != nil {
		return keyMapDef, false
	}
	var m [][]string
	if err := json.Unmarshal(b, &m); err != nil || !keyMapValid(m) {
		return keyMapDef, false
	}
	return m, true
}

// keyMapSet 校验后落盘 0600（close_mode.txt 同口径）：raw 须是映射数组的 JSON。
func (a *app) keyMapSet(raw string) error {
	var m [][]string
	if err := json.Unmarshal([]byte(raw), &m); err != nil {
		return err
	}
	if !keyMapValid(m) {
		return errors.New("非法键位映射（1-4 键、每键 ≥1 页、页名须在白名单且一页一键）")
	}
	if err := os.MkdirAll(filepath.Dir(a.keyMapFile()), 0o700); err != nil {
		return err
	}
	return os.WriteFile(a.keyMapFile(), []byte(raw), 0o600)
}

// applyCloseMode 执行关窗动作：tray=藏进托盘（托盘图标挂不上就不藏——藏了没图标
// 等于把用户关在门外，错误如实回前端）；quit=先摘托盘图标再走原有关窗流程
// （WM_CLOSE → Run() 返回 → defer w.Destroy()），不留孤儿图标。
func (a *app) applyCloseMode(mode string, hwnd uintptr) string {
	switch mode {
	case "tray":
		if hwnd == 0 {
			return jsonErr(errors.New("窗口句柄不可用"))
		}
		if err := trayHide(hwnd); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已退到托盘")
	case "quit":
		trayQuit(hwnd)
		return jsonOK("正在退出")
	}
	return jsonOK("")
}

// cleanupOldExe 删掉更新替换（腾位法）留在 exe 同目录的旧程序残骸，启动时调用一次。
//
// 位置不写死：取 exeDir()（更新流程也按自身 exe 定位，两处同源）。Windows 上删除失败一律
// 不重试——旧进程仍持有 .old（旧窗口尚未关完）或被占用时删不掉，文件留着无害，下次
// 启动再清；失败不阻塞启动、不弹窗，只写日志。
//
// 2026-10-14：腾位名改为唯一（ProxyDeck.exe.old-<纳秒>）后，残骸不再只有一个固定名字，
// 故这里按通配清理（老版本的裸 .old 也在匹配范围内）。
func cleanupOldExe() {
	matches, _ := filepath.Glob(filepath.Join(exeDir(), "ProxyDeck.exe.old*"))
	for _, p := range matches {
		if err := os.Remove(p); err != nil {
			log.Printf("[update] 旧程序残骸删除失败（不影响使用，下次启动再清）：%s: %v", filepath.Base(p), err)
			continue
		}
		log.Printf("[update] 已清理旧程序残骸 %s", filepath.Base(p))
	}
}

// replaceRunningExe 用腾位法把运行中的 exe 换成 newFile（已下载并校验通过的新包），
// 返回腾位后的旧文件路径（供调用方写报错文案；失败时为空串）。
//
// 为什么旧名不能再写死（2026-10-14 群友实测「替换失败(旧程序改名 .old 不成): Access is
// denied」的根因）：写死 ProxyDeck.exe.old 时，只要上一次更新留下的 .old 仍在被运行
// ——旧窗口没关完、或后端子进程还跑在它身上——os.Remove 删不掉，改名到同名目标继续
// 撞 Access denied，于是「二次更新必挂」。改为每次取唯一名（带纳秒时间戳），残留名字
// 永不重复；外面再套 3 次退避重试，吃掉杀软/同步盘对文件的短暂占用。
func replaceRunningExe(exe, newFile string) (string, error) {
	dir := filepath.Dir(exe)
	var lastErr error
	for attempt := 0; attempt < 3; attempt++ {
		if attempt > 0 {
			time.Sleep(time.Duration(300*attempt) * time.Millisecond)
		}
		old := filepath.Join(dir, fmt.Sprintf("ProxyDeck.exe.old-%d", time.Now().UnixNano()))
		if err := os.Rename(exe, old); err == nil {
			return old, nil
		} else {
			lastErr = err
		}
	}
	return "", lastErr
}

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
	keySaved := true
	if key != "" {
		a.apiKey = key
		keySaved = appdir.WriteFile(a.keyFile(), []byte(key)) == nil
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
	cmd := hiddenCmd(exe, append([]string{"-headless"}, appdir.PortableArgs()...)...)
	/* key 已在上面落盘（api-key.txt），子进程未显式传 key 时会自行读取（见 main.go 的
	   runCoreHeadless）——正常路径不把密钥放上 argv：进程命令行对本机任意进程可读，
	   2026-09-11 排查时整行打印进程列表就把 key 打进过会话记录。仅落盘失败才退回 argv。 */
	if key != "" && !keySaved {
		cmd.Args = append(cmd.Args, "-api-key", key)
	}
	// headless 的 stdout/stderr 落盘（数据目录的 headless-error.log），后台异常时可查原因；
	// 日志带时间戳行头并按 1 MB × 3 轮转（否则长跑几小时就涨成几 MB 的单文件）。
	if lf := appdir.OpenLog(appdir.Path("headless-error.log")); lf != nil {
		w := appdir.NewStampedWriter(lf)
		cmd.Stdout = w
		cmd.Stderr = w
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
	if b, err := appdir.ReadFile(a.statsFile()); err == nil {
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
// updateCacheAt 记写入时刻：成功缓存 30 分钟过期重拉（原「永久有效」实测翻车——
// GUI 开着跨一次发版，本会话永远看到旧 latest，检测更新误报「已是最新」，
// 2026-09-14 用户实测踩中）；负缓存（限额失败）10 分钟过期重拉。
var (
	updateCacheMu   sync.Mutex
	updateCache     string
	updateCacheAt   time.Time
	updateRefreshCh = make(chan struct{}, 1) // 过期后由 bind 踢重拉（去重不叠加）
	updateLastFetch time.Time                // 最近一次踢拉取的时刻（断网限频 30s）
)

// tryKickUpdateRefresh 踢一次更新检查后台重拉（非阻塞，通道去重）。
// 本函数是踢取逻辑的可单测抽取：与 ccCheckUpdate 内的内联踢取块语义逐字一致
// （条件取反表述：`Since<=30s 丢弃` ≡ 原 `Since>30s 才踢`；自动路径保持一字不动，
// 故此处复述而非复用，改一处须对另一处）。
// 限频语义（只约束经由本函数的自动过期踢取）：
//   - 30s 踢取限频：距上次有效踢取不足 30s 时直接返回 false，不进通道、不产生拉取。
//     前端 checking 每 2s 重试一次，没有这道门，断网/慢网时每次重试都会排一次拉取
//     （通道去重只能合并"同时排队"的，拉取一完成、下一毫秒的重试又会排新拉取→风暴）。
//   - 手动 Force（checkUpdateForce）不走本函数：它清缓存后无条件踢取（时钟照样前移），
//     靠通道 cap=1 去重＋后台单 goroutine 串行单 fetch 防风暴。
//   - 通道 cap=1 去重：突发 N 次踢取至多 1 个排队；后台单 goroutine 串行消费，
//     同一时刻至多 1 个在途 + 1 个排队。
//   - 匿名 60/h 不被绕过：消耗配额的拉取（成功 200 / 403）在稳态下被成功缓存 30 分钟 /
//     负缓存 10 分钟挡住（Force 触发的那次拉取同样落缓存，同样受挡）。
//
// 返回 true = 本次踢取有效（已排队；若已有排队则合并，仍只一次有效拉取）；
// false = 被 30s 限频丢弃，零拉取。
func tryKickUpdateRefresh() bool {
	updateCacheMu.Lock()
	if time.Since(updateLastFetch) <= 30*time.Second {
		updateCacheMu.Unlock()
		return false
	}
	updateLastFetch = time.Now()
	updateCacheMu.Unlock()
	select {
	case updateRefreshCh <- struct{}{}:
	default: // 已有待处理拉取，合并，不叠加
	}
	return true
}

// checkUpdateForce 手动"立即检查"强制刷新的可单测内核：清缓存 + 无条件踢取，恒回 checking。
// 发版后 30 分钟保鲜期内，旧缓存仍是"最新=发版前版本"——普通 CheckUpdate 只读缓存会误报
// "已是最新"；Force 把缓存清掉，前端既有 checking 重试轮询（2s×4，仍走普通 CheckUpdate）
// 拿到后台 fresh 结果。自动检查路径（ccCheckUpdate 缓存读）不受影响。
// 注意：Force 无条件踢取，不走 30s 限频——GUI 启动自带一次拉取，30s 窗口内用户点手动检查时，
// 若沿用限频会清掉好缓存又踢不出新拉取，前端 4 次重试后误报"暂时无法连接"。
// 30s 限频只约束普通自动路径（tryKickUpdateRefresh 原样保留）；Force 的防风暴靠通道 cap=1
// 去重＋后台单 goroutine 串行单 fetch，突发连点至多 1 个排队，不叠加有效拉取。
func checkUpdateForce() string {
	updateCacheMu.Lock()
	updateCache = ""
	updateLastFetch = time.Now()
	updateCacheMu.Unlock()
	select {
	case updateRefreshCh <- struct{}{}:
	default: // 已有待处理拉取，合并，不叠加
	}
	return `{"ok":false,"msg":"checking"}`
}

// 一键更新流程状态：bind 只读写这几个字段并瞬时返回，真正的下载/替换在
// runOneKeyUpdateFlow 的 goroutine 里。updFlowActive 保证同一时间只跑一个流程。
var (
	updFlowMu     sync.Mutex
	updFlowActive atomic.Bool
	updPhase      = "idle" // idle | downloading | downloaded | replaced | failed
	updPct        = -1     // 0-100；-1 = 服务器没给 Content-Length，前端显示不定进度
	updMsg        = ""
)

// updateMirrorCandidates 一键更新下载镜像链（按序尝试，全部失败回退直连）。
// 公共镜像时有失效（2026-09-08 实测 mirror.ghproxy.com 已死），哪个挂了改这里。
// 2026-09-14 无梯子直连实测：gh-proxy.com 12.5MB/15s（sha256 与 Release 逐字节一致）；
// ghfast.top TCP 直连超时（连不上）——故 gh-proxy 置前，ghfast 留第二候选：
// 排后面只在第一个失效时才被碰到，死着也不拖累日常更新（死镜像要吃满 ~30s 拨号超时才切下一个）。
var updateMirrorCandidates = []string{"https://gh-proxy.com", "https://ghfast.top"}

// keyProbeCache 存储 key 探活结果缓存：bind 只读，真探在后台 goroutine（见 ccProbeKey）。
var (
	keyProbeMu     sync.Mutex
	keyProbeCached string
	keyProbeAt     time.Time
	keyProbeBusy   bool
)

// 官网用量缓存：bind 读缓存，后台 goroutine 只在前端触发（usageRefreshCh）时
// 刷新一次——拉杆启动/手动刷新各触发一次，不做周期轮询（官网 429 限流教训）。
var (
	usageCacheMu   sync.Mutex
	usageCache     string
	usageRefreshCh = make(chan struct{}, 1)
)

// httpClientLocal 打本地代理核心的写接口（校准等）：3s 足够，防止无超时挂死 bind 队列。
var httpClientLocal = &http.Client{Timeout: 3 * time.Second}

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
	// GUI 启动即常拉托盘线程：图标从进程起来就挂着（窗口可见时也在），进程退出前不摘。
	// 启动失败只写日志（见 trayStartup），界面照常可用。
	// 注水专线浮窗（--fax 子进程）不拉：它是独立小窗，给它挂图标 = 平白多一个托盘图标。
	if !*flagFax {
		if h := w.Window(); h != nil {
			trayStartup(uintptr(h))
		}
	}
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
		if b, err := appdir.ReadFile(a.statsFile()); err == nil && len(b) > 0 {
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
		// key 变了探活缓存立即失效：前端保存后会立刻重探，不能吃到旧 key 的状态
		keyProbeMu.Lock()
		keyProbeCached = ""
		keyProbeMu.Unlock()
		if key == "" {
			if err := appdir.Remove(a.keyFile()); err != nil {
				return jsonErr(err)
			}
			a.apiKey = ""
			return jsonOK("已清除保存的 Key")
		}
		if err := appdir.WriteFile(a.keyFile(), []byte(key)); err != nil {
			return jsonErr(err)
		}
		a.apiKey = key
		return jsonOK("API Key 已保存")
	})
	// 存储 key 探活（2026-09-13）：key 注入转发发生在 headless 子进程，主进程拿不到
	// 它的 401，故 GUI 进程自己持有 key 直打上游 whoami（免费端点，零生成计费）。
	// 只回状态字与状态码：不回 key、不回上游响应体，避免泄漏进前端日志。
	// 探测在后台 goroutine（ccUsage 同款范式）：ProbeKey 超时 10s，同步探会拖死
	// WebView2 的串行 bind 队列（UI 线程）——断网时 GUI 一启动就整窗冻结。
	// 结果缓存 25 分钟（略短于前端 30 分钟复查周期，保证每轮复查都真探）；
	// 换 key/清 key（ccSaveKey）立即失效。无可用结果时回 pending，前端 2s 后重查。
	_ = w.Bind("ccProbeKey", func() string {
		keyProbeMu.Lock()
		cached, at, busy := keyProbeCached, keyProbeAt, keyProbeBusy
		fresh := cached != "" && time.Since(at) < 25*time.Minute
		if !fresh && !busy {
			keyProbeBusy = true
		}
		keyProbeMu.Unlock()
		if fresh {
			return cached
		}
		if !busy {
			go func() {
				key := strings.TrimSpace(a.apiKey)
				if key == "" {
					if b, err := appdir.ReadFile(a.keyFile()); err == nil {
						key = strings.TrimSpace(string(b))
					}
				}
				out := map[string]any{"status": "nokey", "httpCode": 0}
				if key != "" {
					status, code, err := proxy.ProbeKey(proxy.DefaultBaseURL, key)
					out["status"], out["httpCode"] = status, code
					if err != nil {
						out["detail"] = "网络错误: " + err.Error()
					}
				}
				b, _ := json.Marshal(out)
				keyProbeMu.Lock()
				keyProbeCached, keyProbeAt, keyProbeBusy = string(b), time.Now(), false
				keyProbeMu.Unlock()
			}()
		}
		return `{"status":"pending"}`
	})
	// 版本更新检查 + 下载统计：查 GitHub Releases 全部版本，汇总所有 assets 的
	// 累计下载量（匿名：GitHub 官方计数，不含任何用户信息）。
	// 注意：不能只看 releases/latest——每次发布新版本 latest 会指向新版，
	// 旧版下载量就"消失"了，用户会误以为没人用。累计下载才是真实反馈。
	_ = w.Bind("ccCheckUpdate", func() string {
		// 只读缓存（瞬时返回，不阻塞 WebView2 的串行 bind 队列）；
		// 网络刷新由后台常驻 goroutine 异步完成（见下方 releases 拉取）。
		// 保鲜期：成功缓存 30 分钟、负缓存 10 分钟，过期即清并踢后台重拉。
		updateCacheMu.Lock()
		cached, cacheAt := updateCache, updateCacheAt
		ttl := 30 * time.Minute
		if strings.Contains(cached, `"ok":false`) {
			ttl = 10 * time.Minute
		}
		if cached != "" && time.Since(cacheAt) >= ttl {
			updateCache = ""
			cached = ""
		}
		updateCacheMu.Unlock()
		if cached != "" {
			return cached
		}
		// 首次/过期：踢后台重拉（通道去重；断网时限频 30s——前端 checking 每 2s
		// 重试，不至把 gh CLI/匿名 API 打成连发）。
		updateCacheMu.Lock()
		canKick := time.Since(updateLastFetch) > 30*time.Second
		if canKick {
			updateLastFetch = time.Now()
		}
		updateCacheMu.Unlock()
		if canKick {
			select {
			case updateRefreshCh <- struct{}{}:
			default:
			}
		}
		return `{"ok":false,"msg":"checking"}`
	})
	// 手动"立即检查"强制刷新：清缓存＋踢一次后台重拉＋回 checking（内核见 checkUpdateForce）。
	// 自动检查（ccCheckUpdate）与每小时整点复查（ui.html 10915 区）一律不走这里，行为零变化。
	_ = w.Bind("ccCheckUpdateForce", checkUpdateForce)
	// 后台异步拉取 GitHub releases（不占 bind 队列；成功写缓存供前端重试读取）。
	// 常驻循环：bind 发现缓存过期时踢一次（通道去重）——GUI 开着跨发版也能感知新版本。
	// 通道优先级（第 46 轮实测教训：匿名 API 限额按出口 IP 计，共享 NAT/代理出口
	// 极易 60/h 撞墙，更新检查整片失败）：
	//   ①本机 gh CLI 已登录（gh api 认证限额 5000/h）→ 用它拉
	//   ②gh 不存在/失败 → 匿名 HTTP 兜底；匿名 403 限额后负缓存 10 分钟，
	//     防手动检查+每小时复查反复烧同一出口的额度
	go func() {
		for range updateRefreshCh {
			const url = "https://api.github.com/repos/Amer-CN/proxydeck/releases?per_page=100"
			type asset struct {
				Name               string `json:"name"`
				Download           int    `json:"download_count"`
				BrowserDownloadURL string `json:"browser_download_url"`
				Size               int64  `json:"size"`
				Digest             string `json:"digest"` // GitHub API 自带，形如 sha256:<hex>（老 release 为空）
			}
			type rel struct {
				TagName string  `json:"tag_name"`
				HTMLURL string  `json:"html_url"`
				Assets  []asset `json:"assets"`
			}
			fetchBody := func() (string, int) {
				// ① gh CLI（已登录时限额 5000/h，共享出口不受匿名 60/h 连坐）
				if out, err := hiddenCmd("gh", "api", "repos/Amer-CN/proxydeck/releases?per_page=100",
					"--jq", ".").Output(); err == nil {
					return string(out), http.StatusOK
				}
				// ② 匿名 HTTP 兜底
				resp, err := httpClientSlow.Get(url)
				if err != nil {
					log.Printf("[update] releases 拉取失败: %v", err)
					return "", 0
				}
				defer resp.Body.Close()
				if resp.StatusCode != http.StatusOK {
					log.Printf("[update] releases 状态异常: %d", resp.StatusCode)
					return "", resp.StatusCode
				}
				b, _ := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
				return string(b), http.StatusOK
			}
			body, status := fetchBody()
			if status == http.StatusForbidden {
				// 匿名限额：负缓存 10 分钟（updateCache 填 not-ok JSON，前端「暂时无法连接」
				// 且不再重试；10 分钟由 cacheAt 判定过期重新拉）
				updateCacheMu.Lock()
				updateCache = `{"ok":false,"msg":"github rate limited"}`
				updateCacheAt = time.Now()
				updateCacheMu.Unlock()
				continue
			}
			if status != http.StatusOK {
				continue
			}
			var releases []rel
			if json.NewDecoder(strings.NewReader(body)).Decode(&releases) != nil || len(releases) == 0 {
				log.Printf("[update] releases 解析失败或为空")
				continue
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
			dlURL, dlSize, dlDigest := "", int64(0), ""
			for _, as := range latest.Assets {
				if as.Name == "ProxyDeck.exe" {
					dlURL, dlSize, dlDigest = as.BrowserDownloadURL, as.Size, as.Digest
					break
				}
			}
			b, _ := json.Marshal(map[string]any{
				"ok": true, "latest": latest.TagName, "url": latest.HTMLURL,
				"downloads": totalDownloads, // 所有版本累计下载量
				"dl_url":    dlURL,          // 新增：exe 直链（老前端忽略新字段，兼容）
				"dl_size":   dlSize,         // 新增：exe 字节数（下载后校验用；0 = 未知不校验）
				"dl_digest": dlDigest,       // 新增：asset digest（sha256:<hex>；空 = 老 release 无，沿用字节数校验）
			})
			updateCacheMu.Lock()
			updateCache = string(b)
			updateCacheAt = time.Now() // 成功缓存同样记时，30 分钟保鲜（原缺失=跨发版失明根因之一）
			updateCacheMu.Unlock()
		}
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
		// GUI 与代理是独立进程，这里直接调用代理的 /v1/calibration 接口。
		// 走 3s 超时本地客户端：http.PostForm 的默认客户端无超时，核心挂起时
		// bind（UI 线程）会被无限拖住
		resp, err := httpClientLocal.PostForm(a.baseURL()+"/v1/calibration",
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
	_ = w.Bind("ccDismiss", func() string {
		if err := appdir.WriteFile(a.noticeFile(), []byte("1")); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已记录")
	})
	_ = w.Bind("ccDismissCloseHint", func() string {
		if err := appdir.WriteFile(a.closeHintFile(), []byte("1")); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已记录")
	})
	// 自绘窗口控制（无边框窗口的标题栏交互）：move=台肩拖拽 / min / max / close。
	// 实现见 platform_windows.go 的 windowCmd。
	// close 走 trayQuit：先摘托盘图标再发 WM_CLOSE（彻底关机拨杆、红灯「直接退出」等
	// 所有经此桥的关窗都从这条走），不留孤儿图标。托盘没起过时 trayQuit 内部直接关窗。
	_ = w.Bind("ccWindowCmd", func(cmd string) string {
		h := w.Window()
		if h == nil {
			return jsonErr(errors.New("窗口句柄不可用"))
		}
		if strings.TrimSpace(cmd) == "close" {
			trayQuit(uintptr(h))
			return jsonOK("")
		}
		windowCmd(uintptr(h), strings.TrimSpace(cmd))
		return jsonOK("")
	})
	/* 关窗退到托盘三桥（红灯行为）。设计：模式裁决与执行都放 Go 侧，前端只负责
	   「ask 时弹窗」这一件事——红灯点击 → ccRequestClose 问模式：
	     ask  → 回 {"ok":true,"mode":"ask"}，前端用 confirmModal 款式弹二选一
	     tray/quit → 直接执行（藏窗 / 关窗），回 {"ok":true,"mode":...}，前端不动
	   ccCloseChoice 是弹窗里选完的回执：mode=实际动作，remember=true 才落盘
	   close_mode.txt（不勾选 = 只做这一次，下次照问）。 */
	_ = w.Bind("ccRequestClose", func() string {
		mode := a.closeMode()
		if mode == "ask" {
			return `{"ok":true,"mode":"ask"}`
		}
		h := w.Window()
		if h == nil {
			return jsonErr(errors.New("窗口句柄不可用"))
		}
		return a.applyCloseMode(mode, uintptr(h))
	})
	_ = w.Bind("ccCloseChoice", func(mode string, remember bool) string {
		mode = strings.TrimSpace(mode)
		if mode != "tray" && mode != "quit" {
			return jsonErr(errors.New("未知关窗模式: " + mode))
		}
		if remember {
			if err := a.setCloseMode(mode); err != nil {
				return jsonErr(err)
			}
		}
		h := w.Window()
		if h == nil {
			return jsonErr(errors.New("窗口句柄不可用"))
		}
		return a.applyCloseMode(mode, uintptr(h))
	})
	_ = w.Bind("ccCloseModeGet", func() string {
		b, _ := json.Marshal(map[string]any{"ok": true, "mode": a.closeMode()})
		return string(b)
	})
	_ = w.Bind("ccCloseModeSet", func(mode string) string {
		mode = strings.TrimSpace(mode)
		if mode != "ask" && mode != "tray" && mode != "quit" {
			return jsonErr(errors.New("未知关窗模式: " + mode))
		}
		if err := a.setCloseMode(mode); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已保存")
	})
	// 键位映射文件持久化（WebView2 无持久 profile，localStorage 随退出丢失，
	// 故映射以 keymap.json 为准：无文件/非法 → ok:false，前端保持快路值且不回写；
	// 每次改映射由前端透传，Go 侧权威复验后 0600 落盘）。
	_ = w.Bind("ccKeyMapGet", func() string {
		m, saved := a.keyMapGet()
		if !saved {
			return `{"ok":false,"msg":"no saved keymap"}`
		}
		b, _ := json.Marshal(map[string]any{"ok": true, "map": m})
		return string(b)
	})
	_ = w.Bind("ccKeyMapSet", func(raw string) string {
		if err := a.keyMapSet(strings.TrimSpace(raw)); err != nil {
			return jsonErr(err)
		}
		return jsonOK("已保存")
	})
}

/* ---------------- 一键更新流程（下载 → 校验 → 腾位替换 → 重启） ---------------- */

// applyMirror 给 GitHub 下载直链加镜像前缀：mirror 为空或 dlURL 不是 https://github.com/
// 直链时原样返回；否则 TrimSuffix(mirror,"/") + "/" + dlURL
// （形如 https://ghfast.top/https://github.com/Amer-CN/proxydeck/releases/download/vX.Y.Z/ProxyDeck.exe）。
func applyMirror(dlURL, mirror string) string {
	if mirror == "" || !strings.HasPrefix(dlURL, "https://github.com/") {
		return dlURL
	}
	return strings.TrimSuffix(mirror, "/") + "/" + dlURL
}

// fileSHA256 计算文件 sha256，返回小写十六进制摘要。
func fileSHA256(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

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
		Latest   string `json:"latest"`
		DlURL    string `json:"dl_url"`
		DlSize   int64  `json:"dl_size"`
		DlDigest string `json:"dl_digest"`
	}
	_ = json.Unmarshal([]byte(cached), &info)
	if info.DlURL == "" {
		updFlowActive.Store(false)
		return `{"ok":false,"msg":"当前版本信息没有 exe 直链（老 Release 无 ProxyDeck.exe asset），请到仓库下载页手动更新"}`
	}
	updFlowMu.Lock()
	updPhase, updPct, updMsg = "downloading", -1, "连接下载源…"
	updFlowMu.Unlock()
	go a.runOneKeyUpdateFlow(w, info.Latest, info.DlURL, info.DlSize, info.DlDigest)
	return `{"ok":true,"msg":"started"}`
}

// runOneKeyUpdateFlow 一键更新主流程（goroutine 内跑，与 bind 队列完全隔离）。
func (a *app) runOneKeyUpdateFlow(w webview.WebView, latest, dlURL string, dlSize int64, dlDigest string) {
	// 流程结束（无论成败）释放名额；失败后允许再次触发重试。
	defer updFlowActive.Store(false)
	setPhase := func(phase string, pct int, msg string) {
		updFlowMu.Lock()
		updPhase, updPct, updMsg = phase, pct, msg
		updFlowMu.Unlock()
	}
	// fail：phase=failed + msg 指路。下载半成品 .new 已随候选失败清理；
	// .old 残留故意不删（用户可手动救）。
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

	// 下载候选链：镜像常量按序在前、直连兜底最后一位（全部候选失败才 fail）。
	// 镜像/直连只进 log——phase 文案不含「镜像」字样，用户零感知。
	// 镜像尝试用 120s 专用超时：公共镜像挂起时（连接建立但不吐数据）不把用户
	// 吊死在单个候选上，超时即切下一个；直连沿用 30 分钟大超时（exe 包体积大，
	// GitHub → objects.githubusercontent 会 302，http.Client 默认跟随重定向，
	// 1.5s/10s 的既有 client 都撑不住整包下载）。
	dlClient := &http.Client{Timeout: 30 * time.Minute}
	mirrorClient := &http.Client{Timeout: 120 * time.Second}
	type dlCandidate struct {
		name   string // log 用的来源标记：镜像前缀本身 / direct
		url    string
		client *http.Client
	}
	candidates := make([]dlCandidate, 0, len(updateMirrorCandidates)+1)
	for _, m := range updateMirrorCandidates {
		// applyMirror 对非 GitHub 直链原样返回 → 与直连重复，不作为候选
		if u := applyMirror(dlURL, m); u != dlURL {
			candidates = append(candidates, dlCandidate{name: m, url: u, client: mirrorClient})
		}
	}
	candidates = append(candidates, dlCandidate{name: "direct", url: dlURL, client: dlClient})
	// downloadOne 用 client 从 u 下载整包写入 newFile：成功返回写入字节数；失败返回
	// 带原因的 fail 文案（沿用直连时代的报错措辞），由候选循环决定切下一个还是收尾。
	downloadOne := func(u string, client *http.Client) (int64, string) {
		resp, err := client.Get(u)
		if err != nil {
			return 0, "下载失败: " + err.Error()
		}
		defer resp.Body.Close()
		if resp.StatusCode != http.StatusOK {
			return 0, fmt.Sprintf("下载失败: HTTP %d", resp.StatusCode)
		}
		out, err := os.Create(newFile)
		if err != nil {
			return 0, "创建临时文件失败（" + newFile + "）: " + err.Error()
		}
		var got int64
		total := resp.ContentLength // -1 = 服务器未给长度 → pct 恒 -1，前端显示不定进度
		buf := make([]byte, 256*1024)
		for {
			n, rerr := resp.Body.Read(buf)
			if n > 0 {
				if _, werr := out.Write(buf[:n]); werr != nil {
					out.Close()
					return 0, "写入临时文件失败（" + newFile + "）: " + werr.Error()
				}
				got += int64(n)
				if total > 0 {
					pct := int(got * 100 / total)
					if pct > 100 {
						pct = 100
					}
					setPhase("downloading", pct, fmt.Sprintf("下载中 %d%%（%.1f MB）", pct, float64(got)/1048576))
				}
			}
			if rerr != nil {
				if rerr == io.EOF {
					break
				}
				out.Close()
				return 0, "下载中断: " + rerr.Error()
			}
		}
		out.Close()
		return got, ""
	}

	// 每个候选完整走：下载 → 字节数校验（有 dlSize 时）→ sha256 校验（digest 带
	// sha256: 前缀时），全过即成功跳出；任一环失败（transport/非 200/size 不符/
	// sha256 不符）：log 记录来源与原因、删半成品 .new、切下一个候选。
	var written int64
	var lastErr string
	for _, c := range candidates {
		log.Printf("[update] 开始下载 %s ← %s（来源 %s）", latest, c.url, c.name)
		setPhase("downloading", -1, "连接下载源…")
		written, lastErr = downloadOne(c.url, c.client)
		if lastErr == "" && dlSize > 0 && written != dlSize {
			// 下载完成校验：字节数必须等于 Releases 里登记的 size（有 size 才校验）。
			lastErr = fmt.Sprintf("校验失败：下载 %d 字节，应为 %d 字节", written, dlSize)
		}
		if lastErr == "" && strings.HasPrefix(dlDigest, "sha256:") {
			// sha256 校验（加固）：GitHub asset 自带 digest（形如 sha256:<hex>）时，
			// 下载文件必须逐字节对得上——不符即疑似下载源被篡改。
			sum, err := fileSHA256(newFile)
			if err != nil {
				lastErr = "sha256 校验失败（读取 " + newFile + "）: " + err.Error()
			} else if !strings.EqualFold("sha256:"+sum, dlDigest) {
				lastErr = fmt.Sprintf("sha256 不符，疑似源被篡改：应为 %s，实为 sha256:%s", dlDigest, sum)
			} else {
				log.Printf("[update] sha256 校验通过：%s", dlDigest)
			}
		}
		if lastErr == "" {
			break
		}
		log.Printf("[update] 下载源 %s 失败：%s；尝试下一个候选", c.name, lastErr)
		_ = os.Remove(newFile) // 清掉半成品，避免下一个候选的 os.Create 混入残留
	}
	if lastErr != "" {
		fail(lastErr) // 全部候选失败：用最后一个候选（直连）的错误收尾
		return
	}
	// digest 非空但非 sha256 前缀（如历史 md5）→ 跳过比对，沿用字节数校验，行为不变。
	if dlDigest != "" && !strings.HasPrefix(dlDigest, "sha256:") {
		log.Printf("[update] 跳过 digest 校验（非 sha256 格式: %s），沿用字节数校验", dlDigest)
	}

	log.Printf("[update] 下载完成 %.1f MB，校验通过", float64(written)/1048576)
	setPhase("downloaded", 100, "下载完成 · 校验通过")

	// 腾位法替换（Windows 允许 rename 运行中的 exe，本会话构建腾位实证多次）：
	// 1) 运行中的旧 exe 改成唯一名腾出原路径 → 2) .new 改名顶替原路径。
	oldFile, rerr := replaceRunningExe(exe, newFile)
	if rerr != nil {
		fail(fmt.Sprintf("替换失败（旧程序改名腾位不成）: %v。常见原因：上一次更新留下的旧实例或后端进程仍在运行（含托盘里的图标），请把它们全部退出后重试", rerr))
		return
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
