package proxy

import (
	"math"
	"os"
	"path/filepath"
	"testing"
)

func TestUsageStatsPersistAcrossRestart(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "stats.json")

	s := NewUsageStats(file)
	s.Record("deepseek/deepseek-v4-flash", 1000, 500, 9000, 200)
	s.Record("deepseek/deepseek-v4-flash", 2000, 800, 12000, 0)

	// Simulate restart: new instance loading the same file.
	s2 := NewUsageStats(file)
	if s2.Models["deepseek/deepseek-v4-flash"] == nil {
		t.Fatal("stats lost after reload")
	}
	ms := s2.Models["deepseek/deepseek-v4-flash"]
	if ms.InputTokens != 3000 || ms.OutputTokens != 1300 || ms.Runs != 2 {
		t.Fatalf("unexpected totals: %+v", ms)
	}
	if ms.CacheReadTokens != 21000 || ms.CacheWriteTokens != 200 {
		t.Fatalf("unexpected cache totals: read=%d write=%d", ms.CacheReadTokens, ms.CacheWriteTokens)
	}
	todayIn, todayOut, todayCacheRead, todayCacheWrite := s2.Today()
	if todayIn != 3000 || todayOut != 1300 || todayCacheRead != 21000 || todayCacheWrite != 200 {
		t.Fatalf("unexpected today: in=%d out=%d cacheRead=%d cacheWrite=%d", todayIn, todayOut, todayCacheRead, todayCacheWrite)
	}
	_ = os.Remove(file)
}

// TestEstimateCostWithCacheHit: 官网 432.8M tokens 仅 $1.86 —— 绝大多数是缓存命中。
// 缓存读取按 $0.0028/1M 计（deepseek-v4-flash），输入 $0.14、输出 $0.28。
func TestEstimateCostWithCacheHit(t *testing.T) {
	models := map[string]*ModelStat{
		"deepseek-v4-flash": {
			InputTokens:      5_000_000,
			OutputTokens:     400_000,
			CacheReadTokens:  370_000_000,
			CacheWriteTokens: 57_400_000,
		},
	}
	cost := estimateCost(models)
	want := 5e6/1e6*0.14 + 0.4e6/1e6*0.28 + 370e6/1e6*0.0028
	if math.Abs(cost-want) > 1e-9 {
		t.Fatalf("cost mismatch: got %.6f want %.6f", cost, want)
	}
	if cost > 1.90 || cost < 1.70 {
		t.Fatalf("cost out of expected range (官网 ~$1.86): got %.4f", cost)
	}
}

// TestEstimateCostCacheFallback: 无缓存价格表的模型按输入价 2% 估算，不 panic。
func TestEstimateCostCacheFallback(t *testing.T) {
	models := map[string]*ModelStat{
		"some/unknown-model": {
			InputTokens:     1_000_000,
			OutputTokens:    1_000_000,
			CacheReadTokens: 10_000_000,
		},
	}
	cost := estimateCost(models) // 未知模型价格全 0，应得 0
	if cost != 0 {
		t.Fatalf("unknown model should cost 0, got %.4f", cost)
	}
	models = map[string]*ModelStat{
		"kimi-k2.6": {
			InputTokens:     1_000_000,
			OutputTokens:    1_000_000,
			CacheReadTokens: 10_000_000,
		},
	}
	cost = estimateCost(models)
	want := 0.95 + 4.0 + 10e6/1e6*(0.95*0.02)
	if math.Abs(cost-want) > 1e-9 {
		t.Fatalf("kimi fallback mismatch: got %.6f want %.6f", cost, want)
	}
}
