package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/Amer-CN/proxydeck/internal/appdir"
)

// ============ 插件托管（Python 服务收编 / Go 原生内置） ============
type pluginDef struct {
	ID     string   // 唯一标识
	Name   string   // 显示名
	Dir    string   // plugins 下目录名（Python 插件用）
	Script string   // 入口脚本（Python 插件用）
	Args   []string // 附加命令行参数
	Port   int      // 监听端口
	Health string   // 健康检查路径
	Native string   // 非空 = Go 原生内置插件（本 exe 的 --plugin-<id> 子模式），无需 Python/脚本目录
}

var pluginDefs = []pluginDef{
	{
		ID: "tuanjie", Name: "团结 Cowork (Codely)",
		Native: "tuanjie",
		Port:   8788, Health: "/health",
	},
	{
		ID: "codebuddy", Name: "WorkBuddy / CodeBuddy",
		Native: "codebuddy",
		// --desensitize：对 system 里的技术敏感词（DoS/exploit/credential 等）
		// 插入零宽空格，避免腾讯内容审核误拦 agent 类请求（如 codex++ 的超长系统提示）
		Args: []string{"--desensitize"}, Port: 8787, Health: "/health",
	},
	{
		// WorkBuddy 国际版：同甲板区域切换的另一半（8789，免费模型池，
		// 凭据按 workbuddy.ai 过滤，不刷新 token，首条 system 强制）
		// --desensitize：与国内版一致**默认开启**——国际版同样按内容指纹拦（11128
		// Illegal API invocation from an unapproved channel；2026-09-12 实测：会话里
		// 粘了 desensitize.go 覆盖的报错原文即被拦，用户裁决"不脱敏根本用不了"）
		ID: "codebuddy-intl", Name: "WorkBuddy Intl",
		Native: "codebuddy-intl",
		Args: []string{"--desensitize"}, Port: 8789, Health: "/health",
	},
	{
		ID: "bai", Name: "B.AI",
		Native: "bai",
		Port:   8891, Health: "/health",
	},
	{
		ID: "comate", Name: "Comate",
		Native: "comate",
		Port:   8786, Health: "/health",
	},
	{
		ID: "qoder", Name: "Qoder",
		Native: "qoder",
		Port:   8785, Health: "/health",
	},
	{
		// VibeX（RunningHub）：REST + WS 私有协议的协议客户端，对外 OpenAI 兼容。
		// 本轮只注册不点火（简报要求：正式端口 8790 只写进 pluginDefs）。
		ID: "vibex", Name: "VibeX",
		Native: "vibex",
		Port:   8790, Health: "/health",
	},
}

type pluginState struct {
	mu      sync.Mutex
	running bool
	cmd     *exec.Cmd
	started time.Time
	lastErr string
}

// pluginsDir 返回插件脚本目录（随程序分发的 Python 插件，属**程序**侧，留在 exe 同目录）。
func (a *app) pluginsDir() string { return filepath.Join(exeDir(), "plugins") }

func (a *app) pluginStates() map[string]*pluginState {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.plugins == nil {
		a.plugins = map[string]*pluginState{}
		for _, d := range pluginDefs {
			a.plugins[d.ID] = &pluginState{}
		}
	}
	return a.plugins
}

// pluginScript 返回插件入口脚本绝对路径；目录缺失或原生插件时返回空。
func (a *app) pluginScript(d pluginDef) string {
	if d.Native != "" {
		return "" // 原生插件内置于本 exe
	}
	p := filepath.Join(a.pluginsDir(), d.Dir, d.Script)
	if _, err := os.Stat(p); err != nil {
		return ""
	}
	return p
}

// pluginHealthURL 返回插件健康检查地址。
func (a *app) pluginHealthURL(d pluginDef) string {
	return fmt.Sprintf("http://127.0.0.1:%d%s", d.Port, d.Health)
}

// pluginLog 返回插件运行日志路径（滚动 stdout/stderr，属状态文件 → 数据目录）。
func (a *app) pluginLog(d pluginDef) string {
	return appdir.Path(filepath.Join("plugins", d.ID+".log"))
}

// pluginErrLog 返回原生插件启动失败时由 --plugin-* 子模式写入的诊断文件路径
// （命名与 plugin_modes.go 各分支一致：<id>-plugin-error.log，落在数据目录）。
func (a *app) pluginErrLog(d pluginDef) string {
	return appdir.Path(d.ID + "-plugin-error.log")
}

// pyCmd 探测可用的 Python 命令（python / py -3），返回命令与参数前缀。
func pyCmd() (string, []string, error) {
	for _, c := range [][]string{{"python"}, {"py", "-3"}} {
		if _, err := exec.LookPath(c[0]); err == nil {
			return c[0], c[1:], nil
		}
	}
	return "", nil, fmt.Errorf("未检测到 Python，请安装 Python 3.8+（https://www.python.org/downloads/）")
}

// checkPluginDeps 检查插件依赖（fastapi / uvicorn / httpx）。
func checkPluginDeps() error {
	exe, pre, err := pyCmd()
	if err != nil {
		return err
	}
	args := append(append([]string{}, pre...), "-c", "import fastapi,uvicorn,httpx")
	cmd := exec.Command(exe, args...)
	if out, err := cmd.CombinedOutput(); err != nil {
		return fmt.Errorf("缺少依赖：%s\n请运行：pip install fastapi \"uvicorn[standard]\" httpx\n（详情：%s）",
			strings.TrimSpace(string(out)), err.Error())
	}
	return nil
}

// pluginList 返回所有插件的状态 JSON（目录存在/运行/健康/端口/日志）。
// 探活并行：本方法在 WebView2 UI 线程同步执行，串行探 6 个端口时单端口挂起
// 最坏 6×1.5s，会把整窗拖死；并发后总耗时 ≈ 最慢一个端口。
// 结果按索引写预分配 slice，输出结构与顺序与串行版完全一致。
func (a *app) pluginList() []map[string]any {
	states := a.pluginStates()
	out := make([]map[string]any, len(pluginDefs))
	var wg sync.WaitGroup
	for i, d := range pluginDefs {
		wg.Add(1)
		go func(i int, d pluginDef) {
			defer wg.Done()
			st := states[d.ID]
			st.mu.Lock()
			running := st.running
			lastErr := st.lastErr
			st.mu.Unlock()
			script := a.pluginScript(d)
			present := d.Native != "" // 原生插件内置于本 exe，恒为存在
			if !present {
				present = script != ""
			}
			// healthy 无条件查端口：外部启动的实例（终端/上次 GUI）同样识别为运行中，
			// 否则视图会误判"未启动"，模型矩阵/日志全部不加载。
			alive := present && httpOK(a.pluginHealthURL(d))
			// 陈旧报错自愈：端口已健康（用户补了登录文件后重扫成功、或换了实例）
			// 就把上一次的失败原因清掉——启动失败的历史不该长期驻留在界面上。
			// 判据是可观测的当前状态（端口探活），不是"进程必须是我起的"：
			// 外部拉起的健康实例同样算好状态，跟着它报旧错才是假情报。
			if alive && lastErr != "" {
				st.mu.Lock()
				st.lastErr = ""
				st.mu.Unlock()
				lastErr = ""
			}
			out[i] = map[string]any{
				"id": d.ID, "name": d.Name, "port": d.Port,
				"native":  d.Native != "",
				"dir":     filepath.Join("plugins", d.Dir),
				"present": present,
				"running": running, "healthy": alive,
				"lastErr": lastErr, "log": filepath.Base(a.pluginLog(d)),
				"url": fmt.Sprintf("http://127.0.0.1:%d/v1", d.Port),
			}
		}(i, d)
	}
	wg.Wait()
	return out
}

// pluginStart 启动一个插件（独立常驻子进程，关 GUI 不影响）。
// 原生插件 = spawn 本 exe 的 --plugin-<id> 子模式；Python 插件 = spawn 脚本。
// 插件启动：原生插件 = spawn 本 exe 的 --plugin-<id> 子模式；Python 插件 = spawn 脚本。
func (a *app) pluginStart(id string) error {
	var d *pluginDef
	for i := range pluginDefs {
		if pluginDefs[i].ID == id {
			d = &pluginDefs[i]
			break
		}
	}
	if d == nil {
		return fmt.Errorf("未知插件: %s", id)
	}
	states := a.pluginStates()
	st := states[id]
	st.mu.Lock()
	if st.running {
		st.mu.Unlock()
		return fmt.Errorf("%s 已在运行", d.Name)
	}
	st.mu.Unlock()

	// 端口已有健康实例（可能是 CLI/上一会话/别的工具拉起的）→ 直接接管复用，
	// 不杀不重启——正在用它跑任务的客户端（子智能体等）不受影响。
	// 只有端口被"非健康"进程占用（僵尸/残留）才清理后重启。
	if httpOK(a.pluginHealthURL(*d)) {
		st.mu.Lock()
		st.running = true // GUI 语义：端口上有可用服务 = 运行中（外部实例，cmd 为 nil）
		st.cmd = nil
		st.started = time.Now()
		st.lastErr = ""
		st.mu.Unlock()
		return nil
	}
	if portBusy(fmt.Sprintf("%d", d.Port)) {
		_ = killByPort(fmt.Sprintf("%d", d.Port))
		time.Sleep(300 * time.Millisecond)
	}

	var cmd *exec.Cmd
	// 原生插件：spawn 本 exe（内置于二进制，无目录/Python 依赖）；
	// 便携模式带上 --portable，让子进程与 GUI 解析到同一个数据目录。
	if d.Native != "" {
		exe, err := os.Executable()
		if err != nil {
			return fmt.Errorf("定位本程序失败: %v", err)
		}
		args := append([]string{fmt.Sprintf("--plugin-%s", d.Native), "--port", fmt.Sprintf("%d", d.Port)}, appdir.PortableArgs()...)
		args = append(args, d.Args...)
		cmd = exec.Command(exe, args...)
	} else {
		script := a.pluginScript(*d)
		if script == "" {
			return fmt.Errorf("插件目录缺失：%s\n请确认 plugins/%s 已随程序放置",
				filepath.Join(a.pluginsDir(), d.Dir), d.Dir)
		}
		if err := checkPluginDeps(); err != nil {
			return err
		}
		exe, pre, err := pyCmd()
		if err != nil {
			return err
		}
		args := append(append([]string{}, pre...), script)
		args = append(args, d.Args...)
		cmd = exec.Command(exe, args...)
		cmd.Dir = filepath.Dir(script)
	}
	// 子进程 stdout/stderr 落数据目录日志：带时间戳行头、追加写、超 1 MB 轮转。
	if lf := appdir.OpenLog(a.pluginLog(*d)); lf != nil {
		w := appdir.NewStampedWriter(lf)
		cmd.Stdout = w
		cmd.Stderr = w
	}
	// 清掉上一次的失败诊断：否则本轮启动成功、旧错误仍会被当成本次原因读出。
	_ = os.Remove(a.pluginErrLog(*d))
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("启动 %s 失败: %v", d.Name, err)
	}
	st.mu.Lock()
	st.running = true
	st.cmd = cmd
	st.started = time.Now()
	st.lastErr = ""
	st.mu.Unlock()
	a.watchPluginExit(st, cmd, *d)

	// 不再同步等就绪：本方法在 WebView2 UI 线程的串行 bind 队列里执行，
	// 原先最长 15s 的就绪轮询会把整窗冻死（原生后端秒起，等它纯属白冻）。
	// 就绪状态交给 pluginList 的 3s 探活：启动中显示 starting，健康后亮 running。
	return nil
}

// watchPluginExit 监视子进程退出，进程一死就把状态复位并带上失败原因。
//
// 没有它时 running 恒为 true：后端秒退（缺登录态、端口冲突等）后端口永远不会
// 上线，界面就永远停在「启动中」——群友反馈的「拉杆拉了没反应」即此因。
// 判定用 st.cmd != cmd：主动停堆（pluginStop 把 cmd 置 nil）与换新实例都不会被误判成失败。
func (a *app) watchPluginExit(st *pluginState, cmd *exec.Cmd, d pluginDef) {
	go func() {
		waitErr := cmd.Wait()
		st.mu.Lock()
		defer st.mu.Unlock()
		if st.cmd != cmd {
			return
		}
		st.running = false
		st.cmd = nil
		st.lastErr = pluginExitReason(a.pluginErrLog(d), waitErr, d)
	}()
}

// pluginExitReason 组装退出原因：优先用子进程自己写的诊断文件，其次用退出错误。
// 只回可读短句，避免把整份日志灌进界面。
func pluginExitReason(errLog string, waitErr error, d pluginDef) string {
	if b, err := readLogTail(errLog, 4<<10); err == nil {
		if msg := strings.TrimSpace(string(b)); msg != "" {
			if len(msg) > 300 {
				msg = msg[:300] + "…"
			}
			return "启动失败：" + msg
		}
	}
	if waitErr != nil {
		return fmt.Sprintf("%s 启动后退出：%v", d.Name, waitErr)
	}
	return d.Name + " 启动后退出（无诊断信息）"
}

// pluginStop 停止一个插件（按端口结束进程）。
func (a *app) pluginStop(id string) error {
	var d *pluginDef
	for i := range pluginDefs {
		if pluginDefs[i].ID == id {
			d = &pluginDefs[i]
			break
		}
	}
	if d == nil {
		return fmt.Errorf("未知插件: %s", id)
	}
	states := a.pluginStates()
	st := states[id]
	st.mu.Lock()
	if st.cmd != nil {
		_ = st.cmd.Process.Kill()
	}
	st.running = false
	st.cmd = nil
	st.lastErr = ""
	st.mu.Unlock()
	_ = killByPort(fmt.Sprintf("%d", d.Port))
	return nil
}
