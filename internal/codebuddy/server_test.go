// toolchoice_test.go —— 对象式 tool_choice 降级单测。
// 2026-09-13 实测：腾讯后端 tool_choice 字段是 Go string，收到对象一律 400
//（plugins/codebuddy.log:4622 实录），故出站前降级为字符串式。不碰网络。
package codebuddy

import "testing"

func TestNormalizeToolChoiceFunctionToRequired(t *testing.T) {
	body := map[string]any{
		"tools":       []any{map[string]any{"type": "function", "function": map[string]any{"name": "get_weather"}}},
		"tool_choice": map[string]any{"type": "function", "function": map[string]any{"name": "get_weather"}},
	}
	normalizeToolChoice(body)
	if got, _ := body["tool_choice"].(string); got != "required" {
		t.Fatalf("强制指定式应降级为 required, got %#v", body["tool_choice"])
	}
}

func TestNormalizeToolChoiceTypeAuto(t *testing.T) {
	body := map[string]any{
		"tools":       []any{map[string]any{"type": "function"}},
		"tool_choice": map[string]any{"type": "auto"},
	}
	normalizeToolChoice(body)
	if got, _ := body["tool_choice"].(string); got != "auto" {
		t.Fatalf("{\"type\":\"auto\"} 应为 auto, got %#v", body["tool_choice"])
	}
}

func TestNormalizeToolChoiceStringUnchanged(t *testing.T) {
	body := map[string]any{
		"tools":       []any{map[string]any{"type": "function"}},
		"tool_choice": "required",
	}
	normalizeToolChoice(body)
	if got, _ := body["tool_choice"].(string); got != "required" {
		t.Fatalf("字符串式应原样保留, got %#v", body["tool_choice"])
	}
}

func TestNormalizeToolChoiceDroppedWithoutTools(t *testing.T) {
	body := map[string]any{
		"tool_choice": map[string]any{"type": "function", "function": map[string]any{"name": "x"}},
	}
	normalizeToolChoice(body)
	if _, ok := body["tool_choice"]; ok {
		t.Fatalf("无 tools 时应删除 tool_choice, got %#v", body["tool_choice"])
	}
}
