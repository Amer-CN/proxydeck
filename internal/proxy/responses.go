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

// HandleResponses 实现 OpenAI Responses API（POST /v1/responses）。
// 请求转换逻辑（responsesToChatRequest）复用 chat 端点，输出改为 Responses
// 格式：非流式返回 {object:"response", output:[message/function_call], usage}；
// 流式发出 response.created / response.output_item.added /
// response.content_part.added / response.output_text.delta /
// response.output_text.done / response.output_item.done /
// response.completed 事件。Codex CLI、OpenAI SDK 等 Responses 客户端可直接使用。
func (p *Proxy) HandleResponses(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		p.writeOpenAIError(w, http.StatusMethodNotAllowed, "Method not allowed", "invalid_request_error")
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
		p.writeOpenAIError(w, http.StatusUnauthorized, "API key required. Set Authorization header.", "authentication_error")
		return
	}

	body, err := io.ReadAll(r.Body)
	if err != nil {
		p.writeOpenAIError(w, http.StatusBadRequest, "Failed to read body", "invalid_request_error")
		return
	}
	p.debugf("[DEBUG] Client responses request body: %s", truncateLog(string(body)))

	var responsesReq api.OpenAIResponsesRequest
	if err := json.Unmarshal(body, &responsesReq); err != nil {
		p.writeOpenAIError(w, http.StatusBadRequest, fmt.Sprintf("Invalid JSON: %s", err.Error()), "invalid_request_error")
		return
	}
	if responsesReq.Model == "" {
		p.writeOpenAIError(w, http.StatusBadRequest, "model is required", "invalid_request_error")
		return
	}

	chatReq := responsesToChatRequest(responsesReq)
	if len(chatReq.Messages) == 0 {
		p.writeOpenAIError(w, http.StatusBadRequest, "input is required", "invalid_request_error")
		return
	}

	ccBody, err := p.BuildRequest(chatReq)
	if err != nil {
		p.writeOpenAIError(w, http.StatusInternalServerError, "Failed to build request", "server_error")
		return
	}

	requestID := "resp_" + uuid.New().String()[:30]
	created := time.Now().Unix()
	if responsesReq.Stream {
		p.streamChatWithRetry(w, r, ccBody, apiKey, requestID, created, p.StreamResponsesResponse, p.writeOpenAIError)
	} else {
		ccResp, ok := p.callUpstreamChecked(w, r, ccBody, apiKey, p.writeOpenAIError)
		if !ok {
			return
		}
		defer ccResp.Body.Close()
		p.NonStreamResponsesResponse(w, ccResp, requestID, ccBody.Params.Model, created)
	}
}

func responsesToChatRequest(req api.OpenAIResponsesRequest) api.OpenAIChatRequest {
	messages := responsesInputToMessages(req.Input)
	if req.Instructions != nil {
		messages = append([]api.OpenAIMessage{{Role: "system", Content: req.Instructions}}, messages...)
	}

	maxTokens := req.MaxCompletionTokens
	if maxTokens == nil {
		maxTokens = req.MaxOutputTokens
	}
	if maxTokens == nil {
		maxTokens = req.MaxTokens
	}

	return api.OpenAIChatRequest{
		Model:               req.Model,
		Messages:            messages,
		Temperature:         req.Temperature,
		MaxTokens:           req.MaxTokens,
		MaxCompletionTokens: maxTokens,
		Stream:              req.Stream,
		Tools:               req.Tools,
		ToolChoice:          req.ToolChoice,
		ParallelToolCalls:   req.ParallelToolCalls,
		ResponseFormat:      req.ResponseFormat,
		Stop:                req.Stop,
		TopP:                req.TopP,
		User:                req.User,
	}
}

func responsesInputToMessages(input any) []api.OpenAIMessage {
	switch v := input.(type) {
	case nil:
		return nil
	case string:
		return []api.OpenAIMessage{{Role: "user", Content: v}}
	case []any:
		if messages := responseItemsToMessages(v); len(messages) > 0 {
			return messages
		}
		return []api.OpenAIMessage{{Role: "user", Content: v}}
	default:
		return []api.OpenAIMessage{{Role: "user", Content: v}}
	}
}

func responseItemsToMessages(items []any) []api.OpenAIMessage {
	messages := make([]api.OpenAIMessage, 0, len(items))
	for _, item := range items {
		m, ok := item.(map[string]any)
		if !ok {
			continue
		}
		role, _ := m["role"].(string)
		if role == "" {
			role = "user"
		}
		content := m["content"]
		if content == nil {
			content = m["text"]
		}
		if content == nil {
			content = m["input"]
		}
		messages = append(messages, api.OpenAIMessage{Role: role, Content: content})
	}
	return messages
}

// NonStreamResponsesResponse 把 CommandCode 响应翻译成 Responses API 非流式
// 格式：output 数组（message 项含 output_text，工具调用为 function_call 项）。
func (p *Proxy) NonStreamResponsesResponse(w http.ResponseWriter, ccResp *http.Response, requestID, model string, created int64) {
	res := p.readCCResult(ccResp.Body)

	var output []api.OpenAIResponseItem
	if res.content.Len() > 0 {
		output = append(output, api.OpenAIResponseItem{
			ID:     "msg_" + uuid.New().String()[:28],
			Type:   "message",
			Status: "completed",
			Role:   "assistant",
			Content: []api.ResponseContentPart{{
				Type:        "output_text",
				Text:        res.content.String(),
				Annotations: []any{},
			}},
		})
	}
	for _, tc := range res.toolCalls {
		output = append(output, api.OpenAIResponseItem{
			ID:        "fc_" + uuid.New().String()[:28],
			Type:      "function_call",
			Status:    "completed",
			CallID:    tc.ID,
			Name:      tc.Function.Name,
			Arguments: tc.Function.Arguments,
		})
	}

	response := api.OpenAIResponse{
		ID:        requestID,
		Object:    "response",
		CreatedAt: created,
		Status:    "completed",
		Model:     model,
		Output:    output,
		Usage: &api.ResponseUsage{
			InputTokens:  res.inputTokens,
			OutputTokens: res.outputTokens,
			TotalTokens:  res.inputTokens + res.outputTokens,
		},
	}
	// 记录本地用量统计（与 chat 端点一致）
	if res.inputTokens > 0 || res.outputTokens > 0 || res.cacheReadTokens > 0 {
		p.Stats.Record(model, int64(res.inputTokens), int64(res.outputTokens), int64(res.cacheReadTokens), int64(res.cacheWriteTokens))
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(response)
}

// responsesStreamEvent 覆盖 Responses 流式事件的公共形状。
type responsesStreamEvent struct {
	Type         string                   `json:"type"`
	Response     *api.OpenAIResponse      `json:"response,omitempty"`
	OutputIndex  *int                     `json:"output_index,omitempty"`
	Item         *api.OpenAIResponseItem  `json:"item,omitempty"`
	ItemID       string                   `json:"item_id,omitempty"`
	ContentIndex *int                     `json:"content_index,omitempty"`
	Part         *api.ResponseContentPart `json:"part,omitempty"`
	Delta        string                   `json:"delta,omitempty"`
	Text         string                   `json:"text,omitempty"`
}

// writeResponsesSSE 以 OpenAI Responses 事件格式写出 SSE（event 行 + data 行）。
func writeResponsesSSE(w io.Writer, flusher http.Flusher, event string, v any) {
	data, _ := json.Marshal(v)
	fmt.Fprintf(w, "event: %s\n", event)
	fmt.Fprintf(w, "data: %s\n\n", data)
	flusher.Flush()
}

// StreamResponsesResponse 把 CommandCode SSE 翻译成 Responses API 事件流：
// response.created → response.output_item.added → response.content_part.added →
// response.output_text.delta → response.output_text.done →
// response.output_item.done → response.completed，末尾补 "data: [DONE]"。
// 工具调用以 function_call 项 + response.function_call_arguments.delta 流出。
// 返回 >0 表示 context 超限（重试信号，与 StreamResponse 契约一致）。
func (p *Proxy) StreamResponsesResponse(w http.ResponseWriter, r *http.Request, ccResp *http.Response, requestID, model string, created int64) int {
	flusher, ok := w.(http.Flusher)
	if !ok {
		p.writeOpenAIError(w, http.StatusInternalServerError, "Streaming not supported", "server_error")
		return 0
	}

	started := false
	outputIndexCounter := 0
	textItemAdded := false
	textOutputIndex := 0
	textItemID := "msg_" + uuid.New().String()[:28]
	var textBuilder strings.Builder
	toolOrder := []string{}              // 工具 id 注册顺序（快照按序输出）
	toolIndexes := map[string]int{}      // 工具 id → output_index
	toolItems := map[string]*api.OpenAIResponseItem{}
	toolArgsBufs := map[string]*strings.Builder{}
	toolArgsStreamed := map[string]bool{} // 是否已发出 arguments delta（tool-call 可能晚到）
	lastToolID := ""                     // tool-delta 事件无 id，沿用最近注册的工具
	var inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens int64

	sendCreated := func() {
		if started {
			return
		}
		started = true
		writeResponsesSSE(w, flusher, "response.created", responsesStreamEvent{
			Type: "response.created",
			Response: &api.OpenAIResponse{
				ID:        requestID,
				Object:    "response",
				CreatedAt: created,
				Status:    "in_progress",
				Model:     model,
				Output:    []api.OpenAIResponseItem{},
			},
		})
	}
	addTextItem := func() {
		if textItemAdded {
			return
		}
		textItemAdded = true
		textOutputIndex = outputIndexCounter
		outputIndexCounter++
		sendCreated()
		writeResponsesSSE(w, flusher, "response.output_item.added", responsesStreamEvent{
			Type:        "response.output_item.added",
			OutputIndex: &textOutputIndex,
			Item: &api.OpenAIResponseItem{
				ID:      textItemID,
				Type:    "message",
				Status:  "in_progress",
				Role:    "assistant",
				Content: []api.ResponseContentPart{},
			},
		})
		writeResponsesSSE(w, flusher, "response.content_part.added", responsesStreamEvent{
			Type:         "response.content_part.added",
			ItemID:       textItemID,
			OutputIndex:  &textOutputIndex,
			ContentIndex: intPointer(0),
			Part: &api.ResponseContentPart{
				Type:        "output_text",
				Text:        "",
				Annotations: []any{},
			},
		})
	}
	registerTool := func(id, name string) {
		if _, ok := toolIndexes[id]; ok {
			return
		}
		idx := outputIndexCounter
		outputIndexCounter++
		toolIndexes[id] = idx
		toolOrder = append(toolOrder, id)
		lastToolID = id
		item := &api.OpenAIResponseItem{
			ID:        "fc_" + uuid.New().String()[:28],
			Type:      "function_call",
			Status:    "in_progress",
			CallID:    id,
			Name:      name,
			Arguments: "",
		}
		toolItems[id] = item
		sendCreated()
		writeResponsesSSE(w, flusher, "response.output_item.added", responsesStreamEvent{
			Type:        "response.output_item.added",
			OutputIndex: &idx,
			Item:        item,
		})
	}
	sendToolDelta := func(id, delta string) {
		idx, ok := toolIndexes[id]
		if !ok {
			return
		}
		toolArgsStreamed[id] = true
		buf := toolArgsBufs[id]
		if buf == nil {
			buf = &strings.Builder{}
			toolArgsBufs[id] = buf
		}
		buf.WriteString(delta)
		itemID := ""
		if it := toolItems[id]; it != nil {
			itemID = it.ID
		}
		writeResponsesSSE(w, flusher, "response.function_call_arguments.delta", responsesStreamEvent{
			Type:        "response.function_call_arguments.delta",
			ItemID:      itemID,
			OutputIndex: &idx,
			Delta:       delta,
		})
	}

	process := func(line string) error {
		p.debugf("[DEBUG] CommandCode stream line: %s", truncateLog(line))

		var event api.CCStreamEvent
		if err := json.Unmarshal([]byte(line), &event); err != nil {
			return nil
		}

		switch event.Type {
		case "text-delta":
			sendCreated()
			addTextItem()
			textBuilder.WriteString(event.Text)
			writeResponsesSSE(w, flusher, "response.output_text.delta", responsesStreamEvent{
				Type:         "response.output_text.delta",
				ItemID:       textItemID,
				OutputIndex:  &textOutputIndex,
				ContentIndex: intPointer(0),
				Delta:        event.Text,
			})

		case "reasoning-delta", "reasoning-start", "reasoning-end":
			// 思考内容：Responses 的 reasoning summary 事件非必需，跳过

		case "tool-use":
			sendCreated()
			registerTool(event.ToolCallID, event.ToolName)

		case "tool-delta":
			sendCreated()
			if lastToolID != "" {
				sendToolDelta(lastToolID, event.Text)
			}

		case "tool-input-start":
			sendCreated()
			registerTool(event.ID, event.ToolName)

		case "tool-input-delta":
			sendCreated()
			if event.ID != "" {
				sendToolDelta(event.ID, event.Delta)
			}

		case "tool-call":
			sendCreated()
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
			if event.TotalUsage != nil {
				inputTokens = int64(event.TotalUsage.InputTokens)
				outputTokens = int64(event.TotalUsage.OutputTokens)
				cacheReadTokens = int64(event.TotalUsage.CacheReadTokens)
				cacheWriteTokens = int64(event.TotalUsage.CacheWriteTokens)
			}
			sendCreated()

			// 完成文本项：output_text.done（全文）→ output_item.done
			var output []api.OpenAIResponseItem
			if textItemAdded {
				full := textBuilder.String()
				writeResponsesSSE(w, flusher, "response.output_text.done", responsesStreamEvent{
					Type:         "response.output_text.done",
					ItemID:       textItemID,
					OutputIndex:  &textOutputIndex,
					ContentIndex: intPointer(0),
					Text:         full,
				})
				textItem := &api.OpenAIResponseItem{
					ID:     textItemID,
					Type:   "message",
					Status: "completed",
					Role:   "assistant",
					Content: []api.ResponseContentPart{{
						Type:        "output_text",
						Text:        full,
						Annotations: []any{},
					}},
				}
				output = append(output, *textItem)
				writeResponsesSSE(w, flusher, "response.output_item.done", responsesStreamEvent{
					Type:        "response.output_item.done",
					OutputIndex: &textOutputIndex,
					Item:        textItem,
				})
			}

			// 完成工具项：output_item.done（含最终 arguments）
			for _, id := range toolOrder {
				item := toolItems[id]
				if buf := toolArgsBufs[id]; buf != nil {
					item.Arguments = buf.String()
				}
				item.Status = "completed"
				idx := toolIndexes[id]
				output = append(output, *item)
				writeResponsesSSE(w, flusher, "response.output_item.done", responsesStreamEvent{
					Type:        "response.output_item.done",
					OutputIndex: &idx,
					Item:        item,
				})
			}

			// response.completed：完整快照（output + usage）
			writeResponsesSSE(w, flusher, "response.completed", responsesStreamEvent{
				Type: "response.completed",
				Response: &api.OpenAIResponse{
					ID:        requestID,
					Object:    "response",
					CreatedAt: created,
					Status:    "completed",
					Model:     model,
					Output:    output,
					Usage: &api.ResponseUsage{
						InputTokens:  int(inputTokens),
						OutputTokens: int(outputTokens),
						TotalTokens:  int(inputTokens + outputTokens),
					},
				},
			})
			fmt.Fprintf(w, "data: [DONE]\n\n")
			flusher.Flush()
			// 记录流式请求的本地统计（与 chat 端点一致）
			if inputTokens > 0 || outputTokens > 0 || cacheReadTokens > 0 {
				p.Stats.Record(model, inputTokens, outputTokens, cacheReadTokens, cacheWriteTokens)
			}

		case "error":
			log.Printf("[ERROR] Stream error: %v", event.Error)
		}
		return nil
	}
	return p.streamCCEvents(w, r, ccResp, process)
}
