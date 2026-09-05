// anthropic_forward_test.go —— Anthropic Messages 转发的单测：
// chat→anthropic 请求转换（system 提取 / image_url→image base64 source /
// tool_calls 历史→tool_use+tool_result / tools→input_schema / max_tokens 缺省）、
// anthropic→chat 非流式回填（tool_use→tool_calls / stop_reason 映射 / usage）、
// Anthropic SSE→chat SSE 流式翻译（首块形态、[DONE] 收尾）、
// forwardExternalAnthropic 失败码映射（4xx 透传 / 5xx 可回落 / 网络错 -1）
// 与上游路径（/v1/messages）、认证头（x-api-key + anthropic-version）断言。
package tuanjie

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestChatToAnthropicRequestBasic(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model":       "claude-sonnet-4",
		"messages":    []any{map[string]any{"role": "user", "content": "hi"}},
		"temperature": 0.5,
		"max_tokens":  100,
	})
	out, ok := chatToAnthropicRequest([]byte(in), false)
	if !ok {
		t.Fatal("基本请求转换失败")
	}
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if m["model"] != "claude-sonnet-4" || m["stream"] != false {
		t.Fatalf("model/stream 不对: %v %v", m["model"], m["stream"])
	}
	if _, has := m["messages"]; !has {
		t.Fatal("messages 不应缺失")
	}
	if m["max_tokens"] != float64(100) || m["temperature"] != 0.5 {
		t.Fatalf("参数映射不对: %v %v", m["max_tokens"], m["temperature"])
	}
	if _, has := m["system"]; has {
		t.Fatalf("无 system 消息时不应有 system 字段: %v", m["system"])
	}
	msgs, _ := m["messages"].([]any)
	if len(msgs) != 1 {
		t.Fatalf("messages 应有 1 条: %v", m["messages"])
	}
	msg, _ := msgs[0].(map[string]any)
	if msg["role"] != "user" {
		t.Fatalf("首条消息角色不对: %v", msg)
	}
	blocks, _ := msg["content"].([]any)
	b0, _ := blocks[0].(map[string]any)
	if b0["type"] != "text" || b0["text"] != "hi" {
		t.Fatalf("字符串 content 未展开为 text 块: %v", b0)
	}
}

func TestChatToAnthropicRequestMaxTokensDefault(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model":    "m",
		"messages": []any{map[string]any{"role": "user", "content": "hi"}},
	})
	out, ok := chatToAnthropicRequest([]byte(in), false)
	if !ok {
		t.Fatal("转换失败")
	}
	var m map[string]any
	_ = json.Unmarshal(out, &m)
	if m["max_tokens"] != float64(anthropicDefaultMaxTokens) {
		t.Fatalf("max_tokens 缺省应给 %d: %v", anthropicDefaultMaxTokens, m["max_tokens"])
	}
}

func TestChatToAnthropicRequestSystemAndImage(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model": "m",
		"messages": []any{
			map[string]any{"role": "system", "content": "be brief"},
			map[string]any{"role": "user", "content": []any{
				map[string]any{"type": "text", "text": "what color? one word"},
				map[string]any{"type": "image_url", "image_url": map[string]any{
					"url": "data:image/png;base64,AAAA",
				}},
			}},
		},
	})
	out, ok := chatToAnthropicRequest([]byte(in), true)
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
	if m["system"] != "be brief" {
		t.Fatalf("system 消息未提取为顶层 system: %v", m["system"])
	}
	msgs, _ := m["messages"].([]any)
	if len(msgs) != 1 {
		t.Fatalf("system 不应占用 messages: %v", m["messages"])
	}
	msg, _ := msgs[0].(map[string]any)
	blocks, _ := msg["content"].([]any)
	if len(blocks) != 2 {
		t.Fatalf("user content 应有 2 个块: %v", blocks)
	}
	img, _ := blocks[1].(map[string]any)
	if img["type"] != "image" {
		t.Fatalf("image_url 块未映射为 image: %v", img)
	}
	src, _ := img["source"].(map[string]any)
	if src["type"] != "base64" || src["media_type"] != "image/png" || src["data"] != "AAAA" {
		t.Fatalf("data URL 未映射为 base64 source: %v", src)
	}
}

func TestChatToAnthropicRequestToolsAndHistory(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model": "m",
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
	out, ok := chatToAnthropicRequest([]byte(in), false)
	if !ok {
		t.Fatal("tools/历史转换失败")
	}
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	tools, _ := m["tools"].([]any)
	if len(tools) != 1 {
		t.Fatalf("tools 应只剩 1 条（非 function 类型被跳过）: %v", tools)
	}
	tl, _ := tools[0].(map[string]any)
	if tl["name"] != "get_weather" || tl["description"] != "weather of a city" {
		t.Fatalf("tools 形态不对: %v", tl)
	}
	if _, has := tl["function"]; has {
		t.Fatal("Anthropic tools 不应残留嵌套 function 字段")
	}
	schema, _ := tl["input_schema"].(map[string]any)
	if schema == nil || schema["type"] != "object" {
		t.Fatalf("parameters 未映射为 input_schema: %v", tl)
	}
	tc, _ := m["tool_choice"].(map[string]any)
	if tc == nil || tc["type"] != "tool" || tc["name"] != "get_weather" {
		t.Fatalf("tool_choice 未映射为 Anthropic 形态: %v", m["tool_choice"])
	}
	msgs, _ := m["messages"].([]any)
	if len(msgs) != 3 {
		t.Fatalf("messages 应为 user + assistant(tool_use) + user(tool_result): %v", m["messages"])
	}
	asst, _ := msgs[1].(map[string]any)
	if asst["role"] != "assistant" {
		t.Fatalf("tool_calls 历史应留在 assistant 消息: %v", asst)
	}
	ablocks, _ := asst["content"].([]any)
	tu, _ := ablocks[0].(map[string]any)
	if tu["type"] != "tool_use" || tu["id"] != "call_1" || tu["name"] != "get_weather" {
		t.Fatalf("tool_calls 未映射为 tool_use 块: %v", tu)
	}
	input, _ := tu["input"].(map[string]any)
	if input["city"] != "Paris" {
		t.Fatalf("arguments 未解析为 input 对象: %v", tu)
	}
	ures, _ := msgs[2].(map[string]any)
	if ures["role"] != "user" {
		t.Fatalf("tool 结果应落 user 消息: %v", ures)
	}
	rblocks, _ := ures["content"].([]any)
	tr, _ := rblocks[0].(map[string]any)
	if tr["type"] != "tool_result" || tr["tool_use_id"] != "call_1" || tr["content"] != "15C sunny" {
		t.Fatalf("tool 消息未映射为 tool_result 块: %v", tr)
	}
}

func TestChatToAnthropicRequestBadJSON(t *testing.T) {
	if _, ok := chatToAnthropicRequest([]byte("not json"), false); ok {
		t.Fatal("非法 JSON 应返回 ok=false")
	}
}

func TestAnthropicStopToFinish(t *testing.T) {
	cases := []struct{ stop, want string }{
		{"end_turn", "stop"}, {"stop_sequence", "stop"}, {"", "stop"},
		{"max_tokens", "length"}, {"tool_use", "tool_calls"},
	}
	for _, c := range cases {
		if got := anthropicStopToFinish(c.stop); got != c.want {
			t.Errorf("anthropicStopToFinish(%q) = %q, want %q", c.stop, got, c.want)
		}
	}
}

func TestAnthropicToChatCompletionNonStream(t *testing.T) {
	up := `{"id":"msg_x","type":"message","role":"assistant","model":"claude-x",
		"content":[{"type":"text","text":"Hi there!"}],
		"stop_reason":"end_turn","stop_sequence":null,
		"usage":{"input_tokens":8,"output_tokens":294}}`
	b, usage, ok := anthropicToChatCompletion([]byte(up), "fallback-model")
	if !ok {
		t.Fatal("非流式回填失败")
	}
	if usage == nil || usage.InputTokens != 8 || usage.OutputTokens != 294 {
		t.Fatalf("usage 映射不对: %+v", usage)
	}
	var m map[string]any
	if err := json.Unmarshal(b, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if m["object"] != "chat.completion" || m["id"] != "msg_x" || m["model"] != "claude-x" {
		t.Fatalf("顶层字段不对: %v", m)
	}
	choices, _ := m["choices"].([]any)
	c0, _ := choices[0].(map[string]any)
	msg, _ := c0["message"].(map[string]any)
	if msg["content"] != "Hi there!" || msg["role"] != "assistant" {
		t.Fatalf("message 回填不对: %v", msg)
	}
	if c0["finish_reason"] != "stop" {
		t.Fatalf("end_turn 应映射为 stop: %v", c0["finish_reason"])
	}
	u, _ := m["usage"].(map[string]any)
	if u["prompt_tokens"] != float64(8) || u["completion_tokens"] != float64(294) || u["total_tokens"] != float64(302) {
		t.Fatalf("usage 字段名未映射为 chat 口径: %v", u)
	}
}

func TestAnthropicToChatCompletionToolUse(t *testing.T) {
	up := `{"id":"msg_y","type":"message","role":"assistant","model":"m",
		"content":[{"type":"tool_use","id":"toolu_9","name":"get_weather","input":{"city":"Paris"}}],
		"stop_reason":"tool_use",
		"usage":{"input_tokens":5,"output_tokens":6}}`
	b, _, ok := anthropicToChatCompletion([]byte(up), "m")
	if !ok {
		t.Fatal("回填失败")
	}
	var m map[string]any
	_ = json.Unmarshal(b, &m)
	choices, _ := m["choices"].([]any)
	c0, _ := choices[0].(map[string]any)
	if c0["finish_reason"] != "tool_calls" {
		t.Fatalf("有 tool_use 时 finish_reason 应为 tool_calls: %v", c0["finish_reason"])
	}
	msg, _ := c0["message"].(map[string]any)
	tcs, _ := msg["tool_calls"].([]any)
	tc0, _ := tcs[0].(map[string]any)
	if tc0["id"] != "toolu_9" || tc0["type"] != "function" {
		t.Fatalf("tool_use 未回填为 tool_calls: %v", tc0)
	}
	fn, _ := tc0["function"].(map[string]any)
	if fn["name"] != "get_weather" || fn["arguments"] != `{"city":"Paris"}` {
		t.Fatalf("function 字段回填不对: %v", fn)
	}
}

// anthropicStreamSample 真实 Anthropic Messages 流式事件形态。
const anthropicStreamSample = `event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-x","content":[],"stop_reason":null,"usage":{"input_tokens":12,"output_tokens":1}}}

event: ping
data: {"type":"ping"}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":321}}

event: message_stop
data: {"type":"message_stop"}
`

func TestStreamAnthropicToChatText(t *testing.T) {
	var sb strings.Builder
	var gotUsage [3]int64
	streamAnthropicToChat(&sb, nopFlusher{}, "m", strings.NewReader(anthropicStreamSample),
		nil, func(in, out, total int64) { gotUsage = [3]int64{in, out, total} })
	out := sb.String()
	if !strings.Contains(out, "data: [DONE]\n\n") {
		t.Fatal("缺 data: [DONE] 收尾")
	}
	chunks := parseSSEData(t, out)
	if len(chunks) != 4 {
		t.Fatalf("chunk 应恰为 role + 2 文本 + 终块 = 4，got %d", len(chunks))
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
	var text strings.Builder
	var finish any
	var finalUsage map[string]any
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
			finalUsage, _ = ch["usage"].(map[string]any)
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
	if finalUsage["total_tokens"] != float64(333) {
		t.Fatalf("终块 usage total 不对: %v", finalUsage)
	}
}

func TestStreamAnthropicToChatToolCall(t *testing.T) {
	sse := `data: {"type":"message_start","message":{"id":"msg_2","type":"message","role":"assistant","usage":{"input_tokens":10,"output_tokens":1}}}` + "\n\n" +
		`data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}` + "\n\n" +
		`data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}` + "\n\n" +
		`data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"Paris\"}"}}` + "\n\n" +
		`data: {"type":"content_block_stop","index":0}` + "\n\n" +
		`data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}` + "\n\n" +
		`data: {"type":"message_stop"}` + "\n\n"
	var sb strings.Builder
	streamAnthropicToChat(&sb, nopFlusher{}, "m", strings.NewReader(sse), nil, nil)
	out := sb.String()
	if !strings.Contains(out, "data: [DONE]\n\n") {
		t.Fatal("缺 data: [DONE]")
	}
	foundTool, foundFinish, args := false, false, ""
	for _, ch := range parseSSEData(t, out) {
		cs, _ := ch["choices"].([]any)
		c, _ := cs[0].(map[string]any)
		if d, ok := c["delta"].(map[string]any); ok {
			if tcs, ok := d["tool_calls"].([]any); ok && len(tcs) > 0 {
				tc, _ := tcs[0].(map[string]any)
				if tc["index"] == float64(0) {
					// 首块（带 id/name）断言 tool_calls 起始形态
					if tc["id"] == "toolu_1" && tc["type"] == "function" {
						fn, _ := tc["function"].(map[string]any)
						if fn["name"] == "get_weather" {
							foundTool = true
						}
					}
					// 增量块（仅 index + arguments）按 OpenAI 语义累积
					if fn, ok := tc["function"].(map[string]any); ok {
						if a, ok := fn["arguments"].(string); ok {
							args += a
						}
					}
				}
			}
		}
		if c["finish_reason"] == "tool_calls" {
			foundFinish = true
		}
	}
	if !foundTool {
		t.Fatal("tool_use 块未翻译为 chat tool_calls delta")
	}
	if args != `{"city":"Paris"}` {
		t.Fatalf("input_json_delta 增量拼接不对: %q", args)
	}
	if !foundFinish {
		t.Fatal("stop_reason=tool_use 时终块 finish_reason 应为 tool_calls")
	}
}

func TestStreamAnthropicToChatAbruptEnd(t *testing.T) {
	// 上游流没走 message_stop 就断：也要补 [DONE]，客户端不悬挂
	var sb strings.Builder
	streamAnthropicToChat(&sb, nopFlusher{}, "m",
		strings.NewReader(`data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}`), nil, nil)
	if !strings.Contains(sb.String(), "data: [DONE]\n\n") {
		t.Fatal("上游中断时应补 [DONE]")
	}
}

// TestForwardExternalAnthropicNonStreamAndFailures 转发主体端到端（httptest
// 回环上游）：覆盖 200 非流式/流式、400 透传、500/429 不写客户端；
// 上游路径与认证头断言单独在 TestForwardExternalAnthropicUpstreamShape。
func TestForwardExternalAnthropicNonStreamAndFailures(t *testing.T) {
	nonStreamBody := `{"id":"msg_z","type":"message","role":"assistant","model":"m",
		"content":[{"type":"text","text":"pong"}],"stop_reason":"end_turn",
		"usage":{"input_tokens":1,"output_tokens":2}}`
	nonStreamUp := func(w http.ResponseWriter, r *http.Request) {
		_ = r.Body.Close()
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, nonStreamBody)
	}

	cases := []struct {
		name       string
		upstream   func(w http.ResponseWriter, r *http.Request)
		chatStream bool
		wantStatus int // forwardExternalAnthropic 返回值
		wantCode   int // 客户端收到的状态码
		wantBody   string
	}{
		{
			name: "200非流式回填", upstream: nonStreamUp,
			wantStatus: 0, wantCode: 200, wantBody: `"object":"chat.completion"`,
		},
		{
			name: "200流式翻译",
			upstream: func(w http.ResponseWriter, r *http.Request) {
				_ = r.Body.Close()
				w.Header().Set("Content-Type", "text/event-stream")
				fmt.Fprint(w, anthropicStreamSample)
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
				"model": "claude-sonnet-4",
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
			prov := &ExternalProvider{Name: "Claude", BaseURL: up.URL, APIKey: "sk-test", Models: []string{"claude-sonnet-4"}}
			st := s.forwardExternalAnthropic(w, r, []byte(chatBody), "claude-sonnet-4", tc.chatStream, prov, time.Now())
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

// TestForwardExternalAnthropicUpstreamShape 上游路径与认证头断言：externalBaseURL
// 已补 /v1，出站应为 {base}/v1/messages；认证用 x-api-key + anthropic-version，
// key 直用账号 api_key（Bearer 前缀剥离）；请求体为 Anthropic 形态
// （含 max_tokens 与 image 块）。
func TestForwardExternalAnthropicUpstreamShape(t *testing.T) {
	var mu sync.Mutex
	var gotPath, gotKey, gotVersion, gotAuth, gotBody string
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		b, _ := io.ReadAll(r.Body)
		_ = r.Body.Close()
		mu.Lock()
		gotPath = r.URL.Path
		gotKey = r.Header.Get("x-api-key")
		gotVersion = r.Header.Get("anthropic-version")
		gotAuth = r.Header.Get("Authorization")
		gotBody = string(b)
		mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"id":"msg_s","type":"message","role":"assistant","model":"m",
			"content":[{"type":"text","text":"pong"}],"stop_reason":"end_turn",
			"usage":{"input_tokens":1,"output_tokens":2}}`)
	}))
	defer up.Close()
	s := newTestServer()
	chatBody := mustJSON(t, map[string]any{
		"model": "claude-sonnet-4",
		"messages": []any{
			map[string]any{"role": "user", "content": []any{
				map[string]any{"type": "text", "text": "hi"},
				map[string]any{"type": "image_url", "image_url": map[string]any{"url": "data:image/png;base64,AAAA"}},
			}},
		},
	})
	r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
	w := httptest.NewRecorder()
	prov := &ExternalProvider{Name: "Claude", BaseURL: up.URL, APIKey: "Bearer sk-test", Models: []string{"claude-sonnet-4"}}
	if st := s.forwardExternalAnthropic(w, r, []byte(chatBody), "claude-sonnet-4", false, prov, time.Now()); st != 0 {
		t.Fatalf("failStatus = %d, want 0", st)
	}
	mu.Lock()
	defer mu.Unlock()
	if gotPath != "/v1/messages" {
		t.Fatalf("上游路径 = %q, want /v1/messages", gotPath)
	}
	if gotKey != "sk-test" {
		t.Fatalf("x-api-key = %q, want sk-test（Bearer 前缀应剥离）", gotKey)
	}
	if gotVersion != anthropicVersion {
		t.Fatalf("anthropic-version = %q, want %q", gotVersion, anthropicVersion)
	}
	if gotAuth != "" {
		t.Fatalf("不应带 Authorization 头: %q", gotAuth)
	}
	if !strings.Contains(gotBody, `"max_tokens"`) || !strings.Contains(gotBody, `"type":"image"`) {
		t.Fatalf("上游请求体应为 Anthropic 形态（含 max_tokens/image 块）: %s", gotBody)
	}
}

func TestForwardExternalAnthropicNetworkError(t *testing.T) {
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	up.Close() // 立刻关掉 = 连接必失败
	s := newTestServer()
	chatBody := mustJSON(t, map[string]any{
		"model": "m", "messages": []any{map[string]any{"role": "user", "content": "hi"}},
	})
	r := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(chatBody))
	w := httptest.NewRecorder()
	prov := &ExternalProvider{Name: "Claude", BaseURL: up.URL, APIKey: "sk-test", Models: []string{"m"}}
	if st := s.forwardExternalAnthropic(w, r, []byte(chatBody), "m", false, prov, time.Now()); st != -1 {
		t.Fatalf("网络错 failStatus = %d, want -1", st)
	}
	if w.Body.Len() != 0 {
		t.Fatalf("网络错不应写客户端: %q", w.Body.String())
	}
}
