package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
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
		ID: "notion", Name: "Notion AI",
		Native: "notion",
		Port:   8789, Health: "/health",
	},
	{
		ID: "lingxi", Name: "WPS 灵犀",
		Native: "lingxi",
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

// pluginLog 返回插件日志路径。
func (a *app) pluginLog(d pluginDef) string {
	return filepath.Join(a.pluginsDir(), d.ID+".log")
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
func (a *app) pluginList() []map[string]any {
	states := a.pluginStates()
	out := make([]map[string]any, 0, len(pluginDefs))
	for _, d := range pluginDefs {
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
		out = append(out, map[string]any{
			"id": d.ID, "name": d.Name, "port": d.Port,
			"native":  d.Native != "",
			"dir":     filepath.Join("plugins", d.Dir),
			"present": present,
			"running": running, "healthy": alive,
			"lastErr": lastErr, "log": filepath.Base(a.pluginLog(d)),
			"url": fmt.Sprintf("http://127.0.0.1:%d/v1", d.Port),
		})
	}
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
	if d.Native != "" {
		// 原生插件：spawn 本 exe（内置于二进制，无目录/Python 依赖）
		exe, err := os.Executable()
		if err != nil {
			return fmt.Errorf("定位本程序失败: %v", err)
		}
		args := []string{fmt.Sprintf("--plugin-%s", d.Native), "--port", fmt.Sprintf("%d", d.Port)}
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
	if lf, err := os.OpenFile(a.pluginLog(*d), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o644); err == nil {
		cmd.Stdout = lf
		cmd.Stderr = lf
	}
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("启动 %s 失败: %v", d.Name, err)
	}
	st.mu.Lock()
	st.running = true
	st.cmd = cmd
	st.started = time.Now()
	st.lastErr = ""
	st.mu.Unlock()

	// 等待就绪（原生秒起；uvicorn 较慢，给足 15s）
	deadline := time.Now().Add(15 * time.Second)
	for time.Now().Before(deadline) {
		if httpOK(a.pluginHealthURL(*d)) {
			return nil
		}
		time.Sleep(300 * time.Millisecond)
	}
	// 启动超时不算失败：可能健康检查路径不同，留给面板状态灯判断
	return nil
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
