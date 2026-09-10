package tuanjie

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

// newUTF8GuardServer 只造拒绝路径需要的字段：本地拒绝不碰上游、不碰账号池，
// 所以 client / pool 留空即可。
func newUTF8GuardServer() *Server {
	return &Server{stats: map[string]*modelStat{}, activity: NewActivityLog()}
}

// Git Bash 用 `-d '你好'` 调原生 curl.exe 时，参数会被转成系统代码页（GBK）：
// 真正发出去的 body 是这串裸 GBK 字节，不是 UTF-8。
const gbkHelloBody = "{\"model\":\"GLM-5.3-FLASH\",\"stream\":true,\"max_tokens\":8," +
	"\"messages\":[{\"role\":\"user\",\"content\":\"\xc4\xe3\xba\xc3\"}]}"

// 非法 UTF-8 的请求体必须在本地拒绝，不能转发出去挨上游那句误导性的 model=None。
func TestRejectInvalidUTF8BodyGBK(t *testing.T) {
	s := newUTF8GuardServer()
	rec := httptest.NewRecorder()
	var rejected bool
	out := captureLog(t, func() {
		rejected = s.rejectInvalidUTF8Body(rec, []byte(gbkHelloBody))
	})

	if !rejected {
		t.Fatal("GBK 字节的 body 必须被拒绝")
	}
	if rec.Code != http.StatusBadRequest {
		t.Errorf("状态码 = %d，期望 400", rec.Code)
	}
	got := rec.Body.String()
	for _, want := range []string{"不是合法 UTF-8", "--data-binary"} {
		if !strings.Contains(got, want) {
			t.Errorf("响应缺少 %q，实际: %s", want, got)
		}
	}
	if strings.Contains(got, "Invalid model name") {
		t.Errorf("不应再出现上游那句误导性报错，实际: %s", got)
	}
	if !strings.Contains(out, "未转发上游") {
		t.Errorf("日志缺少「未转发上游」，实际: %q", out)
	}

	evs := s.activity.List(5)
	if len(evs) != 1 || evs[0].Kind != "error" || evs[0].Status != http.StatusBadRequest {
		t.Errorf("实时动态应记一条 400 error 事件，实际: %+v", evs)
	}
}

// 同一段 JSON 用真 UTF-8 发（"你好" 的合法编码是 E4 BD A0 E5 A5 BD）必须放行，
// 且不写任何响应——否则正常中文请求会被误伤。
func TestRejectInvalidUTF8BodyPassesUTF8(t *testing.T) {
	s := newUTF8GuardServer()
	rec := httptest.NewRecorder()
	body := "{\"model\":\"GLM-5.3-FLASH\",\"stream\":true,\"max_tokens\":8," +
		"\"messages\":[{\"role\":\"user\",\"content\":\"你好\"}]}"

	if rejected := s.rejectInvalidUTF8Body(rec, []byte(body)); rejected {
		t.Fatal("合法 UTF-8 的 body 不该被拒绝")
	}
	if rec.Code != http.StatusOK || rec.Body.Len() != 0 {
		t.Errorf("放行时不应写响应，实际 code=%d body=%q", rec.Code, rec.Body.String())
	}
}

// 走完整 handleChat 入口复验接线：拦截必须在最前面，早于 model 解析、
// 图片预下载、脱敏、重排等步骤，所以不碰 client / pool 也不会 panic。
func TestHandleChatRejectsInvalidUTF8Body(t *testing.T) {
	s := newUTF8GuardServer()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/v1/chat/completions", strings.NewReader(gbkHelloBody))
	s.handleChat(rec, req)

	if rec.Code != http.StatusBadRequest {
		t.Fatalf("状态码 = %d，期望 400（body=%s）", rec.Code, rec.Body.String())
	}
	if !strings.Contains(rec.Body.String(), "不是合法 UTF-8") {
		t.Errorf("响应应说明是编码问题，实际: %s", rec.Body.String())
	}
}

// 确定性错误不值得重试：上游那句 model=None 重发 3 次结果一样。
// 但别把重试整体关掉——「模型未映射 / 网关抖动」仍要保持可重试。
func TestRetriableUpstreamInvalidModelNameNotRetried(t *testing.T) {
	upstream := "{\"error\":{\"message\":\"{'error': '/chat/completions: Invalid model name " +
		"passed in model=None. Call `/v1/models` to view available models for your key.'}\"," +
		"\"type\":\"None\",\"param\":\"None\",\"code\":\"400\"}}"
	if retriableUpstream(upstream) {
		t.Error("Invalid model name 是确定性错误，不该重试")
	}

	for _, body := range []string{
		"litellm.BadRequestError: model not mapped",
		"upstream 503 service unavailable",
		"bad gateway",
	} {
		if !retriableUpstream(body) {
			t.Errorf("%q 应保持可重试", body)
		}
	}
}
