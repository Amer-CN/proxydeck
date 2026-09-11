package tuanjie

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
)

// reqshape.go：把转发到团结 LiteLLM 的请求体重排成官方 CLI buildCreateParams
// 的字段顺序并补默认字段，使请求形态（字段序+默认值）与官方一致。
//
// 官方顺序（[ ] 为按需出现）：
//   model, messages, [temperature], [max_completion_tokens|max_tokens],
//   [reasoning_effort], [top_p], [metadata], [litellm_session_id],
//   [prompt_cache_key], [tools, parallel_tool_calls:true],
//   [tool_choice], stream:true, stream_options:{include_usage:true}
// 非流式时官方不带 stream / stream_options（delete 掉）。
//
// 重排保守原则：客户端没发的字段不凭空造值——只补三个官方带的默认字段
// （parallel_tool_calls / litellm_session_id / prompt_cache_key），
// 其余字段一律按客户端原值透传（含 metadata，官方 OpenAI chat 路径
// 不带 user_id，客户端带了就原样保留）；罕见字段（frequency_penalty 等
// 官方清单外的）放在 metadata 之前，按字母序稳定输出。
//
// 唯一例外是思考档位：客户端发 reasoning_effort 时，紧邻补一份上游真正
// 解析的 reasoning:{effort,summary}（见 reasoningPayloadFor）。

// officialFieldOrder 官方字段顺序。
var officialFieldOrder = []string{
	"model",
	"messages",
	"temperature",
	"max_completion_tokens", // 与 max_tokens 互斥同位
	"max_tokens",
	"reasoning_effort",
	"top_p",
	"metadata",
	"litellm_session_id",
	"prompt_cache_key",
	"tools",
	"parallel_tool_calls",
	"tool_choice",
	"stream",
	"stream_options",
}

// fieldRank 返回字段在官方清单里的权重；清单外（罕见字段）返回 len（最大）。
func fieldRank(name string) int {
	for i, f := range officialFieldOrder {
		if f == name {
			return i
		}
	}
	return len(officialFieldOrder)
}

type orderedField struct {
	key   string
	value any
}

// orderedJSON 按序序列化：fields 为有序 (key, value) 列表。
func orderedJSON(fields []orderedField) []byte {
	var buf bytes.Buffer
	buf.WriteByte('{')
	for i, f := range fields {
		if i > 0 {
			buf.WriteByte(',')
		}
		kb, _ := json.Marshal(f.key)
		buf.Write(kb)
		buf.WriteByte(':')
		vb, err := json.Marshal(f.value)
		if err != nil {
			vb = []byte("null")
		}
		buf.Write(vb)
	}
	buf.WriteByte('}')
	return buf.Bytes()
}

// reshapeChatBody 把 chat/completions 请求体重排为官方 CLI 字段顺序并补默认。
// sess 为本请求的会话（litellm_session_id / prompt_cache_key 取会话 id，
// metadata 四字段从会话对象取）。sess 为 nil 时现场造一次性会话（仅兜底，
// 正常流程 handleChat 总是传真实会话）。body 非法 JSON 时原样返回（不拦转发）。
func reshapeChatBody(body []byte, sess *LitellmSession) []byte {
	if sess == nil {
		sess = newLitellmSession()
		sess.promptSeq = 1
	}
	sessionID := sess.ID
	var m map[string]json.RawMessage
	if err := json.Unmarshal(body, &m); err != nil {
		return body
	}

	// 非流式（无 stream 字段或 stream=false）：官方 delete 掉 stream/stream_options
	streaming := false
	if sv, ok := m["stream"]; ok {
		var b bool
		if json.Unmarshal(sv, &b) == nil && b {
			streaming = true
		}
	}

	// 罕见字段（官方清单外）：字母序，插在 metadata 之前
	extras := make([]string, 0, len(m))
	for k := range m {
		if fieldRank(k) < len(officialFieldOrder) {
			continue
		}
		extras = append(extras, k)
	}
	sortStrings(extras)

	// 1. 官方顺序字段（客户端传了才带；max_completion_tokens/max_tokens
	//    互斥同位，带哪个放哪个）
	fields := make([]orderedField, 0, len(m)+4)
	rareDone := false
	for _, name := range officialFieldOrder {
		// 罕见字段在 metadata 之前统一插入；客户端没带 metadata 时在此补注入
		// （官方 buildMetadata 每请求都带 metadata，字段序在 top_p 之后）。
		if name == "metadata" && !rareDone {
			for _, k := range extras {
				fields = append(fields, orderedField{key: k, value: json.RawMessage(m[k])})
			}
			rareDone = true
			if _, ok := m["metadata"]; !ok {
				fields = append(fields, orderedField{key: "metadata", value: buildMetadataPayload(sess)})
			}
		}
		if name == "max_completion_tokens" || name == "max_tokens" {
			// 互斥同位：两个名字占同一个官方位置，带哪个放哪个（都带按
			// max_completion_tokens 在前），主循环只处理一次
			if name == "max_tokens" {
				continue
			}
			if v, ok := m["max_completion_tokens"]; ok {
				fields = append(fields, orderedField{key: "max_completion_tokens", value: v})
			}
			if v, ok := m["max_tokens"]; ok {
				fields = append(fields, orderedField{key: "max_tokens", value: v})
			}
			continue
		}
		if v, ok := m[name]; ok {
			fields = append(fields, orderedField{key: name, value: v})
			// 客户端（ZCode 等）只发 chat 形态 reasoning_effort，团结上游
			// 只接收不解析它；紧邻补一份 Responses 形态 reasoning 才生效。
			// 客户端自带 reasoning 时不覆盖（尊重原值）。
			if name == "reasoning_effort" {
				if _, own := m["reasoning"]; !own {
					if rp := reasoningPayloadFor(v); rp != nil {
						fields = append(fields, orderedField{key: "reasoning", value: rp})
					}
				}
			}
		}
	}
	if !rareDone {
		for _, k := range extras {
			fields = append(fields, orderedField{key: k, value: json.RawMessage(m[k])})
		}
	}

	// 2. 补官方默认字段（客户端没发才补；插在官方位置上）
	if _, ok := m["parallel_tool_calls"]; !ok {
		// 官方 buildCreateParams 里 tools 与 parallel_tool_calls:true 成对出现
		if _, hasTools := m["tools"]; hasTools {
			fields = insertField(fields, "parallel_tool_calls", true, "tools")
		}
	}
	if _, ok := m["litellm_session_id"]; !ok {
		fields = insertField(fields, "litellm_session_id", sessionID, "metadata")
	}
	if _, ok := m["prompt_cache_key"]; !ok {
		fields = insertField(fields, "prompt_cache_key", sessionID, "litellm_session_id")
	}

	// 3. stream 相关：流式带 stream:true + stream_options（include_usage 强制
	//    true——我们本来也注入，统计需要）；非流式两个都不带（官方 delete 行为）。
	fields = removeFields(fields, "stream", "stream_options")
	if streaming {
		fields = append(fields, orderedField{key: "stream", value: true})
		so := map[string]any{"include_usage": true}
		if raw, ok := m["stream_options"]; ok {
			var orig map[string]any
			if json.Unmarshal(raw, &orig) == nil {
				so = orig
				so["include_usage"] = true
			}
		}
		fields = append(fields, orderedField{key: "stream_options", value: so})
	}
	return orderedJSON(fields)
}

// reasoningPayloadFor 把客户端 chat 形态的 reasoning_effort 翻译成团结上游
// 实际解析的 Responses 形态 reasoning:{effort,summary}。
//
// 2026-09-11 直连团结 LiteLLM 实测（reasoning_tokens 中位，问同一道多步题）：
// reasoning_effort 上游只接收不解析——非法值 "banana" 也返回 200，low/high/max
// 三档无差别；换成 reasoning:{effort,summary:"auto"} 后档位才落到上游：
// codely-core 的 low/high 明显压薄思考（多次采样 low≈100-180、high≈140-260，
// 默认≈550-870），max 与默认无差别；KIMI-K3 采样噪声大、未见稳定档位关系。
// 官方桌面客户端即此形态：服务端模型配置 extras.thinkingEfforts 的选中值，
// 由 CLI 落成 reasoning:{effort,summary:"auto"}。
//
// 非字符串或空值返回 nil（不注入，不给上游塞垃圾）；注入的 effort 值经
// reasoningEffortWire 映射（值映射依据官方 CLI 的 WSi()，见其注释），summary
// 固定 "auto"；上游对未知值静默忽略，不会打坏请求。
func reasoningPayloadFor(raw json.RawMessage) json.RawMessage {
	var v string
	if len(raw) == 0 || json.Unmarshal(raw, &v) != nil || v == "" {
		return nil
	}
	return json.RawMessage(orderedJSON([]orderedField{
		{key: "effort", value: reasoningEffortWire(v)},
		{key: "summary", value: "auto"},
	}))
}

// reasoningEffortWire 把客户端档位值映射为官方 Responses 形态实际发出的值。
// 对齐官方 CLI 的 WSi()：low/medium/high/xhigh 原样、minimal→low、其余（含 max）→xhigh。
// 有意偏离一处：官方梯子（minimal…max）之外的值（off/none/未知）不兜底成 xhigh，
// 原样透传——别的客户端用 off/none 表示「关思考」，兜底成 xhigh 会把语义反过来；
// 且团结上游对未知值静默忽略，透传无副作用。
func reasoningEffortWire(v string) string {
	switch e := strings.ToLower(v); e {
	case "low", "medium", "high", "xhigh":
		return e
	case "minimal":
		return "low"
	case "max":
		return "xhigh"
	}
	return v
}

// buildMetadataPayload 构造 metadata 字段（对齐官方 buildMetadata LiteLLM
// 分支：prompt_id/session_id/cwd/litellm_conversation_id，字段序按官方）。
// 用 orderedJSON 保序——官方内层字段序即此序，map 序列化会打乱。
func buildMetadataPayload(sess *LitellmSession) json.RawMessage {
	fields := []orderedField{
		{key: "prompt_id", value: fmt.Sprintf("%s########%d", sess.ID, sess.promptSeq)},
		{key: "session_id", value: sess.ID},
		{key: "cwd", value: exeDirForAccounts()},
		{key: "litellm_conversation_id", value: sess.ConversationID},
	}
	return json.RawMessage(orderedJSON(fields))
}

// insertField 在 anchor 字段之后插入新字段（anchor 不存在则追加到末尾）。
func insertField(fields []orderedField, key string, value any, anchor string) []orderedField {
	if indexField(fields, key) >= 0 {
		return fields
	}
	idx := indexField(fields, anchor)
	if idx < 0 {
		return append(fields, orderedField{key: key, value: value})
	}
	out := make([]orderedField, 0, len(fields)+1)
	out = append(out, fields[:idx+1]...)
	out = append(out, orderedField{key: key, value: value})
	out = append(out, fields[idx+1:]...)
	return out
}

// indexField 返回字段下标（-1 不存在）。
func indexField(fields []orderedField, key string) int {
	for i, f := range fields {
		if f.key == key {
			return i
		}
	}
	return -1
}

// removeFields 删掉指定键。
func removeFields(fields []orderedField, keys ...string) []orderedField {
	drop := map[string]bool{}
	for _, k := range keys {
		drop[k] = true
	}
	out := make([]orderedField, 0, len(fields))
	for _, f := range fields {
		if !drop[f.key] {
			out = append(out, f)
		}
	}
	return out
}

// sortStrings 插入排序（字段数是个位数，没必要引 sort 包语义）。
func sortStrings(s []string) {
	for i := 1; i < len(s); i++ {
		for j := i; j > 0 && s[j] < s[j-1]; j-- {
			s[j], s[j-1] = s[j-1], s[j]
		}
	}
}
