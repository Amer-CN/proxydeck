package bai

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

// 上游 base URL：api.b.ai 是标准 OpenAI 兼容端点。
// 注意：部分客户端/网络栈（Node/Electron fetch）连 api.b.ai 时连接层会间歇失败，
// 而 Go 的 HTTP 栈实测最稳。本服务监听本地端口把请求用 Go 栈转发给上游。
const upstreamBase = "https://api.b.ai"

// Server 是 B.AI 的本地 OpenAI 兼容转发服务。
type Server struct {
	ln        net.Listener
	srv       *http.Server
	startedAt time.Time
	matrix    matrixState // 模型矩阵缓存（GUI 专用，见 models.go）

	statsMu   sync.Mutex
	stats     map[string]*modelStat // 按模型累计（GUI 消耗 TOP）
	statsPath string
}

// modelStat 单模型用量累计（字段名与 tuanjie/codebuddy 同形，GUI 共用一套渲染）。
type modelStat struct {
	Calls     int64 `json:"calls"`
	InputTok  int64 `json:"inputTokens"`
	OutputTok int64 `json:"outputTokens"`
	TotalTok  int64 `json:"totalTokens"`
}

// NewServer 创建服务。统计文件 bai-stats.json 与 exe 同目录（同 codebuddy 的落盘口径）。
func NewServer() *Server {
	s := &Server{startedAt: time.Now(), stats: map[string]*modelStat{}}
	if exe, err := os.Executable(); err == nil {
		s.statsPath = filepath.Join(filepath.Dir(exe), "bai-stats.json")
		s.loadStats()
	}
	return s
}

// corsWith 给所有响应加 CORS 头：GUI 页面跑在 localhost:随机端口，
// fetch 127.0.0.1:8891 属跨域，没有这个头前端全部拉不到数据。
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

// handleHealth 直接回 200（不转发上游，否则无 key 会 401 导致健康误判）。
func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{
		"status":    "ok",
		"service":   "bai-go",
		"uptimeSec": int64(time.Since(s.startedAt).Seconds()),
	})
}

// handleStats 返回消耗统计 + 运行时长（GUI 消耗 TOP 用，结构同 tuanjie/codebuddy）。
func (s *Server) handleStats(w http.ResponseWriter, r *http.Request) {
	s.statsMu.Lock()
	out := make(map[string]*modelStat, len(s.stats))
	for k, v := range s.stats {
		cp := *v
		out[k] = &cp
	}
	s.statsMu.Unlock()
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{
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
	log.Printf("bai-plugin: stat model=%s in=%d out=%d", model, in, out)
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

// Start 在 host:port 上监听（阻塞）。
// /health 与 /model/matrix（GUI 甲板用的带缓存旁路，见 models.go）由本服务自己应答，
// 其余路径——含 /v1/models——一律 ReverseProxy 到 https://api.b.ai，透传语义不变。
// Director 必须把 req.Host 改为 api.b.ai（Cloudflare 对不认识的主机名直接 403）；
// FlushInterval 50ms 保证 SSE 流式及时刷新。
func (s *Server) Start(host, port string) error {
	target, err := url.Parse(upstreamBase)
	if err != nil {
		return fmt.Errorf("上游地址解析失败: %v", err)
	}
	director := func(req *http.Request) {
		req.URL.Scheme = target.Scheme
		req.URL.Host = target.Host
		req.Host = target.Host // 必须改：Cloudflare 对未知 Host 直接 403
	}
	// api.b.ai 对某些网络（尤其国内直连）是间歇性失败的：连接层错误 / Cloudflare 520。
	// 520/502/503 意味着请求没到模型层（无计费），重放安全。
	// 前置 bufferBody 把请求体缓存为可重放（GetBody），POST 也能自动重试。
	retry := &retryRoundTripper{transport: detectUpstreamTransport(), max: 3}
	proxy := &httputil.ReverseProxy{
		Director:      director,
		Transport:     retry,
		FlushInterval: 50 * time.Millisecond, // SSE 流式及时刷新
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/model/matrix", s.handleModelMatrix) // GUI 甲板专用；/v1/models 仍是纯透传
	mux.HandleFunc("/v1/stats", s.handleStats)           // 本地统计（GUI 消耗 TOP）；不透传上游
	mux.Handle("/v1/models", proxy)
	mux.Handle("/v1/chat/completions", s.statsWith(adaptQuirks(proxy)))
	mux.Handle("/v1/", proxy)
	mux.Handle("/", proxy)

	ln, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return fmt.Errorf("端口 %s 被占用: %w", port, err)
	}
	s.ln = ln
	s.srv = &http.Server{Handler: corsWith(bufferBody(mux))}
	return s.srv.Serve(ln)
}

// detectUpstreamTransport 决定转发上游用的 HTTP Transport：
// 进程已有 HTTPS_PROXY/https_proxy 时直接用 http.DefaultTransport（Go 自动认环境变量，行为同现状）；
// 否则顺次 TCP 试连常见本地代理端口，第一个连通的作为代理（仅作用于本 Transport，不改全局环境变量）；
// 全部不通则沿用 http.DefaultTransport 直连。结果写一行日志。
func detectUpstreamTransport() http.RoundTripper {
	if os.Getenv("HTTPS_PROXY") != "" || os.Getenv("https_proxy") != "" {
		log.Printf("bai-plugin: 检测到 HTTPS_PROXY/https_proxy 环境变量，使用 http.DefaultTransport（按环境变量走代理）")
		return http.DefaultTransport
	}
	for _, p := range []string{"7897", "7890", "7892", "7896", "10809", "1080"} {
		addr := net.JoinHostPort("127.0.0.1", p)
		conn, err := net.DialTimeout("tcp", addr, 300*time.Millisecond)
		if err != nil {
			continue
		}
		_ = conn.Close()
		proxyURL, _ := url.Parse("http://" + addr)
		base := http.DefaultTransport.(*http.Transport)
		tr := base.Clone() // 复用默认超时等行为，仅覆盖 Proxy
		tr.Proxy = http.ProxyURL(proxyURL)
		log.Printf("bai-plugin: 使用本地代理 %s", addr)
		return tr
	}
	log.Printf("bai-plugin: 未发现本地代理，上游直连")
	return http.DefaultTransport
}

// baiContextLimit B.ai DeepSeek 系列实测上下文上限（约 1M token：
// 6M 字符进 75 万正常、8M 字符被 400 拒）。zcode 侧配置 context=1000000。
const baiContextLimit = 1_000_000

// estTokens 保守估算 messages 占用的 token 数（4 字符 ≈ 1 token，
// 中文/英文混合的通用保守值；宁可高估多压缩，也要保证不超窗被上游拒）。
func estTokens(msgs []any) int {
	total := 0
	for _, raw := range msgs {
		msg, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		switch c := msg["content"].(type) {
		case string:
			total += len([]rune(c)) / 4
		case []any:
			for _, part := range c {
				if p, ok := part.(map[string]any); ok {
					if t, _ := p["text"].(string); t != "" {
						total += len([]rune(t)) / 4
					} else if img, ok := p["image_url"].(map[string]any); ok {
						// 图片按固定代价估算（data URI 的 base64 按体积折算进窗口）
						if u, _ := img["url"].(string); strings.HasPrefix(u, "data:image") {
							total += len(u) / 4 / 8 // base64 文本的 token 代价
						} else {
							total += 512 // 外链图固定估 512 token
						}
					}
				}
			}
		}
		// 工具调用：arguments 常含大段代码/文件内容，漏算会低估窗口导致截断后仍超窗
		if tcs, ok := msg["tool_calls"].([]any); ok {
			for _, rawTc := range tcs {
				if tc, ok := rawTc.(map[string]any); ok {
					if fn, ok := tc["function"].(map[string]any); ok {
						if args, _ := fn["arguments"].(string); args != "" {
							total += len([]rune(args)) / 4
						}
					}
				}
			}
		}
		// 每个消息的 overhead（角色名/结构 ≈ 4 token）
		total += 4
	}
	return total
}

// truncateToContext 超窗时从最早的非 system 轮次截起（保留 system 与最近
// 轮次），直到估算达标或只剩 system+最后一条。返回 (新messages, 是否截断)。
func truncateToContext(msgs []any) ([]any, bool) {
	if estTokens(msgs) <= baiContextLimit {
		return msgs, false
	}
	out := make([]any, 0, len(msgs))
	var tail []any
	for i, raw := range msgs {
		if msg, ok := raw.(map[string]any); ok {
			if role, _ := msg["role"].(string); role == "system" {
				out = append(out, raw) // system 永远保留
				continue
			}
		}
		if i == len(msgs)-1 {
			out = append(out, raw) // 最后一条（当前问题）永远保留
			continue
		}
		tail = append(tail, raw)
	}
	// 从最老的轮次截起：从尾部倒推"还能保留多少最近的轮次"，
	// 保证截断后保留尽量多的有效上下文（而不是全删只剩头尾）。
	keepStart := len(tail) // 默认全删
	acc := estTokens(out)  // system + 最后一条
	for i := len(tail) - 1; i >= 0; i-- {
		one := estTokens([]any{tail[i]})
		if acc+one > baiContextLimit {
			break
		}
		acc += one
		keepStart = i
	}
	// 配对保护：保留序列开头不能是孤立 role=tool 消息（其配对的
	// assistant.tool_calls 已被删则上游必 400）——跨过它们（tool 响应通常很小）
	for keepStart < len(tail) {
		if mm, ok := tail[keepStart].(map[string]any); ok {
			if role, _ := mm["role"].(string); role == "tool" {
				keepStart++
				continue
			}
		}
		break
	}
	out = append(out, tail[keepStart:]...) // 保留最近的轮次，顺序不变
	if estTokens(out) > baiContextLimit {
		return out, true // 最后一条本身超窗的极端情况：尽力截断，由上游判
	}
	return out, true
}

// adaptQuirks 适配 b.ai 的协议怪癖（DSH 接入包实战经验 + 官方 API 常见约束）：
//  1. 不认 developer 角色 → 降级为 system（否则 400）；
//  2. reasoning_effort 只认 off/low/medium/high → max 改写成 high；
//  3. max_tokens 超过 8192 钳到 8192（b.ai 上限，超发会被拒）；
//  4. messages 超上下文窗口（约 1M token）→ 截断最早对话轮次，防上游
//     "Input token exceed the limit" 400（即用户遇到的 ZCode 报错）。
// 只动 JSON 请求体；改写后同步 Content-Length。解析失败的原样放行（让上游报错）。
func adaptQuirks(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodPost && r.Body != nil {
			if buf, err := io.ReadAll(io.LimitReader(r.Body, 32<<20)); err == nil {
				var m map[string]any
				if json.Unmarshal(buf, &m) == nil {
					changed := false
					if msgs, ok := m["messages"].([]any); ok {
						cut := false
						for _, raw := range msgs {
							if msg, ok := raw.(map[string]any); ok {
								if role, _ := msg["role"].(string); role == "developer" {
									msg["role"] = "system"
									changed = true
								}
							}
						}
						msgs, cut = truncateToContext(msgs)
						if cut {
							m["messages"] = msgs
							changed = true
							log.Printf("bai-plugin: 上下文超窗已截断最早对话轮次（截断后估算 %d token ≤ %d）", estTokens(msgs), baiContextLimit)
						}
					}
					if eff, ok := m["reasoning_effort"].(string); ok && eff == "max" {
						m["reasoning_effort"] = "high"
						changed = true
					}
					if mt, ok := m["max_tokens"].(float64); ok && mt > 8192 {
						m["max_tokens"] = 8192
						changed = true
					}
					if changed {
						if nb, err := json.Marshal(m); err == nil {
							buf = nb
						}
					}
				}
				r.Body = io.NopCloser(bytes.NewReader(buf))
				r.ContentLength = int64(len(buf))
				r.Header.Set("Content-Length", strconv.Itoa(len(buf)))
			}
		}
		next.ServeHTTP(w, r)
	})
}

// statsWith 只读统计中间件（/v1/chat/completions 专用）：
// 请求体里取 model（只读，不改写请求体），响应体旁路拷贝一份——流式是 SSE 的
// data: 行、非流式是整段 JSON——从 usage 字段入账，供 /v1/stats 与 GUI 消耗 TOP 用。
// 对请求/响应字节一律原样透传（只统计，不改语义）；写盘失败/解析失败只丢统计不报错。
func (s *Server) statsWith(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		model := peekModel(r)
		cw := &captureWriter{ResponseWriter: w}
		next.ServeHTTP(cw, r)
		if model == "" {
			return
		}
		if u, ok := extractUsage(cw.cap()); ok {
			s.addStat(model, u.PromptTokens, u.CompletionTokens, u.TotalTokens)
		}
	})
}

// captureWriter 包一层 ResponseWriter：原样写下游，同时旁路留一份响应体副本（上限 8MB）。
// Flush/Unwrap 透传，保证 ReverseProxy 的流式刷新与 http.ResponseController 行为不变。
type captureWriter struct {
	http.ResponseWriter
	buf    bytes.Buffer
	tooBig bool
}

func (c *captureWriter) Write(p []byte) (int, error) {
	if !c.tooBig {
		if c.buf.Len()+len(p) > 8<<20 {
			c.tooBig = true
			c.buf.Reset()
		} else {
			c.buf.Write(p)
		}
	}
	return c.ResponseWriter.Write(p)
}

func (c *captureWriter) Flush() {
	if f, ok := c.ResponseWriter.(http.Flusher); ok {
		f.Flush()
	}
}

func (c *captureWriter) Unwrap() http.ResponseWriter { return c.ResponseWriter }

// cap 取副本；超限视为无统计（不半截解析出错误数字）。
func (c *captureWriter) cap() []byte {
	if c.tooBig {
		return nil
	}
	return c.buf.Bytes()
}

// peekModel 从请求体只读取 model 字段（不消费请求体，正文原样留给下游）。
func peekModel(r *http.Request) string {
	if r.Body == nil {
		return ""
	}
	buf, err := io.ReadAll(io.LimitReader(r.Body, 32<<20))
	_ = r.Body.Close()
	r.Body = io.NopCloser(bytes.NewReader(buf))
	if err != nil {
		return ""
	}
	var m struct {
		Model string `json:"model"`
	}
	if json.Unmarshal(buf, &m) != nil {
		return ""
	}
	return m.Model
}

// tokenUsage 上游 usage 字段（OpenAI 口径）。
type tokenUsage struct {
	PromptTokens     int64 `json:"prompt_tokens"`
	CompletionTokens int64 `json:"completion_tokens"`
	TotalTokens      int64 `json:"total_tokens"`
}

// extractUsage 从响应体副本里抠 usage：
// 非流式是整段 chat.completion JSON；流式是 SSE（每行 data: {...}），
// stream_options.include_usage 时末帧带 usage（[DONE] 行跳过）。
// 流式只取最后一个含 usage 的帧——累计口径的 usage 只出现一次。
func extractUsage(body []byte) (tokenUsage, bool) {
	var full struct {
		Usage *tokenUsage `json:"usage"`
	}
	if json.Unmarshal(bytes.TrimSpace(body), &full) == nil && full.Usage != nil {
		return *full.Usage, true
	}
	buf := bytes.NewReader(body)
	sc := bufio.NewScanner(buf)
	sc.Buffer(make([]byte, 64*1024), 4<<20)
	var got tokenUsage
	found := false
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		payload := strings.TrimSpace(line[len("data:"):])
		if payload == "" || payload == "[DONE]" {
			continue
		}
		var chunk struct {
			Usage *tokenUsage `json:"usage"`
		}
		if json.Unmarshal([]byte(payload), &chunk) == nil && chunk.Usage != nil {
			got = *chunk.Usage
			found = true
		}
	}
	return got, found
}

// bufferBody 把带请求体的请求读入内存并设 GetBody（可重放），重试的前提。
// 聊天请求都是小 JSON（< 数 MB），缓冲代价可忽略；超限（>32MB）或读取失败的不缓冲（单发）。
func bufferBody(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Body == nil || r.Method == http.MethodGet || r.Method == http.MethodHead {
			next.ServeHTTP(w, r)
			return
		}
		const limit = 32 << 20
		buf, err := io.ReadAll(io.LimitReader(r.Body, limit+1))
		_ = r.Body.Close()
		r.Body = io.NopCloser(bytes.NewReader(buf))
		if err == nil && len(buf) <= limit {
			r.GetBody = func() (io.ReadCloser, error) {
				return io.NopCloser(bytes.NewReader(buf)), nil
			}
		}
		next.ServeHTTP(w, r)
	})
}

// retryRoundTripper 对可重放的请求做有限次重试：传输错误 / 5xx（含 Cloudflare 520）。
// 只在请求体可重放（GetBody 非空或无请求体）时重试；4xx 不重试（真实 API 应答）。
type retryRoundTripper struct {
	transport http.RoundTripper
	max       int
}

func (r *retryRoundTripper) RoundTrip(req *http.Request) (*http.Response, error) {
	if req.Body != nil && req.GetBody == nil {
		return r.transport.RoundTrip(req) // 不可重放：单发
	}
	var lastErr error
	for i := 0; i < r.max; i++ {
		last := i == r.max-1
		if i > 0 && req.GetBody != nil {
			if nb, err := req.GetBody(); err == nil {
				req.Body = nb
			}
		}
		resp, err := r.transport.RoundTrip(req)
		if err != nil {
			lastErr = err
			if last {
				return nil, err
			}
			time.Sleep(300 * time.Millisecond)
			continue
		}
		if resp.StatusCode < 500 || last {
			return resp, nil
		}
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 8192)) // 5xx：丢弃响应体后重试
		_ = resp.Body.Close()
		time.Sleep(300 * time.Millisecond)
	}
	return nil, lastErr
}

// Stop 停止服务。
func (s *Server) Stop() {
	if s.srv != nil {
		_ = s.srv.Close()
	}
}