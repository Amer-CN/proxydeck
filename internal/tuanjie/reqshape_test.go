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

// testSess 构造 metadata 注入所需的测试会话（signature 变更为 *LitellmSession）。
func testSess(id string) *LitellmSession {
	return &LitellmSession{
		ID:             id,
		ConversationID: "litellm_conversation_1700000000000_abc123",
		promptSeq:      1,
	}
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
// 补 parallel_tool_calls/litellm_session_id/prompt_cache_key，客户端未带
// metadata 时注入四字段（官方 buildMetadata 每请求都带），stream/stream_options
// 在尾且 include_usage 注入。
func TestReshapeStreamOfficialOrder(t *testing.T) {
	in := []byte(`{"stream_options":{"include_usage":true},"model":"codely-flash","max_tokens":20,"reasoning_effort":"low","messages":[{"role":"user","content":"OK"}],"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":"auto","stream":true}`)
	out := reshapeChatBody(in, testSess("sess-123"))
	wantOfficialOrder(t, out, []string{
		"model", "messages", "max_tokens", "reasoning_effort", "reasoning",
		"metadata", "litellm_session_id", "prompt_cache_key",
		"tools", "parallel_tool_calls", "tool_choice",
		"stream", "stream_options",
	})
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	if m["parallel_tool_calls"] != true {
		t.Fatalf("parallel_tool_calls 应补 true，得到 %v", m["parallel_tool_calls"])
	}
	md, ok := m["metadata"].(map[string]any)
	if !ok || md == nil {
		t.Fatalf("客户端未带 metadata 时应注入，得到 %s", out)
	}
	for _, k := range []string{"prompt_id", "session_id", "cwd", "litellm_conversation_id"} {
		if _, ok := md[k]; !ok {
			t.Fatalf("注入的 metadata 缺字段 %s: %s", k, out)
		}
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

// TestReshapeInjectMetadata 客户端未带 metadata 时注入四子字段（官方
// buildMetadata 每请求都带）：prompt_id=<ID>########<N>（会话粒度计数）、
// session_id=会话 id、cwd=exe 目录、litellm_conversation_id=会话 ConversationID；
// 字段序时 metadata 在 top_p 之后。
func TestReshapeInjectMetadata(t *testing.T) {
	in := []byte(`{"stream_options":{"include_usage":true},"model":"codely-flash","max_tokens":20,"reasoning_effort":"low","messages":[{"role":"user","content":"OK"}],"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":"auto","stream":true,"top_p":0.9}`)
	sess := testSess("sess-777")
	sess.promptSeq = 7
	out := reshapeChatBody(in, sess)
	wantOfficialOrder(t, out, []string{
		"model", "messages", "max_tokens", "reasoning_effort", "reasoning", "top_p",
		"metadata", "litellm_session_id", "prompt_cache_key",
		"tools", "parallel_tool_calls", "tool_choice",
		"stream", "stream_options",
	})
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	md, ok := m["metadata"].(map[string]any)
	if !ok || md == nil {
		t.Fatalf("应注入 metadata 四字段: %s", out)
	}
	if md["prompt_id"] != "sess-777########7" {
		t.Fatalf("prompt_id 应=<会话ID>########<N>，得到 %v", md["prompt_id"])
	}
	if md["session_id"] != "sess-777" {
		t.Fatalf("session_id 应=会话 id，得到 %v", md["session_id"])
	}
	if md["litellm_conversation_id"] != sess.ConversationID {
		t.Fatalf("litellm_conversation_id 应=会话 ConversationID，得到 %v", md["litellm_conversation_id"])
	}
	if cwd, _ := md["cwd"].(string); cwd == "" {
		t.Fatalf("cwd 应为 exe 目录绝对路径，得到 %v", md["cwd"])
	}
}

// TestReshapeClientMetadataNotOverridden 客户端自带 metadata 时原样保留，
// 不补官方四字段、不删除客户端已有键。
func TestReshapeClientMetadataNotOverridden(t *testing.T) {
	in := []byte(`{"model":"m","messages":[{"role":"user","content":"OK"}],"metadata":{"user_id":"my-dev","tag":"x"},"stream":true}`)
	out := reshapeChatBody(in, testSess("s-9"))
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	md, _ := m["metadata"].(map[string]any)
	if md == nil || md["user_id"] != "my-dev" || md["tag"] != "x" {
		t.Fatalf("客户端自带 metadata 应原值透传（不补不删键），得到 %v", m["metadata"])
	}
	if _, ok := md["prompt_id"]; ok {
		t.Fatalf("客户端自带 metadata 不应被注入 prompt_id（不覆盖）: %s", out)
	}
}

// TestReshapeNonStreamDropsStreamFields 非流式：stream/stream_options 都删（官方行为）。
func TestReshapeNonStreamDropsStreamFields(t *testing.T) {
	in := []byte(`{"model":"codely-flash","stream_options":{"include_usage":true},"messages":[{"role":"user","content":"OK"}],"max_tokens":20,"stream":false}`)
	out := reshapeChatBody(in, testSess("sess-456"))
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
		"model", "messages", "max_tokens", "metadata",
		"litellm_session_id", "prompt_cache_key",
	})
}

// TestReshapeMissingStreamField 客户端根本没发 stream 字段：视为非流式。
func TestReshapeMissingStreamField(t *testing.T) {
	in := []byte(`{"messages":[{"role":"user","content":"OK"}],"model":"codely-flash"}`)
	out := reshapeChatBody(in, testSess("s"))
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
	out := reshapeChatBody(in, testSess("s"))
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
	out := reshapeChatBody(in, testSess("s"))
	wantOfficialOrder(t, out, []string{
		"model", "messages",
		"frequency_penalty", "presence_penalty",
		"metadata", "litellm_session_id", "prompt_cache_key",
		"stream", "stream_options",
	})
}

// TestReshapeClientValuesPreserved 客户端真传的值原样保留（temperature/top_p/
// max_completion_tokens 系、已有 metadata 原值透传不增删键）。
func TestReshapeClientValuesPreserved(t *testing.T) {
	in := []byte(`{"model":"m","messages":[{"role":"user","content":"OK"}],"temperature":0.2,"top_p":0.9,"max_completion_tokens":100,"metadata":{"user_id":"my-dev","tag":"x"},"stream":true,"parallel_tool_calls":false}`)
	out := reshapeChatBody(in, testSess("s"))
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
	if out := reshapeChatBody(in, testSess("s")); string(out) != "not json" {
		t.Fatalf("非法 JSON 应原样返回，得到 %s", out)
	}
}

// TestReshapeStreamOptionsPreserved 客户端 stream_options 已有其他键时保留并强include_usage。
func TestReshapeStreamOptionsPreserved(t *testing.T) {
	in := []byte(`{"model":"m","messages":[{"role":"user","content":"OK"}],"stream":true,"stream_options":{"include_usage":false}}`)
	out := reshapeChatBody(in, testSess("s"))
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	so, _ := m["stream_options"].(map[string]any)
	if so == nil || so["include_usage"] != true {
		t.Fatalf("stream_options.include_usage 应强制 true，得到 %v", m["stream_options"])
	}
}

// TestReshapeInjectsReasoningShape 客户端只发 chat 形态 reasoning_effort 时，
// 紧邻补一份团结上游真正解析的 Responses 形态 reasoning:{effort,summary:"auto"}。
// 2026-09-11 直连实测：reasoning_effort 上游只接收不解析（非法值 "banana" 也
// 200、low/high/max 无差别），reasoning.effort 才按档位分档。
// 注入的 effort 值经 reasoningEffortWire 映射（对齐官方 CLI 的 WSi()）：
// 客户端发 max 时注入 xhigh（实测 xhigh 不弱于 max）。
func TestReshapeInjectsReasoningShape(t *testing.T) {
	in := []byte(`{"model":"codely-core","messages":[{"role":"user","content":"OK"}],"max_tokens":20,"reasoning_effort":"max","stream":true}`)
	out := reshapeChatBody(in, testSess("s"))
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	if m["reasoning_effort"] != "max" {
		t.Fatalf("原 reasoning_effort 应保留，得到 %v", m["reasoning_effort"])
	}
	r, _ := m["reasoning"].(map[string]any)
	if r == nil || r["effort"] != "xhigh" || r["summary"] != "auto" {
		t.Fatalf("应注入 reasoning:{effort:xhigh,summary:auto}，得到 %v", m["reasoning"])
	}
	order := topFieldOrder(t, out)
	for i, k := range order {
		if k == "reasoning" && (i == 0 || order[i-1] != "reasoning_effort") {
			t.Fatalf("reasoning 应紧跟 reasoning_effort，得到 %v", order)
		}
	}
}

// TestReshapeClientReasoningNotOverridden 客户端自带 reasoning 时原值保留，
// 不注入、不覆盖、不重复。
func TestReshapeClientReasoningNotOverridden(t *testing.T) {
	in := []byte(`{"model":"m","messages":[{"role":"user","content":"OK"}],"reasoning_effort":"low","reasoning":{"effort":"high","summary":"concise"},"stream":true}`)
	out := reshapeChatBody(in, testSess("s"))
	var m map[string]any
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("重排后 JSON 非法: %v", err)
	}
	r, _ := m["reasoning"].(map[string]any)
	if r == nil || r["effort"] != "high" || r["summary"] != "concise" {
		t.Fatalf("客户端自带 reasoning 应原值保留，得到 %v", m["reasoning"])
	}
	n := 0
	for _, k := range topFieldOrder(t, out) {
		if k == "reasoning" {
			n++
		}
	}
	if n != 1 {
		t.Fatalf("reasoning 只应出现一次，得到 %d 次: %s", n, out)
	}
}

// TestReshapeReasoningEffortNonString 非字符串（null/数字）reasoning_effort
// 不注入 reasoning，避免给上游塞垃圾值。
func TestReshapeReasoningEffortNonString(t *testing.T) {
	for _, in := range []string{
		`{"model":"m","messages":[{"role":"user","content":"OK"}],"reasoning_effort":null,"stream":true}`,
		`{"model":"m","messages":[{"role":"user","content":"OK"}],"reasoning_effort":5,"stream":true}`,
		`{"model":"m","messages":[{"role":"user","content":"OK"}],"reasoning_effort":"","stream":true}`,
	} {
		out := reshapeChatBody([]byte(in), testSess("s"))
		var m map[string]any
		if err := json.Unmarshal(out, &m); err != nil {
			t.Fatalf("重排后 JSON 非法: %v", err)
		}
		if _, ok := m["reasoning"]; ok {
			t.Fatalf("非字符串/空值 reasoning_effort 不应注入 reasoning: %s", out)
		}
	}
}

// TestReasoningEffortWire 档位值映射（对齐官方 CLI 的 WSi()）：minimal→low、
// low/medium/high/xhigh 原样（大小写不敏感）、max→xhigh；
// 官方梯子之外的值（off/none/未知）原样透传。
func TestReasoningEffortWire(t *testing.T) {
	cases := []struct{ in, want string }{
		{"minimal", "low"},
		{"Minimal", "low"},
		{"low", "low"},
		{"medium", "medium"},
		{"high", "high"},
		{"xhigh", "xhigh"},
		{"max", "xhigh"},
		{"MAX", "xhigh"},
		{"off", "off"},
		{"none", "none"},
		{"banana", "banana"},
	}
	for _, c := range cases {
		if got := reasoningEffortWire(c.in); got != c.want {
			t.Fatalf("reasoningEffortWire(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

// TestReasoningTiersFor 档位按模型声明：官方目录只给三个模型声明
// [max, high, low]，其余别名未声明返回空串。
func TestReasoningTiersFor(t *testing.T) {
	for _, m := range []string{"codely-core", "KIMI-K3", "GLM-5.3-FLASH"} {
		if got := reasoningTiersFor(m); got != "max/high/low" {
			t.Fatalf("reasoningTiersFor(%q) = %q, want max/high/low", m, got)
		}
	}
	for _, m := range []string{"codely-basic", "codely-vl", "GLM-5.3"} {
		if got := reasoningTiersFor(m); got != "" {
			t.Fatalf("reasoningTiersFor(%q) = %q, want 空串（官方未声明）", m, got)
		}
	}
}
