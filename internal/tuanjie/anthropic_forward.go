// anthropic_forward.go —— Anthropic Messages 协议转发。
// 背景：protocol=anthropic 的外部账号出站走原生 /v1/messages（externalBaseURL
// 已补 /v1 后缀，这里只拼 /messages）。本文件把入站 chat 请求转成 Anthropic
// Messages 请求，再把结果翻回 chat 格式（非流式回填 + 流式 SSE 翻译），客户端
// 零改动。形态要点（Anthropic Messages API）：
//   - 请求：{model, max_tokens(必填，缺省 4096), system?, messages[{role,
//     content blocks}], tools?(name/description/input_schema), tool_choice?,
//     stream?}；tool_result 嵌在 user 消息里，tool_use 嵌在 assistant 消息里，
//     user/assistant 必须交替（连续同角色上游 400，这里合并）。
//   - 出站认证：x-api-key 头 + anthropic-version 头（不用 Bearer）。
//   - 非流式响应：content[] 为 text/tool_use 块，stop_reason，usage 为
//     input_tokens/output_tokens。
//   - 流式事件：message_start → content_block_start/delta/stop →
//     message_delta（stop_reason + output_tokens）→ message_stop；另有 ping
//     （忽略）与 error 事件；没有 data: [DONE]，由本翻译层补发。
//
// 失败语义沿用 forwardExternal：返回 0 = 已向客户端写出（不可重试），
// >0/-1 = 尚未写任何字节（上游状态码 / 网络错），调用方可回落。
package tuanjie

import (
	"bufio"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"strconv"
	"strings"
	"time"
)

// anthropicVersion Anthropic Messages API 要求的版本头。
const anthropicVersion = "2023-06-01"

// anthropicDefaultMaxTokens Anthropic max_tokens 必填，chat 侧缺省时给该值。
const anthropicDefaultMaxTokens int64 = 4096

// chatToAnthropicRequest 把入站 chat 请求体转成 Anthropic Messages 请求体。
// stream 由调用方传入（与 body 的 stream 字段同源，以调用方为准）。
// 返回 (转换后 body, ok)；body 非法 JSON 时 ok=false。
func chatToAnthropicRequest(body []byte, stream bool) ([]byte, bool) {
	var chat struct {
		Model    string `json:"model"`
		Messages []struct {
			Role       string          `json:"role"`
			Content    json.RawMessage `json:"content"`
			ToolCallID string          `json:"tool_call_id"`
			ToolCalls  []struct {
				ID       string `json:"id"`
				Type     string `json:"type"`
				Function struct {
					Name      string `json:"name"`
					Arguments string `json:"arguments"`
				} `json:"function"`
			} `json:"tool_calls"`
		} `json:"messages"`
		Tools               json.RawMessage `json:"tools"`
		ToolChoice          json.RawMessage `json:"tool_choice"`
		Temperature         *float64        `json:"temperature"`
		TopP                *float64        `json:"top_p"`
		MaxTokens           *int64          `json:"max_tokens"`
		MaxCompletionTokens *int64          `json:"max_completion_tokens"`
	}
	if err := json.Unmarshal(body, &chat); err != nil {
		return nil, false
	}

	systemParts := []string{}
	messages := []map[string]any{}
	// appendMsg 追加一条消息；Anthropic 要求 user/assistant 交替（tool_result
	// 落 user 角色），连续同角色合并进上一条，避免上游 400。
	appendMsg := func(role string, blocks []any) {
		if n := len(messages); n > 0 {
			last := messages[n-1]
			if last["role"] == role {
				lb, _ := last["content"].([]any)
				last["content"] = append(lb, blocks...)
				return
			}
		}
		messages = append(messages, map[string]any{"role": role, "content": blocks})
	}
	for i := range chat.Messages {
		msg := &chat.Messages[i]
		switch msg.Role {
		case "system":
			// system 消息不进 messages：提取文本归一为顶层 system 字符串
			if t := contentAsString(msg.Content); t != "" {
				systemParts = append(systemParts, t)
			}
		case "tool":
			// tool 消息 → user 消息内的 tool_result 块（content 必须是字符串）
			appendMsg("user", []any{map[string]any{
				"type": "tool_result", "tool_use_id": msg.ToolCallID,
				"content": contentAsString(msg.Content),
			}})
		default:
			// assistant 有 tool_calls：文本块（若有）+ 逐个 tool_use 块
			if msg.Role == "assistant" && len(msg.ToolCalls) > 0 {
				blocks := []any{}
				if s := contentAsString(msg.Content); s != "" {
					blocks = append(blocks, map[string]any{"type": "text", "text": s})
				}
				for _, tc := range msg.ToolCalls {
					blocks = append(blocks, map[string]any{
						"type": "tool_use", "id": tc.ID, "name": tc.Function.Name,
						"input": anthropicToolInput(tc.Function.Arguments),
					})
				}
				appendMsg(msg.Role, blocks)
				continue
			}
			if !hasContent(msg.Content) {
				continue // 空消息（如 assistant 占位）不产出消息
			}
			appendMsg(msg.Role, contentBlocksToAnthropic(msg.Content))
		}
	}

	maxTok := chat.MaxCompletionTokens
	if maxTok == nil {
		maxTok = chat.MaxTokens
	}
	maxTokens := anthropicDefaultMaxTokens
	if maxTok != nil {
		maxTokens = *maxTok
	}
	out := map[string]any{
		"model": chat.Model, "max_tokens": maxTokens,
		"messages": messages, "stream": stream,
	}
	if sys := strings.Join(systemParts, "\n\n"); sys != "" {
		out["system"] = sys
	}
	if chat.Temperature != nil {
		out["temperature"] = *chat.Temperature
	}
	if chat.TopP != nil {
		out["top_p"] = *chat.TopP
	}
	if tools := chatToolsToAnthropic(chat.Tools); tools != nil {
		out["tools"] = tools
	}
	if tc := chatToolChoiceToAnthropic(chat.ToolChoice); tc != nil {
		out["tool_choice"] = tc
	}
	b, err := json.Marshal(out)
	if err != nil {
		return nil, false
	}
	return b, true
}

// contentBlocksToAnthropic chat content（字符串或块数组）→ Anthropic 内容块数组。
func contentBlocksToAnthropic(content json.RawMessage) []any {
	var s string
	if json.Unmarshal(content, &s) == nil {
		return []any{map[string]any{"type": "text", "text": s}}
	}
	var blocks []map[string]any
	if json.Unmarshal(content, &blocks) != nil {
		return nil
	}
	out := []any{}
	for _, b := range blocks {
		out = append(out, chatBlockToAnthropic(b))
	}
	return out
}

// chatBlockToAnthropic 单个 chat 内容块 → Anthropic 内容块。
// 未识别的块原样透传（上游不支持会 400，走 4xx 透传语义，与 responses 路一致）。
func chatBlockToAnthropic(b map[string]any) map[string]any {
	switch t, _ := b["type"].(string); t {
	case "text":
		text, _ := b["text"].(string)
		return map[string]any{"type": "text", "text": text}
	case "image_url":
		url := ""
		if iu, ok := b["image_url"].(map[string]any); ok {
			url, _ = iu["url"].(string)
		} else if s, ok := b["image_url"].(string); ok {
			url = s // 已经是字符串 URL 的非标形态
		}
		return anthropicImageBlock(url)
	default:
		return b
	}
}

// anthropicImageBlock chat image_url → Anthropic image 块：
// data URL（base64）→ source{type:base64, media_type, data}，media_type 取自
// data URL（缺 image/png）；http(s) URL → source{type:url, url}。
func anthropicImageBlock(url string) map[string]any {
	if mt, data, ok := strings.Cut(url, ","); ok && strings.HasPrefix(mt, "data:") && strings.Contains(mt, ";base64") {
		mediaType := strings.SplitN(strings.TrimPrefix(mt, "data:"), ";", 2)[0]
		if mediaType == "" {
			mediaType = "image/png"
		}
		return map[string]any{"type": "image", "source": map[string]any{
			"type": "base64", "media_type": mediaType, "data": data,
		}}
	}
	return map[string]any{"type": "image", "source": map[string]any{"type": "url", "url": url}}
}

// anthropicToolInput chat tool_calls 的 arguments 字符串 → Anthropic tool_use
// 的 input 对象（必填）：合法 JSON 对象原样用；空串/非法/非对象给空对象。
func anthropicToolInput(arguments string) any {
	if arguments != "" {
		var m map[string]any
		if json.Unmarshal([]byte(arguments), &m) == nil {
			return m
		}
	}
	return map[string]any{}
}

// chatToolsToAnthropic chat 嵌套 function tools → Anthropic 形态
// （name/description/input_schema）；非 function 类型跳过，全部跳过时返回
// nil = 不带 tools。input_schema 必填，parameters 缺失时给最小合法 schema。
func chatToolsToAnthropic(raw json.RawMessage) []any {
	if len(raw) == 0 {
		return nil
	}
	var tools []struct {
		Type     string `json:"type"`
		Function *struct {
			Name        string          `json:"name"`
			Description string          `json:"description"`
			Parameters  json.RawMessage `json:"parameters"`
		} `json:"function"`
	}
	if json.Unmarshal(raw, &tools) != nil {
		return nil
	}
	out := []any{}
	for _, t := range tools {
		if t.Type != "function" || t.Function == nil {
			continue
		}
		item := map[string]any{"name": t.Function.Name}
		if t.Function.Description != "" {
			item["description"] = t.Function.Description
		}
		if len(t.Function.Parameters) > 0 {
			item["input_schema"] = t.Function.Parameters
		} else {
			item["input_schema"] = map[string]any{"type": "object"}
		}
		out = append(out, item)
	}
	if len(out) == 0 {
		return nil
	}
	return out
}

// chatToolChoiceToAnthropic chat tool_choice → Anthropic 形态：
// "auto" → auto、"required" → any、{"type":"function","function":{"name":X}} →
// {"type":"tool","name":X}；"none"/其余（Anthropic 无对应形态）返回 nil = 不带。
func chatToolChoiceToAnthropic(raw json.RawMessage) any {
	if len(raw) == 0 {
		return nil
	}
	var s string
	if json.Unmarshal(raw, &s) == nil {
		switch s {
		case "auto":
			return map[string]any{"type": "auto"}
		case "required":
			return map[string]any{"type": "any"}
		}
		return nil
	}
	var obj struct {
		Type     string `json:"type"`
		Function *struct {
			Name string `json:"name"`
		} `json:"function"`
	}
	if json.Unmarshal(raw, &obj) == nil && obj.Function != nil && obj.Function.Name != "" {
		return map[string]any{"type": "tool", "name": obj.Function.Name}
	}
	return nil
}

// anthropicUsage Anthropic 的 usage 字段（无 total，chat 口径 total=in+out）。
type anthropicUsage struct {
	InputTokens  int64 `json:"input_tokens"`
	OutputTokens int64 `json:"output_tokens"`
}

// anthropicStopToFinish Anthropic stop_reason → chat finish_reason：
// max_tokens → length、tool_use → tool_calls、其余（end_turn/stop_sequence/空）→ stop。
func anthropicStopToFinish(stop string) string {
	switch stop {
	case "max_tokens":
		return "length"
	case "tool_use":
		return "tool_calls"
	default:
		return "stop"
	}
}

// anthropicToChatCompletion Anthropic Messages 非流式响应 → chat completion。
// content 的 text 块拼接为 content；tool_use 块转 tool_calls（finish_reason
// 相应变 tool_calls）。usage 按字段名映射（total=in+out）。
// 返回 (chat body, usage, ok)；非法 JSON 时 ok=false。
func anthropicToChatCompletion(respBody []byte, model string) ([]byte, *anthropicUsage, bool) {
	var ar struct {
		ID      string `json:"id"`
		Model   string `json:"model"`
		Content []struct {
			Type  string          `json:"type"`
			Text  string          `json:"text"`
			ID    string          `json:"id"`
			Name  string          `json:"name"`
			Input json.RawMessage `json:"input"`
		} `json:"content"`
		StopReason string          `json:"stop_reason"`
		Usage      *anthropicUsage `json:"usage"`
	}
	if err := json.Unmarshal(respBody, &ar); err != nil {
		return nil, nil, false
	}
	content := ""
	toolCalls := []any{}
	for _, b := range ar.Content {
		switch b.Type {
		case "text":
			content += b.Text
		case "tool_use":
			args := "{}"
			if len(b.Input) > 0 && json.Valid(b.Input) {
				args = string(b.Input)
			}
			toolCalls = append(toolCalls, map[string]any{
				"id": b.ID, "type": "function",
				"function": map[string]any{"name": b.Name, "arguments": args},
			})
		}
	}
	mdl := ar.Model
	if mdl == "" {
		mdl = model
	}
	finish := anthropicStopToFinish(ar.StopReason)
	if len(toolCalls) > 0 {
		finish = "tool_calls"
	}
	message := map[string]any{"role": "assistant", "content": content}
	if len(toolCalls) > 0 {
		message["tool_calls"] = toolCalls
	}
	out := map[string]any{
		"id": ar.ID, "object": "chat.completion", "created": time.Now().Unix(), "model": mdl,
		"choices": []any{map[string]any{"index": 0, "message": message, "finish_reason": finish, "logprobs": nil}},
	}
	if ar.Usage != nil {
		out["usage"] = map[string]any{
			"prompt_tokens":     ar.Usage.InputTokens,
			"completion_tokens": ar.Usage.OutputTokens,
			"total_tokens":      ar.Usage.InputTokens + ar.Usage.OutputTokens,
		}
	}
	b, err := json.Marshal(out)
	if err != nil {
		return nil, nil, false
	}
	return b, ar.Usage, true
}

// streamAnthropicToChat 逐行读上游 Anthropic Messages SSE，翻译成 chat SSE 写出：
// message_start → 首块 delta{role:"assistant",content:""}（记 input_tokens）；
// content_block_start(text) → ensureRole；content_block_start(tool_use) →
// tool_calls 首块（id/name/空 arguments）；content_block_delta 的 text_delta →
// delta{content}、input_json_delta → tool_calls arguments 增量（客户端按
// OpenAI 语义累积）；message_delta 记 stop_reason + output_tokens；
// message_stop → 终块 finish_reason + usage + "data: [DONE]"。
// error 事件或上游流异常中断时也补 [DONE] 收尾，客户端不悬挂。
// touch 每个 chunk 回调一次（registry Touch），onUsage 收 message_stop 的 usage。
func streamAnthropicToChat(w io.Writer, fl http.Flusher, model string, body io.Reader, touch func(int64), onUsage func(in, out, total int64)) {
	if fl == nil {
		fl = nopFlusher{}
	}
	chunkID := "chatcmpl-" + newRID()
	created := time.Now().Unix()
	roleSent := false
	toolCount := 0
	toolIdx := map[int]int{} // 上游 content block index → chat tool_calls index
	inTokens, outTokens := int64(0), int64(0)
	stopReason := ""
	done := false

	sendChunk := func(delta map[string]any, finish string, usage *anthropicUsage) {
		ch := map[string]any{
			"id": chunkID, "object": "chat.completion.chunk", "created": created, "model": model,
			"choices": []any{map[string]any{"index": 0, "delta": delta, "finish_reason": finish, "logprobs": nil}},
		}
		if usage != nil {
			ch["usage"] = map[string]any{
				"prompt_tokens":     usage.InputTokens,
				"completion_tokens": usage.OutputTokens,
				"total_tokens":      usage.InputTokens + usage.OutputTokens,
			}
		}
		b, err := json.Marshal(ch)
		if err != nil {
			return
		}
		n, _ := fmt.Fprintf(w, "data: %s\n\n", b)
		if touch != nil {
			touch(int64(n))
		}
		fl.Flush()
	}
	ensureRole := func() {
		if roleSent {
			return
		}
		roleSent = true
		sendChunk(map[string]any{"role": "assistant", "content": ""}, "", nil)
	}

	br := bufio.NewReaderSize(body, 64*1024)
	for {
		line, rerr := br.ReadString('\n')
		trimmed := strings.TrimSpace(line)
		if payload, ok := strings.CutPrefix(trimmed, "data:"); ok {
			payload = strings.TrimSpace(payload)
			if payload != "" && payload != "[DONE]" {
				var ev struct {
					Type    string `json:"type"`
					Index   int    `json:"index"`
					Message *struct {
						Usage *anthropicUsage `json:"usage"`
					} `json:"message"`
					ContentBlock *struct {
						Type string `json:"type"`
						ID   string `json:"id"`
						Name string `json:"name"`
					} `json:"content_block"`
					Delta *struct {
						Type        string `json:"type"`
						Text        string `json:"text"`
						PartialJSON string `json:"partial_json"`
						StopReason  string `json:"stop_reason"`
					} `json:"delta"`
					Usage *anthropicUsage `json:"usage"`
					Error *struct {
						Message string `json:"message"`
					} `json:"error"`
				}
				if json.Unmarshal([]byte(payload), &ev) == nil {
					switch ev.Type {
					case "message_start":
						if ev.Message != nil && ev.Message.Usage != nil {
							inTokens = ev.Message.Usage.InputTokens
						}
						ensureRole()
					case "content_block_start":
						if ev.ContentBlock == nil {
							break
						}
						switch ev.ContentBlock.Type {
						case "text":
							ensureRole()
						case "tool_use":
							ensureRole()
							toolIdx[ev.Index] = toolCount
							sendChunk(map[string]any{"tool_calls": []any{map[string]any{
								"index": toolCount, "id": ev.ContentBlock.ID, "type": "function",
								"function": map[string]any{"name": ev.ContentBlock.Name, "arguments": ""},
							}}}, "", nil)
							toolCount++
						}
					case "content_block_delta":
						if ev.Delta == nil {
							break
						}
						switch ev.Delta.Type {
						case "text_delta":
							ensureRole()
							sendChunk(map[string]any{"content": ev.Delta.Text}, "", nil)
						case "input_json_delta":
							if ti, ok := toolIdx[ev.Index]; ok {
								sendChunk(map[string]any{"tool_calls": []any{map[string]any{
									"index": ti, "function": map[string]any{"arguments": ev.Delta.PartialJSON},
								}}}, "", nil)
							}
						}
					case "message_delta":
						if ev.Delta != nil && ev.Delta.StopReason != "" {
							stopReason = ev.Delta.StopReason
						}
						if ev.Usage != nil && ev.Usage.OutputTokens > 0 {
							outTokens = ev.Usage.OutputTokens
						}
					case "message_stop":
						finish := anthropicStopToFinish(stopReason)
						if stopReason == "" && toolCount > 0 {
							finish = "tool_calls"
						}
						var usage *anthropicUsage
						if inTokens > 0 || outTokens > 0 {
							usage = &anthropicUsage{InputTokens: inTokens, OutputTokens: outTokens}
							if onUsage != nil {
								onUsage(inTokens, outTokens, inTokens+outTokens)
							}
						}
						sendChunk(map[string]any{}, finish, usage)
						fmt.Fprint(w, "data: [DONE]\n\n")
						fl.Flush()
						done = true
					case "error":
						msg := "upstream stream failed"
						if ev.Error != nil && ev.Error.Message != "" {
							msg = ev.Error.Message
						}
						errObj, _ := json.Marshal(map[string]any{"error": map[string]any{"message": msg, "type": "server_error"}})
						fmt.Fprintf(w, "data: %s\n\n", errObj)
						fmt.Fprint(w, "data: [DONE]\n\n")
						fl.Flush()
						done = true
					}
				}
			}
		}
		if rerr != nil {
			break
		}
	}
	if !done {
		// 上游流没走完 message_stop 就断了：补 [DONE] 收尾（客户端按 chat 语义解析）
		fmt.Fprint(w, "data: [DONE]\n\n")
		fl.Flush()
	}
}

// forwardExternalAnthropic Anthropic Messages 分支的转发主体（结构对齐
// forwardExternalResponses：registry 计 inflight、activity 记事件、失败语义
// 一致）。出站 POST {base}/v1/messages（externalBaseURL 已补 /v1），认证用
// Anthropic 原生头 x-api-key + anthropic-version（key 直用账号 api_key，
// Bearer 前缀如有则剥离）。
func (s *Server) forwardExternalAnthropic(w http.ResponseWriter, r *http.Request, body []byte, model string, wantsStream bool, prov *ExternalProvider, started time.Time) (failStatus int) {
	pname := prov.Name
	rid := s.registry.Register(model, pname, wantsStream)
	defer s.registry.Finish(rid)

	reqBody, ok := chatToAnthropicRequest(body, wantsStream)
	if !ok {
		// 转换失败（body 非法 JSON）：与网络错同语义，交调用方终态/回落
		return -1
	}
	req, err := http.NewRequestWithContext(r.Context(), http.MethodPost, externalBaseURL(prov.BaseURL)+"/messages", bytes.NewReader(reqBody))
	if err != nil {
		return -1
	}
	req.Header.Set("x-api-key", strings.TrimPrefix(prov.APIKey, "Bearer "))
	req.Header.Set("anthropic-version", anthropicVersion)
	req.Header.Set("Content-Type", "application/json")
	accept := "application/json"
	if wantsStream {
		accept = "text/event-stream"
	}
	req.Header.Set("Accept", accept)
	cl := &http.Client{Timeout: 120 * time.Second, Transport: smartProxyTransport}
	resp, err := cl.Do(req)
	if err != nil {
		latency := time.Since(started).Milliseconds()
		s.activity.Add("error", model+" · "+pname+" 异常: "+err.Error(), model, pname, latency, 0, 0)
		log.Printf("[tuanjie] external anthropic model=%s provider=%s err=%v", model, pname, err)
		return -1
	}
	defer resp.Body.Close()
	latency := time.Since(started).Milliseconds()
	if resp.StatusCode != http.StatusOK {
		errBody, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		s.activity.Add("error", model+" · 外部源 "+pname+" 上游返回 "+strconv.Itoa(resp.StatusCode),
			model, pname, latency, 0, resp.StatusCode)
		log.Printf("[tuanjie] external anthropic model=%s provider=%s status=%d", model, pname, resp.StatusCode)
		if shouldFallback(resp.StatusCode) {
			// 瞬时故障（5xx/429）：不写客户端，把状态报给调用方回落重试
			return resp.StatusCode
		}
		// 4xx 配置错：原样透传
		w.Header().Set("Content-Type", resp.Header.Get("Content-Type"))
		w.WriteHeader(resp.StatusCode)
		_, _ = w.Write(errBody)
		return 0
	}
	log.Printf("[tuanjie] external anthropic model=%s provider=%s status=200", model, pname)
	defer func() {
		s.activity.Add("ok", "外部源 "+pname+" · "+model+"（Anthropic）"+(map[bool]string{true: "（流式）", false: ""})[wantsStream],
			model, pname, time.Since(started).Milliseconds(), 0, 200)
	}()

	if wantsStream {
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		w.Header().Set("X-Accel-Buffering", "no")
		fl, _ := w.(http.Flusher)
		streamAnthropicToChat(w, fl, model, resp.Body,
			func(n int64) { s.registry.Touch(rid, n) },
			func(in, out, total int64) { s.addStat(model, in, out, total) })
		return 0
	}

	respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
	s.registry.Touch(rid, int64(len(respBody)))
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("X-Accel-Buffering", "no")
	cb, usage, ok := anthropicToChatCompletion(respBody, model)
	if !ok {
		// 非预期响应形态：原样透传上游 body（诊断用），不阻断
		log.Printf("[tuanjie] external anthropic model=%s provider=%s 非预期响应形态，原样透传", model, pname)
		_, _ = w.Write(respBody)
		return 0
	}
	_, _ = w.Write(cb)
	if usage != nil {
		s.addStat(model, usage.InputTokens, usage.OutputTokens, usage.InputTokens+usage.OutputTokens)
	}
	return 0
}
