// responses_forward.go —— Zen Responses 协议转发。
// 背景：Zen（opencode.ai/zen）的 muse-spark-1.3-contributor-free 走
// /chat/completions 必 500，/v1/responses 正常 —— 该模型只支持 Responses
// 协议。本文件把入站 chat 请求转成出站 Responses 请求，再把 Responses 结果
// 翻回 chat 格式（非流式回填 + 流式 SSE 翻译），客户端零改动。
// 请求/响应形态均经真实上游 curl 实测（2026-09-05）：
//   - 入站 items：{type:"message",role,content:[{type:"input_text"|"input_image"}]}、
//     {type:"function_call",call_id,name,arguments}、
//     {type:"function_call_output",call_id,output}；tools 为扁平 function
//     {type,name,description,parameters}（与 chat 的嵌套 function 不同）。
//   - 出站：output[] 含 reasoning（跳过）/ message（content[].output_text）/
//     function_call；usage 为 input_tokens/output_tokens/total_tokens。
//   - 流式事件：response.created → output_item.added/done →
//     response.output_text.delta → response.completed（usage 在 completed 里），
//     另有 ping（忽略）；没有 data: [DONE]，由本翻译层补发。
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

// zenResponsesModelName 当前唯一的 Zen Responses 协议模型（chat 协议必 500）。
const zenResponsesModelName = "muse-spark-1.3-contributor-free"

// isZenResponsesModel 最小白名单：base_url 指向 opencode.ai/zen 且模型为
// 实测只支持 Responses 协议的那一个；其余模型（含 Zen 其他模型）仍走 chat 协议。
// 仅作旧配置兜底：新配置一律按账号 protocol 字段分流（externalProtocolRoute），
// 保留本判定只为旧 JSON（无 protocol 字段）里的 Zen 条目不断流。
func isZenResponsesModel(prov *ExternalProvider, model string) bool {
	return model == zenResponsesModelName && strings.Contains(prov.BaseURL, "opencode.ai/zen")
}

// externalProtocolRoute 按账号配置决定出站协议分支：responses / anthropic / chat。
// protocol 字段由 Add 归一化落盘（providers.go）；旧配置无该字段（空）时回退
// isZenResponsesModel 白名单，保证 Zen 老配置仍走 responses、其余走 chat。
func externalProtocolRoute(prov *ExternalProvider, model string) string {
	switch prov.Protocol {
	case "responses":
		return "responses"
	case "anthropic":
		return "anthropic"
	case "":
		if isZenResponsesModel(prov, model) {
			return "responses"
		}
	}
	return "chat"
}

// chatToResponsesRequest 把入站 chat 请求体转成 Responses 请求体。
// stream 由调用方传入（与 body 的 stream 字段同源，以调用方为准）。
// 返回 (转换后 body, ok)；body 非法 JSON 时 ok=false。
func chatToResponsesRequest(body []byte, stream bool) ([]byte, bool) {
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
		Tools      json.RawMessage `json:"tools"`
		ToolChoice json.RawMessage `json:"tool_choice"`
		Temperature *float64       `json:"temperature"`
		TopP       *float64        `json:"top_p"`
		MaxTokens  *int64          `json:"max_tokens"`
		MaxCompletionTokens *int64 `json:"max_completion_tokens"`
	}
	if err := json.Unmarshal(body, &chat); err != nil {
		return nil, false
	}

	input := []any{}
	for i := range chat.Messages {
		msg := &chat.Messages[i]
		switch msg.Role {
		case "tool":
			// tool 消息 → function_call_output（output 必须是字符串）
			item := map[string]any{"type": "function_call_output", "call_id": msg.ToolCallID, "output": contentAsString(msg.Content)}
			input = append(input, item)
		default:
			// assistant 有 tool_calls：文本消息（若有）+ 逐个 function_call item
			if msg.Role == "assistant" && len(msg.ToolCalls) > 0 {
				if hasContent(msg.Content) {
					input = append(input, messageItem(msg.Role, msg.Content))
				}
				for _, tc := range msg.ToolCalls {
					input = append(input, map[string]any{
						"type": "function_call", "call_id": tc.ID,
						"name": tc.Function.Name, "arguments": tc.Function.Arguments,
					})
				}
				continue
			}
			if !hasContent(msg.Content) {
				continue // 空消息（如 assistant 占位）不产出 item
			}
			input = append(input, messageItem(msg.Role, msg.Content))
		}
	}

	out := map[string]any{"model": chat.Model, "input": input, "stream": stream}
	if chat.Temperature != nil {
		out["temperature"] = *chat.Temperature
	}
	if chat.TopP != nil {
		out["top_p"] = *chat.TopP
	}
	maxTok := chat.MaxCompletionTokens
	if maxTok == nil {
		maxTok = chat.MaxTokens
	}
	if maxTok != nil {
		out["max_output_tokens"] = *maxTok
	}
	if tools := chatToolsToResponses(chat.Tools); tools != nil {
		out["tools"] = tools
	}
	if tc := chatToolChoiceToResponses(chat.ToolChoice); tc != nil {
		out["tool_choice"] = tc
	}
	b, err := json.Marshal(out)
	if err != nil {
		return nil, false
	}
	return b, true
}

// hasContent 报告 chat 消息 content 是否非空（字符串非空 或 块数组非空）。
func hasContent(raw json.RawMessage) bool {
	if len(raw) == 0 || string(raw) == "null" {
		return false
	}
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s != ""
	}
	var blocks []map[string]any
	if json.Unmarshal(raw, &blocks) == nil {
		return len(blocks) > 0
	}
	return true
}

// messageItem chat 消息 → Responses message item。content 字符串或块数组
// 统一展开为 content parts（user/system 用 input_text，assistant 用
// output_text；image_url → input_image）。未识别的块原样透传（上游不支持
// 会 400，走 4xx 透传语义）。
func messageItem(role string, content json.RawMessage) map[string]any {
	parts := []any{}
	var s string
	if json.Unmarshal(content, &s) == nil {
		parts = append(parts, textPart(role, s))
	} else {
		var blocks []map[string]any
		if json.Unmarshal(content, &blocks) == nil {
			for _, b := range blocks {
				parts = append(parts, contentBlockToResponses(role, b))
			}
		}
	}
	return map[string]any{"type": "message", "role": role, "content": parts}
}

// textPart 按角色产出文本 part（assistant 历史消息用 output_text）。
func textPart(role, text string) map[string]any {
	t := "input_text"
	if role == "assistant" {
		t = "output_text"
	}
	return map[string]any{"type": t, "text": text}
}

// contentBlockToResponses 单个 chat 内容块 → Responses 内容块。
func contentBlockToResponses(role string, b map[string]any) map[string]any {
	switch t, _ := b["type"].(string); t {
	case "text":
		text, _ := b["text"].(string)
		return textPart(role, text)
	case "image_url":
		out := map[string]any{"type": "input_image"}
		if iu, ok := b["image_url"].(map[string]any); ok {
			out["image_url"] = iu["url"]
			if d, ok := iu["detail"].(string); ok && d != "" {
				out["detail"] = d
			}
		} else {
			out["image_url"] = b["image_url"] // 已经是字符串 URL 的非标形态
		}
		return out
	default:
		// input_text / input_image 等已是 Responses 形态，或其他类型原样透传
		return b
	}
}

// contentAsString chat content → 字符串（function_call_output 的 output 用）：
// 字符串原样；块数组拼 text 字段；其余 JSON 序列化。
func contentAsString(raw json.RawMessage) string {
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	var blocks []map[string]any
	if json.Unmarshal(raw, &blocks) == nil {
		parts := []string{}
		for _, b := range blocks {
			if t, _ := b["text"].(string); t != "" {
				parts = append(parts, t)
			}
		}
		return strings.Join(parts, " ")
	}
	b, err := json.Marshal(raw)
	if err != nil {
		return string(raw)
	}
	return string(b)
}

// chatToolsToResponses chat 嵌套 function tools → Responses 扁平形态；
// 非 function 类型跳过，全部跳过（含字段缺失）时返回 nil = 不带 tools。
func chatToolsToResponses(raw json.RawMessage) []any {
	if len(raw) == 0 {
		return nil
	}
	var tools []struct {
		Type     string `json:"type"`
		Function *struct {
			Name        string          `json:"name"`
			Description string          `json:"description"`
			Parameters  json.RawMessage `json:"parameters"`
			Strict      any             `json:"strict"`
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
		item := map[string]any{"type": "function", "name": t.Function.Name}
		if t.Function.Description != "" {
			item["description"] = t.Function.Description
		}
		if len(t.Function.Parameters) > 0 {
			item["parameters"] = t.Function.Parameters
		}
		if t.Function.Strict != nil {
			item["strict"] = t.Function.Strict
		}
		out = append(out, item)
	}
	if len(out) == 0 {
		return nil
	}
	return out
}

// chatToolChoiceToResponses chat tool_choice → Responses 形态：
// 字符串原样；{"type":"function","function":{"name":X}} → {"type":"function","name":X}；
// 其余（含缺失）返回 nil = 不带。
func chatToolChoiceToResponses(raw json.RawMessage) any {
	if len(raw) == 0 {
		return nil
	}
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	var obj struct {
		Type     string `json:"type"`
		Function *struct {
			Name string `json:"name"`
		} `json:"function"`
		Name string `json:"name"`
	}
	if json.Unmarshal(raw, &obj) == nil && obj.Function != nil && obj.Function.Name != "" {
		return map[string]any{"type": "function", "name": obj.Function.Name}
	}
	if obj.Name != "" && obj.Type != "" {
		return map[string]any{"type": obj.Type, "name": obj.Name}
	}
	return nil
}

// respUsage Responses 的 usage 字段。
type respUsage struct {
	InputTokens  int64 `json:"input_tokens"`
	OutputTokens int64 `json:"output_tokens"`
	TotalTokens  int64 `json:"total_tokens"`
}

// responsesToChatCompletion Responses 非流式响应 → chat completion。
// reasoning item 跳过；message 的 output_text 拼为 content；function_call item
// 转 tool_calls（finish_reason 相应变 tool_calls）。usage 按字段名映射。
// 返回 (chat body, usage, ok)；非法 JSON 时 ok=false。
func responsesToChatCompletion(respBody []byte, model string) ([]byte, *respUsage, bool) {
	var rr struct {
		ID        string `json:"id"`
		Status    string `json:"status"`
		CreatedAt int64  `json:"created_at"`
		Model     string `json:"model"`
		Output    []struct {
			Type    string `json:"type"`
			Role    string `json:"role"`
			Content []struct {
				Type string `json:"type"`
				Text string `json:"text"`
			} `json:"content"`
			CallID    string `json:"call_id"`
			ID        string `json:"id"`
			Name      string `json:"name"`
			Arguments string `json:"arguments"`
		} `json:"output"`
		Usage *respUsage `json:"usage"`
	}
	if err := json.Unmarshal(respBody, &rr); err != nil {
		return nil, nil, false
	}
	content := ""
	toolCalls := []any{}
	for _, item := range rr.Output {
		switch item.Type {
		case "message":
			texts := []string{}
			for _, p := range item.Content {
				if p.Type == "output_text" && p.Text != "" {
					texts = append(texts, p.Text)
				}
			}
			content = strings.Join(texts, "")
		case "function_call":
			cid := item.CallID
			if cid == "" {
				cid = item.ID
			}
			toolCalls = append(toolCalls, map[string]any{
				"id": cid, "type": "function",
				"function": map[string]any{"name": item.Name, "arguments": item.Arguments},
			})
		}
	}
	created := rr.CreatedAt
	if created == 0 {
		created = time.Now().Unix()
	}
	mdl := rr.Model
	if mdl == "" {
		mdl = model
	}
	finish := "stop"
	if len(toolCalls) > 0 {
		finish = "tool_calls"
	}
	message := map[string]any{"role": "assistant", "content": content}
	if len(toolCalls) > 0 {
		message["tool_calls"] = toolCalls
	}
	out := map[string]any{
		"id": rr.ID, "object": "chat.completion", "created": created, "model": mdl,
		"choices": []any{map[string]any{"index": 0, "message": message, "finish_reason": finish, "logprobs": nil}},
	}
	if rr.Usage != nil {
		out["usage"] = map[string]any{
			"prompt_tokens":     rr.Usage.InputTokens,
			"completion_tokens": rr.Usage.OutputTokens,
			"total_tokens":      rr.Usage.TotalTokens,
		}
	}
	b, err := json.Marshal(out)
	if err != nil {
		return nil, nil, false
	}
	return b, rr.Usage, true
}

// nopFlusher 无 Flusher 时的空实现（正常 http.ResponseWriter 都有 Flusher）。
type nopFlusher struct{}

func (nopFlusher) Flush() {}

// streamResponsesToChat 逐行读上游 Responses SSE，翻译成 chat SSE 写出：
// output_item.added(message) → 首块 delta{role:"assistant",content:""}；
// output_text.delta → delta{content}；output_item.done(function_call) →
// delta{tool_calls:[…]}（整调用一个 delta，客户端按 OpenAI 语义累积）；
// response.completed → 终块 finish_reason + usage + "data: [DONE]"。
// response.failed/error 或上游流异常中断时也补 [DONE] 收尾，客户端不悬挂。
// touch 每个 chunk 回调一次（registry Touch），onUsage 收 completed 的 usage。
func streamResponsesToChat(w io.Writer, fl http.Flusher, model string, body io.Reader, touch func(int64), onUsage func(in, out, total int64)) {
	if fl == nil {
		fl = nopFlusher{}
	}
	chunkID := "chatcmpl-" + newRID()
	created := time.Now().Unix()
	roleSent := false
	toolCount := 0
	done := false

	sendChunk := func(delta map[string]any, finish string, usage *respUsage) {
		ch := map[string]any{
			"id": chunkID, "object": "chat.completion.chunk", "created": created, "model": model,
			"choices": []any{map[string]any{"index": 0, "delta": delta, "finish_reason": finish, "logprobs": nil}},
		}
		if usage != nil {
			ch["usage"] = map[string]any{
				"prompt_tokens":     usage.InputTokens,
				"completion_tokens": usage.OutputTokens,
				"total_tokens":      usage.TotalTokens,
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
					Type  string `json:"type"`
					Delta string `json:"delta"`
					Item  *struct {
						Type      string `json:"type"`
						CallID    string `json:"call_id"`
						Name      string `json:"name"`
						Arguments string `json:"arguments"`
					} `json:"item"`
					Response *struct {
						Usage *respUsage `json:"usage"`
						Error *struct {
							Message string `json:"message"`
						} `json:"error"`
					} `json:"response"`
				}
				if json.Unmarshal([]byte(payload), &ev) == nil {
					switch ev.Type {
					case "response.output_item.added":
						if ev.Item != nil && ev.Item.Type == "message" {
							ensureRole()
						}
					case "response.output_text.delta":
						ensureRole()
						sendChunk(map[string]any{"content": ev.Delta}, "", nil)
					case "response.output_item.done":
						if ev.Item != nil && ev.Item.Type == "function_call" {
							sendChunk(map[string]any{"tool_calls": []any{map[string]any{
								"index": toolCount, "id": ev.Item.CallID, "type": "function",
								"function": map[string]any{"name": ev.Item.Name, "arguments": ev.Item.Arguments},
							}}}, "", nil)
							toolCount++
						}
					case "response.completed":
						if ev.Response != nil && ev.Response.Usage != nil && onUsage != nil {
							u := ev.Response.Usage
							onUsage(u.InputTokens, u.OutputTokens, u.TotalTokens)
						}
						finish := "stop"
						if toolCount > 0 {
							finish = "tool_calls"
						}
						var usage *respUsage
						if ev.Response != nil {
							usage = ev.Response.Usage
						}
						sendChunk(map[string]any{}, finish, usage)
						fmt.Fprint(w, "data: [DONE]\n\n")
						fl.Flush()
						done = true
					case "response.failed", "response.error":
						msg := "upstream stream failed"
						if ev.Response != nil && ev.Response.Error != nil && ev.Response.Error.Message != "" {
							msg = ev.Response.Error.Message
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
		// 上游流没走完 completed 就断了：补 [DONE] 收尾（客户端按 chat 语义解析）
		fmt.Fprint(w, "data: [DONE]\n\n")
		fl.Flush()
	}
}

// forwardExternalResponses Zen Responses 分支的转发主体（结构对齐
// forwardExternal：registry 计 inflight、activity 记事件、失败语义一致）。
func (s *Server) forwardExternalResponses(w http.ResponseWriter, r *http.Request, body []byte, model string, wantsStream bool, prov *ExternalProvider, started time.Time) (failStatus int) {
	pname := prov.Name
	rid := s.registry.Register(model, pname, wantsStream)
	defer s.registry.Finish(rid)

	reqBody, ok := chatToResponsesRequest(body, wantsStream)
	if !ok {
		// 转换失败（body 非法 JSON）：与网络错同语义，交调用方终态/回落
		return -1
	}
	req, err := http.NewRequestWithContext(r.Context(), http.MethodPost, externalBaseURL(prov.BaseURL)+"/responses", bytes.NewReader(reqBody))
	if err != nil {
		return -1
	}
	req.Header.Set("Authorization", "Bearer "+prov.APIKey)
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
		log.Printf("[tuanjie] external responses model=%s provider=%s err=%v", model, pname, err)
		return -1
	}
	defer resp.Body.Close()
	latency := time.Since(started).Milliseconds()
	if resp.StatusCode != http.StatusOK {
		errBody, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		s.activity.Add("error", model+" · 外部源 "+pname+" 上游返回 "+strconv.Itoa(resp.StatusCode),
			model, pname, latency, 0, resp.StatusCode)
		log.Printf("[tuanjie] external responses model=%s provider=%s status=%d", model, pname, resp.StatusCode)
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
	log.Printf("[tuanjie] external responses model=%s provider=%s status=200", model, pname)
	defer func() {
		s.activity.Add("ok", "外部源 "+pname+" · "+model+"（Responses）"+(map[bool]string{true: "（流式）", false: ""})[wantsStream],
			model, pname, time.Since(started).Milliseconds(), 0, 200)
	}()

	if wantsStream {
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		w.Header().Set("X-Accel-Buffering", "no")
		fl, _ := w.(http.Flusher)
		streamResponsesToChat(w, fl, model, resp.Body,
			func(n int64) { s.registry.Touch(rid, n) },
			func(in, out, total int64) { s.addStat(model, in, out, total) })
		return 0
	}

	respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
	s.registry.Touch(rid, int64(len(respBody)))
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("X-Accel-Buffering", "no")
	cb, usage, ok := responsesToChatCompletion(respBody, model)
	if !ok {
		// 非预期响应形态：原样透传上游 body（诊断用），不阻断
		log.Printf("[tuanjie] external responses model=%s provider=%s 非预期响应形态，原样透传", model, pname)
		_, _ = w.Write(respBody)
		return 0
	}
	_, _ = w.Write(cb)
	if usage != nil {
		s.addStat(model, usage.InputTokens, usage.OutputTokens, usage.TotalTokens)
	}
	return 0
}
