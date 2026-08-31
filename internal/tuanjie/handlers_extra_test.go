package tuanjie

import "testing"

// buildWaterReportItems 四个人话检测项的组装（纯函数，无 IO）。
func TestBuildWaterReportItemsAllMatch(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Account: "38261", SampledAt: "2026-08-25 15:55:50",
		Dist: &distResult{Valid: 60, Insufficient: false, Counts: make([]int, distBuckets)}}
	cmps := []probeCompare{
		{Name: "tokenizer_en", Current: "100", Baseline: "100", Match: true, Status: "ok"},
		{Name: "finish_stop", Current: "stop", Baseline: "stop", Match: true, Status: "ok"},
	}
	sim := distSimilarity{Cosine: 0.9, Overall: 0.98, ModeA: 10, ModeB: 10}
	canary := &WaterProbeResult{Answers: map[string]bool{"repeat": true, "math": true, "knowledge": true}}
	items := buildWaterReportItems("tuanjie", canary, cmps, sim, base, false)
	if len(items) != 4 {
		t.Fatalf("应有 4 个检测项，得到 %d: %+v", len(items), items)
	}
	want := []string{"✔", "✔", "✔", "✔"}
	for i, it := range items {
		if it.Result != want[i] {
			t.Errorf("第 %d 项 %s result=%q 应为 %q（detail=%s）", i, it.Name, it.Result, want[i], it.Detail)
		}
	}
}

func TestBuildWaterReportItemsFirstTime(t *testing.T) {
	base := &Baseline{Model: "GLM-X", Account: "38261", SampledAt: "2026-08-26 10:00:00",
		Dist: &distResult{Valid: 60, Insufficient: false, Counts: make([]int, distBuckets)}}
	cmps := []probeCompare{{Name: "tokenizer_en", Current: "100", Baseline: "100", Match: true, Status: "ok"}}
	sim := distSimilarity{Cosine: 0.9, Overall: 0.99, ModeA: 7, ModeB: 7}
	canary := &WaterProbeResult{Answers: map[string]bool{"repeat": true, "math": true, "knowledge": true}}
	items := buildWaterReportItems("tuanjie", canary, cmps, sim, base, true)
	if len(items) != 4 {
		t.Fatalf("应有 4 个检测项，得到 %d", len(items))
	}
	if items[3].Result != "🆕" {
		t.Errorf("首次检测基准状态应为 🆕，得到 %q", items[3].Result)
	}
	/* 第 32 轮：plainVerdictReason 加 strong 维度（基准口径诚实化） */
	if plainVerdictReason("grey", true) != "已自动采集官方链路基准并完成首次检测" {
		t.Errorf("grey 灯文案（strong）不符：%s", plainVerdictReason("grey", true))
	}
	if plainVerdictReason("grey", false) != "已自动采集首测锚定（无官方链路，弱判）并完成首次检测" {
		t.Errorf("grey 灯文案（弱渠道）不符：%s", plainVerdictReason("grey", false))
	}
}

func TestBuildWaterReportItemsMismatchAndNoDist(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Account: "38261", SampledAt: "2026-08-25 15:55:50"} // Dist=nil → 分布不可比
	cmps := []probeCompare{
		{Name: "tokenizer_en", Current: "100", Baseline: "100", Match: true, Status: "ok"},
		{Name: "tokenizer_cjk", Current: "99", Baseline: "120", Match: false, Status: "ok"},
		{Name: "finish_stop", Current: "stop", Baseline: "length", Match: false, Status: "ok"},
	}
	canary := &WaterProbeResult{Answers: map[string]bool{"repeat": true, "math": false, "knowledge": true}}
	items := buildWaterReportItems("tuanjie", canary, cmps, distSimilarity{}, base, false)
	if items[0].Result != "✖" {
		t.Errorf("①身份指纹应 ✖，得到 %q", items[0].Result)
	}
	if items[1].Result != "—" {
		t.Errorf("②权重指纹无分布应 —，得到 %q", items[1].Result)
	}
	if items[2].Result != "✖" {
		t.Errorf("③能力答题有错应 ✖，得到 %q", items[2].Result)
	}
}

func TestBuildWaterReportItemsCanaryNil(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Account: "38261", SampledAt: "2026-08-25 15:55:50"}
	items := buildWaterReportItems("tuanjie", nil, nil, distSimilarity{}, base, false)
	if items[2].Result != "⚠" {
		t.Errorf("金丝雀失败应 ⚠，得到 %q", items[2].Result)
	}
}
