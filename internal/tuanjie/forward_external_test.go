// forward_external_test.go —— 外部 provider 转发的单测：
// sanitizeForExternal 三种修复路径（补 id / 回填 tool_call_id / 降级 user）
// 与 ProviderStore.Match 命中规则（空 models 不命中）。
package tuanjie

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// mustJSON 序列化失败直接挂测试。
func mustJSON(t *testing.T, v any) string {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	return string(b)
}

// TestSanitizeFillsMissingToolCallID assistant.tool_calls 缺 id → 补 call_xxx。
func TestSanitizeFillsMissingToolCallID(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model": "agnes-2.5-flash",
		"messages": []any{
			map[string]any{"role": "assistant", "content": "", "tool_calls": []any{
				map[string]any{"type": "function", "function": map[string]any{"name": "f", "arguments": "{}"}},
			}},
		},
	})
	out := sanitizeForExternal([]byte(in))
	var m struct {
		Messages []struct {
			Role      string `json:"role"`
			ToolCalls []struct {
				ID string `json:"id"`
			} `json:"tool_calls"`
		} `json:"messages"`
	}
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(m.Messages) != 1 || len(m.Messages[0].ToolCalls) != 1 {
		t.Fatalf("消息结构被破坏: %s", out)
	}
	id := m.Messages[0].ToolCalls[0].ID
	if len(id) <= len("call_") || id[:len("call_")] != "call_" {
		t.Fatalf("缺 id 未补齐，got %q", id)
	}
}

// TestSanitizeBackfillsToolCallID role=tool 缺 tool_call_id → 按序回填前面的 id。
func TestSanitizeBackfillsToolCallID(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model": "agnes-2.5-flash",
		"messages": []any{
			map[string]any{"role": "assistant", "content": "", "tool_calls": []any{
				map[string]any{"id": "call_aaa111", "type": "function", "function": map[string]any{"name": "f", "arguments": "{}"}},
			}},
			map[string]any{"role": "tool", "content": "result"},
		},
	})
	out := sanitizeForExternal([]byte(in))
	var m struct {
		Messages []struct {
			Role         string `json:"role"`
			ToolCallID   string `json:"tool_call_id"`
			ToolCalls    []struct {
				ID string `json:"id"`
			} `json:"tool_calls"`
		} `json:"messages"`
	}
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(m.Messages) != 2 {
		t.Fatalf("消息数不对: %s", out)
	}
	if m.Messages[1].Role != "tool" || m.Messages[1].ToolCallID != "call_aaa111" {
		t.Fatalf("tool_call_id 未回填: role=%q tool_call_id=%q", m.Messages[1].Role, m.Messages[1].ToolCallID)
	}
}

// TestSanitizeDowngradesOrphanTool 无对应调用的 role=tool → 降级为 user 文本。
func TestSanitizeDowngradesOrphanTool(t *testing.T) {
	in := mustJSON(t, map[string]any{
		"model": "agnes-2.5-flash",
		"messages": []any{
			map[string]any{"role": "user", "content": "hi"},
			map[string]any{"role": "tool", "content": []any{
				map[string]any{"type": "text", "text": "part1"},
				map[string]any{"type": "text", "text": "part2"},
			}},
		},
	})
	out := sanitizeForExternal([]byte(in))
	var m struct {
		Messages []struct {
			Role       string          `json:"role"`
			Content    json.RawMessage `json:"content"`
			ToolCallID string          `json:"tool_call_id"`
		} `json:"messages"`
	}
	if err := json.Unmarshal(out, &m); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(m.Messages) != 2 {
		t.Fatalf("消息数不对: %s", out)
	}
	last := m.Messages[1]
	if last.Role != "user" {
		t.Fatalf("孤立 tool 消息未降级为 user，role=%q", last.Role)
	}
	var content string
	if err := json.Unmarshal(last.Content, &content); err != nil {
		t.Fatalf("降级后 content 应为字符串: %s", last.Content)
	}
	if content != "[tool result] part1 part2" {
		t.Fatalf("降级文本不对: %q", content)
	}
	if last.ToolCallID != "" {
		t.Fatalf("降级后不应残留 tool_call_id: %q", last.ToolCallID)
	}
}

// TestSanitizeKeepsBodyWhenNoMessages 无 messages / 非 JSON → 原样返回。
func TestSanitizeKeepsBodyWhenNoMessages(t *testing.T) {
	in := []byte(`{"model":"x"}`)
	if out := sanitizeForExternal(in); string(out) != string(in) {
		t.Fatalf("无 messages 的 body 被改动: %s", out)
	}
}

// TestProviderMatchAndEmptyModels Match 命中返回副本；空 models 不命中。
func TestProviderMatchAndEmptyModels(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "tuanjie-providers.json")
	cfg := `[{"name":"agnes","base_url":"https://api.agnes-ai.cn/v1","api_key":"sk-x","models":["agnes-2.5-flash","agnes-image-2.1-flash"]},{"name":"bare","base_url":"https://b.example/v1","api_key":"sk-y"}]`
	if err := os.WriteFile(path, []byte(cfg), 0o600); err != nil {
		t.Fatalf("write: %v", err)
	}
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	ps := NewProviderStore()
	if p := ps.Match("agnes-2.5-flash"); p == nil {
		t.Fatal("命中模型应返回 provider")
	} else if p.Name != "agnes" || p.BaseURL != "https://api.agnes-ai.cn/v1" {
		t.Fatalf("命中返回的 provider 不对: %+v", p)
	}
	if p := ps.Match("GLM-4.5"); p != nil {
		t.Fatalf("未配置的模型不应命中: %+v", p)
	}
	// 向后兼容：旧配置没有 models 字段 = 仅展示，不参与转发
	if p := ps.Match(""); p != nil {
		t.Fatalf("空模型不应命中: %+v", p)
	}
	if p := ps.Match("anything-of-bare"); p != nil {
		t.Fatalf("空 models 的 provider 不应命中任何模型: %+v", p)
	}
	// AllModels 只含配置了 models 的 provider 条目
	all := ps.AllModels()
	if len(all) != 2 {
		t.Fatalf("AllModels 应有 2 条，got %d", len(all))
	}
	if all[0].ID != "agnes-2.5-flash" || all[0].OwnedBy != "agnes" {
		t.Fatalf("AllModels 条目不对: %+v", all[0])
	}
}
