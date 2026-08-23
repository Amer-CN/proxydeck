package proxy

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Amer-CN/proxydeck/internal/api"
)

// ccTextStream 模拟上游返回的一段简单文本流（含 totalUsage）。
func ccTextStream() io.ReadCloser {
	lines := []string{
		`{"type":"text-delta","text":"你好"}`,
		`{"type":"text-delta","text":"，世界"}`,
		`{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":12,"outputTokens":6,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}`,
	}
	return io.NopCloser(strings.NewReader(strings.Join(lines, "\n") + "\n"))
}

// failTransport 用于断言"上游不应被调用"的场景。
type failTransport struct {
	called bool
}

func (t *failTransport) RoundTrip(*http.Request) (*http.Response, error) {
	t.called = true
	return nil, errors.New("upstream should not be called")
}

// TestAnthropicToChatRequest 验证 Anthropic 请求 → OpenAI chat 请求的映射：
// system 前置、tool_use 记名、tool_result 拆成独立 tool 消息、其余内容保留。
func TestAnthropicToChatRequest(t *testing.T) {
	req := api.AnthropicMessageRequest{
		Model:     "claude-sonnet-5",
		MaxTokens: intPtr(4096),
		System:    "你是助手",
		Messages: []api.AnthropicMessage{
			{Role: "user", Content: "你好"},
			{Role: "assistant", Content: []any{
				map[string]any{"type": "text", "text": "好的"},
				map[string]any{"type": "tool_use", "id": "toolu_1", "name": "bash", "input": map[string]any{"command": "ls"}},
			}},
			{Role: "user", Content: []any{
				map[string]any{"type": "tool_result", "tool_use_id": "toolu_1", "content": []any{
					map[string]any{"type": "text", "text": "ok"},
				}},
			}},
			{Role: "user", Content: "继续"},
		},
		Stream: true,
	}
	chat := anthropicToChatRequest(req)

	if chat.Model != "claude-sonnet-5" || !chat.Stream || chat.MaxTokens == nil || *chat.MaxTokens != 4096 {
		t.Fatalf("model/stream/max_tokens not mapped: %+v", chat)
	}
	if chat.Messages[0].Role != "system" || chat.Messages[0].Content != "你是助手" {
		t.Fatalf("system not mapped first: %+v", chat.Messages[0])
	}

	var toolMsg, userMsg *api.OpenAIMessage
	var sawToolUse bool
	for i := range chat.Messages {
		m := &chat.Messages[i]
		switch {
		case m.Role == "tool":
			toolMsg = m
		case m.Role == "user" && m.Content == "继续":
			userMsg = m
		}
		if m.Role == "assistant" {
			if parts, ok := m.Content.([]any); ok {
				for _, part := range parts {
					if pm, ok := part.(map[string]any); ok {
						if typ, _ := pm["type"].(string); typ == "tool_use" {
							sawToolUse = true
						}
					}
				}
			}
		}
	}
	if toolMsg == nil {
		t.Fatalf("tool_result not flattened into a tool message: %+v", chat.Messages)
	}
	if toolMsg.ToolCallID != "toolu_1" || toolMsg.Name != "bash" || toolMsg.Content != "ok" {
		t.Fatalf("tool message fields wrong: %+v", toolMsg)
	}
	if !sawToolUse {
		t.Fatalf("assistant tool_use block lost")
	}
	if userMsg == nil {
		t.Fatalf("plain user message lost")
	}
}

// TestAnthropicSystemBlocks system 参数为 blocks 数组时拼接其中的 text。
func TestAnthropicSystemBlocks(t *testing.T) {
	req := api.AnthropicMessageRequest{
		Model: "deepseek-v4-flash",
		System: []any{
			map[string]any{"type": "text", "text": "第一段"},
			map[string]any{"type": "text", "text": "第二段"},
			map[string]any{"type": "image", "source": map[string]any{"type": "base64", "data": "x"}},
		},
		Messages: []api.AnthropicMessage{{Role: "user", Content: "hi"}},
	}
	chat := anthropicToChatRequest(req)
	if got := chat.Messages[0].Content; got != "第一段\n第二段" {
		t.Fatalf("system blocks = %v, want 第一段\n第二段", got)
	}
}

// TestCheckPlanModel 验证套餐门槛：claude-*（OnGo:false）拒绝，Go 套餐内与
// 未知模型放行。
func TestCheckPlanModel(t *testing.T) {
	for _, m := range []string{"claude-sonnet-5", "claude-opus-5", "sonnet-5", "gpt-5.4", "gemini-3.5-flash", "grok-4.6"} {
		if err := checkPlanModel(m); err == nil {
			t.Errorf("checkPlanModel(%q) = nil, want plan error", m)
		}
	}
	for _, m := range []string{"deepseek-v4-flash", "deepseek/deepseek-v4-pro", "gpt-5.6-luna", "some-unknown-model"} {
		if err := checkPlanModel(m); err != nil {
			t.Errorf("checkPlanModel(%q) = %v, want nil", m, err)
		}
	}
}

// TestHandleMessages_PlanGate 验证 Go 套餐外模型在请求上游之前即被拒绝。
func TestHandleMessages_PlanGate(t *testing.T) {
	p := NewProxy("test-key")
	ft := &failTransport{}
	p.Client = &http.Client{Transport: ft}

	body := `{"model":"claude-sonnet-5","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/messages", strings.NewReader(body))
	rec := httptest.NewRecorder()
	p.HandleMessages(rec, req)

	if rec.Code != http.StatusForbidden {
		t.Fatalf("status = %d, want 403", rec.Code)
	}
	var errResp api.AnthropicErrorResponse
	if err := json.Unmarshal(rec.Body.Bytes(), &errResp); err != nil {
		t.Fatalf("unmarshal error response: %v", err)
	}
	if errResp.Type != "error" || errResp.Error.Type != "permission_error" {
		t.Fatalf("unexpected error shape: %+v", errResp)
	}
	if ft.called {
		t.Fatalf("upstream called for plan-restricted model")
	}
}

// TestNonStreamAnthropicResponse 验证非流式响应的 Anthropic 格式。
func TestNonStreamAnthropicResponse(t *testing.T) {
	p := NewProxy("")
	ccResp := &http.Response{StatusCode: 200, Body: ccTextStream()}
	rec := httptest.NewRecorder()
	p.NonStreamAnthropicResponse(rec, ccResp, "msg_test123", "claude-sonnet-5", 1755000000)

	var resp api.AnthropicMessageResponse
	if err := json.Unmarshal(rec.Body.Bytes(), &resp); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if resp.Type != "message" || resp.Role != "assistant" {
		t.Fatalf("type/role wrong: %+v", resp)
	}
	if !strings.HasPrefix(resp.ID, "msg_") {
		t.Fatalf("id = %q, want msg_ prefix", resp.ID)
	}
	if len(resp.Content) != 1 || resp.Content[0].Type != "text" || resp.Content[0].Text != "你好，世界" {
		t.Fatalf("content wrong: %+v", resp.Content)
	}
	if resp.StopReason == nil || *resp.StopReason != "end_turn" {
		t.Fatalf("stop_reason = %v, want end_turn", resp.StopReason)
	}
	if resp.Usage.InputTokens != 12 || resp.Usage.OutputTokens != 6 {
		t.Fatalf("usage wrong: %+v", resp.Usage)
	}
}

// parseSSEEvents 把 SSE 响应体解析成 (event 名, data) 序列。
func parseSSEEvents(t *testing.T, body string) [][2]string {
	t.Helper()
	var events [][2]string
	var curEvent, curData string
	for _, line := range strings.Split(body, "\n") {
		switch {
		case strings.HasPrefix(line, "event: "):
			curEvent = strings.TrimPrefix(line, "event: ")
		case strings.HasPrefix(line, "data: "):
			curData = strings.TrimPrefix(line, "data: ")
			if curData == "[DONE]" {
				events = append(events, [2]string{curEvent, curData})
			} else {
				events = append(events, [2]string{curEvent, curData})
			}
			curEvent, curData = "", ""
		}
	}
	if len(events) == 0 {
		t.Fatalf("no SSE events parsed from body: %q", body)
	}
	return events
}

// TestStreamAnthropicResponse 验证 Anthropic 事件序列：
// message_start → content_block_start → content_block_delta(text_delta) →
// content_block_stop → message_delta → message_stop → [DONE]。
func TestStreamAnthropicResponse(t *testing.T) {
	p := NewProxy("")
	ccResp := &http.Response{StatusCode: 200, Body: ccTextStream()}
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/v1/messages", nil)
	got := p.StreamAnthropicResponse(rec, req, ccResp, "msg_test123", "claude-sonnet-5", 1755000000)
	if got != 0 {
		t.Fatalf("retry signal = %d, want 0", got)
	}

	events := parseSSEEvents(t, rec.Body.String())
	wantTypes := []string{
		"message_start", "content_block_start", "content_block_delta",
		"content_block_delta", "content_block_stop", "message_delta",
		"message_stop", "",
	}
	if len(events) != len(wantTypes) {
		t.Fatalf("event count = %d, want %d: %v", len(events), len(wantTypes), events)
	}
	for i, wt := range wantTypes {
		if events[i][0] != wt {
			t.Fatalf("event %d = %q, want %q", i, events[i][0], wt)
		}
	}

	// 内容校验：首个 content_block_delta 是 text_delta，message_delta 带 stop_reason
	var delta struct {
		Type  string `json:"type"`
		Delta struct {
			Type string `json:"type"`
			Text string `json:"text"`
		} `json:"delta"`
	}
	if err := json.Unmarshal([]byte(events[2][1]), &delta); err != nil {
		t.Fatalf("unmarshal delta event: %v", err)
	}
	if delta.Delta.Type != "text_delta" || delta.Delta.Text != "你好" {
		t.Fatalf("delta wrong: %+v", delta)
	}
	var msgDelta struct {
		Type  string `json:"type"`
		Delta struct {
			StopReason string `json:"stop_reason"`
		} `json:"delta"`
		Usage struct {
			OutputTokens int `json:"output_tokens"`
		} `json:"usage"`
	}
	if err := json.Unmarshal([]byte(events[5][1]), &msgDelta); err != nil {
		t.Fatalf("unmarshal message_delta: %v", err)
	}
	if msgDelta.Delta.StopReason != "end_turn" || msgDelta.Usage.OutputTokens != 6 {
		t.Fatalf("message_delta wrong: %+v", msgDelta)
	}
}

// TestHandleMessages_NonStreamE2E 端到端：假上游返回 SSE，验证整条链路的
// Anthropic 非流式输出。
func TestHandleMessages_NonStreamE2E(t *testing.T) {
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/alpha/generate" {
			t.Errorf("upstream path = %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		body := ccTextStream()
		io.Copy(w, body)
	}))
	defer upstream.Close()

	p := NewProxy("test-key")
	p.BaseURL = upstream.URL

	body := `{"model":"deepseek-v4-flash","max_tokens":1024,"system":"sys","messages":[{"role":"user","content":"hi"}]}`
	req := httptest.NewRequest(http.MethodPost, "/v1/messages", strings.NewReader(body))
	req.Header.Set("Authorization", "Bearer test-key")
	rec := httptest.NewRecorder()
	p.HandleMessages(rec, req)

	if rec.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", rec.Code, rec.Body.String())
	}
	var resp api.AnthropicMessageResponse
	if err := json.Unmarshal(rec.Body.Bytes(), &resp); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if len(resp.Content) != 1 || resp.Content[0].Text != "你好，世界" {
		t.Fatalf("content wrong: %+v", resp.Content)
	}
	if resp.Model != "deepseek-v4-flash" {
		t.Fatalf("model = %q, want mapped deepseek-v4-flash（官方裸名即完整 ID）", resp.Model)
	}
}
