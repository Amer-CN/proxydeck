// server.go —— Notion AI 的 OpenAI 兼容本地服务（8789）。
package notion

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// Server Notion 插件服务。
type Server struct {
	client *Client
	stats  *Stats
	sem    chan struct{} // 并发节流（1 并发 + 8 排队）
	start  time.Time
}

// Stats 本地统计（notion-stats.json 持久化）。
type Stats struct {
	mu      sync.Mutex
	path    string
	Calls   int64 `json:"calls"`
	InTok   int64 `json:"inputTokens"`
	OutTok  int64 `json:"outputTokens"`
	ByModel map[string]*modelStat `json:"models"`
}

type modelStat struct {
	Calls int64 `json:"calls"`
	In    int64 `json:"inputTokens"`
	Out   int64 `json:"outputTokens"`
}

// NewServer 创建服务。
func NewServer() *Server {
	exeDir := func() string {
		if e, err := os.Executable(); err == nil {
			return filepath.Dir(e)
		}
		return "."
	}()
	s := &Stats{path: filepath.Join(exeDir, "notion-stats.json"), ByModel: map[string]*modelStat{}}
	if b, err := os.ReadFile(s.path); err == nil {
		_ = json.Unmarshal(b, s)
		if s.ByModel == nil {
			s.ByModel = map[string]*modelStat{}
		}
	}
	return &Server{
		client: NewClient(),
		stats:  s,
		sem:    make(chan struct{}, 1),
		start:  time.Now(),
	}
}

// Start 监听并阻塞服务。
func (s *Server) Start(host, port string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/v1/models", s.handleModels)
	mux.HandleFunc("/model/info", s.handleModelInfo)
	mux.HandleFunc("/v1/chat/completions", s.handleChat)
	mux.HandleFunc("/v1/stats", s.handleStats)
	mux.HandleFunc("/quota", s.handleQuota)
	mux.HandleFunc("/refresh-token", s.handleRefreshToken)
	mux.HandleFunc("/spaces", s.handleSpaces)
	mux.HandleFunc("/space/select", s.handleSpaceSelect)
	mux.HandleFunc("/health", s.handleHealth)
	log.Printf("notion-plugin: listening on %s:%s", host, port)
	// CORS：GUI（127.0.0.1:1826 内嵌源）跨端口 fetch 本服务，无以下头会全部 Failed to fetch
	cors := func(h http.Handler) http.Handler {
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
	return http.ListenAndServe(host+":"+port, cors(mux))
}

func jsonOut(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(v)
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	ok := true
	token := "missing"
	if cr, err := s.client.Credentials(); err == nil {
		token = "ok"
		_ = cr
	} else {
		ok = false
	}
	jsonOut(w, 200, map[string]any{
		"service": "notion-ai", "status": "ok", "token": token, "tokenOk": ok,
		"uptimeSec": int(time.Since(s.start).Seconds()),
	})
}

// handleModelInfo 参数表（三关键模型 2026-08-17 实测定案；其余未测——显示 —，不编数）。
func (s *Server) handleModelInfo(w http.ResponseWriter, r *http.Request) {
	type info struct {
		Name      string `json:"name"`
		MaxInput  int    `json:"maxInput"`
		MaxOutput int    `json:"maxOutput"`
		Reasoning string `json:"reasoning"`
		Note      string `json:"note"`
	}
	// 实测定案（新账号两轮一致，服务端 maxContextTokens/maxInputTokens）
	// 身份验证 2026-08-17：Notion 为官方转发（与灵犀的假牌集群不同）
	measured := map[string]struct {
		ctx  int
		maxI int
		note string
	}{
		"opus-5":      {200000, 160000, "实测定案·审查备选"},
		"gpt-5.6-sol": {400000, 272000, "实测定案·GPT系推理"},
		"kimi-k3":     {500000, 460000, "实测定案·真Kimi·审查主力首选(460K)"},
	}
	out := []info{}
	for _, m := range modelTable {
		if md, ok := measured[m.ID]; ok {
			out = append(out, info{Name: m.ID, MaxInput: md.maxI, Note: md.note})
		} else {
			out = append(out, info{Name: m.ID, MaxInput: 0, Note: "未实测"})
		}
	}
	jsonOut(w, 200, out)
}

func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	list := Models()
	data := make([]map[string]string, 0, len(list))
	for _, id := range list {
		data = append(data, map[string]string{"id": id})
	}
	jsonOut(w, 200, map[string]any{"object": "list", "data": data})
}

func (s *Server) handleStats(w http.ResponseWriter, r *http.Request) {
	s.stats.mu.Lock()
	defer s.stats.mu.Unlock()
	models := map[string]any{}
	for k, v := range s.stats.ByModel {
		models[k] = map[string]any{"calls": v.Calls, "inputTokens": v.In, "outputTokens": v.Out,
			"totalTokens": v.In + v.Out}
	}
	jsonOut(w, 200, map[string]any{
		"uptimeSec": int(time.Since(s.start).Seconds()),
		"total":     map[string]any{"calls": s.stats.Calls, "inputTokens": s.stats.InTok, "outputTokens": s.stats.OutTok},
		"models":    models,
	})
}

var (
	quotaCacheMu sync.Mutex
	quotaCache   *Quota
	quotaCacheAt time.Time
)

func (s *Server) handleQuota(w http.ResponseWriter, r *http.Request) {
	quotaCacheMu.Lock()
	if quotaCache != nil && time.Since(quotaCacheAt) < 5*time.Minute {
		q := *quotaCache
		quotaCacheMu.Unlock()
		jsonOut(w, 200, q)
		return
	}
	quotaCacheMu.Unlock()
	q := s.client.FetchQuota("")
	quotaCacheMu.Lock()
	quotaCache = q
	quotaCacheAt = time.Now()
	quotaCacheMu.Unlock()
	jsonOut(w, 200, q)
}

// handleSpaces 列出账号全部工作空间（名称/套餐/是否当前）。
func (s *Server) handleSpaces(w http.ResponseWriter, r *http.Request) {
	list, err := s.client.ListSpaces()
	if err != nil {
		jsonOut(w, 200, map[string]any{"ok": false, "msg": err.Error()})
		return
	}
	jsonOut(w, 200, map[string]any{"ok": true, "spaces": list})
}

// handleSpaceSelect 切换当前工作空间（额度按 space 独立结算）。
func (s *Server) handleSpaceSelect(w http.ResponseWriter, r *http.Request) {
	var req struct {
		SpaceID string `json:"spaceId"`
	}
	_ = json.NewDecoder(r.Body).Decode(&req)
	if req.SpaceID == "" {
		jsonOut(w, 400, map[string]any{"ok": false, "msg": "spaceId 为空"})
		return
	}
	if err := s.client.SelectSpace(req.SpaceID); err != nil {
		jsonOut(w, 200, map[string]any{"ok": false, "msg": err.Error()})
		return
	}
	// 切换即失效额度缓存：额度按空间独立结算，立即反映新空间
	quotaCacheMu.Lock()
	quotaCache = nil
	quotaCacheMu.Unlock()
	jsonOut(w, 200, map[string]any{"ok": true, "msg": "已切换"})
}

func (s *Server) handleRefreshToken(w http.ResponseWriter, r *http.Request) {
	if err := s.client.RefreshToken(); err != nil {
		jsonOut(w, 200, map[string]any{"ok": false, "msg": err.Error()})
		return
	}
	jsonOut(w, 200, map[string]any{"ok": true, "msg": "令牌已刷新"})
}

func (s *Server) record(model string, in, out int64) {
	s.stats.mu.Lock()
	defer s.stats.mu.Unlock()
	s.stats.Calls++
	s.stats.InTok += in
	s.stats.OutTok += out
	ms, ok := s.stats.ByModel[model]
	if !ok {
		ms = &modelStat{}
		s.stats.ByModel[model] = ms
	}
	ms.Calls++
	ms.In += in
	ms.Out += out
	if b, err := json.Marshal(s.stats); err == nil {
		_ = os.WriteFile(s.stats.path, b, 0o644)
	}
}

func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	if r.Method != "POST" {
		jsonOut(w, 405, map[string]any{"error": map[string]string{"message": "method not allowed"}})
		return
	}
	var req struct {
		Model    string        `json:"model"`
		Stream   bool          `json:"stream"`
		Messages []ChatMessage `json:"messages"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		jsonOut(w, 400, map[string]any{"error": map[string]string{"message": "invalid JSON: " + err.Error()}})
		return
	}
	if len(req.Messages) == 0 {
		jsonOut(w, 400, map[string]any{"error": map[string]string{"message": "messages 为空"}})
		return
	}
	model := req.Model
	if model == "" {
		model = "notion/opus-4.7"
	}
	// 节流：并发 1，队列 8
	select {
	case s.sem <- struct{}{}:
		defer func() { <-s.sem }()
	case <-time.After(2 * time.Second):
		w.Header().Set("Retry-After", "10")
		jsonOut(w, 429, map[string]any{"error": map[string]string{"message": "繁忙（Notion AI 限速保护），请稍后重试"}})
		return
	}

	last := req.Messages[len(req.Messages)-1]
	if last.Role != "user" {
		jsonOut(w, 400, map[string]any{"error": map[string]string{"message": "最后一条消息须为 user"}})
		return
	}
	created := time.Now().Unix()
	reqID := "chatcmpl-notion-" + fmt.Sprint(created)

	if !req.Stream {
		text, usage, err := s.client.RunChat(model, last.Content, req.Messages[:len(req.Messages)-1], nil)
		if err != nil {
			jsonOut(w, 502, map[string]any{"error": map[string]string{"message": err.Error()}})
			return
		}
		s.record(model, int64(usage["input_tokens"]), int64(usage["output_tokens"]))
		jsonOut(w, 200, map[string]any{
			"id": reqID, "object": "chat.completion", "created": created, "model": model,
			"choices": []any{map[string]any{
				"index": 0, "finish_reason": "stop",
				"message": map[string]string{"role": "assistant", "content": text},
			}},
			"usage": map[string]int{"prompt_tokens": usage["input_tokens"], "completion_tokens": usage["output_tokens"],
				"total_tokens": usage["input_tokens"] + usage["output_tokens"]},
		})
		return
	}

	// 流式：SSE
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.WriteHeader(200)
	flusher, _ := w.(http.Flusher)
	send := func(obj any) {
		b, _ := json.Marshal(obj)
		fmt.Fprintf(w, "data: %s\n\n", b)
		if flusher != nil {
			flusher.Flush()
		}
	}
	send(map[string]any{
		"id": reqID, "object": "chat.completion.chunk", "created": created, "model": model,
		"choices": []any{map[string]any{"index": 0, "delta": map[string]string{"role": "assistant"}}},
	})
	var mu sync.Mutex
	var lastDelta string
	text, usage, err := s.client.RunChat(model, last.Content, req.Messages[:len(req.Messages)-1], func(delta string) {
		mu.Lock()
		// p=整体替换语义：发送相对上次的增量
		var inc string
		if len(delta) >= len(lastDelta) && strings.HasPrefix(delta, lastDelta) {
			inc = delta[len(lastDelta):]
		} else {
			inc = delta
		}
		lastDelta = delta
		mu.Unlock()
		if inc != "" {
			send(map[string]any{
				"id": reqID, "object": "chat.completion.chunk", "created": created, "model": model,
				"choices": []any{map[string]any{"index": 0, "delta": map[string]string{"content": inc}}},
			})
		}
	})
	if err != nil {
		if text != "" {
			log.Printf("[notion] stream ended with partial text (%d runes): %v", len([]rune(text)), err)
		}
		send(map[string]any{"error": map[string]string{"message": err.Error()}})
	} else {
		s.record(model, int64(usage["input_tokens"]), int64(usage["output_tokens"]))
		send(map[string]any{
			"id": reqID, "object": "chat.completion.chunk", "created": created, "model": model,
			"choices": []any{map[string]any{"index": 0, "delta": map[string]string{}, "finish_reason": "stop"}},
		})
	}
	fmt.Fprint(w, "data: [DONE]\n\n")
	if flusher != nil {
		flusher.Flush()
	}
}
