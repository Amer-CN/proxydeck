package proxy

import (
	"testing"

	"github.com/Amer-CN/proxydeck/internal/api"
)

func TestMaxTokensClampedToAPIlimit(t *testing.T) {
	p := NewProxy("")

	tests := []struct {
		name       string
		maxTokens  *int
		maxCompTok *int
		want       int
	}{
		{"default when unset", nil, nil, 64000},
		{"small value passes through", intPtr(1000), nil, 1000},
		{"exactly at limit passes", intPtr(200000), nil, 200000},
		{"huge value clamped", intPtr(1000000), nil, 200000},
		{"huge max_completion_tokens clamped", nil, intPtr(1000000), 200000},
		{"max_completion_tokens wins over max_tokens", intPtr(5000), intPtr(999999), 200000},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			req := api.OpenAIChatRequest{
				Model:               "deepseek-v4-flash",
				Messages:            []api.OpenAIMessage{{Role: "user", Content: "hi"}},
				MaxTokens:           tt.maxTokens,
				MaxCompletionTokens: tt.maxCompTok,
			}
			ccBody, err := p.BuildRequest(req)
			if err != nil {
				t.Fatalf("BuildRequest failed: %v", err)
			}
			if ccBody.Params.MaxTokens != tt.want {
				t.Fatalf("expected max_tokens=%d, got %d", tt.want, ccBody.Params.MaxTokens)
			}
		})
	}
}

func intPtr(v int) *int {
	return &v
}

// TestBuildRequestContextAwareMaxTokens: 超长对话（messages 接近 1M 上限）时，
// max_tokens 必须被压缩到剩余空间，而不是固定 200000——否则上游以 context
// length 拒绝整请求，表现为"模型未返回任何内容"。
func TestBuildRequestContextAwareMaxTokens(t *testing.T) {
	p := NewProxy("")
	// ~180 万字符（估算 ~90 万 token）：剩余空间 < 20 万 → 触发上下文压缩
	big := make([]byte, 1_800_000)
	for i := range big {
		big[i] = 'a'
	}
	req := api.OpenAIChatRequest{
		Model: "deepseek-v4-flash",
		Messages: []api.OpenAIMessage{
			{Role: "user", Content: string(big)},
		},
		MaxTokens: intPtr(384000),
	}
	cc, err := p.BuildRequest(req)
	if err != nil {
		t.Fatalf("BuildRequest: %v", err)
	}
	want := 1048576 - estTokens(req.Messages)
	if cc.Params.MaxTokens != want {
		t.Fatalf("max_tokens = %d, want %d (context-aware)", cc.Params.MaxTokens, want)
	}
	if cc.Params.MaxTokens > 200000 {
		t.Fatalf("max_tokens still above 200000: %d", cc.Params.MaxTokens)
	}
}

// TestBuildRequestNormalMaxTokens: 普通请求（上下文远未满）保持客户端指定值。
func TestBuildRequestNormalMaxTokens(t *testing.T) {
	p := NewProxy("")
	req := api.OpenAIChatRequest{
		Model: "deepseek-v4-flash",
		Messages: []api.OpenAIMessage{
			{Role: "user", Content: "hi"},
		},
		MaxTokens: intPtr(8000),
	}
	cc, err := p.BuildRequest(req)
	if err != nil {
		t.Fatalf("BuildRequest: %v", err)
	}
	if cc.Params.MaxTokens != 8000 {
		t.Fatalf("max_tokens = %d, want 8000", cc.Params.MaxTokens)
	}
}

// TestParseContextOverflow: 解析上游真实报错文本里的 messages token 数。
func TestParseContextOverflow(t *testing.T) {
	msg := "This model's maximum context length is 1048576 tokens. However, " +
		"you requested 1048593 tokens (848593 in the messages, 200000 in the " +
		"completion). Please reduce the length of the messages or completion."
	if got := parseContextOverflow(msg); got != 848593 {
		t.Fatalf("parseContextOverflow = %d, want 848593", got)
	}
	if got := parseContextOverflow("unrelated error"); got != 0 {
		t.Fatalf("unrelated error parsed as %d, want 0", got)
	}
	if got := parseContextOverflow(""); got != 0 {
		t.Fatalf("empty string parsed as %d, want 0", got)
	}
}
