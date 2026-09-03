// water_resolution_test.go —— 别名解析变化检测的单测：
// RecordResolution 记账规则（直连跳过/首次/重复/变化/归一化相等）
// 与 usageScanner 流式 model 字段接线（记入、同流不重复、新流检出变化）。
package tuanjie

import (
	"path/filepath"
	"testing"
)

// TestRecordResolution 表驱动：直连同名与归一化相等的跳过不记；首次 ("", true)；
// 重复 (旧值, false)；变化 (旧值, true)。path 落 t.TempDir()，不碰真实 exe 目录。
func TestRecordResolution(t *testing.T) {
	w := &WaterCheck{path: filepath.Join(t.TempDir(), "tuanjie-water.json")}
	cases := []struct {
		name        string
		requested   string
		returned    string
		wantFrom    string
		wantChanged bool
	}{
		{"直连同名跳过", "glm-5.3-flash", "glm-5.3-flash", "", false},
		{"归一化大小写相等跳过", "GLM-5.3-FLASH", "glm-5.3-flash", "", false},
		{"归一化前缀相等跳过", "z-ai/x", "x", "", false},
		{"空请求跳过", "", "glm-5.3-flash", "", false},
		{"空返回跳过", "codely-core", "", "", false},
		{"首次记录", "codely-core", "glm-5-fp8-128k", "", true},
		{"重复不变化", "codely-core", "glm-5-fp8-128k", "glm-5-fp8-128k", false},
		{"变化检出", "codely-core", "deepseek-v4-flash", "glm-5-fp8-128k", true},
	}
	for _, c := range cases {
		from, changed := w.RecordResolution(c.requested, c.returned)
		if from != c.wantFrom || changed != c.wantChanged {
			t.Errorf("%s: RecordResolution(%q, %q) = (%q, %v), want (%q, %v)",
				c.name, c.requested, c.returned, from, changed, c.wantFrom, c.wantChanged)
		}
	}
	if got := w.Resolved["codely-core"]; got != "deepseek-v4-flash" {
		t.Errorf("Resolved[codely-core] = %q, want %q", got, "deepseek-v4-flash")
	}
	if len(w.Resolved) != 1 {
		t.Errorf("跳过用例不应记账，Resolved 应只余 1 条，实际 %d: %v", len(w.Resolved), w.Resolved)
	}
}

// TestUsageScannerModelDetect 流式接线：feed 含 model 的 data: 行 → Resolved
// 记入；同流后续块不再重复记录（resSeen）；新流换 model → 检出变化。
// 测试数据不带 usage（feed 只在 usage 非空时才调 addStat），最小构造即可。
func TestUsageScannerModelDetect(t *testing.T) {
	w := &WaterCheck{path: filepath.Join(t.TempDir(), "tuanjie-water.json")}
	srv := &Server{water: w}

	u := newUsageScanner("codely-core", srv)
	u.feed([]byte("data: {\"model\":\"glm-5-fp8-128k\"}\n"))
	if got := w.Resolved["codely-core"]; got != "glm-5-fp8-128k" {
		t.Fatalf("feed 后 Resolved[codely-core] = %q, want %q", got, "glm-5-fp8-128k")
	}

	// 同流再喂（SSE 每个数据块都带 model）不应重复记录
	u.feed([]byte("data: {\"model\":\"deepseek-v4-flash\"}\n"))
	if got := w.Resolved["codely-core"]; got != "glm-5-fp8-128k" {
		t.Fatalf("同流后续块不应重复记录, Resolved[codely-core] = %q, want %q", got, "glm-5-fp8-128k")
	}

	// 新流（新 scanner）换 model → 检出变化并更新
	u2 := newUsageScanner("codely-core", srv)
	u2.feed([]byte("data: {\"model\":\"deepseek-v4-flash\"}\n"))
	if got := w.Resolved["codely-core"]; got != "deepseek-v4-flash" {
		t.Fatalf("新流换 model 后 Resolved[codely-core] = %q, want %q", got, "deepseek-v4-flash")
	}
}
