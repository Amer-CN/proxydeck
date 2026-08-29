// models_test.go —— B.AI 模型矩阵端点 GET /model/matrix 的行为契约：
// 上游清单落缓存、缓存只由拉杆/手动刷新打破、single-flight 并发合流、
// 失败负缓存、有旧值时失败不清空、无 key 不打上游。
package bai

import (
	"encoding/json"
	"fmt"
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

// newMatrixTest 建一个矩阵测试环境：假上游 + 临时渠道密钥文件，返回测试缝还原函数。
func newMatrixTest(t *testing.T, upstream http.Handler, key string) *Server {
	t.Helper()
	srv := httptest.NewServer(upstream)
	t.Cleanup(srv.Close)

	oldURL, oldPath, oldTransport := upstreamModelsURL, channelsConfigPath, matrixTransport
	upstreamModelsURL = srv.URL + "/v1/models"
	path := filepath.Join(t.TempDir(), "tuanjie-water-channels.json")
	if key != "" {
		if err := os.WriteFile(path, []byte(`[{"id":"bai","key":"`+key+`"}]`), 0o600); err != nil {
			t.Fatalf("写渠道配置: %v", err)
		}
	}
	channelsConfigPath = func() string { return path }
	matrixTransport = func() http.RoundTripper { return srv.Client().Transport }
	t.Cleanup(func() {
		upstreamModelsURL, channelsConfigPath, matrixTransport = oldURL, oldPath, oldTransport
	})
	return NewServer()
}

// getMatrix 请求一次矩阵端点，refresh=true 时带 ?refresh=1。
func getMatrix(t *testing.T, s *Server, refresh bool) matrixResp {
	t.Helper()
	u := "/model/matrix"
	if refresh {
		u += "?refresh=1"
	}
	r := httptest.NewRequest(http.MethodGet, u, nil)
	w := httptest.NewRecorder()
	s.handleModelMatrix(w, r)
	if w.Code != http.StatusOK {
		t.Fatalf("HTTP %d，期望 200（矩阵端点不应把失败变成状态码）", w.Code)
	}
	var out matrixResp
	if err := json.Unmarshal(w.Body.Bytes(), &out); err != nil {
		t.Fatalf("响应不是合法 JSON: %v / %s", err, w.Body.String())
	}
	return out
}

// modelsListHandler 回一个标准 OpenAI 模型清单。
func modelsListHandler(ids ...string) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") == "" {
			w.WriteHeader(http.StatusUnauthorized)
			_, _ = w.Write([]byte(`{"error":{"message":"Invalid token"}}`))
			return
		}
		items := make([]map[string]any, 0, len(ids))
		for _, id := range ids {
			items = append(items, map[string]any{"id": id, "object": "model", "owned_by": "minimax"})
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"object": "list", "data": items})
	}
}

// TestMatrixServesUpstreamList 上游清单原样落到 data，顺序与条数一致。
func TestMatrixServesUpstreamList(t *testing.T) {
	s := newMatrixTest(t, modelsListHandler("glm-5.3", "deepseek-v4-flash", "hy3"), "sk-test")
	got := getMatrix(t, s, false)
	if !got.OK || got.Err != "" {
		t.Fatalf("期望成功，got ok=%v err=%q", got.OK, got.Err)
	}
	if got.Count != 3 {
		t.Fatalf("期望 3 个模型，got %d", got.Count)
	}
	if got.Source != "live" || got.FetchedAt == 0 {
		t.Fatalf("首次应为 live 且带时间戳，got %s / %d", got.Source, got.FetchedAt)
	}
	var data []struct {
		ID      string `json:"id"`
		OwnedBy string `json:"owned_by"`
	}
	if err := json.Unmarshal(got.Data, &data); err != nil {
		t.Fatalf("data 结构异常: %v", err)
	}
	if len(data) != 3 || data[0].ID != "glm-5.3" || data[2].ID != "hy3" || data[1].OwnedBy != "minimax" {
		t.Fatalf("data 内容/顺序被改写: %s", got.Data)
	}
}

// TestMatrixCachedUntilRefresh 矩阵只在拉杆/手动刷新时打上游——3 秒轮询不得漏到官网。
func TestMatrixCachedUntilRefresh(t *testing.T) {
	var hits int32
	s := newMatrixTest(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&hits, 1)
		modelsListHandler("glm-5.3").ServeHTTP(w, r)
	}), "sk-test")

	for i := 0; i < 6; i++ { // 模拟 6 轮 GUI 轮询
		getMatrix(t, s, false)
	}
	if n := atomic.LoadInt32(&hits); n != 1 {
		t.Fatalf("轮询漏到上游：期望 1 次，实际 %d 次", n)
	}
	getMatrix(t, s, true) // 拉杆点火 / ↻ 刷新
	if n := atomic.LoadInt32(&hits); n != 2 {
		t.Fatalf("refresh=1 未强制刷新：期望 2 次，实际 %d 次", n)
	}
}

// TestMatrixSingleFlight 冷启动并发请求只放一个去上游，其余吃同一份结果。
func TestMatrixSingleFlight(t *testing.T) {
	var hits int32
	slow := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&hits, 1)
		time.Sleep(120 * time.Millisecond)
		modelsListHandler("glm-5.3", "kimi-k3").ServeHTTP(w, r)
	})
	s := newMatrixTest(t, slow, "sk-test")

	var wg sync.WaitGroup
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() { defer wg.Done(); getMatrix(t, s, false) }()
	}
	wg.Wait()
	if n := atomic.LoadInt32(&hits); n != 1 {
		t.Fatalf("并发未合流：期望 1 次上游请求，实际 %d 次", n)
	}
}

// TestMatrixNeedKey 没配密钥时不发任何上游请求，直接给可解释的错误码。
func TestMatrixNeedKey(t *testing.T) {
	var hits int32
	s := newMatrixTest(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&hits, 1)
		modelsListHandler("glm-5.3").ServeHTTP(w, r)
	}), "") // 渠道配置里无 bai 条目

	got := getMatrix(t, s, false)
	if got.OK || got.Err != "need_key" {
		t.Fatalf("期望 need_key，got ok=%v err=%q", got.OK, got.Err)
	}
	if n := atomic.LoadInt32(&hits); n != 0 {
		t.Fatalf("无密钥不应打上游，实际 %d 次", n)
	}
}

// TestMatrixNegativeCacheOnFailure 上游故障后 60 秒内轮询不得反复重试。
func TestMatrixNegativeCacheOnFailure(t *testing.T) {
	var hits int32
	s := newMatrixTest(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&hits, 1)
		w.WriteHeader(http.StatusBadGateway) // Cloudflare 520/502 类
		_, _ = w.Write([]byte(`{"error":{"message":"bad gateway"}}`))
	}), "sk-test")

	got := getMatrix(t, s, false)
	if got.OK || got.Err != "upstream_error" {
		t.Fatalf("期望 upstream_error，got ok=%v err=%q", got.OK, got.Err)
	}
	first := atomic.LoadInt32(&hits)
	for i := 0; i < 5; i++ {
		getMatrix(t, s, false)
	}
	if n := atomic.LoadInt32(&hits); n != first {
		t.Fatalf("失败后未做负缓存：期望仍为 %d 次，实际 %d 次", first, n)
	}
}

// TestMatrixKeepsStaleOnFailure 已有清单时上游挂了继续画旧值（source=cache），不清空不报错。
func TestMatrixKeepsStaleOnFailure(t *testing.T) {
	var fail atomic.Bool
	s := newMatrixTest(t, http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if fail.Load() {
			w.WriteHeader(http.StatusBadGateway)
			return
		}
		modelsListHandler("glm-5.3", "kimi-k3").ServeHTTP(w, r)
	}), "sk-test")

	if got := getMatrix(t, s, false); !got.OK || got.Count != 2 {
		t.Fatalf("前置条件失败：首次未取到清单 (%+v)", got)
	}
	fail.Store(true)
	got := getMatrix(t, s, true)
	if !got.OK || got.Err != "" {
		t.Fatalf("上游故障时应回旧清单而非报错，got ok=%v err=%q", got.OK, got.Err)
	}
	if got.Source != "cache" {
		t.Fatalf("旧数据必须标 source=cache，让 UI 能说实话，got %s", got.Source)
	}
	if got.Count != 2 {
		t.Fatalf("旧清单被清空：期望 2 条，got %d", got.Count)
	}
}

// TestBaiChannelKeyReadsSharedConfig 密钥取自注水检测共用的渠道配置文件。
func TestBaiChannelKeyReadsSharedConfig(t *testing.T) {
	path := filepath.Join(t.TempDir(), "tuanjie-water-channels.json")
	body := `[{"id":"command","key":"sk-c"},{"id":"bai","key":"sk-bai"}]`
	if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
		t.Fatalf("写配置: %v", err)
	}
	old := channelsConfigPath
	channelsConfigPath = func() string { return path }
	defer func() { channelsConfigPath = old }()

	if got := baiChannelKey(); got != "sk-bai" {
		t.Fatalf("期望取到 bai 条目密钥，got %q", got)
	}

	// 文件不存在 / JSON 坏掉都不能panic，回空串走 need_key 分支
	channelsConfigPath = func() string { return filepath.Join(t.TempDir(), "missing.json") }
	if got := baiChannelKey(); got != "" {
		t.Fatalf("缺失配置应回空串，got %q", got)
	}
	broken := filepath.Join(t.TempDir(), "broken.json")
	if err := os.WriteFile(broken, []byte("{not json"), 0o600); err != nil {
		t.Fatalf("写坏配置: %v", err)
	}
	channelsConfigPath = func() string { return broken }
	if got := baiChannelKey(); got != "" {
		t.Fatalf("坏配置应回空串，got %q", got)
	}
	if strings.Contains(fmt.Sprint(baiChannelKey()), "not json") {
		t.Fatal("错误内容不得泄漏")
	}
}
