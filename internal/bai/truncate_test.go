package bai

import (
	"strings"
	"testing"
)

func msg(role, content string) map[string]any {
	return map[string]any{"role": role, "content": content}
}

// 多轮超窗：应保留 system + 最近轮次 + 最后问题，删最老的大轮次
func TestTruncateKeepsRecentTurns(t *testing.T) {
	msgs := []any{
		msg("system", "你是助手"),
		msg("user", strings.Repeat("A", 2000000)), // 老的大轮（2M 字符 ≈ 50 万 token）
		msg("assistant", "ok"),
		msg("user", strings.Repeat("B", 2000000)), // 第二个大轮
		msg("user", "最后的问题"),
	}
	out, cut := truncateToContext(msgs)
	if !cut {
		t.Fatal("超窗应截断")
	}
	// 应保留：system + 尽量多的最近轮 + 最后问题；至少不能把最后问题弄丢
	joined := ""
	for _, m := range out {
		if mm, ok := m.(map[string]any); ok {
			joined += mm["content"].(string)
		}
	}
	if !strings.Contains(joined, "最后的问题") {
		t.Fatal("最后一条必须保留")
	}
	if !strings.Contains(joined, "你是助手") {
		t.Fatal("system 必须保留")
	}
	// 截断后应达标（或只剩兜底的头尾）
	if est := estTokens(out); est > baiContextLimit && len(out) > 2 {
		t.Fatalf("截断后仍超窗 est=%d len=%d（应保留尽量多但达标）", est, len(out))
	}
}

// 单条消息本身超窗：尽力截断（剩 system+问题），不 panic
func TestTruncateSingleHugeTurn(t *testing.T) {
	msgs := []any{
		msg("system", "s"),
		msg("user", strings.Repeat("C", 10000000)), // 2.5M token 单条
	}
	out, cut := truncateToContext(msgs)
	if !cut {
		t.Fatal("应截断")
	}
	if len(out) != 2 {
		t.Fatalf("单条超窗应只剩 system+问题，实际 %d 条", len(out))
	}
}

// 达标：不截断
func TestTruncateNoopWhenFit(t *testing.T) {
	msgs := []any{msg("user", "hi"), msg("assistant", "hello")}
	out, cut := truncateToContext(msgs)
	if cut || len(out) != 2 {
		t.Fatalf("达标不应截断: cut=%v len=%d", cut, len(out))
	}
}

// 配对保护：截断点落在 tool 响应上时，保留序列开头不得出现孤立 role=tool
func TestTruncateNoOrphanTool(t *testing.T) {
	msgs := []any{
		msg("system", "s"),
		msg("user", strings.Repeat("A", 3000000)), // 老的大轮（触发截断）
		msg("assistant", "思考中", ), // 占位（无 tool_calls）
		msg("user", strings.Repeat("B", 3000000)), // 第二个大轮
		// 最近的一组工具调用：assistant.tool_calls + tool 响应
		map[string]any{"role": "assistant", "content": "", "tool_calls": []any{
			map[string]any{"id": "call_1", "type": "function", "function": map[string]any{"name": "read_file", "arguments": `{"path":"a.go"}`}},
		}},
		msg("tool", `{"content":"读取结果"}`),
		msg("user", "最后的问题"),
	}
	out, cut := truncateToContext(msgs)
	if !cut {
		t.Fatal("应截断")
	}
	// 保留序列里第一条非 system 消息不允许是 role=tool（孤立配对）
	first := true
	for _, m := range out {
		mm, ok := m.(map[string]any)
		if !ok {
			continue
		}
		if r, _ := mm["role"].(string); r == "system" && first {
			continue
		}
		if r, _ := mm["role"].(string); r == "tool" && first {
			t.Fatalf("保留序列以孤立 tool 开头（配对被拆散）: %v", out)
		}
		first = false
	}
	// 若 assistant.tool_calls 被保留，其 tool 响应必须也在（相邻）
	for i, m := range out {
		mm, ok := m.(map[string]any)
		if !ok {
			continue
		}
		if _, hasTc := mm["tool_calls"]; hasTc {
			// 它的响应应该在紧邻的下一条（若后面还有消息）
			if i+1 < len(out) {
				if nxt, ok := out[i+1].(map[string]any); ok {
					if r, _ := nxt["role"].(string); r == "tool" {
						continue
					}
				}
				t.Fatalf("assistant.tool_calls 后无 tool 响应（配对不完整）: idx %d", i)
			}
		}
	}
}
