package proxy

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/Amer-CN/proxydeck/internal/api"
	"github.com/Amer-CN/proxydeck/internal/version"
	"github.com/google/uuid"
)

const defaultBaseURL = "https://api.commandcode.ai"
const defaultTimeout = 300 * time.Second
const debugLogLimit = 20000

func truncateLog(s string) string {
	if len(s) <= debugLogLimit {
		return s
	}
	return s[:debugLogLimit] + fmt.Sprintf("... [truncated %d bytes]", len(s)-debugLogLimit)
}

func (p *Proxy) debugf(format string, args ...any) {
	if p.Debug {
		log.Printf(format, args...)
	}
}

func (p *Proxy) writeOpenAIError(w http.ResponseWriter, status int, message, errType string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(api.OpenAIErrorResponse{Error: api.OpenAIError{
		Message: message,
		Type:    errType,
		Param:   nil,
		Code:    nil,
	}})
}

func normalizeFinishReason(reason string) string {
	switch reason {
	case "tool_calls", "tool-calls":
		return "tool_calls"
	case "length", "max_tokens":
		return "length"
	case "content_filter", "content-filter":
		return "content_filter"
	default:
		return "stop"
	}
}

// Proxy struct
type Proxy struct {
	APIKey  string
	BaseURL string
	Client  *http.Client
	Debug   bool
	Stats   *UsageStats

	// /v1/usage 结果缓存（见 usage.go：避免 GUI 轮询同步阻塞拉官网）
	usageMu   sync.Mutex
	usageData []byte
	usageAt   time.Time
}

// NewProxy creates a new proxy instance
func NewProxy(apiKey string) *Proxy {
	return &Proxy{
		APIKey:  apiKey,
		BaseURL: defaultBaseURL,
		Client:  &http.Client{Timeout: defaultTimeout},
		Stats:   NewUsageStats(""),
	}
}

// SetStatsFile sets the disk path used to persist local usage stats.
func (p *Proxy) SetStatsFile(path string) {
	if path == "" {
		return
	}
	p.Stats.file = path
	p.Stats.load()
}

// BuildRequest builds the CommandCode request body
func (p *Proxy) BuildRequest(openAIReq api.OpenAIChatRequest) (api.CCRequestBody, error) {
	model := MapModel(openAIReq.Model)
	system, msgs := ExtractSystem(openAIReq.Messages)
	ccMessages := ConvertMessages(msgs)

	temperature := 0.3
	maxTokens := 64000
	if openAIReq.Temperature != nil {
		temperature = *openAIReq.Temperature
	}
	if openAIReq.MaxTokens != nil {
		maxTokens = *openAIReq.MaxTokens
	}
	if openAIReq.MaxCompletionTokens != nil {
		maxTokens = *openAIReq.MaxCompletionTokens
	}
	// CommandCode rejects max_tokens above 200000; clamp to the API limit
	if maxTokens > 200000 {
		maxTokens = 200000
	}
	// 上下文感知：messages 已占用的 token + 输出预算必须 ≤ 模型 1M 上下文上限。
	// 超长对话（ZCode/Codex 携带全程历史）messages 可能已接近上限，此时若仍
	// 按 200000 申请输出，上游会以 "maximum context length" 拒绝整个请求
	// （表现为"模型未返回任何内容"）。估算偏保守（字符/2），不会低估。
	if room := ccContextLimit - estTokens(msgs) - len(system)/2; room < maxTokens {
		maxTokens = room
		if maxTokens < 1024 {
			maxTokens = 1024 // 至少留一点输出空间；messages 真超限时由上游报错
		}
	}

	tools := ConvertTools(openAIReq.Tools)

	ccBody := api.CCRequestBody{
		Config: api.CCConfig{
			WorkingDir:    ".",
			Date:          time.Now().Format("2006-01-02"),
			Environment:   "cli",
			Structure:     []string{},
			IsGitRepo:     false,
			CurrentBranch: "",
			MainBranch:    "main",
			GitStatus:     "",
			RecentCommits: []string{},
		},
		Memory: "",
		Taste:  "",
		Skills: "",
		Params: api.CCChatParams{
			Model:       model,
			Messages:    ccMessages,
			Tools:       tools,
			System:      system,
			MaxTokens:   maxTokens,
			Temperature: temperature,
			Stream:      true,
		},
		ThreadID: uuid.New().String(),
	}

	return ccBody, nil
}

// CreateUpstreamRequest creates a new HTTP request to the CommandCode API
func (p *Proxy) CreateUpstreamRequest(ctx context.Context, ccBody api.CCRequestBody, apiKey string) (*http.Request, error) {
	reqJSON, err := json.Marshal(ccBody)
	if err != nil {
		return nil, fmt.Errorf("failed to build request: %w", err)
	}

	p.debugf("[DEBUG] CommandCode request body: %s", truncateLog(string(reqJSON)))

	ccReq, err := http.NewRequestWithContext(ctx, http.MethodPost,
		p.BaseURL+"/alpha/generate", bytes.NewReader(reqJSON))
	if err != nil {
		return nil, fmt.Errorf("failed to create upstream request: %w", err)
	}

	ccReq.Header.Set("Content-Type", "application/json")
	ccReq.Header.Set("Authorization", "Bearer "+apiKey)
	ccReq.Header.Set("x-command-code-version", version.GetCommandCodeVersion())
	ccReq.Header.Set("x-cli-environment", "production")
	ccReq.Header.Set("Accept", "text/event-stream")

	return ccReq, nil
}

// CallUpstream makes the request to CommandCode API
func (p *Proxy) CallUpstream(req *http.Request) (*http.Response, error) {
	resp, err := p.Client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("upstream error: %w", err)
	}
	return resp, nil
}

// HandleChatCompletions handles the /v1/chat/completions endpoint
func (p *Proxy) HandleChatCompletions(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		p.writeOpenAIError(w, http.StatusMethodNotAllowed, "Method not allowed", "invalid_request_error")
		return
	}

	// Get API key from client Authorization header or server default
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

	// Read request
	body, err := io.ReadAll(r.Body)
	if err != nil {
		p.writeOpenAIError(w, http.StatusBadRequest, "Failed to read body", "invalid_request_error")
		return
	}

	p.debugf("[DEBUG] Client request body: %s", truncateLog(string(body)))

	var openAIReq api.OpenAIChatRequest
	if err := json.Unmarshal(body, &openAIReq); err != nil {
		p.writeOpenAIError(w, http.StatusBadRequest, fmt.Sprintf("Invalid JSON: %s", err.Error()), "invalid_request_error")
		return
	}

	if len(openAIReq.Messages) == 0 {
		p.writeOpenAIError(w, http.StatusBadRequest, "messages array is required", "invalid_request_error")
		return
	}

	// Build CommandCode request
	ccBody, err := p.BuildRequest(openAIReq)
	if err != nil {
		p.writeOpenAIError(w, http.StatusInternalServerError, "Failed to build request", "server_error")
		return
	}

	requestID := "chatcmpl-" + uuid.New().String()[:29]
	created := time.Now().Unix()

	if openAIReq.Stream {
		// 上下文超限自动重试（最多 1 次）在 streamChatWithRetry 内统一处理
		p.streamChatWithRetry(w, r, ccBody, apiKey, requestID, created, p.StreamResponse, p.writeOpenAIError)
	} else {
		ccResp, ok := p.callUpstreamChecked(w, r, ccBody, apiKey, p.writeOpenAIError)
		if !ok {
			return
		}
		defer ccResp.Body.Close()
		p.NonStreamResponse(w, ccResp, requestID, ccBody.Params.Model, created)
	}
}

// callUpstreamChecked 构造并调用上游 /alpha/generate；失败时已通过 writeErr
// 写出错误响应并返回 ok=false。构建请求体的 Debug 日志也在此统一输出。
func (p *Proxy) callUpstreamChecked(w http.ResponseWriter, r *http.Request, ccBody api.CCRequestBody, apiKey string, writeErr func(w http.ResponseWriter, status int, message, errType string)) (*http.Response, bool) {
	if p.Debug {
		if ccJSON, jerr := json.Marshal(ccBody); jerr == nil {
			p.debugf("[DEBUG] CommandCode body: %s", truncateLog(string(ccJSON)))
		}
	}

	ccReq, err := p.CreateUpstreamRequest(r.Context(), ccBody, apiKey)
	if err != nil {
		writeErr(w, http.StatusInternalServerError, "Failed to create upstream request", "server_error")
		return nil, false
	}

	ccResp, err := p.CallUpstream(ccReq)
	if err != nil {
		writeErr(w, http.StatusBadGateway, err.Error(), "api_error")
		return nil, false
	}

	if ccResp.StatusCode != http.StatusOK {
		errBody, _ := io.ReadAll(ccResp.Body)
		ccResp.Body.Close()
		message := fmt.Sprintf("Upstream error: %s", string(errBody))
		log.Printf("[ERROR] Upstream returned %d: %s", ccResp.StatusCode, string(errBody))
		status := http.StatusBadGateway
		if ccResp.StatusCode >= http.StatusBadRequest && ccResp.StatusCode < http.StatusInternalServerError {
			status = ccResp.StatusCode
		}
		writeErr(w, status, message, "api_error")
		return nil, false
	}
	return ccResp, true
}

// streamChatWithRetry 公共流式路径：调用 streamFn 把上游 SSE 翻译成目标格式。
// 上游以 context 超限拒绝（streamFn 返回 >0，此时未写出任何字节）时，按上游
// 给出的精确 messages token 数压缩 max_tokens 后自动重发一次（最多 1 次）——
// estTokens 估算的精确兜底。
func (p *Proxy) streamChatWithRetry(w http.ResponseWriter, r *http.Request, ccBody api.CCRequestBody, apiKey, requestID string, created int64, streamFn func(w http.ResponseWriter, r *http.Request, ccResp *http.Response, requestID, model string, created int64) int, writeErr func(w http.ResponseWriter, status int, message, errType string)) {
	ccResp, ok := p.callUpstreamChecked(w, r, ccBody, apiKey, writeErr)
	if !ok {
		return
	}
	defer ccResp.Body.Close()

	model := ccBody.Params.Model
	if msgTokens := streamFn(w, r, ccResp, requestID, model, created); msgTokens > 0 {
		ccBody.Params.MaxTokens = ccContextLimit - msgTokens - 16
		if ccBody.Params.MaxTokens < 1024 {
			ccBody.Params.MaxTokens = 1024
		}
		log.Printf("[RETRY] context overflow retry with max_tokens=%d", ccBody.Params.MaxTokens)
		if ccResp2, ok2 := p.callUpstreamChecked(w, r, ccBody, apiKey, writeErr); ok2 {
			streamFn(w, r, ccResp2, requestID, model, created)
			ccResp2.Body.Close()
		}
	}
}

// forEachLine reads newline-delimited JSON from r and calls fn for each
// non-empty trimmed line. Unlike bufio.Scanner it has no line-length limit:
// CommandCode may emit very large tool events when continuing long threads.
func forEachLine(r io.Reader, ctx context.Context, fn func(string) error) error {
	reader := bufio.NewReader(r)
	for {
		lineBytes, err := reader.ReadBytes('\n')
		if len(lineBytes) > 0 {
			line := strings.TrimSpace(string(lineBytes))
			if line != "" {
				if ferr := fn(line); ferr != nil {
					return ferr
				}
			}
		}
		if err != nil {
			if err == io.EOF {
				return nil
			}
			return err
		}
		if ctx != nil {
			select {
			case <-ctx.Done():
				return nil
			default:
			}
		}
	}
}

// streamCCEvents 是流式转发的公共骨架：写 SSE 头之前先读上游首行，若立即报
// context 超限（长对话携带全程历史时常见）则返回重试信号（此时尚未写出任何
// 字节）；否则设置 SSE 头，把首行与其余行逐条交给 process 翻译成目标事件格式。
func (p *Proxy) streamCCEvents(w http.ResponseWriter, r *http.Request, ccResp *http.Response, process func(line string) error) int {
	reader := bufio.NewReader(ccResp.Body)
	firstLine, firstErr := readFirstLine(reader)
	if firstErr == nil && firstLine != "" {
		var ev api.CCStreamEvent
		if json.Unmarshal([]byte(firstLine), &ev) == nil && ev.Type == "error" && ev.Error != nil {
			if msgTokens := parseContextOverflow(ev.Error.Message); msgTokens > 0 {
				log.Printf("[RETRY] context overflow: %d tokens in messages, shrinking max_tokens", msgTokens)
				return msgTokens
			}
		}
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.WriteHeader(http.StatusOK)

	if firstLine != "" {
		if err := process(firstLine); err != nil {
			log.Printf("[ERROR] Stream first-line error: %v", err)
			return 0
		}
	}
	if lineErr := forEachLine(reader, r.Context(), process); lineErr != nil {
		log.Printf("[ERROR] Stream read error: %v", lineErr)
	}
	return 0
}

// StreamResponse handles streaming response from CommandCode to OpenAI SSE.
// 返回 >0 表示上游以 context 超限拒绝（返回值为 messages 已占用的 token 数），
// 此时尚未写出任何 SSE 数据，调用方可压缩 max_tokens 后安全重发（精确兜底，
// 覆盖 estTokens 估算偏差的场景）。
func (p *Proxy) StreamResponse(w http.ResponseWriter, r *http.Request, ccResp *http.Response, requestID, model string, created int64) int {
	flusher, ok := w.(http.Flusher)
	if !ok {
		p.writeOpenAIError(w, http.StatusInternalServerError, "Streaming not supported", "server_error")
		return 0
	}

	sentRole := false
	toolCallIndex := 0
	toolCallIndexes := map[string]int{}
	var streamIn, streamOut, streamCacheRead, streamCacheWrite int64 // 累计本次流的 token 用量（finish 事件报告）
	var reasoningBuilder strings.Builder                            // 累积思考内容（DeepSeek 方言透传）

	process := func(line string) error {
		p.debugf("[DEBUG] CommandCode stream line: %s", truncateLog(line))

		var event api.CCStreamEvent
		if err := json.Unmarshal([]byte(line), &event); err != nil {
			return nil
		}

		switch event.Type {
		case "text-delta":
			delta := api.OpenAIDelta{Content: event.Text}
			if !sentRole {
				delta.Role = "assistant"
				sentRole = true
			}
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{Index: 0, Delta: &delta}},
			})

		case "reasoning-delta":
			// 思考内容透传（DeepSeek 方言 delta.reasoning_content）：
			// DSH 等客户端靠它渲染思考过程、统计 reasoning_tokens。
			reasoningBuilder.WriteString(event.Text)
			delta := api.OpenAIDelta{ReasoningContent: event.Text}
			if !sentRole {
				delta.Role = "assistant"
				sentRole = true
			}
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{Index: 0, Delta: &delta}},
			})

		case "reasoning-start", "reasoning-end":
			// 无内容标记事件，跳过（内容由 reasoning-delta 携带）

		case "tool-use":
			toolCalls := []api.OpenAIDeltaToolCall{{
				Index:    toolCallIndex,
				ID:       event.ToolCallID,
				Type:     "function",
				Function: &api.OpenAIDeltaFunction{Name: event.ToolName},
			}}
			delta := api.OpenAIDelta{ToolCalls: toolCalls}
			if !sentRole {
				delta.Role = "assistant"
				sentRole = true
			}
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{Index: 0, Delta: &delta}},
			})
			toolCallIndex++

		case "tool-delta":
			toolCalls := []api.OpenAIDeltaToolCall{{
				Index:    toolCallIndex - 1,
				Function: &api.OpenAIDeltaFunction{Arguments: event.Text},
			}}
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{Index: 0, Delta: &api.OpenAIDelta{ToolCalls: toolCalls}}},
			})

		case "tool-input-start":
			if _, ok := toolCallIndexes[event.ID]; !ok {
				toolCallIndexes[event.ID] = toolCallIndex
				toolCallIndex++
			}
			delta := api.OpenAIDelta{ToolCalls: []api.OpenAIDeltaToolCall{{
				Index: toolCallIndexes[event.ID],
				ID:    event.ID,
				Type:  "function",
				Function: &api.OpenAIDeltaFunction{
					Name: event.ToolName,
				},
			}}}
			if !sentRole {
				delta.Role = "assistant"
				sentRole = true
			}
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{Index: 0, Delta: &delta}},
			})

		case "tool-input-delta":
			idx, ok := toolCallIndexes[event.ID]
			if !ok {
				idx = toolCallIndex
				toolCallIndexes[event.ID] = idx
				toolCallIndex++
			}
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{Index: 0, Delta: &api.OpenAIDelta{ToolCalls: []api.OpenAIDeltaToolCall{{
					Index:    idx,
					Function: &api.OpenAIDeltaFunction{Arguments: event.Delta},
				}}}}},
			})

		case "tool-call":
			if _, alreadyStreamed := toolCallIndexes[event.ToolCallID]; alreadyStreamed {
				return nil
			}
			idx := toolCallIndex
			toolCallIndexes[event.ToolCallID] = idx
			toolCallIndex++
			args := ""
			if event.Input != nil {
				if data, err := json.Marshal(event.Input); err == nil {
					args = string(data)
				}
			}
			delta := api.OpenAIDelta{ToolCalls: []api.OpenAIDeltaToolCall{{
				Index: idx,
				ID:    event.ToolCallID,
				Type:  "function",
				Function: &api.OpenAIDeltaFunction{
					Name:      event.ToolName,
					Arguments: args,
				},
			}}}
			if !sentRole {
				delta.Role = "assistant"
				sentRole = true
			}
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{Index: 0, Delta: &delta}},
			})

		case "finish":
			reason := normalizeFinishReason(event.FinishReason)
			p.WriteSSE(w, flusher, api.OpenAIChatResponse{
				ID:      requestID,
				Object:  "chat.completion.chunk",
				Created: created,
				Model:   model,
				Choices: []api.OpenAIChoice{{
					Index:        0,
					Delta:        &api.OpenAIDelta{},
					FinishReason: &reason,
				}},
			})
			fmt.Fprintf(w, "data: [DONE]\n\n")
			flusher.Flush()
			// 记录流式请求的本地统计（Codex 等 agent 均走流式）
			if event.TotalUsage != nil {
				streamIn = int64(event.TotalUsage.InputTokens)
				streamOut = int64(event.TotalUsage.OutputTokens)
				streamCacheRead = int64(event.TotalUsage.CacheReadTokens)
				streamCacheWrite = int64(event.TotalUsage.CacheWriteTokens)
				if streamIn > 0 || streamOut > 0 || streamCacheRead > 0 {
					p.Stats.Record(model, streamIn, streamOut, streamCacheRead, streamCacheWrite)
				}
			}

		case "error":
			log.Printf("[ERROR] Stream error: %v", event.Error)
		}
		return nil
	}
	return p.streamCCEvents(w, r, ccResp, process)
}

// readFirstLine 读取缓冲读取器的一行（不阻塞等待更多数据）。
func readFirstLine(r *bufio.Reader) (string, error) {
	lineBytes, err := r.ReadBytes('\n')
	line := strings.TrimSpace(string(lineBytes))
	if err != nil && len(line) == 0 {
		return "", err
	}
	return line, nil
}

// contextOverflowRe 匹配上游 context 超限报错中的 messages token 数。
var contextOverflowRe = regexp.MustCompile(`requested \d+ tokens \((\d+) in the messages`)

// ccContextLimit 是上游模型的上下文上限（token）：messages + 输出预算不能超过它。
const ccContextLimit = 1048576

// parseContextOverflow 从上游错误文本提取 messages 已占用的 token 数。
// 匹配 CommandCode 的报错格式：
//
//	"This model's maximum context length is 1048576 tokens. However, you
//	 requested 1048593 tokens (848593 in the messages, 200000 in the completion)..."
func parseContextOverflow(errMsg string) int {
	m := contextOverflowRe.FindStringSubmatch(errMsg)
	if len(m) == 2 {
		if n, err := strconv.Atoi(m[1]); err == nil && n > 0 {
			return n
		}
	}
	return 0
}

// WriteSSE writes a Server-Sent Event
func (p *Proxy) WriteSSE(w io.Writer, flusher http.Flusher, resp api.OpenAIChatResponse) {
	data, _ := json.Marshal(resp)
	fmt.Fprintf(w, "data: %s\n\n", data)
	flusher.Flush()
}

// ccResult 汇总一次上游响应中的全部文本 / 工具 / 用量事件（流与非流共用）。
type ccResult struct {
	content            strings.Builder
	reasoning          strings.Builder // DeepSeek 方言：思考内容
	toolCalls          []api.ToolCall
	toolCallByID       map[string]int
	toolInputBuffers   map[string]*strings.Builder
	hasToolCalls       bool
	inputTokens        int
	outputTokens       int
	cacheReadTokens    int
	cacheWriteTokens   int
	reasoningTokens    int
	finishReason       string
}

// readCCResult 逐行解析上游响应（非流场景），累积文本、工具调用与用量。
func (p *Proxy) readCCResult(r io.Reader) *ccResult {
	res := &ccResult{
		toolCallByID:     map[string]int{},
		toolInputBuffers: map[string]*strings.Builder{},
	}

	lineErr := forEachLine(r, nil, func(line string) error {
		p.debugf("[DEBUG] CommandCode stream line: %s", truncateLog(line))

		var event api.CCStreamEvent
		if err := json.Unmarshal([]byte(line), &event); err != nil {
			return nil
		}

		switch event.Type {
		case "text-delta":
			res.content.WriteString(event.Text)
		case "tool-use":
			res.hasToolCalls = true
			res.toolCallByID[event.ToolCallID] = len(res.toolCalls)
			res.toolCalls = append(res.toolCalls, api.ToolCall{
				ID:   event.ToolCallID,
				Type: "function",
				Function: api.FunctionCall{
					Name:      event.ToolName,
					Arguments: "",
				},
			})
		case "tool-delta":
			if len(res.toolCalls) > 0 {
				res.toolCalls[len(res.toolCalls)-1].Function.Arguments += event.Text
			}
		case "tool-input-start":
			res.hasToolCalls = true
			res.toolCallByID[event.ID] = len(res.toolCalls)
			res.toolInputBuffers[event.ID] = &strings.Builder{}
			res.toolCalls = append(res.toolCalls, api.ToolCall{
				ID:   event.ID,
				Type: "function",
				Function: api.FunctionCall{
					Name:      event.ToolName,
					Arguments: "",
				},
			})
		case "tool-input-delta":
			if b := res.toolInputBuffers[event.ID]; b != nil {
				b.WriteString(event.Delta)
			}
			if idx, ok := res.toolCallByID[event.ID]; ok {
				res.toolCalls[idx].Function.Arguments += event.Delta
			}
		case "tool-call":
			res.hasToolCalls = true
			args := ""
			if event.Input != nil {
				if data, err := json.Marshal(event.Input); err == nil {
					args = string(data)
				}
			}
			if idx, ok := res.toolCallByID[event.ToolCallID]; ok {
				res.toolCalls[idx].Function.Name = event.ToolName
				if args != "" {
					res.toolCalls[idx].Function.Arguments = args
				}
			} else {
				res.toolCallByID[event.ToolCallID] = len(res.toolCalls)
				res.toolCalls = append(res.toolCalls, api.ToolCall{
					ID:   event.ToolCallID,
					Type: "function",
					Function: api.FunctionCall{
						Name:      event.ToolName,
						Arguments: args,
					},
				})
			}
		case "reasoning-delta":
			// 思考内容（DeepSeek 方言）累积，最终放进 message.reasoning_content
			res.reasoning.WriteString(event.Text)

		case "finish":
			res.finishReason = event.FinishReason
			if event.TotalUsage != nil {
				res.inputTokens = event.TotalUsage.InputTokens
				res.outputTokens = event.TotalUsage.OutputTokens
				res.cacheReadTokens = event.TotalUsage.CacheReadTokens
				res.cacheWriteTokens = event.TotalUsage.CacheWriteTokens
				res.reasoningTokens = event.TotalUsage.ReasoningTokens
			}
		case "error":
			log.Printf("[ERROR] Stream error: %v", event.Error)
		}
		return nil
	})
	if lineErr != nil {
		log.Printf("[ERROR] Stream read error: %v", lineErr)
	}
	return res
}

// NonStreamResponse handles non-streaming response
func (p *Proxy) NonStreamResponse(w http.ResponseWriter, ccResp *http.Response, requestID, model string, created int64) {
	res := p.readCCResult(ccResp.Body)

	msg := &api.OpenAIMessage{
		Role:    "assistant",
		Content: res.content.String(),
	}
	if res.reasoning.Len() > 0 {
		msg.ReasoningContent = res.reasoning.String() // DeepSeek 方言：思考内容
	}
	finishReason := "stop"
	if res.hasToolCalls {
		msg.Content = nil
		msg.ToolCalls = res.toolCalls
		finishReason = "tool_calls"
	}

	response := api.OpenAIChatResponse{
		ID:      requestID,
		Object:  "chat.completion",
		Created: created,
		Model:   model,
		Choices: []api.OpenAIChoice{{
			Index:        0,
			Message:      msg,
			FinishReason: &finishReason,
		}},
		Usage: &api.OpenAIUsage{
			PromptTokens:     res.inputTokens,
			CompletionTokens: res.outputTokens,
			TotalTokens:      res.inputTokens + res.outputTokens,
		},
	}
	if res.reasoningTokens > 0 {
		response.Usage.CompletionTokensDetails = &api.OpenAICompletionDetails{ReasoningTokens: res.reasoningTokens}
	}

	// Record local usage stats (counts tokens CommandCode actually reported).
	if res.inputTokens > 0 || res.outputTokens > 0 || res.cacheReadTokens > 0 {
		p.Stats.Record(model, int64(res.inputTokens), int64(res.outputTokens), int64(res.cacheReadTokens), int64(res.cacheWriteTokens))
	}

	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(response)
}

