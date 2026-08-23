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

// TestIsPacingModel 仅 model 名含 "KIMI"（不分大小写）的 chat 请求走节奏器；
// GLM/codely 等一律直通零影响（验收标准 4 的判定函数）。
func TestIsPacingModel(t *testing.T) {
	cases := map[string]bool{
		"KIMI-K3":                 true,
		"kimi-k3":                 true,
		"kimi/K3-thinking-public": true,
		"GLM-5.3":                 false,
		"glm-5.3":                 false,
		"codely-core":             false,
		"codely-vl-public":        false,
		"deepseek-v4-flash-0731":  false,
		"?":                       false,
		"":                        false,
	}
	for model, want := range cases {
		if got := IsPacingModel(model); got != want {
			t.Fatalf("IsPacingModel(%q)=%v, want %v", model, got, want)
		}
	}
}

// TestPacingGate 验收标准 3：enabled=false 时代码路径短路——
// handleChat 的判定 `pacer.Enabled() && IsPacingModel(model)` 为 false，
// 不排队、不重发，行为与无节奏器版本完全一致。
func TestPacingGate(t *testing.T) {
	p := &Pacer{}
	if p.Enabled() && IsPacingModel("KIMI-K3") {
		t.Fatal("关闭时 KIMI 请求也必须短路直通")
	}
	p.enabled.Store(true) // 直接置位，避免落盘
	if !(p.Enabled() && IsPacingModel("KIMI-K3")) {
		t.Fatal("开启且 KIMI 请求应进节奏器")
	}
	if p.Enabled() && IsPacingModel("GLM-5.3") {
		t.Fatal("开启但非 KIMI 请求也必须直通")
	}
}

// TestEstimateTokens token 估算 = len(body)/3（中英混合粗估）。
func TestEstimateTokens(t *testing.T) {
	if got := EstimateTokens([]byte(strings.Repeat("a", 3000))); got != 1000 {
		t.Fatalf("EstimateTokens(3000B)=%d, want 1000", got)
	}
	if got := EstimateTokens(nil); got != 0 {
		t.Fatalf("EstimateTokens(nil)=%d, want 0", got)
	}
}

// near 断言时长在期望值 ±100ms 内（换算含浮点，避免严格相等）。
func near(t *testing.T, got, want time.Duration) {
	t.Helper()
	if d := got - want; d > 100*time.Millisecond || d < -100*time.Millisecond {
		t.Fatalf("时长=%v, want≈%v", got, want)
	}
}

// TestWaitSeconds 等待换算（纯函数）：预算足够立即放行；缺口按
// (缺口 token)/(上游 cap/60) 秒换算（简报示例：缺 60K → 18s）。
func TestWaitSeconds(t *testing.T) {
	// 预算充足（est+margin ≤ 180000-used）→ 0
	near(t, WaitSeconds(50000, 0), 0)
	near(t, WaitSeconds(150000, 10000), 0)
	// 缺 60K（est=120K, used=100K → 预算 80K, 缺 60K）→ 18s
	near(t, WaitSeconds(120000, 100000), 18*time.Second)
	// 单请求超有效预算（est=250K, used=0 → 缺 90K）→ 27s
	near(t, WaitSeconds(250000, 0), 27*time.Second)
	// 窗口几乎打满（est=250K, used=170K → 缺 260K）→ 78s
	near(t, WaitSeconds(250000, 170000), 78*time.Second)
}

// TestRateLimitWait 429 错误体解析换算（纯函数）：匹配 used/cap 按超出量
// /(cap/60)+5s；解析失败固定 65s；used 未超 cap 时只加固定 5s。
func TestRateLimitWait(t *testing.T) {
	// 超 30K → 30K/(200000/60)+5 = 9+5 = 14s
	near(t, RateLimitWait(`{"error":"rate limit exceeded used=230000, cap=200000"}`), 14*time.Second)
	// 未超 cap → 0+5 = 5s
	near(t, RateLimitWait("used=100000, cap=200000"), 5*time.Second)
	// 解析失败 → 固定 65s
	near(t, RateLimitWait("too many requests"), 65*time.Second)
	near(t, RateLimitWait(""), 65*time.Second)
	near(t, RateLimitWait("used=abc, cap=200000"), 65*time.Second)
	near(t, RateLimitWait("used=100, cap=0"), 65*time.Second)
}

// TestPacerWindow 滑窗记账：窗口内求和、窗口外（>60s）记录被清理。
func TestPacerWindow(t *testing.T) {
	p := &Pacer{}
	now := time.Now()
	p.recordAt(100000, now.Add(-90*time.Second)) // 窗口外
	p.recordAt(50000, now.Add(-30*time.Second))  // 窗口内
	p.recordAt(30000, now.Add(-1*time.Second))   // 窗口内
	if got := p.windowUsedAt(now); got != 80000 {
		t.Fatalf("窗口内已记 token=%d, want 80000", got)
	}
	if n := len(p.window); n != 2 {
		t.Fatalf("窗口外记录未清理, 剩 %d 条, want 2", n)
	}
	if got := p.WindowUsed(); got != 80000 {
		t.Fatalf("WindowUsed()=%d, want 80000", got)
	}
}

// TestPacerAcquireImmediate 预算足够时 Acquire 立即放行并入账滑窗
// （验收标准 4：预算不足才挂起等待；此处覆盖充足分支，不发网络请求）。
func TestPacerAcquireImmediate(t *testing.T) {
	p := &Pacer{}
	p.enabled.Store(true)
	before := time.Now()
	deadline := p.Acquire(context.Background(), 1000)
	if time.Since(before) > time.Second {
		t.Fatal("预算充足时 Acquire 不应等待")
	}
	if !time.Now().Before(deadline) {
		t.Fatal("应返回未来 30 分钟的总预算截止时刻")
	}
	if got := p.WindowUsed(); got != 1000 {
		t.Fatalf("放行后滑窗已记 token=%d, want 1000", got)
	}
}

// TestKimiPacingEndpoint 开关 API（httptest，不发真实网络请求）：
// GET 返回状态；POST 切换后 GET 反映变化；非法 POST 返回 400。
func TestKimiPacingEndpoint(t *testing.T) {
	s := &Server{pacer: &Pacer{}}
	get := func() (int, map[string]any) {
		w := httptest.NewRecorder()
		s.handleKimiPacing(w, httptest.NewRequest(http.MethodGet, "/kimi-pacing", nil))
		var m map[string]any
		_ = json.NewDecoder(w.Body).Decode(&m)
		return w.Code, m
	}
	if code, m := get(); code != 200 || m["enabled"] != false {
		t.Fatalf("初始 GET: code=%d body=%v, want enabled=false", code, m)
	}
	w := httptest.NewRecorder()
	s.handleKimiPacing(w, httptest.NewRequest(http.MethodPost, "/kimi-pacing", strings.NewReader(`{"enabled":true}`)))
	if w.Code != 200 {
		t.Fatalf("POST 切换: code=%d, want 200", w.Code)
	}
	if code, m := get(); code != 200 || m["enabled"] != true {
		t.Fatalf("切换后 GET: code=%d body=%v, want enabled=true", code, m)
	}
	w = httptest.NewRecorder()
	s.handleKimiPacing(w, httptest.NewRequest(http.MethodPost, "/kimi-pacing", strings.NewReader(`not json`)))
	if w.Code != 400 {
		t.Fatalf("非法 POST: code=%d, want 400", w.Code)
	}
}

// TestPacerPersistence 开关持久化：SetEnabled 落盘 tuanjie-pacing.json，
// 新实例（模拟重启）load 后状态保持（验收标准 2 的持久化部分）。
func TestPacerPersistence(t *testing.T) {
	path := filepath.Join(t.TempDir(), "tuanjie-pacing.json")
	p := &Pacer{path: path}
	p.SetEnabled(true)
	if !p.Enabled() {
		t.Fatal("SetEnabled(true) 后应为开")
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("开关未落盘: %v", err)
	}
	// 模拟进程重启：同路径新实例读回状态
	p2 := &Pacer{path: path}
	p2.load()
	if !p2.Enabled() {
		t.Fatal("重启后开关状态丢失")
	}
	p2.SetEnabled(false)
	p3 := &Pacer{path: path}
	p3.load()
	if p3.Enabled() {
		t.Fatal("重启后应恢复为关")
	}
}
