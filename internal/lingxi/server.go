// server.go —— 灵犀 OpenAI 兼容本地服务（8790）。
package lingxi

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

// Server 灵犀插件服务。
type Server struct {
	client *Client
	stats  *Stats
	sem    chan struct{}
	start  time.Time
}

// Stats 本地统计（lingxi-stats.json）。
type Stats struct {
	mu      sync.Mutex
	path    string
	Calls   int64                  `json:"calls"`
	ByModel map[string]*modelCount `json:"models"`
}

type modelCount struct {
	Calls int64 `json:"calls"`
}

// NewServer 创建服务。
func NewServer() *Server {
	dir := func() string {
		if e, err := os.Executable(); err == nil {
			return filepath.Dir(e)
		}
		return "."
	}()
	s := &Stats{path: filepath.Join(dir, "lingxi-stats.json"), ByModel: map[string]*modelCount{}}
	if b, err := os.ReadFile(s.path); err == nil {
		_ = json.Unmarshal(b, s)
		if s.ByModel == nil {
			s.ByModel = map[string]*modelCount{}
		}
	}
	return &Server{client: NewClient(), stats: s, sem: make(chan struct{}, 2), start: time.Now()}
}

// Start 监听并阻塞。
func (s *Server) Start(host, port string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/v1/models", s.handleModels)
	mux.HandleFunc("/v1/chat/completions", s.handleChat)
	mux.HandleFunc("/v1/stats", s.handleStats)
	mux.HandleFunc("/quota", s.handleQuota)
	mux.HandleFunc("/refresh-token", s.handleRefreshToken)
	mux.HandleFunc("/model/info", s.handleModelInfo)
	mux.HandleFunc("/health", s.handleHealth)
	log.Printf("lingxi-plugin: listening on %s:%s", host, port)
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
	token := "ok"
	if _, err := s.client.Cookie(); err != nil {
		token = "missing"
	}
	jsonOut(w, 200, map[string]any{"service": "lingxi", "status": "ok", "token": token,
		"uptimeSec": int(time.Since(s.start).Seconds())})
}

func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	list := Models(s.client)
	data := make([]map[string]string, 0, len(list))
	for _, m := range list {
		data = append(data, map[string]string{"id": m.Key, "owned_by": "lingxi"})
	}
	jsonOut(w, 200, map[string]any{"object": "list", "data": data})
}

// handleModelInfo 参数表：2026-08-17 实测（上下文=压力测试失忆点边界；身份=模型自述验证）。
// 注意：各模型官方原生均 1M 级上下文，以下为灵犀网关实测限制（按最保守可用值填）。
func (s *Server) handleModelInfo(w http.ResponseWriter, r *http.Request) {
	type info struct {
		Name      string `json:"name"`
		MaxInput  int    `json:"maxInput"`
		MaxOutput int    `json:"maxOutput"`
		Reasoning string `json:"reasoning"`
		Note      string `json:"note"`
	}
	// 2026-08-17 终局鉴定（身份问询+版本知识探针+上下文压力测试）：
	// 全部槽位知识截止整齐 2025-04 = WPS 自研模型集群挂名牌，非各家官方 API。
	measured := map[string]struct {
		ctx      int
		identity string
		tip      string
	}{
		"deepseek-v4-flash-0731":      {200000, "假牌·WPS自研(截止25-04)", "灵犀唯一推荐 · 0.05x性价比"},
		"deepseek-v4-pro-0813":        {200000, "假牌·WPS自研(截止25-04)", "重推理可用的第二档"},
		"mimo-v2.5-pro":               {130000, "假牌·WPS自研(截止25-04)", "轻量"},
		"kimi-k3":                     {150000, "半真·Kimi系但自认只到K2", "❌1.60x智商税 · 审查请用Notion版K3(460K)"},
		"GLM-5.2":                     {180000, "假牌·WPS自研(截止25-04)", "❌0.75x不值"},
		"GLM-5.3":                     {180000, "假牌·WPS自研(截止25-04)", "❌0.75x不值"},
		"minimax_m3":                  {150000, "半真·自认MiniMax(截止25-04)", "中等可选"},
		"qwen3.8_max":                 {150000, "假牌·不识Qwen3.8(截止25-04)", "❌0.80x智商税"},
		"doubao_seed_2_1_pro_260628":  {130000, "假牌·自认非豆包(截止25-04)", "❌上下文最小"},
	}
	out := []info{}
	for _, m := range Models(s.client) {
		md := measured[m.Key]
		note := "灵点倍率 " + m.Multiplier
		if m.Tag != "" {
			note += " · " + m.Tag
		}
		if md.identity != "" {
			note += " · " + md.identity
		}
		if md.tip != "" {
			note += " · " + md.tip
		}
		out = append(out, info{Name: m.Key, MaxInput: md.ctx, MaxOutput: 0, Reasoning: "", Note: note})
	}
	jsonOut(w, 200, out)
}

func (s *Server) handleQuota(w http.ResponseWriter, r *http.Request) {
	// 灵点额度：plans 接口只有倍率，余额需 aigc 端点（未挖到）——先返回模型倍率与令牌状态
	if _, err := s.client.Cookie(); err != nil {
		jsonOut(w, 200, map[string]any{"source": "none", "err": err.Error()})
		return
	}
	plans := s.client.FetchPlans()
	models := Models(s.client)
	type mInfo struct {
		Key        string `json:"key"`
		Multiplier string `json:"multiplier"`
	}
	mis := make([]mInfo, 0, len(models))
	for _, m := range models {
		mis = append(mis, mInfo{m.Key, m.Multiplier})
	}
	total, packs, _ := s.client.FetchCredits()
	type pk struct {
		Balance int    `json:"balance"`
		Expire  string `json:"expire"`
	}
	pks := make([]pk, 0, len(packs))
	for _, p := range packs {
		pks = append(pks, pk{p.Balance, p.ExpireTime[:10]})
	}
	jsonOut(w, 200, map[string]any{"source": "live", "credits": total, "packs": pks,
		"plans": plans, "models": mis})
}

func (s *Server) handleRefreshToken(w http.ResponseWriter, r *http.Request) {
	if err := s.client.RefreshToken(); err != nil {
		jsonOut(w, 200, map[string]any{"ok": false, "msg": err.Error()})
		return
	}
	jsonOut(w, 200, map[string]any{"ok": true, "msg": "已刷新"})
}

func (s *Server) handleStats(w http.ResponseWriter, r *http.Request) {
	s.stats.mu.Lock()
	defer s.stats.mu.Unlock()
	models := map[string]any{}
	for k, v := range s.stats.ByModel {
		models[k] = map[string]any{"calls": v.Calls}
	}
	jsonOut(w, 200, map[string]any{"uptimeSec": int(time.Since(s.start).Seconds()),
		"total": map[string]any{"calls": s.stats.Calls}, "models": models})
}

func (s *Server) record(model string) {
	s.stats.mu.Lock()
	defer s.stats.mu.Unlock()
	s.stats.Calls++
	mc, ok := s.stats.ByModel[model]
	if !ok {
		mc = &modelCount{}
		s.stats.ByModel[model] = mc
	}
	mc.Calls++
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
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil || len(req.Messages) == 0 {
		jsonOut(w, 400, map[string]any{"error": map[string]string{"message": "invalid request"}})
		return
	}
	select {
	case s.sem <- struct{}{}:
		defer func() { <-s.sem }()
	case <-time.After(2 * time.Second):
		w.Header().Set("Retry-After", "5")
		jsonOut(w, 429, map[string]any{"error": map[string]string{"message": "繁忙，请稍后"}})
		return
	}

	model := ResolveModel(req.Model, s.client)
	if model == "" {
		jsonOut(w, 400, map[string]any{"error": map[string]string{"message": ErrUnknownModel(req.Model, s.client).Error()}})
		return
	}
	last := req.Messages[len(req.Messages)-1]
	if last.Role != "user" {
		jsonOut(w, 400, map[string]any{"error": map[string]string{"message": "最后一条须为 user"}})
		return
	}
	created := time.Now().Unix()
	reqID := fmt.Sprintf("chatcmpl-lingxi-%d", created)

	if !req.Stream {
		text, err := s.client.Chat(model, last.Content, req.Messages[:len(req.Messages)-1], nil)
		if err != nil {
			jsonOut(w, 502, map[string]any{"error": map[string]string{"message": err.Error()}})
			return
		}
		s.record(model)
		jsonOut(w, 200, map[string]any{
			"id": reqID, "object": "chat.completion", "created": created, "model": model,
			"choices": []any{map[string]any{"index": 0, "finish_reason": "stop",
				"message": map[string]string{"role": "assistant", "content": text}}},
		})
		return
	}

	// 流式
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
	send(map[string]any{"id": reqID, "object": "chat.completion.chunk", "created": created, "model": model,
		"choices": []any{map[string]any{"index": 0, "delta": map[string]string{"role": "assistant"}}}})
	text, err := s.client.Chat(model, last.Content, req.Messages[:len(req.Messages)-1], func(delta string) {
		send(map[string]any{"id": reqID, "object": "chat.completion.chunk", "created": created, "model": model,
			"choices": []any{map[string]any{"index": 0, "delta": map[string]string{"content": delta}}}})
	})
	if err != nil && text == "" {
		send(map[string]any{"error": map[string]string{"message": err.Error()}})
	} else {
		s.record(model)
		send(map[string]any{"id": reqID, "object": "chat.completion.chunk", "created": created, "model": model,
			"choices": []any{map[string]any{"index": 0, "delta": map[string]string{}, "finish_reason": "stop"}}})
	}
	fmt.Fprint(w, "data: [DONE]\n\n")
	if flusher != nil {
		flusher.Flush()
	}
	_ = strings.TrimSpace("") // keep strings import
}
