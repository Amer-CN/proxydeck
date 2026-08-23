package api

// OpenAI-compatible types (client-facing)

type OpenAIMessage struct {
	Role             string        `json:"role"`
	Content          interface{}   `json:"content,omitempty"`
	Name             string        `json:"name,omitempty"`
	ToolCalls        []ToolCall    `json:"tool_calls,omitempty"`
	ToolCallID       string        `json:"tool_call_id,omitempty"`
	Refusal          string        `json:"refusal,omitempty"`
	Audio            *MessageAudio `json:"audio,omitempty"`
	ReasoningContent string        `json:"reasoning_content,omitempty"` // DeepSeek 方言：思考内容
}

type ContentPart struct {
	Type     string    `json:"type"`
	Text     string    `json:"text,omitempty"`
	ImageURL *ImageURL `json:"image_url,omitempty"`
}

type ImageURL struct {
	URL        string `json:"url"`
	Detail     string `json:"detail,omitempty"`
	Modalities string `json:"modalities,omitempty"`
}

type ToolCall struct {
	ID       string       `json:"id"`
	Type     string       `json:"type"`
	Function FunctionCall `json:"function"`
}

type FunctionCall struct {
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

type MessageAudio struct {
	ID       string  `json:"id"`
	Data     string  `json:"data"`
	Duration float64 `json:"duration"`
}

type OpenAIChatRequest struct {
	Model               string          `json:"model"`
	Messages            []OpenAIMessage `json:"messages"`
	Temperature         *float64        `json:"temperature,omitempty"`
	MaxTokens           *int            `json:"max_tokens,omitempty"`
	MaxCompletionTokens *int            `json:"max_completion_tokens,omitempty"`
	Stream              bool            `json:"stream,omitempty"`
	StreamOptions       any             `json:"stream_options,omitempty"`
	Tools               []any           `json:"tools,omitempty"`
	ToolChoice          any             `json:"tool_choice,omitempty"`
	ParallelToolCalls   *bool           `json:"parallel_tool_calls,omitempty"`
	ResponseFormat      any             `json:"response_format,omitempty"`
	Stop                any             `json:"stop,omitempty"`
	TopP                *float64        `json:"top_p,omitempty"`
	PresencePenalty     *float64        `json:"presence_penalty,omitempty"`
	FrequencyPenalty    *float64        `json:"frequency_penalty,omitempty"`
	User                string          `json:"user,omitempty"`
}

type OpenAIResponsesRequest struct {
	Model               string   `json:"model"`
	Input               any      `json:"input"`
	Instructions        any      `json:"instructions,omitempty"`
	Temperature         *float64 `json:"temperature,omitempty"`
	MaxOutputTokens     *int     `json:"max_output_tokens,omitempty"`
	MaxTokens           *int     `json:"max_tokens,omitempty"`
	MaxCompletionTokens *int     `json:"max_completion_tokens,omitempty"`
	Stream              bool     `json:"stream,omitempty"`
	Tools               []any    `json:"tools,omitempty"`
	ToolChoice          any      `json:"tool_choice,omitempty"`
	ParallelToolCalls   *bool    `json:"parallel_tool_calls,omitempty"`
	ResponseFormat      any      `json:"response_format,omitempty"`
	Stop                any      `json:"stop,omitempty"`
	TopP                *float64 `json:"top_p,omitempty"`
	User                string   `json:"user,omitempty"`
}

type OpenAIChoice struct {
	Index        int            `json:"index"`
	Message      *OpenAIMessage `json:"message,omitempty"`
	Delta        *OpenAIDelta   `json:"delta,omitempty"`
	FinishReason *string        `json:"finish_reason,omitempty"`
}

type OpenAIDelta struct {
	Role             string                `json:"role,omitempty"`
	Content          string                `json:"content,omitempty"`
	ToolCalls        []OpenAIDeltaToolCall `json:"tool_calls,omitempty"`
	Refusal          string                `json:"refusal,omitempty"`
	ReasoningContent string                `json:"reasoning_content,omitempty"` // DeepSeek 方言：思考内容
}

type OpenAIDeltaToolCall struct {
	Index    int                  `json:"index"`
	ID       string               `json:"id,omitempty"`
	Type     string               `json:"type,omitempty"`
	Function *OpenAIDeltaFunction `json:"function,omitempty"`
}

type OpenAIDeltaFunction struct {
	Name      string `json:"name,omitempty"`
	Arguments string `json:"arguments,omitempty"`
}

type OpenAIUsage struct {
	PromptTokens     int `json:"prompt_tokens"`
	CompletionTokens int `json:"completion_tokens"`
	TotalTokens      int `json:"total_tokens"`
	// DeepSeek 方言：推理 token 明细
	CompletionTokensDetails *OpenAICompletionDetails `json:"completion_tokens_details,omitempty"`
}

type OpenAICompletionDetails struct {
	ReasoningTokens int `json:"reasoning_tokens"`
}

type OpenAIChatResponse struct {
	ID      string         `json:"id"`
	Object  string         `json:"object"`
	Created int64          `json:"created"`
	Model   string         `json:"model"`
	Choices []OpenAIChoice `json:"choices"`
	Usage   *OpenAIUsage   `json:"usage,omitempty"`
}

type OpenAIErrorResponse struct {
	Error OpenAIError `json:"error"`
}

type OpenAIError struct {
	Message string `json:"message"`
	Type    string `json:"type"`
	Param   any    `json:"param"`
	Code    any    `json:"code"`
}

type OpenAIModel struct {
	ID      string `json:"id"`
	Object  string `json:"object"`
	Created int64  `json:"created"`
	OwnedBy string `json:"owned_by"`
}

type OpenAIModelList struct {
	Object string        `json:"object"`
	Data   []OpenAIModel `json:"data"`
}
