// baseline_test.go —— 注水检测修复的单测：
// probeValueMatch 短字符探针 ±1 容差 + 金丝雀错答回灌聚合 + 渠道注册表 strong
// 标记 + 第 33 轮基准归属（按模型键迁移/质量门槛/跨渠道比对）。
package tuanjie

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

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
	// tokenizer 数值容差：±1（2026-09-12 收紧；旧 ±8 是「"a" 底座取错返回值
	// 导致整批伪漂移」年代的标定，根因已修，见 tokenizerDriftTolerance 注释）
	if !probeValueMatch("tokenizer_en", "43", "42") {
		t.Errorf("tokenizer 平移 1 应在容差内 match")
	}
	if probeValueMatch("tokenizer_en", "44", "42") {
		t.Errorf("tokenizer 平移 2 应 mismatch")
	}
	if probeValueMatch("tokenizer_en", "50", "42") {
		t.Errorf("tokenizer 平移 8 应 mismatch（容差已收紧到 ±1）")
	}
	if probeValueMatch("tokenizer_en", "99", "42") {
		t.Errorf("tokenizer 平移 57 应 mismatch")
	}
}

// TestTokenizerDriftToleranceTightened tokenizer 容差收紧到 ±1 的验收（2026-09-12）：
// 归一化底座钉 prompt_tokens 后实测 4 组跨渠道/跨运行对照 Δ=0，旧 ±8 会把两个
// 不同模型的指纹判成「全部匹配」假绿。用真实测得的两组归一化指纹验收：
//   - 当前 KIMI-K3（26/29/58/55）vs 基准 GLM-5.3-FLASH（26/35/57/63）→
//     cjk Δ6、mixed Δ8 超容差必须判偏离（≥2 项 → red），不得再「全部匹配」；
//   - 当前=基准完全相同（26/29/58/55）→ 全部匹配、green；
//   - 单项 ±1 → 仍算匹配（证明留了余量）；单项 ±2 → 判偏离。
func TestTokenizerDriftToleranceTightened(t *testing.T) {
	// 真实测得的归一化指纹（勿改数值：验收依据就是这两组实测值）
	glmFlashProbes := []probeResult{
		{Name: "tokenizer_en", Value: "26", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "35", Status: "ok"},
		{Name: "tokenizer_code", Value: "57", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "63", Status: "ok"},
	}
	kimiK3Probes := []probeResult{
		{Name: "tokenizer_en", Value: "26", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "29", Status: "ok"},
		{Name: "tokenizer_code", Value: "58", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "55", Status: "ok"},
	}

	// ① 换皮反例：渠道挂 GLM-5.3-FLASH 的名实际返回 KIMI-K3 → 四项差值
	// 0/6/1/8，cjk/mixed 超出 ±1 必须判偏离（旧 ±8 下这是「全部匹配」假绿）
	base := &Baseline{Model: "GLM-5.3-FLASH", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: glmFlashProbes}
	cmps, _, v := CompareToBaseline(base, kimiK3Probes, nil, "bai")
	if v.Light != "red" {
		t.Fatalf("KIMI-K3 指纹冒充 GLM-5.3-FLASH 应判 red（cjk Δ6、mixed Δ8 超容差）: %+v", v)
	}
	if containsStr(v.Reason, "全部匹配") {
		t.Fatalf("不得再产出「全部匹配」假绿: %q", v.Reason)
	}
	for _, c := range cmps {
		switch c.Name {
		case "tokenizer_cjk", "tokenizer_mixed": // Δ6/Δ8 超容差
			if !c.Comparable || c.Match {
				t.Fatalf("%s 应判偏离（超 ±1 容差）: %+v", c.Name, c)
			}
		case "tokenizer_en", "tokenizer_code": // Δ0/Δ1 容差内
			if !c.Comparable || !c.Match {
				t.Fatalf("%s 应判匹配（容差内）: %+v", c.Name, c)
			}
		}
	}

	// ② 同模型自比：当前=基准（26/29/58/55）完全相同 → 全部匹配 green
	kimiBase := &Baseline{Model: "KIMI-K3", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: kimiK3Probes}
	_, _, v = CompareToBaseline(kimiBase, kimiK3Probes, nil, "tuanjie")
	if v.Light != "green" {
		t.Fatalf("当前=基准完全相同时应判 green: %+v", v)
	}
	if !containsStr(v.Reason, "全部匹配") {
		t.Fatalf("green reason 应为「全部匹配」口径: %q", v.Reason)
	}

	// ③ ±1 余量边界：单项 ±1 仍匹配；单项 ±2 判偏离
	if !probeValueMatch("tokenizer_cjk", "30", "29") {
		t.Errorf("tokenizer 单项 +1 应仍算匹配（±1 余量）")
	}
	if !probeValueMatch("tokenizer_cjk", "28", "29") {
		t.Errorf("tokenizer 单项 -1 应仍算匹配（±1 余量）")
	}
	if probeValueMatch("tokenizer_cjk", "31", "29") {
		t.Errorf("tokenizer 单项 +2 应判偏离")
	}
	if probeValueMatch("tokenizer_cjk", "27", "29") {
		t.Errorf("tokenizer 单项 -2 应判偏离")
	}
}

// TestTokenizerBatchShiftShape 漂移形状判定（纯函数）：真·模板平移 = 偏离项
// 带符号差值整批同量；换模型 = 偏离项 delta 不一致 / 正负混杂；delta 非数值
// 按形状不可判定处理——后三者一律不得判「整批同量平移」（保守取严，不降级）。
func TestTokenizerBatchShiftShape(t *testing.T) {
	// tok 构造一条可比 tokenizer 比对项（Match 按现行容差规则真实产出）
	tok := func(name, cur, base string) probeCompare {
		return probeCompare{Name: name, Current: cur, Baseline: base,
			Comparable: true, Match: probeValueMatch(name, cur, base), Status: "ok"}
	}
	// 整批同量平移：基准 26/29/58/55 → 当前 34/37/66/63，四项 delta 均 +8
	cmps := []probeCompare{
		tok("tokenizer_en", "34", "26"), tok("tokenizer_cjk", "37", "29"),
		tok("tokenizer_code", "66", "58"), tok("tokenizer_mixed", "63", "55"),
	}
	if ok, shift := tokenizerBatchShift(cmps); !ok || shift != 8 {
		t.Fatalf("四项 delta 均 +8 应判整批同量平移 +8: ok=%v shift=%d", ok, shift)
	}
	// 部分项落在 ±1 容差内不偏离：形状只看偏离项的 delta（+8/+8 一致）→ 仍整批同量
	cmps = []probeCompare{
		tok("tokenizer_en", "34", "26"), tok("tokenizer_cjk", "37", "29"),
		tok("tokenizer_code", "58", "57"), tok("tokenizer_mixed", "56", "55"),
	}
	if ok, shift := tokenizerBatchShift(cmps); !ok || shift != 8 {
		t.Fatalf("偏离项 delta 一致（+8/+8）、容差内项不进形状集合: ok=%v shift=%d", ok, shift)
	}
	// 换模型（真实 KIMI-K3 26/29/58/55 冒充 GLM-5.3-FLASH 26/35/57/63）：
	// 偏离项 delta −6/−8，散布 2 → 不得判平移
	cmps = []probeCompare{
		tok("tokenizer_en", "26", "26"), tok("tokenizer_cjk", "29", "35"),
		tok("tokenizer_code", "58", "57"), tok("tokenizer_mixed", "55", "63"),
	}
	if ok, _ := tokenizerBatchShift(cmps); ok {
		t.Fatal("偏离项 delta −6/−8（散布 2）应判换模型形状，不得降级")
	}
	// 正负混杂 → 不算平移
	cmps = []probeCompare{tok("tokenizer_en", "34", "26"), tok("tokenizer_cjk", "23", "29")}
	if ok, _ := tokenizerBatchShift(cmps); ok {
		t.Fatal("偏离项 delta +8/−6 正负混杂应判换模型形状，不得降级")
	}
	// 逐项 ±1 噪声内（散布恰为 1）仍算同量平移
	cmps = []probeCompare{tok("tokenizer_en", "34", "26"), tok("tokenizer_cjk", "38", "29")}
	if ok, _ := tokenizerBatchShift(cmps); !ok {
		t.Fatal("偏离项 delta +8/+9（散布 1，逐项 ±1 噪声）应仍判整批同量平移")
	}
	// delta 非数值（无法解析为整数）→ 形状不可判定，保守不降级
	cmps = []probeCompare{
		{Name: "tokenizer_en", Current: "34", Baseline: "26", Comparable: true, Status: "ok"},
		{Name: "tokenizer_cjk", Current: "unstable", Baseline: "29", Comparable: true, Status: "ok"},
	}
	if ok, _ := tokenizerBatchShift(cmps); ok {
		t.Fatal("delta 非数值应按形状不可判定处理，保守不降级")
	}
	// 无偏离项 → 无形状可判
	if ok, _ := tokenizerBatchShift(nil); ok {
		t.Fatal("无偏离项不应判整批同量平移")
	}
}

// TestApplyCanaryFeedback 金丝雀错答回灌聚合：
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
		_, _, v := CompareToBaseline(base, base.Probes, distOf(counts, 50), "tuanjie")
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

	// canary 3 错 + 探针全匹配 → red（金丝雀全错）
	v := applyCanaryFeedback(green(), canaryWrong(3), nil)
	if v.Light != "red" {
		t.Fatalf("金丝雀 3 错应回灌 red, got %+v", v)
	}
	if !containsStr(v.Reason, "金丝雀全错") {
		t.Fatalf("red reason 应含「金丝雀全错」: %q", v.Reason)
	}

	// 2 错 → yellow（reason 补错答 N/3）
	v = applyCanaryFeedback(green(), canaryWrong(2), nil)
	if v.Light != "yellow" {
		t.Fatalf("金丝雀 2 错应降 yellow, got %+v", v)
	}
	if !containsStr(v.Reason, "金丝雀错答 2/3") {
		t.Fatalf("yellow reason 应含「金丝雀错答 2/3」: %q", v.Reason)
	}

	// 1 错保持 green（只有 ≥2 才降级）
	v = applyCanaryFeedback(green(), canaryWrong(1), nil)
	if v.Light != "green" {
		t.Fatalf("金丝雀 1 错应保持 green, got %+v", v)
	}

	// 本就 red 不动（取更严者：reason 不被回灌改写）
	changed := []probeResult{
		{Name: "tokenizer_en", Value: "99", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "20", Status: "ok"},
		{Name: "finish_stop", Value: "length", Status: "ok"},
	}
	_, _, red := CompareToBaseline(base, changed, distOf(counts, 50), "tuanjie")
	if red.Light != "red" {
		t.Fatalf("前置失败：双探针不匹配应判 red，got %+v", red)
	}
	v = applyCanaryFeedback(red, canaryWrong(3), nil)
	if v.Light != "red" || v.Reason != red.Reason {
		t.Fatalf("本就 red 不应被回灌改写, got %+v", v)
	}

	// canary 缺失（nil）不降级
	v = applyCanaryFeedback(green(), nil, nil)
	if v.Light != "green" {
		t.Fatalf("canary nil 应保持 green, got %+v", v)
	}

	// 第 33 轮：基准口径统一「官方基准」——非官方渠道回灌 red 也不再提「首测锚定」
	v = applyCanaryFeedback(green(), canaryWrong(3), nil)
	if v.Light != "red" || !containsStr(v.Reason, "官方基准") || containsStr(v.Reason, "首测锚定") {
		t.Fatalf("red reason 应为官方基准口径且不再含「首测锚定」: %+v", v)
	}
}

// TestApplyCanaryFeedbackKeepsCoverageSuffix 必改 2（金丝雀错答回灌）：
// reason 覆写必须保留 coverage 限定语——探针比对里有跨渠道跳过项时，
// 降级后的 reason 仍带「N 项跨渠道不可比，已跳过」，一句话结论不得声称
// 比实际测到的更多（cmps 走 CompareToBaseline 真实产出，非手搓字段）。
func TestApplyCanaryFeedbackKeepsCoverageSuffix(t *testing.T) {
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	base := &Baseline{
		Model: "GLM-5.3-FLASH", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: goodBaselineProbes(), Dist: distOf(counts, 50),
	}
	// bai 侧探针：tokenizer/finish 与官方基准同值，报错原文是 B 网关措辞 →
	// 跨渠道跳过 2 项（probeCoverageSuffix 应报「已跳过」）
	cur := []probeResult{
		{Name: "tokenizer_en", Value: "26", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "35", Status: "ok"},
		{Name: "tokenizer_code", Value: "57", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "58", Status: "ok"},
		{Name: "error_temp2", Value: `{"message":"gateway B: temperature 参数非法"}`, Status: "ok"},
		{Name: "error_maxtok", Value: `{"message":"gateway B: max_tokens 参数非法"}`, Status: "ok"},
		{Name: "finish_stop", Value: "stop", Status: "ok"},
		{Name: "finish_length", Value: "length", Status: "ok"},
	}
	cmps, _, v := CompareToBaseline(base, cur, distOf(counts, 50), "bai")
	if v.Light != "green" {
		t.Fatalf("前置失败：可比项全匹配应判 green，got %+v", v)
	}
	skipN := 0
	for _, c := range cmps {
		if c.Status == "skip" {
			skipN++
		}
	}
	if skipN != 2 {
		t.Fatalf("前置失败：报错原文探针应 2 项跨渠道跳过: %+v", cmps)
	}

	canaryWrong := func(n int) *WaterProbeResult {
		a := map[string]bool{}
		for i, q := range canaryQuestions {
			a[q.ID] = i >= n
		}
		return &WaterProbeResult{Answers: a}
	}
	// 错答 2 → yellow：reason 既带金丝雀错答说明，也保留「已跳过」限定语
	out := applyCanaryFeedback(v, canaryWrong(2), cmps)
	if out.Light != "yellow" {
		t.Fatalf("金丝雀 2 错应降 yellow, got %+v", out)
	}
	if !containsStr(out.Reason, "金丝雀错答 2/3") || !containsStr(out.Reason, "已跳过") {
		t.Fatalf("yellow reason 应保留金丝雀错答说明与跳过限定语: %q", out.Reason)
	}
	// 错答 3 → red：同样保留
	out = applyCanaryFeedback(v, canaryWrong(3), cmps)
	if out.Light != "red" {
		t.Fatalf("金丝雀 3 错应回灌 red, got %+v", out)
	}
	if !containsStr(out.Reason, "金丝雀全错") || !containsStr(out.Reason, "已跳过") {
		t.Fatalf("red reason 应保留金丝雀全错与跳过限定语: %q", out.Reason)
	}
}

// goodBaselineProbes 构造 8 项全 ok 的探针集（质量门槛合格形态）。
func goodBaselineProbes() []probeResult {
	return []probeResult{
		{Name: "tokenizer_en", Value: "26", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "35", Status: "ok"},
		{Name: "tokenizer_code", Value: "57", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "58", Status: "ok"},
		{Name: "error_temp2", Value: `{"message":"gateway A: temperature 参数非法"}`, Status: "ok"},
		{Name: "error_maxtok", Value: `{"message":"gateway A: max_tokens 参数非法"}`, Status: "ok"},
		{Name: "finish_stop", Value: "stop", Status: "ok"},
		{Name: "finish_length", Value: "length", Status: "ok"},
	}
}

// pollutedBaselineProbes 构造真实污染形态的探针集：单字符 "a" 报 ~88 →
// 归一化值 = 文本值 − 88 = -57/-48/-26/-20（tuanjie-baselines.json 里
// tuanjie|GLM-5.3-FLASH 与 tuanjie|codely-core 的存量实况）。
// 探针 ok=7/8、dist 有效 43——能过旧门槛（只看 ok 数与 dist.Valid），
// 新物理判据下必须判不合格。
func pollutedBaselineProbes() []probeResult {
	return []probeResult{
		{Name: "tokenizer_en", Value: "-57", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "-48", Status: "ok"},
		{Name: "tokenizer_code", Value: "-26", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "-20", Status: "ok"},
		{Name: "error_temp2", Value: `{"message":"gateway A: temperature 参数非法"}`, Status: "ok"},
		{Name: "error_maxtok", Value: `{"message":"gateway A: max_tokens 参数非法"}`, Status: "ok"},
		{Name: "finish_stop", Value: "stop", Status: "ok"},
		{Name: "finish_length", Value: "error", Status: "error"},
	}
}

// TestBaselineStoreMigrateLegacyKeys 旧键迁移（第 33 轮基准归属 + 第 39 轮多锚键）：
// v2 内容版本的「渠道|模型」旧键 → 模型|渠道 多锚键（升版本 3）；仅官方渠道 +
// 过质量门槛的保留，非官方渠道自采基准与劣质采集一律丢弃；重读稳定不再改键。
func TestBaselineStoreMigrateLegacyKeys(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

		goodDist := func() *distResult {
			c := makeCounts()
			for i := 1; i <= 50; i++ {
				c[i]++
			}
			return distOf(c, 50)
		}
		file := map[string]*Baseline{
			// 官方渠道 + 合格 + 旧内容版本（v2，键为 v1「渠道|模型」形状）：迁移
			// （body 缺 channel，按键前缀补 tuanjie；重键为 模型|渠道、升版本 3）
			"tuanjie|GLM-5.3": {Model: "GLM-5.3", SchemaVersion: baselineMigratableVersion, Probes: goodBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-01 10:00:00"},
			// 官方渠道 + 劣质（探针 2/8、无分布）：丢弃
			"tuanjie|KIMI-K3": {Model: "KIMI-K3", Channel: "tuanjie", SchemaVersion: baselineMigratableVersion,
				Probes: []probeResult{{Name: "tokenizer_en", Value: "1", Status: "ok"}, {Name: "finish_stop", Value: "stop", Status: "ok"}},
				SampledAt: "2026-09-01 11:00:00"},
			// 非官方渠道自采（合格形态）：丢弃（不再用自采基准判定）
			"bai|glm-5.3-flash": {Model: "glm-5.3-flash", Channel: "bai", SchemaVersion: baselineMigratableVersion, Probes: goodBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-02 10:00:00"},
			"workbuddy|kimi-k3": {Model: "kimi-k3", Channel: "workbuddy", SchemaVersion: baselineMigratableVersion, Probes: goodBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-02 11:00:00"},
			// 官方渠道 comate + 合格：迁移保留
			"comate|Foo-X": {Model: "Foo-X", Channel: "comate", SchemaVersion: baselineMigratableVersion, Probes: goodBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-03 10:00:00"},
			// 官方渠道 + tokenizer 污染（"a" 报 ~88 → 归一化值 -57/-48/-26/-20）：
			// 探针 ok=7、dist 有效 43 能过旧门槛，但归一化值为负物理不可能 → 丢弃
			"tuanjie|GLM-5.3-FLASH": {Model: "GLM-5.3-FLASH", Channel: "tuanjie", SchemaVersion: baselineMigratableVersion, Probes: pollutedBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-04 10:00:00"},
		}
	b, _ := json.Marshal(map[string]any{"baselines": file})
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-baselines.json"), b, 0o600); err != nil {
		t.Fatal(err)
	}

	bs := LoadBaselines()
	if got := len(bs.Baselines); got != 2 {
		t.Fatalf("迁移后应只剩 2 条（tuanjie+comate），got %d: %v", got, bs.Baselines)
	}
	g := bs.Get("GLM-5.3") // 大小写归一命中
	if g == nil {
		t.Fatal("官方渠道合格基准应迁移到模型键 glm-5.3")
	}
	if !g.Official || g.Channel != "tuanjie" {
		t.Fatalf("迁移条目应补记来源渠道与官方标记: %+v", g)
	}
	if bs.Get("glm-5.3-flash") != nil {
		t.Fatal("bai 自采基准应被丢弃（非官方渠道不再有效）")
	}
	if bs.Get("glm-5.3-flash") != nil || bs.Get("GLM-5.3-FLASH") != nil {
		t.Fatal("tokenizer 值为负的官方污染锚点应被丢弃（归一化值物理下限 ≥1）")
	}
	if bs.Get("kimi-k3") != nil {
		t.Fatal("workbuddy 自采与 tuanjie 劣质条目应都被丢弃")
	}
	c := bs.Get("foo-x")
	if c == nil || c.Channel != "comate" || !c.Official {
		t.Fatalf("comate 官方合格基准应保留: %+v", c)
	}
	// 落盘重读稳定：第二次加载不再有结构变化（键即 模型|渠道）
	bs2 := LoadBaselines()
	if len(bs2.Baselines) != 2 || bs2.Get("GLM-5.3") == nil || bs2.Get("foo-x") == nil {
		t.Fatalf("二次加载应稳定: %v", bs2.Baselines)
	}
}

// probeFakeUpstream 构造探针假上游（验收 6 用）：
//   - failProbe 名称命中列表的探针请求 → 429 空响应体（重试注入零退避后仍 429）
//   - failProbe["aHigh"] → 单字符 "a" 报 ~88（真实污染形态）
//   - failProbe["auth401"] → 错误探针（error_temp2/error_maxtok）回 401 鉴权
//     错误体（渠道 key 失效形态）
//   - distContent 为 distPrompt 的回答（"" 表示 length+空正文→未作答）
//   - 其余探针按固定指纹回放（8 项全 ok 的形态）
func probeFakeUpstream(t *testing.T, failProbe map[string]bool, distContent string) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Messages  []struct {
				Content string `json:"content"`
			} `json:"messages"`
			Temperature float64 `json:"temperature"`
			MaxTokens    int     `json:"max_tokens"`
		}
		if json.NewDecoder(r.Body).Decode(&body) != nil || len(body.Messages) == 0 {
			w.WriteHeader(400)
			return
		}
		msg := body.Messages[0].Content
		isProbe := func() {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(429) // 空体 429：非 200 且无 JSON 错误体
		}
		chat := func(content, finish string, promptTok int) {
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(map[string]any{
				"usage":  map[string]any{"prompt_tokens": promptTok, "completion_tokens": 3},
				"choices": []map[string]any{{"finish_reason": finish,
					"message": map[string]any{"content": content}}},
			})
		}
		errJSON := func(msg string) {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(400)
			_ = json.NewEncoder(w).Encode(map[string]any{
				"error": map[string]any{"message": msg, "type": "invalid_request_error", "code": "400"},
			})
		}
		auth401 := func() {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(401) // key 失效形态：401 + JSON 鉴权错误体
			_ = json.NewEncoder(w).Encode(map[string]any{
				"error": map[string]any{"message": "无效的令牌或密钥", "type": "invalid_request_error", "code": "invalid_api_key"},
			})
		}
		switch {
		case msg == "a":
			if failProbe["aHigh"] {
				// 真实污染形态：单字符 "a" 报 ~88 tokens（正常 13），且 3 次
				// 采样全部偏高——中位数离群判据对此结构性失效
				chat("a", "stop", 88)
				return
			}
			if failProbe["a"] {
				isProbe()
				return
			}
			chat("a", "stop", 13)
		case strings.Contains(msg, "philosophers"): // tokenizer_en
			if failProbe["tokenizer"] {
				isProbe()
				return
			}
			chat("x", "stop", 39)
		case strings.Contains(msg, "团结引擎"): // tokenizer_cjk
			if failProbe["tokenizer"] {
				isProbe()
				return
			}
			chat("x", "stop", 48)
		case strings.Contains(msg, "quicksort"): // tokenizer_code
			if failProbe["tokenizer"] {
				isProbe()
				return
			}
			chat("x", "stop", 70)
		case strings.Contains(msg, "混合文本"): // tokenizer_mixed
			if failProbe["tokenizer"] {
				isProbe()
				return
			}
			chat("x", "stop", 71)
		case msg == "hello" && body.Temperature == 2.0: // error_temp2
			if failProbe["errprobe"] {
				isProbe()
				return
			}
			if failProbe["auth401"] {
				auth401()
				return
			}
			errJSON("gateway: temperature 参数非法")
		case msg == "hello": // error_maxtok（max_tokens=10^9）
			if failProbe["errprobe"] {
				isProbe()
				return
			}
			if failProbe["auth401"] {
				auth401()
				return
			}
			errJSON("gateway: max_tokens 参数非法")
		case strings.Contains(msg, "回复一个字"): // finish_stop
			if failProbe["finish"] {
				isProbe()
				return
			}
			chat("好", "stop", 10)
		case strings.Contains(msg, "散文"): // finish_length
			if failProbe["finish"] {
				isProbe()
				return
			}
			chat("秋", "length", 20)
		case msg == distPrompt: // 分布采样
			if distContent == "" {
				chat("", "length", 20) // 思考型预算耗尽：正文未出
				return
			}
			chat(distContent, "stop", 20)
		default:
			chat("ok", "stop", 10)
		}
	}))
}

// TestCollectBaselineQualityGate 质量门槛（验收 6）：
// 探针 ok=3/8 或分布有效样本 <40 → CollectBaseline 返回错误且库中无该键；
// 合格采集正常落盘（模型键 + 官方标记）；非官方渠道直接拒收。
func TestCollectBaselineQualityGate(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	// 假上游直连（不走系统代理）+ 429 退避缩短为零，测试才不睡 26 秒
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = http.DefaultTransport
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	// 限流早停适配（2026-09-12）：场景 A 用 429 构造探针全错，会触发包级 429
	// 计数早停——本测覆盖「质量门槛」，注入高阈值跑满采集；退出清零 + 还原
	oldTh := rateLimitStormThreshold
	rateLimitStormThreshold = 1000
	defer func() { rateLimitStormThreshold = oldTh; resetProbeCost() }()

	// 场景 A：tokenizer_mixed / 错误探针 / finish 探针全 429 → 探针 ok=3/8 → 拒绝
	srvA := probeFakeUpstream(t, map[string]bool{"tokenizer": true, "errprobe": true, "finish": true}, "217")
	defer srvA.Close()
	bsA := &BaselineStore{path: filepath.Join(dir, "a.json"), Baselines: map[string]*Baseline{}}
	_, err := bsA.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srvA.URL, Channel: "tuanjie"}, "tuanjie", "GLM-5.3", "u1", 60)
	if err == nil || !containsStr(err.Error(), "门槛") {
		t.Fatalf("探针 ok=3/8 应被质量门槛拒绝: %v", err)
	}
	if len(bsA.Baselines) != 0 {
		t.Fatalf("被拒后库中不应有该键: %v", bsA.Baselines)
	}

	// 场景 B：8 项探针全 ok，但分布有效样本 10 <40 → 拒绝
	srvB := probeFakeUpstream(t, nil, "217")
	defer srvB.Close()
	bsB := &BaselineStore{path: filepath.Join(dir, "b.json"), Baselines: map[string]*Baseline{}}
	_, err = bsB.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srvB.URL, Channel: "tuanjie"}, "tuanjie", "GLM-5.3", "u1", 10)
	if err == nil || !containsStr(err.Error(), "分布有效样本") {
		t.Fatalf("分布有效样本 10 应被质量门槛拒绝: %v", err)
	}
	if len(bsB.Baselines) != 0 {
		t.Fatalf("被拒后库中不应有该键: %v", bsB.Baselines)
	}

	// 场景 C：全部合格 → 正常落盘（模型键 + 官方标记）
	srvC := probeFakeUpstream(t, nil, "217")
	defer srvC.Close()
	bsC := &BaselineStore{path: filepath.Join(dir, "c.json"), Baselines: map[string]*Baseline{}}
	bl, err := bsC.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srvC.URL, Channel: "tuanjie"}, "tuanjie", "GLM-5.3", "u1", 50)
	if err != nil {
		t.Fatalf("合格采集不应报错: %v", err)
	}
	if !bl.Official || bl.Channel != "tuanjie" {
		t.Fatalf("采集结果应带官方标记与来源渠道: %+v", bl)
	}
	if bsC.Get("GLM-5.3") == nil || bsC.Get("glm-5.3") == nil {
		t.Fatalf("落盘键应为规范化模型名: %v", bsC.Baselines)
	}

	// 场景 D：非官方渠道 → 直接拒收（不再自动采集）
	srvD := probeFakeUpstream(t, nil, "217")
	defer srvD.Close()
	bsD := &BaselineStore{path: filepath.Join(dir, "d.json"), Baselines: map[string]*Baseline{}}
	_, err = bsD.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srvD.URL, Channel: "bai"}, "bai", "m", "u1", 50)
	if err == nil || !containsStr(err.Error(), "非官方渠道") {
		t.Fatalf("非官方渠道采集应被拒: %v", err)
	}
	if len(bsD.Baselines) != 0 {
		t.Fatalf("非官方渠道不应落盘: %v", bsD.Baselines)
	}

	// 场景 E（第 34 轮）：采集结果 tokenizer 归一化值为负（"a" 报 ~88 的
	// 污染形态，dist 有效 50）→ 探针整批 unstable（ok=4/8 未过门槛）+
	// 物理判据双防线拒绝落盘
	srvE := probeFakeUpstream(t, map[string]bool{"aHigh": true}, "217")
	defer srvE.Close()
	bsE := &BaselineStore{path: filepath.Join(dir, "e.json"), Baselines: map[string]*Baseline{}}
	_, err = bsE.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srvE.URL, Channel: "tuanjie"}, "tuanjie", "GLM-5.3-FLASH", "u1", 50)
	if err == nil || !containsStr(err.Error(), "门槛") {
		t.Fatalf("tokenizer 整批 unstable（归一化值必为负）的采集应被质量门槛拒绝: %v", err)
	}
	if bsE.Get("GLM-5.3-FLASH") != nil || len(bsE.Baselines) != 0 {
		t.Fatalf("污染采集不应落盘: %v", bsE.Baselines)
	}
}

// TestBaselineMigrationDropsPollutedAnchors 第 34 轮验收（迁移路径）：
// 构造带 tuanjie|GLM-5.3-FLASH 负值（-57/-48/-26/-20）的旧库，探针 ok=7、
// dist 有效 43（旧门槛全过）→ 加载后该键必须不存在（迁移时丢弃）。
func TestBaselineMigrationDropsPollutedAnchors(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	counts := makeCounts()
	for i := 1; i <= 43; i++ {
		counts[i]++
	}
	file := map[string]*Baseline{
		// 真实污染形态：官方渠道、ok=7、dist 43、tokenizer 全负（v2 内容版本旧键）
		"tuanjie|GLM-5.3-FLASH": {Model: "GLM-5.3-FLASH", Channel: "tuanjie", SchemaVersion: baselineMigratableVersion,
			Probes: pollutedBaselineProbes(), Dist: distOf(counts, 43), SampledAt: "2026-09-05 10:00:00"},
		"tuanjie|codely-core": {Model: "codely-core", Channel: "tuanjie", SchemaVersion: baselineMigratableVersion,
			Probes: pollutedBaselineProbes(), Dist: distOf(counts, 49), SampledAt: "2026-09-05 11:00:00"},
	}
	b, _ := json.Marshal(map[string]any{"baselines": file})
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-baselines.json"), b, 0o600); err != nil {
		t.Fatal(err)
	}

	bs := LoadBaselines()
	if len(bs.Baselines) != 0 {
		t.Fatalf("两条污染锚点迁移后应全部丢弃，got %v", bs.Baselines)
	}
	if bs.Get("GLM-5.3-FLASH") != nil || bs.Get("codely-core") != nil {
		t.Fatal("污染锚点不应以任何模型键存活")
	}
}

// TestBaselineSchemaVersionGate 第 35 轮版本门禁：存量基准的 tokenizer 归一化
// 底座误取 completion_tokens（probeCall 第 2 个返回值），数值整体不可信——
// ① 旧格式（无版本号 / 低于当前版本）的干净官方基准 → 加载后不存在（哪怕
//    各项都过质量门槛）；
// ② 新采集（CollectBaseline）的基准带当前版本号且能正常加载（save→Load 往返）。
func TestBaselineSchemaVersionGate(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	goodDist := distOf(counts, 50)
	file := map[string]*Baseline{
		// 真实存量形态之一：版本号字段根本不存在（SchemaVersion 零值）
		"glm-5.3": {Model: "GLM-5.3", Channel: "tuanjie",
			Probes: goodBaselineProbes(), Dist: goodDist, SampledAt: "2026-09-06 21:58:05"},
		// 真实存量形态之二：内容版本低于可迁移下限（<2，tokenizer 底座不可信）
		"codely-air": {Model: "codely-air", Channel: "comate", SchemaVersion: baselineMigratableVersion - 1,
			Probes: goodBaselineProbes(), Dist: goodDist, SampledAt: "2026-09-06 22:28:25"},
	}
	b, _ := json.Marshal(map[string]any{"baselines": file})
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-baselines.json"), b, 0o600); err != nil {
		t.Fatal(err)
	}

	// ① 旧格式（无版本号 / 旧版本）加载后不存在
	bs := LoadBaselines()
	if len(bs.Baselines) != 0 {
		t.Fatalf("无版本号 / 旧版本条目应按版本门禁全部丢弃，got %v", bs.Baselines)
	}
	if bs.Get("GLM-5.3") != nil || bs.Get("codely-air") != nil {
		t.Fatal("旧格式基准不应以任何模型键存活")
	}

	// ② 新采集的基准带当前版本号且能正常加载（不走真实网络：直接写入
	//    CollectBaseline 的落盘形态后重读；键 = 模型|渠道 多锚格式）
	bs2 := &BaselineStore{path: filepath.Join(dir, "tuanjie-baselines.json"), Baselines: map[string]*Baseline{}}
	bs2.Baselines[baselineKey("GLM-5.3", "tuanjie")] = &Baseline{
		Model: "GLM-5.3", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion,
		Probes:        goodBaselineProbes(), Dist: goodDist,
		SampledAt: "2026-09-11 23:00:00", Account: "u1", SampleCount: 50,
	}
	bs2.save()
	bs3 := LoadBaselines()
	got := bs3.Get("GLM-5.3")
	if got == nil {
		t.Fatal("带当前版本号的基准应正常加载")
	}
	if got.SchemaVersion != baselineSchemaVersion {
		t.Fatalf("加载后版本号应保持当前版本 %d, got %d", baselineSchemaVersion, got.SchemaVersion)
	}
	if got.Model != "GLM-5.3" || len(got.Probes) != 8 || got.Dist == nil || got.Dist.Valid != 50 {
		t.Fatalf("基准内容往返不一致: %+v", got)
	}

	// ③ CollectBaseline 落盘写入当前版本号（借假上游走真实采集路径）
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = http.DefaultTransport
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	srv := probeFakeUpstream(t, nil, "217")
	defer srv.Close()
	bs4 := &BaselineStore{path: filepath.Join(dir, "collect.json"), Baselines: map[string]*Baseline{}}
	if _, err := bs4.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srv.URL, Channel: "tuanjie"}, "tuanjie", "GLM-5.3", "u1", 50); err != nil {
		t.Fatalf("合格采集不应报错: %v", err)
	}
	if bs4.Get("GLM-5.3") == nil || bs4.Get("GLM-5.3").SchemaVersion != baselineSchemaVersion {
		t.Fatalf("采集落盘应写入当前版本号 %d: %+v", baselineSchemaVersion, bs4.Get("GLM-5.3"))
	}
}

// TestCrossChannelOfficialBaselineCompare 跨渠道按模型命中官方基准（验收 3）：
// tuanjie 采的 GLM-5.3-FLASH 官方基准（tokenizer 26/35/57/58、dist 有效 50），
// 以 bai glm-5.3-flash 的同样探针值比对 → tokenizer 4 项 match、网关报错探针
// 跨渠道跳过不计 mismatch、结论 green/yellow（不再红灯）；同渠道时报错探针
// 照常比对（措辞不同即 mismatch）。
func TestCrossChannelOfficialBaselineCompare(t *testing.T) {
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	base := &Baseline{
		Model: "GLM-5.3-FLASH", Channel: "tuanjie", Official: true,
		Probes: goodBaselineProbes(),
		Dist:   distOf(counts, 50),
	}
	// bai 侧探针：tokenizer/finish 与官方基准相同；报错原文是 B 网关自己的措辞
	cur := []probeResult{
		{Name: "tokenizer_en", Value: "26", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "35", Status: "ok"},
		{Name: "tokenizer_code", Value: "57", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "58", Status: "ok"},
		{Name: "error_temp2", Value: `{"message":"gateway B: temperature 参数非法"}`, Status: "ok"},
		{Name: "error_maxtok", Value: `{"message":"gateway B: max_tokens 参数非法"}`, Status: "ok"},
		{Name: "finish_stop", Value: "stop", Status: "ok"},
		{Name: "finish_length", Value: "length", Status: "ok"},
	}
	cmps, sim, v := CompareToBaseline(base, cur, distOf(counts, 50), "bai")
	for _, c := range cmps {
		if strings.HasPrefix(c.Name, "tokenizer_") && !c.Match {
			t.Fatalf("tokenizer 探针 %s 同值应 match: %+v", c.Name, c)
		}
		if gatewayWordingProbe(c.Name) && c.Status != "skip" {
			t.Fatalf("报错原文探针 %s 跨渠道应跳过: %+v", c.Name, c)
		}
	}
	if v.Light != "green" && v.Light != "yellow" {
		t.Fatalf("跨渠道命中官方基准不应红灯: %+v (sim=%+v)", v, sim)
	}
	if !containsStr(v.Reason, "跨渠道不可比") {
		t.Fatalf("reason 应提示跨渠道跳过: %q", v.Reason)
	}

	// 同渠道（tuanjie 对 tuanjie 基准）：报错探针措辞变化照常比对 → mismatch
	cmps2, _, v2 := CompareToBaseline(base, cur, distOf(counts, 50), "tuanjie")
	skipped := 0
	for _, c := range cmps2 {
		if c.Status == "skip" {
			skipped++
		}
	}
	if skipped != 0 {
		t.Fatalf("同渠道不应跳过报错探针: %v", cmps2)
	}
	if v2.Light != "red" {
		t.Fatalf("同渠道两项报错探针不匹配应红灯: %+v", v2)
	}
}

// TestAuthErrorNotFingerprint 终审第 36 轮·假红验收：401/403/404 鉴权/未找到
// 类响应不是「上游报错原文」指纹——错误探针如实标 error 带状态码、不产指纹
// 值；同渠道比对下两条鉴权错误不产生 mismatch、不得判 red（旧版把 401 的
// JSON 错误体当指纹值，两条鉴权错误措辞不同即计 mismatch ≥2 → 假红
// 「疑似注水」）。
func TestAuthErrorNotFingerprint(t *testing.T) {
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = http.DefaultTransport
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// 假上游：错误探针一打就 401（渠道 key 失效形态），其余探针正常
	srv := probeFakeUpstream(t, map[string]bool{"auth401": true}, "217")
	defer srv.Close()
	probes := RunPipelineProbes(context.Background(),
		&probeTarget{BaseURL: srv.URL, Channel: "tuanjie"}, "GLM-5.3")

	for _, name := range []string{"error_temp2", "error_maxtok"} {
		found := false
		for _, p := range probes {
			if p.Name != name {
				continue
			}
			found = true
			if p.Status != "error" {
				t.Fatalf("401 鉴权错误应如实标 error（不得当指纹值）: %+v", p)
			}
			if p.Value != "" {
				t.Fatalf("401 不得产生指纹值: %+v", p)
			}
			if p.HTTPStatus != 401 {
				t.Fatalf("探针应保留 HTTP 401 状态码: %+v", p)
			}
		}
		if !found {
			t.Fatalf("探针 %s 缺失: %+v", name, probes)
		}
	}

	// 同渠道比对：基准报错原文是网关措辞、本次两条 401 鉴权错误——不得计
	// mismatch 判 red；2 项未测成 → grey「无法判定」
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	base := &Baseline{Model: "GLM-5.3", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: goodBaselineProbes(), Dist: distOf(counts, 50)}
	cmps, _, v := CompareToBaseline(base, probes, distOf(counts, 50), "tuanjie")
	for _, c := range cmps {
		if c.Status == "ok" && !c.Match {
			t.Fatalf("鉴权错误探针不得产生 mismatch（假红源头）: %+v", c)
		}
	}
	if v.Light != "grey" {
		t.Fatalf("同渠道两条 401 鉴权错误不得判 red（应为 grey 无法判定）: %+v", v)
	}
}

// TestProbeTokenizerUnstableOnAllPollutedASamples 第 34 轮验收："a" 三次采样
// 全部被污染（88/88/88，中位数离群判据结构性失效）→ 归一化值 39-88/48-88/
// 70-88/71-88 = -49/-40/-18/-17 为负 → 该批 tokenizer 探针整批标 unstable
// 「无法归一化」，其余探针不受影响；CollectBaseline 拒绝落盘（库中无该键）。
func TestProbeTokenizerUnstableOnAllPollutedASamples(t *testing.T) {
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = http.DefaultTransport
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// "a" 恒 88、文本恒正常（39/48/70/71）→ 归一化值全负，且三次采样全 88
	// （中位数=最小值，离群判据必失效）
	srv := probeFakeUpstream(t, map[string]bool{"aHigh": true}, "217")
	defer srv.Close()
	probes := RunPipelineProbes(context.Background(),
		&probeTarget{BaseURL: srv.URL, Channel: "tuanjie"}, "GLM-5.3-FLASH")

	tokCount := 0
	for _, p := range probes {
		if !strings.HasPrefix(p.Name, "tokenizer_") {
			continue
		}
		tokCount++
		if p.Status != "unstable" {
			t.Fatalf("污染采样下 tokenizer 探针 %s 应整批 unstable: %+v", p.Name, p)
		}
		if p.Value != "" {
			t.Fatalf("unstable 探针不应携带归一化值（防污染入库）: %+v", p)
		}
		if !strings.Contains(p.Note, "无法归一化") {
			t.Fatalf("note 应说明无法归一化: %+v", p)
		}
	}
	if tokCount != 4 {
		t.Fatalf("应有 4 项 tokenizer 探针，got %d", tokCount)
	}
	// 其余探针不受 "a" 污染影响：报错 2 项 + finish 2 项照常 ok
	for _, p := range probes {
		if strings.HasPrefix(p.Name, "tokenizer_") {
			continue
		}
		if p.Status != "ok" {
			t.Fatalf("非 tokenizer 探针 %s 不应受 a 基准污染影响: %+v", p.Name, p)
		}
	}

	// 采集路径：即使探针 ok 数与 dist 都过旧门槛，tokenizer 全 unstable →
	// ok<6 被拒（防线 1）；再叠加负值物理判据兜底（防线 2），库中无该键。
	bs := &BaselineStore{path: filepath.Join(t.TempDir(), "b.json"), Baselines: map[string]*Baseline{}}
	if _, err := bs.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srv.URL, Channel: "tuanjie"}, "tuanjie", "GLM-5.3-FLASH", "u1", 50); err == nil {
		t.Fatal("tokenizer 整批 unstable 的采集应被质量门槛拒绝")
	}
	if len(bs.Baselines) != 0 {
		t.Fatalf("污染采集不应写库: %v", bs.Baselines)
	}
}
func TestWaterChannelsRegistry(t *testing.T) {
	ids := map[string]waterChannelDef{}
	for _, c := range waterChannels {
		ids[c.ID] = c
	}
	for _, id := range []string{"tuanjie", "command", "workbuddy", "workbuddy-intl", "bai", "comate", "qoder", "tokenrouter"} {
		if _, ok := ids[id]; !ok {
			t.Fatalf("注册表缺渠道 %s", id)
		}
	}
	if ids["workbuddy-intl"].Port != 8789 || ids["workbuddy-intl"].BaseURL != "http://127.0.0.1:8789" {
		t.Fatalf("workbuddy-intl 端点定义不符: %+v", ids["workbuddy-intl"])
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
	// 远端官方锚点渠道 TokenRouter：BaseURL 直连远端（本地端口无意义、零值），
	// key 走渠道配置（tuanjie-water-channels.json 的 setkey 机制）
	if ids["tokenrouter"].BaseURL != "https://api.tokenrouter.com" || ids["tokenrouter"].BearerFrom != "config" || ids["tokenrouter"].Port != 0 {
		t.Fatalf("tokenrouter 端点定义不符: %+v", ids["tokenrouter"])
	}
	// strong 标记：官方链路四渠道（tuanjie/comate/qoder/tokenrouter）true，其余 false
	for _, c := range waterChannels {
		if got := isStrongChannel(c.ID); got != (c.ID == "tuanjie" || c.ID == "comate" || c.ID == "qoder" || c.ID == "tokenrouter") {
			t.Fatalf("渠道 %s strong 判定不符: %v", c.ID, got)
		}
	}
}

// TestBaselineMultiAnchorMigration 验收 2（多锚存储与迁移）：v2 裸模型名键 +
// v1「渠道|模型」旧键（同模型不同渠道的两条旧锚）→ 加载后键为 模型|渠道、
// GetAnchors 返回 2 条；内容版本 <2 的条目不加载（tokenizer 底座不可信，
// 迁移后仍低于 3 的正是这一类）；非官方渠道旧锚丢弃；重读稳定。
func TestBaselineMultiAnchorMigration(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	goodDist := func() *distResult {
		c := makeCounts()
		for i := 1; i <= 50; i++ {
			c[i]++
		}
		return distOf(c, 50)
	}
	// tokenrouter 侧旧锚探针：tokenizer cjk/mixed 与团结锚差超 ±1（锚间不一致形态）
	trProbes := goodBaselineProbes()
	for i := range trProbes {
		switch trProbes[i].Name {
		case "tokenizer_cjk":
			trProbes[i].Value = "29"
		case "tokenizer_mixed":
			trProbes[i].Value = "55"
		}
	}
	file := map[string]*Baseline{
		// v2 裸模型名键（内容版本 2）：按数据体 Channel 字段重键为 模型|渠道
		"glm-5.3-flash": {Model: "glm-5.3-flash", Channel: "tuanjie", SchemaVersion: baselineMigratableVersion,
			Probes: goodBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-11 10:00:00"},
		// v1「渠道|模型」旧键（内容版本 2）：同模型另一渠道（tokenrouter）的锚
		"tokenrouter|GLM-5.3-FLASH": {Model: "GLM-5.3-FLASH", Channel: "tokenrouter", SchemaVersion: baselineMigratableVersion,
			Probes: trProbes, Dist: goodDist(), SampledAt: "2026-09-12 10:00:00"},
		// 内容版本 <2（无版本号/旧格式，归一化底座不可信）：不迁移不加载
		"legacy-glm": {Model: "legacy-glm", Channel: "tuanjie", SchemaVersion: baselineMigratableVersion - 1,
			Probes: goodBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-10 10:00:00"},
		// 非官方渠道旧锚：丢弃（不再用自采基准判定）
		"bai|glm-5.3-flash": {Model: "glm-5.3-flash", Channel: "bai", SchemaVersion: baselineMigratableVersion,
			Probes: goodBaselineProbes(), Dist: goodDist(), SampledAt: "2026-09-12 11:00:00"},
	}
	b, _ := json.Marshal(map[string]any{"baselines": file})
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-baselines.json"), b, 0o600); err != nil {
		t.Fatal(err)
	}

	bs := LoadBaselines()
	if got := len(bs.Baselines); got != 2 {
		t.Fatalf("迁移后应只剩 2 条官方锚，got %d: %v", got, bs.Baselines)
	}
	// 键为 模型|渠道 多锚格式（裸模型名键与渠道|模型键都归并到该格式）
	if bs.Baselines["glm-5.3-flash|tuanjie"] == nil || bs.Baselines["glm-5.3-flash|tokenrouter"] == nil {
		t.Fatalf("加载后键应为 模型|渠道: %v", bs.Baselines)
	}
	if bs.Baselines["glm-5.3-flash"] != nil || bs.Baselines["tokenrouter|GLM-5.3-FLASH"] != nil {
		t.Fatalf("旧格式键不应存活: %v", bs.Baselines)
	}
	// 内容可信的旧条目迁移升版本为当前版本（键格式升级，内容未变）
	for k, v := range bs.Baselines {
		if v.SchemaVersion != baselineSchemaVersion {
			t.Fatalf("迁移条目 %s 应升版本为 %d, got %d", k, baselineSchemaVersion, v.SchemaVersion)
		}
	}
	// GetAnchors：同模型两条锚，按 SampledAt 新→旧（tokenrouter 在前）
	anchors := bs.GetAnchors("GLM-5.3-FLASH")
	if len(anchors) != 2 {
		t.Fatalf("GetAnchors 应返回 2 条锚, got %d: %+v", len(anchors), anchors)
	}
	if anchors[0].Channel != "tokenrouter" || anchors[1].Channel != "tuanjie" {
		t.Fatalf("GetAnchors 应按 SampledAt 新→旧排序: %+v", anchors)
	}
	// Get 兼容：多锚下取最新一条
	if g := bs.Get("glm-5.3-flash"); g == nil || g.Channel != "tokenrouter" {
		t.Fatalf("Get 应返回最新锚（tokenrouter）: %+v", g)
	}
	// 版本 <2 / 非官方渠道的条目不加载
	if bs.Get("legacy-glm") != nil {
		t.Fatal("内容版本 <2 的条目不应加载")
	}
	if bs.Baselines["glm-5.3-flash|bai"] != nil {
		t.Fatal("非官方渠道旧锚不应加载")
	}
	// 落盘重读稳定：键与锚数不再变化
	bs2 := LoadBaselines()
	if len(bs2.Baselines) != 2 || len(bs2.GetAnchors("glm-5.3-flash")) != 2 {
		t.Fatalf("二次加载应稳定: %v", bs2.Baselines)
	}
}

// TestCompareAgainstAnchorsAnyMatchWins 验收 3（任一锚一致即一致）：
//   - 锚A（团结）匹配、锚B（tokenrouter）偏离 → 结论 green 且 reason 点名「团结」；
//   - 两条都偏离 → red（维持单锚 red 语义）；
//   - 锚列表为空 → 无法判定（缺官方基准）。
func TestCompareAgainstAnchorsAnyMatchWins(t *testing.T) {
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	anchorTuanjie := &Baseline{Model: "glm-5.3-flash", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: goodBaselineProbes(), Dist: distOf(counts, 50),
		SampledAt: "2026-09-11 10:00:00"}
	// tokenrouter 锚：tokenizer cjk/mixed 偏离团结锚超 ±1（真实 KIMI-K3 形态 29/55 vs 35/58）
	trProbes := goodBaselineProbes()
	for i := range trProbes {
		switch trProbes[i].Name {
		case "tokenizer_cjk":
			trProbes[i].Value = "29"
		case "tokenizer_mixed":
			trProbes[i].Value = "55"
		}
	}
	anchorTR := &Baseline{Model: "glm-5.3-flash", Channel: "tokenrouter", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: trProbes, Dist: distOf(counts, 50),
		SampledAt: "2026-09-12 10:00:00"}
	// bai 侧当前探针：tokenizer/finish 与团结锚一致，报错原文为 B 网关措辞
	//（跨渠道对两条锚都跳过，不计 mismatch）
	cur := []probeResult{
		{Name: "tokenizer_en", Value: "26", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "35", Status: "ok"},
		{Name: "tokenizer_code", Value: "57", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "58", Status: "ok"},
		{Name: "error_temp2", Value: `{"message":"gateway B: temperature 参数非法"}`, Status: "ok"},
		{Name: "error_maxtok", Value: `{"message":"gateway B: max_tokens 参数非法"}`, Status: "ok"},
		{Name: "finish_stop", Value: "stop", Status: "ok"},
		{Name: "finish_length", Value: "length", Status: "ok"},
	}

	// ① 锚A 匹配、锚B 偏离 → 与任一锚一致即一致：green，reason 点名团结
	cmps, _, v, matched := CompareAgainstAnchors([]*Baseline{anchorTuanjie, anchorTR}, cur, distOf(counts, 50), "bai")
	if v.Light != "green" {
		t.Fatalf("与团结锚全匹配应判 green（与任一锚一致即一致）: %+v", v)
	}
	if !containsStr(v.Reason, "团结") || !containsStr(v.Reason, "一致") {
		t.Fatalf("green reason 应点名匹配锚渠道「团结」: %q", v.Reason)
	}
	if matched == nil || matched.Channel != "tuanjie" {
		t.Fatalf("匹配锚应为团结锚: %+v", matched)
	}
	for _, c := range cmps {
		if strings.HasPrefix(c.Name, "tokenizer_") && (!c.Comparable || !c.Match) {
			t.Fatalf("tokenizer %s 对团结锚应全匹配: %+v", c.Name, c)
		}
	}

	// ② 两条锚都偏离 → red（择优仍取偏离最少那条，reason 点名比对锚）
	deviated := []probeResult{
		{Name: "tokenizer_en", Value: "40", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "50", Status: "ok"},
		{Name: "tokenizer_code", Value: "60", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "70", Status: "ok"},
		{Name: "error_temp2", Value: `{"message":"gateway B: temperature 参数非法"}`, Status: "ok"},
		{Name: "error_maxtok", Value: `{"message":"gateway B: max_tokens 参数非法"}`, Status: "ok"},
		{Name: "finish_stop", Value: "stop", Status: "ok"},
		{Name: "finish_length", Value: "length", Status: "ok"},
	}
	_, _, v2, matched2 := CompareAgainstAnchors([]*Baseline{anchorTuanjie, anchorTR}, deviated, distOf(counts, 50), "bai")
	if v2.Light != "red" {
		t.Fatalf("两条锚都偏离应判 red: %+v", v2)
	}
	if matched2 == nil {
		t.Fatal("red 也应择优返回比对锚（偏离最少那条）")
	}
	if !containsStr(v2.Reason, "比对") {
		t.Fatalf("red reason 应点名比对锚: %q", v2.Reason)
	}

	// ③ 锚列表为空 → 无法判定（缺官方基准），matched 为 nil
	_, _, v3, matched3 := CompareAgainstAnchors(nil, cur, distOf(counts, 50), "bai")
	if v3.Light != "grey" || !containsStr(v3.Reason, "无官方基准") {
		t.Fatalf("无锚应 grey 引导采集: %+v", v3)
	}
	if matched3 != nil {
		t.Fatalf("无锚时 matched 应为 nil: %+v", matched3)
	}
}

// TestAnchorConflictNoteStandalone 验收 4（锚间一致性独立信号）：
// 两条锚 ≥2 项可比探针超 ±1 容差 → 提示点名两条锚的渠道与项数；仅 1 项超容差
// 或单锚 → 无提示（不占位）；同验收 3 场景对照——锚间冲突不得改变灯色
//（匹配锚场景下有/无冲突锚结论同为 green）。
func TestAnchorConflictNoteStandalone(t *testing.T) {
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	dist := distOf(counts, 50)
	anchorA := &Baseline{Model: "glm-5.3-flash", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: goodBaselineProbes(), Dist: dist,
		SampledAt: "2026-09-11 10:00:00"}
	// 锚B：cjk Δ6、mixed Δ3 超 ±1 容差（2 项），code Δ1 在容差内
	bProbes := goodBaselineProbes()
	for i := range bProbes {
		switch bProbes[i].Name {
		case "tokenizer_cjk":
			bProbes[i].Value = "29"
		case "tokenizer_code":
			bProbes[i].Value = "58"
		case "tokenizer_mixed":
			bProbes[i].Value = "55"
		}
	}
	anchorB := &Baseline{Model: "glm-5.3-flash", Channel: "tokenrouter", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: bProbes, Dist: dist,
		SampledAt: "2026-09-12 10:00:00"}

	// ≥2 项超容差 → 提示如实点名两条锚的渠道与项数
	note := anchorConflictNote([]*Baseline{anchorA, anchorB})
	if !containsStr(note, "团结") || !containsStr(note, "TokenRouter") || !containsStr(note, "2 项") {
		t.Fatalf("锚间不一致提示应点名渠道与项数: %q", note)
	}
	if !containsStr(note, "不参与灯色") {
		t.Fatalf("提示应注明不参与灯色判定: %q", note)
	}

	// 仅 1 项超容差 → 无提示（不占位）
	cProbes := goodBaselineProbes()
	for i := range cProbes {
		if cProbes[i].Name == "tokenizer_cjk" {
			cProbes[i].Value = "37" // Δ2 超容差，其余全一致
		}
	}
	anchorC := &Baseline{Model: "glm-5.3-flash", Channel: "tokenrouter", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: cProbes, Dist: dist,
		SampledAt: "2026-09-12 10:00:00"}
	if note := anchorConflictNote([]*Baseline{anchorA, anchorC}); note != "" {
		t.Fatalf("仅 1 项超容差不应提示: %q", note)
	}
	// 单锚 → 无提示
	if note := anchorConflictNote([]*Baseline{anchorA}); note != "" {
		t.Fatalf("单锚不应提示: %q", note)
	}

	// 灯色不因锚间冲突改变（同验收 3 场景对照）：当前探针匹配团结锚，
	// 有/无冲突的 tokenrouter 锚结论同为 green——锚间打架只提示、不参与灯色
	cur := []probeResult{
		{Name: "tokenizer_en", Value: "26", Status: "ok"},
		{Name: "tokenizer_cjk", Value: "35", Status: "ok"},
		{Name: "tokenizer_code", Value: "57", Status: "ok"},
		{Name: "tokenizer_mixed", Value: "58", Status: "ok"},
		{Name: "error_temp2", Value: `{"message":"gateway B: temperature 参数非法"}`, Status: "ok"},
		{Name: "error_maxtok", Value: `{"message":"gateway B: max_tokens 参数非法"}`, Status: "ok"},
		{Name: "finish_stop", Value: "stop", Status: "ok"},
		{Name: "finish_length", Value: "length", Status: "ok"},
	}
	_, _, vSingle, _ := CompareAgainstAnchors([]*Baseline{anchorA}, cur, dist, "bai")
	_, _, vMulti, _ := CompareAgainstAnchors([]*Baseline{anchorA, anchorB}, cur, dist, "bai")
	if vSingle.Light != "green" || vMulti.Light != "green" {
		t.Fatalf("锚间冲突不得改变灯色: 单锚=%v 多锚=%v", vSingle.Light, vMulti.Light)
	}
}

// TestTokenRouterChannelRemoteModelsAndKeyErrors 验收 5（远端渠道）：
// tokenrouter 渠道走 BaseURL 的 HTTP 拉模型列表（无本地端口拨测）能出模型；
// 缺 key 报错文案按渠道名通用（含 TokenRouter，不再写死 B.ai）；channelTarget
// 同口径，配 key 后构造远端 target（Bearer 头 + 渠道名）。
func TestTokenRouterChannelRemoteModelsAndKeyErrors(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	// 假远端上游：/v1/models 出模型列表（tokenrouter 侧真实形态 z-ai/glm-5.3-free）
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/models" {
			w.WriteHeader(404)
			return
		}
		if r.Header.Get("Authorization") != "Bearer k" {
			w.WriteHeader(401)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{
			"data": []map[string]any{{"id": "z-ai/glm-5.3-free"}, {"id": "z-ai/glm-5.3-air"}},
		})
	}))
	defer srv.Close()

	// 注册表里的 tokenrouter 定义（BaseURL 改指假上游，其余字段保持原样）
	var def waterChannelDef
	for _, c := range waterChannels {
		if c.ID == "tokenrouter" {
			def = c
		}
	}
	def.BaseURL = srv.URL
	s := &Server{}
	models, note := s.fetchChannelModels(context.Background(), def, map[string]string{"tokenrouter": "k"})
	if note != "" || len(models) != 2 || models[0] != "z-ai/glm-5.3-free" {
		t.Fatalf("远端渠道应能按 BaseURL 出模型列表: models=%v note=%q", models, note)
	}

	// 缺 key：模型列表报错文案含渠道名 TokenRouter，不写死 B.ai
	_, note = s.fetchChannelModels(context.Background(), def, map[string]string{})
	if !containsStr(note, "TokenRouter") || containsStr(note, "B.ai") {
		t.Fatalf("缺 key 报错应按渠道名通用: %q", note)
	}
	if _, err := s.channelTarget("tokenrouter"); err == nil ||
		!containsStr(err.Error(), "TokenRouter") || containsStr(err.Error(), "B.ai") {
		t.Fatalf("channelTarget 缺 key 报错应含渠道名而非写死 B.ai: %v", err)
	}
	// bai 渠道同口径：报错含 B.ai（通用文案按各自渠道名渲染）
	if _, err := s.channelTarget("bai"); err == nil || !containsStr(err.Error(), "B.ai") {
		t.Fatalf("bai 缺 key 报错应含 B.ai: %v", err)
	}
	// 配了 key：channelTarget 构造远端 target（探针拼 /v1/chat/completions 由 BaseURL 决定）
	if err := SaveChannelKey("tokenrouter", "k"); err != nil {
		t.Fatal(err)
	}
	tgt, err := s.channelTarget("tokenrouter")
	if err != nil {
		t.Fatalf("配 key 后 channelTarget 不应报错: %v", err)
	}
	if tgt == nil || tgt.Headers["Authorization"] != "Bearer k" || tgt.Channel != "tokenrouter" {
		t.Fatalf("远端 target 构造不符: %+v", tgt)
	}
}

// tokenRouterFakeUpstream 远端锚点采集假上游：8 项探针全 ok + 分布采样出数，
// 且每个成功 chat 响应自报 model=glm-5.3（登记名为 glm-5.3-flash——名实不符
// 形态，学 TokenRouter 请求 z-ai/glm-5.3-free、自报 glm-5.3 的实测）。
func tokenRouterFakeUpstream(t *testing.T) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Messages  []struct {
				Content string `json:"content"`
			} `json:"messages"`
			Temperature float64 `json:"temperature"`
			MaxTokens   int     `json:"max_tokens"`
		}
		if json.NewDecoder(r.Body).Decode(&body) != nil || len(body.Messages) == 0 {
			w.WriteHeader(400)
			return
		}
		msg := body.Messages[0].Content
		chat := func(content, finish string, promptTok int) {
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(map[string]any{
				"model":    "glm-5.3", // 上游自报名（与登记名名实不符）
				"usage":    map[string]any{"prompt_tokens": promptTok, "completion_tokens": 3},
				"choices":  []map[string]any{{"finish_reason": finish, "message": map[string]any{"content": content}}},
			})
		}
		errJSON := func(msg string) {
			w.Header().Set("Content-Type", "application/json")
			w.WriteHeader(400)
			_ = json.NewEncoder(w).Encode(map[string]any{
				"error": map[string]any{"message": msg, "type": "invalid_request_error", "code": "400"},
			})
		}
		switch {
		case msg == "a":
			chat("a", "stop", 13)
		case strings.Contains(msg, "philosophers"): // tokenizer_en 39−13=26
			chat("x", "stop", 39)
		case strings.Contains(msg, "团结引擎"): // tokenizer_cjk 48−13=35
			chat("x", "stop", 48)
		case strings.Contains(msg, "quicksort"): // tokenizer_code 70−13=57
			chat("x", "stop", 70)
		case strings.Contains(msg, "混合文本"): // tokenizer_mixed 71−13=58
			chat("x", "stop", 71)
		case msg == "hello" && body.Temperature == 2.0: // error_temp2
			errJSON("gateway T: temperature 参数非法")
		case msg == "hello": // error_maxtok
			errJSON("gateway T: max_tokens 参数非法")
		case strings.Contains(msg, "回复一个字"): // finish_stop
			chat("好", "stop", 10)
		case strings.Contains(msg, "散文"): // finish_length
			chat("秋", "length", 20)
		case msg == distPrompt: // 分布采样
			chat("217", "stop", 20)
		default:
			chat("ok", "stop", 10)
		}
	}))
}

// TestCollectBaselineReturnedModelAndKey 验收 5（登记名 vs 上游自报名）：
// tokenrouter 采集登记名 glm-5.3-flash、上游自报 glm-5.3 → 锚记录 ReturnedModel
// 且报告④如实显示不一致；键仍按登记名建（glm-5.3-flash|tokenrouter，绝不按
// 返回名——按返回名建键会查不到锚）；同渠道重采覆盖自己、不影响团结锚。
func TestCollectBaselineReturnedModelAndKey(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = http.DefaultTransport
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB; resetProbeCost() }()

	srv := tokenRouterFakeUpstream(t)
	defer srv.Close()
	bs := &BaselineStore{path: filepath.Join(dir, "tuanjie-baselines.json"), Baselines: map[string]*Baseline{}}
	bl, err := bs.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srv.URL, Channel: "tokenrouter"}, "tokenrouter", "glm-5.3-flash", "u1", 50)
	if err != nil {
		t.Fatalf("官方渠道（tokenrouter）采集不应报错: %v", err)
	}
	// 自报名如实记录；键按登记名建
	if bl.ReturnedModel != "glm-5.3" {
		t.Fatalf("锚记录应含上游自报名 glm-5.3: %+v", bl)
	}
	if bl.Channel != "tokenrouter" || !bl.Official {
		t.Fatalf("采集结果应带来源渠道与官方标记: %+v", bl)
	}
	if bs.Baselines["glm-5.3-flash|tokenrouter"] == nil {
		t.Fatalf("键应按登记名建为 glm-5.3-flash|tokenrouter: %v", bs.Baselines)
	}
	if bs.Baselines["glm-5.3|tokenrouter"] != nil {
		t.Fatal("不得按上游返回名建键（否则登记名查不到锚）")
	}
	if got := bs.Get("GLM-5.3-FLASH"); got == nil || got.ReturnedModel != "glm-5.3" {
		t.Fatalf("按登记名（大小写归一）应能取回锚: %+v", got)
	}
	if len(bs.GetAnchors("glm-5.3-flash")) != 1 {
		t.Fatalf("同模型应恰 1 条锚: %+v", bs.GetAnchors("glm-5.3-flash"))
	}

	// 同渠道重采覆盖自己、不影响其他渠道的锚（多锚并存）
	bs.Baselines[baselineKey("glm-5.3-flash", "tuanjie")] = &Baseline{
		Model: "glm-5.3-flash", Channel: "tuanjie", Official: true, SchemaVersion: baselineSchemaVersion,
		SampledAt: "2026-09-11 10:00:00", Account: "u0", SampleCount: 50,
	}
	bl2, err := bs.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: srv.URL, Channel: "tokenrouter"}, "tokenrouter", "glm-5.3-flash", "u1", 50)
	if err != nil {
		t.Fatalf("重采 tokenrouter 不应报错: %v", err)
	}
	if len(bs.Baselines) != 2 || bs.Baselines["glm-5.3-flash|tuanjie"] == nil {
		t.Fatalf("同渠道重采应覆盖自己、不影响团结锚: %v", bs.Baselines)
	}

	// 报告④如实显示名实不符（该锚点登记为 glm-5.3-flash，上游自报 glm-5.3）
	items := buildWaterReportItems("tokenrouter", nil, nil, distSimilarity{}, bl2, false)
	if len(items) < 4 || items[3].Name != "基准状态" {
		t.Fatalf("④基准状态项缺失: %+v", items)
	}
	if !containsStr(items[3].Detail, "登记为 glm-5.3-flash") || !containsStr(items[3].Detail, "自报 glm-5.3") {
		t.Fatalf("④应如实显示登记名与上游自报名不一致: %+v", items[3])
	}
	// 大小写/路由前缀差异不算名实不符（normalizeModelName 同名不提示）
	sameName := *bl2
	sameName.ReturnedModel = "GLM-5.3-Flash"
	items2 := buildWaterReportItems("tokenrouter", nil, nil, distSimilarity{}, &sameName, false)
	if containsStr(items2[3].Detail, "自报") {
		t.Fatalf("大小写差异不算名实不符: %+v", items2[3])
	}
}

// TestCollectBaselineRateLimitAbort 限流早停（采集路径，2026-09-12 回归修复）
// 验收：前 3 个请求正常（恰为 "a" 基准 3 次采样）、其后全部 429 空体 →
// CollectBaseline 因限流中止返回错误（含「限流」）、不落盘；早停后请求数
// 远小于完整采集规模（探针 15 调 + 采样 60）。
func TestCollectBaselineRateLimitAbort(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0} // 零退避注入：不依赖真实等待
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	resetProbeCost() // 不经 handleWaterCheck 入口，手动清零包级 429 计数
	defer resetProbeCost()

	rt := &countingRT{route: func(_ string, _ float64, _ int, n int32) *http.Response {
		if n <= 3 { // 前 3 个请求正常，其后全部 429 空体（限流风暴形态）
			return chatResp("a", "stop", 13)
		}
		return rtResp(429, "")
	}}
	probeTransport = rt
	bs := &BaselineStore{path: filepath.Join(dir, "tuanjie-baselines.json"), Baselines: map[string]*Baseline{}}
	_, err := bs.CollectBaseline(context.Background(),
		&probeTarget{BaseURL: "http://fake.local", Channel: "tuanjie"}, "tuanjie", "glm-5.3-flash", "u1", 60)
	if err == nil || !containsStr(err.Error(), "限流") {
		t.Fatalf("限流中止的采集应返回含「限流」的错误: %v", err)
	}
	if len(bs.Baselines) != 0 {
		t.Fatalf("限流中止的采集不应落盘: %v", bs.Baselines)
	}
	if _, statErr := os.Stat(filepath.Join(dir, "tuanjie-baselines.json")); statErr == nil {
		t.Fatal("限流中止的采集不应写基准文件")
	}
	total := rt.calls.Load()
	t.Logf("限流中止时采集实际请求数 = %d（完整规模为探针 15 调 + 采样 60）", total)
	if total >= 30 {
		t.Fatalf("限流早停后请求数应远小于完整采集规模（<30），got %d", total)
	}
}
