package qoder

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

// workerTimeout 单次请求总超时（含 worker 起动 + 完整 agent 任务；写码类任务可长达数分钟）。
const workerTimeout = 600 * time.Second

// cwdDir 解析 worker 的工作目录——跟随客户端会话（ZCode 等客户端的系统提示
// 自带 Working directory/工作目录 标记），agent 的文件读写/命令就落在
// 客户端同一个项目里，工具执行、路径解析与用户所见完全一致。
// 优先级：请求内提取 > env QODER_CWD（显式钉死）> 系统临时目录。
// 提取到的路径必须真实存在且是目录才算数（防提示词里随机路径误匹配）。
func cwdDir(prompt string) string {
	if p := extractCwdFromPrompt(prompt); p != "" {
		return p
	}
	if p := strings.TrimSpace(os.Getenv("QODER_CWD")); p != "" {
		if fi, err := os.Stat(p); err == nil && fi.IsDir() {
			return p
		}
	}
	return os.TempDir()
}

// cwdMarkers 客户端系统提示里常见的工作目录标记（ZCode/Claude Code 系均为此格式）。
var cwdMarkers = []string{
	"working directory", "current directory", "工作目录", "当前工作目录", "cwd",
}

// extractCwdFromPrompt 从扁平化 prompt 中提取客户端会话的工作目录。
func extractCwdFromPrompt(prompt string) string {
	lower := strings.ToLower(prompt)
	for _, marker := range cwdMarkers {
		idx := 0
		for {
			i := strings.Index(lower[idx:], marker)
			if i < 0 {
				break
			}
			start := idx + i + len(marker)
			seg := prompt[start:]
			// 跳过冒号/空白
			trimmed := strings.TrimLeft(seg, ":： \t\r\n")
			// 抓取盘符绝对路径（到行尾/引号/反引号为止）
			j := 0
			for j < len(trimmed) {
				c := trimmed[j]
				if c == '\r' || c == '\n' || c == '"' || c == '`' || c == '\'' {
					break
				}
				j++
			}
			cand := strings.TrimRight(trimmed[:j], " \t.,；;")
			if len(cand) >= 3 && cand[1] == ':' {
				if fi, err := os.Stat(cand); err == nil && fi.IsDir() {
					return cand
				}
			}
			idx = start + j
			if idx >= len(prompt) {
				break
			}
		}
	}
	return ""
}

// findWorker 探测官方 SDK worker（node 单文件）路径：
// env QODER_WORKER → D:\Program Files\Qoder\Qoder CN\... → %ProgramFiles%\Qoder\Qoder CN\...。
// 找不到返回 ""（调用方回 503 提示安装 Qoder）。
func findWorker() string {
	rel := filepath.Join("Qoder", "Qoder CN", "resources", "app.asar.unpacked",
		"node_modules", "@qoder-ai", "qoder-cn-agent-sdk", "dist", "_worker",
		"qoder-worker-runtime.obf.mjs")
	var candidates []string
	if p := strings.TrimSpace(os.Getenv("QODER_WORKER")); p != "" {
		candidates = append(candidates, p)
	}
	candidates = append(candidates, filepath.Join(`D:\Program Files`, rel))
	if pf := os.Getenv("ProgramFiles"); pf != "" {
		candidates = append(candidates, filepath.Join(pf, rel))
	}
	for _, c := range candidates {
		if fi, err := os.Stat(c); err == nil && !fi.IsDir() {
			return c
		}
	}
	return ""
}

// hideConsole 隐藏 node 子进程控制台窗口（CREATE_NO_WINDOW），写法与 comate/zulu.go 一致。
func hideConsole(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true, CreationFlags: 0x08000000}
}

// workerResult 是一次 worker 会话的收成。
type workerResult struct {
	Text  string   // 全部 assistant text 块拼接
	Final string   // result.result（官方最终全文）
	IsErr bool     // result.is_error
	Errs  []string // result.errors 失败原因数组

	// TotalCredits 是 result.total_credits：本次会话累计消耗的积分
	//（每请求 spawn 单个 worker，其值即本次请求的消耗，代理侧按它扣账本）。
	TotalCredits float64
}

// ctrlReq 是 worker stdout 里 control_request 的最小字段。
type ctrlReq struct {
	RequestID string `json:"request_id"`
	Request   struct {
		Subtype   string `json:"subtype"`
		ToolUseID string `json:"tool_use_id"`
	} `json:"request"`
}

// runWorker 跑一次 worker 会话（每请求 spawn，官方 App/SDK 同款，无常驻 serve）。
// 协议为 stdin/stdout 各一行一个 JSON（与 Claude Code stream-json 同构），时序：
//  1. 宿主 → control_request initialize，worker → control_response 应答；
//  2. 宿主 → {"type":"user",...} 用户消息；
//  3. worker → control_request fetch_job_token（payload hostTokenCallback=true 必发），
//     宿主回 dt- token（accessToken 直注会被上游拒收，jobToken 回调是唯一验证成功的方式）；
//  4. worker 输出 system/assistant 流，最后 result 收尾退出。
//
// onText 非 nil 时逐 text 块回调（流式翻译用）。
func runWorker(auth *qoderAuth, workerPath, model, prompt string, onText func(string)) (*workerResult, error) {
	// payload.json：官方 SDK spawn worker 时以 QODER_SDK_AUTH_PAYLOAD_FILE 指路。
	pf, err := os.CreateTemp("", "qoder-auth-*.json")
	if err != nil {
		return nil, fmt.Errorf("写 auth payload 失败: %w", err)
	}
	payloadPath := pf.Name()
	defer os.Remove(payloadPath)
	if _, err := pf.WriteString(`{"hostTokenCallback":true,"type":"jobToken","jobTokenProvider":"host"}`); err != nil {
		pf.Close()
		return nil, fmt.Errorf("写 auth payload 失败: %w", err)
	}
	pf.Close()

	ctx, cancel := context.WithTimeout(context.Background(), workerTimeout)
	defer cancel()
	// 参数对齐官方 App 的精简集（其 qoder-agent-sdk.log 实录）：掐掉内置技能装载与
	// 配置链扫描——worker 冷启慢的主因。--strict-mcp-config 掐外部 MCP 发现（断流元凶）。
	// 注意：官方另带 --bare（纯会话控制模式），实测会拒绝普通用户消息
	// （bare_session_control_only），代理要发用户对话，绝不能带。
	cmd := exec.CommandContext(ctx, "node", workerPath,
		"--print", "--output-format", "stream-json", "--input-format", "stream-json",
		"--model", model,
		"--disable-builtin-skills",
		"--setting-sources", "",
		"--strict-mcp-config")
	hideConsole(cmd)
	cmd.Dir = cwdDir(prompt)
	log.Printf("qoder-plugin: worker cwd=%s", cmd.Dir)
	env := make([]string, 0, len(os.Environ())+3)
	for _, e := range os.Environ() {
		if strings.HasPrefix(e, "NODE_OPTIONS=") {
			continue // 官方 SDK spawn 时清理 NODE_OPTIONS
		}
		env = append(env, e)
	}
	cmd.Env = append(env,
		"QODER_AGENT_SDK_ENTRYPOINT=sdk-ts",
		"QODER_AGENT_SDK_VERSION=1.0.27",
		"QODER_SDK_AUTH_PAYLOAD_FILE="+payloadPath,
	)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	stderrBuf := &bytes.Buffer{}
	cmd.Stderr = stderrBuf
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("启动 worker 失败: %w", err)
	}

	send := func(v any) error {
		b, err := json.Marshal(v)
		if err != nil {
			return err
		}
		_, err = stdin.Write(append(b, '\n'))
		return err
	}
	abort := func(cause error) (*workerResult, error) {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
		return nil, cause
	}
	// 应答格式解自官方 SDK dist/index.js：processControlRequest 的 fetch_job_token
	// 分支返回 {token, expires_at?}，经 xn() 包成
	// {"type":"control_response","response":{"subtype":"success","request_id":<id>,"response":{...}}}。
	// expires_at 须为 Unix 毫秒（<1e12 会被 SDK 判错）；失败路径为 subtype:"error"+error 字段。
	replyOK := func(req ctrlReq, resp map[string]any) {
		_ = send(map[string]any{
			"type": "control_response",
			"response": map[string]any{
				"subtype": "success", "request_id": req.RequestID, "response": resp,
			},
		})
	}

	if err := send(map[string]any{
		"type": "control_request", "request_id": "pd-init",
		"request": map[string]any{
			"type": "initialize", "subtype": "initialize",
			"modelPolicyProvider":            false,
			"supportsCatalogReadyInitialize": true,
			"initializeTimeoutMs":            120000,
		},
	}); err != nil {
		return abort(fmt.Errorf("写 initialize 失败: %w", err))
	}

	res := &workerResult{}
	inited, done := false, false
	sc := bufio.NewScanner(stdout)
	sc.Buffer(make([]byte, 64*1024), 8*1024*1024)
	for !done && sc.Scan() {
		var msg map[string]any
		if json.Unmarshal(sc.Bytes(), &msg) != nil {
			continue
		}
		switch msg["type"] {
		case "control_response":
			resp, _ := msg["response"].(map[string]any)
			if !inited && resp != nil && resp["request_id"] == "pd-init" {
				inited = true
				// 握手完成 → 喂用户消息（官方 SDK query 的 stream-json user 形态）
				if err := send(map[string]any{
					"type": "user",
					"message": map[string]any{
						"role":    "user",
						"content": []map[string]any{{"type": "text", "text": prompt}},
					},
				}); err != nil {
					return abort(fmt.Errorf("写用户消息失败: %w", err))
				}
			}
		case "control_request":
			var req ctrlReq
			if json.Unmarshal(sc.Bytes(), &req) != nil {
				continue
			}
			switch req.Request.Subtype {
			case "fetch_job_token":
				resp := map[string]any{"token": auth.Token}
				if !auth.ExpiresAt.IsZero() {
					resp["expires_at"] = auth.ExpiresAt.UnixMilli()
				}
				replyOK(req, resp)
			case "can_use_tool":
				// 全放行：与官方 App 的 bypassPermissions 同语义——工具由 worker 在本机
				// 正常执行（读/写/命令都允许），模型能力不被代理阉割。工作目录见 cwdDir()。
				replyOK(req, map[string]any{
					"behavior": "allow", "toolUseID": req.Request.ToolUseID,
				})
			default:
				_ = send(map[string]any{
					"type": "control_response",
					"response": map[string]any{
						"subtype": "error", "request_id": req.RequestID,
						"error": "unhandled control request: " + req.Request.Subtype,
					},
				})
			}
		case "assistant":
			m, _ := msg["message"].(map[string]any)
			content, _ := m["content"].([]any)
			for _, c := range content {
				cm, ok := c.(map[string]any)
				if !ok || cm["type"] != "text" {
					continue
				}
				if t, _ := cm["text"].(string); t != "" {
					res.Text += t
					if onText != nil {
						onText(t)
					}
				}
			}
		case "result":
			res.Final, _ = msg["result"].(string)
			res.IsErr, _ = msg["is_error"].(bool)
			if tc, ok := msg["total_credits"].(float64); ok {
				res.TotalCredits = tc
			}
			if errs, ok := msg["errors"].([]any); ok {
				for _, e := range errs {
					if s, ok := e.(string); ok {
						res.Errs = append(res.Errs, s)
					}
				}
			}
			done = true
		}
	}
	_ = stdin.Close()
	waitErr := cmd.Wait()
	if done {
		return res, nil
	}
	cause := "worker 结束但未收到 result"
	if ctx.Err() != nil {
		cause = fmt.Sprintf("worker 超时（%v）", workerTimeout)
	} else if serr := sc.Err(); serr != nil {
		cause = "worker stdout 中断: " + serr.Error()
	} else if waitErr != nil {
		cause = fmt.Sprintf("worker 退出异常: %v", waitErr)
	} else if !inited {
		cause = "worker 握手失败（未收到 initialize 应答）"
	}
	if tail := strings.TrimSpace(stderrBuf.String()); tail != "" {
		if len(tail) > 500 {
			tail = tail[len(tail)-500:]
		}
		cause += "; stderr: " + tail
	}
	return res, fmt.Errorf("%s", cause)
}
