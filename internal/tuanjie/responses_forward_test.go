// responses_forward_test.go —— Zen Responses 转发的单测：
// chat→responses 请求转换（含图片块 / tools 扁平化 / function_call 历史）、
// responses→chat 非流式回填、Responses SSE→chat SSE 流式翻译（首块形态、
// [DONE] 收尾）、forwardExternalResponses 失败码映射（4xx 透传 / 5xx 可回落 /
// 网络错 -1）、isZenResponsesModel 最小白名单与账号 protocol 分流
// （externalProtocolRoute：归一化 / 旧 JSON 缺省 / Zen 兜底 / anthropic 命中）。
package tuanjie

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// parseSSEData 从 SSE 文本里抽出全部 data: 行的 payload（跳过 [DONE]）。
func parseSSEData(t *testing.T, s string) []map[string]any {
	t.Helper()
	var out []map[string]any
	for _, line := range strings.Split(s, "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "data: ") {
			continue
		}
		payload := strings.TrimSpace(strings.TrimPrefix(line, "data: "))
		if payload == "[DONE]" {
			continue
		}
		var m map[string]any
		if err := json.Unmarshal([]byte(payload), &m); err != nil {
			t.Fatalf("data 行非法 JSON: %q", payload)
		}
		out = append(out, m)
	}
	return out
}

func TestChatToResponsesRequestBasic(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model":       zenResponsesModelName,
		"messages":    []any{map[string]any{"role": "user", "content": "hi"}},
		"temperature": 0.5,
		"max_tokens":  100,
	})
	out, ok := chatToResponsesRequest([]byte(in), false)
	if !ok {
		t.Fatal("基本请求转换失败")
	}
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if m["model"] != zenResponsesModelName || m["stream"] != false {
		t.Fatalf("model/stream 不对: %v %v", m["model"], m["stream"])
	}
	if _, has := m["messages"]; has {
		t.Fatal("转换结果不应残留 messages 字段")
	}
	if _, has := m["max_tokens"]; has {
		t.Fatal("max_tokens 应改名为 max_output_tokens")
	}
	if m["max_output_tokens"] != float64(100) || m["temperature"] != 0.5 {
		t.Fatalf("参数映射不对: %v %v", m["max_output_tokens"], m["temperature"])
	}
	input, _ := m["input"].([]any)
	if len(input) != 1 {
		t.Fatalf("input 应有 1 条消息: %v", m["input"])
	}
	item, _ := input[0].(map[string]any)
	if item["type"] != "message" || item["role"] != "user" {
		t.Fatalf("首条 input item 形态不对: %v", item)
	}
	parts, _ := item["content"].([]any)
	p0, _ := parts[0].(map[string]any)
	if p0["type"] != "input_text" || p0["text"] != "hi" {
		t.Fatalf("字符串 content 未展开为 input_text part: %v", p0)
	}
}

func TestChatToResponsesRequestImageBlocks(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model": zenResponsesModelName,
		"messages": []any{
			map[string]any{"role": "system", "content": "be brief"},
			map[string]any{"role": "user", "content": []any{
				map[string]any{"type": "text", "text": "what color? one word"},
				map[string]any{"type": "image_url", "image_url": map[string]any{
					"url": "data:image/png;base64,AAAA", "detail": "low",
				}},
			}},
		},
	})
	out, ok := chatToResponsesRequest([]byte(in), true)
	if !ok {
		t.Fatal("图片块请求转换失败")
	}
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if m["stream"] != true {
		t.Fatalf("stream 未透传: %v", m["stream"])
	}
	input, _ := m["input"].([]any)
	if len(input) != 2 {
		t.Fatalf("input 应有 2 条消息: %v", m["input"])
	}
	sys, _ := input[0].(map[string]any)
	if sys["role"] != "system" {
		t.Fatalf("system 角色丢失: %v", sys)
	}
	user, _ := input[1].(map[string]any)
	parts, _ := user["content"].([]any)
	if len(parts) != 2 {
		t.Fatalf("user content 应有 2 个 part: %v", parts)
	}
	img, _ := parts[1].(map[string]any)
	if img["type"] != "input_image" || img["image_url"] != "data:image/png;base64,AAAA" || img["detail"] != "low" {
		t.Fatalf("image_url 块未映射为 input_image: %v", img)
	}
}

func TestChatToResponsesRequestToolsAndHistory(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model": zenResponsesModelName,
		"tools": []any{
			map[string]any{"type": "function", "function": map[string]any{
				"name": "get_weather", "description": "weather of a city", "parameters": map[string]any{"type": "object"},
			}},
			map[string]any{"type": "web_search"}, // 非 function：应被跳过
		},
		"tool_choice": map[string]any{"type": "function", "function": map[string]any{"name": "get_weather"}},
		"messages": []any{
			map[string]any{"role": "user", "content": "weather in Paris?"},
			map[string]any{"role": "assistant", "content": "", "tool_calls": []any{
				map[string]any{"id": "call_1", "type": "function", "function": map[string]any{
					"name": "get_weather", "arguments": `{"city":"Paris"}`,
				}},
			}},
			map[string]any{"role": "tool", "tool_call_id": "call_1", "content": "15C sunny"},
		},
	})
	out, ok := chatToResponsesRequest([]byte(in), false)
	if !ok {
		t.Fatal("tools/历史转换失败")
	}
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	tools, _ := m["tools"].([]any)
	if len(tools) != 1 {
		t.Fatalf("tools 应只剩 1 条扁平 function（非 function 类型被跳过）: %v", tools)
	}
	tl, _ := tools[0].(map[string]any)
	if tl["type"] != "function" || tl["name"] != "get_weather" || tl["description"] != "weather of a city" {
		t.Fatalf("tools 未扁平化: %v", tl)
	}
	if _, has := tl["function"]; has {
		t.Fatal("扁平 tools 不应残留嵌套 function 字段")
	}
	tc, _ := m["tool_choice"].(map[string]any)
	if tc == nil || tc["type"] != "function" || tc["name"] != "get_weather" {
		t.Fatalf("tool_choice 未扁平化: %v", m["tool_choice"])
	}
	input, _ := m["input"].([]any)
	if len(input) != 3 {
		t.Fatalf("input 应为 user + function_call + function_call_output: %v", input)
	}
	fc, _ := input[1].(map[string]any)
	if fc["type"] != "function_call" || fc["call_id"] != "call_1" || fc["name"] != "get_weather" {
		t.Fatalf("tool_calls 未映射为 function_call item: %v", fc)
	}
	fo, _ := input[2].(map[string]any)
	if fo["type"] != "function_call_output" || fo["call_id"] != "call_1" || fo["output"] != "15C sunny" {
		t.Fatalf("tool 消息未映射为 function_call_output: %v", fo)
	}
}

func TestChatToResponsesRequestBadJSON(t *testing.T) {
	if _, ok := chatToResponsesRequest([]byte("not json"), false); ok {
		t.Fatal("非法 JSON 应返回 ok=false")
	}
}

func TestResponsesToChatCompletionNonStream(t *testing.T) {
	up := `{"id":"resp_x","object":"response","created_at":1788544472,"completed_at":1788544472,
		"status":"completed","model":"muse-spark-1.3-contributor-free",
		"output":[
			{"id":"rs_1","type":"reasoning","status":"completed","encrypted_content":"Q-xx","summary":[]},
			{"id":"msg_1","type":"message","status":"completed","role":"assistant",
			 "content":[{"type":"output_text","text":"Hi there!","annotations":[]}]}
		],
		"usage":{"input_tokens":8,"output_tokens":294,"total_tokens":302}}`
	b, usage, ok := responsesToChatCompletion([]byte(up), "fallback-model")
	if !ok {
		t.Fatal("非流式回填失败")
	}
	if usage == nil || usage.InputTokens != 8 || usage.OutputTokens != 294 || usage.TotalTokens != 302 {
		t.Fatalf("usage 映射不对: %+v", usage)
	}
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if m["object"] != "chat.completion" || m["id"] != "resp_x" || m["model"] != "muse-spark-1.3-contributor-free" {
		t.Fatalf("顶层字段不对: %v", m)
	}
	choices, _ := m["choices"].([]any)
	c0, _ := choices[0].(map[string]any)
	msg, _ := c0["message"].(map[string]any)
	if msg["content"] != "Hi there!" || msg["role"] != "assistant" {
		t.Fatalf("message 回填不对: %v", msg)
	}
	if c0["finish_reason"] != "stop" {
		t.Fatalf("finish_reason 应为 stop: %v", c0["finish_reason"])
	}
	u, _ := m["usage"].(map[string]any)
	if u["prompt_tokens"] != float64(8) || u["completion_tokens"] != float64(294) || u["total_tokens"] != float64(302) {
		t.Fatalf("usage 字段名未映射为 chat 口径: %v", u)
	}
}

func TestResponsesToChatCompletionToolCalls(t *testing.T) {
	up := `{"id":"resp_y","object":"response","created_at":1,"status":"completed","model":"m",
		"output":[
			{"type":"function_call","call_id":"call_9","name":"get_weather","arguments":"{\"city\":\"Paris\"}"},
			{"type":"message","role":"assistant","content":[{"type":"output_text","text":""}]}
		],
		"usage":{"input_tokens":5,"output_tokens":6,"total_tokens":11}}`
	b, _, ok := responsesToChatCompletion([]byte(up), "m")
	if !ok {
		t.Fatal("回填失败")
	}
	var m map[string]any
	_ = json.Unmarshal(b, &m)
	choices, _ := m["choices"].([]any)
	c0, _ := choices[0].(map[string]any)
	if c0["finish_reason"] != "tool_calls" {
		t.Fatalf("有 tool_calls 时 finish_reason 应为 tool_calls: %v", c0["finish_reason"])
	}
	msg, _ := c0["message"].(map[string]any)
	tcs, _ := msg["tool_calls"].([]any)
	tc0, _ := tcs[0].(map[string]any)
	if tc0["id"] != "call_9" || tc0["type"] != "function" {
		t.Fatalf("tool_calls 回填不对: %v", tc0)
	}
	fn, _ := tc0["function"].(map[string]any)
	if fn["name"] != "get_weather" || fn["arguments"] != `{"city":"Paris"}` {
		t.Fatalf("function 字段回填不对: %v", fn)
	}
}

// zenStreamSample 真实 Zen 流式事件形态（2026-09-05 实测，reasoning 事件在前）。
const zenStreamSample = `event: response.created
data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_1","status":"in_progress"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":2,"item":{"type":"reasoning","status":"in_progress"}}

event: response.output_item.done
data: {"type":"response.output_item.done","sequence_number":3,"item":{"type":"reasoning","status":"completed"}}

event: response.output_item.added
data: {"type":"response.output_item.added","sequence_number":4,"item":{"type":"message","role":"assistant","status":"in_progress"}}

event: response.content_part.added
data: {"type":"response.content_part.added","sequence_number":5,"part":{"type":"output_text","text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":6,"delta":"Hi"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","sequence_number":7,"delta":" there"}

event: response.completed
data: {"type":"response.completed","sequence_number":9,"response":{"id":"resp_1","status":"completed","usage":{"input_tokens":12,"output_tokens":321,"total_tokens":333}}}

event: ping
data: {"type":"ping"}
`

func TestStreamResponsesToChatText(t *testing.T) {
	var sb strings.Builder
	var gotUsage [3]int64
	streamResponsesToChat(&sb, nopFlusher{}, "m", strings.NewReader(zenStreamSample),
		nil, func(in, out, total int64) { gotUsage = [3]int64{in, out, total} })
	out := sb.String()
	if !strings.Contains(out, "data: [DONE]\n\n") {
		t.Fatal("缺 data: [DONE] 收尾")
	}
	chunks := parseSSEData(t, out)
	if len(chunks) < 4 {
		t.Fatalf("chunk 数不对: %d", len(chunks))
	}
	first := chunks[0]
	if first["object"] != "chat.completion.chunk" {
		t.Fatalf("首块应为 chat.completion.chunk: %v", first)
	}
	fc, _ := first["choices"].([]any)
	f0, _ := fc[0].(map[string]any)
	fd, _ := f0["delta"].(map[string]any)
	if fd["role"] != "assistant" {
		t.Fatalf("首块 delta 应含 role=assistant: %v", fd)
	}
	// 中间块拼出全文
	var text strings.Builder
	var finish any
	for _, ch := range chunks {
		cs, _ := ch["choices"].([]any)
		c, _ := cs[0].(map[string]any)
		if d, ok := c["delta"].(map[string]any); ok {
			if s, ok := d["content"].(string); ok {
				text.WriteString(s)
			}
		}
		if c["finish_reason"] == "stop" || c["finish_reason"] == "tool_calls" {
			finish = c["finish_reason"]
		}
	}
	if text.String() != "Hi there" {
		t.Fatalf("流式文本拼接不对: %q", text.String())
	}
	if finish != "stop" {
		t.Fatalf("终块 finish_reason 应为 stop: %v", finish)
	}
	if gotUsage != [3]int64{12, 321, 333} {
		t.Fatalf("流式 usage 未回传: %v", gotUsage)
	}
	// reasoning 事件不应产生 chunk：首块即 role 块（上面已断言 fd["role"]），
	// 若 reasoning 先发了一个 chunk，这里 len 应比 role+2 文本+终块 = 4 多。
	if len(chunks) != 4 {
		t.Fatalf("chunk 应恰为 role + 2 文本 + 终块 = 4，got %d", len(chunks))
	}
}

func TestStreamResponsesToChatToolCall(t *testing.T) {
	sse := "event: response.created\n" +
		`data: {"type":"response.created","response":{"id":"resp_2","status":"in_progress"}}` + "\n\n" +
		`data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}` + "\n\n" +
		`data: {"type":"response.completed","response":{"id":"resp_2","status":"completed","usage":{"input_tokens":10,"output_tokens":20,"total_tokens":30}}}` + "\n\n"
	var sb strings.Builder
	streamResponsesToChat(&sb, nopFlusher{}, "m", strings.NewReader(sse), nil, nil)
	out := sb.String()
	if !strings.Contains(out, "data: [DONE]\n\n") {
		t.Fatal("缺 data: [DONE]")
	}
	foundTool, foundFinish := false, false
	for _, ch := range parseSSEData(t, out) {
		cs, _ := ch["choices"].([]any)
		c, _ := cs[0].(map[string]any)
		if d, ok := c["delta"].(map[string]any); ok {
			if tcs, ok := d["tool_calls"].([]any); ok && len(tcs) > 0 {
				tc, _ := tcs[0].(map[string]any)
				if tc["index"] == float64(0) && tc["id"] == "call_1" && tc["type"] == "function" {
					fn, _ := tc["function"].(map[string]any)
					if fn["name"] == "get_weather" {
						foundTool = true
					}
				}
			}
		}
		if c["finish_reason"] == "tool_calls" {
			foundFinish = true
		}
	}
	if !foundTool {
		t.Fatal("function_call 未翻译为 chat tool_calls delta")
	}
	if !foundFinish {
		t.Fatal("有 tool_calls 时终块 finish_reason 应为 tool_calls")
	}
}

func TestStreamResponsesToChatAbruptEnd(t *testing.T) {
	// 上游流没走 completed 就断：也要补 [DONE]，客户端不悬挂
	var sb strings.Builder
	streamResponsesToChat(&sb, nopFlusher{}, "m", strings.NewReader(`data: {"type":"response.output_text.delta","delta":"x"}`), nil, nil)
	if !strings.Contains(sb.String(), "data: [DONE]\n\n") {
		t.Fatal("上游中断时应补 [DONE]")
	}
}

// newTestServer 最小 Server（registry/activity/stats 就绪，不落盘）。
func newTestServer() *Server {
	return &Server{registry: NewRegistry(), activity: NewActivityLog(), stats: map[string]*modelStat{}}
}

func TestForwardExternalResponsesNonStreamAndFailures(t *testing.T) {
	nonStreamBody := `{"id":"resp_z","object":"response","created_at":1,"status":"completed","model":"muse-spark-1.3-contributor-free",
		"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"pong"}]}],
		"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}`
	cases := []struct {
		name       string
		upstream   func(w http.ResponseWriter, r *http.Request)
		chatStream bool
		wantStatus int // forwardExternalResponses 返回值
		wantCode   int // 客户端收到的状态码
		wantBody   string
	}{
		{
			name: "200非流式回填",
			upstream: func(w http.ResponseWriter, r *http.Request) {
				_ = r.Body.Close()
				w.Header().Set("Content-Type", "application/json")
				fmt.Fprint(w, nonStreamBody)
			},
			wantStatus: 0, wantCode: 200, wantBody: `"object":"chat.completion"`,
		},
		{
			name: "200流式翻译",
			upstream: func(w http.ResponseWriter, r *http.Request) {
				w.Header().Set("Content-Type", "text/event-stream")
				fmt.Fprint(w, zenStreamSample)
			},
			chatStream: true, wantStatus: 0, wantCode: 200, wantBody: "data: [DONE]",
		},
		{
			name: "400透传",
			upstream: func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(http.StatusBadRequest)
				fmt.Fprint(w, `{"error":{"message":"bad request"}}`)
			},
			wantStatus: 0, wantCode: 400, wantBody: "bad request",
		},
		{
			name: "500不写客户端",
			upstream: func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(http.StatusInternalServerError)
				fmt.Fprint(w, `{"error":"boom"}`)
			},
			wantStatus: 500, wantCode: 200, wantBody: "", // recorder 默认 200 但未写任何字节
		},
		{
			name: "429不写客户端",
			upstream: func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(http.StatusTooManyRequests)
			},
			wantStatus: 429, wantCode: 200, wantBody: "",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			up := httptest.NewServer(http.HandlerFunc(tc.upstream))
			defer up.Close()
			s := newTestServer()
			chatBody := mustJSON(t, map[string]any{
				"model": zenResponsesModelName,
				"messages": []any{
					map[string]any{"role": "user", "content": []any{
						map[string]any{"type": "text", "text": "hi"},
						map[string]any{"type": "image_url", "image_url": map[string]any{"url": "data:image/png;base64,AAAA"}},
					}},
				},
				"stream": tc.chatStream,
			})
			r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
			w := httptest.NewRecorder()
			prov := &ExternalProvider{Name: "Zen", BaseURL: up.URL, APIKey: "sk-test", Models: []string{zenResponsesModelName}}
			st := s.forwardExternalResponses(w, r, []byte(chatBody), zenResponsesModelName, tc.chatStream, prov, time.Now())
			if st != tc.wantStatus {
				t.Fatalf("failStatus = %d, want %d", st, tc.wantStatus)
			}
			if tc.wantStatus >= 500 || tc.wantStatus == 429 || tc.wantStatus == -1 {
				if w.Body.Len() != 0 {
					t.Fatalf("可回落失败不应写客户端，却写了: %q", w.Body.String())
				}
				return
			}
			if w.Code != tc.wantCode {
				t.Fatalf("客户端状态码 = %d, want %d", w.Code, tc.wantCode)
			}
			if !strings.Contains(w.Body.String(), tc.wantBody) {
				t.Fatalf("客户端 body 不含 %q: %q", tc.wantBody, w.Body.String())
			}
		})
	}
}

func TestForwardExternalResponsesNetworkError(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	up.Close() // 立刻关掉 = 连接必失败
	s := newTestServer()
	chatBody := mustJSON(t, map[string]any{
		"model": zenResponsesModelName, "messages": []any{map[string]any{"role": "user", "content": "hi"}},
	})
	r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
	w := httptest.NewRecorder()
	prov := &ExternalProvider{Name: "Zen", BaseURL: up.URL, APIKey: "sk-test", Models: []string{zenResponsesModelName}}
	if st := s.forwardExternalResponses(w, r, []byte(chatBody), zenResponsesModelName, false, prov, time.Now()); st != -1 {
		t.Fatalf("网络错 failStatus = %d, want -1", st)
	}
	if w.Body.Len() != 0 {
		t.Fatalf("网络错不应写客户端: %q", w.Body.String())
	}
}

func TestIsZenResponsesModelWhitelist(t *testing.T) {
	zen := &ExternalProvider{Name: "Zen", BaseURL: "https://opencode.ai/zen/v1", APIKey: "k", Models: []string{zenResponsesModelName}}
	if !isZenResponsesModel(zen, zenResponsesModelName) {
		t.Fatal("Zen + muse-spark 应命中 responses 分支")
	}
	if isZenResponsesModel(zen, "grok-code") {
		t.Fatal("Zen 其他模型不应命中（仍走 chat 协议）")
	}
	agnes := &ExternalProvider{Name: "Agnes", BaseURL: "https://apihub.agnes-ai.com/v1", APIKey: "k", Models: []string{"agnes-2.5-flash"}}
	if isZenResponsesModel(agnes, zenResponsesModelName) {
		t.Fatal("非 Zen base_url 不应命中")
	}
}

// TestProviderProtocolNormalization protocol 归一化：trim+小写、三值白名单、
// 空值/非法值兜底 chat。
func TestProviderProtocolNormalization(t *testing.T) {
	cases := []struct{ raw, want string }{
		{"", "chat"}, {"chat", "chat"}, {" Chat ", "chat"},
		{"responses", "responses"}, {" RESPONSES ", "responses"},
		{"Anthropic", "anthropic"}, {"anthropic ", "anthropic"},
		{"grpc", "chat"}, {"anthropicx", "chat"},
	}
	for _, c := range cases {
		if got := ProviderProtocol(&ExternalProvider{Protocol: c.raw}); got != c.want {
			t.Errorf("ProviderProtocol(%q) = %q, want %q", c.raw, got, c.want)
		}
	}
}

// TestProviderProtocolLegacyJSON 旧 JSON（无 protocol 字段）加载后归为 chat，
// 分流侧由 externalProtocolRoute 的 Zen 白名单兜底。
func TestProviderProtocolLegacyJSON(t *testing.T) {
	var p ExternalProvider
	if err := json.Unmarshal([]byte(`{"name":"Zen","base_url":"https://opencode.ai/zen/v1","api_key":"k","models":["muse-spark-1.3-contributor-free"]}`), &p); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if p.Protocol != "" {
		t.Fatalf("旧 JSON 不应带 protocol 字段: %q", p.Protocol)
	}
	if got := ProviderProtocol(&p); got != "chat" {
		t.Fatalf("旧 JSON 协议应归为 chat: %q", got)
	}
}

// TestProviderAddPersistsProtocol Add 归一化落盘：合法值保留、非法值回退 chat。
// 用临时目录文件，不碰真实 tuanjie-providers.json。
func TestProviderAddPersistsProtocol(t *testing.T) {
	ps := &ProviderStore{path: filepath.Join(t.TempDir(), "providers.json"),
		cache: map[string]cachedInfo{}, refreshing: map[string]bool{}}
	if !ps.Add(ExternalProvider{Name: "r1", BaseURL: "https://h/v1", APIKey: "k", Protocol: " Responses "}) {
		t.Fatal("Add r1 失败")
	}
	if !ps.Add(ExternalProvider{Name: "r2", BaseURL: "https://h/v1", APIKey: "k", Protocol: "grpc"}) {
		t.Fatal("Add r2 失败")
	}
	raw, err := os.ReadFile(ps.path)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	s := string(raw)
	if !strings.Contains(s, `"protocol": "responses"`) {
		t.Fatalf("r1 落盘应含归一化 responses: %s", s)
	}
	if !strings.Contains(s, `"protocol": "chat"`) {
		t.Fatalf("r2 非法值应回退 chat 落盘: %s", s)
	}
}

// TestExternalProtocolRoute 分流判定：protocol 优先；旧配置（空）回退 Zen 白名单。
func TestExternalProtocolRoute(t *testing.T) {
	zen := func() *ExternalProvider {
		return &ExternalProvider{Name: "Zen", BaseURL: "https://opencode.ai/zen/v1", APIKey: "k"}
	}
	// 旧配置（无 protocol）：Zen + muse 模型仍走 responses（兜底不断流）
	if got := externalProtocolRoute(zen(), zenResponsesModelName); got != "responses" {
		t.Fatalf("旧配置 Zen + muse 应走 responses: %q", got)
	}
	// 旧配置：Zen 其他模型走 chat
	if got := externalProtocolRoute(zen(), "grok-code"); got != "chat" {
		t.Fatalf("旧配置 Zen 其他模型应走 chat: %q", got)
	}
	// 旧配置：非 Zen base 即使命中 muse 模型名也走 chat
	nonZen := &ExternalProvider{Name: "X", BaseURL: "https://apihub.agnes-ai.com/v1", APIKey: "k"}
	if got := externalProtocolRoute(nonZen, zenResponsesModelName); got != "chat" {
		t.Fatalf("旧配置非 Zen 不应走 responses: %q", got)
	}
	// protocol=responses 不依赖模型名 / base_url
	p := &ExternalProvider{Name: "R", BaseURL: "https://any.host/v1", APIKey: "k", Protocol: "responses"}
	if got := externalProtocolRoute(p, "whatever-model"); got != "responses" {
		t.Fatalf("protocol=responses 应无视模型名: %q", got)
	}
	// protocol=chat 显式声明：即使 Zen + muse 也走 chat（用户选了什么就是什么）
	p2 := zen()
	p2.Protocol = "chat"
	if got := externalProtocolRoute(p2, zenResponsesModelName); got != "chat" {
		t.Fatalf("protocol=chat 显式声明应覆盖白名单: %q", got)
	}
	// protocol=anthropic
	p3 := zen()
	p3.Protocol = "anthropic"
	if got := externalProtocolRoute(p3, "m"); got != "anthropic" {
		t.Fatalf("protocol=anthropic 应命中 anthropic 分支: %q", got)
	}
}

// TestForwardExternalRoutesByProtocol forwardExternal 端到端分流（httptest 回环
// 上游，记路径）：protocol=responses 按协议分流不依赖模型名；anthropic 命中上游
// /v1/messages 并翻回 chat 格式；旧配置非 Zen 走 chat 不变。
// 旧配置 Zen 兜底需 base_url 含 opencode.ai/zen，httptest 回环无法模拟，
// 由 TestExternalProtocolRoute 直接覆盖。
func TestForwardExternalRoutesByProtocol(t *testing.T) {
	var mu sync.Mutex
	var paths []string
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = r.Body.Close()
		mu.Lock()
		paths = append(paths, r.URL.Path)
		mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		if strings.HasSuffix(r.URL.Path, "/responses") {
			fmt.Fprint(w, `{"id":"resp_p","object":"response","created_at":1,"status":"completed","model":"m",
				"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
				"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}`)
			return
		}
		if strings.HasSuffix(r.URL.Path, "/messages") {
			fmt.Fprint(w, `{"id":"msg_p","type":"message","role":"assistant","model":"m",
				"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
				"usage":{"input_tokens":1,"output_tokens":2}}`)
			return
		}
		fmt.Fprint(w, `{"id":"c1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"chat-ok"},"finish_reason":"stop"}]}`)
	}))
	defer up.Close()
	s := newTestServer()
	chatBody := mustJSON(t, map[string]any{
		"model": "whatever-model", "messages": []any{map[string]any{"role": "user", "content": "hi"}},
	})

	t.Run("protocol=responses不依赖模型名", func(t *testing.T) {
		prov := &ExternalProvider{Name: "R", BaseURL: up.URL, APIKey: "k", Models: []string{"whatever-model"}, Protocol: "responses"}
		w := httptest.NewRecorder()
		r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
		if st := s.forwardExternal(w, r, []byte(chatBody), "whatever-model", false, prov, "/chat/completions"); st != 0 {
			t.Fatalf("failStatus = %d, want 0", st)
		}
		if !strings.Contains(w.Body.String(), `"object":"chat.completion"`) {
			t.Fatalf("responses 结果应翻回 chat 格式: %s", w.Body.String())
		}
		mu.Lock()
		defer mu.Unlock()
		if len(paths) != 1 || !strings.HasSuffix(paths[0], "/responses") {
			t.Fatalf("应恰好命中上游 /responses 一次: %v", paths)
		}
		paths = paths[:0]
	})

	t.Run("anthropic转发命中上游/v1/messages", func(t *testing.T) {
		prov := &ExternalProvider{Name: "A", BaseURL: up.URL, APIKey: "k", Protocol: "anthropic"}
		w := httptest.NewRecorder()
		r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
		if st := s.forwardExternal(w, r, []byte(chatBody), "whatever-model", false, prov, "/chat/completions"); st != 0 {
			t.Fatalf("anthropic 转发应返回 0: %d", st)
		}
		if !strings.Contains(w.Body.String(), `"object":"chat.completion"`) {
			t.Fatalf("anthropic 结果应翻回 chat 格式: %s", w.Body.String())
		}
		mu.Lock()
		defer mu.Unlock()
		if len(paths) != 1 || !strings.HasSuffix(paths[0], "/v1/messages") {
			t.Fatalf("应恰好命中上游 /v1/messages 一次: %v", paths)
		}
		paths = paths[:0]
	})

	t.Run("旧配置无protocol非Zen走chat", func(t *testing.T) {
		prov := &ExternalProvider{Name: "C", BaseURL: up.URL, APIKey: "k", Models: []string{"whatever-model"}}
		w := httptest.NewRecorder()
		r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
		if st := s.forwardExternal(w, r, []byte(chatBody), "whatever-model", false, prov, "/chat/completions"); st != 0 {
			t.Fatalf("failStatus = %d, want 0", st)
		}
		mu.Lock()
		defer mu.Unlock()
		if len(paths) != 1 || !strings.HasSuffix(paths[0], "/chat/completions") {
			t.Fatalf("旧配置非 Zen 应走 chat/completions: %v", paths)
		}
	})
}
