// server.go —— CodeBuddy 本地 OpenAI 兼容服务（Go 重写）。
//
// 端点：
//   GET  /health                诊断（auth 文件/账号/token 状态）
//   GET  /quota                 官方积分快照（get-user-resource，5min 缓存）
//   GET  /v1/models             动态探测的可用模型（1h 缓存）
//   GET  /model/info            模型元数据（Agent 填写指南：上下文/最大输出/思考级别）
//   POST /v1/chat/completions   OpenAI Chat（流式透传 / 非流式聚合）
package codebuddy

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// Server 是 CodeBuddy 转发的本地服务。
type Server struct {
	cred        *Credential
	desensitize bool
	client      *http.Client
	ln          net.Listener
	mu          sync.Mutex
	srv         *http.Server

	probeMu   sync.Mutex
	probeAt   time.Time
	probeList []string

	startedAt  time.Time // 运行时长展示
	statsMu    sync.Mutex
	stats      map[string]*modelStat // 按模型累计（GUI 消耗 TOP）
	statsPath  string
}

// modelStat 单模型用量累计。
type modelStat struct {
	Calls      int64 `json:"calls"`
	InputTok   int64 `json:"inputTokens"`
	OutputTok  int64 `json:"outputTokens"`
	TotalTok   int64 `json:"totalTokens"`
}

// corsWith 给所有响应加 CORS 头：GUI 页面跑在 localhost:随机端口，
// fetch 127.0.0.1:8787 属跨域，没有这个头前端全部拉不到数据。
func corsWith(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		h.ServeHTTP(w, r)
	})
}

// NewServer 创建服务。desensitize=true 时对 system/developer/tools 做零宽脱敏。
func NewServer(desensitize bool) (*Server, error) {
	cred := NewCredential("")
	if cred.Path() == "" {
		return nil, fmt.Errorf("未找到 CodeBuddy/WorkBuddy 登录文件，请先在桌面端完成登录")
	}
	s := &Server{
		cred:        cred,
		desensitize: desensitize,
		client:      &http.Client{Timeout: 0}, // 流式长连接不设总超时
		stats:       map[string]*modelStat{},
		startedAt:   time.Now(),
	}
	if exe, err := os.Executable(); err == nil {
		s.statsPath = filepath.Join(filepath.Dir(exe), "codebuddy-stats.json")
		s.loadStats()
	}
	return s, nil
}

// Start 在 host:port 监听（阻塞）。
func (s *Server) Start(host, port string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/quota", s.handleQuota)
	mux.HandleFunc("/v1/models", s.handleModels)
	mux.HandleFunc("/model/info", s.handleModelInfo)
	mux.HandleFunc("/v1/stats", s.handleStats)
	mux.HandleFunc("/v1/chat/completions", s.handleChat)

	ln, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return fmt.Errorf("端口 %s 被占用: %w", port, err)
	}
	s.mu.Lock()
	s.ln = ln
	s.startedAt = time.Now()
	s.mu.Unlock()
	srv := &http.Server{Handler: corsWith(mux)}
	s.mu.Lock()
	s.srv = srv
	s.mu.Unlock()
	return srv.Serve(ln)
}

// Stop 停止服务。
func (s *Server) Stop() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.srv != nil {
		_ = s.srv.Close()
	}
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	resp := map[string]any{
		"status":  "ok",
		"service": "codebuddy-go",
		"backend": backendBase,
		"mode":    "direct-proxy (native function calling)",
	}
	if !s.startedAt.IsZero() {
		resp["uptimeSec"] = int64(time.Since(s.startedAt).Seconds())
	}
	if sum := s.cred.Summary(); sum != nil {
		if err, ok := sum["error"]; ok {
			resp["status"] = "error"
			resp["message"] = fmt.Sprintf("%v", err)
		} else {
			resp["credential"] = sum
		}
	}
	writeJSON(w, resp)
}

// handleQuota 返回官方积分快照（GUI 积分卡用）。refresh=1 强制实时（手动刷新按钮）。
func (s *Server) handleQuota(w http.ResponseWriter, r *http.Request) {
	if r.URL.Query().Get("refresh") == "1" {
		writeJSON(w, s.FetchQuotaForce(r.Context()))
		return
	}
	writeJSON(w, s.FetchQuota(r.Context()))
}

// handleStats 返回消耗统计 + 运行时长（GUI 视图用）。
func (s *Server) handleStats(w http.ResponseWriter, r *http.Request) {
	s.statsMu.Lock()
	out := make(map[string]*modelStat, len(s.stats))
	for k, v := range s.stats {
		cp := *v
		out[k] = &cp
	}
	s.statsMu.Unlock()
	writeJSON(w, map[string]any{
		"models":    out,
		"uptimeSec": int64(time.Since(s.startedAt).Seconds()),
	})
}

// addStat 累计一次调用的用量并落盘。
func (s *Server) addStat(model string, in, out, total int64) {
	if total == 0 && in == 0 && out == 0 {
		return
	}
	s.statsMu.Lock()
	st := s.stats[model]
	if st == nil {
		st = &modelStat{}
		s.stats[model] = st
	}
	st.Calls++
	st.InputTok += in
	st.OutputTok += out
	if total > 0 {
		st.TotalTok += total
	} else {
		st.TotalTok += in + out
	}
	s.statsMu.Unlock()
	s.saveStats()
	log.Printf("[codebuddy] stat model=%s in=%d out=%d", model, in, out)
}

// loadStats 启动时读回历史统计。
func (s *Server) loadStats() {
	if s.statsPath == "" {
		return
	}
	b, err := os.ReadFile(s.statsPath)
	if err != nil {
		return
	}
	var m map[string]*modelStat
	if json.Unmarshal(b, &m) == nil {
		s.stats = m
	}
}

// saveStats 原子落盘。
func (s *Server) saveStats() {
	if s.statsPath == "" {
		return
	}
	s.statsMu.Lock()
	b, err := json.Marshal(s.stats)
	s.statsMu.Unlock()
	if err != nil {
		return
	}
	tmp := s.statsPath + ".tmp"
	if os.WriteFile(tmp, b, 0o644) == nil {
		_ = os.Rename(tmp, s.statsPath)
	}
}

// ============ 模型列表（动态探测，后端无模型列表端点） ============

// modelCandidates 已知候选池（探测后只返回真实可用的）。上游无模型列表
// 端点（2026-08-28 实测 /models、/v1/models、/v2/models 均 404），名单外
// 模型永远不会进矩阵。
// 名单对齐官方客户端模型列表（2026-08-30 用户实测截图）：Hy4 preview 免费档
// 与付费档同 id；客户端「Kimi-K2.7-Code」即 id kimi-k2.7（kimi-k2.7-code
// 实测 400 11102 不存在）；glm-5.0 / glm-4.7 已被上游下线（400）。不在官方
// 列表的历史候选（kimi-k2.5 / deepseek-v3.2 系 / minimax-m2.7 等）部分仍
// 可用，但按官方口径不进矩阵。
var modelCandidates = []string{
	"auto",
	"hy4-preview", "hy3",
	"glm-5.3", "glm-5.3-flash", "glm-5.2", "glm-5.1", "glm-5v-turbo",
	"minimax-m3",
	"kimi-k3", "kimi-k2.7", "kimi-k2.6",
	"deepseek-v4-flash", "deepseek-v4-pro",
}

// Models 返回真实可用模型（并行最小请求探测，1h 缓存）。
func (s *Server) Models(ctx context.Context) []string {
	s.probeMu.Lock()
	fresh := time.Since(s.probeAt) < time.Hour && len(s.probeList) > 0
	cached := s.probeList
	s.probeMu.Unlock()
	if fresh {
		return cached
	}

	hdr, err := s.cred.Headers(ctx)
	if err == nil {
		var wg sync.WaitGroup
		var mu sync.Mutex
		okSet := map[string]bool{}
		sem := make(chan struct{}, 10)
		for _, m := range modelCandidates {
			wg.Add(1)
			go func(model string) {
				defer wg.Done()
				sem <- struct{}{}
				defer func() { <-sem }()
				if s.probeOne(ctx, hdr, model) {
					mu.Lock()
					okSet[model] = true
					mu.Unlock()
				}
			}(m)
		}
		wg.Wait()
		if len(okSet) > 0 {
			list := make([]string, 0, len(okSet))
			for _, m := range modelCandidates { // 按候选池顺序输出（稳定）
				if okSet[m] {
					list = append(list, m)
				}
			}
			s.probeMu.Lock()
			s.probeList = list
			s.probeAt = time.Now()
			s.probeMu.Unlock()
			log.Printf("[codebuddy] 模型探测完成: %d/%d 可用", len(list), len(modelCandidates))
			return list
		}
	}
	// 全部失败（网络异常）→ 回退候选池，保证列表不空
	s.probeMu.Lock()
	s.probeList = append([]string{}, modelCandidates...)
	s.probeAt = time.Now()
	s.probeMu.Unlock()
	return modelCandidates
}

// probeOne 发最小流式请求探测模型是否可用（读到数据即认为后端接受该模型名）。
func (s *Server) probeOne(ctx context.Context, hdr http.Header, model string) bool {
	pctx, cancel := context.WithTimeout(ctx, 25*time.Second)
	defer cancel()
	body, _ := json.Marshal(map[string]any{
		"model": model, "stream": true,
		"messages": []map[string]string{{"role": "user", "content": "hi"}},
	})
	req, err := http.NewRequestWithContext(pctx, http.MethodPost,
		backendBase+"/v2/chat/completions", bytes.NewReader(body))
	if err != nil {
		return false
	}
	req.Header = cloneHeader(hdr)
	resp, err := s.client.Do(req)
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return false
	}
	buf := make([]byte, 64)
	if _, err := resp.Body.Read(buf); err != nil && err.Error() == "EOF" {
		return false
	}
	return true
}

func cloneHeader(h http.Header) http.Header {
	n := http.Header{}
	for k, vs := range h {
		for _, v := range vs {
			n.Add(k, v)
		}
	}
	return n
}

func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	models := s.Models(r.Context())
	data := make([]map[string]any, 0, len(models))
	for _, m := range models {
		data = append(data, map[string]any{
			"id": m, "object": "model", "created": 1700000000, "owned_by": "codebuddy",
		})
	}
	writeJSON(w, map[string]any{"object": "list", "data": data})
}

// ============ /model/info（Agent 填写指南） ============

// modelMeta 腾讯后端无元数据端点，以下为实测整理（与 Python 版一致）。
var modelMeta = map[string]map[string]any{
	"glm-5.3":             {"maxInput": 131072, "maxOutput": 32768, "reasoning": "low/medium/high", "note": "新模型"},
	"glm-5.2":             {"maxInput": 1048576, "maxOutput": 131072, "reasoning": "low/medium/high", "note": "上下文实测 1M"},
	"glm-5.1":             {"maxInput": 131072, "maxOutput": 32768, "reasoning": "low/medium/high"},
	"glm-5v-turbo":        {"maxInput": 131072, "maxOutput": 16384, "reasoning": "off", "note": "视觉模型"},
	"kimi-k3":             {"maxInput": 262144, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"kimi-k2.7":           {"maxInput": 262144, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"kimi-k2.6":           {"maxInput": 262144, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"kimi-k2.5":           {"maxInput": 262144, "maxOutput": 32768, "reasoning": "low/medium/high"},
	"deepseek-v4-pro":     {"maxInput": 1048576, "maxOutput": 65536, "reasoning": "low/medium/high", "note": "上下文实测 1M"},
	"deepseek-v4-flash":   {"maxInput": 1048576, "maxOutput": 65536, "reasoning": "low/medium/high", "note": "思考深：思考消耗输出预算，max_tokens 建议 ≥8000"},
	"deepseek-v3.2":       {"maxInput": 131072, "maxOutput": 32768, "reasoning": "low/medium/high"},
	"minimax-m3-pay":      {"maxInput": 1048576, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"minimax-m3":          {"maxInput": 1048576, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"hy3-preview-agent":   {"maxInput": 262144, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"hy3":                 {"maxInput": 262144, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"hy3-preview":         {"maxInput": 262144, "maxOutput": 65536, "reasoning": "low/medium/high"},
	"auto":                {"maxInput": 1048576, "maxOutput": 65536, "reasoning": "low/medium/high", "note": "后端自动路由到合适模型"},
}

func (s *Server) handleModelInfo(w http.ResponseWriter, r *http.Request) {
	out := make([]map[string]any, 0, len(modelCandidates))
	for _, m := range s.Models(r.Context()) {
		meta := modelMeta[m]
		if meta == nil {
			meta = map[string]any{"reasoning": "low/medium/high"}
		}
		entry := map[string]any{"name": m}
		for k, v := range meta {
			entry[k] = v
		}
		if entry["note"] == nil {
			entry["note"] = ""
		}
		out = append(out, entry)
	}
	writeJSON(w, out)
}

// ============ /v1/chat/completions ============

// passthroughBodyKeys 后端接受的标准字段（与 Python 版一致）。
var passthroughBodyKeys = []string{
	"model", "messages", "tools", "tool_choice", "temperature",
	"max_tokens", "max_completion_tokens", "top_p", "stream",
	"stream_options", "stop", "presence_penalty", "frequency_penalty",
	"n", "response_format", "seed", "user", "reasoning_effort",
	"verbosity", "reasoning_summary",
}

func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(io.LimitReader(r.Body, 64<<20))
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err != nil {
		writeErr(w, 400, "bad json: "+err.Error())
		return
	}
	msgs, _ := payload["messages"].([]any)
	if len(msgs) == 0 {
		writeErr(w, 400, "messages is required")
		return
	}
	modelName, _ := payload["model"].(string)
	if modelName == "" {
		modelName = "auto"
	}
	clientWantsStream, _ := payload["stream"].(bool)

	// 构造后端 body：只透传已知合法字段；后端只支持流式
	backendBody := map[string]any{}
	for _, k := range passthroughBodyKeys {
		if v, ok := payload[k]; ok {
			backendBody[k] = v
		}
	}
	if _, ok := backendBody["model"]; !ok {
		backendBody["model"] = "auto"
	}
	backendBody["stream"] = true
	if _, ok := backendBody["stream_options"]; !ok {
		backendBody["stream_options"] = map[string]any{"include_usage": true}
	}
	if s.desensitize {
		backendBody = DesensitizeBody(backendBody,
			[]string{"system", "developer"}, true, true, true)
	}

	hdr, err := s.cred.Headers(r.Context())
	if err != nil {
		writeErr(w, 502, err.Error())
		return
	}
	req, err := http.NewRequestWithContext(r.Context(), http.MethodPost,
		backendBase+"/v2/chat/completions", mustJSONReader(backendBody))
	if err != nil {
		writeErr(w, 500, err.Error())
		return
	}
	req.Header = hdr
	start := time.Now()
	resp, err := s.client.Do(req)
	if err != nil {
		log.Printf("[codebuddy] chat model=%s err=%v", modelName, err)
		writeErr(w, 502, "upstream error: "+err.Error())
		return
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		errBody, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		log.Printf("[codebuddy] chat model=%s status=%d err=%s", modelName, resp.StatusCode, truncate(string(errBody), 200))
		writeErr(w, resp.StatusCode, string(errBody))
		return
	}

	if clientWantsStream {
		// 流式：SSE 原样转发（后端已是标准 OpenAI SSE）；
		// 同时行扫描 data: 行提取 usage 入账（消耗 TOP 统计）
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		w.Header().Set("X-Accel-Buffering", "no")
		flusher, _ := w.(http.Flusher)
		scan := newUsageScanner(modelName, s)
		buf := make([]byte, 32*1024)
		for {
			n, rerr := resp.Body.Read(buf)
			if n > 0 {
				scan.feed(buf[:n])
				if _, werr := w.Write(buf[:n]); werr != nil {
					return
				}
				if flusher != nil {
					flusher.Flush()
				}
			}
			if rerr != nil {
				return
			}
		}
	}

	// 非流式：聚合后端 SSE 为单个 chat.completion（usage 顺带入账）
	result := collectStream(resp.Body, modelName)
	log.Printf("[codebuddy] chat model=%s status=200 dur=%s finish=%v", modelName,
		time.Since(start).Round(time.Millisecond), result["finish_reason_debug"])
	if usage, ok := result["usage"].(map[string]any); ok {
		s.addStat(modelName, toInt(usage["prompt_tokens"]), toInt(usage["completion_tokens"]), toInt(usage["total_tokens"]))
	}
	writeJSON(w, result)
}

// toInt 宽松取数（JSON 数字是 float64）。
func toInt(v any) int64 {
	if f, ok := v.(float64); ok {
		return int64(f)
	}
	return 0
}

// usageScanner 流式 SSE 行扫描：拼行、解析 data: 行里的 usage，命中即入账。
type usageScanner struct {
	model string
	buf   string
	srv   *Server
}

func newUsageScanner(model string, srv *Server) *usageScanner {
	return &usageScanner{model: model, srv: srv}
}

func (u *usageScanner) feed(chunk []byte) {
	u.buf += string(chunk)
	for {
		i := strings.IndexByte(u.buf, '\n')
		if i < 0 {
			break
		}
		line := strings.TrimSpace(u.buf[:i])
		u.buf = u.buf[i+1:]
		if !strings.HasPrefix(line, "data:") || strings.Contains(line, "[DONE]") {
			continue
		}
		var chunk struct {
			Usage *struct {
				PromptTokens     int64 `json:"prompt_tokens"`
				CompletionTokens int64 `json:"completion_tokens"`
				TotalTokens      int64 `json:"total_tokens"`
			} `json:"usage"`
		}
		if json.Unmarshal([]byte(strings.TrimSpace(line[5:])), &chunk) == nil && chunk.Usage != nil {
			u.srv.addStat(u.model, chunk.Usage.PromptTokens, chunk.Usage.CompletionTokens, chunk.Usage.TotalTokens)
		}
	}
}

func mustJSONReader(v any) io.Reader {
	b, _ := json.Marshal(v)
	return bytes.NewReader(b)
}

// collectStream 消费 OpenAI SSE 流，聚合为单个非流式 chat.completion 对象
// （合并 content/tool_calls 分片，取 usage/finish_reason）。
func collectStream(body io.Reader, modelName string) map[string]any {
	var contentParts []string
	type toolSlot struct {
		id, name, args string
	}
	toolCalls := map[int]*toolSlot{}
	var order []int
	var model, finish string
	var usage map[string]any

	sc := bufio.NewScanner(body)
	sc.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		data := strings.TrimSpace(line[5:])
		if data == "[DONE]" {
			break
		}
		var chunk map[string]any
		if json.Unmarshal([]byte(data), &chunk) != nil {
			continue
		}
		if m, ok := chunk["model"].(string); ok && m != "" {
			model = m
		}
		if u, ok := chunk["usage"].(map[string]any); ok {
			usage = u
		}
		choices, _ := chunk["choices"].([]any)
		for _, c := range choices {
			cm, ok := c.(map[string]any)
			if !ok {
				continue
			}
			if fr, ok := cm["finish_reason"].(string); ok && fr != "" {
				finish = fr
			}
			delta, _ := cm["delta"].(map[string]any)
			if delta == nil {
				continue
			}
			if ctext, ok := delta["content"].(string); ok && ctext != "" {
				contentParts = append(contentParts, ctext)
			}
			if tcs, ok := delta["tool_calls"].([]any); ok {
				for _, t := range tcs {
					tm, ok := t.(map[string]any)
					if !ok {
						continue
					}
					idx := 0
					switch v := tm["index"].(type) {
					case float64:
						idx = int(v)
					}
					slot := toolCalls[idx]
					if slot == nil {
						slot = &toolSlot{}
						toolCalls[idx] = slot
						order = append(order, idx)
					}
					if id, ok := tm["id"].(string); ok && id != "" {
						slot.id = id
					}
					if fn, ok := tm["function"].(map[string]any); ok {
						if n, ok := fn["name"].(string); ok && n != "" {
							slot.name = n
						}
						if a, ok := fn["arguments"].(string); ok {
							slot.args += a
						}
					}
				}
			}
		}
	}

	message := map[string]any{"role": "assistant"}
	content := strings.Join(contentParts, "")
	if content != "" {
		message["content"] = content
	} else {
		message["content"] = nil
	}
	finReason := finish
	if len(toolCalls) > 0 {
		tcs := make([]any, 0, len(order))
		for _, idx := range order {
			slot := toolCalls[idx]
			tcs = append(tcs, map[string]any{
				"id": slot.id, "type": "function",
				"function": map[string]any{"name": slot.name, "arguments": slot.args},
			})
		}
		message["tool_calls"] = tcs
		if finReason == "" {
			finReason = "tool_calls"
		}
	}
	if finReason == "" {
		finReason = "stop"
	}
	rid := make([]byte, 6)
	_, _ = rand.Read(rid)
	if usage == nil {
		usage = map[string]any{"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
	}
	return map[string]any{
		"id":                 "chatcmpl-" + hex.EncodeToString(rid),
		"object":             "chat.completion",
		"created":            time.Now().Unix(),
		"model":              orDefault(model, modelName),
		"choices":            []any{map[string]any{"index": 0, "message": message, "finish_reason": finReason}},
		"usage":              usage,
		"finish_reason_debug": finReason,
	}
}

func orDefault(s, def string) string {
	if s == "" {
		return def
	}
	return s
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, status int, msg string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{
		"error": map[string]any{"message": msg, "type": "upstream_error"},
	})
}
