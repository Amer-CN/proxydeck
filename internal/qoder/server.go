// Package qoder —— 阿里 Qoder 第六平台甲板。
//
// 传输层：每请求 spawn 官方 agent SDK 的 worker 进程（qoder-worker-runtime.obf.mjs，
// stdin/stdout stream-json，jobToken 回调鉴权；全部上游流量出自官方 worker 进程，
// 与 Comate 同款"零自造请求"），对外暴露本地 OpenAI 兼容接口 http://127.0.0.1:8785/v1，
// 客户端（Codex/Cursor 等）直接接入。
package qoder

import (
	"encoding/json"
	"fmt"
	"io"
	"log"
	"math/rand"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// qoderModels 静态模型目录：以用户 2026-08-30 的 Qoder 界面截图为准（13 条全量）。
// id 传给 worker 的 --model 原文（displayName 上游不认——实测），Name 为界面展示名。
// 代号确证来源：客户端缓存（state.vscdb/render er缓存）+ 用户实切模型后 App 日志
// （main.log 的 model:"..." 与 SDK 启动参数 --model）——q37fmodel=Qwen3.7-Flash、
// gfmodel=GLM-5.3-Flash 即 2026-08-30 用户实切验证所得。
var qoderModels = []struct{ ID, Name string }{
	{"Auto", "Auto"},
	{"qmodel_38max", "Qwen3.8-Max"},
	{"qfmodel", "Qwen3.8-Flash"},
	{"q37fmodel", "Qwen3.7-Flash"},
	{"qmodel_latest", "Qwen3.7-Max"},
	{"qmodel", "Qwen3.7-Plus"},
	{"dmodel", "DeepSeek-V4-Pro"},
	{"dfmodel", "DeepSeek-V4-Flash"},
	{"gmodel", "GLM-5.3"},
	{"gfmodel", "GLM-5.3-Flash"},
	{"gm51model", "GLM-5.2"},
	{"kmodel", "Kimi-K2.7-Code"},
	{"mmodel", "MiniMax-M3"},
}

// Server 是 Qoder 的本地 OpenAI 兼容服务：翻译代理转发官方 worker 会话引擎。
type Server struct {
	ln        net.Listener
	srv       *http.Server
	startedAt time.Time

	authMu    sync.Mutex
	authCache *qoderAuth

	creditsMu     sync.Mutex
	spent         float64 // 本次进程内 worker result total_credits 合计（代理侧积分累计）
	quotaExceeded bool    // 捕获到服务端配额错误（112/113/119 等）后置位，/health 报警

	usageMu    sync.Mutex
	usageCache *usageQuota // openapi 实时额度缓存（60s），/health 展示用
	usageAt    time.Time
}

// NewServer 创建服务。
func NewServer() *Server {
	return &Server{startedAt: time.Now()}
}

// corsWith 给所有响应加 CORS 头：GUI 页面跑在 localhost:随机端口，
// fetch 127.0.0.1:8785 属跨域，没有这个头前端全部拉不到数据。
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

// currentAuth 取当前登录态：内存缓存，expiresAt 临近过期（<30s）或解不出时重读
// 文件（App 在跑时 auth.v1.dat 会被轮换刷新，重读自动跟上）。
func (s *Server) currentAuth() (*qoderAuth, error) {
	s.authMu.Lock()
	defer s.authMu.Unlock()
	if s.authCache != nil && time.Until(s.authCache.ExpiresAt) > 30*time.Second {
		return s.authCache, nil
	}
	a, err := loadAuth()
	if err != nil {
		return nil, err
	}
	s.authCache = a
	return a, nil
}

// creditsLedgerPath 积分账本文件：~/.qoder-cli-credits.json（ProxyDeck 自己的小账本）。
// 口径说明：服务端是多桶计费（套餐 Credits + 按厂商专属包，各有有效期与扣减优先级），
// 代理无法得知每次消耗落在哪个桶——账本只记「累计消耗」（worker result 的
// total_credits 是服务端逐请求下发的真实消耗），不假装知道「剩余」。
func creditsLedgerPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".qoder-cli-credits.json")
}

// readCreditsSpent 读账本累计消耗；文件不存在/损坏返回 0。
func readCreditsSpent() float64 {
	p := creditsLedgerPath()
	if p == "" {
		return 0
	}
	b, err := os.ReadFile(p)
	if err != nil {
		return 0
	}
	var v struct {
		Spent float64 `json:"spent"`
	}
	if json.Unmarshal(b, &v) != nil {
		return 0
	}
	return v.Spent
}

// writeCreditsSpent 写账本：{"spent":<float64>,"updatedAt":<unix ms>}。
func writeCreditsSpent(spent float64) error {
	p := creditsLedgerPath()
	if p == "" {
		return fmt.Errorf("无法定位用户主目录")
	}
	b, err := json.Marshal(map[string]any{"spent": spent, "updatedAt": time.Now().UnixMilli()})
	if err != nil {
		return err
	}
	return os.WriteFile(p, b, 0o644)
}

// addCredits 在 worker result 到达后累计消耗：
// 每请求 spawn 单个 worker，result.total_credits 即本次请求消耗（服务端下发，真实口径）。
// 剩余积分无法代理侧得知（多桶计费+openapi 走 WASM 签名，直连已实测被拒），
// 故只展示累计；额度耗尽由配额错误检测（quotaExceeded）在 /health 报警。
func (s *Server) addCredits(total float64) {
	if total <= 0 {
		return
	}
	s.creditsMu.Lock()
	defer s.creditsMu.Unlock()
	s.spent += total
	spent := readCreditsSpent() + total
	if err := writeCreditsSpent(spent); err != nil {
		log.Printf("qoder-plugin: credits 写账本失败: %v", err)
		return
	}
	log.Printf("qoder-plugin: credits +%.2f → 累计 %.2f", total, spent)
}

// handleHealth 返回本服务健康态：无登录态时 200 + {"status":"no_license"}；
// 否则 200 + {"status":"ok","uptimeSec":...}。每次现读 auth.v1.dat
// （App 可能已轮换刷新 token，展示口径跟随最新文件）。
// credits：{realtime 官方实时额度(openapi 60s 缓存：套餐+专属包+汇总剩余),
// spent 累计消耗(账本持久), spentThisSession 本次进程, quotaExceeded 配额报警}。
func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	a, err := loadAuth()
	if err != nil {
		_ = json.NewEncoder(w).Encode(map[string]any{"status": "no_license"})
		return
	}
	out := map[string]any{
		"status":    "ok",
		"service":   "qoder",
		"uptimeSec": int64(time.Since(s.startedAt).Seconds()),
	}
	s.creditsMu.Lock()
	spent := s.spent
	exceeded := s.quotaExceeded
	s.creditsMu.Unlock()
	credits := map[string]any{
		"spent":            readCreditsSpent(),
		"spentThisSession": spent,
		"quotaExceeded":    exceeded,
	}
	if v := usageView(s.cachedUsage(a.Token, r.URL.Query().Get("refresh") == "1")); v != nil {
		credits["realtime"] = v
		if qe, _ := v["isQuotaExceeded"].(bool); qe {
			credits["quotaExceeded"] = true
		}
	}
	out["credits"] = credits
	_ = json.NewEncoder(w).Encode(out)
}

// isQuotaError 识别服务端配额类错误（error-code-cache.json 实录：110 今日问答上限、
// 112/113 配额上限、115 轻量模型月上限、119 免费次数用完）。
func isQuotaError(msg string) bool {
	if msg == "" {
		return false
	}
	for _, pat := range []string{"ResourceUsageOverlimit", "配额", "升级专业版", "免费限额", "auto-free-individual"} {
		if strings.Contains(msg, pat) {
			return true
		}
	}
	for _, code := range []string{"110", "112", "113", "115", "119"} {
		if strings.Contains(msg, "code\":"+code) || strings.Contains(msg, "code="+code) {
			return true
		}
	}
	return false
}

// handleCreditsReset 清零累计消耗：POST /credits/set body {"spent":0}
// （用户对完账后想从头计数时点卡片触发）。
func (s *Server) handleCreditsSet(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	if r.Method != http.MethodPost {
		_ = json.NewEncoder(w).Encode(map[string]any{"ok": false, "error": "仅支持 POST"})
		return
	}
	var body struct {
		Spent float64 `json:"spent"`
	}
	if err := json.NewDecoder(io.LimitReader(r.Body, 64<<10)).Decode(&body); err != nil {
		_ = json.NewEncoder(w).Encode(map[string]any{"ok": false, "error": "请求体解析失败: " + err.Error()})
		return
	}
	if body.Spent != 0 {
		_ = json.NewEncoder(w).Encode(map[string]any{"ok": false, "error": "只支持清零（spent=0）"})
		return
	}
	s.creditsMu.Lock()
	s.spent = 0
	s.creditsMu.Unlock()
	if err := writeCreditsSpent(0); err != nil {
		_ = json.NewEncoder(w).Encode(map[string]any{"ok": false, "error": err.Error()})
		return
	}
	log.Printf("qoder-plugin: credits 累计清零")
	_ = json.NewEncoder(w).Encode(map[string]any{"ok": true})
}

// handleModels 返回静态模型目录（13 条 = 截图全量，首条 Auto，代号全部确证）。
func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	data := make([]map[string]any, 0, len(qoderModels))
	for _, m := range qoderModels {
		data = append(data, map[string]any{"id": m.ID, "display_name": m.Name, "object": "model"})
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{"object": "list", "data": data})
}

// resolveModel：'auto'/空 → 'Auto'；白名单内（忽略大小写，id 与显示名双向匹配）
// → 目录 id 原文；不认识的返回 ""（调用方回 502 invalid_model，提示改用 auto）。
func resolveModel(requested string) string {
	if requested == "" || strings.EqualFold(requested, "auto") {
		return "Auto"
	}
	for _, m := range qoderModels {
		if strings.EqualFold(m.ID, requested) || strings.EqualFold(m.Name, requested) {
			return m.ID
		}
	}
	return ""
}

// chatRequest 是 OpenAI /v1/chat/completions 请求体的最小字段。
type chatRequest struct {
	Model    string `json:"model"`
	Stream   bool   `json:"stream"`
	Messages []any  `json:"messages"`
}

// writeError 输出 OpenAI 风格错误 JSON。
func writeError(w http.ResponseWriter, status int, message, errType string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{
		"error": map[string]any{"message": message, "type": errType},
	})
}

// flattenMessages 把 OpenAI messages 扁平化为 worker 的 prompt 文本：
// 按顺序 role: content 拼接，system→[System]\n、user→[User]\n、assistant→[Assistant]\n；
// content 为数组时取其中 text 部分拼接；忽略 tool_calls 等复杂字段。
func flattenMessages(messages []any) string {
	var sb strings.Builder
	for _, raw := range messages {
		msg, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		role, _ := msg["role"].(string)
		switch role {
		case "system":
			sb.WriteString("[System]\n")
		case "user":
			sb.WriteString("[User]\n")
		case "assistant":
			sb.WriteString("[Assistant]\n")
		default:
			sb.WriteString("[" + role + "]\n")
		}
		switch c := msg["content"].(type) {
		case string:
			sb.WriteString(c)
		case []any:
			for _, part := range c {
				if p, ok := part.(map[string]any); ok {
					if t, _ := p["text"].(string); t != "" {
						sb.WriteString(t)
					}
				}
			}
		}
		sb.WriteString("\n")
	}
	return strings.TrimRight(sb.String(), "\n")
}

// chunkJSON 构造 OpenAI 流式 chunk（delta.content 为增量文本；finish 为空时 finish_reason=null）。
func chunkJSON(id, model, delta, finish string, created int64) []byte {
	d := map[string]any{}
	if delta != "" {
		d["content"] = delta
	}
	fr := any(nil)
	if finish != "" {
		fr = finish
	}
	m := map[string]any{
		"id":      id,
		"object":  "chat.completion.chunk",
		"created": created,
		"model":   model,
		"choices": []map[string]any{{
			"index":         0,
			"delta":         d,
			"finish_reason": fr,
		}},
	}
	b, _ := json.Marshal(m)
	return b
}

// completeJSON 构造 OpenAI 非流式完整响应（上游无 usage 数据，填 0）。
func completeJSON(id, model, content string, created int64) []byte {
	m := map[string]any{
		"id":      id,
		"object":  "chat.completion",
		"created": created,
		"model":   model,
		"choices": []map[string]any{{
			"index":         0,
			"message":       map[string]any{"role": "assistant", "content": content},
			"finish_reason": "stop",
		}},
		"usage": map[string]any{
			"prompt_tokens":     0,
			"completion_tokens": 0,
			"total_tokens":      0,
		},
	}
	b, _ := json.Marshal(m)
	return b
}

// firstErr 取失败原因：result.errors 第一条；无则兜底文案。
func firstErr(res *workerResult) string {
	if res != nil {
		for _, e := range res.Errs {
			if strings.TrimSpace(e) != "" {
				return e
			}
		}
	}
	return "Qoder worker 返回错误"
}

// handleChat /v1/chat/completions 入口：解析 OpenAI body → 扁平化 prompt →
// spawn worker 会话 → 翻译回 OpenAI。
func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	start := time.Now()
	if r.Method != http.MethodPost {
		writeError(w, http.StatusMethodNotAllowed, "仅支持 POST", "invalid_request")
		return
	}
	auth, err := s.currentAuth()
	if err != nil {
		log.Printf("qoder-plugin: %s %s model=- 503 no_license %s", r.Method, r.URL.Path, time.Since(start))
		writeError(w, http.StatusServiceUnavailable, "未找到 Qoder 登录态，请先在 Qoder CN App 登录", "no_license")
		return
	}
	workerPath := findWorker()
	if workerPath == "" {
		log.Printf("qoder-plugin: %s %s model=- 503 no_worker %s", r.Method, r.URL.Path, time.Since(start))
		writeError(w, http.StatusServiceUnavailable, "未找到 Qoder 官方 worker，请先安装 Qoder CN（AI IDE）", "no_license")
		return
	}
	var req chatRequest
	if err := json.NewDecoder(io.LimitReader(r.Body, 32<<20)).Decode(&req); err != nil {
		log.Printf("qoder-plugin: %s %s model=- 400 %s", r.Method, r.URL.Path, time.Since(start))
		writeError(w, http.StatusBadRequest, "请求体解析失败: "+err.Error(), "invalid_request")
		return
	}
	model := resolveModel(req.Model)
	if model == "" {
		log.Printf("qoder-plugin: %s %s model=%s 502 invalid_model %s", r.Method, r.URL.Path, req.Model, time.Since(start))
		writeError(w, http.StatusBadGateway, "不支持的模型: "+req.Model+"（/v1/models 查看可用列表，或改用 auto 由 Qoder 自动路由）", "invalid_model")
		return
	}
	prompt := flattenMessages(req.Messages)
	if req.Stream {
		s.handleChatStream(w, r, auth, workerPath, prompt, model, start)
	} else {
		s.handleChatOnce(w, r, auth, workerPath, prompt, model, start)
	}
}

// handleChatOnce 非流式：等 result，content=result.result（空则用拼接文本）。
func (s *Server) handleChatOnce(w http.ResponseWriter, r *http.Request, auth *qoderAuth, workerPath, prompt, model string, start time.Time) {
	created := time.Now().Unix()
	id := fmt.Sprintf("chatcmpl-qoder-%d", rand.Int63())
	res, err := runWorker(auth, workerPath, model, prompt, nil)
	if err != nil {
		log.Printf("qoder-plugin: %s %s model=%s 502 %s", r.Method, r.URL.Path, model, time.Since(start))
		writeError(w, http.StatusBadGateway, err.Error(), "upstream_error")
		return
	}
	s.addCredits(res.TotalCredits) // result 已到达，累计本次消耗（服务端下发真实口径）
	if res.IsErr {
		if isQuotaError(firstErr(res)) {
			s.creditsMu.Lock()
			s.quotaExceeded = true
			s.creditsMu.Unlock()
			log.Printf("qoder-plugin: %s %s 检测到配额错误 → /health 报警", r.Method, r.URL.Path)
		}
		log.Printf("qoder-plugin: %s %s model=%s 502 is_error %s", r.Method, r.URL.Path, model, time.Since(start))
		writeError(w, http.StatusBadGateway, firstErr(res), "upstream_error")
		return
	}
	content := res.Final
	if content == "" {
		content = res.Text
	}
	w.Header().Set("Content-Type", "application/json")
	_, _ = w.Write(completeJSON(id, model, content, created))
	log.Printf("qoder-plugin: %s %s model=%s 200 %s", r.Method, r.URL.Path, model, time.Since(start))
}

// handleChatStream 流式：assistant text 块逐个转 OpenAI chunk，result 后发
// finish_reason=stop chunk + [DONE]。首个文本块落盘前失败可回 502。
func (s *Server) handleChatStream(w http.ResponseWriter, r *http.Request, auth *qoderAuth, workerPath, prompt, model string, start time.Time) {
	created := time.Now().Unix()
	id := fmt.Sprintf("chatcmpl-qoder-%d", rand.Int63())
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	fl, _ := w.(http.Flusher)

	// committed=true 表示首个文本块已落盘（HTTP 200 已提交）。在此之前失败可回 502。
	committed := false
	emit := func(delta string) {
		committed = true
		_, _ = w.Write([]byte("data: " + string(chunkJSON(id, model, delta, "", created)) + "\n\n"))
		fl.Flush()
	}
	res, err := runWorker(auth, workerPath, model, prompt, emit)
	if err == nil {
		s.addCredits(res.TotalCredits) // result 已到达，累计本次消耗（服务端下发真实口径）
	}
	if err != nil || res.IsErr {
		msg := firstErr(res)
		if err != nil {
			msg = err.Error()
		}
		if isQuotaError(msg) {
			s.creditsMu.Lock()
			s.quotaExceeded = true
			s.creditsMu.Unlock()
			log.Printf("qoder-plugin: %s %s 检测到配额错误 → /health 报警", r.Method, r.URL.Path)
		}
		if !committed {
			log.Printf("qoder-plugin: %s %s model=%s stream 502 %s", r.Method, r.URL.Path, model, time.Since(start))
			writeError(w, http.StatusBadGateway, msg, "upstream_error")
			return
		}
		log.Printf("qoder-plugin: %s %s model=%s stream 200(部分) 上游错误:%s %s", r.Method, r.URL.Path, model, msg, time.Since(start))
	} else {
		log.Printf("qoder-plugin: %s %s model=%s stream 200 %s", r.Method, r.URL.Path, model, time.Since(start))
	}
	// 结束块：finish_reason=stop + [DONE]
	stop := chunkJSON(id, model, "", "stop", created)
	_, _ = w.Write([]byte("data: " + string(stop) + "\n\n"))
	fl.Flush()
	_, _ = w.Write([]byte("data: [DONE]\n\n"))
	fl.Flush()
}

// Start 在 host:port 上监听（阻塞）。worker 按请求 spawn，无需预热。
func (s *Server) Start(host, port string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/credits/set", s.handleCreditsSet)
	mux.HandleFunc("/v1/models", s.handleModels)
	mux.HandleFunc("/v1/chat/completions", s.handleChat)

	ln, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return fmt.Errorf("端口 %s 被占用: %w", port, err)
	}
	s.ln = ln
	s.srv = &http.Server{Handler: corsWith(mux)}
	return s.srv.Serve(ln)
}

// Stop 停止监听；worker 进程随请求结束，无后台子进程可停。
func (s *Server) Stop() {
	if s.srv != nil {
		_ = s.srv.Close()
	}
}
