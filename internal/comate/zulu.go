// Package comate —— 百度 Comate（文心快码）第五平台甲板。
//
// 传输层：托管 zulu serve 子进程（内部固定 8792 端口），对外暴露本地
// OpenAI 兼容接口 http://127.0.0.1:8786/v1，客户端（Codex/Cursor 等）直接接入。
package comate

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// zuluPort 内部 zulu serve 固定端口（对外 OpenAI 接口 8786，内部 8792，
// 避开 8687 IDE 默认、8791 被占）。
const zuluPort = "8792"

// licenseFile 返回 Comate 登录态文件路径。
func licenseFile() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".comate", "cli-login.json")
}

// readLicense 读取 Comate 登录态 license；文件缺失或 license 为空返回 ""。
func readLicense() string {
	b, err := os.ReadFile(licenseFile())
	if err != nil {
		return ""
	}
	var m struct {
		License string `json:"license"`
	}
	if json.Unmarshal(b, &m) != nil {
		return ""
	}
	return strings.TrimSpace(m.License)
}

// findZulu 探测 zulu CLI 路径（顺序）：
// COMATE_ZULU 环境变量（直接指向 zulu.cmd 或 zulu 脚本）→
// %ProgramFiles(x86)%\Comate\... → %ProgramFiles%\Comate\... →
// D:\Program Files (x86)\Comate\...。找不到返回 ""。
func findZulu() string {
	if p := strings.TrimSpace(os.Getenv("COMATE_ZULU")); p != "" {
		if fi, err := os.Stat(p); err == nil && !fi.IsDir() {
			return p
		}
	}
	rel := filepath.Join("Comate", "resources", "app", "extensions",
		"baiducomate.comate", "dist", "zulu-cli", "bin", "zulu.cmd")
	candidates := []string{}
	for _, env := range []string{"ProgramFiles(x86)", "ProgramFiles"} {
		if pf := os.Getenv(env); pf != "" {
			candidates = append(candidates, filepath.Join(pf, rel))
		}
	}
	candidates = append(candidates, filepath.Join(`D:\Program Files (x86)`, rel))
	for _, c := range candidates {
		if fi, err := os.Stat(c); err == nil && !fi.IsDir() {
			return c
		}
	}
	return ""
}

// zuluHealth 检查内部 zulu serve 是否已就绪。
func zuluHealth() bool {
	client := &http.Client{Timeout: 2 * time.Second}
	resp, err := client.Get("http://127.0.0.1:" + zuluPort + "/health")
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	return resp.StatusCode == http.StatusOK
}

// zuluProc 跟踪 zulu serve 子进程。spawned=false 表示复用外部/遗留实例，不负责 kill。
type zuluProc struct {
	cmd     *exec.Cmd
	spawned bool
}

// ensureZuluServe 保证内部 zulu serve 在线：
// 已健康则复用（不 spawn）；否则 spawn `cmd /c <zulu.cmd> serve --port 8792 --host 127.0.0.1 -l <license>`
// 并轮询 health 最多 20s（间隔 500ms）。
func ensureZuluServe(zuluPath, license string) (*zuluProc, error) {
	if zuluHealth() {
		return &zuluProc{spawned: false}, nil
	}
	cmd := exec.Command("cmd", "/c", zuluPath, "serve",
		"--port", zuluPort, "--host", "127.0.0.1", "-l", license)
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("启动 zulu serve 失败: %v", err)
	}
	deadline := time.Now().Add(20 * time.Second)
	for time.Now().Before(deadline) {
		if zuluHealth() {
			return &zuluProc{cmd: cmd, spawned: true}, nil
		}
		time.Sleep(500 * time.Millisecond)
	}
	_ = cmd.Process.Kill()
	return nil, fmt.Errorf("zulu serve 启动超时（20 秒未就绪）")
}

// stop 结束自己 spawn 的 zulu serve；复用的实例不 kill。
func (z *zuluProc) stop() {
	if z != nil && z.spawned && z.cmd != nil && z.cmd.Process != nil {
		_ = z.cmd.Process.Kill()
	}
}

// modelInfo 是 zulu list-model 输出的一条。
type modelInfo struct {
	ModelID     string `json:"modelId"`
	DisplayName string `json:"displayName"`
}

// listModels 调用 `cmd /c <zulu.cmd> list-model -l <license>` 并解析 JSON 数组。
func listModels(zuluPath, license string) ([]modelInfo, error) {
	cmd := exec.Command("cmd", "/c", zuluPath, "list-model", "-l", license)
	out, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("list-model 失败: %v", err)
	}
	var list []modelInfo
	if err := json.Unmarshal(bytes.TrimSpace(out), &list); err != nil {
		return nil, fmt.Errorf("list-model 输出解析失败: %v", err)
	}
	return list, nil
}
