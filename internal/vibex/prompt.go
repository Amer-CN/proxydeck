// prompt.go —— messages→prompt 打平（SPEC-T1 §3.10）与 claude_event 事件折叠（§3.6）。
package vibex

import (
	"strings"
)

// outEvent 是一轮对话中由上游产出、供 HTTP 层消费的事件。
// Kind 为空 = 哨兵（本轮结束，且发送者的错误状态已就位——见 engine.turn 的时序注释）。
type outEvent struct {
	Kind  string // text / thinking / assistant_text / error_text / tool / note / result / done / error
	Text  string
	Usage map[string]any // result 事件的 usage
	Flags map[string]any // done 事件的 flags
}

// extractText 从 message.content 提取纯文本；支持 str / 多模态 list / 对象。
func extractText(content any) string {
	switch c := content.(type) {
	case nil:
		return ""
	case string:
		return c
	case []any:
		parts := make([]string, 0, len(c))
		for _, blk := range c {
			switch b := blk.(type) {
			case string:
				parts = append(parts, b)
			case map[string]any:
				_, hasImage := b["image_url"]
				if str(b["type"]) == "image_url" || hasImage {
					parts = append(parts, imagePlaceholder)
				} else if s, ok := b["text"].(string); ok {
					parts = append(parts, s)
				} else if s, ok := b["content"].(string); ok {
					parts = append(parts, s)
				}
			}
		}
		out := make([]string, 0, len(parts))
		for _, p := range parts {
			if p != "" {
				out = append(out, p)
			}
		}
		return strings.Join(out, "\n")
	case map[string]any:
		if s, ok := c["text"].(string); ok {
			return s
		}
	}
	return str(content)
}

// buildPrompt 把 OpenAI messages 拍平成单条 VibeX prompt。
//
// 特例（SPEC §3.10）：仅 1 条 user 且无 system/developer → 直接透传原文
// （只有 chat_instruction 非空时才追加【任务】块）。
func buildPrompt(messages []any, chatInstruction string) string {
	var sysTexts []string
	type line struct{ label, text string }
	var history []line
	for _, raw := range messages {
		m := toMap(raw)
		if m == nil {
			continue
		}
		role := strings.ToLower(str(m["role"]))
		if role == "" {
			role = "user"
		}
		text := extractText(m["content"])
		switch role {
		case "system", "developer":
			sysTexts = append(sysTexts, text)
		case "assistant":
			history = append(history, line{"助手", text})
		case "tool":
			history = append(history, line{"工具输出", text})
		default: // user 及未知角色
			history = append(history, line{"用户", text})
		}
	}
	instruction := chatInstruction
	if instruction == "" {
		instruction = defaultInstruction
	}
	if len(sysTexts) == 0 && len(messages) == 1 {
		if m := toMap(messages[0]); m != nil && strings.ToLower(str(m["role"])) == "user" {
			base := extractText(m["content"])
			if chatInstruction == "" {
				return base
			}
			return base + "\n\n【任务】\n" + instruction
		}
	}
	var blocks []string
	if len(sysTexts) > 0 {
		blocks = append(blocks, "【系统设定】\n"+strings.Join(sysTexts, "\n"))
	}
	if len(history) > 0 {
		lines := make([]string, 0, len(history))
		for _, l := range history {
			lines = append(lines, "【"+l.label+"】\n"+l.text)
		}
		blocks = append(blocks, "【对话记录】\n"+strings.Join(lines, "\n\n"))
	}
	blocks = append(blocks, "【任务】\n"+instruction)
	return strings.Join(blocks, "\n\n")
}

// claudeEvents 把一条 claude_event 的 event 折叠成 outEvent 序列（SPEC §3.6）。
func claudeEvents(ev any, cfg *Config) []outEvent {
	m := toMap(ev)
	if m == nil {
		return nil
	}
	switch str(m["type"]) {
	case "stream_event":
		inner := toMap(m["event"])
		if str(inner["type"]) != "content_block_delta" {
			return nil
		}
		delta := toMap(inner["delta"])
		switch str(delta["type"]) {
		case "text_delta":
			return []outEvent{{Kind: "text", Text: str(delta["text"])}}
		case "thinking_delta":
			return []outEvent{{Kind: "thinking", Text: str(delta["thinking"])}}
		}
	case "assistant":
		msg := toMap(m["message"])
		blocks := toList(msg["content"])
		isErr := truthy(m["isApiErrorMessage"]) || truthy(m["balance_insufficient"])
		var out []outEvent
		for _, raw := range blocks {
			blk := toMap(raw)
			if blk == nil {
				continue
			}
			switch str(blk["type"]) {
			case "text":
				txt := str(blk["text"])
				out = append(out, outEvent{Kind: "assistant_text", Text: txt})
				if isErr {
					out = append(out, outEvent{Kind: "error_text", Text: txt})
				}
			case "tool_use":
				if cfg != nil && cfg.ExposeTools {
					out = append(out, outEvent{Kind: "tool", Text: "[tool: " + str(blk["name"]) + "]"})
				}
			}
		}
		return out
	case "result":
		resText := ""
		if s, ok := m["result"].(string); ok {
			resText = s
		}
		return []outEvent{{Kind: "result", Text: resText, Usage: toMap(m["usage"])}}
	}
	return nil // system / user / 其它 → 忽略
}

// usageOut 是 OpenAI 口径的 usage。
type usageOut struct {
	PromptTokens     int `json:"prompt_tokens"`
	CompletionTokens int `json:"completion_tokens"`
	TotalTokens      int `json:"total_tokens"`
}

// oaiUsage 把 Claude usage 映射成 OpenAI usage（SPEC §3.11）。
func oaiUsage(usage map[string]any) usageOut {
	it, ot := intOf(usage["input_tokens"]), intOf(usage["output_tokens"])
	return usageOut{PromptTokens: it, CompletionTokens: ot, TotalTokens: it + ot}
}

func intOf(v any) int {
	switch t := v.(type) {
	case float64:
		return int(t)
	case int:
		return t
	case int64:
		return int(t)
	}
	return 0
}

// foldSync 把事件序列折叠成 (content, usage, doneFlags, lastError)（对齐 _fold_sync_events）。
// 非流式聚合：text/error_text/tool/note 优先；这些来源为空时回退 assistant_text 缓冲。
func foldSync(events []outEvent) (string, map[string]any, map[string]any, string) {
	var parts, assistantBuf []string
	var usage, flags map[string]any
	lastErr := ""
	for _, ev := range events {
		switch ev.Kind {
		case "text", "error_text", "tool", "note":
			if ev.Text != "" {
				parts = append(parts, ev.Text)
			}
		case "assistant_text":
			if ev.Text != "" {
				assistantBuf = append(assistantBuf, ev.Text)
			}
		case "result":
			usage = ev.Usage
		case "done":
			flags = ev.Flags
		case "error":
			if lastErr == "" {
				lastErr = ev.Text
			}
		}
	}
	content := strings.Join(parts, "")
	if content == "" {
		content = strings.Join(assistantBuf, "")
	}
	return content, usage, flags, lastErr
}
