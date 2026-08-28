package comate

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"math/rand"
	"net"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"
)

// upstreamBase 内部 zulu serve 地址（固定 8792）。
const upstreamBase = "http://127.0.0.1:" + zuluPort

// fallbackModels list-model 失败时回退的硬编码目录（modelId 含后缀，原样保留）。
var fallbackModels = []map[string]any{
	{"id": "deepseek-v4-flash_a8668ea86c3d40e7ad1f8ee45715d447", "object": "model"},
	{"id": "deepseek-v4-pro_4ba5277b630243be9eeb19fe48f853fa", "object": "model"},
	{"id": "glm-4.7-fc_0b87452a03394418b5c8d5468deff6c7", "object": "model"},
	{"id": "glm-5-turbo-fc_6748c832242b4bcb8f715c7c7902d1ad", "object": "model"},
	{"id": "glm-5.0-fc_c0e11dc07fbb4516a6b687014d9b4fa8", "object": "model"},
	{"id": "glm-5.1_a231cc317baf4eaaa0b5afdd4de5f1d9", "object": "model"},
	{"id": "glm-5.2_50c584482e9d47edbbe47938a88b56b1", "object": "model"},
	{"id": "glm-5.3_37c550fc6d5b4fb2825abc2c1c88a6b3", "object": "model"},
	{"id": "glm-5.3-flash_805131a9381a4940ba0f2d90cad0e7eb", "object": "model"},
	{"id": "glm-5v-turbo_ea9e4f222f0a40b281953ebcb6981dda", "object": "model"},
	{"id": "kimi-k2.5-oneapi_c047f2503bd648269ac7d6fb447d574c", "object": "model"},
	{"id": "kimi-k2.6-oneapi_123e4567e89b12d3a456426614174000", "object": "model"},
	{"id": "minimax-m2.5-fc_c99ea14f526548a8bc28717d4e854123", "object": "model"},
	{"id": "minimax-m3_a8f465fbedb942efadace88370ad7ff1", "object": "model"},
	{"id": "minimax-m2.7-fc_047f123f661c414bb253c05b605c391e", "object": "model"},
}

// Server 是 Comate 的本地 OpenAI 兼容服务：翻译代理转发 zulu serve 会话引擎。
type Server struct {
	ln        net.Listener
	srv       *http.Server
	startedAt time.Time

	zuluMu sync.Mutex
	zulu   *zuluProc

	modelsMu    sync.Mutex
	modelsCache []modelInfo
	modelsAt    time.Time
}

// NewServer 创建服务。
func NewServer() *Server {
	return &Server{startedAt: time.Now()}
}

// corsWith 给所有响应加 CORS 头：GUI 页面跑在 localhost:随机端口，
// fetch 127.0.0.1:8786 属跨域，没有这个头前端全部拉不到数据。
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

// handleHealth 返回本服务健康态：无登录态时 200 + {"status":"no_license"}；
// 否则 200 + {"status":"ok","uptimeSec":...}（供前端里程表）。
func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	if readLicense() == "" {
		_ = json.NewEncoder(w).Encode(map[string]any{"status": "no_license"})
		return
	}
	_ = json.NewEncoder(w).Encode(map[string]any{
		"status":    "ok",
		"service":   "comate",
		"uptimeSec": int64(time.Since(s.startedAt).Seconds()),
	})
}

// ensureZulu 保证内部 zulu serve 在线（复用或 spawn）。并发安全。
func (s *Server) ensureZulu(zuluPath, license string) error {
	s.zuluMu.Lock()
	defer s.zuluMu.Unlock()
	if zuluHealth() {
		return nil // 已在线：复用自己 spawn 的或外部实例，不 kill
	}
	if s.zulu != nil {
		s.zulu.stop() // 自己 spawn 的已死：清掉记录再重拉
		s.zulu = nil
	}
	z, err := ensureZuluServe(zuluPath, license)
	if err != nil {
		return err
	}
	s.zulu = z
	return nil
}

// cachedModels 优先调用 zulu list-model 取实时目录（缓存 10 分钟）。
func (s *Server) cachedModels(zuluPath, license string) ([]modelInfo, error) {
	s.modelsMu.Lock()
	defer s.modelsMu.Unlock()
	if s.modelsCache != nil && time.Since(s.modelsAt) < 10*time.Minute {
		return s.modelsCache, nil
	}
	list, err := listModels(zuluPath, license)
	if err != nil {
		return nil, err
	}
	s.modelsCache = list
	s.modelsAt = time.Now()
	return list, nil
}

// handleModels 返回 OpenAI models 格式目录：首条 auto，其后为 list-model 实时目录
// 或硬编码 fallback。对外只暴露去后缀短名（deepseek-v4-flash_a866... → deepseek-v4-flash），
// 长名由 handleChat 的 resolveModel 负责还原。
func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	data := []map[string]any{{"id": "auto", "display_name": "Auto-Free", "object": "model"}}
	if license, zuluPath := readLicense(), findZulu(); license != "" && zuluPath != "" {
		if list, err := s.cachedModels(zuluPath, license); err == nil {
			for _, m := range list {
				entry := map[string]any{"id": shortModelID(m.ModelID), "object": "model"}
				if m.DisplayName != "" {
					entry["display_name"] = m.DisplayName
				}
				data = append(data, entry)
			}
		} else {
			log.Printf("comate-plugin: list-model 失败，回退硬编码目录: %v", err)
			data = append(data, fallbackShortModels()...)
		}
	} else {
		data = append(data, fallbackShortModels()...)
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{"object": "list", "data": data})
}

// shortModelID 去掉服务端内部后缀：deepseek-v4-flash_a866... → deepseek-v4-flash。
func shortModelID(id string) string {
	if i := strings.IndexByte(id, '_'); i > 0 {
		return id[:i]
	}
	return id
}

// fallbackShortModels 硬编码目录的去后缀形态（/v1/models 展示用）。
func fallbackShortModels() []map[string]any {
	out := make([]map[string]any, 0, len(fallbackModels))
	for _, m := range fallbackModels {
		if id, _ := m["id"].(string); id != "" {
			out = append(out, map[string]any{"id": shortModelID(id), "object": "model"})
		}
	}
	return out
}

// fullModelList 当前可用的完整 modelId 列表（实时优先，失败回退硬编码）。
func (s *Server) fullModelList(zuluPath, license string) []string {
	if zuluPath != "" && license != "" {
		if list, err := s.cachedModels(zuluPath, license); err == nil {
			out := make([]string, 0, len(list))
			for _, m := range list {
				out = append(out, m.ModelID)
			}
			return out
		}
	}
	out := make([]string, 0, len(fallbackModels))
	for _, m := range fallbackModels {
		if id, _ := m["id"].(string); id != "" {
			out = append(out, id)
		}
	}
	return out
}

// resolveModel 把调用方请求的模型名解析成上游完整 modelId：
// auto/auto-free/空 原样返回（由调用方决定是否带 model 字段）；
// 完整 id 精确命中原样通过；短名（忽略大小写）命中补全后缀；
// 未知名原样透传，交由上游校验报错。
func (s *Server) resolveModel(zuluPath, license, requested string) string {
	if requested == "" || requested == "auto" || requested == "auto-free" {
		return requested
	}
	list := s.fullModelList(zuluPath, license)
	for _, id := range list {
		if id == requested {
			return id
		}
	}
	for _, id := range list {
		if strings.EqualFold(shortModelID(id), requested) {
			return id
		}
	}
	return requested
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

// postUpstream 向 zulu 会话引擎发起会话请求（超时 300s，真实推理 30~90s 含思考）。
func postUpstream(body map[string]any) (*http.Response, error) {
	buf, _ := json.Marshal(body)
	client := &http.Client{Timeout: 300 * time.Second}
	return client.Post(upstreamBase+"/api/v1/conversations/init", "application/json", bytes.NewReader(buf))
}

// handleChat /v1/chat/completions 入口：解析 OpenAI body → 扁平化 query → 转发 zulu → 翻译回 OpenAI。
func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	start := time.Now()
	if r.Method != http.MethodPost {
		writeError(w, http.StatusMethodNotAllowed, "仅支持 POST", "invalid_request")
		return
	}
	license := readLicense()
	if license == "" {
		log.Printf("comate-plugin: %s %s model=- 503 no_license %s", r.Method, r.URL.Path, time.Since(start))
		writeError(w, http.StatusServiceUnavailable, "未找到 Comate 登录态，请先在 Comate IDE 或 zulu CLI 登录", "no_license")
		return
	}
	zuluPath := findZulu()
	if zuluPath == "" {
		log.Printf("comate-plugin: %s %s model=- 503 no_zulu %s", r.Method, r.URL.Path, time.Since(start))
		writeError(w, http.StatusServiceUnavailable, "未找到 zulu CLI，请先安装 Comate（文心快码 AI IDE）", "no_license")
		return
	}
	var req chatRequest
	if err := json.NewDecoder(io.LimitReader(r.Body, 32<<20)).Decode(&req); err != nil {
		log.Printf("comate-plugin: %s %s model=- 400 %s", r.Method, r.URL.Path, time.Since(start))
		writeError(w, http.StatusBadRequest, "请求体解析失败: "+err.Error(), "invalid_request")
		return
	}
	if err := s.ensureZulu(zuluPath, license); err != nil {
		log.Printf("comate-plugin: %s %s model=%s 503 %s", r.Method, r.URL.Path, req.Model, time.Since(start))
		writeError(w, http.StatusServiceUnavailable, err.Error(), "upstream_error")
		return
	}

	body := map[string]any{
		"query":   flattenMessages(req.Messages),
		"cwd":     os.TempDir(), // cwd 固定值即可（插件进程工作目录或系统临时目录）
		"license": license,
	}
	model := s.resolveModel(zuluPath, license, req.Model)
	if model != "" && model != "auto" && model != "auto-free" {
		body["model"] = model // 短名已还原成完整 modelId；未知名原样透传交上游校验
	}

	if req.Stream {
		s.handleChatStream(w, r, body, req.Model, start)
	} else {
		s.handleChatOnce(w, r, body, req.Model, start)
	}
}

// handleChatOnce 非流式：等上游 completed 事件，用其 content 组完整响应。
func (s *Server) handleChatOnce(w http.ResponseWriter, r *http.Request, body map[string]any, model string, start time.Time) {
	created := time.Now().Unix()
	id := fmt.Sprintf("chatcmpl-comate-%d", rand.Int63())
	if model == "" {
		model = "auto"
	}
	resp, err := postUpstream(body)
	if err != nil {
		log.Printf("comate-plugin: %s %s model=%s 502 %s", r.Method, r.URL.Path, model, time.Since(start))
		writeError(w, http.StatusBadGateway, "上游连接失败: "+err.Error(), "upstream_error")
		return
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		msg := strings.TrimSpace(string(b))
		log.Printf("comate-plugin: %s %s model=%s 502 上游HTTP%d %s", r.Method, r.URL.Path, model, resp.StatusCode, time.Since(start))
		writeError(w, http.StatusBadGateway, fmt.Sprintf("上游 HTTP %d: %s", resp.StatusCode, msg), "upstream_error")
		return
	}
	content, errMsg := readFullAnswer(resp.Body)
	if errMsg != "" {
		log.Printf("comate-plugin: %s %s model=%s 502 %s", r.Method, r.URL.Path, model, time.Since(start))
		writeError(w, http.StatusBadGateway, errMsg, "upstream_error")
		return
	}
	w.Header().Set("Content-Type", "application/json")
	_, _ = w.Write(completeJSON(id, model, content, created))
	log.Printf("comate-plugin: %s %s model=%s 200 %s", r.Method, r.URL.Path, model, time.Since(start))
}

// readFullAnswer 读完整条上游 SSE，返回 completed 事件的 content（status=ok）；
// status!=ok 或 SSE 中断返回错误信息。
func readFullAnswer(r io.Reader) (string, string) {
	scanner := bufio.NewScanner(r)
	scanner.Buffer(make([]byte, 64*1024), 1024*1024)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		var ev sseEvent
		if json.Unmarshal([]byte(strings.TrimSpace(strings.TrimPrefix(line, "data:"))), &ev) != nil {
			continue
		}
		if ev.Kind != "completed" {
			continue
		}
		if ev.Status != "ok" {
			msg := ev.Status
			if ev.Content != "" {
				msg = ev.Content
			}
			return "", msg
		}
		return ev.Content, ""
	}
	if err := scanner.Err(); err != nil {
		return "", "上游 SSE 中断: " + err.Error()
	}
	return "", "上游会话结束但未收到 completed（SSE 中断）"
}

// handleChatStream 流式：把上游 text-delta / element-add 增量逐块转成 OpenAI chunk，
// 结束发 finish_reason=stop chunk + data: [DONE]；thinking-delta 忽略。
// 首个文本块落盘前若上游报错（completed.status!=ok / HTTP 错误 / SSE 中断），可回 502。
func (s *Server) handleChatStream(w http.ResponseWriter, r *http.Request, body map[string]any, model string, start time.Time) {
	created := time.Now().Unix()
	id := fmt.Sprintf("chatcmpl-comate-%d", rand.Int63())
	if model == "" {
		model = "auto"
	}
	resp, err := postUpstream(body)
	if err != nil {
		log.Printf("comate-plugin: %s %s model=%s stream 502 %s", r.Method, r.URL.Path, model, time.Since(start))
		writeError(w, http.StatusBadGateway, "上游连接失败: "+err.Error(), "upstream_error")
		return
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		msg := strings.TrimSpace(string(b))
		log.Printf("comate-plugin: %s %s model=%s stream 502 上游HTTP%d %s", r.Method, r.URL.Path, model, resp.StatusCode, time.Since(start))
		writeError(w, http.StatusBadGateway, fmt.Sprintf("上游 HTTP %d: %s", resp.StatusCode, msg), "upstream_error")
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	fl, _ := w.(http.Flusher)

	// committed=true 表示首个文本块已落盘（HTTP 200 已提交）。在此之前若上游报错可回 502。
	committed := false
	emit := func(delta string) {
		chunk := chunkJSON(id, model, delta, "", created)
		if !committed {
			committed = true
		}
		_, _ = w.Write([]byte("data: " + string(chunk) + "\n\n"))
		fl.Flush()
	}

	okDone := false
	upstreamErr := ""
	scanner := bufio.NewScanner(resp.Body)
	scanner.Buffer(make([]byte, 64*1024), 1024*1024)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		var ev sseEvent
		if json.Unmarshal([]byte(strings.TrimSpace(strings.TrimPrefix(line, "data:"))), &ev) != nil {
			continue
		}
		switch ev.Kind {
		case "delta-batch":
			for _, c := range ev.Chunks {
				if c.Kind == "text-delta" && c.Delta != "" {
					emit(c.Delta)
				}
			}
		case "element-add":
			if ev.Element != nil && ev.Element.Type == "TEXT" && ev.Element.Content != "" {
				emit(ev.Element.Content)
			}
		case "completed":
			if ev.Status == "ok" {
				okDone = true
			} else {
				upstreamErr = ev.Status
				if ev.Content != "" {
					upstreamErr = ev.Content
				}
			}
		}
	}
	if upstreamErr != "" {
		if !committed {
			log.Printf("comate-plugin: %s %s model=%s stream 502 %s", r.Method, r.URL.Path, model, time.Since(start))
			writeError(w, http.StatusBadGateway, upstreamErr, "upstream_error")
			return
		}
		log.Printf("comate-plugin: %s %s model=%s stream 200(部分) 上游错误:%s %s", r.Method, r.URL.Path, model, upstreamErr, time.Since(start))
	} else if !okDone && !committed {
		// SSE 中断且未发出任何内容 → 502
		log.Printf("comate-plugin: %s %s model=%s stream 502 SSE中断 %s", r.Method, r.URL.Path, model, time.Since(start))
		writeError(w, http.StatusBadGateway, "上游 SSE 中断", "upstream_error")
		return
	}
	// 结束块：finish_reason=stop + [DONE]
	stop := chunkJSON(id, model, "", "stop", created)
	_, _ = w.Write([]byte("data: " + string(stop) + "\n\n"))
	fl.Flush()
	_, _ = w.Write([]byte("data: [DONE]\n\n"))
	fl.Flush()
	log.Printf("comate-plugin: %s %s model=%s stream 200 %s", r.Method, r.URL.Path, model, time.Since(start))
}

// Start 在 host:port 上监听（阻塞）。点火（插件进程启动时）即异步拉起内部 zulu serve。
func (s *Server) Start(host, port string) error {
	if license, zuluPath := readLicense(), findZulu(); license != "" && zuluPath != "" {
		go func() {
			if err := s.ensureZulu(zuluPath, license); err != nil {
				log.Printf("comate-plugin: zulu serve 启动失败: %v", err)
			}
		}()
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
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

// Stop 停止本服务，并结束自己 spawn 的 zulu serve（复用的不 kill）。
func (s *Server) Stop() {
	if s.srv != nil {
		_ = s.srv.Close()
	}
	s.zuluMu.Lock()
	if s.zulu != nil {
		s.zulu.stop()
		s.zulu = nil
	}
	s.zuluMu.Unlock()
}
