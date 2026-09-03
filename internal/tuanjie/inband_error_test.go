package tuanjie

import (
	"bytes"
	"log"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"testing"
)

// captureLog 捕获 fn 执行期间写入全局 log 的内容。
func captureLog(t *testing.T, fn func()) string {
	t.Helper()
	var buf bytes.Buffer
	flags := log.Flags()
	log.SetFlags(0)
	log.SetOutput(&buf)
	defer func() {
		log.SetOutput(os.Stderr)
		log.SetFlags(flags)
	}()
	fn()
	return buf.String()
}

func newScanServer() *Server {
	return &Server{stats: map[string]*modelStat{}} // statsPath 留空 → saveStats 不落盘
}

// 上游网关先回 HTTP 200 并把异常原文塞进 SSE 流时，必须在透传处留一行日志：
// 这类 in-band 错误在 pool chat 的 status= 里永远看不到。
func TestUsageScannerLogsInBandStreamError(t *testing.T) {
	u := newUsageScanner("GLM-5.3-FLASH", newScanServer())
	out := captureLog(t, func() {
		u.feed([]byte("data: {\"error\":{\"message\":\"litellm.UnprocessableEntityError: litellm.APIConnectionError: OpenAIException - Connection closed.\",\"type\":null,\"param\":null,\"code\":\"422\"}}\n\n"))
	})
	for _, want := range []string{"流内错误", "model=GLM-5.3-FLASH", "Connection closed", "422"} {
		if !strings.Contains(out, want) {
			t.Errorf("日志缺少 %q，实际输出: %q", want, out)
		}
	}
}

func TestUsageScannerNoErrorLogForNormalStream(t *testing.T) {
	u := newUsageScanner("GLM-5.3-FLASH", newScanServer())
	out := captureLog(t, func() {
		u.feed([]byte("data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"想\"}}]}\n\n"))
		u.feed([]byte("data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n"))
		u.feed([]byte("data: [DONE]\n\n"))
	})
	if strings.Contains(out, "流内错误") {
		t.Errorf("正常流不应出现流内错误日志，实际: %q", out)
	}
}

// 上游可能把同一条错误反复重发，一条流只记一次，别刷穿日志。
func TestUsageScannerLogsInBandErrorOnlyOnce(t *testing.T) {
	u := newUsageScanner("GLM-5.3-FLASH", newScanServer())
	line := "data: {\"error\":{\"message\":\"litellm.APIConnectionError - Connection closed.\",\"code\":\"422\"}}\n\n"
	out := captureLog(t, func() {
		u.feed([]byte(line))
		u.feed([]byte(line))
		u.feed([]byte(line))
	})
	if got := strings.Count(out, "流内错误"); got != 1 {
		t.Errorf("流内错误日志应只有一行，实际 %d 行: %q", got, out)
	}
}

// 回归护栏：读块边界可以把一行 JSON 从中间切开，拼行后仍只记一行。
func TestUsageScannerDetectsErrorSplitAcrossReads(t *testing.T) {
	u := newUsageScanner("GLM-5.3-FLASH", newScanServer())
	line := "data: {\"error\":{\"message\":\"litellm.APIConnectionError - Connection closed.\",\"code\":\"422\"}}\n\n"
	cut := strings.Index(line, "Connection")
	out := captureLog(t, func() {
		u.feed([]byte(line[:cut]))
		u.feed([]byte(line[cut:]))
	})
	if got := strings.Count(out, "流内错误"); got != 1 {
		t.Errorf("跨读块的流内错误应记且只记一行，实际 %d 行: %q", got, out)
	}
}

// 端到端：走真实 forwardExternal 泵循环。上游先回 200 再在流里塞错误时，
// 既要记下行内错误日志，也必须原样透传（本改动只加观测，不改写调用方看到的东西）。
func TestForwardExternalLogsInBandStreamError(t *testing.T) {
	const stream = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n" +
		"data: {\"error\":{\"message\":\"litellm.APIConnectionError: OpenAIException - Connection closed.\",\"code\":\"422\"}}\n\n" +
		"data: [DONE]\n\n"
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(stream))
	}))
	defer up.Close()

	s := NewServer()
	prov := &ExternalProvider{Name: "fake", BaseURL: up.URL, APIKey: "k", Models: []string{"GLM-5.3-FLASH"}}
	req := httptest.NewRequest(http.MethodPost, "/chat/completions", nil)
	body := []byte(`{"model":"GLM-5.3-FLASH","stream":true,"messages":[{"role":"user","content":"hi"}]}`)

	var rec *httptest.ResponseRecorder
	out := captureLog(t, func() {
		rec = httptest.NewRecorder()
		if fail := s.forwardExternal(rec, req, body, "GLM-5.3-FLASH", true, prov, "/chat/completions"); fail != 0 {
			t.Fatalf("200 流不该回落，fail=%d", fail)
		}
	})
	if !strings.Contains(out, "流内错误") || !strings.Contains(out, "model=GLM-5.3-FLASH") {
		t.Errorf("未记到流内错误日志: %q", out)
	}
	if got := rec.Body.String(); got != stream {
		t.Errorf("透传被改动:\n实得 %q\n期望 %q", got, stream)
	}
}
