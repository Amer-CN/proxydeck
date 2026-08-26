package tuanjie

import (
	"encoding/json"
	"strings"
	"testing"
)

// topFieldOrder 解析 JSON 顶层键序（Go json.Decoder 保证字段顺序）。
func topFieldOrder(t *testing.T, body []byte) []string {
	t.Helper()
	dec := json.NewDecoder(strings.NewReader(string(body)))
	tok, err := dec.Token()
	if err != nil || tok != json.Delim('{') {
		t.Fatalf("body 不是 JSON 对象: %v %s", err, body)
	}
	var order []string
	for dec.More() {
		keyTok, err := dec.Token()
		if err != nil {
			t.Fatalf("读键失败: %v", err)
		}
		key, _ := keyTok.(string)
		order = append(order, key)
		var raw json.RawMessage
		if err := dec.Decode(&raw); err != nil {
			t.Fatalf("读值失败: %v", err)
		}
	}
	return order
}

// wantOfficialOrder 官方 CLI 字段顺序断言（裁掉请求里没带的可选字段后应严格命中）。
func wantOfficialOrder(t *testing.T, body []byte, want []string) {
	t.Helper()
	got := topFieldOrder(t, body)
	if len(got) != len(want) {
		t.Fatalf("字段序不符:\n got  %v\n want %v\nbody: %s", got, want, body)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("字段序不符:\n got  %v\n want %v\nbody: %s", got, want, body)
		}
	}
}

// TestReshapeStreamOfficialOrder 流式 ZCode 形态重排：字段序=官方序，
// 补 parallel_tool_calls/litellm_session_id/prompt_cache_key（不补 metadata），
// stream/stream_options 在尾且 include_usage 注入。
func TestReshapeStreamOfficialOrder(t *testing.T) {
	in := []byte(`{"stream_options":{"include_usage":true},"model":"codely-flash","max_tokens":20,"reasoning_effort":"low","messages":[{"role":"user","content":"OK"}],"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":"auto","stream":true}`)
	out := reshapeChatBody(in, "sess-123")
	wantOfficialOrder(t, out, []string{
		"model", "messages", "max_tokens", "reasoning_effort",
		"tools", "parallel_tool_calls", "tool_choice",
		"litellm_session_id", "prompt_cache_key",
		"stream", "stream_options",
	})
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	if m["parallel_tool_calls"] != true {
		t.Fatalf("parallel_tool_calls 应补 true，得到 %v", m["parallel_tool_calls"])
	}
	if _, ok := m["metadata"]; ok {
		t.Fatalf("客户端未带 metadata 时不应补 metadata: %s", out)
	}
	if m["litellm_session_id"] != "sess-123" {
		t.Fatalf("litellm_session_id 应与 sessionID 同值，得到 %v", m["litellm_session_id"])
	}
	if m["prompt_cache_key"] != "sess-123" {
		t.Fatalf("prompt_cache_key 应=会话 id，得到 %v", m["prompt_cache_key"])
	}
	if m["stream"] != true {
		t.Fatalf("stream 应为 true，得到 %v", m["stream"])
	}
	so, _ := m["stream_options"].(map[string]any)
	if so == nil || so["include_usage"] != true {
		t.Fatalf("stream_options.include_usage 应 true，得到 %v", m["stream_options"])
	}
}

// TestReshapeNonStreamDropsStreamFields 非流式：stream/stream_options 都删（官方行为）。
func TestReshapeNonStreamDropsStreamFields(t *testing.T) {
	in := []byte(`{"model":"codely-flash","stream_options":{"include_usage":true},"messages":[{"role":"user","content":"OK"}],"max_tokens":20,"stream":false}`)
	out := reshapeChatBody(in, "sess-456")
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	if _, ok := m["stream"]; ok {
		t.Fatalf("非流式不应带 stream 字段: %s", out)
	}
	if _, ok := m["stream_options"]; ok {
		t.Fatalf("非流式不应带 stream_options 字段: %s", out)
	}
	wantOfficialOrder(t, out, []string{
		"model", "messages", "max_tokens",
		"litellm_session_id", "prompt_cache_key",
	})
}

// TestReshapeMissingStreamField 客户端根本没发 stream 字段：视为非流式。
func TestReshapeMissingStreamField(t *testing.T) {
	in := []byte(`{"messages":[{"role":"user","content":"OK"}],"model":"codely-flash"}`)
	out := reshapeChatBody(in, "s")
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	if _, ok := m["stream"]; ok {
		t.Fatalf("未发 stream 的请求不应带 stream 字段: %s", out)
	}
	if _, ok := m["stream_options"]; ok {
		t.Fatalf("未发 stream 的请求不应带 stream_options 字段: %s", out)
	}
}

// TestReshapeNoToolsNoParallel 无 tools 时不补 parallel_tool_calls（官方成对出现）。
func TestReshapeNoToolsNoParallel(t *testing.T) {
	in := []byte(`{"model":"m","messages":[{"role":"user","content":"OK"}],"stream":true}`)
	out := reshapeChatBody(in, "s")
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	if _, ok := m["parallel_tool_calls"]; ok {
		t.Fatalf("无 tools 不应补 parallel_tool_calls: %s", out)
	}
}

// TestReshapeRareFieldsBeforeMetadata 罕见字段（官方清单外）排 metadata 之前，
// 字母序稳定。
func TestReshapeRareFieldsBeforeMetadata(t *testing.T) {
	in := []byte(`{"frequency_penalty":0.5,"model":"m","messages":[{"role":"user","content":"OK"}],"presence_penalty":0.1,"stream":true}`)
	out := reshapeChatBody(in, "s")
	wantOfficialOrder(t, out, []string{
		"model", "messages",
		"frequency_penalty", "presence_penalty",
		"litellm_session_id", "prompt_cache_key",
		"stream", "stream_options",
	})
}

// TestReshapeClientValuesPreserved 客户端真传的值原样保留（temperature/top_p/
// max_completion_tokens 系、已有 metadata 原值透传不增删键）。
func TestReshapeClientValuesPreserved(t *testing.T) {
	in := []byte(`{"model":"m","messages":[{"role":"user","content":"OK"}],"temperature":0.2,"top_p":0.9,"max_completion_tokens":100,"metadata":{"user_id":"my-dev","tag":"x"},"stream":true,"parallel_tool_calls":false}`)
	out := reshapeChatBody(in, "s")
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	if m["temperature"] != 0.2 || m["top_p"] != 0.9 {
		t.Fatalf("temperature/top_p 应原样保留: %s", out)
	}
	if n, _ := m["max_completion_tokens"].(float64); n != 100 {
		t.Fatalf("max_completion_tokens 应原样保留: %s", out)
	}
	if m["parallel_tool_calls"] != false {
		t.Fatalf("客户端显式传的 parallel_tool_calls 不应被覆盖: %s", out)
	}
	md, _ := m["metadata"].(map[string]any)
	if md == nil || md["user_id"] != "my-dev" || md["tag"] != "x" {
		t.Fatalf("已有 metadata 应原值透传（不补不删键），得到 %v", m["metadata"])
	}
	if _, ok := m["litellm_session_id"]; !ok {
		t.Fatalf("litellm_session_id 应补: %s", out)
	}
	if _, ok := m["prompt_cache_key"]; !ok {
		t.Fatalf("prompt_cache_key 应补: %s", out)
	}
}

// TestReshapeInvalidJSONPassthrough 非 JSON 请求原样返回（不拦转发）。
func TestReshapeInvalidJSONPassthrough(t *testing.T) {
	in := []byte("not json")
	if out := reshapeChatBody(in, "s"); string(out) != "not json" {
		t.Fatalf("非法 JSON 应原样返回，得到 %s", out)
	}
}

// TestReshapeStreamOptionsPreserved 客户端 stream_options 已有其他键时保留并强include_usage。
func TestReshapeStreamOptionsPreserved(t *testing.T) {
	in := []byte(`{"model":"m","messages":[{"role":"user","content":"OK"}],"stream":true,"stream_options":{"include_usage":false}}`)
	out := reshapeChatBody(in, "s")
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	so, _ := m["stream_options"].(map[string]any)
	if so == nil || so["include_usage"] != true {
		t.Fatalf("stream_options.include_usage 应强制 true，得到 %v", m["stream_options"])
	}
}
