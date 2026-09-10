package codebuddy

import (
	"encoding/json"
	"strings"
	"testing"
)

// 流式清洗：空字符串 content / reasoning_content 键剔除，非空与非 data 行原样。
func TestSSEFieldCleanerStripsEmptyKeys(t *testing.T) {
	in := strings.Join([]string{
		`data: {"choices":[{"delta":{"role":"assistant","content":"","reasoning_content":"The"}}]}`,
		``,
		`data: {"choices":[{"delta":{"content":"","reasoning_content":" fox"}}]}`,
		``,
		`data: {"choices":[{"delta":{"content":"ok","reasoning_content":""}}]}`,
		``,
		`data: [DONE]`,
		``,
	}, "\n")
	c := &sseFieldCleaner{}
	out := string(c.feed([]byte(in))) + string(c.flush())

	var deltas []map[string]any
	for _, line := range strings.Split(out, "\n") {
		line = strings.TrimSpace(line)
		if !strings.HasPrefix(line, "data:") || strings.Contains(line, "[DONE]") {
			if strings.Contains(line, "[DONE]") && !strings.Contains(in, line) {
				t.Fatalf("[DONE] 应原样保留, got %q", line)
			}
			continue
		}
		var d map[string]any
		if err := json.Unmarshal([]byte(strings.TrimPrefix(line, "data:")), &d); err != nil {
			t.Fatalf("重写后的 data 行应为合法 JSON: %v (%q)", err, line)
		}
		deltas = append(deltas, d)
	}
	if len(deltas) != 3 {
		t.Fatalf("应解析出 3 个 delta, got %d (%q)", len(deltas), out)
	}
	check := func(i int, wantR, wantC string) {
		delta := deltas[i]["choices"].([]any)[0].(map[string]any)["delta"].(map[string]any)
		if wantR == "" {
			if _, exists := delta["reasoning_content"]; exists {
				t.Fatalf("delta%d: 空 reasoning_content 应被剔除, got %v", i+1, delta)
			}
		} else if delta["reasoning_content"] != wantR {
			t.Fatalf("delta%d: reasoning_content=%v, want %q", i+1, delta["reasoning_content"], wantR)
		}
		if wantC == "" {
			if _, exists := delta["content"]; exists {
				t.Fatalf("delta%d: 空 content 应被剔除, got %v", i+1, delta)
			}
		} else if delta["content"] != wantC {
			t.Fatalf("delta%d: content=%v, want %q", i+1, delta["content"], wantC)
		}
	}
	check(0, "The", "")
	check(1, " fox", "")
	check(2, "", "ok")
}

// 两键同时非空时原样保留（真实并发输出不受影响）。
func TestSSEFieldCleanerKeepsBothNonEmpty(t *testing.T) {
	line := `data: {"choices":[{"delta":{"content":"a","reasoning_content":"b"}}]}` + "\n"
	c := &sseFieldCleaner{}
	if got := string(c.feed([]byte(line))); got != line {
		t.Fatalf("双非空 delta 应原样透传, got %q", got)
	}
}

// 不完整行跨 feed 缓冲，flush 冲尾。
func TestSSEFieldCleanerPartialLine(t *testing.T) {
	full := `data: {"choices":[{"delta":{"content":"","reasoning_content":"hi"}}]}` + "\n"
	// 从中间任意位置切断都应正确拼装
	for cut := 1; cut < len(full)-1; cut++ {
		c2 := &sseFieldCleaner{}
		out := string(c2.feed([]byte(full[:cut]))) + string(c2.feed([]byte(full[cut:]))) + string(c2.flush())
		if strings.Contains(out, `"content":""`) {
			t.Fatalf("cut=%d: 空键未被剔除, out=%q", cut, out)
		}
		if !strings.Contains(out, `"reasoning_content":"hi"`) {
			t.Fatalf("cut=%d: 非空键丢失, out=%q", cut, out)
		}
	}
}

// 非 data 行（注释/空行/坏 JSON）原样透传。
func TestSSEFieldCleanerPassthrough(t *testing.T) {
	in := ": keep-alive\n\nnot json data: xxx\ndata: not-json\n"
	c := &sseFieldCleaner{}
	if got := string(c.feed([]byte(in))); got != in {
		t.Fatalf("非 data 行应原样透传, got %q", got)
	}
}

// 空数组的 tool_calls 键剔除：ZCode 用 `tool_calls != null` 判定工具调用，
// 空数组同样命中，会把一次思考按帧切成逐 token 的几十条。
func TestSSEFieldCleanerStripsEmptyToolCalls(t *testing.T) {
	line := `data: {"choices":[{"delta":{"role":"assistant","content":"","reasoning_content":"fox","tool_calls":[]}},` +
		`{"index":0,"delta":{"content":"ok","tool_calls":[]}}]}` + "\n"
	c := &sseFieldCleaner{}
	out := string(c.feed([]byte(line))) + string(c.flush())
	if strings.Contains(out, `"tool_calls"`) {
		t.Fatalf("空数组 tool_calls 应被剔除, got %q", out)
	}
	if !strings.Contains(out, `"reasoning_content":"fox"`) || !strings.Contains(out, `"content":"ok"`) {
		t.Fatalf("非空键不应丢失, got %q", out)
	}
	if !strings.Contains(out, `"role":"assistant"`) {
		t.Fatalf("其他键不应丢失, got %q", out)
	}
}

// 真实工具调用（非空数组）必须原样保留，不能被清洗器误伤。
func TestSSEFieldCleanerKeepsRealToolCalls(t *testing.T) {
	line := `data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function",` +
		`"function":{"name":"read","arguments":"{}"}}]}}]}` + "\n"
	c := &sseFieldCleaner{}
	if got := string(c.feed([]byte(line))) + string(c.flush()); got != line {
		t.Fatalf("非空 tool_calls 应原样透传, got %q", got)
	}
}
