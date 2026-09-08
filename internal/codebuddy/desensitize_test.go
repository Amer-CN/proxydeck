// desensitize_test.go —— 11128 指纹清洗 + 思考缺省档注入判定的纯逻辑单测
// （2026-09-08 实证上游精确字符串黑名单）。不碰网络、不起 httptest。
package codebuddy

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// 指纹删除：标准句逐字等于 `Main branch: main`（wnddd839 v0.3.10 实测 400→200
// 的最小改动：仅删括号注解，语义无损）。
func TestFingerprintStripMainBranch(t *testing.T) {
	got := stripFingerprints("Main branch (you will usually use this for PRs): main")
	if want := "Main branch: main"; got != want {
		t.Fatalf("标准句应逐字等于 %q, got %q", want, got)
	}

	// 小写变体同样命中（黑名单大小写不敏感），$1 保留原大小写
	got = stripFingerprints("main branch (you will usually use this for prs): dev")
	if want := "main branch: dev"; got != want {
		t.Fatalf("小写变体应删除括号注解, got %q", got)
	}

	// 不含该句的文本原样返回
	plain := "please keep the user informed with concise progress updates."
	if got := stripFingerprints(plain); got != plain {
		t.Fatalf("不含指纹句的文本应原样返回, got %q", got)
	}
}

// 词表增补生效：Codex / OpenAI / Error: API error 在 system 文本中被插入零宽（U+200B）。
// "API error" 为主智能体裁决后的裸词形（无尾冒号），须覆盖两点：
// `API error: 500` 带冒号+空格形态能命中；`API errors` 复数形态不被误命中。
func TestSensitiveTermsNewEntries(t *testing.T) {
	for _, term := range []string{"Codex", "OpenAI", "Error: API error"} {
		text := "runtime notice: " + term + " referenced"
		got := DesensitizeText(text)
		if !strings.Contains(got, zwsp) {
			t.Fatalf("%q 应被插入零宽空格, got %q", term, got)
		}
	}

	// 带冒号+空格的常见形态能命中（裸词形覆盖带冒号形态）
	if got := DesensitizeText("runtime notice: API error: 500"); !strings.Contains(got, zwsp) {
		t.Fatalf(`"API error: 500" 应被插入零宽空格, got %q`, got)
	}
	// 复数形态不被误命中（\b 尾边界挡住 error+s）
	if got, want := DesensitizeText("runtime notice: API errors"), "runtime notice: API errors"; got != want {
		t.Fatalf(`"API errors" 不应被命中, got %q`, got)
	}
}

// 完整链路：DesensitizeBody 对 system 命中角色先删指纹、再插零宽（顺序：删除优先）。
func TestDesensitizeBodyFingerprintChain(t *testing.T) {
	body := map[string]any{
		"model": "hy4-preview",
		"messages": []any{
			map[string]any{
				"role":    "system",
				"content": "Main branch (you will usually use this for PRs): main. Codex CLI rules apply.",
			},
			map[string]any{
				"role": "system",
				"content": []any{
					map[string]any{"type": "text",
						"text": "Main branch (you will usually use this for PRs): main"},
				},
			},
		},
	}
	out := DesensitizeBody(body, []string{"system"}, false, false, false)
	msgs := out["messages"].([]any)

	sys, _ := msgs[0].(map[string]any)
	if got, _ := sys["content"].(string); got != "Main branch: main. C"+zwsp+"odex CLI rules apply." {
		t.Fatalf("system 字符串 content 应先删指纹再插零宽, got %q", got)
	}

	user, _ := msgs[1].(map[string]any)
	blocks, _ := user["content"].([]any)
	blk, _ := blocks[0].(map[string]any)
	if got, _ := blk["text"].(string); got != "Main branch: main" {
		t.Fatalf("text 块应删指纹, got %q", got)
	}
}

// reasoningDefaultFor 四个边界：正常注入 / 模型不在矩阵 / 档位不在该模型矩阵 / 矩阵缺失。
func TestReasoningDefaultFor(t *testing.T) {
	// 正常：hy4-preview + low → "low"
	if got := reasoningDefaultFor(modelMeta["hy4-preview"], "hy4-preview", "low"); got != "low" {
		t.Fatalf("hy4-preview+low 应注入 low, got %q", got)
	}
	// 模型不在矩阵（元数据为 nil）→ 不注入
	if got := reasoningDefaultFor(nil, "no-such-model", "low"); got != "" {
		t.Fatalf("模型不在矩阵不应注入, got %q", got)
	}
	// 配置档位不在该模型矩阵：deepseek-v4-pro 矩阵无 off → 不注入
	if got := reasoningDefaultFor(modelMeta["deepseek-v4-pro"], "deepseek-v4-pro", "off"); got != "" {
		t.Fatalf("deepseek-v4-pro+off 不应注入, got %q", got)
	}
	// 矩阵缺失 → 不注入
	if got := reasoningDefaultFor(map[string]any{"note": "no reasoning key"}, "mystery", "low"); got != "" {
		t.Fatalf("矩阵缺失不应注入, got %q", got)
	}
	// 客户端已带 reasoning_effort 不注入的分支由 handleChat 入参体现：
	// backendBody 已含该键时调用方短路，不进本函数（见 handleChat）。
}

// failoverCfg 读写往返：default_reasoning 带值落盘再读回不丢。
func TestFailoverDefaultReasoningRoundtrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "codebuddy-failover.json")
	s := &Server{failPath: path}
	s.loadFailover()
	s.failMu.Lock()
	s.failDefaultReasoning = "high"
	s.failMu.Unlock()
	s.saveFailover()

	s2 := &Server{failPath: path}
	s2.loadFailover()
	s2.failMu.Lock()
	got := s2.failDefaultReasoning
	s2.failMu.Unlock()
	if got != "high" {
		t.Fatalf("default_reasoning 往返应保持 high, got %q", got)
	}
}

// failoverCfg 缺字段 / 非法值 → 读回落缺省 "low"。
func TestFailoverDefaultReasoningFallbackToLow(t *testing.T) {
	cases := map[string]string{
		"missing": `{"enabled":true,"fallback":"glm-5.3"}`,
		"invalid": `{"enabled":true,"fallback":"glm-5.3","default_reasoning":"ultra"}`,
	}
	for name, content := range cases {
		path := filepath.Join(t.TempDir(), "codebuddy-failover.json")
		if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
		s := &Server{failPath: path}
		s.loadFailover()
		s.failMu.Lock()
		got := s.failDefaultReasoning
		s.failMu.Unlock()
		if got != "low" {
			t.Fatalf("%s: 缺字段/非法 default_reasoning 应回落 low, got %q", name, got)
		}
	}
}
