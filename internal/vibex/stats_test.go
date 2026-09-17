// stats_test.go —— /v1/stats 单测：真 usage 入账读回一致 + 落盘重启读回一致。
package vibex

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// writeFileForTest 写一个测试用文件（损坏文件用例）。
func writeFileForTest(path, s string) error { return os.WriteFile(path, []byte(s), 0o644) }

// statsOf 取 /v1/stats 的 models 段。
func statsOf(t *testing.T, url string) map[string]map[string]any {
	t.Helper()
	code, body := getBody(t, url+"/v1/stats")
	if code != 200 {
		t.Fatalf("/v1/stats 应 200，得到 %d：%s", code, body)
	}
	var got map[string]any
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatalf("/v1/stats 不是 JSON: %v (%s)", err, body)
	}
	if _, ok := got["uptimeSec"]; !ok {
		t.Fatalf("/v1/stats 缺 uptimeSec: %s", body)
	}
	models, ok := got["models"].(map[string]any)
	if !ok {
		t.Fatalf("/v1/stats 缺 models 对象: %s", body)
	}
	out := map[string]map[string]any{}
	for k, v := range models {
		out[k] = toMap(v)
	}
	return out
}

// 验收 1：非流式一轮后 /v1/stats 的 calls 与 tokens 与假上游 usage 真值一致。
func TestStatsAfterSyncChat(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	s.statsPath = filepath.Join(t.TempDir(), "vibex-stats.json")
	ts := newTestHTTP(t, s)

	// 入账前：空表也 200（文件不存在不 500）。
	if m := statsOf(t, ts.URL); len(m) != 0 {
		t.Fatalf("未跑对话时应为空表，实际 %v", m)
	}

	code, body := postChat(t, ts.URL, `{"model":"lite-1","messages":[{"role":"user","content":"你好"}]}`)
	if code != 200 {
		t.Fatalf("非流式应 200，得到 %d：%s", code, body)
	}
	m := statsOf(t, ts.URL)
	st := m["lite-1"]
	if st == nil {
		t.Fatalf("应记到 model=lite-1，实际 %v", m)
	}
	// 假上游 result 事件的真 usage：input=12 output=7
	if st["calls"] != float64(1) || st["inputTokens"] != float64(12) ||
		st["outputTokens"] != float64(7) || st["totalTokens"] != float64(19) {
		t.Fatalf("统计与 result 真 usage 不一致: %v", st)
	}
}

// 验收 1（流式分支）：流式一轮后同样入账，且字段形状与 B.AI 一字对齐。
func TestStatsAfterStreamChat(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	s.statsPath = filepath.Join(t.TempDir(), "vibex-stats.json")
	ts := newTestHTTP(t, s)

	code, body := postChat(t, ts.URL,
		`{"model":"lite-1","messages":[{"role":"user","content":"打个招呼"}],"stream":true}`)
	if code != 200 {
		t.Fatalf("流式应 200，得到 %d：%s", code, body)
	}
	st := statsOf(t, ts.URL)["lite-1"]
	if st == nil || st["calls"] != float64(1) || st["totalTokens"] != float64(19) {
		t.Fatalf("流式统计不对: %v", st)
	}
	// 形状断言：GUI 只读这四个键（ui.html:8665）。
	for _, k := range []string{"calls", "inputTokens", "outputTokens", "totalTokens"} {
		if _, ok := st[k]; !ok {
			t.Fatalf("/v1/stats 缺键 %s: %v", k, st)
		}
	}
}

// 验收 2：落盘 → 新 Server 读回，数值一致（重启不丢）。
func TestStatsPersistAcrossRestart(t *testing.T) {
	path := filepath.Join(t.TempDir(), "vibex-stats.json")
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	s.statsPath = path
	ts := newTestHTTP(t, s)
	if _, body := postChat(t, ts.URL, `{"model":"lite-1","messages":[{"role":"user","content":"你好"}]}`); body == "" {
		t.Fatal("对话无响应")
	}

	// 重启：新 Server 实例读同一份落盘文件。
	s2 := newTestServer(t, up.url(), "tok-1")
	s2.statsPath = path
	s2.loadStats()
	ts2 := newTestHTTP(t, s2)
	st := statsOf(t, ts2.URL)["lite-1"]
	if st == nil || st["calls"] != float64(1) || st["inputTokens"] != float64(12) ||
		st["outputTokens"] != float64(7) || st["totalTokens"] != float64(19) {
		t.Fatalf("重启读回不一致: %v", st)
	}
}

// 硬约束 1：无 usage 的轮次不入账（不估算脏数据）。
func TestStatsSkipsMissingUsage(t *testing.T) {
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url(), "tok-1")
	s.statsPath = filepath.Join(t.TempDir(), "vibex-stats.json")
	ts := newTestHTTP(t, s)

	s.recordUsage("lite-1", nil)
	s.recordUsage("lite-1", map[string]any{})
	if m := statsOf(t, ts.URL); len(m) != 0 {
		t.Fatalf("无 usage 不应入账，实际 %v", m)
	}
}

// 硬约束 3：统计文件缺失/损坏 → 空表 200，不 500。
func TestStatsMissingAndCorruptFile(t *testing.T) {
	up := newFakeUpstream(t)
	dir := t.TempDir()

	s := newTestServer(t, up.url(), "tok-1")
	s.statsPath = filepath.Join(dir, "vibex-stats.json") // 不存在
	s.loadStats()
	if m := statsOf(t, newTestHTTP(t, s).URL); len(m) != 0 {
		t.Fatalf("文件缺失应空表，实际 %v", m)
	}

	bad := filepath.Join(dir, "bad-stats.json")
	if err := writeFileForTest(bad, "{不是 JSON"); err != nil {
		t.Fatal(err)
	}
	s2 := newTestServer(t, up.url(), "tok-1")
	s2.statsPath = bad
	s2.loadStats()
	if m := statsOf(t, newTestHTTP(t, s2).URL); len(m) != 0 {
		t.Fatalf("文件损坏应空表，实际 %v", m)
	}
}
