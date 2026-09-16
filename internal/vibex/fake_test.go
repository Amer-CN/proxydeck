// fake_test.go —— 单测用的本地假 VibeX 上游（REST + WS），不需要真实 token。
package vibex

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

const testAppID = "app-1"

// wsRecord 记录一条 WS 连接上收到的客户端消息（按到达顺序）。
type wsRecord struct {
	mu   sync.Mutex
	msgs []string
}

func (r *wsRecord) add(s string) {
	r.mu.Lock()
	r.msgs = append(r.msgs, s)
	r.mu.Unlock()
}

func (r *wsRecord) all() []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	return append([]string(nil), r.msgs...)
}

// hasType 报告是否收到过指定 type 的消息。
func (r *wsRecord) hasType(want string) bool {
	for _, m := range r.all() {
		msg := toMap(jsonUnmarshal([]byte(m)))
		if str(msg["type"]) == want {
			return true
		}
	}
	return false
}

// promptTexts 取所有 prompt 消息的 text。
func (r *wsRecord) promptTexts() []string {
	var out []string
	for _, m := range r.all() {
		msg := toMap(jsonUnmarshal([]byte(m)))
		if str(msg["type"]) == "prompt" {
			out = append(out, str(msg["text"]))
		}
	}
	return out
}

// fakeUpstream 是一个假上游：REST 走 /api/*，WS 走 /app-ws/{id}/ws。
type fakeUpstream struct {
	t   *testing.T
	srv *httptest.Server

	mu            sync.Mutex
	authTokens    []string            // HTTP Authorization 里出现的 token（按到达顺序）
	cookieTokens  []string            // WS Cookie 里的 token
	consecutiveWS []*wsRecord         // 每条 WS 连接的记录
	providerHits  int                 // /api/llm-providers 命中次数（验缓存）
	reject        map[string]int      // token → REST 直接返回该 HTTP 状态
	wsReject      map[string]int      // token → WS 握手直接返回该 HTTP 状态
	extra         map[string][]string // 额外回放：prompt → 一段文本增量
}

func newFakeUpstream(t *testing.T) *fakeUpstream {
	t.Helper()
	f := &fakeUpstream{
		t: t, reject: map[string]int{}, wsReject: map[string]int{}, extra: map[string][]string{},
	}
	f.srv = httptest.NewServer(http.HandlerFunc(f.handle))
	t.Cleanup(f.srv.Close)
	return f
}

func (f *fakeUpstream) url() string { return f.srv.URL }

func bearer(r *http.Request) string {
	return strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
}

func cookieToken(r *http.Request) string {
	c, err := r.Cookie("Rh-Accesstoken")
	if err != nil {
		return ""
	}
	return c.Value
}

func (f *fakeUpstream) handle(w http.ResponseWriter, r *http.Request) {
	if strings.HasPrefix(r.URL.Path, "/app-ws/") {
		f.handleWS(w, r)
		return
	}
	tok := bearer(r)
	f.mu.Lock()
	f.authTokens = append(f.authTokens, tok)
	if code, ok := f.reject[tok]; ok {
		f.mu.Unlock()
		w.WriteHeader(code)
		if code == http.StatusUnauthorized {
			_, _ = w.Write([]byte(`{"code":403,"msg":"TOKEN_MISSION"}`))
		}
		return
	}
	if r.URL.Path == "/api/llm-providers" {
		f.providerHits++
	}
	f.mu.Unlock()

	switch {
	case r.URL.Path == "/api/llm-providers":
		writeJSON(w, 200, map[string]any{"providers": []any{
			map[string]any{"id": "lite-1", "billing_tier": "lite", "display_name": "Lite One", "description": "免费档"},
			map[string]any{"id": "pro-1", "billing_tier": "pro", "display_name": "Pro One"},
		}})
	case r.URL.Path == "/api/apps":
		writeJSON(w, 200, map[string]any{"apps": []any{fakeApp()}})
	case strings.HasPrefix(r.URL.Path, "/api/apps/") && strings.HasSuffix(r.URL.Path, "/start"):
		writeJSON(w, 200, map[string]any{"code": 0})
	case strings.HasPrefix(r.URL.Path, "/api/apps/") && strings.HasSuffix(r.URL.Path, "/llm-provider"):
		writeJSON(w, 200, map[string]any{"code": 0})
	case strings.HasPrefix(r.URL.Path, "/api/apps/") && strings.HasSuffix(r.URL.Path, "/ensure-llm-settings"):
		writeJSON(w, 200, map[string]any{"code": 0})
	case strings.HasPrefix(r.URL.Path, "/api/apps/"):
		writeJSON(w, 200, fakeApp())
	default:
		writeJSON(w, 404, map[string]any{"code": 404, "msg": "not found"})
	}
}

// fakeApp 返回一个「已 running 且端口就绪」的沙箱项目对象。
func fakeApp() map[string]any {
	return map[string]any{
		"app_id": testAppID, "name": "vibex2api", "app_type": "web",
		"live":       map[string]any{"status": "running"},
		"host_ports": map[string]any{"9000": 31000, "7000": 31001},
		"created_at": "2026-09-16T00:00:00Z",
	}
}

func (f *fakeUpstream) handleWS(w http.ResponseWriter, r *http.Request) {
	rec := &wsRecord{}
	tok := cookieToken(r)
	f.mu.Lock()
	f.cookieTokens = append(f.cookieTokens, tok)
	f.consecutiveWS = append(f.consecutiveWS, rec)
	code, rejected := f.wsReject[tok]
	f.mu.Unlock()

	hj, ok := w.(http.Hijacker)
	if !ok {
		f.t.Error("httptest 不支持 Hijack")
		return
	}
	conn, rw, err := hj.Hijack()
	if err != nil {
		return
	}
	defer func() { _ = conn.Close() }()
	if rejected {
		fmt.Fprintf(rw, "HTTP/1.1 %d Rejected\r\nContent-Length: 0\r\n\r\n", code)
		_ = rw.Flush()
		return
	}
	fmt.Fprintf(rw, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"+
		"Sec-WebSocket-Accept: %s\r\n\r\n", wsAccept(r.Header.Get("Sec-WebSocket-Key")))
	if err := rw.Flush(); err != nil {
		return
	}
	turn := 0
	for {
		_ = conn.SetReadDeadline(time.Now().Add(10 * time.Second))
		op, _, payload, err := readFrame(rw.Reader)
		if err != nil {
			return
		}
		switch op {
		case wsOpClose:
			return
		case wsOpPing:
			_ = writeFrame(rw, wsOpPong, payload, false)
			_ = rw.Flush()
		case wsOpText:
			rec.add(string(payload))
			msg := toMap(jsonUnmarshal(payload))
			switch str(msg["type"]) {
			case "init", "init_root":
				f.send(rw, map[string]any{"type": "ready", "cwd": wsInitCwd})
			case "prompt":
				f.replay(rw, str(msg["text"]), turn)
				turn++
			}
		}
	}
}

// replay 按 SPEC §3.6 的形状回放：text_delta → result(usage) → done。
func (f *fakeUpstream) replay(rw *bufio.ReadWriter, prompt string, turn int) {
	pieces := []string{"你好", "世界"}
	f.mu.Lock()
	if extra, ok := f.extra[prompt]; ok {
		pieces = extra
	}
	f.mu.Unlock()
	for _, piece := range pieces {
		f.send(rw, map[string]any{"type": "claude_event", "event": map[string]any{
			"type": "stream_event",
			"event": map[string]any{
				"type":  "content_block_delta",
				"index": 0,
				"delta": map[string]any{"type": "text_delta", "text": piece},
			},
		}})
	}
	f.send(rw, map[string]any{"type": "claude_event", "event": map[string]any{
		"type": "result", "subtype": "success", "result": strings.Join(pieces, ""),
		"usage": map[string]any{"input_tokens": 12, "output_tokens": 7},
	}})
	f.send(rw, map[string]any{"type": "done", "turn": turn})
}

func (f *fakeUpstream) send(rw *bufio.ReadWriter, msg map[string]any) {
	b, err := json.Marshal(msg)
	if err != nil {
		f.t.Error(err)
		return
	}
	if err := writeFrame(rw, wsOpText, b, false); err != nil {
		return
	}
	_ = rw.Flush()
}

// wsConn 返回第 i 条 WS 连接的记录（nil = 还没连上）。
func (f *fakeUpstream) wsConn(i int) *wsRecord {
	f.mu.Lock()
	defer f.mu.Unlock()
	if i >= len(f.consecutiveWS) {
		return nil
	}
	return f.consecutiveWS[i]
}

func (f *fakeUpstream) authSeen() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.authTokens...)
}

func (f *fakeUpstream) cookieSeen() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.cookieTokens...)
}

func (f *fakeUpstream) hits() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.providerHits
}

func (f *fakeUpstream) connCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return len(f.consecutiveWS)
}

// newTestServer 建一个指向假上游的 vibex 服务（token 由调用方给）。
func newTestServer(t *testing.T, baseURL string, tokens ...string) *Server {
	t.Helper()
	cfg := &Config{
		BaseURL: baseURL, AppName: "vibex2api", AppType: "web",
		FreshSession: true, TurnTimeoutSec: 20, ReadyTimeoutSec: 8,
		QueueLimit: 4, HeartbeatSec: 15, ConnectMode: "auto",
	}
	pool := NewPool(tokens, tokenCooldown)
	client := NewClient(cfg, pool)
	return &Server{
		cfg: cfg, pool: pool, client: client,
		engine: NewEngine(cfg, client, pool),
		queue:  make(chan struct{}, cfg.QueueLimit), startedAt: time.Now(),
	}
}

// newTestHTTP 把测试服务挂到 httptest 上。
func newTestHTTP(t *testing.T, s *Server) *httptest.Server {
	t.Helper()
	ts := httptest.NewServer(s.handler())
	t.Cleanup(ts.Close)
	return ts
}
