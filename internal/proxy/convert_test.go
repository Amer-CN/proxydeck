package proxy

import (
	"testing"

	"github.com/Amer-CN/proxydeck/internal/api"
)

func TestConvertMessagesSkipsEmptyContent(t *testing.T) {
	msgs := []api.OpenAIMessage{
		{Role: "user", Content: "hello"},
		{Role: "user", Content: nil},     // null content (e.g. reasoning item from Codex)
		{Role: "user", Content: ""},      // empty string content
		{Role: "user", Content: []any{}}, // empty content array
		{Role: "assistant", Content: "reply"},
		{Role: "assistant", ToolCalls: []api.ToolCall{{ID: "c1", Type: "function", Function: api.FunctionCall{Name: "bash", Arguments: "{}"}}}},
		{Role: "tool", ToolCallID: "c1", Name: "bash", Content: "ok"},
	}

	cc := ConvertMessages(msgs)
	if len(cc) != 4 {
		t.Fatalf("expected 4 messages (3 empty skipped), got %d", len(cc))
	}
	for i, m := range cc {
		if m.Content == nil || len(m.Content) == 0 {
			t.Fatalf("message %d (role %s) has empty content", i, m.Role)
		}
	}
	if cc[0].Role != "user" || cc[1].Role != "assistant" || cc[2].Role != "assistant" || cc[3].Role != "tool" {
		t.Fatalf("unexpected order/roles: %+v", cc)
	}
}

// TestExtractSystemDeveloperRole: DSH 等框架用 role:"developer" 做系统提示，
// 必须与 "system" 同等提取，否则原样转发导致上游 400（只认 user|assistant|tool）。
func TestExtractSystemDeveloperRole(t *testing.T) {
	msgs := []api.OpenAIMessage{
		{Role: "developer", Content: "You are an AI agent."},
		{Role: "user", Content: "hi"},
	}
	sys, rest := ExtractSystem(msgs)
	if sys != "You are an AI agent." {
		t.Fatalf("system = %q, want developer content extracted", sys)
	}
	if len(rest) != 1 || rest[0].Role != "user" {
		t.Fatalf("rest = %+v, want only the user message", rest)
	}
}

// TestConvertUnknownRoleFallback: 未知 role（如残留的 developer）兜底为 user，
// 保证上游 schema（user|assistant|tool）不会因非法 role 报 400。
func TestConvertUnknownRoleFallback(t *testing.T) {
	msgs := []api.OpenAIMessage{
		{Role: "developer", Content: "instructions"},
		{Role: "user", Content: "hi"},
	}
	cc := ConvertMessages(msgs)
	if len(cc) != 2 {
		t.Fatalf("len = %d, want 2", len(cc))
	}
	for i, m := range cc {
		if m.Role != "user" && m.Role != "assistant" && m.Role != "tool" {
			t.Fatalf("cc[%d].role = %q, must be user|assistant|tool", i, m.Role)
		}
	}
}
