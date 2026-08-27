// providers_swr_test.go —— 外部账号计费缓存的 stale-while-revalidate 口径：
// 缓存过期时 Infos 必须立刻把旧值交出去（远端查询丢给后台单飞），
// 管理端子页面点开不再被 3×N 个远端往返同步阻塞。
package tuanjie

import (
	"net/http"
	"net/http/httptest"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// slowProvider 每个端点延迟 delay 响应，并累计被请求次数。
func slowProvider(t *testing.T, delay time.Duration) (*httptest.Server, *int32) {
	t.Helper()
	var hits int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&hits, 1)
		time.Sleep(delay)
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"data":[{"id":"m-fresh"}]}`))
	}))
	t.Cleanup(srv.Close)
	return srv, &hits
}

func newTestStore(baseURL string) *ProviderStore {
	return &ProviderStore{
		path:       "providers-swr-test.json",
		list:       []ExternalProvider{{Name: "p1", BaseURL: baseURL, APIKey: "k", Models: []string{"m1"}}},
		cache:      map[string]cachedInfo{},
		refreshing: map[string]bool{},
	}
}

// waitIdleRefresh 等后台刷新收尾（refreshing 清空）。
func waitIdleRefresh(ps *ProviderStore) bool {
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		ps.mu.Lock()
		idle := len(ps.refreshing) == 0
		ps.mu.Unlock()
		if idle {
			return true
		}
		time.Sleep(30 * time.Millisecond)
	}
	return false
}

// TestInfosReturnsStaleWithoutBlocking 过期缓存：立即回旧值，后台补齐新值。
func TestInfosReturnsStaleWithoutBlocking(t *testing.T) {
	srv, _ := slowProvider(t, 300*time.Millisecond)
	ps := newTestStore(srv.URL)
	ps.cache["p1"] = cachedInfo{
		info:    ProviderInfo{Name: "p1", BaseURL: srv.URL, SubStatus: "STALE"},
		fetched: time.Now().Add(-2 * providerTTL),
	}

	start := time.Now()
	out := ps.Infos()
	elapsed := time.Since(start)

	if len(out) != 1 || out[0].SubStatus != "STALE" {
		t.Fatalf("过期缓存应立刻返回旧值，实得 %+v", out)
	}
	if elapsed > 150*time.Millisecond {
		t.Fatalf("Infos 被远端查询同步阻塞了 %v", elapsed)
	}

	if !waitIdleRefresh(ps) {
		t.Fatal("后台刷新未在超时内结束")
	}
	got := ps.Infos()
	if len(got) != 1 || got[0].SubStatus == "STALE" {
		t.Fatalf("后台刷新后应看到新值，实得 %+v", got)
	}
	if len(got[0].Models) != 1 || got[0].Models[0] != "m-fresh" {
		t.Fatalf("后台刷新结果不对：%+v", got[0])
	}
}

// TestInfosSingleFlight 并发 Infos 只触发一轮远端查询（3 个端点），不叠加。
func TestInfosSingleFlight(t *testing.T) {
	srv, hits := slowProvider(t, 200*time.Millisecond)
	ps := newTestStore(srv.URL)
	ps.cache["p1"] = cachedInfo{
		info:    ProviderInfo{Name: "p1", SubStatus: "STALE"},
		fetched: time.Now().Add(-2 * providerTTL),
	}

	var wg sync.WaitGroup
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for _, info := range ps.Infos() {
				if info.SubStatus != "STALE" {
					t.Errorf("并发调用不该拿到新值抢先：%+v", info)
				}
			}
		}()
	}
	wg.Wait()
	if !waitIdleRefresh(ps) {
		t.Fatal("后台刷新未在超时内结束")
	}
	if n := atomic.LoadInt32(hits); n != 3 {
		t.Fatalf("单飞失效：8 次并发 Infos 触发了 %d 次远端请求（期望 3）", n)
	}
}

// TestInfosColdStartFetches 完全无缓存时同步拉一次，且带回配置信息与可达状态。
func TestInfosColdStartFetches(t *testing.T) {
	srv, _ := slowProvider(t, 50*time.Millisecond)
	ps := newTestStore(srv.URL)
	out := ps.Infos()
	if len(out) != 1 {
		t.Fatalf("应有 1 条，实得 %d", len(out))
	}
	if out[0].Name != "p1" || len(out[0].ConfigModels) != 1 {
		t.Fatalf("冷启动结果缺少配置信息：%+v", out[0])
	}
	if !out[0].OK {
		t.Fatalf("同步首拉应带回可达状态：%+v", out[0])
	}
}
