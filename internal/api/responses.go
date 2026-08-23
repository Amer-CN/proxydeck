package api

// OpenAI Responses API types (client-facing)

type ResponseContentPart struct {
	Type        string `json:"type"` // "output_text"
	Text        string `json:"text"`
	Annotations []any  `json:"annotations"`
}

// OpenAIResponseItem output 数组元素：message（含 output_text）或 function_call。
type OpenAIResponseItem struct {
	ID        string                `json:"id"`
	Type      string                `json:"type"`
	Status    string                `json:"status"`
	Role      string                `json:"role,omitempty"`
	Content   []ResponseContentPart `json:"content,omitempty"`
	CallID    string                `json:"call_id,omitempty"`
	Name      string                `json:"name,omitempty"`
	Arguments string                `json:"arguments,omitempty"`
}

type OpenAIResponse struct {
	ID        string               `json:"id"`
	Object    string               `json:"object"`
	CreatedAt int64                `json:"created_at"`
	Status    string               `json:"status"`
	Model     string               `json:"model"`
	Output    []OpenAIResponseItem `json:"output"`
	Usage     *ResponseUsage       `json:"usage,omitempty"`
}

type ResponseUsage struct {
	InputTokens  int `json:"input_tokens"`
	OutputTokens int `json:"output_tokens"`
	TotalTokens  int `json:"total_tokens"`
}
