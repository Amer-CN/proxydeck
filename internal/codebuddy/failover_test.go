// failover_test.go —— hy4-preview 限流自动故障转移的单测（第 43 轮）：
// 重置时点解析（真实报文样例）+ 限流窗口状态机（trip/到期自动解除）。
package codebuddy

import (
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// 用户实测的 WorkBuddy 桌面端报错原文（Server Detail 字段）。
const realQuotaBody = `{"code":-32003,"message":"Quota exceeded: 429 您的使用量已超出频率限制，将在 2026-09-02 06:20:02 UTC+8 重置，您也可以切换其他模型继续使用。 (11f467bf84994b82a84ccd849526a192/afa38b36-b7c0-4bd3-91a4-73b50e48713a)","data":{"details":"429 您的使用量已超出频率限制，将在 2026-09-02 06:20:02 UTC+8 重置。","statusCode":429,"code":6000,"category":"quota"}}`

func TestParseQuotaReset(t *testing.T) {
	until := parseQuotaReset(realQuotaBody)
	if until.IsZero() {
		t.Fatal("真实配额报文应解析出重置时点")
	}
	want := time.Date(2026, 9, 2, 6, 20, 2, 0, time.FixedZone("UTC+8", 8*3600))
	if !until.Equal(want) {
		t.Fatalf("重置时点不符: got %v want %v", until, want)
	}
	// 嵌套 details 里的第二处时点不应干扰（取第一处，同值）
	if !strings.Contains(realQuotaBody, "频率限制") {
		t.Fatal("样例应含中文限流特征")
	}

	// 非配额类 429（普通限流/其他错误）不触发
	for _, body := range []string{
		`{"code":11102,"message":"model not found"}`,
		`{"error":"rate limited"}`,
		``,
	} {
		if until := parseQuotaReset(body); !until.IsZero() {
			t.Fatalf("非配额报文不应解析出时点: %q -> %v", body, until)
		}
	}

	// 配额报文但无时点 → 零值（如实透传，不瞎猜窗口）
	if until := parseQuotaReset(`{"code":-32003,"message":"Quota exceeded: 429 频率限制"}`); !until.IsZero() {
		t.Fatal("无时点的配额报文应返回零值")
	}
}

func TestHy4LimitWindow(t *testing.T) {
	s := &Server{}

	if s.hy4Limited() {
		t.Fatal("初始状态不应处于限流")
	}

	// 记录未来窗口 → 限流中
	s.hy4Trip(time.Now().Add(time.Hour))
	if !s.hy4Limited() {
		t.Fatal("窗口内应报告限流")
	}
	// hy4Limited 只读不解除
	if !s.hy4Limited() {
		t.Fatal("窗口内重复检查应保持限流")
	}

	// 窗口过期 → 第一次检查自动解除
	s.hy4Trip(time.Now().Add(-time.Second))
	if s.hy4Limited() {
		t.Fatal("过期窗口应自动解除")
	}
	if s.hy4Limited() {
		t.Fatal("解除后不应再报告限流")
	}

	// 新窗口覆盖旧窗口
	s.hy4Trip(time.Now().Add(-time.Minute))
	s.hy4Trip(time.Now().Add(time.Minute))
	if !s.hy4Limited() {
		t.Fatal("新窗口应覆盖旧窗口")
	}

	// 模型对常量（hy4Fallback 已降级为「缺省值」用途，缺省 fallback 仍是 deepseek-v4-pro）
	if hy4Primary != "hy4-preview" || hy4Fallback != "deepseek-v4-pro" {
		t.Fatalf("模型对被意外改动: %s -> %s", hy4Primary, hy4Fallback)
	}
}

// 配置缺省：文件缺失（或 failPath 为空）→ enabled=true、fallback=deepseek-v4-pro。
func TestFailoverConfigDefaults(t *testing.T) {
	for name, s := range map[string]*Server{
		"no-path": {},
		"no-file": {failPath: filepath.Join(t.TempDir(), "codebuddy-failover.json")},
	} {
		s.loadFailover()
		if !s.failEnabled {
			t.Fatalf("%s: 缺省应为开启", name)
		}
		if s.failFallback != "deepseek-v4-pro" {
			t.Fatalf("%s: 缺省 fallback 应为 deepseek-v4-pro, got %q", name, s.failFallback)
		}
	}

	// 损坏文件 → 缺省值
	s := &Server{failPath: filepath.Join(t.TempDir(), "codebuddy-failover.json")}
	if err := os.WriteFile(s.failPath, []byte("{broken"), 0o644); err != nil {
		t.Fatal(err)
	}
	s.loadFailover()
	if !s.failEnabled || s.failFallback != "deepseek-v4-pro" {
		t.Fatalf("损坏文件应回退缺省值: enabled=%v fallback=%q", s.failEnabled, s.failFallback)
	}
}

// 配置往返：enabled=false + fallback=glm-5.3 → save → load 回读一致。
func TestFailoverConfigRoundtrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "codebuddy-failover.json")
	s := &Server{failPath: path}
	s.loadFailover()
	s.failMu.Lock()
	s.failEnabled = false
	s.failFallback = "glm-5.3"
	s.failMu.Unlock()
	s.saveFailover()

	s2 := &Server{failPath: path}
	s2.loadFailover()
	if s2.failEnabled {
		t.Fatal("回读 enabled 应为 false")
	}
	if s2.failFallback != "glm-5.3" {
		t.Fatalf("回读 fallback 应为 glm-5.3, got %q", s2.failFallback)
	}
}

// 守卫纯逻辑：disabled 时不应改写/不应 trip；enabled 时按窗口与 429 报文判定。
func TestFailoverDecide(t *testing.T) {
	on := failoverConfig{enabled: true, fallback: "deepseek-v4-pro"}
	off := failoverConfig{enabled: false, fallback: "deepseek-v4-pro"}

	// disabled：窗口内也不改写，429 也不 trip（如实透传）
	if d := failoverDecide(off, hy4Primary, true, 0, ""); d.rewrite || d.trip {
		t.Fatalf("disabled 不应改写: %+v", d)
	}
	if d := failoverDecide(off, hy4Primary, true, http.StatusTooManyRequests, realQuotaBody); d.rewrite || d.trip {
		t.Fatalf("disabled 不应 trip: %+v", d)
	}

	// enabled + 窗口内 hy4-preview → 改写，不 trip
	d := failoverDecide(on, hy4Primary, true, 0, "")
	if !d.rewrite || d.trip {
		t.Fatalf("窗口内应改写且不 trip: %+v", d)
	}
	// enabled + 窗口外 → 无动作
	if d := failoverDecide(on, hy4Primary, false, 0, ""); d.rewrite || d.trip {
		t.Fatalf("窗口外不应有动作: %+v", d)
	}

	// enabled + 429 配额报文 → trip 且 until 来自报文；不 rewrite（改写在请求前已判定）
	d = failoverDecide(on, hy4Primary, false, http.StatusTooManyRequests, realQuotaBody)
	if !d.trip || d.until.IsZero() {
		t.Fatalf("配额 429 应 trip: %+v", d)
	}
	if want := time.Date(2026, 9, 2, 6, 20, 2, 0, time.FixedZone("UTC+8", 8*3600)); !d.until.Equal(want) {
		t.Fatalf("trip until 不符: got %v want %v", d.until, want)
	}

	// enabled + 429 非配额报文 → 不 trip（如实透传）
	if d := failoverDecide(on, hy4Primary, false, http.StatusTooManyRequests, `{"error":"rate limited"}`); d.trip {
		t.Fatalf("非配额 429 不应 trip: %+v", d)
	}

	// 非 hy4 模型：改写与 trip 都不触发
	if d := failoverDecide(on, "kimi-k3", true, http.StatusTooManyRequests, realQuotaBody); d.rewrite || d.trip {
		t.Fatalf("非 hy4 模型不应触发: %+v", d)
	}
}

// TestFailoverPostValidation 穿透 handler 的校验回归（第 43 轮勘误的守门测试）：
// 曾因 valid := f != "" 初始化，任何非空 fallback 都绕过候选池校验直写入配置。
func TestFailoverPostValidation(t *testing.T) {
	s := &Server{}
	mux := http.NewServeMux()
	mux.HandleFunc("/v1/failover", s.handleFailover)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	post := func(body string) int {
		resp, err := http.Post(srv.URL+"/v1/failover", "application/json", strings.NewReader(body))
		if err != nil {
			t.Fatal(err)
		}
		defer resp.Body.Close()
		io.Copy(io.Discard, resp.Body)
		return resp.StatusCode
	}

	// 非法 fallback（非空但不在候选池）必须 400 且不落配置
	if got := post(`{"fallback":"xyz-abc"}`); got != 400 {
		t.Fatalf("非法 fallback 应 400，got %d", got)
	}
	if cfg := s.failoverConfig(); cfg.fallback == "xyz-abc" {
		t.Fatal("非法 fallback 不应写入配置")
	}

	// 合法 fallback 200 并生效
	if got := post(`{"fallback":"glm-5.3"}`); got != 200 {
		t.Fatalf("合法 fallback 应 200，got %d", got)
	}
	if cfg := s.failoverConfig(); cfg.fallback != "glm-5.3" {
		t.Fatalf("合法 fallback 应生效，got %q", cfg.fallback)
	}

	// 空字符串同样 400
	if got := post(`{"fallback":""}`); got != 400 {
		t.Fatalf("空 fallback 应 400，got %d", got)
	}
}
