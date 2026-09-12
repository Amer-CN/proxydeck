// probes_baseline_test.go —— 注水检测三层升级的单测：
// 探针归一化/脱敏、分布相似度公式（余弦/JS/众数）、基准存取与比对。
// 第 33 轮新增：HTTP 状态码如实化（429 空体不再裸 EOF、退避重试）、
// 空正文「未作答」+ 加大预算重试。
package tuanjie

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
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

// TestBaselineStoreSaveLoad 基准存取往返（键 = 规范化模型名|来源渠道 → 官方锚）。
// 第 39 轮：键改为多锚格式（模型|渠道），同渠道重采覆盖自己。
func TestBaselineStoreSaveLoad(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	bs := LoadBaselines()
	if bs.Get("GLM-5.3") != nil {
		t.Fatal("空库不应有基准")
	}
	// 手工构造一份合格官方基准（8 探针 ok + 分布有效 50）借 save 落盘
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	bl := &Baseline{
		Model:         "GLM-5.3",
		Channel:       "tuanjie",
		Official:      true,
		SchemaVersion: baselineSchemaVersion,
		Probes:        goodBaselineProbes(),
		Dist:          distOf(counts, 50),
		SampledAt:     "2026-08-24 00:00:00",
		Account:       "u1",
		SampleCount:   50,
	}
	bs.Baselines[baselineKey("GLM-5.3", "tuanjie")] = bl
	bs.save()

	// 重新加载：按多锚键（模型|渠道）往返一致（大小写归一命中）
	bs2 := LoadBaselines()
	got := bs2.Get("GLM-5.3")
	if got == nil {
		t.Fatal("落盘后重读应能拿到基准")
	}
	if got.Model != "GLM-5.3" || got.Account != "u1" || got.SampleCount != 50 {
		t.Fatalf("基准字段往返不一致: %+v", got)
	}
	if len(got.Probes) != 8 || got.Probes[0].Value != "26" {
		t.Fatalf("探针值往返不一致: %+v", got.Probes)
	}
	if !got.Official || got.Channel != "tuanjie" {
		t.Fatalf("官方标记/来源渠道往返不一致: %+v", got)
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
	_, _, v := CompareToBaseline(nil, base.Probes, distOf(counts, 50), "tuanjie")
	if v.Light != "grey" {
		t.Fatalf("无基准应为灰灯: %+v", v)
	}

	// 全匹配 + 同分布 → 绿灯
	_, _, v = CompareToBaseline(base, base.Probes, distOf(counts, 50), "tuanjie")
	if v.Light != "green" || v.Label != "一致" {
		t.Fatalf("全匹配应绿灯一致: %+v", v)
	}

	// 两个探针值变了（错误文本/finish 用相等性）→ 红灯（注水嫌疑）
	changed := []probeResult{
		{Name: "tokenizer_en", Value: "99", Status: "ok"}, // 差值 57 > 容差 8
		{Name: "tokenizer_cjk", Value: "20", Status: "ok"},
		{Name: "finish_stop", Value: "length", Status: "ok"}, // 不匹配
	}
	_, _, v = CompareToBaseline(base, changed, distOf(counts, 50), "tuanjie")
	if v.Light != "red" {
		t.Fatalf("两个探针不匹配应红灯: %+v", v)
	}

	// tokenizer 整批平移 1（±1 余量内小抖动）→ 容差内不算 mismatch，仍绿灯。
	// 旧「整批平移 8 仍绿灯」断言已按新语义修正：±8 时代的整批漂移是 "a" 底座
	// 取错返回值（completion_tokens）的伪象，根因修复（底座=prompt_tokens）后
	// 实测 Δ=0，容差已收紧到 ±1（见 tokenizerDriftTolerance 注释）。
	shiftedProbes := []probeResult{
		{Name: "tokenizer_en", Value: "43", Status: "ok"}, // 42+1
		{Name: "tokenizer_cjk", Value: "21", Status: "ok"}, // 20+1
		{Name: "finish_stop", Value: "stop", Status: "ok"},
	}
	_, _, v = CompareToBaseline(base, shiftedProbes, distOf(counts, 50), "tuanjie")
	if v.Light != "green" {
		t.Fatalf("tokenizer 平移 1 应在 ±1 余量内判绿灯: %+v", v)
	}

	// 探针全匹配但分布不同（第 38 轮起分布仅参考）：判别力实验证明判档线落在
	// 抽样噪声带内，分布相似度不再驱动灯色——分布显著不同（overall≈0）也仍 green
	shifted := makeCounts()
	for i := 0; i < 50; i++ {
		shifted[200]++
	}
	_, simShift, vShift := CompareToBaseline(base, base.Probes, distOf(shifted, 50), "tuanjie")
	if simShift.Overall > 0.3 {
		t.Fatalf("前置失败：构造的分布应显著不同（overall≤0.3）: %+v", simShift)
	}
	if vShift.Light != "green" {
		t.Fatalf("分布显著不同但探针全匹配应仍 green（分布仅参考，不参与判定）: %+v", vShift)
	}

	// 样本不足（<40 有效）→ 只看探针，全匹配仍绿但 reason 注明分布不可比
	small := makeCounts(1, 2, 3)
	_, _, v = CompareToBaseline(base, base.Probes, distOf(small, 3), "tuanjie")
	if v.Light != "green" {
		t.Fatalf("样本不足时探针全匹配应仍绿灯: %+v", v)
	}
	if !containsStr(v.Reason, "样本不足") {
		t.Fatalf("reason 应注明分布不可比: %q", v.Reason)
	}
}

// TestCompareToBaselineDistReferenceOnly 第 38 轮验收：分布指纹降为参考项——
// 灯色只由可比探针决定，分布相似度不参与判定：
//   - 分布相似度极低（与基准完全不重叠，overall≈0）但可比探针全匹配 → green，
//     不得因分布低而 red/yellow（判档线落在噪声带内，旧版会把同模型判假红）；
//   - 分布相似度极高（overall≥0.98）但探针 2 项偏离 → 仍 red，不得因分布
//     像而放水（实测两个真不同模型相似度可达 0.981，旧版会假绿）。
func TestCompareToBaselineDistReferenceOnly(t *testing.T) {
	baseCounts := makeCounts()
	for i := 0; i < 50; i++ {
		baseCounts[217]++ // 众数 217：模型在偏爱数上打转的真实形态
	}
	base := &Baseline{Model: "glm-5.3-flash", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: goodBaselineProbes(), Dist: distOf(baseCounts, 50)}

	// 分布相似度极低：本次全部落在 100（与基准 217 完全不重叠）→ cosine 0、
	// 众数分 0；可比探针（tokenizer×4 + finish×2）全匹配 → 仍 green
	lowCounts := makeCounts()
	for i := 0; i < 50; i++ {
		lowCounts[100]++
	}
	_, simLow, vLow := CompareToBaseline(base, goodBaselineProbes(), distOf(lowCounts, 50), "tuanjie")
	if simLow.Overall > 0.3 {
		t.Fatalf("前置失败：构造的分布应极不相似（overall≤0.3）: %+v", simLow)
	}
	if vLow.Light != "green" {
		t.Fatalf("分布极低但探针全匹配应 green（分布不参与判定）: %+v (sim=%+v)", vLow, simLow)
	}

	// 分布相似度极高：与基准同分布（overall=1 ≥0.98）但探针 2 项偏离 → 仍 red
	cur := goodBaselineProbes()
	for i := range cur {
		if cur[i].Name == "tokenizer_en" {
			cur[i].Value = "99" // 基准 26，差 73 远超 ±8 容差
		}
		if cur[i].Name == "finish_stop" {
			cur[i].Value = "length"
		}
	}
	_, simHigh, vHigh := CompareToBaseline(base, cur, distOf(baseCounts, 50), "tuanjie")
	if simHigh.Overall < 0.98 {
		t.Fatalf("前置失败：构造的分布应极相似（overall≥0.98）: %+v", simHigh)
	}
	if vHigh.Light != "red" {
		t.Fatalf("分布极高但 2 项探针偏离仍应 red（不得反向放水）: %+v (sim=%+v)", vHigh, simHigh)
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

// ---- 第 33 轮「测不了如实化」验收用例 ----

// scriptedRT 固定脚本回放响应的计数 RoundTripper（验收 4：429 退避重试次数）。
type scriptedRT struct {
	mu    atomic.Int32
	reply func(calls int32) *http.Response
}

func (s *scriptedRT) RoundTrip(*http.Request) (*http.Response, error) {
	n := s.mu.Add(1)
	return s.reply(n), nil
}

func rtResp(status int, body string) *http.Response {
	return &http.Response{
		StatusCode: status,
		Header:     http.Header{"Content-Type": []string{"application/json"}},
		Body:       io.NopCloser(strings.NewReader(body)),
	}
}

// TestProbeCallHTTPStatusNote 验收 4：先判状态码再解析正文——
//   - 429 空体 → errText 含 HTTP 状态码（如 429）与「空响应体」，不出现裸 "EOF"
//   - 402 非 JSON 体 → errText 含状态码
//   - 400 JSON 错误体 → errText 为脱敏后的 error 字段原文（指纹口径与既有基准兼容）
//   - 429 触发退避重试（计数 RoundTripper 断言重试次数 >1，≤3 次）
func TestProbeCallHTTPStatusNote(t *testing.T) {
	oldT, oldB := probeTransport, probeRetryBackoff
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	probeRetryBackoff = []time.Duration{0, 0, 0}

	// 429 空响应体：重试 3 次（1 首发 + 3 重试 = 4 次请求）
	var rt429 scriptedRT
	rt429.reply = func(int32) *http.Response { return rtResp(429, "") }
	probeTransport = &rt429
	target := &probeTarget{BaseURL: "http://fake.local", Channel: "bai", Headers: map[string]string{}}
	_, _, st, _, _, errText, err := probeCall(context.Background(), target, "m",
		[]map[string]any{{"role": "user", "content": "hi"}}, nil)
	if err != nil {
		t.Fatalf("上游错误响应不是传输失败，err 应为 nil: %v", err)
	}
	if st != 429 {
		t.Fatalf("应如实返回状态码 429，got %d", st)
	}
	if !strings.Contains(errText, "429") || !strings.Contains(errText, "空响应体") {
		t.Fatalf("errText 应带 HTTP 状态码与空响应体说明: %q", errText)
	}
	if strings.Contains(errText, "EOF") || strings.TrimSpace(errText) == "EOF" {
		t.Fatalf("429 空体不得再被吞成裸 EOF: %q", errText)
	}
	if calls := rt429.mu.Load(); calls <= 1 || calls > 4 {
		t.Fatalf("429 应触发退避重试（>1 次请求、≤3 次重试），实际 %d 次", calls)
	}

	// 402 非 JSON 错误体：带状态码
	var rt402 scriptedRT
	rt402.reply = func(int32) *http.Response { return rtResp(402, "payment required") }
	probeTransport = &rt402
	defer resetProbeCost() // probeCall 直测也累加包级 429 计数，退出清零防泄漏到后续测试
	_, _, st2, _, _, te2, _ := probeCall(context.Background(), target, "m",
		[]map[string]any{{"role": "user", "content": "hi"}}, nil)
	if st2 != 402 || !strings.Contains(te2, "402") {
		t.Fatalf("402 应按状态码如实呈现: st=%d te=%q", st2, te2)
	}

	// 400 JSON 错误体：原始错误体脱敏后即指纹值（无 HTTP 前缀，口径与既有基准兼容）
	var rt400 scriptedRT
	rt400.reply = func(int32) *http.Response {
		return rtResp(400, `{"error":{"message":"litellm.BadRequestError: temperature 参数非法"}}`)
	}
	probeTransport = &rt400
	_, _, st3, _, _, te3, _ := probeCall(context.Background(), target, "m",
		[]map[string]any{{"role": "user", "content": "hello"}}, map[string]any{"temperature": 2.0})
	if st3 != 400 {
		t.Fatalf("400 场景状态码不符: %d", st3)
	}
	if te3 != `{"message":"litellm.BadRequestError: temperature 参数非法"}` {
		t.Fatalf("JSON 错误体应原样脱敏为指纹值: %q", te3)
	}
}

// TestProbeHTTPStatusPropagation 非 200 状态码应记进探针结果（402 预算守卫
// 按状态码判定的基础）：RunPipelineProbes 在全 429 上游下，各探针 error 且
// HTTPStatus=429、note 含状态码（不出现裸 EOF）。
// 限流早停适配（2026-09-12）：全 429 上游会触发包级 429 计数早停，本测要
// 跑满 8 项覆盖「状态码透传」，注入高阈值；退出清零计数 + 还原阈值。
func TestProbeHTTPStatusPropagation(t *testing.T) {
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = &scriptedRT{reply: func(int32) *http.Response { return rtResp(429, "") }}
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	oldTh := rateLimitStormThreshold
	rateLimitStormThreshold = 1000
	defer func() { rateLimitStormThreshold = oldTh; resetProbeCost() }()

	target := &probeTarget{BaseURL: "http://fake.local", Channel: "bai", Headers: map[string]string{}}
	probes := RunPipelineProbes(context.Background(), target, "m")
	if len(probes) != 8 {
		t.Fatalf("探针应为 8 项，got %d", len(probes))
	}
	for _, p := range probes {
		if p.Status != "error" {
			t.Fatalf("全 429 上游下探针应为 error: %+v", p)
		}
		if p.HTTPStatus != 429 {
			t.Fatalf("探针应记下 HTTP 429（402 预算守卫按状态码判定）: %+v", p)
		}
		if !strings.Contains(p.Note, "429") {
			t.Fatalf("探针 note 应含 HTTP 状态码: %+v", p)
		}
	}
}

// TestDistUnansweredBudgetRetry 验收 5（分布采样侧）：空正文（finish=length
// 且 content=""）触发一次更大预算（max_tokens=512）重试；重试出数字 → 记入
// 有效样本；重试仍空 → 记「未作答」，不算无效样本。
func TestDistUnansweredBudgetRetry(t *testing.T) {
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = http.DefaultTransport
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// 场景 A：首答空正文（96 预算）→ 512 预算重试出 "217" → 有效样本
	var muA sync.Mutex
	var budgetsA []int
	srvA := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Messages  []struct {
				Content string `json:"content"`
			} `json:"messages"`
			MaxTokens int `json:"max_tokens"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		if len(body.Messages) > 0 && body.Messages[0].Content == distPrompt {
			muA.Lock()
			budgetsA = append(budgetsA, body.MaxTokens)
			n := len(budgetsA)
			muA.Unlock()
			w.Header().Set("Content-Type", "application/json")
			finish, content := "stop", "217"
			if n == 1 {
				finish, content = "length", "" // 首次预算全烧 reasoning
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"usage":   map[string]any{"prompt_tokens": 20, "completion_tokens": 3},
				"choices": []map[string]any{{"finish_reason": finish, "message": map[string]any{"content": content}}},
			})
			return
		}
		w.WriteHeader(400)
	}))
	defer srvA.Close()
	resA := collectDistSamples(context.Background(),
		&probeTarget{BaseURL: srvA.URL, Channel: "bai"}, "glm-5.3-flash", 1)
	muA.Lock()
	first, second := budgetsA[0], budgetsA[1]
	muA.Unlock()
	if len(budgetsA) != 2 || second <= first {
		t.Fatalf("空正文应触发一次更大预算重试（首次 %d → 重试 %d）", first, second)
	}
	if second != 512 {
		t.Fatalf("分布采样重试预算应为 512，got %d", second)
	}
	if resA.Valid != 1 || resA.Unanswered != 0 || resA.Invalid != 0 {
		t.Fatalf("重试出数字应记有效样本: %+v", resA)
	}

	// 场景 B：始终空正文 → 未作答（不是无效样本）
	srvB := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{
			"usage":   map[string]any{"prompt_tokens": 20, "completion_tokens": 96},
			"choices": []map[string]any{{"finish_reason": "length", "message": map[string]any{"content": ""}}},
		})
	}))
	defer srvB.Close()
	resB := collectDistSamples(context.Background(),
		&probeTarget{BaseURL: srvB.URL, Channel: "bai"}, "glm-5.3-flash", 3)
	if resB.Valid != 0 || resB.Invalid != 0 || resB.Unanswered != 3 {
		t.Fatalf("重试仍空应记「未作答」（不进无效样本）: %+v", resB)
	}
}

// TestProbeHTTPOKEmptyBody 终审第 36 轮·残余裸 EOF：HTTP 200 + 空正文不得把
// 裸 "EOF" 写进探针 note（UI 徽章「未测成（原因）」直接渲染 note）——如实写
// 「HTTP 200（空响应体）」，保留状态码。
func TestProbeHTTPOKEmptyBody(t *testing.T) {
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = &scriptedRT{reply: func(int32) *http.Response { return rtResp(200, "") }}
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	target := &probeTarget{BaseURL: "http://fake.local", Channel: "bai", Headers: map[string]string{}}
	// probeCall 直测：200 空正文 → err 文案带状态码与「空响应体」，无裸 EOF
	_, _, st, _, _, _, err := probeCall(context.Background(), target, "m",
		[]map[string]any{{"role": "user", "content": "hi"}}, nil)
	if st != 200 {
		t.Fatalf("状态码应如实为 200, got %d", st)
	}
	if err == nil {
		t.Fatal("200 空正文应返回说明性错误（上游响应体异常）")
	}
	if !strings.Contains(err.Error(), "200") || !strings.Contains(err.Error(), "空响应体") {
		t.Fatalf("err 应含状态码与「空响应体」: %v", err)
	}
	if strings.Contains(err.Error(), "EOF") {
		t.Fatalf("不得再出现裸 EOF: %v", err)
	}

	// 探针级：8 项 note 全部同口径（含状态码与空响应体说明、无裸 EOF）
	probes := RunPipelineProbes(context.Background(), target, "m")
	if len(probes) != 8 {
		t.Fatalf("探针应为 8 项, got %d", len(probes))
	}
	for _, p := range probes {
		if p.Status != "error" {
			t.Fatalf("200 空体上游下探针应为 error: %+v", p)
		}
		if !strings.Contains(p.Note, "200") || !strings.Contains(p.Note, "空响应体") {
			t.Fatalf("note 应含状态码与「空响应体」: %+v", p)
		}
		if strings.Contains(p.Note, "EOF") {
			t.Fatalf("note 不得再出现裸 EOF: %+v", p)
		}
	}
}
