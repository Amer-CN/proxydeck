// accounts_resume_test.go —— 账号池 402 自动恢复：5h 窗口到点复查官网额度，
// 已回清标记重新入池；月窗耗尽顺延 nextReset；查询失败顺延 5 分钟；
// 手动停用（Enabled=false）的账号只清标记、绝不复活。
package tuanjie

import (
	"context"
	"strings"
	"testing"
	"time"
)

// 用例 1：标记 + 到点 + 额度已回 → 清 BudgetExceeded 与 BudgetResetAt（重新入池
// 由 Pick 现有的 Enabled && !BudgetExceeded 过滤天然生效），并留恢复日志。
func TestResumeScanOnce_QuotaBackClearsMark(t *testing.T) {
	p := newTestPool(&Account{
		UserID: "u1", Enabled: true, AccessToken: "tok",
		BudgetExceeded: true, BudgetResetAt: time.Now().Add(-time.Minute),
	})
	calls := 0
	out := captureLog(t, func() {
		p.resumeScanOnce(func(userID string) (float64, time.Time, bool) {
			calls++
			if userID != "u1" {
				t.Fatalf("queryQuota userID = %q, want u1", userID)
			}
			return 123, time.Now().Add(5 * time.Hour), true
		})
	})
	if calls != 1 {
		t.Fatalf("queryQuota 被调 %d 次, want 1", calls)
	}
	a := p.Get("u1")
	if a == nil || a.BudgetExceeded {
		t.Fatalf("额度已回应清 BudgetExceeded，实际 account=%+v", a)
	}
	if !a.BudgetResetAt.IsZero() {
		t.Errorf("BudgetResetAt = %v, want 零值", a.BudgetResetAt)
	}
	if !containsAll(out, "额度已恢复", "自动恢复轮询") {
		t.Errorf("缺少恢复日志，实际输出: %q", out)
	}
}

// 用例 2：到点但 remaining<=0 且有 nextReset（月窗耗尽场景）→ 更新 resetAt 不清标记。
func TestResumeScanOnce_MonthExhaustedDefersToNextReset(t *testing.T) {
	p := newTestPool(&Account{UserID: "u1", Enabled: true, BudgetExceeded: true, BudgetResetAt: time.Now().Add(-time.Minute)})
	next := time.Now().Add(3 * time.Hour)
	p.resumeScanOnce(func(string) (float64, time.Time, bool) {
		return 0, next, true
	})
	a := p.Get("u1")
	if !a.BudgetExceeded {
		t.Fatal("额度未回不应清 BudgetExceeded")
	}
	if !a.BudgetResetAt.Equal(next) {
		t.Errorf("BudgetResetAt = %v, want 顺延到 nextReset %v", a.BudgetResetAt, next)
	}
}

// 用例 2 补充：到点但没拿到新 nextReset → BudgetResetAt 设 now+5min，不留过去防快扫死循环。
func TestResumeScanOnce_NoNextResetFallsBackTo5min(t *testing.T) {
	p := newTestPool(&Account{UserID: "u1", Enabled: true, BudgetExceeded: true, BudgetResetAt: time.Now().Add(-time.Minute)})
	before := time.Now()
	p.resumeScanOnce(func(string) (float64, time.Time, bool) {
		return 0, time.Time{}, true
	})
	a := p.Get("u1")
	if !a.BudgetExceeded {
		t.Fatal("额度未回不应清 BudgetExceeded")
	}
	if !a.BudgetResetAt.After(before.Add(4*time.Minute)) || a.BudgetResetAt.After(before.Add(6*time.Minute)) {
		t.Errorf("BudgetResetAt = %v, want now+5min 附近（基准 %v）", a.BudgetResetAt, before)
	}
}

// 用例 2 补充：查询失败（ok=false）→ BudgetResetAt 顺延 5 分钟重试。
func TestResumeScanOnce_QueryFailDefers5min(t *testing.T) {
	p := newTestPool(&Account{UserID: "u1", Enabled: true, BudgetExceeded: true, BudgetResetAt: time.Now().Add(-time.Minute)})
	before := time.Now()
	p.resumeScanOnce(func(string) (float64, time.Time, bool) {
		return 0, time.Time{}, false
	})
	a := p.Get("u1")
	if !a.BudgetExceeded {
		t.Fatal("查询失败不应清 BudgetExceeded")
	}
	if !a.BudgetResetAt.After(before.Add(4*time.Minute)) || a.BudgetResetAt.After(before.Add(6*time.Minute)) {
		t.Errorf("BudgetResetAt = %v, want now+5min 附近（基准 %v）", a.BudgetResetAt, before)
	}
}

// 用例 3：手动 Toggle off 的账号（Enabled=false）即使 BudgetResetAt 到点也不复活
// ——恢复只看 BudgetExceeded，额度已回只清标记，绝不把 Enabled 置回 true。
func TestResumeScanOnce_NeverReEnablesDisabled(t *testing.T) {
	p := newTestPool(&Account{UserID: "u1", Enabled: false, BudgetExceeded: true, BudgetResetAt: time.Now().Add(-time.Minute)})
	p.resumeScanOnce(func(string) (float64, time.Time, bool) {
		return 100, time.Time{}, true
	})
	a := p.Get("u1")
	if a.Enabled {
		t.Fatal("自动恢复不得把 Enabled 置回 true（手动停用不受影响）")
	}
	if a.BudgetExceeded {
		t.Error("额度已回应清 BudgetExceeded（是否入池仍由 Enabled 决定）")
	}
}

// 未到点（BudgetResetAt 在未来）与零值 BudgetResetAt（查询失败的 402 标记）
// 都不参与本轮复查。
func TestResumeScanOnce_SkipsNotDueAndZeroResetAt(t *testing.T) {
	p := newTestPool(
		&Account{UserID: "future", Enabled: true, BudgetExceeded: true, BudgetResetAt: time.Now().Add(time.Hour)},
		&Account{UserID: "zero", Enabled: true, BudgetExceeded: true},
	)
	calls := 0
	p.resumeScanOnce(func(string) (float64, time.Time, bool) {
		calls++
		return 0, time.Time{}, false
	})
	if calls != 0 {
		t.Fatalf("未到点/零值 resetAt 不应复查，queryQuota 被调 %d 次", calls)
	}
}

// MarkBudgetExceededAt 写入 resetAt；MarkBudgetExceeded（零值 resetAt）行为不变：
// 只置 BudgetExceeded，resetAt 留零值 = 不参与自动恢复。
func TestMarkBudgetExceededAt_SetsResetAt(t *testing.T) {
	p := newTestPool(&Account{UserID: "u1", Enabled: true})
	at := time.Now().Add(5 * time.Hour)
	p.MarkBudgetExceededAt("u1", at)
	a := p.Get("u1")
	if !a.BudgetExceeded || !a.BudgetResetAt.Equal(at) {
		t.Fatalf("MarkBudgetExceededAt 后 BudgetExceeded=%v BudgetResetAt=%v, want true/%v", a.BudgetExceeded, a.BudgetResetAt, at)
	}
	p.MarkBudgetExceeded("u1")
	a = p.Get("u1")
	if !a.BudgetExceeded || !a.BudgetResetAt.IsZero() {
		t.Fatalf("MarkBudgetExceeded 后 BudgetExceeded=%v BudgetResetAt=%v, want true/零值", a.BudgetExceeded, a.BudgetResetAt)
	}
}

// parseQuota5h 解析规则：usage_5h 窗口优先；窗口剩余<=0 时用 EffectiveRemaining
// 判断是否月窗耗尽；resetAt 按 PeriodEnd(RFC3339) → NextBoundaryAt → now+5h 兜底；
// 查询失败（Source=none）返回 ok=false。
func TestParseQuota5h(t *testing.T) {
	// 5h 窗口耗尽但综合剩余>0 → remaining 取综合剩余；resetAt 取窗口 PeriodEnd
	q := &Quota{Source: "live", Plan: PlanQuota{
		EffectiveRemaining: 42,
		NextBoundaryAt:     "2026-08-30T20:00:00Z",
		Windows: []PlanWindow{
			{Type: "usage_5h", Remaining: 0, PeriodEnd: "2026-08-30T15:00:00Z"},
			{Type: "subscription_month", Remaining: 100},
		},
	}}
	rem, reset, ok := parseQuota5h(q)
	if !ok || rem != 42 {
		t.Fatalf("rem=%v ok=%v, want 42/true（窗口耗尽时用 EffectiveRemaining）", rem, ok)
	}
	want, _ := time.Parse(time.RFC3339, "2026-08-30T15:00:00Z")
	if !reset.Equal(want) {
		t.Fatalf("reset=%v, want %v（usage_5h PeriodEnd）", reset, want)
	}

	// 5h 窗口有剩余 → remaining 取窗口剩余
	q2 := &Quota{Source: "cache", Plan: PlanQuota{EffectiveRemaining: 7,
		Windows: []PlanWindow{{Type: "usage_5h", Remaining: 9, PeriodEnd: "2026-08-30 15:00:05"}}}}
	rem2, reset2, ok2 := parseQuota5h(q2)
	if !ok2 || rem2 != 9 {
		t.Fatalf("rem=%v ok=%v, want 9/true（窗口有剩余取窗口值）", rem2, ok2)
	}
	want2, _ := time.Parse("2006-01-02 15:04:05", "2026-08-30 15:00:05")
	if !reset2.Equal(want2) {
		t.Fatalf("reset=%v, want %v（非 RFC3339 布局回退解析）", reset2, want2)
	}

	// 无 usage_5h 窗口 → resetAt 退到 NextBoundaryAt；查询失败 → ok=false
	q3 := &Quota{Source: "live", Plan: PlanQuota{EffectiveRemaining: 1, NextBoundaryAt: "2026-08-31T00:00:00Z"}}
	if _, reset3, _ := parseQuota5h(q3); !reset3.Equal(time.Date(2026, 8, 31, 0, 0, 0, 0, time.UTC)) {
		t.Fatalf("reset=%v, want NextBoundaryAt（无 usage_5h 窗口时）", reset3)
	}
	if _, _, ok4 := parseQuota5h(&Quota{Source: "none"}); ok4 {
		t.Fatal("查询失败（Source=none）应返回 ok=false")
	}
}

// ResumeBudgetLoop：ctx 取消后退出（不挂死）。
func TestResumeBudgetLoop_CtxCancelExits(t *testing.T) {
	p := newTestPool()
	ctx, cancel := context.WithCancel(context.Background())
	cancel() // 先取消再进循环：应立即返回
	done := make(chan struct{})
	go func() {
		p.ResumeBudgetLoop(ctx, func(string) (float64, time.Time, bool) { return 0, time.Time{}, false })
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("ctx 取消后 ResumeBudgetLoop 未退出")
	}
}

// containsAll 判定 s 是否同时包含 subs 里所有子串。
func containsAll(s string, subs ...string) bool {
	for _, sub := range subs {
		if !strings.Contains(s, sub) {
			return false
		}
	}
	return true
}
