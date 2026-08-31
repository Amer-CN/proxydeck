// baseline_test.go —— 第 32 轮注水检测修复的单测：
// probeValueMatch 短字符探针 ±1 容差 + 金丝雀错答回灌聚合 + 渠道注册表 strong 标记。
package tuanjie

import "testing"

// TestProbeValueMatchShortTolerance 短字符探针（len≤4）±1 容差：
// 1 个字符位差 match、2 处偏差不 match、len>4 严格相等、数值型不受容差影响。
func TestProbeValueMatchShortTolerance(t *testing.T) {
	cases := []struct {
		name, cur, base string
		want            bool
	}{
		// 验收 3：短探针 ±1 容差三例
		{"2字符差1位", "ab", "ac", true},
		{"4字符差1位", "abcd", "abce", true},
		{"5字符严格", "hello", "world", false},
		// 容差边界
		{"2字符差2位", "ab", "xy", false},
		{"4字符差2位", "abcd", "abxy", false},
		{"等长要求", "ab", "abc", false},
		{"单字符差1位", "a", "b", true},
		{"完全相等", "abcd", "abcd", true},
		// 数值型不动（双方可解析为数字 → 严格相等，不吃短值容差）
		{"数值差1", "42", "43", false},
		{"数值相等", "42", "42", true},
		// 长值（>4）维持逐字符相等
		{"finish不变", "stop", "stop", true},
		{"finish变化", "stop", "length", false},
	}
	for _, c := range cases {
		if got := probeValueMatch(c.name, c.cur, c.base); got != c.want {
			t.Errorf("probeValueMatch(%q, %q, %q) = %v, want %v", c.name, c.cur, c.base, got, c.want)
		}
	}
	// tokenizer 数值容差路径不受本次改动影响
	if !probeValueMatch("tokenizer_en", "50", "42") {
		t.Errorf("tokenizer 平移 8 应在容差内 match")
	}
	if probeValueMatch("tokenizer_en", "99", "42") {
		t.Errorf("tokenizer 平移 57 应 mismatch")
	}
}

// TestApplyCanaryFeedback 金丝雀错答回灌聚合（验收 2）：
// canary 3 错 + 探针全匹配 → red；2 错 → yellow；1 错保持 green；
// 本就 red/yellow 不动（取更严者）；canary 缺失不降级。
func TestApplyCanaryFeedback(t *testing.T) {
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	base := &Baseline{
		Model: "GLM-5.3",
		Probes: []probeResult{
			{Name: "tokenizer_en", Value: "42", Status: "ok"},
			{Name: "tokenizer_cjk", Value: "20", Status: "ok"},
			{Name: "finish_stop", Value: "stop", Status: "ok"},
		},
		Dist: distOf(counts, 50),
	}
	green := func() overallVerdict {
		_, _, v := CompareToBaseline(base, base.Probes, distOf(counts, 50))
		if v.Light != "green" {
			t.Fatalf("前置失败：探针全匹配应判 green，got %+v", v)
		}
		return v
	}
	canaryWrong := func(n int) *WaterProbeResult {
		a := map[string]bool{}
		for i, q := range canaryQuestions {
			a[q.ID] = i >= n // 前 n 题答错
		}
		return &WaterProbeResult{Answers: a}
	}

	// 验收 2：canary 3 错 + 探针全匹配 → red（金丝雀全错）
	v := applyCanaryFeedback(green(), "tuanjie", canaryWrong(3))
	if v.Light != "red" {
		t.Fatalf("金丝雀 3 错应回灌 red, got %+v", v)
	}
	if !containsStr(v.Reason, "金丝雀全错") {
		t.Fatalf("red reason 应含「金丝雀全错」: %q", v.Reason)
	}

	// 2 错 → yellow（reason 补错答 N/3）
	v = applyCanaryFeedback(green(), "tuanjie", canaryWrong(2))
	if v.Light != "yellow" {
		t.Fatalf("金丝雀 2 错应降 yellow, got %+v", v)
	}
	if !containsStr(v.Reason, "金丝雀错答 2/3") {
		t.Fatalf("yellow reason 应含「金丝雀错答 2/3」: %q", v.Reason)
	}

	// 1 错保持 green（只有 ≥2 才降级）
	v = applyCanaryFeedback(green(), "tuanjie", canaryWrong(1))
	if v.Light != "green" {
		t.Fatalf("金丝雀 1 错应保持 green, got %+v", v)
	}

	// 本就 red 不动（取更严者：reason 不被回灌改写）
	changed := []probeResult{
		{Name: "tokenizer_en", Value: "99", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "20", Status: "ok"},
		{Name: "finish_stop", Value: "length", Status: "ok"},
	}
	_, _, red := CompareToBaseline(base, changed, distOf(counts, 50))
	if red.Light != "red" {
		t.Fatalf("前置失败：双探针不匹配应判 red，got %+v", red)
	}
	v = applyCanaryFeedback(red, "tuanjie", canaryWrong(3))
	if v.Light != "red" || v.Reason != red.Reason {
		t.Fatalf("本就 red 不应被回灌改写, got %+v", v)
	}

	// canary 缺失（nil）不降级
	v = applyCanaryFeedback(green(), "tuanjie", nil)
	if v.Light != "green" {
		t.Fatalf("canary nil 应保持 green, got %+v", v)
	}

	// 弱渠道 3 错 → red，reason 基准口径为首测锚定
	v = applyCanaryFeedback(green(), "bai", canaryWrong(3))
	if v.Light != "red" || !containsStr(v.Reason, "首测锚定") {
		t.Fatalf("弱渠道 red reason 应含「首测锚定」: %+v", v)
	}
}

// TestWaterChannelsRegistry 渠道注册表：六渠道 + strong 标记
// （tuanjie/comate/qoder 官方链路，其余首测锚定弱判）。
func TestWaterChannelsRegistry(t *testing.T) {
	ids := map[string]waterChannelDef{}
	for _, c := range waterChannels {
		ids[c.ID] = c
	}
	for _, id := range []string{"tuanjie", "command", "workbuddy", "bai", "comate", "qoder"} {
		if _, ok := ids[id]; !ok {
			t.Fatalf("注册表缺渠道 %s", id)
		}
	}
	if ids["comate"].Port != 8786 || ids["comate"].BaseURL != "http://127.0.0.1:8786" {
		t.Fatalf("comate 端点定义不符: %+v", ids["comate"])
	}
	if ids["qoder"].Port != 8785 || ids["qoder"].BaseURL != "http://127.0.0.1:8785" {
		t.Fatalf("qoder 端点定义不符: %+v", ids["qoder"])
	}
	// 既有四渠道定义不被改动
	if ids["tuanjie"].Port != 8788 || ids["command"].Port != 55990 ||
		ids["workbuddy"].Port != 8787 || ids["bai"].Port != 8891 {
		t.Fatalf("既有四渠道端口定义被改动: %+v", ids)
	}
	// strong 标记：官方链路三渠道 true，其余 false
	for _, c := range waterChannels {
		if got := isStrongChannel(c.ID); got != (c.ID == "tuanjie" || c.ID == "comate" || c.ID == "qoder") {
			t.Fatalf("渠道 %s strong 判定不符: %v", c.ID, got)
		}
	}
}
