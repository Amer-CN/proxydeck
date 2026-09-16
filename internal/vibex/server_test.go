// server_test.go —— 端到端单测：假上游 + 本地服务（/health、/v1/models、/v1/chat/completions）。
package vibex

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func getBody(t *testing.T, url string) (int, string) {
	t.Helper()
	resp, err := http.Get(url)
	if err != nil {
		t.Fatalf("GET %s 失败: %v", url, err)
	}
	defer func() { _ = resp.Body.Close() }()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}

func postChat(t *testing.T, url, body string) (int, string) {
	t.Helper()
	resp, err := http.Post(url+"/v1/chat/completions", "application/json", strings.NewReader(body))
	if err != nil {
		t.Fatalf("POST chat 失败: %v", err)
	}
	defer func() { _ = resp.Body.Close() }()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}

// 验收 1：无 token 时 /health 仍 200 且 token_configured:false。
func TestHealthWithoutToken(t *testing.T) {
	s := newTestServer(t, "http://127.0.0.1:1")
	ts := newTestHTTP(t, s)
	code, body := getBody(t, ts.URL+"/health")
	if code != http.StatusOK {
		t.Fatalf("/health 应 200，得到 %d：%s", code, body)
	}
	var got map[string]any
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatalf("/health 不是 JSON: %v (%s)", err, body)
	}
	if got["token_configured"] != false {
		t.Fatalf("token_configured 应为 false: %s", body)
	}
	if got["status"] != "ok" || got["service"] != "vibex-go" {
		t.Fatalf("/health 字段不对: %s", body)
	}
}

// 验收 2：无 token 时 /v1/models 401 + Rh-Accesstoken 指引；空 body 的 chat 400。
func TestNoTokenModelsAndEmptyChatBody(t *testing.T) {
	s := newTestServer(t, "http://127.0.0.1:1")
	ts := newTestHTTP(t, s)
	code, body := getBody(t, ts.URL+"/v1/models")
	if code != http.StatusUnauthorized {
		t.Fatalf("/v1/models 应 401，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, "Rh-Accesstoken") {
		t.Fatalf("/v1/models 错误信息应含 Rh-Accesstoken 指引: %s", body)
	}
	if !strings.Contains(body, "TOKEN_NOT_CONFIGURED") {
		t.Fatalf("错误码不对: %s", body)
	}
	code, body = postChat(t, ts.URL, "")
	if code != http.StatusBadRequest {
		t.Fatalf("空 body 应 400，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, "BAD_REQUEST") {
		t.Fatalf("空 body 错误码不对: %s", body)
	}
}

// /v1/models：正常返回 + 5 分钟内存缓存（?refresh=1 旁路）。
func TestModelsCacheAndRefresh(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	ts := newTestHTTP(t, s)

	code, body := getBody(t, ts.URL+"/v1/models")
	if code != http.StatusOK {
		t.Fatalf("/v1/models 应 200，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, `"id":"lite-1"`) || !strings.Contains(body, `"owned_by":"vibex"`) {
		t.Fatalf("模型列表形状不对: %s", body)
	}
	if hits := up.hits(); hits != 1 {
		t.Fatalf("首次应打上游 1 次，实际 %d", hits)
	}
	if code, _ := getBody(t, ts.URL+"/v1/models"); code != http.StatusOK {
		t.Fatalf("第二次 /v1/models 应 200，得到 %d", code)
	}
	if hits := up.hits(); hits != 1 {
		t.Fatalf("缓存期内不应再打上游，实际 %d 次", hits)
	}
	if code, _ := getBody(t, ts.URL+"/v1/models?refresh=1"); code != http.StatusOK {
		t.Fatalf("refresh=1 应 200，得到 %d", code)
	}
	if hits := up.hits(); hits != 2 {
		t.Fatalf("refresh=1 应旁路缓存再打一次，实际 %d 次", hits)
	}
}

// 验收 3 之一：假上游 401 → 自动换号（401 标死，下一个 token 顶上）。
func TestTokenRotationOn401(t *testing.T) {
	up := newFakeUpstream(t)
	up.reject["dead-token"] = http.StatusUnauthorized
	s := newTestServer(t, up.url(), "dead-token", "good-token")
	ts := newTestHTTP(t, s)

	code, body := getBody(t, ts.URL+"/v1/models")
	if code != http.StatusOK {
		t.Fatalf("换号后应 200，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, `"id":"lite-1"`) {
		t.Fatalf("换号后应拿到模型列表: %s", body)
	}
	seen := up.authSeen()
	if len(seen) < 2 || seen[0] != "dead-token" || seen[1] != "good-token" {
		t.Fatalf("应先用 dead-token 再换 good-token，实际 %v", seen)
	}
	snap := s.pool.snapshot()
	if len(snap) != 2 {
		t.Fatalf("池应有 2 个 token: %+v", snap)
	}
	if snap[0]["dead"] != true || snap[0]["available"] != false {
		t.Fatalf("401 应把第一个 token 标死: %+v", snap[0])
	}
	if snap[1]["dead"] == true {
		t.Fatalf("第二个 token 不应被标死: %+v", snap[1])
	}
}

// 验收 3 之一：假 WS 回放 text_delta → SSE 直到 data: [DONE]。
func TestChatStreamSSE(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	ts := newTestHTTP(t, s)

	code, body := postChat(t, ts.URL,
		`{"model":"lite-1","messages":[{"role":"user","content":"打个招呼"}],"stream":true}`)
	if code != http.StatusOK {
		t.Fatalf("流式应 200，得到 %d：%s", code, body)
	}
	if !strings.HasPrefix(body, "data: ") {
		t.Fatalf("不是 SSE: %q", body)
	}
	for _, needle := range []string{`"role":"assistant"`, `"content":"你好"`, `"content":"世界"`, `"finish_reason":"stop"`} {
		if !strings.Contains(body, needle) {
			t.Fatalf("SSE 缺 %s\n%s", needle, body)
		}
	}
	if !strings.HasSuffix(strings.TrimRight(body, "\n"), "data: [DONE]") {
		t.Fatalf("SSE 未以 data: [DONE] 收尾:\n%s", body)
	}
	if !strings.Contains(body, `"usage":{"prompt_tokens":12,"completion_tokens":7,"total_tokens":19}`) {
		t.Fatalf("尾块 usage 不对:\n%s", body)
	}
	rec := up.wsConn(0)
	if rec == nil {
		t.Fatal("假上游没收到 WS 连接")
	}
	if !rec.hasType("init_root") {
		t.Fatalf("首次连接应发 init_root: %v", rec.all())
	}
	if !rec.hasType("new_session") {
		t.Fatalf("fresh_session 时应发 new_session: %v", rec.all())
	}
	texts := rec.promptTexts()
	if len(texts) != 1 || texts[0] != "打个招呼" {
		t.Fatalf("prompt 应透传单条 user 原文，实际 %v", texts)
	}
}

// 非流式聚合（含 assistant_text 兜底与错误码路径）。
func TestChatSyncCompletion(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	ts := newTestHTTP(t, s)

	code, body := postChat(t, ts.URL, `{"messages":[{"role":"user","content":"你好"}]}`)
	if code != http.StatusOK {
		t.Fatalf("非流式应 200，得到 %d：%s", code, body)
	}
	var got map[string]any
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatalf("不是 JSON: %v (%s)", err, body)
	}
	if got["object"] != "chat.completion" {
		t.Fatalf("object 不对: %s", body)
	}
	if !strings.HasPrefix(str(got["id"]), "chatcmpl-") {
		t.Fatalf("id 前缀不对: %s", body)
	}
	if got["model"] != "lite-1" { // 未指定 model → 兜底首个 lite
		t.Fatalf("model 应回显 provider id: %s", body)
	}
	choices := toList(got["choices"])
	msg := toMap(toMap(choices[0])["message"])
	if str(msg["content"]) != "你好世界" {
		t.Fatalf("content 聚合不对: %s", body)
	}
	if u := toMap(got["usage"]); u["total_tokens"] != float64(19) {
		t.Fatalf("usage 不对: %s", body)
	}
}

// 验收 3 之一：带 session_id 时跳过 new_session 且只发增量。
func TestSessionAffinitySkipsNewSession(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	ts := newTestHTTP(t, s)

	code, body := postChat(t, ts.URL,
		`{"session_id":"s-1","messages":[{"role":"user","content":"第一条"}]}`)
	if code != http.StatusOK {
		t.Fatalf("首轮应 200，得到 %d：%s", code, body)
	}
	code, body = postChat(t, ts.URL,
		`{"session_id":"s-1","messages":[{"role":"user","content":"第一条"},{"role":"user","content":"第二条"}]}`)
	if code != http.StatusOK {
		t.Fatalf("第二轮应 200，得到 %d：%s", code, body)
	}
	if up.connCount() < 2 {
		t.Fatalf("应有两条 WS 连接，实际 %d", up.connCount())
	}
	first, second := up.wsConn(0), up.wsConn(1)
	if !first.hasType("new_session") {
		t.Fatalf("首轮应发 new_session: %v", first.all())
	}
	if second.hasType("new_session") {
		t.Fatalf("带 session_id 的续轮不应再发 new_session: %v", second.all())
	}
	if first.hasType("init") && !first.hasType("init_root") {
		t.Fatalf("首次连接应走 init_root: %v", first.all())
	}
	if !second.hasType("init") || second.hasType("init_root") {
		t.Fatalf("第二次连接应走 init: %v", second.all())
	}
	texts := second.promptTexts()
	if len(texts) != 1 {
		t.Fatalf("第二轮应只发一条 prompt，实际 %v", texts)
	}
	if !strings.Contains(texts[0], "第二条") || strings.Contains(texts[0], "第一条") {
		t.Fatalf("第二轮应只发增量（新消息），实际 %q", texts[0])
	}
	// 不同 session_id 视为新会话 → 仍发 new_session
	if _, body := postChat(t, ts.URL,
		`{"session_id":"s-2","messages":[{"role":"user","content":"另起"}]}`); !strings.Contains(body, "chat.completion") {
		t.Fatalf("新会话请求失败: %s", body)
	}
	if !up.wsConn(2).hasType("new_session") {
		t.Fatalf("新 session_id 应发 new_session: %v", up.wsConn(2).all())
	}
	// 同一 session_id 但没有增量（重发同一份历史）→ 当新会话全量重发，不打出空 prompt
	if _, body := postChat(t, ts.URL,
		`{"session_id":"s-2","messages":[{"role":"user","content":"另起"}]}`); !strings.Contains(body, "chat.completion") {
		t.Fatalf("重发请求失败: %s", body)
	}
	if !up.wsConn(3).hasType("new_session") {
		t.Fatalf("无增量的重发应回到新会话并再发 new_session: %v", up.wsConn(3).all())
	}
	if texts := up.wsConn(3).promptTexts(); len(texts) != 1 || !strings.Contains(texts[0], "另起") {
		t.Fatalf("无增量时应全量重发原文，实际 %v", texts)
	}
}

// 队列满 → 429（对齐 SPEC §3.9 的排队上限语义）。
func TestQueueFull(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	s.queue = make(chan struct{}, 1)
	s.queue <- struct{}{} // 占满
	ts := newTestHTTP(t, s)
	code, body := postChat(t, ts.URL, `{"messages":[{"role":"user","content":"x"}]}`)
	if code != http.StatusTooManyRequests {
		t.Fatalf("排队满应 429，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, "QUEUE_FULL") {
		t.Fatalf("错误码不对: %s", body)
	}
}

// WS 握手 401 → 该 token 标死，换下一个 token 重试（P0-1 在 WS 层同样生效）。
func TestWSHandshake401RotatesToken(t *testing.T) {
	up := newFakeUpstream(t)
	up.wsReject["dead-token"] = http.StatusUnauthorized
	s := newTestServer(t, up.url(), "dead-token", "good-token")
	ts := newTestHTTP(t, s)

	code, body := postChat(t, ts.URL, `{"messages":[{"role":"user","content":"你好"}]}`)
	if code != http.StatusOK {
		t.Fatalf("换号后应 200，得到 %d：%s", code, body)
	}
	if cookies := up.cookieSeen(); len(cookies) < 2 || cookies[0] != "dead-token" || cookies[1] != "good-token" {
		t.Fatalf("WS 应先用 dead-token 再换 good-token，实际 %v", cookies)
	}
	if snap := s.pool.snapshot(); snap[0]["dead"] != true {
		t.Fatalf("WS 握手 401 应把该 token 标死: %+v", snap[0])
	}
}

// WS 在就绪前关闭 → 502（错误经 state 抵达消费方，不会丢）。
func TestWSClosedBeforeReady(t *testing.T) {
	up := newFakeUpstream(t)
	up.wsReject["tok-1"] = http.StatusServiceUnavailable
	s := newTestServer(t, up.url(), "tok-1")
	ts := newTestHTTP(t, s)
	code, body := postChat(t, ts.URL, `{"messages":[{"role":"user","content":"x"}]}`)
	if code != http.StatusBadGateway {
		t.Fatalf("WS 握手失败应 502，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, "ws 握手失败") && !strings.Contains(body, "WS 握手失败") {
		t.Fatalf("错误信息应说明 WS 握手失败: %s", body)
	}
}

var _ = httptest.NewServer
