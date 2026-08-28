package tuanjie

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// TestIsPacingModel 仅 model 名含 "KIMI"（不分大小写）的 chat 请求走 429 兜底；
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
// 不重发，行为与无兜底版本完全一致。
func TestPacingGate(t *testing.T) {
	p := &Pacer{}
	if p.Enabled() && IsPacingModel("KIMI-K3") {
		t.Fatal("关闭时 KIMI 请求也必须短路直通")
	}
	p.enabled.Store(true) // 直接置位，避免落盘
	if !(p.Enabled() && IsPacingModel("KIMI-K3")) {
		t.Fatal("开启且 KIMI 请求应进 429 兜底")
	}
	if p.Enabled() && IsPacingModel("GLM-5.3") {
		t.Fatal("开启但非 KIMI 请求也必须直通")
	}
}

// TestRetryWait Retry-After 头解析（纯函数）：TrimSpace 后 >0 整数秒 → 该秒数；
// 空/非数字/0/负数 → 缺省 60s（一个窗口）。
func TestRetryWait(t *testing.T) {
	cases := map[string]time.Duration{
		"60":  60 * time.Second,
		"90":  90 * time.Second,
		"":    60 * time.Second,
		"abc": 60 * time.Second,
		"0":   60 * time.Second,
		"-5":  60 * time.Second,
	}
	for in, want := range cases {
		if got := RetryWait(in); got != want {
			t.Fatalf("RetryWait(%q)=%v, want %v", in, got, want)
		}
	}
}

// TestKimiPacingEndpoint 开关 API（httptest，不发真实网络请求）：
// GET 返回 enabled/pending；POST 切换后 GET 反映变化；非法 POST 返回 400。
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
