// tooldiag_test.go —— 11129 结构诊断单测。
// 2026-09-14 GPT-6 工具定义校验失败：summarizeToolsForDiag 只提取出站 tools
// 定义与 tool_choice，不碰 messages、不碰认证头。不碰网络。
package codebuddy

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestSummarizeToolsForDiagKeepsParameters(t *testing.T) {
	body := map[string]any{
		"model": "gpt-6-astra",
		"messages": []any{
			map[string]any{"role": "user", "content": "SECRET-MESSAGE-MUST-NOT-LEAK"},
		},
		"tools": []any{
			map[string]any{
				"type": "function",
				"function": map[string]any{
					"name":        "get_weather",
					"description": "query weather",
					"parameters": map[string]any{
						"type":                 "object",
						"properties":           map[string]any{"city": map[string]any{"type": "string"}},
						"required":             []any{"city"},
						"additionalProperties": false,
					},
					"strict": true,
				},
			},
		},
		"tool_choice": "auto",
	}
	out := summarizeToolsForDiag(body)
	if strings.Contains(out, "SECRET-MESSAGE-MUST-NOT-LEAK") {
		t.Fatalf("诊断不得包含消息正文: %s", out)
	}
	var m map[string]any
	if err := json.Unmarshal([]byte(out), &m); err != nil {
		t.Fatalf("诊断输出不是合法 JSON: %v", err)
	}
	if n, _ := m["tool_count"].(float64); n != 1 {
		t.Fatalf("tool_count 应为 1, got %v", m)
	}
	tools, _ := m["tools"].([]any)
	if len(tools) != 1 {
		t.Fatalf("tools 应有 1 条, got %v", m)
	}
	tl, _ := tools[0].(map[string]any)
	if tl["name"] != "get_weather" {
		t.Fatalf("应保留工具名, got %v", tl)
	}
	params, ok := tl["parameters"].(map[string]any)
	if !ok {
		t.Fatalf("应保留完整 parameters, got %v", tl)
	}
	if params["type"] != "object" {
		t.Fatalf("parameters.type 应保留, got %v", params)
	}
	if tl["strict"] != true {
		t.Fatalf("应保留 strict, got %v", tl)
	}
	if m["tool_choice"] != "auto" {
		t.Fatalf("应保留 tool_choice, got %v", m)
	}
}

func TestSummarizeToolsForDiagMissingParameters(t *testing.T) {
	body := map[string]any{
		"tools": []any{
			map[string]any{
				"type":     "function",
				"function": map[string]any{"name": "f"},
			},
		},
	}
	out := summarizeToolsForDiag(body)
	if !strings.Contains(out, "missing") {
		t.Fatalf("缺 parameters 应标 missing, got %s", out)
	}
}

func TestSummarizeToolsForDiagNoTools(t *testing.T) {
	body := map[string]any{"model": "x"}
	out := summarizeToolsForDiag(body)
	var m map[string]any
	if err := json.Unmarshal([]byte(out), &m); err != nil {
		t.Fatalf("诊断输出不是合法 JSON: %v", err)
	}
	if n, _ := m["tool_count"].(float64); n != 0 {
		t.Fatalf("无 tools 时 tool_count 应为 0, got %v", m)
	}
}
