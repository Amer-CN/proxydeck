package api

// Anthropic Messages API types (client-facing)

type AnthropicMessageRequest struct {
	Model         string              `json:"model"`
	Messages      []AnthropicMessage  `json:"messages"`
	MaxTokens     *int                `json:"max_tokens,omitempty"`
	System        any                 `json:"system,omitempty"` // string 或 blocks 数组
	Temperature   *float64            `json:"temperature,omitempty"`
	TopP          *float64            `json:"top_p,omitempty"`
	StopSequences []string            `json:"stop_sequences,omitempty"`
	Stream        bool                `json:"stream,omitempty"`
	Tools         []any               `json:"tools,omitempty"`
	ToolChoice    any                 `json:"tool_choice,omitempty"`
}

type AnthropicMessage struct {
	Role    string `json:"role"`
	Content any    `json:"content"` // string 或 content blocks 数组
}

// AnthropicMessageResponse 非流式响应。stop_reason/stop_sequence 序列化为 null
// 而不是省略，与 Anthropic 原生格式一致。
type AnthropicMessageResponse struct {
	ID           string                  `json:"id"`
	Type         string                  `json:"type"`
	Role         string                  `json:"role"`
	Model        string                  `json:"model"`
	Content      []AnthropicContentBlock `json:"content"`
	StopReason   *string                 `json:"stop_reason"`
	StopSequence *string                 `json:"stop_sequence"`
	Usage        AnthropicUsage          `json:"usage"`
}

// AnthropicContentBlock 响应 content 块：text 或 tool_use。
type AnthropicContentBlock struct {
	Type  string `json:"type"`
	Text  string `json:"text,omitempty"`
	ID    string `json:"id,omitempty"`
	Name  string `json:"name,omitempty"`
	Input any    `json:"input,omitempty"`
}

type AnthropicUsage struct {
	InputTokens  int `json:"input_tokens"`
	OutputTokens int `json:"output_tokens"`
}

type AnthropicErrorResponse struct {
	Type  string         `json:"type"`
	Error AnthropicError `json:"error"`
}

type AnthropicError struct {
	Type    string `json:"type"`
	Message string `json:"message"`
}
