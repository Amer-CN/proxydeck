package proxy

import (
	"encoding/json"
	"fmt"
	"strings"

	"github.com/Amer-CN/proxydeck/internal/api"
)

// estTokens 保守估算 messages 已占用的 token 数（用于上下文感知的 max_tokens 压缩）。
// 按"字符数/2"估算：中文约 0.7 token/字、代码/英文约 0.25 token/字符，除以 2
// 在任何场景都不会低估，宁可多压缩一点输出空间，也要保证请求不超模型上下文上限。
func estTokens(msgs []api.OpenAIMessage) int {
	n := 0
	for _, m := range msgs {
		switch c := m.Content.(type) {
		case string:
			n += len(c)
		case []any:
			for _, part := range c {
				if pm, ok := part.(map[string]any); ok {
					if t, ok := pm["text"].(string); ok {
						n += len(t)
					}
				}
			}
		}
		// tool 调用参数也是上下文的一部分
		for _, tc := range m.ToolCalls {
			n += len(tc.Function.Name) + len(tc.Function.Arguments)
		}
	}
	return n/2 + 64 // +64 余量
}

// Convert OpenAI messages to CommandCode format
func ConvertMessages(openAIMsgs []api.OpenAIMessage) []api.CCMessage {
	var ccMsgs []api.CCMessage
	toolNames := map[string]string{}

	for _, m := range openAIMsgs {
		for _, tc := range m.ToolCalls {
			if tc.ID != "" && tc.Function.Name != "" {
				toolNames[tc.ID] = tc.Function.Name
			}
		}

		if m.Role == "tool" {
			toolName := m.Name
			if toolName == "" {
				toolName = toolNames[m.ToolCallID]
			}
			if toolName == "" {
				toolName = "unknown"
			}
			contentStr := contentToString(m.Content)
			outputType := "text"
			if strings.HasPrefix(contentStr, "Error:") {
				outputType = "error-text"
			}
			ccMsgs = append(ccMsgs, api.CCMessage{
				Role: "tool",
				Content: []api.CCContentPart{{
					Type:       "tool-result",
					ToolCallID: strPtr(m.ToolCallID),
					ToolName:   strPtr(toolName),
					Output: &api.CCToolOutput{
						Type:  outputType,
						Value: contentStr,
					},
				}},
			})
			continue
		}

		if m.Role == "assistant" && len(m.ToolCalls) > 0 {
			contentParts := parseContent(m.Content, toolNames)
			addedTools := map[string]bool{}
			for _, part := range contentParts {
				if part.Type == "tool-call" && part.ToolCallID != nil {
					addedTools[*part.ToolCallID] = true
				}
			}
			for _, tc := range m.ToolCalls {
				if addedTools[tc.ID] {
					continue
				}
				contentParts = append(contentParts, api.CCContentPart{
					Type:       "tool-call",
					ToolCallID: strPtr(tc.ID),
					ToolName:   strPtr(tc.Function.Name),
					Input:      parseToolInput(tc.Function.Arguments),
				})
				addedTools[tc.ID] = true
			}
			ccMsgs = append(ccMsgs, api.CCMessage{Role: m.Role, Content: contentParts})
			continue
		}

		contentParts := parseContent(m.Content, toolNames)
		if len(contentParts) == 0 {
			// Skip empty messages: CommandCode rejects null content (e.g. Codex
			// history items such as reasoning summaries that carry no text).
			continue
		}
		// 上游只接受 user|assistant|tool；未知 role（如未提取的 developer、
		// 第三方客户端的自定义 role）兜底为 user，避免 400。
		role := m.Role
		if role != "user" && role != "assistant" && role != "tool" {
			role = "user"
		}
		ccMsgs = append(ccMsgs, api.CCMessage{Role: role, Content: contentParts})
	}
	return ccMsgs
}

func ConvertTools(openAITools []any) []any {
	if len(openAITools) == 0 {
		return []any{}
	}

	tools := make([]any, 0, len(openAITools))
	for _, tool := range openAITools {
		toolMap, ok := tool.(map[string]any)
		if !ok {
			continue
		}

		toolType, _ := toolMap["type"].(string)
		if toolType != "function" {
			tools = append(tools, toolMap)
			continue
		}

		fn, ok := toolMap["function"].(map[string]any)
		if !ok {
			continue
		}

		name, _ := fn["name"].(string)
		if name == "" {
			continue
		}

		inputSchema, ok := fn["parameters"].(map[string]any)
		if !ok || inputSchema == nil {
			inputSchema = map[string]any{"type": "object", "properties": map[string]any{}}
		}

		ccTool := map[string]any{"name": name, "input_schema": inputSchema}
		if description, ok := fn["description"].(string); ok && description != "" {
			ccTool["description"] = description
		}
		tools = append(tools, ccTool)
	}

	return tools
}

func parseToolInput(arguments string) any {
	if arguments == "" {
		return map[string]any{}
	}
	var input any
	if err := json.Unmarshal([]byte(arguments), &input); err != nil {
		return map[string]any{"arguments": arguments}
	}
	return input
}

func strPtr(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}

// intPointer 返回 int 的指针（注意：intPtr 已被测试文件占用，生产代码用此名）。
func intPointer(v int) *int {
	return &v
}

func contentToString(content interface{}) string {
	switch v := content.(type) {
	case nil:
		return ""
	case string:
		return v
	case []any:
		var b strings.Builder
		for _, part := range v {
			if partMap, ok := part.(map[string]any); ok {
				text := contentPartToString(partMap)
				if text != "" {
					b.WriteString(text)
				}
			}
		}
		return b.String()
	default:
		data, err := json.Marshal(v)
		if err != nil {
			return ""
		}
		return string(data)
	}
}

func contentPartToString(content any) string {
	switch v := content.(type) {
	case nil:
		return ""
	case string:
		return v
	case []any:
		var b strings.Builder
		for _, item := range v {
			if m, ok := item.(map[string]any); ok {
				b.WriteString(contentPartToString(m))
			}
		}
		return b.String()
	case map[string]any:
		for _, key := range []string{"text", "content", "output_text", "input_text", "refusal", "thinking", "redacted_thinking"} {
			if text, ok := v[key].(string); ok {
				return text
			}
		}
		if imgURL, ok := v["image_url"].(map[string]any); ok {
			if url, ok := imgURL["url"].(string); ok {
				return "[Image URL: " + url + "]"
			}
		}
		if url, ok := v["image_url"].(string); ok {
			return "[Image URL: " + url + "]"
		}
		data, err := json.Marshal(v)
		if err != nil {
			return ""
		}
		return string(data)
	default:
		return fmt.Sprint(v)
	}
}

func parseContent(content interface{}, toolNames map[string]string) []api.CCContentPart {
	switch v := content.(type) {
	case nil:
		return nil
	case string:
		if v == "" {
			return nil
		}
		return []api.CCContentPart{{Type: "text", Text: strPtr(v)}}
	case []any:
		var parts []api.CCContentPart
		for _, part := range v {
			partMap, ok := part.(map[string]any)
			if !ok {
				continue
			}
			typ, _ := partMap["type"].(string)
			switch typ {
			case "text", "input_text", "output_text", "refusal", "thinking", "redacted_thinking", "reasoning", "document", "search_result":
				if text := contentPartToString(partMap); text != "" {
					parts = append(parts, api.CCContentPart{Type: "text", Text: strPtr(text)})
				}
			case "image_url", "input_image", "image":
				if text := contentPartToString(partMap); text != "" {
					parts = append(parts, api.CCContentPart{Type: "text", Text: strPtr(text)})
				}
			case "tool_use", "tool-call":
				id, _ := partMap["id"].(string)
				if id == "" {
					id, _ = partMap["toolCallId"].(string)
				}
				if id == "" {
					id, _ = partMap["tool_use_id"].(string)
				}
				name, _ := partMap["name"].(string)
				if name == "" {
					name, _ = partMap["toolName"].(string)
				}
				if id != "" && name != "" {
					toolNames[id] = name
				}
				input := partMap["input"]
				if input == nil {
					input = partMap["arguments"]
				}
				parts = append(parts, api.CCContentPart{
					Type:       "tool-call",
					ToolCallID: strPtr(id),
					ToolName:   strPtr(name),
					Input:      input,
				})
			case "tool_result", "tool-result":
				toolID, _ := partMap["tool_use_id"].(string)
				if toolID == "" {
					toolID, _ = partMap["toolCallId"].(string)
				}
				toolName, _ := partMap["toolName"].(string)
				if toolName == "" {
					toolName = toolNames[toolID]
				}
				if toolName == "" {
					toolName = "unknown"
				}
				contentVal := contentPartToString(partMap["content"])
				if contentVal == "" {
					contentVal = contentPartToString(partMap["output"])
				}
				outputType := "text"
				if strings.HasPrefix(contentVal, "Error:") {
					outputType = "error-text"
				}
				parts = append(parts, api.CCContentPart{
					Type:       "tool-result",
					ToolCallID: strPtr(toolID),
					ToolName:   strPtr(toolName),
					Output: &api.CCToolOutput{
						Type:  outputType,
						Value: contentVal,
					},
				})
			}
		}
		return parts
	default:
		return []api.CCContentPart{{Type: "text", Text: strPtr(contentToString(v))}}
	}
}

// Extract system message and remaining messages
func ExtractSystem(msgs []api.OpenAIMessage) (string, []api.OpenAIMessage) {
	var system strings.Builder
	var rest []api.OpenAIMessage
	for _, m := range msgs {
		// "developer" 是 OpenAI/DeepSeek 对 system 的新命名（DSH 等框架用它做
		// 系统提示），必须与 system 同等提取——否则会原样转发给上游，
		// 而 CommandCode 的 messages[].role 只接受 user|assistant|tool。
		if m.Role == "system" || m.Role == "developer" {
			if system.Len() > 0 {
				system.WriteString("\n")
			}
			system.WriteString(contentToString(m.Content))
		} else {
			rest = append(rest, m)
		}
	}
	return system.String(), rest
}
