package comate

import (
	"encoding/json"
	"strings"
)

// flattenMessages 把 OpenAI messages 扁平化为 zulu 会话 query 文本：
// 按顺序 role: content 拼接，system→[System]\n、user→[User]\n、assistant→[Assistant]\n；
// content 为数组时取其中 text 部分拼接；忽略 tool_calls 等复杂字段。
func flattenMessages(messages []any) string {
	var sb strings.Builder
	for _, raw := range messages {
		msg, ok := raw.(map[string]any)
		if !ok {
			continue
		}
		role, _ := msg["role"].(string)
		switch role {
		case "system":
			sb.WriteString("[System]\n")
		case "user":
			sb.WriteString("[User]\n")
		case "assistant":
			sb.WriteString("[Assistant]\n")
		default:
			sb.WriteString("[" + role + "]\n")
		}
		switch c := msg["content"].(type) {
		case string:
			sb.WriteString(c)
		case []any:
			for _, part := range c {
				if p, ok := part.(map[string]any); ok {
					if t, _ := p["text"].(string); t != "" {
						sb.WriteString(t)
					}
				}
			}
		}
		sb.WriteString("\n")
	}
	return strings.TrimRight(sb.String(), "\n")
}

// chunkJSON 构造 OpenAI 流式 chunk（delta.content 为增量文本；finish 为空时 finish_reason=null）。
func chunkJSON(id, model, delta, finish string, created int64) []byte {
	d := map[string]any{}
	if delta != "" {
		d["content"] = delta
	}
	fr := any(nil)
	if finish != "" {
		fr = finish
	}
	m := map[string]any{
		"id":      id,
		"object":  "chat.completion.chunk",
		"created": created,
		"model":   model,
		"choices": []map[string]any{{
			"index":         0,
			"delta":         d,
			"finish_reason": fr,
		}},
	}
	b, _ := json.Marshal(m)
	return b
}

// completeJSON 构造 OpenAI 非流式完整响应（上游无 usage 数据，填 0）。
func completeJSON(id, model, content string, created int64) []byte {
	m := map[string]any{
		"id":      id,
		"object":  "chat.completion",
		"created": created,
		"model":   model,
		"choices": []map[string]any{{
			"index":         0,
			"message":       map[string]any{"role": "assistant", "content": content},
			"finish_reason": "stop",
		}},
		"usage": map[string]any{
			"prompt_tokens":     0,
			"completion_tokens": 0,
			"total_tokens":      0,
		},
	}
	b, _ := json.Marshal(m)
	return b
}

// sseEvent 是上游 /api/v1/conversations/init 的 SSE data 事件。
type sseEvent struct {
	Kind    string      `json:"kind"`
	Status  string      `json:"status"`
	Content string      `json:"content"`
	Chunks  []sseChunk  `json:"chunks"`
	Element *sseElement `json:"element"`
}

type sseChunk struct {
	Kind  string `json:"kind"`
	Delta string `json:"delta"`
}

type sseElement struct {
	Type    string `json:"type"`
	Content string `json:"content"`
}
