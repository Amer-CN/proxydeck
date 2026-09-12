package tuanjie

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// buildWaterReportItems 四个人话检测项的组装（纯函数，无 IO）。
func TestBuildWaterReportItemsAllMatch(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Account: "38261", SampledAt: "2026-08-25 15:55:50",
		Dist: &distResult{Valid: 60, Insufficient: false, Counts: make([]int, distBuckets)}}
	cmps := []probeCompare{
		{Name: "tokenizer_en", Current: "100", Baseline: "100", Match: true, Comparable: true, Status: "ok"},
		{Name: "finish_stop", Current: "stop", Baseline: "stop", Match: true, Comparable: true, Status: "ok"},
	}
	sim := distSimilarity{Cosine: 0.9, Overall: 0.98, ModeA: 10, ModeB: 10}
	canary := &WaterProbeResult{Answers: map[string]bool{"repeat": true, "math": true, "knowledge": true}}
	items := buildWaterReportItems("tuanjie", canary, cmps, sim, base, false)
	if len(items) != 4 {
		t.Fatalf("应有 4 个检测项，得到 %d: %+v", len(items), items)
	}
	// 第 38 轮：②权重指纹改为参考项（不打 ✔/✖），全匹配场景下为 ℹ
	want := []string{"✔", "ℹ", "✔", "✔"}
	for i, it := range items {
		if it.Result != want[i] {
			t.Errorf("第 %d 项 %s result=%q 应为 %q（detail=%s）", i, it.Name, it.Result, want[i], it.Detail)
		}
	}
}

func TestBuildWaterReportItemsFirstTime(t *testing.T) {
	base := &Baseline{Model: "GLM-X", Account: "38261", SampledAt: "2026-08-26 10:00:00",
		Dist: &distResult{Valid: 60, Insufficient: false, Counts: make([]int, distBuckets)}}
	cmps := []probeCompare{{Name: "tokenizer_en", Current: "100", Baseline: "100", Match: true, Comparable: true, Status: "ok"}}
	sim := distSimilarity{Cosine: 0.9, Overall: 0.99, ModeA: 7, ModeB: 7}
	canary := &WaterProbeResult{Answers: map[string]bool{"repeat": true, "math": true, "knowledge": true}}
	items := buildWaterReportItems("tuanjie", canary, cmps, sim, base, true)
	if len(items) != 4 {
		t.Fatalf("应有 4 个检测项，得到 %d", len(items))
	}
	if items[3].Result != "🆕" {
		t.Errorf("首次检测基准状态应为 🆕，得到 %q", items[3].Result)
	}
	if !strings.Contains(items[3].Detail, "本次建基准，非判定") {
		t.Errorf("首次检测④应注明「本次建基准，非判定」：%s", items[3].Detail)
	}
	/* 第 33 轮：基准口径统一「官方基准 / 无官方基准」，不再按渠道 strong/weak 换口径 */
	if plainVerdictReason("grey") != "本次建基准，非判定（官方基准已自动采集，再次检测起可比对判定）" {
		t.Errorf("grey 灯文案（官方基准口径）不符：%s", plainVerdictReason("grey"))
	}
	if plainVerdictReason("red") != "指纹与官方基准不符，疑似注水" {
		t.Errorf("red 灯文案（官方基准口径）不符：%s", plainVerdictReason("red"))
	}
}

func TestBuildWaterReportItemsMismatchAndNoDist(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Account: "38261", SampledAt: "2026-08-25 15:55:50"} // Dist=nil → 分布不可比
	cmps := []probeCompare{
		{Name: "tokenizer_en", Current: "100", Baseline: "100", Match: true, Comparable: true, Status: "ok"},
		{Name: "tokenizer_cjk", Current: "99", Baseline: "120", Match: false, Comparable: true, Status: "ok"},
		{Name: "finish_stop", Current: "stop", Baseline: "length", Match: false, Comparable: true, Status: "ok"},
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
	if !strings.Contains(items[0].Detail, "官方基准") || strings.Contains(items[0].Detail, "渠道") {
		t.Errorf("①文案应统一官方基准口径且不再按目标渠道换口径：%s", items[0].Detail)
	}
}

// TestReportItemDistReferenceOnly 第 38 轮验收：②权重指纹为参考项——
// 有分布数据时 Result 为 ℹ（不再打 ✔/✖），detail 给出相似度数字并注明
// 「参考项，不参与判定」；无分布数据时为 —。
func TestReportItemDistReferenceOnly(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Channel: "tuanjie", Account: "u1", SampledAt: "2026-09-12 10:00:00",
		Dist: &distResult{Valid: 50, Insufficient: false, Counts: make([]int, distBuckets)}}
	cmps := []probeCompare{
		{Name: "tokenizer_en", Current: "26", Baseline: "26", Match: true, Comparable: true, Status: "ok"},
		{Name: "finish_stop", Current: "stop", Baseline: "stop", Match: true, Comparable: true, Status: "ok"},
	}
	sim := distSimilarity{Cosine: 0.8, Overall: 0.55, ModeA: 217, ModeB: 217}
	items := buildWaterReportItems("tuanjie", nil, cmps, sim, base, false)
	distItem := items[1]
	if distItem.Name != "权重指纹" {
		t.Fatalf("items[1] 应为权重指纹项: %+v", distItem)
	}
	if distItem.Result == "✔" || distItem.Result == "✖" {
		t.Fatalf("② 参考项不得再打 ✔/✖: %+v", distItem)
	}
	if distItem.Result != "ℹ" {
		t.Fatalf("② 有分布数据时应为 ℹ 参考项标记: %+v", distItem)
	}
	if !strings.Contains(distItem.Detail, "参考") {
		t.Fatalf("② detail 应注明参考项: %+v", distItem)
	}
	if !strings.Contains(distItem.Detail, "55%") {
		t.Fatalf("② detail 应给出相似度数字: %+v", distItem)
	}
	// 无分布数据（样本不足/基准无分布）→ —，同样不参与判定
	items2 := buildWaterReportItems("tuanjie", nil, cmps, distSimilarity{}, base, false)
	if items2[1].Result != "—" {
		t.Fatalf("② 无分布数据时应为 —: %+v", items2[1])
	}
}

func TestBuildWaterReportItemsCanaryNil(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Account: "38261", SampledAt: "2026-08-25 15:55:50"}
	items := buildWaterReportItems("tuanjie", nil, nil, distSimilarity{}, base, false)
	if items[2].Result != "⚠" {
		t.Errorf("金丝雀失败应 ⚠，得到 %q", items[2].Result)
	}
}

// TestBuildWaterReportItemsCanaryUnanswered ③能力答题的「未作答」语义：
// 未作答显示「未作答（上游未返回正文）」而不是「与预期不符」；错答与未作答
// 同场时各自如实列出。
func TestBuildWaterReportItemsCanaryUnanswered(t *testing.T) {
	base := &Baseline{Model: "GLM-5.3", Account: "u", SampledAt: "2026-09-11 10:00:00"}
	// math 未作答、knowledge 答错、repeat 正常
	canary := &WaterProbeResult{
		Answers:    map[string]bool{"repeat": true, "knowledge": false},
		Unanswered: map[string]string{"math": "未作答（上游未返回正文：HTTP 429（空响应体））"},
	}
	items := buildWaterReportItems("bai", canary, nil, distSimilarity{}, base, false)
	if items[2].Result != "✖" {
		t.Fatalf("有错答时③应为 ✖: %+v", items[2])
	}
	if !strings.Contains(items[2].Detail, "与预期不符") || !strings.Contains(items[2].Detail, "未作答（上游未返回正文）") {
		t.Fatalf("③应分开如实列错答与未作答：%s", items[2].Detail)
	}
	// 全部未作答：⚠ 且绝不写「与预期不符」
	canary2 := &WaterProbeResult{Unanswered: map[string]string{
		"math": "未作答（上游未返回正文）", "knowledge": "未作答（上游未返回正文）"}}
	items2 := buildWaterReportItems("bai", canary2, nil, distSimilarity{}, base, false)
	if items2[2].Result != "⚠" || strings.Contains(items2[2].Detail, "与预期不符") {
		t.Fatalf("全部未作答应 ⚠ 且不写与预期不符: %+v", items2[2])
	}
	if !strings.Contains(items2[2].Detail, "未作答（上游未返回正文）") {
		t.Fatalf("③未作答文案不符：%s", items2[2].Detail)
	}
}

// TestHandleWaterCheckUnofficialNoBaseline 验收 2：非官方渠道 + 无官方基准 →
// grey「无法判定（缺官方基准）」，不采集、不写库（基线库为空时对 bai 走
// check，断言库仍为空、基准文件未落盘）。
func TestHandleWaterCheckUnofficialNoBaseline(t *testing.T) {
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	defer func() { exeDirOverride = old }()

	bs := &BaselineStore{path: filepath.Join(dir, "tuanjie-baselines.json"), Baselines: map[string]*Baseline{}}
	s := &Server{baselines: bs, water: &WaterCheck{path: filepath.Join(dir, "tuanjie-water.json")}}
	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()

	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 60)

	if w.Code != 200 {
		t.Fatalf("响应码 %d", w.Code)
	}
	var resp struct {
		OK     bool   `json:"ok"`
		Action string `json:"action"`
		Report struct {
			Verdict struct {
				Light  string  `json:"light"`
				Score  float64 `json:"score"`
				Reason string  `json:"reason"`
			} `json:"verdict"`
			Items     []waterReportItem `json:"items"`
			FirstTime bool              `json:"first_time"`
		} `json:"report"`
		Probes []probeResult `json:"probes"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	if !resp.OK || resp.Action != "check" {
		t.Fatalf("应为 ok check 响应: %s", w.Body.String())
	}
	if resp.Report.Verdict.Light != "grey" {
		t.Fatalf("非官方渠道+无官方基准应为 grey，got %q", resp.Report.Verdict.Light)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "无法判定（缺官方基准）") {
		t.Fatalf("reason 应含「无法判定（缺官方基准）」：%q", resp.Report.Verdict.Reason)
	}
	if len(resp.Probes) != 0 {
		t.Fatalf("该路径不应跑探针，got %d 项", len(resp.Probes))
	}
	// 不采集、不写库
	if len(bs.Baselines) != 0 {
		t.Fatalf("基线库应仍为空，got %v", bs.Baselines)
	}
	if _, err := os.Stat(filepath.Join(dir, "tuanjie-baselines.json")); err == nil {
		t.Fatal("基准文件不应被写入")
	}
}

// TestCanaryUnansweredBudgetRetry 验收 5（金丝雀侧）：空正文（finish=length
// 且 content=""）触发一次更大预算（max_tokens=800）重试；重试出正文按答案
// 判对错；重试仍空 → 记「未作答」独立字段而非答错。
func TestCanaryUnansweredBudgetRetry(t *testing.T) {
	oldT, oldB := probeTransport, probeRetryBackoff
	probeTransport = http.DefaultTransport
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	var mu sync.Mutex
	var mathBudgets []int // math 题各次请求的 max_tokens
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Messages []struct {
				Content string `json:"content"`
			} `json:"messages"`
			MaxTokens int `json:"max_tokens"`
		}
		if json.NewDecoder(r.Body).Decode(&body) != nil || len(body.Messages) == 0 {
			w.WriteHeader(400)
			return
		}
		msg := body.Messages[0].Content
		chat := func(content, finish string) {
			w.Header().Set("Content-Type", "application/json")
			_ = json.NewEncoder(w).Encode(map[string]any{
				"usage":   map[string]any{"prompt_tokens": 100, "completion_tokens": 5},
				"choices": []map[string]any{{"finish_reason": finish, "message": map[string]any{"content": content}}},
			})
		}
		switch {
		case strings.Contains(msg, "QX7"): // repeat 指纹题
			chat("QX7-8842 光子 torpedo 0.73 E=mc^2", "stop")
		case strings.Contains(msg, "17×23"): // math 算术题
			mu.Lock()
			mathBudgets = append(mathBudgets, body.MaxTokens)
			n := len(mathBudgets)
			mu.Unlock()
			if n == 1 {
				chat("", "length") // 首次：思考型预算全烧推理段，正文未出
			} else {
				chat("396", "stop") // 加大预算重试 → 正确答案
			}
		case strings.Contains(msg, "国土面积"): // knowledge 常识题
			chat("960", "stop")
		default:
			chat("ok", "stop")
		}
	}))
	defer srv.Close()

	// 场景 A：重试出正文 → 按答案判对，无未作答
	wchk := &WaterCheck{}
	res, err := wchk.ProbeAccountTarget(context.Background(),
		&probeTarget{BaseURL: srv.URL, Channel: "bai"}, "local", "glm-5.3-flash")
	if err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	first, second := mathBudgets[0], mathBudgets[1]
	mu.Unlock()
	if len(mathBudgets) != 2 || second <= first {
		t.Fatalf("空正文应触发一次更大预算重试（首次 %d → 重试 %d）", first, second)
	}
	if second != 800 {
		t.Fatalf("金丝雀重试预算应为 800，got %d", second)
	}
	if !res.Answers["math"] || len(res.Unanswered) != 0 {
		t.Fatalf("重试出正文应判对且无未作答: answers=%v unanswered=%v", res.Answers, res.Unanswered)
	}
	if !res.Pass {
		t.Fatalf("全对且无未作答应 Pass: %+v", res)
	}

	// 场景 B：math 永远空正文 → 未作答（独立字段），不算答错
	srv2 := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body struct {
			Messages []struct {
				Content string `json:"content"`
			} `json:"messages"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		msg := ""
		if len(body.Messages) > 0 {
			msg = body.Messages[0].Content
		}
		w.Header().Set("Content-Type", "application/json")
		finish := "stop"
		content := "ok"
		switch {
		case strings.Contains(msg, "QX7"):
			content, finish = "QX7-8842 光子 torpedo 0.73 E=mc^2", "stop"
		case strings.Contains(msg, "17×23"):
			content, finish = "", "length" // 首次与 800 重试都空
		case strings.Contains(msg, "国土面积"):
			content, finish = "960", "stop"
		}
		_ = json.NewEncoder(w).Encode(map[string]any{
			"usage":   map[string]any{"prompt_tokens": 100, "completion_tokens": 5},
			"choices": []map[string]any{{"finish_reason": finish, "message": map[string]any{"content": content}}},
		})
	}))
	defer srv2.Close()
	res2, err := wchk.ProbeAccountTarget(context.Background(),
		&probeTarget{BaseURL: srv2.URL, Channel: "bai"}, "local", "glm-5.3-flash")
	if err != nil {
		t.Fatal(err)
	}
	if _, answered := res2.Answers["math"]; answered {
		t.Fatalf("未作答不应写进 Answers（不算答错）: %+v", res2.Answers)
	}
	if res2.Unanswered["math"] == "" {
		t.Fatalf("未作答应记独立字段: %+v", res2.Unanswered)
	}
	if res2.Pass {
		t.Fatalf("有未作答不算干净通过: %+v", res2)
	}
}

// ---- 终审第 36 轮·假结论路径验收（handleWaterCheck 级） ----

// fakeProbeRT 按请求体路由的探针假上游（handleWaterCheck 级单测用）：
// channelTarget 给本地渠道的 BaseURL 是写死的 127.0.0.1 端口，起不了真实
// 假服务——全部请求经 probeTransport 替身按 messages 内容应答。
type fakeProbeRT struct {
	route func(content string, temperature float64, maxTokens int) *http.Response
}

func (f *fakeProbeRT) RoundTrip(r *http.Request) (*http.Response, error) {
	body, _ := io.ReadAll(r.Body)
	var b struct {
		Messages []struct {
			Content string `json:"content"`
		} `json:"messages"`
		Temperature float64 `json:"temperature"`
		MaxTokens   int     `json:"max_tokens"`
	}
	_ = json.Unmarshal(body, &b)
	content := ""
	if len(b.Messages) > 0 {
		content = b.Messages[0].Content
	}
	return f.route(content, b.Temperature, b.MaxTokens), nil
}

// chatResp 200 正常回答（usage / finish_reason / content 齐全）。
func chatResp(content, finish string, promptTok int) *http.Response {
	b, _ := json.Marshal(map[string]any{
		"usage":   map[string]any{"prompt_tokens": promptTok, "completion_tokens": 3},
		"choices": []map[string]any{{"finish_reason": finish, "message": map[string]any{"content": content}}},
	})
	return rtResp(200, string(b))
}

// errJSONResp JSON 错误体（探针报错原文口径：状态码 + error 字段）。
func errJSONResp(status int, msg string) *http.Response {
	b, _ := json.Marshal(map[string]any{
		"error": map[string]any{"message": msg, "type": "invalid_request_error"},
	})
	return rtResp(status, string(b))
}

// waterCheckTestServer handleWaterCheck 级单测脚手架：临时目录（渠道 key
// 配置 / 基准文件 / water 文件都落这里）+ glm-5.3-flash 的官方基准
// （tuanjie 渠道、8 探针 ok、分布 50：众数 217）。
func waterCheckTestServer(t *testing.T) *Server {
	t.Helper()
	dir := t.TempDir()
	old := exeDirOverride
	exeDirOverride = func() string { return dir }
	t.Cleanup(func() { exeDirOverride = old })
	if err := os.WriteFile(filepath.Join(dir, "tuanjie-water-channels.json"),
		[]byte(`[{"id":"bai","key":"k"}]`), 0o600); err != nil {
		t.Fatal(err)
	}
	counts := makeCounts()
	counts[217] = 50
	bl := &Baseline{
		Model: "glm-5.3-flash", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion,
		Probes:        goodBaselineProbes(), Dist: distOf(counts, 50),
		SampledAt: "2026-09-11 10:00:00", Account: "u1", SampleCount: 50,
	}
	bs := &BaselineStore{path: filepath.Join(dir, "tuanjie-baselines.json"),
		Baselines: map[string]*Baseline{baselineKey("glm-5.3-flash", "tuanjie"): bl}}
	return &Server{baselines: bs, water: &WaterCheck{path: filepath.Join(dir, "tuanjie-water.json")}}
}

// TestWaterCheckAllProbesErrorNotGreen 终审第 36 轮·假绿验收 1：
// 8 项探针全 error（429 空体）+ 分布达标（同分布 100%）→ 不得判 green，
// reason 不得出现「模型一致」——什么都没测到 ≠ 确认一致（旧版分布相似度
// 达标即判绿，reason 还被覆写成「模型一致，未发现注水」）。
// 限流早停适配（2026-09-12）：全 429 上游会触发包级 429 计数早停，本测要
// 跑满 8 项覆盖「假绿防线」，注入高阈值；退出清零计数 + 还原阈值。
func TestWaterCheckAllProbesErrorNotGreen(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	oldTh := rateLimitStormThreshold
	rateLimitStormThreshold = 1000
	defer func() { rateLimitStormThreshold = oldTh; resetProbeCost() }()

	// 假上游：只有分布采样出数（分布达标），探针/金丝雀请求一律 429 空体
	probeTransport = &fakeProbeRT{route: func(content string, _ float64, _ int) *http.Response {
		if content == distPrompt {
			return chatResp("217", "stop", 20)
		}
		return rtResp(429, "")
	}}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		Probes []probeResult `json:"probes"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：8 项探针确实全 error
	if len(resp.Probes) != 8 {
		t.Fatalf("探针应 8 项, got %d", len(resp.Probes))
	}
	for _, p := range resp.Probes {
		if p.Status != "error" {
			t.Fatalf("场景构造失败：探针 %s 应全 error, got %+v", p.Name, p)
		}
	}
	if resp.Report.Verdict.Light != "grey" {
		t.Fatalf("8 项探针全 error 时分布再达标也应压灰「无法判定」（不得判 green）: %+v", resp.Report.Verdict)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "模型一致") {
		t.Fatalf("reason 不得出现「模型一致」: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "不构成比对结论") {
		t.Fatalf("reason 应如实说明本次不构成比对结论: %q", resp.Report.Verdict.Reason)
	}
}

// TestWaterCheckPartialProbesNotGreen 终审第 36 轮·假绿验收 2：
// tokenizer 4 项整批 unstable（"a" 报 ~88 无法归一化）、报错原文 2 项跨渠道
// 跳过、finish 2 项匹配、分布同分布、金丝雀全对 → 不得判 green，① 身份指纹
// 不得把未测成/跳过的项写成「全部一致」，应如实写「可比 2 项一致」。
func TestWaterCheckPartialProbesNotGreen(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	probeTransport = &fakeProbeRT{route: func(content string, temp float64, _ int) *http.Response {
		switch {
		case content == distPrompt: // 分布采样 → 同分布出数
			return chatResp("217", "stop", 20)
		case content == "a": // "a" 整批污染：报 ~88（正常 13）→ 无法归一化
			return chatResp("a", "stop", 88)
		case strings.Contains(content, "philosophers"): // tokenizer_en
			return chatResp("x", "stop", 39)
		case strings.Contains(content, "团结引擎"): // tokenizer_cjk
			return chatResp("x", "stop", 48)
		case strings.Contains(content, "quicksort"): // tokenizer_code
			return chatResp("x", "stop", 70)
		case strings.Contains(content, "混合文本"): // tokenizer_mixed
			return chatResp("x", "stop", 71)
		case content == "hello" && temp == 2.0: // error_temp2：400 参数校验 = 指纹
			return errJSONResp(400, "gateway B: temperature 参数非法")
		case content == "hello": // error_maxtok
			return errJSONResp(400, "gateway B: max_tokens 参数非法")
		case strings.Contains(content, "回复一个字"): // finish_stop
			return chatResp("好", "stop", 10)
		case strings.Contains(content, "散文"): // finish_length
			return chatResp("秋", "length", 20)
		case strings.Contains(content, "QX7"): // 金丝雀·指令复读
			return chatResp("QX7-8842 光子 torpedo 0.73 E=mc^2", "stop", 100)
		case strings.Contains(content, "17×23"): // 金丝雀·算术
			return chatResp("396", "stop", 100)
		case strings.Contains(content, "国土面积"): // 金丝雀·常识
			return chatResp("960", "stop", 100)
		default:
			return chatResp("ok", "stop", 10)
		}
	}}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
			Items []waterReportItem `json:"items"`
		} `json:"report"`
		Probes []probeResult     `json:"probes"`
		Canary *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：tokenizer 4 项整批 unstable、金丝雀全对
	unstable := 0
	for _, p := range resp.Probes {
		if strings.HasPrefix(p.Name, "tokenizer_") && p.Status == "unstable" {
			unstable++
		}
	}
	if unstable != 4 {
		t.Fatalf("场景构造失败：tokenizer 应 4 项 unstable, got %d (%+v)", unstable, resp.Probes)
	}
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应全对: %+v", resp.Canary)
	}
	if resp.Report.Verdict.Light != "grey" {
		t.Fatalf("tokenizer 整批未测成不得判 green，应压灰「无法判定」: %+v", resp.Report.Verdict)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "模型一致") {
		t.Fatalf("reason 不得出现「模型一致」: %q", resp.Report.Verdict.Reason)
	}
	if len(resp.Report.Items) == 0 || resp.Report.Items[0].Name != "身份指纹" {
		t.Fatalf("① 身份指纹项缺失: %+v", resp.Report.Items)
	}
	fp := resp.Report.Items[0]
	if strings.Contains(fp.Detail, "全部一致") || strings.Contains(fp.Detail, "分词器") {
		t.Fatalf("① 不得声称未测成/跨渠道跳过的项「一致」: %+v", fp)
	}
	if !strings.Contains(fp.Detail, "可比 2 项") || !strings.Contains(fp.Detail, "未纳入比对") || !strings.Contains(fp.Detail, "跨渠道跳过") {
		t.Fatalf("① 应按实际可比项数如实陈述: %+v", fp)
	}
	// 金丝雀全对如实打勾（能力项全对不反过来把身份指纹说成一致）
	if len(resp.Report.Items) < 3 || resp.Report.Items[2].Result != "✔" {
		t.Fatalf("③ 金丝雀全对应如实打勾: %+v", resp.Report.Items)
	}
}

// ---- 终审第 37 轮·条目假红 / 限定语吞没收尾验收 ----

// baselineWithProbeErr 构造「某探针基准侧未测成」的官方基准：其余 7 项 ok
// （质量门槛 7/8 仍过），name 指定项为 error（真实形态：采集当次该探针 429
// 空响应体，Value 无指纹值）。
func baselineWithProbeErr(name string) *Baseline {
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	ps := goodBaselineProbes()
	for i := range ps {
		if ps[i].Name == name {
			ps[i] = probeResult{Name: name, Status: "error", Note: "HTTP 429（空响应体）"}
		}
	}
	return &Baseline{Model: "GLM-5.3", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: ps, Dist: distOf(counts, 50),
		SampledAt: "2026-09-12 10:00:00", Account: "u1", SampleCount: 50}
}

// TestReportItemBaselineSideErrorNotMismatch 必改 1·单测 1（条目假红）：
// 基准侧某探针 status=error、当前侧同项 ok → 该项不计入 ① 的偏离数；
// ① 文案不写「偏离」，而是如实说明该项基准侧未测成/缺失、未纳入比对。
// 旧版按 c.Status=="ok" && !c.Match 统计，Match 零值 false 被当成不匹配，
// 当前侧正常、基准侧没测成的探针被写成「偏离官方基准（疑似换模型）」。
func TestReportItemBaselineSideErrorNotMismatch(t *testing.T) {
	base := baselineWithProbeErr("error_temp2")
	// 当前侧 8 项全 ok、值与基准一致（cmps 走 CompareToBaseline 真实产出）
	cmps, _, v := CompareToBaseline(base, goodBaselineProbes(), base.Dist, "tuanjie")
	// 前置自证：该探针当前侧 ok、基准侧 error → 不可比、Match 是零值 false
	var ec *probeCompare
	for i := range cmps {
		if cmps[i].Name == "error_temp2" {
			ec = &cmps[i]
		}
	}
	if ec == nil || ec.Status != "ok" || ec.Comparable || ec.Match {
		t.Fatalf("场景构造失败：error_temp2 应为当前侧 ok、基准侧未测成（Match 零值）: %+v", ec)
	}
	items := buildWaterReportItems("tuanjie", nil, cmps, distSimilarity{}, base, false)
	fp := items[0]
	if strings.Contains(fp.Detail, "偏离") {
		t.Fatalf("基准侧未测成的项不得写成「偏离官方基准」: %+v", fp)
	}
	if !strings.Contains(fp.Detail, "可比 7 项") {
		t.Fatalf("① 应只按真正可比的 7 项统计: %+v", fp)
	}
	if !strings.Contains(fp.Detail, "1 项基准侧未测成/缺失") || !strings.Contains(fp.Detail, "未纳入比对") {
		t.Fatalf("① 应如实说明该项基准侧未测成、未纳入比对: %+v", fp)
	}
	// 总灯色口径本就正确（CompareToBaseline 的 mismatch 只数可比项）：
	// 可比 7 项全一致 + 1 项基准侧未测成 → grey 局部证据，不构成一致结论
	if v.Light != "grey" {
		t.Fatalf("1 项基准侧未测成时不得判 green（局部证据）: %+v", v)
	}
}

// TestReportItemRealMismatchStillDeviation 必改 1·单测 3（反例，防改过头）：
// 双方都 ok 且值超容差 → 仍判偏离、仍计入 mismatch——① 文案照写「偏离」，
// 总灯色不因 Comparable 改造被洗白。
func TestReportItemRealMismatchStillDeviation(t *testing.T) {
	counts := makeCounts()
	for i := 1; i <= 50; i++ {
		counts[i]++
	}
	base := &Baseline{Model: "GLM-5.3", Channel: "tuanjie", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: goodBaselineProbes(), Dist: distOf(counts, 50)}
	cur := goodBaselineProbes()
	for i := range cur {
		if cur[i].Name == "tokenizer_en" {
			cur[i].Value = "99" // 基准 26，差 73 远超 ±8 容差
		}
	}
	cmps, _, v := CompareToBaseline(base, cur, distOf(counts, 50), "tuanjie")
	items := buildWaterReportItems("tuanjie", nil, cmps, distSimilarity{}, base, false)
	fp := items[0]
	if !strings.Contains(fp.Detail, "偏离") || !strings.Contains(fp.Detail, "1 项") {
		t.Fatalf("真偏离必须仍计入 mismatch 并写「偏离」: %+v", fp)
	}
	if fp.Result != "⚠" {
		t.Fatalf("1 项真偏离的 ① 应 ⚠: %+v", fp)
	}
	if v.Light == "green" {
		t.Fatalf("真偏离不得把总灯色洗白成 green: %+v", v)
	}
}

// TestWaterCheckTemplateDriftKeepsSkipSuffix 必改 2·单测 2（「疑似模板漂移」
// 降级吞限定语）：金丝雀全对、tokenizer 指纹整批超容差漂移、报错原文探针
// 跨渠道跳过、分布同形状 → 红灯降级黄灯「疑似模板漂移」后，reason 覆写
// 必须保留「N 项跨渠道不可比，已跳过」限定语——一句话结论不得声称得比
// 实际测到的多。
func TestWaterCheckTemplateDriftKeepsSkipSuffix(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// 假上游（bai 渠道）：tokenizer 4 项整体平移 +73（远超 ±8 容差）、报错
	// 原文是 B 网关措辞（跨渠道 → 跳过）、finish 与分布同官方基准、金丝雀全对
	probeTransport = &fakeProbeRT{route: func(content string, temp float64, _ int) *http.Response {
		switch {
		case content == distPrompt: // 分布采样 → 与基准同分布（cosine=1）
			return chatResp("217", "stop", 20)
		case content == "a": // "a" 基准正常 13
			return chatResp("a", "stop", 13)
		case strings.Contains(content, "philosophers"): // tokenizer_en 26 → 99
			return chatResp("x", "stop", 112)
		case strings.Contains(content, "团结引擎"): // tokenizer_cjk 35 → 108
			return chatResp("x", "stop", 121)
		case strings.Contains(content, "quicksort"): // tokenizer_code 57 → 130
			return chatResp("x", "stop", 143)
		case strings.Contains(content, "混合文本"): // tokenizer_mixed 58 → 131
			return chatResp("x", "stop", 144)
		case content == "hello" && temp == 2.0: // error_temp2：B 网关措辞
			return errJSONResp(400, "gateway B: temperature 参数非法")
		case content == "hello": // error_maxtok：B 网关措辞
			return errJSONResp(400, "gateway B: max_tokens 参数非法")
		case strings.Contains(content, "回复一个字"): // finish_stop 与基准一致
			return chatResp("好", "stop", 10)
		case strings.Contains(content, "散文"): // finish_length 与基准一致
			return chatResp("秋", "length", 20)
		case strings.Contains(content, "QX7"): // 金丝雀·指令复读
			return chatResp("QX7-8842 光子 torpedo 0.73 E=mc^2", "stop", 100)
		case strings.Contains(content, "17×23"): // 金丝雀·算术
			return chatResp("396", "stop", 100)
		case strings.Contains(content, "国土面积"): // 金丝雀·常识
			return chatResp("960", "stop", 100)
		default:
			return chatResp("ok", "stop", 10)
		}
	}}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		ProbeCompare []probeCompare    `json:"probe_compare"`
		Canary       *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：金丝雀全对、2 项报错原文探针跨渠道跳过
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应全对: %+v", resp.Canary)
	}
	skipN := 0
	for _, c := range resp.ProbeCompare {
		if c.Status == "skip" {
			skipN++
		}
	}
	if skipN != 2 {
		t.Fatalf("场景构造失败：报错原文探针应 2 项跨渠道跳过: %+v", resp.ProbeCompare)
	}
	// 降级必须发生（red → yellow「疑似模板漂移」），且覆写后的 reason 保留
	// 跳过限定语——旧的 red 一句话结论也不得残留
	if resp.Report.Verdict.Light != "yellow" {
		t.Fatalf("金丝雀全对 + 仅 tokenizer 漂移 + 分布同形状应降级 yellow: %+v", resp.Report.Verdict)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "更像上游模板变动") {
		t.Fatalf("降级 reason 应为「疑似模板漂移」口径: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "已跳过") {
		t.Fatalf("降级 reason 覆写不得吞掉「跨渠道不可比，已跳过」限定语: %q", resp.Report.Verdict.Reason)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "指纹与官方基准不符") {
		t.Fatalf("降级后不得残留 red 一句话结论: %q", resp.Report.Verdict.Reason)
	}
}

// driftCheckRoute 「tokenizer 部分漂移」检测场景的假上游路由（bai 渠道，比对
// waterCheckTestServer 预置的 tuanjie 官方基准）：tokenizer_en/cjk 平移 +73
// （2 项超 ±8 容差偏离）、tokenizer_code/mixed 与基准一致、报错原文 B 网关
// 措辞（跨渠道跳过 2 项）、finish_length 恒与基准一致；分布采样、金丝雀算术、
// finish_stop 三处按参数变化，其余与既有模板漂移用例同形。
func driftCheckRoute(distContent, mathContent, finishStop string) func(string, float64, int) *http.Response {
	return func(content string, temp float64, _ int) *http.Response {
		switch {
		case content == distPrompt:
			return chatResp(distContent, "stop", 20)
		case content == "a": // "a" 基准正常 13
			return chatResp("a", "stop", 13)
		case strings.Contains(content, "philosophers"): // tokenizer_en 26 → 99
			return chatResp("x", "stop", 112)
		case strings.Contains(content, "团结引擎"): // tokenizer_cjk 35 → 108
			return chatResp("x", "stop", 121)
		case strings.Contains(content, "quicksort"): // tokenizer_code 57 一致
			return chatResp("x", "stop", 70)
		case strings.Contains(content, "混合文本"): // tokenizer_mixed 58 一致
			return chatResp("x", "stop", 71)
		case content == "hello" && temp == 2.0: // error_temp2：B 网关措辞 → 跨渠道跳过
			return errJSONResp(400, "gateway B: temperature 参数非法")
		case content == "hello": // error_maxtok：B 网关措辞 → 跨渠道跳过
			return errJSONResp(400, "gateway B: max_tokens 参数非法")
		case strings.Contains(content, "回复一个字"): // finish_stop：stop 一致 / length 偏离
			return chatResp("好", finishStop, 10)
		case strings.Contains(content, "散文"): // finish_length 与基准一致
			return chatResp("秋", "length", 20)
		case strings.Contains(content, "QX7"): // 金丝雀·指令复读
			return chatResp("QX7-8842 光子 torpedo 0.73 E=mc^2", "stop", 100)
		case strings.Contains(content, "17×23"): // 金丝雀·算术按参数（396 对 / 42 错）
			return chatResp(mathContent, "stop", 100)
		case strings.Contains(content, "国土面积"): // 金丝雀·常识
			return chatResp("960", "stop", 100)
		default:
			return chatResp("ok", "stop", 10)
		}
	}
}

// TestWaterCheckTemplateDriftLowDistStillDowngrades 降级护栏不再被分布相似度
// 卡住：2 项 tokenizer 偏离 + 金丝雀全对 + 分布相似度极低（余弦 0，远低于旧
// 判据 0.9）→ 仍降级 yellow「疑似模板漂移」。分布指纹已是参考项（同模型
// 拆半自比中位仅 0.552/0.941，判档线落在噪声带内），同样的 tokenizer 偏离
// 不得因一次噪声抽样在红/黄之间随机翻转；降级 reason 不得再拿分布相似度当
// 证据，且仍带 probeCoverageSuffix 限定语。
func TestWaterCheckTemplateDriftLowDistStillDowngrades(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// 分布采样全部落 300 桶（基准全在 217 桶，完全不重叠）→ 余弦 0
	probeTransport = &fakeProbeRT{route: driftCheckRoute("300", "396", "stop")}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		ProbeCompare   []probeCompare    `json:"probe_compare"`
		DistSimilarity distSimilarity    `json:"dist_similarity"`
		Canary         *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：恰 2 项 tokenizer 偏离（无其他偏离）、金丝雀全对、分布相似度极低
	tokMis, otherMis := 0, 0
	for _, c := range resp.ProbeCompare {
		if c.Comparable && !c.Match {
			if strings.HasPrefix(c.Name, "tokenizer_") {
				tokMis++
			} else {
				otherMis++
			}
		}
	}
	if tokMis != 2 || otherMis != 0 {
		t.Fatalf("场景构造失败：应恰 2 项 tokenizer 偏离且无其他偏离, tok=%d other=%d: %+v", tokMis, otherMis, resp.ProbeCompare)
	}
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应全对: %+v", resp.Canary)
	}
	if resp.DistSimilarity.Cosine > 0.2 {
		t.Fatalf("场景构造失败：分布相似度应极低（≤0.2），got %+v", resp.DistSimilarity)
	}
	// 降级必须发生（旧判据 cosine≥0.9 会把这里卡成 red）
	if resp.Report.Verdict.Light != "yellow" {
		t.Fatalf("分布相似度极低不得卡住降级：仅 tokenizer 漂移 + 金丝雀全对应仍降 yellow: %+v", resp.Report.Verdict)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "更像上游模板变动") {
		t.Fatalf("降级 reason 应为「疑似模板漂移」口径: %q", resp.Report.Verdict.Reason)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "分布形状相似度") {
		t.Fatalf("降级 reason 不得再拿分布相似度当证据: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "已跳过") {
		t.Fatalf("降级 reason 必须保留 coverage 限定语: %q", resp.Report.Verdict.Reason)
	}
}

// TestWaterCheckTemplateDriftCanaryWrongStaysRed 金丝雀有错答 → 不降级：
// 同样 2 项 tokenizer 偏离、分布同形状，但金丝雀算术答错 → 降级护栏要求
// 「金丝雀实际作答且全对」不满足，保持 red「疑似注水」。
func TestWaterCheckTemplateDriftCanaryWrongStaysRed(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// 金丝雀算术题答 42（正确 396）→ 有错答
	probeTransport = &fakeProbeRT{route: driftCheckRoute("217", "42", "stop")}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		ProbeCompare []probeCompare    `json:"probe_compare"`
		Canary       *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：恰 2 项 tokenizer 偏离、金丝雀未全对
	tokMis, otherMis := 0, 0
	for _, c := range resp.ProbeCompare {
		if c.Comparable && !c.Match {
			if strings.HasPrefix(c.Name, "tokenizer_") {
				tokMis++
			} else {
				otherMis++
			}
		}
	}
	if tokMis != 2 || otherMis != 0 {
		t.Fatalf("场景构造失败：应恰 2 项 tokenizer 偏离且无其他偏离, tok=%d other=%d: %+v", tokMis, otherMis, resp.ProbeCompare)
	}
	if resp.Canary == nil || resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应有错答（不得 Pass）: %+v", resp.Canary)
	}
	if resp.Report.Verdict.Light != "red" {
		t.Fatalf("金丝雀有错答时不得降级，应保持 red: %+v", resp.Report.Verdict)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "更像上游模板变动") {
		t.Fatalf("金丝雀未全对不得写成模板漂移口径: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "疑似注水") {
		t.Fatalf("应保持 red「疑似注水」口径: %q", resp.Report.Verdict.Reason)
	}
}

// TestWaterCheckTemplateDriftOtherMismatchStaysRed 偏离项含非 tokenizer 系
// → 不降级：2 项 tokenizer 偏离 + 1 项 finish_reason 偏离（otherMis>0）、
// 金丝雀全对、分布同形状（余弦 1）→ 模板漂移护栏只覆盖 tokenizer 系偏离，
// 保持 red「疑似注水」。
func TestWaterCheckTemplateDriftOtherMismatchStaysRed(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// finish_stop 答成 length → 非 tokenizer 系偏离 1 项
	probeTransport = &fakeProbeRT{route: driftCheckRoute("217", "396", "length")}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		ProbeCompare   []probeCompare    `json:"probe_compare"`
		DistSimilarity distSimilarity    `json:"dist_similarity"`
		Canary         *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：2 项 tokenizer 偏离 + 1 项非 tokenizer 偏离、金丝雀全对、分布同形状
	tokMis, otherMis := 0, 0
	for _, c := range resp.ProbeCompare {
		if c.Comparable && !c.Match {
			if strings.HasPrefix(c.Name, "tokenizer_") {
				tokMis++
			} else {
				otherMis++
			}
		}
	}
	if tokMis != 2 || otherMis != 1 {
		t.Fatalf("场景构造失败：应 2 项 tokenizer 偏离 + 1 项非 tokenizer 偏离, tok=%d other=%d: %+v", tokMis, otherMis, resp.ProbeCompare)
	}
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应全对: %+v", resp.Canary)
	}
	if resp.DistSimilarity.Cosine < 0.9 {
		t.Fatalf("场景构造失败：分布应同形状（cosine≥0.9），got %+v", resp.DistSimilarity)
	}
	if resp.Report.Verdict.Light != "red" {
		t.Fatalf("偏离含非 tokenizer 系时不得降级，应保持 red: %+v", resp.Report.Verdict)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "更像上游模板变动") {
		t.Fatalf("含非 tokenizer 偏离不得写成模板漂移口径: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "疑似注水") {
		t.Fatalf("应保持 red「疑似注水」口径: %q", resp.Report.Verdict.Reason)
	}
}

// ---- 漂移形状判据（换模型 vs 真·模板平移）验收 ----

// setBaselineTokenizerValues 就地改写测试服务器预置官方基准的 4 个 tokenizer
// 归一化值（en/cjk/code/mixed 顺序）——漂移形状用例需按真实测得的指纹构造
// 基准侧数值（waterCheckTestServer 预置的是 goodBaselineProbes 的 26/35/57/58）。
func setBaselineTokenizerValues(s *Server, en, cjk, code, mixed string) {
	bl := s.baselines.Baselines[baselineKey("glm-5.3-flash", "tuanjie")]
	for i := range bl.Probes {
		switch bl.Probes[i].Name {
		case "tokenizer_en":
			bl.Probes[i].Value = en
		case "tokenizer_cjk":
			bl.Probes[i].Value = cjk
		case "tokenizer_code":
			bl.Probes[i].Value = code
		case "tokenizer_mixed":
			bl.Probes[i].Value = mixed
		}
	}
}

// shiftedTokenizerRoute 「tokenizer 指纹平移」检测场景的假上游路由（bai 渠道，
// 比对测试服务器预置的官方基准）："a" 基准正常 13，四段 tokenizer 探针按参数
// 报原始 prompt_tokens（归一化值 = raw − 13）；报错原文 B 网关措辞（跨渠道
// 跳过 2 项）；finish_length 与基准一致；分布采样、金丝雀算术、finish_stop
// 三处按参数变化，其余与既有模板漂移用例同形。
func shiftedTokenizerRoute(rawEn, rawCjk, rawCode, rawMixed int, distContent, mathContent, finishStop string) func(string, float64, int) *http.Response {
	return func(content string, temp float64, _ int) *http.Response {
		switch {
		case content == distPrompt:
			return chatResp(distContent, "stop", 20)
		case content == "a": // "a" 基准正常 13
			return chatResp("a", "stop", 13)
		case strings.Contains(content, "philosophers"): // tokenizer_en
			return chatResp("x", "stop", rawEn)
		case strings.Contains(content, "团结引擎"): // tokenizer_cjk
			return chatResp("x", "stop", rawCjk)
		case strings.Contains(content, "quicksort"): // tokenizer_code
			return chatResp("x", "stop", rawCode)
		case strings.Contains(content, "混合文本"): // tokenizer_mixed
			return chatResp("x", "stop", rawMixed)
		case content == "hello" && temp == 2.0: // error_temp2：B 网关措辞 → 跨渠道跳过
			return errJSONResp(400, "gateway B: temperature 参数非法")
		case content == "hello": // error_maxtok：B 网关措辞 → 跨渠道跳过
			return errJSONResp(400, "gateway B: max_tokens 参数非法")
		case strings.Contains(content, "回复一个字"): // finish_stop：stop 一致 / length 偏离
			return chatResp("好", finishStop, 10)
		case strings.Contains(content, "散文"): // finish_length 与基准一致
			return chatResp("秋", "length", 20)
		case strings.Contains(content, "QX7"): // 金丝雀·指令复读
			return chatResp("QX7-8842 光子 torpedo 0.73 E=mc^2", "stop", 100)
		case strings.Contains(content, "17×23"): // 金丝雀·算术按参数（396 对 / 42 错）
			return chatResp(mathContent, "stop", 100)
		case strings.Contains(content, "国土面积"): // 金丝雀·常识
			return chatResp("960", "stop", 100)
		default:
			return chatResp("ok", "stop", 10)
		}
	}
}

// TestWaterCheckTemplateDriftInconsistentDeltaStaysRed 漂移形状判据·换模型
// 形状不降级：KIMI-K3（26/29/58/55）冒充 GLM-5.3-FLASH（26/35/57/63）——四项
// 带符号差值 0/−6/+1/−8，其中 2 项超 ±1 容差判偏离，偏离项 delta（−6/−8）
// 散布 2 不呈整批同量平移（换模型形状）；KIMI-K3 完全能答对算术/常识（金丝雀
// 全对）→ 不得被模板漂移降级护栏吞成 yellow 软放行，保持 red「疑似注水」，
// reason 不得出现「模板漂移」。
func TestWaterCheckTemplateDriftInconsistentDeltaStaysRed(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// 官方基准 = 真实测得的 GLM-5.3-FLASH 指纹 26/35/57/63
	// （goodBaselineProbes 预置 mixed=58，按实测值改写）
	setBaselineTokenizerValues(s, "26", "35", "57", "63")

	// 假上游（bai 渠道）= 真实测得的 KIMI-K3 指纹 26/29/58/55
	// （raw prompt_tokens = 归一化值 + "a" 基准 13），金丝雀全对
	probeTransport = &fakeProbeRT{route: shiftedTokenizerRoute(39, 42, 71, 68, "217", "396", "stop")}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		ProbeCompare []probeCompare    `json:"probe_compare"`
		Canary       *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：恰 2 项 tokenizer 偏离（cjk Δ−6、mixed Δ−8）、无其他偏离、金丝雀全对
	tokMis, otherMis := 0, 0
	for _, c := range resp.ProbeCompare {
		if c.Comparable && !c.Match {
			if strings.HasPrefix(c.Name, "tokenizer_") {
				tokMis++
			} else {
				otherMis++
			}
		}
	}
	if tokMis != 2 || otherMis != 0 {
		t.Fatalf("场景构造失败：应恰 2 项 tokenizer 偏离且无其他偏离, tok=%d other=%d: %+v", tokMis, otherMis, resp.ProbeCompare)
	}
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应全对: %+v", resp.Canary)
	}
	// 换模型形状（偏离项 delta −6/−8 不一致）→ 不降级，保持 red
	if resp.Report.Verdict.Light != "red" {
		t.Fatalf("KIMI-K3 冒充 GLM-5.3-FLASH（偏离 delta −6/−8）+ 金丝雀全对也不得降级: %+v", resp.Report.Verdict)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "模板漂移") || strings.Contains(resp.Report.Verdict.Reason, "更像上游模板变动") {
		t.Fatalf("换模型形状不得写成模板漂移口径: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "疑似注水") {
		t.Fatalf("应保持 red「疑似注水」口径: %q", resp.Report.Verdict.Reason)
	}
}

// TestWaterCheckTemplateDriftUniformShiftDowngrades 漂移形状判据·真·模板
// 平移降级：基准 26/29/58/55、当前 34/37/66/63——四项带符号差值均为 +8，
// 偏离项 delta 集合散布 0（整批同量平移）+ 金丝雀全对 → 降 yellow「疑似模板
// 漂移」，reason 如实写明「整批同量平移 +8、金丝雀全对」，并保留 coverage
// 限定语（报错原文探针跨渠道跳过）。
func TestWaterCheckTemplateDriftUniformShiftDowngrades(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	// 官方基准 26/29/58/55；当前 34/37/66/63（raw = 归一化值 + 13），四项 delta 均 +8
	setBaselineTokenizerValues(s, "26", "29", "58", "55")
	probeTransport = &fakeProbeRT{route: shiftedTokenizerRoute(47, 50, 79, 76, "217", "396", "stop")}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		ProbeCompare []probeCompare    `json:"probe_compare"`
		Canary       *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：4 项 tokenizer 全部偏离且带符号差值均为 +8、无其他偏离、金丝雀全对
	tokMis, otherMis, deltas := 0, 0, []int{}
	for _, c := range resp.ProbeCompare {
		if c.Comparable && !c.Match {
			if strings.HasPrefix(c.Name, "tokenizer_") {
				tokMis++
				cur, _ := strconv.Atoi(c.Current)
				base, _ := strconv.Atoi(c.Baseline)
				deltas = append(deltas, cur-base)
			} else {
				otherMis++
			}
		}
	}
	if tokMis != 4 || otherMis != 0 {
		t.Fatalf("场景构造失败：应 4 项 tokenizer 偏离且无其他偏离, tok=%d other=%d: %+v", tokMis, otherMis, resp.ProbeCompare)
	}
	for _, d := range deltas {
		if d != 8 {
			t.Fatalf("场景构造失败：四项 delta 应均为 +8, got %v: %+v", deltas, resp.ProbeCompare)
		}
	}
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应全对: %+v", resp.Canary)
	}
	// 整批同量平移 + 金丝雀全对 → 降级 yellow，文案如实反映依据
	if resp.Report.Verdict.Light != "yellow" {
		t.Fatalf("整批同量平移（4 项 delta 均 +8）+ 金丝雀全对应降 yellow: %+v", resp.Report.Verdict)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "模板") {
		t.Fatalf("降级 reason 应含「模板」: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "同量平移 +8") || !strings.Contains(resp.Report.Verdict.Reason, "金丝雀全对") {
		t.Fatalf("降级 reason 应如实写明「整批同量平移 +8、金丝雀全对」: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "已跳过") {
		t.Fatalf("降级 reason 必须保留 coverage 限定语: %q", resp.Report.Verdict.Reason)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "指纹与官方基准不符") {
		t.Fatalf("降级后不得残留 red 一句话结论: %q", resp.Report.Verdict.Reason)
	}
}

// TestWaterCheckTemplateDriftUniformShiftCanaryWrongStaysRed 整批同量平移但
// 金丝雀有错答 → 不降级：四项 delta 均 +8 的模板平移形状、分布同形状，但
// 金丝雀算术答错（42，正确 396）→ 降级护栏要求「金丝雀实际作答且全对」
// 不满足，保持 red「疑似注水」。
func TestWaterCheckTemplateDriftUniformShiftCanaryWrongStaysRed(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()

	setBaselineTokenizerValues(s, "26", "29", "58", "55")
	// 同整批平移形状，但金丝雀算术答 42（正确 396）→ 有错答
	probeTransport = &fakeProbeRT{route: shiftedTokenizerRoute(47, 50, 79, 76, "217", "42", "stop")}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		ProbeCompare []probeCompare    `json:"probe_compare"`
		Canary       *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 前置自证：4 项 tokenizer 全部偏离、金丝雀有错答
	tokMis, otherMis := 0, 0
	for _, c := range resp.ProbeCompare {
		if c.Comparable && !c.Match {
			if strings.HasPrefix(c.Name, "tokenizer_") {
				tokMis++
			} else {
				otherMis++
			}
		}
	}
	if tokMis != 4 || otherMis != 0 {
		t.Fatalf("场景构造失败：应 4 项 tokenizer 偏离且无其他偏离, tok=%d other=%d: %+v", tokMis, otherMis, resp.ProbeCompare)
	}
	if resp.Canary == nil || resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应有错答（不得 Pass）: %+v", resp.Canary)
	}
	if resp.Report.Verdict.Light != "red" {
		t.Fatalf("整批平移但金丝雀有错答时不得降级，应保持 red: %+v", resp.Report.Verdict)
	}
	if strings.Contains(resp.Report.Verdict.Reason, "更像上游模板变动") {
		t.Fatalf("金丝雀未全对不得写成模板漂移口径: %q", resp.Report.Verdict.Reason)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "疑似注水") {
		t.Fatalf("应保持 red「疑似注水」口径: %q", resp.Report.Verdict.Reason)
	}
}

// ---- 限流早停（2026-09-12 bai 实测回归修复）验收 ----

// countingRT 计数 + 按内容路由的假上游（限流早停单测用）：calls 记录真实发出
// 的请求总数（含退避重试的每次尝试），route 按请求体内容/参数应答。
type countingRT struct {
	calls atomic.Int32
	route func(content string, temperature float64, maxTokens int, callNo int32) *http.Response
}

func (c *countingRT) RoundTrip(r *http.Request) (*http.Response, error) {
	n := c.calls.Add(1)
	body, _ := io.ReadAll(r.Body)
	var b struct {
		Messages []struct {
			Content string `json:"content"`
		} `json:"messages"`
		Temperature float64 `json:"temperature"`
		MaxTokens   int     `json:"max_tokens"`
	}
	_ = json.Unmarshal(body, &b)
	content := ""
	if len(b.Messages) > 0 {
		content = b.Messages[0].Content
	}
	return c.route(content, b.Temperature, b.MaxTokens, n), nil
}

// TestWaterCheckRateLimitEarlyStop 限流早停验收：前 3 个请求正常（"a" 基准
// 采样）、其后全部 429 空体 → 探针在 429 累计达阈值时提前 break、分布采样
// 一轮不再投递——本次检测发出的请求总数远小于「探针+采样+金丝雀」完整规模
// （<30 而非 75+）；结论 grey 且 reason 含「限流」；不产出任何判定（无比对、
// 无金丝雀）；基准库未被写入。
func TestWaterCheckRateLimitEarlyStop(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0} // 零退避注入：不依赖真实等待
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	defer resetProbeCost() // 包级 429 计数退出清零，防泄漏到后续测试

	rt := &countingRT{route: func(_ string, _ float64, _ int, n int32) *http.Response {
		if n <= 3 { // 前 3 个请求正常（恰为 "a" 基准 3 次采样），其后全部 429 空体
			return chatResp("a", "stop", 13)
		}
		return rtResp(429, "")
	}}
	probeTransport = rt

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 60)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
			Items []waterReportItem `json:"items"`
		} `json:"report"`
		Probes       []probeResult     `json:"probes"`
		ProbeCompare []probeCompare    `json:"probe_compare"`
		Canary       *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	if resp.Report.Verdict.Light != "grey" {
		t.Fatalf("限流早停结论应为 grey，got %q", resp.Report.Verdict.Light)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "限流") {
		t.Fatalf("reason 应含「限流」：%q", resp.Report.Verdict.Reason)
	}
	// 早停生效：请求总数远小于完整规模（探针 15 调 + 采样 60 + 金丝雀 3 的 75+）
	total := rt.calls.Load()
	t.Logf("限流早停时实际请求数 = %d（完整规模为 75+）", total)
	if total == 0 || total >= 30 {
		t.Fatalf("限流早停后请求总数应远小于完整规模（<30），got %d", total)
	}
	if len(resp.Probes) >= 8 {
		t.Fatalf("探针应提前中止（<8 项），got %d 项", len(resp.Probes))
	}
	// 不产出任何判定：无比对结果、不跑金丝雀
	if len(resp.ProbeCompare) != 0 || resp.Canary != nil {
		t.Fatalf("限流早停不应产出比对/金丝雀: cmps=%v canary=%v", resp.ProbeCompare, resp.Canary)
	}
	// 各检测项如实写明因限流未完成
	if len(resp.Report.Items) != 4 {
		t.Fatalf("应有 4 个检测项，got %d", len(resp.Report.Items))
	}
	for _, it := range resp.Report.Items {
		if !strings.Contains(it.Detail, "限流") || !strings.Contains(it.Detail, "未完成") {
			t.Fatalf("检测项 %s 应写明因限流未完成: %+v", it.Name, it)
		}
	}
	// 基准库未被写入：仍只有预置官方基准，基准文件未落盘
	if len(s.baselines.Baselines) != 1 || s.baselines.Get("glm-5.3-flash") == nil {
		t.Fatalf("基准库不应被写入: %v", s.baselines.Baselines)
	}
	if _, err := os.Stat(s.baselines.path); err == nil {
		t.Fatal("基准文件不应被写入")
	}
}

// TestWaterCheckTransient429Completes 限流早停反例：仅 1 次瞬时 429（第 2 个
// 请求，退避重试后恢复）→ 不触发早停，检测照常完成：8 项探针跑满、分布采样
// 满额、金丝雀照跑，请求总数为完整规模量级，结论为正常比对结论（green）。
func TestWaterCheckTransient429Completes(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	defer resetProbeCost()

	rt := &countingRT{route: func(content string, temp float64, _ int, n int32) *http.Response {
		if n == 2 { // 仅第 2 个请求瞬时 429（重试即恢复），累计 1 次远低于阈值
			return rtResp(429, "")
		}
		switch {
		case content == distPrompt: // 分布采样 → 与基准同分布
			return chatResp("217", "stop", 20)
		case content == "a": // "a" 基准正常 13
			return chatResp("a", "stop", 13)
		case strings.Contains(content, "philosophers"): // tokenizer_en 39−13=26
			return chatResp("x", "stop", 39)
		case strings.Contains(content, "团结引擎"): // tokenizer_cjk 48−13=35
			return chatResp("x", "stop", 48)
		case strings.Contains(content, "quicksort"): // tokenizer_code 70−13=57
			return chatResp("x", "stop", 70)
		case strings.Contains(content, "混合文本"): // tokenizer_mixed 71−13=58
			return chatResp("x", "stop", 71)
		case content == "hello" && temp == 2.0: // error_temp2：B 网关措辞
			return errJSONResp(400, "gateway B: temperature 参数非法")
		case content == "hello": // error_maxtok：B 网关措辞
			return errJSONResp(400, "gateway B: max_tokens 参数非法")
		case strings.Contains(content, "回复一个字"): // finish_stop 与基准一致
			return chatResp("好", "stop", 10)
		case strings.Contains(content, "散文"): // finish_length 与基准一致
			return chatResp("秋", "length", 20)
		case strings.Contains(content, "QX7"): // 金丝雀·指令复读
			return chatResp("QX7-8842 光子 torpedo 0.73 E=mc^2", "stop", 100)
		case strings.Contains(content, "17×23"): // 金丝雀·算术
			return chatResp("396", "stop", 100)
		case strings.Contains(content, "国土面积"): // 金丝雀·常识
			return chatResp("960", "stop", 100)
		default:
			return chatResp("ok", "stop", 10)
		}
	}}
	probeTransport = rt

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
		} `json:"report"`
		Probes []probeResult     `json:"probes"`
		Dist   *distResult       `json:"dist"`
		Canary *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	total := rt.calls.Load()
	t.Logf("瞬时 429 反例实际请求数 = %d（完整规模）", total)
	if total < 60 {
		t.Fatalf("瞬时 429 不应触发早停，请求总数应为完整规模量级（≥60），got %d", total)
	}
	if len(resp.Probes) != 8 {
		t.Fatalf("检测应照常完成（8 项探针跑满），got %d 项", len(resp.Probes))
	}
	if strings.Contains(resp.Report.Verdict.Reason, "限流") {
		t.Fatalf("未触发早停时 reason 不应含「限流」：%q", resp.Report.Verdict.Reason)
	}
	if resp.Report.Verdict.Light != "green" {
		t.Fatalf("与官方基准全匹配应为 green: %+v", resp.Report.Verdict)
	}
	if resp.Dist == nil || resp.Dist.Valid != 45 || resp.Dist.RateLimited {
		t.Fatalf("分布采样应满额完成（45 有效样本、非限流中止）: %+v", resp.Dist)
	}
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("金丝雀应照常跑完且全对: %+v", resp.Canary)
	}
}

// ---- 第 39 轮多锚（远端官方锚点 + 多锚比对）验收 ----

// TestWaterCheckMultiAnchorNamesAnchorAndConflictNote 验收 3+4（check 全流程）：
// 同一模型两条官方锚（团结 + tokenrouter，锚间 tokenizer ≥2 项超 ±1 不一致），
// bai 侧探针与团结锚全匹配 → green 且 reason 点名「团结」；报告 items 出现
// 「锚点一致性」独立提示（点名 团结 vs TokenRouter）且灯色不因它改变；
// ④基准状态按匹配锚显示来源渠道。
func TestWaterCheckMultiAnchorNamesAnchorAndConflictNote(t *testing.T) {
	s := waterCheckTestServer(t)
	oldT, oldB := probeTransport, probeRetryBackoff
	probeRetryBackoff = []time.Duration{0, 0, 0}
	defer func() { probeTransport, probeRetryBackoff = oldT, oldB }()
	defer resetProbeCost()

	// 追加 tokenrouter 锚：tokenizer cjk/mixed 与团结锚差超 ±1（锚间不一致形态，
	// 真实 KIMI-K3 形态 29/55 vs 35/58），SampledAt 更新（GetAnchors 排最前）
	trProbes := goodBaselineProbes()
	for i := range trProbes {
		switch trProbes[i].Name {
		case "tokenizer_cjk":
			trProbes[i].Value = "29"
		case "tokenizer_mixed":
			trProbes[i].Value = "55"
		}
	}
	s.baselines.Baselines[baselineKey("glm-5.3-flash", "tokenrouter")] = &Baseline{
		Model: "glm-5.3-flash", Channel: "tokenrouter", Official: true,
		SchemaVersion: baselineSchemaVersion, Probes: trProbes,
		Dist:      s.baselines.Baselines[baselineKey("glm-5.3-flash", "tuanjie")].Dist,
		SampledAt: "2026-09-12 12:00:00", Account: "u2", SampleCount: 50,
	}

	// bai 侧假上游：探针/分布/金丝雀全部与团结锚一致（tokenrouter 锚则 cjk/mixed 偏离）
	probeTransport = &fakeProbeRT{route: func(content string, temp float64, _ int) *http.Response {
		switch {
		case content == distPrompt: // 分布采样 → 与基准同分布
			return chatResp("217", "stop", 20)
		case content == "a": // "a" 基准正常 13
			return chatResp("a", "stop", 13)
		case strings.Contains(content, "philosophers"): // tokenizer_en 26
			return chatResp("x", "stop", 39)
		case strings.Contains(content, "团结引擎"): // tokenizer_cjk 35（团结锚一致 / tokenrouter 锚偏离）
			return chatResp("x", "stop", 48)
		case strings.Contains(content, "quicksort"): // tokenizer_code 57
			return chatResp("x", "stop", 70)
		case strings.Contains(content, "混合文本"): // tokenizer_mixed 58（团结锚一致 / tokenrouter 锚偏离）
			return chatResp("x", "stop", 71)
		case content == "hello" && temp == 2.0: // error_temp2：B 网关措辞 → 跨渠道跳过
			return errJSONResp(400, "gateway B: temperature 参数非法")
		case content == "hello": // error_maxtok：B 网关措辞 → 跨渠道跳过
			return errJSONResp(400, "gateway B: max_tokens 参数非法")
		case strings.Contains(content, "回复一个字"): // finish_stop 一致
			return chatResp("好", "stop", 10)
		case strings.Contains(content, "散文"): // finish_length 一致
			return chatResp("秋", "length", 20)
		case strings.Contains(content, "QX7"): // 金丝雀·指令复读
			return chatResp("QX7-8842 光子 torpedo 0.73 E=mc^2", "stop", 100)
		case strings.Contains(content, "17×23"): // 金丝雀·算术
			return chatResp("396", "stop", 100)
		case strings.Contains(content, "国土面积"): // 金丝雀·常识
			return chatResp("960", "stop", 100)
		default:
			return chatResp("ok", "stop", 10)
		}
	}}

	r := httptest.NewRequest(http.MethodPost, "/water-probe", nil)
	w := httptest.NewRecorder()
	s.handleWaterCheck(w, r, "bai", "local", "glm-5.3-flash", 45)
	if w.Code != 200 {
		t.Fatalf("响应码 %d: %s", w.Code, w.Body.String())
	}
	var resp struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
			Items []waterReportItem `json:"items"`
		} `json:"report"`
		ProbeCompare []probeCompare    `json:"probe_compare"`
		Canary       *WaterProbeResult `json:"canary"`
	}
	if err := json.Unmarshal(w.Body.Bytes(), &resp); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w.Body.String())
	}
	// 与团结锚全匹配（tokenrouter 锚偏离）→ green：与任一锚一致即一致
	if resp.Report.Verdict.Light != "green" {
		t.Fatalf("与团结锚全匹配应 green（与任一锚一致即一致）: %+v", resp.Report.Verdict)
	}
	if !strings.Contains(resp.Report.Verdict.Reason, "团结") || !strings.Contains(resp.Report.Verdict.Reason, "官方基准一致") {
		t.Fatalf("reason 应点名匹配锚（团结）: %q", resp.Report.Verdict.Reason)
	}
	// 「锚点一致性」独立提示：点名 团结 vs TokenRouter，且注明不参与灯色
	var conflictItem *waterReportItem
	for i := range resp.Report.Items {
		if resp.Report.Items[i].Name == "锚点一致性" {
			conflictItem = &resp.Report.Items[i]
		}
	}
	if conflictItem == nil {
		t.Fatalf("锚间 ≥2 项超容差时报告应出现「锚点一致性」提示: %+v", resp.Report.Items)
	}
	if !strings.Contains(conflictItem.Detail, "团结") || !strings.Contains(conflictItem.Detail, "TokenRouter") ||
		!strings.Contains(conflictItem.Detail, "不参与灯色") {
		t.Fatalf("锚间提示应点名两条锚渠道且注明不参与灯色: %+v", conflictItem)
	}
	// 灯色不因锚间提示改变：green 结论保持（对照单锚场景语义）
	if resp.Canary == nil || !resp.Canary.Pass {
		t.Fatalf("场景构造失败：金丝雀应全对: %+v", resp.Canary)
	}
	// ④基准状态按匹配锚显示来源渠道（团结）
	var baseItem *waterReportItem
	for i := range resp.Report.Items {
		if resp.Report.Items[i].Name == "基准状态" {
			baseItem = &resp.Report.Items[i]
		}
	}
	if baseItem == nil || !strings.Contains(baseItem.Detail, "来源渠道 团结") {
		t.Fatalf("④基准状态应显示匹配锚来源渠道（团结）: %+v", baseItem)
	}
	// 单锚对照（删掉 tokenrouter 锚重跑）：结论同为 green——锚间冲突不参与灯色
	delete(s.baselines.Baselines, baselineKey("glm-5.3-flash", "tokenrouter"))
	w2 := httptest.NewRecorder()
	s.handleWaterCheck(w2, r, "bai", "local", "glm-5.3-flash", 45)
	var resp2 struct {
		Report struct {
			Verdict struct {
				Light  string `json:"light"`
				Reason string `json:"reason"`
			} `json:"verdict"`
			Items []waterReportItem `json:"items"`
		} `json:"report"`
	}
	if err := json.Unmarshal(w2.Body.Bytes(), &resp2); err != nil {
		t.Fatalf("响应解析失败: %v %s", err, w2.Body.String())
	}
	if resp2.Report.Verdict.Light != "green" {
		t.Fatalf("单锚（仅团结）同场景结论应仍为 green: %+v", resp2.Report.Verdict)
	}
	for _, it := range resp2.Report.Items {
		if it.Name == "锚点一致性" {
			t.Fatalf("单锚场景不应出现锚间提示（不占位）: %+v", it)
		}
	}
}
