package proxy

import (
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"strings"
	"time"

	"github.com/Amer-CN/proxydeck/internal/api"
	"github.com/google/uuid"
)

// HandleMessages 实现 Anthropic Messages API 兼容端点（POST /v1/messages）。
// 请求（system / messages / content blocks / max_tokens / stream 等）翻译成
// OpenAI chat 请求后复用 BuildRequest 的 CommandCode 转换管线，响应与流式
// 事件再转回 Anthropic 格式。Claude Code、Cline 等 Anthropic 原生客户端可
// 直接指向本端点。
func (p *Proxy) HandleMessages(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		p.writeAnthropicError(w, http.StatusMethodNotAllowed, "Method not allowed", "invalid_request_error")
		return
	}

	// API key：客户端 Authorization 头优先，否则用服务端默认 key（同 chat 端点）
	apiKey := r.Header.Get("Authorization")
	if apiKey != "" {
		apiKey = strings.TrimPrefix(apiKey, "Bearer ")
		apiKey = strings.TrimSpace(apiKey)
	} else if p.APIKey != "" {
		apiKey = p.APIKey
	} else {
		p.writeAnthropicError(w, http.StatusUnauthorized, "API key required. Set Authorization header.", "authentication_error")
		return
	}

	body, err := io.ReadAll(r.Body)
	if err != nil {
		p.writeAnthropicError(w, http.StatusBadRequest, "Failed to read body", "invalid_request_error")
		return
	}
	p.debugf("[DEBUG] Client anthropic request body: %s", truncateLog(string(body)))

	var req api.AnthropicMessageRequest
	if err := json.Unmarshal(body, &req); err != nil {
		p.writeAnthropicError(w, http.StatusBadRequest, fmt.Sprintf("Invalid JSON: %s", err.Error()), "invalid_request_error")
		return
	}
	if req.Model == "" {
		p.writeAnthropicError(w, http.StatusBadRequest, "model is required", "invalid_request_error")
		return
	}
	if len(req.Messages) == 0 {
		p.writeAnthropicError(w, http.StatusBadRequest, "messages array is required", "invalid_request_error")
		return
	}
	// Go 套餐不含 claude-* 等模型（catalogModels.OnGo=false），在请求到达上游
	// 之前直接给出明确错误，避免上游才报错、客户端难以定位。
	if err := checkPlanModel(req.Model); err != nil {
		p.writeAnthropicError(w, http.StatusForbidden, err.Error(), "permission_error")
		return
	}

	ccBody, err := p.BuildRequest(anthropicToChatRequest(req))
	if err != nil {
		p.writeAnthropicError(w, http.StatusInternalServerError, "Failed to build request", "api_error")
		return
	}

	requestID := "msg_" + uuid.New().String()[:32]
	created := time.Now().Unix()
	if req.Stream {
		p.streamChatWithRetry(w, r, ccBody, apiKey, requestID, created, p.StreamAnthropicResponse, p.writeAnthropicError)
	} else {
		ccResp, ok := p.callUpstreamChecked(w, r, ccBody, apiKey, p.writeAnthropicError)
		if !ok {
			return
		}
		defer ccResp.Body.Close()
		p.NonStreamAnthropicResponse(w, ccResp, requestID, ccBody.Params.Model, created)
	}
}

// writeAnthropicError 按 Anthropic 错误格式写错误响应。
// errType 与 Anthropic 错误类型对齐（invalid_request_error / authentication_error /
// permission_error / api_error 等）；"server_error" 是 OpenAI 方言，映射为 api_error。
func (p *Proxy) writeAnthropicError(w http.ResponseWriter, status int, message, errType string) {
	if errType == "server_error" {
		errType = "api_error"
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(api.AnthropicErrorResponse{
		Type: "error",
		Error: api.AnthropicError{
			Type:    errType,
			Message: message,
		},
	})
}

// checkPlanModel 校验模型是否在当前套餐内：catalogModels 中 OnGo=false 的模型
// （claude-*、google/gemini-* 等）不在 Go 套餐（32 个）内，直接返回明确错误；
// 未收录的未知模型放行（与 MapModel 的透传策略一致，由上游决定是否接受）。
func checkPlanModel(model string) error {
	mapped := MapModel(model)
	for _, m := range catalogModels {
		if m.ID == mapped {
			if !m.OnGo {
				return fmt.Errorf("model %q is not included in your current plan; it requires Pro or Max (Go plan includes 32 models, see /v1/models?plan=go)", model)
			}
			return nil
		}
	}
	return nil
}

// anthropicToChatRequest 把 Anthropic Messages 请求映射为 OpenAI chat 请求，
// 复用 BuildRequest 的 CommandCode 转换管线（消息、system、tools）。
// 关键差异：Anthropic 的 tool_result 嵌在 user 消息里，这里拆成独立的
// role=tool 消息（上游只接受 user|assistant|tool）。
func anthropicToChatRequest(req api.AnthropicMessageRequest) api.OpenAIChatRequest {
	chat := api.OpenAIChatRequest{
		Model:       req.Model,
		Temperature: req.Temperature,
		TopP:        req.TopP,
		MaxTokens:   req.MaxTokens,
		Stream:      req.Stream,
		Tools:       req.Tools,
		ToolChoice:  req.ToolChoice,
		Stop:        req.StopSequences,
	}
	if req.System != nil {
		chat.Messages = append(chat.Messages, api.OpenAIMessage{
			Role:    "system",
			Content: anthropicSystemToString(req.System),
		})
	}
	// 收集 assistant 消息里的 tool_use（id → name），供 tool_result 拆解用
	toolNames := map[string]string{}
	for _, m := range req.Messages {
		if m.Role != "assistant" {
			continue
		}
		if parts, ok := m.Content.([]any); ok {
			for _, part := range parts {
				if pm, ok := part.(map[string]any); ok {
					if typ, _ := pm["type"].(string); typ == "tool_use" {
						if id, _ := pm["id"].(string); id != "" {
							if name, _ := pm["name"].(string); name != "" {
								toolNames[id] = name
							}
						}
					}
				}
			}
		}
	}

	for _, m := range req.Messages {
		if m.Role == "user" {
			// tool_result 块 → 独立 role=tool 消息；其余内容仍作 user 消息
			chat.Messages = append(chat.Messages, extractToolResults(m.Content, toolNames)...)
			if rest := dropToolResults(m.Content); rest != "" {
				chat.Messages = append(chat.Messages, api.OpenAIMessage{Role: "user", Content: rest})
			}
		} else {
			chat.Messages = append(chat.Messages, api.OpenAIMessage{Role: m.Role, Content: m.Content})
		}
	}
	return chat
}

// anthropicSystemToString 把 system 参数归一化成字符串：
// 字符串原样返回；blocks 数组拼接其中的 text 块。
func anthropicSystemToString(system any) string {
	switch v := system.(type) {
	case string:
		return v
	case []any:
		var b strings.Builder
		for _, part := range v {
			pm, ok := part.(map[string]any)
			if !ok {
				continue
			}
			if typ, _ := pm["type"].(string); typ != "text" {
				continue
			}
			if text, ok := pm["text"].(string); ok && text != "" {
				if b.Len() > 0 {
					b.WriteString("\n")
				}
				b.WriteString(text)
			}
		}
		return b.String()
	}
	return ""
}

// extractToolResults 把 Anthropic user 消息里的 tool_result 块拆成独立的
// role=tool 消息（对应 OpenAI 的 tool 消息，含 ToolCallID/Name）。
func extractToolResults(content any, toolNames map[string]string) []api.OpenAIMessage {
	parts, ok := content.([]any)
	if !ok {
		return nil
	}
	var out []api.OpenAIMessage
	for _, part := range parts {
		pm, ok := part.(map[string]any)
		if !ok {
			continue
		}
		if typ, _ := pm["type"].(string); typ != "tool_result" {
			continue
		}
		id, _ := pm["tool_use_id"].(string)
		if id == "" {
			continue
		}
		out = append(out, api.OpenAIMessage{
			Role:       "tool",
			ToolCallID: id,
			Name:       toolNames[id],
			Content:    toolResultText(pm["content"]),
		})
	}
	return out
}

// dropToolResults 去掉 user 消息里的 tool_result 块，返回剩余内容。
// 全为 tool_result（或空）时返回 ""（ConvertMessages 会跳过空消息）。
func dropToolResults(content any) any {
	parts, ok := content.([]any)
	if !ok {
		return content
	}
	rest := make([]any, 0, len(parts))
	for _, part := range parts {
		pm, ok := part.(map[string]any)
		if !ok {
			rest = append(rest, part)
			continue
		}
		if typ, _ := pm["type"].(string); typ == "tool_result" {
			continue
		}
		rest = append(rest, part)
	}
	if len(rest) == 0 {
		return ""
	}
	return rest
}

// toolResultText 提取 tool_result 的文本内容（string 或 text blocks）。
func toolResultText(content any) string {
	switch v := content.(type) {
	case string:
		return v
	case []any:
		var b strings.Builder
		for _, part := range v {
			if pm, ok := part.(map[string]any); ok {
				if text, ok := pm["text"].(string); ok {
					b.WriteString(text)
				}
			}
		}
		return b.String()
	case map[string]any:
		if text, ok := v["text"].(string); ok {
			return text
		}
	}
	return ""
}

// NonStreamAnthropicResponse 把 CommandCode 响应翻译成 Anthropic 非流式格式：
// {id:"msg_...", type:"message", role:"assistant", content:[text/tool_use 块],
// stop_reason, usage}。
func (p *Proxy) NonStreamAnthropicResponse(w http.ResponseWriter, ccResp *http.Response, requestID, model string, created int64) {
	res := p.readCCResult(ccResp.Body)

	blocks := []api.AnthropicContentBlock{}
	stopReason := "end_turn"
	if res.hasToolCalls {
		stopReason = "tool_use"
		for _, tc := range res.toolCalls {
			input := any(map[string]any{})
			if tc.Function.Arguments != "" {
				var parsed any
				if err := json.Unmarshal([]byte(tc.Function.Arguments), &parsed); err == nil {
					input = parsed
				} else {
					input = map[string]any{"arguments": tc.Function.Arguments}
				}
			}
			blocks = append(blocks, api.AnthropicContentBlock{
				Type:  "tool_use",
				ID:    tc.ID,
				Name:  tc.Function.Name,
				Input: input,
			})
		}
	} else if res.content.Len() > 0 {
		blocks = append(blocks, api.AnthropicContentBlock{Type: "text", Text: res.content.String()})
	}
	if res.finishReason == "length" || res.finishReason == "max_tokens" {
		stopReason = "max_tokens"
	}

	response := api.AnthropicMessageResponse{
		ID:         requestID,
		Type:       "message",
		Role:       "assistant",
		Model:      model,
		Content:    blocks,
		StopReason: &stopReason,
		Usage: api.AnthropicUsage{
			InputTokens:  res.inputTokens,
			OutputTokens: res.outputTokens,
		},
	}
	// 记录本地用量统计（与 chat 端点一致）
	if res.inputTokens > 0 || res.outputTokens > 0 || res.cacheReadTokens > 0 {
		p.Stats.Record(model, int64(res.inputTokens), int64(res.outputTokens), int64(res.cacheReadTokens), int64(res.cacheWriteTokens))
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(response)
}

// anthropicStreamEvent 覆盖 Anthropic 流式事件的公共形状：
// message_start（Message）/ content_block_start（Index+ContentBlock）/
// content_block_delta（Index+Delta）/ content_block_stop（Index）/
// message_delta（Delta+Usage）/ message_stop（仅 Type）。
type anthropicStreamEvent struct {
	Type         string                       `json:"type"`
	Message      *api.AnthropicMessageResponse `json:"message,omitempty"`
	Index        *int                         `json:"index,omitempty"`
	ContentBlock *api.AnthropicContentBlock   `json:"content_block,omitempty"`
	Delta        any                          `json:"delta,omitempty"`
	Usage        *api.AnthropicUsage          `json:"usage,omitempty"`
}

// writeAnthropicSSE 以 Anthropic 事件格式写出 SSE（event 行 + data 行）。
func writeAnthropicSSE(w io.Writer, flusher http.Flusher, event string, v any) {
	data, _ := json.Marshal(v)
	fmt.Fprintf(w, "event: %s\n", event)
	fmt.Fprintf(w, "data: %s\n\n", data)
	flusher.Flush()
}

// StreamAnthropicResponse 把 CommandCode SSE 翻译成 Anthropic 事件流：
// message_start → content_block_start → content_block_delta(text_delta) →
// content_block_stop → message_delta → message_stop，末尾补 "data: [DONE]"。
// 工具调用以 tool_use 块 + input_json_delta 形式流出。返回 >0 表示 context
// 超限（重试信号，与 StreamResponse 契约一致）。
func (p *Proxy) StreamAnthropicResponse(w http.ResponseWriter, r *http.Request, ccResp *http.Response, requestID, model string, created int64) int {
	flusher, ok := w.(http.Flusher)
	if !ok {
		p.writeAnthropicError(w, http.StatusInternalServerError, "Streaming not supported", "api_error")
		return 0
	}

	started := false    // message_start 是否已发出（首个内容事件时惰性触发）
	textStarted := false
	textIdx := 0
	blockIndex := 0
	blockStopOrder := []int{} // 各内容块的索引（文本 + 工具），finish 时按序补 content_block_stop
	stopReason := "end_turn"
	var outputTokens int
	var streamIn, streamOut, streamCacheRead, streamCacheWrite int64
	toolIndexes := map[string]int{}     // 工具 id → 块索引
	toolArgsStreamed := map[string]bool{} // 该工具的 input_json_delta 是否已发出（tool-call 可能晚到）
	lastToolID := ""                    // tool-delta 事件无 id，沿用最近注册的工具

	process := func(line string) error {
		p.debugf("[DEBUG] CommandCode stream line: %s", truncateLog(line))

		var event api.CCStreamEvent
		if err := json.Unmarshal([]byte(line), &event); err != nil {
			return nil
		}

		// message_start 惰性触发：所有内容事件之前必须先发
		sendStart := func() {
			if started {
				return
			}
			started = true
			writeAnthropicSSE(w, flusher, "message_start", anthropicStreamEvent{
				Type: "message_start",
				Message: &api.AnthropicMessageResponse{
					ID:      requestID,
					Type:    "message",
					Role:    "assistant",
					Model:   model,
					Content: []api.AnthropicContentBlock{},
					Usage:   api.AnthropicUsage{},
				},
			})
		}
		sendTextStart := func() {
			if textStarted {
				return
			}
			textStarted = true
			textIdx = blockIndex
			blockIndex++
			blockStopOrder = append(blockStopOrder, textIdx)
			writeAnthropicSSE(w, flusher, "content_block_start", anthropicStreamEvent{
				Type:  "content_block_start",
				Index: &textIdx,
				ContentBlock: &api.AnthropicContentBlock{
					Type: "text",
					Text: "",
				},
			})
		}
		// 注册一个工具块（tool-use / tool-input-start / tool-call 首次到达）
		registerTool := func(id, name string) {
			if _, ok := toolIndexes[id]; ok {
				return
			}
			idx := blockIndex
			blockIndex++
			toolIndexes[id] = idx
			blockStopOrder = append(blockStopOrder, idx)
			lastToolID = id
			writeAnthropicSSE(w, flusher, "content_block_start", anthropicStreamEvent{
				Type:  "content_block_start",
				Index: &idx,
				ContentBlock: &api.AnthropicContentBlock{
					Type:  "tool_use",
					ID:    id,
					Name:  name,
					Input: map[string]any{},
				},
			})
		}
		sendToolDelta := func(id, partial string) {
			idx, ok := toolIndexes[id]
			if !ok {
				return
			}
			toolArgsStreamed[id] = true
			writeAnthropicSSE(w, flusher, "content_block_delta", anthropicStreamEvent{
				Type:  "content_block_delta",
				Index: &idx,
				Delta: map[string]any{"type": "input_json_delta", "partial_json": partial},
			})
		}

		switch event.Type {
		case "text-delta":
			sendStart()
			sendTextStart()
			writeAnthropicSSE(w, flusher, "content_block_delta", anthropicStreamEvent{
				Type:  "content_block_delta",
				Index: &textIdx,
				Delta: map[string]any{"type": "text_delta", "text": event.Text},
			})

		case "reasoning-delta", "reasoning-start", "reasoning-end":
			// 思考内容在 Anthropic 协议里无直接对应事件（thinking 块），跳过

		case "tool-use":
			sendStart()
			registerTool(event.ToolCallID, event.ToolName)

		case "tool-delta":
			sendStart()
			if lastToolID != "" {
				sendToolDelta(lastToolID, event.Text)
			}

		case "tool-input-start":
			sendStart()
			registerTool(event.ID, event.ToolName)

		case "tool-input-delta":
			sendStart()
			if event.ID != "" {
				sendToolDelta(event.ID, event.Delta)
			}

		case "tool-call":
			sendStart()
			registerTool(event.ToolCallID, event.ToolName)
			if !toolArgsStreamed[event.ToolCallID] {
				args := ""
				if event.Input != nil {
					if data, err := json.Marshal(event.Input); err == nil {
						args = string(data)
					}
				}
				sendToolDelta(event.ToolCallID, args)
			}

		case "finish":
			// finishReason → Anthropic stop_reason
			switch normalizeFinishReason(event.FinishReason) {
			case "tool_calls":
				stopReason = "tool_use"
			case "length":
				stopReason = "max_tokens"
			}
			if event.TotalUsage != nil {
				outputTokens = event.TotalUsage.OutputTokens
				streamIn = int64(event.TotalUsage.InputTokens)
				streamOut = int64(event.TotalUsage.OutputTokens)
				streamCacheRead = int64(event.TotalUsage.CacheReadTokens)
				streamCacheWrite = int64(event.TotalUsage.CacheWriteTokens)
			}
			sendStart()
			// 先补发各内容块的 content_block_stop，再 message_delta / message_stop
			for _, idx := range blockStopOrder {
				writeAnthropicSSE(w, flusher, "content_block_stop", anthropicStreamEvent{
					Type:  "content_block_stop",
					Index: &idx,
				})
			}
			writeAnthropicSSE(w, flusher, "message_delta", anthropicStreamEvent{
				Type: "message_delta",
				Delta: map[string]any{
					"stop_reason":    stopReason,
					"stop_sequence":  nil,
				},
				Usage: &api.AnthropicUsage{InputTokens: 0, OutputTokens: outputTokens},
			})
			writeAnthropicSSE(w, flusher, "message_stop", anthropicStreamEvent{Type: "message_stop"})
			fmt.Fprintf(w, "data: [DONE]\n\n")
			flusher.Flush()
			// 记录流式请求的本地统计（与 chat 端点一致）
			if streamIn > 0 || streamOut > 0 || streamCacheRead > 0 {
				p.Stats.Record(model, streamIn, streamOut, streamCacheRead, streamCacheWrite)
			}

		case "error":
			log.Printf("[ERROR] Stream error: %v", event.Error)
		}
		return nil
	}
	return p.streamCCEvents(w, r, ccResp, process)
}
