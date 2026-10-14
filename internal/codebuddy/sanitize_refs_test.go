package codebuddy

import (
	"encoding/json"
	"strings"
	"testing"
)

// 兄弟键按 2020-12 合并（同名兄弟键优先），且不留 $ref。
func TestInlineSchemaRefsSiblingMerge(t *testing.T) {
	defs := map[string]any{
		"A": map[string]any{"type": "string", "description": "orig"},
	}
	got, ok := inlineSchemaRefs(map[string]any{
		"$ref": "#/$defs/A", "description": "override",
	}, defs, nil).(map[string]any)
	if !ok {
		t.Fatalf("应返回 map，实际 %T", got)
	}
	if got["type"] != "string" || got["description"] != "override" {
		t.Errorf("兄弟键合并错误：%v", got)
	}
	if _, has := got["$ref"]; has {
		t.Errorf("仍有 $ref 残留：%v", got)
	}
}

// 循环引用必须终止（A→B→A），不得栈溢出/死循环。
func TestInlineSchemaRefsCycle(t *testing.T) {
	defs := map[string]any{
		"A": map[string]any{"next": map[string]any{"$ref": "#/$defs/B"}},
		"B": map[string]any{"next": map[string]any{"$ref": "#/$defs/A"}},
	}
	got := inlineSchemaRefs(map[string]any{"$ref": "#/$defs/A"}, defs, nil)
	s, _ := json.Marshal(got)
	if strings.Count(string(s), "cyclic-ref-removed") != 1 {
		t.Errorf("循环应被截断恰一次，实际：%s", string(s)[:200])
	}
	if strings.Contains(string(s), `"$ref"`) {
		t.Errorf("$ref 应被消灭干净")
	}
}

// 外部引用与缺失定义原样保留（不编造内容）。
func TestInlineSchemaRefsPassthrough(t *testing.T) {
	defs := map[string]any{"A": map[string]any{"type": "string"}}
	ext := map[string]any{"$ref": "https://example.com/s.json#/x"}
	if got := inlineSchemaRefs(ext, defs, nil); !equalJSON(got, ext) {
		t.Errorf("外部引用应原样保留，实际 %v", got)
	}
	missing := map[string]any{"$ref": "#/$defs/Nope"}
	if got := inlineSchemaRefs(missing, defs, nil); !equalJSON(got, missing) {
		t.Errorf("缺失定义应原样保留，实际 %v", got)
	}
}

// sanitizeToolRefs：有 $ref 的工具被内联并丢 $defs；无 $ref 的一字不动；畸形不 panic。
func TestSanitizeToolRefs(t *testing.T) {
	withRef := map[string]any{
		"type": "function",
		"function": map[string]any{"name": "w",
			"description": "w",
			"parameters": map[string]any{
				"type": "object",
				"properties": map[string]any{
					"v": map[string]any{"$ref": "#/$defs/A", "description": "vv"},
				},
				"required": []any{"v"},
				"$defs":    map[string]any{"A": map[string]any{"type": "string"}},
			}},
	}
	plainParams := map[string]any{
		"type":       "object",
		"properties": map[string]any{"q": map[string]any{"type": "string"}},
	}
	plain := map[string]any{
		"type": "function",
		"function": map[string]any{"name": "p", "description": "p",
			"parameters": plainParams},
	}
	plainBefore, _ := json.Marshal(plainParams)
	body := map[string]any{"tools": []any{withRef, plain, "junk", 42}}
	sanitizeToolRefs(body)

	fn := withRef["function"].(map[string]any)
	pm := fn["parameters"].(map[string]any)
	if _, has := pm["$defs"]; has {
		t.Errorf("$defs 用完应丢弃")
	}
	bs, _ := json.Marshal(pm)
	if strings.Contains(string(bs), `"$ref"`) {
		t.Errorf("$ref 应被消灭干净：%s", string(bs)[:200])
	}
	vp := pm["properties"].(map[string]any)["v"].(map[string]any)
	if vp["type"] != "string" || vp["description"] != "vv" {
		t.Errorf("内联合并错误：%v", vp)
	}
	if after, _ := json.Marshal(plainParams); string(after) != string(plainBefore) {
		t.Errorf("无 $ref 工具必须一字不动")
	}
	// 无 tools / 畸形 tools 不得 panic
	sanitizeToolRefs(map[string]any{})
	sanitizeToolRefs(map[string]any{"tools": "nope"})
}

func equalJSON(a, b any) bool {
	x, err1 := json.Marshal(a)
	y, err2 := json.Marshal(b)
	return err1 == nil && err2 == nil && string(x) == string(y)
}
