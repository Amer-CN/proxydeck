// failover_test.go —— hy4-preview 限流自动故障转移的单测（第 43 轮）：
// 重置时点解析（真实报文样例）+ 限流窗口状态机（trip/到期自动解除）。
package codebuddy

import (
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

	// 模型对常量（切换目标写死为 deepseek-v4-pro，用户裁决）
	if hy4Primary != "hy4-preview" || hy4Fallback != "deepseek-v4-pro" {
		t.Fatalf("模型对被意外改动: %s -> %s", hy4Primary, hy4Fallback)
	}
}
