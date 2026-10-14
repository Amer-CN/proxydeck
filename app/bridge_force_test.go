package main

import (
	"encoding/json"
	"testing"
	"time"
)

// resetUpdateRefreshState 清更新检查全局态（缓存/时钟/通道），各单测前调用，互不串扰。
func resetUpdateRefreshState() {
	updateCacheMu.Lock()
	updateCache = ""
	updateCacheAt = time.Time{}
	updateLastFetch = time.Time{}
	updateCacheMu.Unlock()
	select {
	case <-updateRefreshCh:
	default:
	}
}

func updateRefreshQueueLen() int { return len(updateRefreshCh) }

// TestTryKickUpdateRefreshRateLimit 30s 踢取限频：窗口内第二次踢取被丢弃，不进通道；
// 窗口外恢复有效（与 ccCheckUpdate 内联 `Since>30s 才踢` 语义一致）。
func TestTryKickUpdateRefreshRateLimit(t *testing.T) {
	resetUpdateRefreshState()
	if !tryKickUpdateRefresh() {
		t.Fatal("首次踢取应有效（时钟零值=从未拉取）")
	}
	if updateRefreshQueueLen() != 1 {
		t.Fatalf("首次踢取后通道应有 1 个待处理，got %d", updateRefreshQueueLen())
	}
	if tryKickUpdateRefresh() {
		t.Fatal("30s 窗口内第二次踢取必须被限频丢弃")
	}
	if updateRefreshQueueLen() != 1 {
		t.Fatalf("被丢弃的踢取不得进通道，got %d", updateRefreshQueueLen())
	}
	// 30s 窗口外恢复有效
	updateCacheMu.Lock()
	updateLastFetch = time.Now().Add(-31 * time.Second)
	updateCacheMu.Unlock()
	select {
	case <-updateRefreshCh: // 腾空，观察下一次是否重新排队
	default:
	}
	if !tryKickUpdateRefresh() {
		t.Fatal("31s 后踢取应恢复有效")
	}
	if updateRefreshQueueLen() != 1 {
		t.Fatalf("恢复后通道应有 1 个待处理，got %d", updateRefreshQueueLen())
	}
	resetUpdateRefreshState()
}

// TestTryKickChannelDedup 通道 cap=1 去重：已有排队时新踢取合并，不堆积。
func TestTryKickChannelDedup(t *testing.T) {
	resetUpdateRefreshState()
	updateRefreshCh <- struct{}{} // 模拟已有一次待处理拉取
	updateCacheMu.Lock()
	updateLastFetch = time.Now().Add(-time.Hour) // 时钟放行，走到通道分支
	updateCacheMu.Unlock()
	if !tryKickUpdateRefresh() {
		t.Fatal("时钟放行时应返回 true")
	}
	if updateRefreshQueueLen() != 1 {
		t.Fatalf("通道 cap=1：已有排队时不得堆积，got %d", updateRefreshQueueLen())
	}
	resetUpdateRefreshState()
}

// TestCheckUpdateForceClearsCache Force 清缓存＋恒回 checking（验收1：发版后即点能查新版
// 的前提——保鲜期内的旧缓存被清，前端重试轮询才能拿到 fresh 结果）。
func TestCheckUpdateForceClearsCache(t *testing.T) {
	resetUpdateRefreshState()
	updateCacheMu.Lock()
	updateCache = `{"ok":true,"latest":"v3.20.2","url":"","downloads":1}`
	updateCacheAt = time.Now() // 保鲜期内（模拟发版后 30 分钟内的旧缓存）
	updateLastFetch = time.Now().Add(-time.Hour)
	updateCacheMu.Unlock()
	got := checkUpdateForce()
	var m map[string]any
	if err := json.Unmarshal([]byte(got), &m); err != nil || m["msg"] != "checking" {
		t.Fatalf("Force 应恒回 checking，got %q", got)
	}
	updateCacheMu.Lock()
	cached := updateCache
	updateCacheMu.Unlock()
	if cached != "" {
		t.Fatalf("Force 必须清掉旧缓存，残留 %q", cached)
	}
	if updateRefreshQueueLen() != 1 {
		t.Fatalf("Force 应踢出一次有效拉取，通道待处理 got %d", updateRefreshQueueLen())
	}
	resetUpdateRefreshState()
}

// TestCheckUpdateForceIgnoresRateLimit Force 无条件踢取（第2轮修正断言）：
// 30s 限频只约束普通自动路径；GUI 启动自带一次拉取，限频窗口内用户点手动检查时，
// 若沿用限频会清掉好缓存又踢不出新拉取，前端 4 次重试后误报"暂时无法连接"。
// 故限频窗口内单次 Force 仍产生恰 1 次有效拉取；紧接着第二次 Force 靠通道 cap=1 去重合并，仍为 1。
func TestCheckUpdateForceIgnoresRateLimit(t *testing.T) {
	resetUpdateRefreshState()
	updateCacheMu.Lock()
	updateCache = `{"ok":true,"latest":"v3.20.2","url":"","downloads":1}`
	updateCacheAt = time.Now()
	updateLastFetch = time.Now() // 模拟 GUI 启动刚拉取过：普通自动路径 30s 内被限频
	updateCacheMu.Unlock()
	first := checkUpdateForce()
	if first != `{"ok":false,"msg":"checking"}` {
		t.Fatalf("限频窗口内 Force 应恒回 checking，got %q", first)
	}
	if updateRefreshQueueLen() != 1 {
		t.Fatalf("限频窗口内单次 Force 必须产生恰 1 次有效拉取，通道 got %d", updateRefreshQueueLen())
	}
	updateCacheMu.Lock()
	cached := updateCache
	updateCacheMu.Unlock()
	if cached != "" {
		t.Fatalf("Force 必须清掉旧缓存，残留 %q", cached)
	}
	// 紧接着连点第二次：恒回 checking，且不叠加（通道去重，仍为 1）
	second := checkUpdateForce()
	if second != `{"ok":false,"msg":"checking"}` {
		t.Fatalf("第二次 Force 应恒回 checking，got %q", second)
	}
	if updateRefreshQueueLen() != 1 {
		t.Fatalf("连点两次 Force 不得叠加有效拉取，通道 got %d", updateRefreshQueueLen())
	}
	resetUpdateRefreshState()
}
