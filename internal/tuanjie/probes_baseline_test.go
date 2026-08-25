// probes_baseline_test.go —— 注水检测三层升级的单测：
// 探针归一化/脱敏、分布相似度公式（余弦/JS/众数）、基准存取与比对。
package tuanjie

import (
	"os"
	"path/filepath"
	"testing"
)

// TestScrubErrorText 错误原文脱敏：uuid 与 13 位以上数字被抹掉，300 字符截断。
func TestScrubErrorText(t *testing.T) {
	in := `{"message":"request 550e8400-e29b-41d4-a716-446655440000 failed at 1724499999123 (id=42)","code":1}`
	got := scrubErrorText(in)
	if got == in {
		t.Fatalf("脱敏未生效: %q", got)
	}
	if !containsStr(got, "<uuid>") || !containsStr(got, "<num>") {
		t.Fatalf("uuid/长数字未被替换: %q", got)
	}
	if containsStr(got, "550e8400") || containsStr(got, "1724499999123") {
		t.Fatalf("易变成分残留: %q", got)
	}
	if containsStr(got, "42") == false {
		t.Fatalf("短数字不应被误伤: %q", got)
	}
	// 截断
	long := ""
	for i := 0; i < 60; i++ {
		long += "abcdefghij"
	}
	if got := scrubErrorText(long); len(got) != 300 {
		t.Fatalf("应截断到 300 字符, got %d", len(got))
	}
}

func containsStr(s, sub string) bool {
	return len(s) >= len(sub) && (func() bool {
		for i := 0; i+len(sub) <= len(s); i++ {
			if s[i:i+len(sub)] == sub {
				return true
			}
		}
		return false
	})()
}

// TestParseDistAnswer 分布采样回答解析：数字/带标点/带前缀/越界/非数字。
func TestParseDistAnswer(t *testing.T) {
	cases := map[string]int{
		"42":        42,
		" 42 ":      42,
		"42。":       42,
		"355":       355,
		"1":         1,
		"答案是 77": 77,
		"356":       0, // 越界
		"0":         0,
		"abc":       0,
		"":          0,
		"-5":        0,
		"1,234":     1, // 取首个数字段
	}
	for in, want := range cases {
		if got := parseDistAnswer(in); got != want {
			t.Errorf("parseDistAnswer(%q) = %d, want %d", in, got, want)
		}
	}
}

// makeCounts 构造 1..355 的 counts（指定的桶 +1）。
func makeCounts(vals ...int) []int {
	c := make([]int, distBuckets+1)
	for _, v := range vals {
		if v >= 1 && v <= distBuckets {
			c[v]++
		}
	}
	return c
}

// distOf 从 counts 构造 distResult（Valid=总数）。
func distOf(counts []int, valid int) *distResult {
	d := &distResult{Counts: counts[1:], Valid: valid}
	d.Stats = distStatsOf(counts)
	d.Insufficient = valid < 40
	return d
}

// TestCompareDistIdentical 同分布 → 余弦 1、JS 0、overall 1。
func TestCompareDistIdentical(t *testing.T) {
	a := makeCounts()
	for i := 1; i <= 60; i++ {
		a[i] = 1 // 60 个不同数字各 1 次
	}
	b := makeCounts()
	copy(b, a)
	sim := CompareDist(a[1:], b[1:], 60, 60)
	if sim.Cosine != 1 || sim.JSDiv != 0 || sim.DistribScore != 1 {
		t.Fatalf("同分布应满分: %+v", sim)
	}
	if sim.ModeA != sim.ModeB || sim.ModeScore != 1 {
		t.Fatalf("众数应相等且 modeScore=1: %+v", sim)
	}
	if sim.Overall != 1 {
		t.Fatalf("overall 应为 1: %+v", sim)
	}
}

// TestCompareDistShifted 众数偏移 50+ → modeScore 归 0；整体显著低。
func TestCompareDistShifted(t *testing.T) {
	a := makeCounts()
	for i := 0; i < 60; i++ {
		a[10]++ // 众数 10
	}
	b := makeCounts()
	for i := 0; i < 60; i++ {
		b[61]++ // 众数 61（偏移 51）
	}
	sim := CompareDist(a[1:], b[1:], 60, 60)
	if sim.ModeScore != 0 {
		t.Fatalf("众数偏移 ≥50 应得 0 分: %+v", sim)
	}
	if sim.Cosine != 0 || sim.DistribScore != 0 {
		t.Fatalf("完全不相交的分布余弦应为 0: %+v", sim)
	}
	if sim.Overall >= 0.9 {
		t.Fatalf("显著不同分布 overall 不应 ≥90%%: %+v", sim)
	}
}

// TestCompareDistInsufficient 样本不足（<40）→ 全 0，由调用方按 insufficient 处理。
func TestCompareDistInsufficient(t *testing.T) {
	a := makeCounts(1, 2, 3)
	b := makeCounts(1, 2, 3)
	sim := CompareDist(a[1:], b[1:], 3, 3)
	if sim.Cosine != 0 || sim.Overall != 0 {
		t.Fatalf("样本不足应返回零值相似度: %+v", sim)
	}
}

// TestDistVerdict 档位：≥96 一致 / ≥90 轻微 / <90 显著。
func TestDistVerdict(t *testing.T) {
	if _, v := distVerdict(distSimilarity{Overall: 0.97}); v != "一致" {
		t.Fatalf("97%% 应判一致: %s", v)
	}
	if _, v := distVerdict(distSimilarity{Overall: 0.95}); v != "轻微偏差" {
		t.Fatalf("95%% 应判轻微偏差: %s", v)
	}
	if _, v := distVerdict(distSimilarity{Overall: 0.85}); v != "显著偏差" {
		t.Fatalf("85%% 应判显著偏差: %s", v)
	}
	if l := distVerdictLabel("一致"); l != "green" {
		t.Fatalf("一致应为 green: %s", l)
	}
	if l := distVerdictLabel("轻微偏差"); l != "yellow" {
		t.Fatalf("轻微偏差应为 yellow: %s", l)
	}
	if l := distVerdictLabel("显著偏差"); l != "red" {
		t.Fatalf("显著偏差应为 red: %s", l)
	}
}

// TestBaselineStoreSaveLoad 基准存取往返（model → 基准）。
func TestBaselineStoreSaveLoad(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	bs := LoadBaselines()
	if bs.Get("GLM-5.3") != nil {
		t.Fatal("空库不应有基准")
	}
	// 手工构造一份基准并借 save 落盘
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	bl := &Baseline{
		Model: "GLM-5.3",
		Probes: []probeResult{
			{Name: "tokenizer_en", Value: "42", Status: "ok"},
			{Name: "finish_stop", Value: "stop", Status: "ok"},
		},
		Dist:        distOf(counts, 50),
		SampledAt:   "2026-08-24 00:00:00",
		Account:     "u1",
		SampleCount: 50,
	}
	bs.Baselines["GLM-5.3"] = bl
	bs.save()

	// 重新加载：往返一致
	bs2 := LoadBaselines()
	got := bs2.Get("GLM-5.3")
	if got == nil {
		t.Fatal("落盘后重读应能拿到基准")
	}
	if got.Model != "GLM-5.3" || got.Account != "u1" || got.SampleCount != 50 {
		t.Fatalf("基准字段往返不一致: %+v", got)
	}
	if len(got.Probes) != 2 || got.Probes[0].Value != "42" {
		t.Fatalf("探针值往返不一致: %+v", got.Probes)
	}
	if got.Dist == nil || got.Dist.Valid != 50 || len(got.Dist.Counts) != distBuckets {
		t.Fatalf("分布往返不一致: %+v", got.Dist)
	}
	if _, err := os.Stat(filepath.Join(dir, "tuanjie-baselines.json")); err != nil {
		t.Fatalf("基准文件应存在: %v", err)
	}
}

// TestCompareToBaselineVerdicts 综合判定：无基准灰灯 / 全匹配绿 / 探针不符红。
func TestCompareToBaselineVerdicts(t *testing.T) {
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

	// 无基准 → 灰灯引导
	_, _, v := CompareToBaseline(nil, base.Probes, distOf(counts, 50))
	if v.Light != "grey" {
		t.Fatalf("无基准应为灰灯: %+v", v)
	}

	// 全匹配 + 同分布 → 绿灯
	_, _, v = CompareToBaseline(base, base.Probes, distOf(counts, 50))
	if v.Light != "green" || v.Label != "一致" {
		t.Fatalf("全匹配应绿灯一致: %+v", v)
	}

	// 两个探针值变了（错误文本/finish 用相等性）→ 红灯（注水嫌疑）
	changed := []probeResult{
		{Name: "tokenizer_en", Value: "99", Status: "ok"}, // 差值 57 > 容差 8
		{Name: "tokenizer_cjk", Value: "20", Status: "ok"},
		{Name: "finish_stop", Value: "length", Status: "ok"}, // 不匹配
	}
	_, _, v = CompareToBaseline(base, changed, distOf(counts, 50))
	if v.Light != "red" {
		t.Fatalf("两个探针不匹配应红灯: %+v", v)
	}

	// tokenizer 整批平移 8（宿主隐藏模板漂移）→ 容差内不算 mismatch，仍绿灯
	shiftedProbes := []probeResult{
		{Name: "tokenizer_en", Value: "50", Status: "ok"}, // 42+8
		{Name: "tokenizer_cjk", Value: "28", Status: "ok"}, // 20+8
		{Name: "finish_stop", Value: "stop", Status: "ok"},
	}
	_, _, v = CompareToBaseline(base, shiftedProbes, distOf(counts, 50))
	if v.Light != "green" {
		t.Fatalf("tokenizer 平移 8 应在容差内判绿灯: %+v", v)
	}

	// 探针全匹配但分布不同（众数偏移 51）→ 黄或红（不判绿即可）
	shifted := makeCounts()
	for i := 0; i < 50; i++ {
		shifted[200]++
	}
	_, _, v = CompareToBaseline(base, base.Probes, distOf(shifted, 50))
	if v.Light == "green" {
		t.Fatalf("分布显著不同不应绿灯: %+v", v)
	}

	// 样本不足（<40 有效）→ 只看探针，全匹配仍绿但 reason 注明分布不可比
	small := makeCounts(1, 2, 3)
	_, _, v = CompareToBaseline(base, base.Probes, distOf(small, 3))
	if v.Light != "green" {
		t.Fatalf("样本不足时探针全匹配应仍绿灯: %+v", v)
	}
	if !containsStr(v.Reason, "样本不足") {
		t.Fatalf("reason 应注明分布不可比: %q", v.Reason)
	}
}

// TestDistStatsOf 统计公式：mean/median/stdDev/mode 手工验算。
func TestDistStatsOf(t *testing.T) {
	// 样本 [1,2,2,6]：mean=2.75 median=2 mode=2
	c := makeCounts(1, 2, 2, 6)
	st := distStatsOf(c)
	if st.Mean != 2.75 || st.Median != 2 || st.Mode != 2 || st.ModeKw != 2 {
		t.Fatalf("统计不对: %+v", st)
	}
	// 总体方差 = ((1-2.75)²+(2-2.75)²+(2-2.75)²+(6-2.75)²)/4 = (3.0625+0.5625+0.5625+10.5625)/4 = 3.6875 → σ≈1.9203
	if st.StdDev < 1.92 || st.StdDev > 1.93 {
		t.Fatalf("stdDev 应约 1.92: %+v", st)
	}
}
