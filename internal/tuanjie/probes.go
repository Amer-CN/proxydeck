// probes.go —— 管道探针集（第一层，学 modelprint）：对指定账号+模型执行
// 一组高频廉价的小调用（每次 ≤10 个、max_tokens≤8 为主），抓上游管道的
// 结构化指纹：分词器归一化 token 数 ×4、校验报错原文 ×2、finish_reason
// 词汇 ×2。同文本双发一致性检查（不一致 = 多主机路由，标 unstable）。
// 探针请求全部裸发（不带用户会话 system prompt，防上游注入影响结果）。
package tuanjie

import (
	"context"
	"encoding/json"
	"fmt"
	"math/rand"
	"net/http"
	"regexp"
	"strings"
	"time"
)

// probeTimeout 单个探针请求超时（max_tokens 极小，正常秒级返回；慢渠道
// workbuddy 实测单请求可达 90s+，2026-08-25 提到 120s 覆盖）。
const probeTimeout = 120 * time.Second

// probeTarget 一次探针要打到哪、带什么头：渠道端点 + 静态请求头 + 渠道名。
// 动态头（团结的 X-Codely-Signature / x-litellm-session-id 每请求现算）由
// requestHeaders 按 Channel 现场构造；本地渠道（command/workbuddy/bai）的头
// 全部静态，直接进 Headers 即可。
type probeTarget struct {
	BaseURL string
	Headers map[string]string
	Channel string
}

// tuanjieTarget 构造团结渠道探针目标（cli_api_key 走账号池换取；签名头、
// session 头由 requestHeaders 每请求生成，行为与改造前一致）。
func tuanjieTarget(cliKey string) *probeTarget {
	return &probeTarget{
		BaseURL: litellmAPIBase,
		Channel: "tuanjie",
		Headers: map[string]string{"Authorization": "Bearer " + cliKey},
	}
}

// requestHeaders 构造本次请求的完整请求头：公共底座（Content-Type/Accept/
// User-Agent）+ 渠道静态头；团结渠道追加 x-litellm-session-id 与签名头
// （签名密钥取 Authorization 里的 cli_api_key，每请求实时时间戳）。
func (t *probeTarget) requestHeaders(path string) http.Header {
	h := http.Header{}
	h.Set("Content-Type", "application/json")
	h.Set("Accept", "application/json")
	h.Set("User-Agent", cliUserAgent)
	for k, v := range t.Headers {
		h.Set(k, v)
	}
	if t.Channel == "tuanjie" {
		h.Set("x-litellm-session-id", newLitellmSessionID())
		key := strings.TrimPrefix(t.Headers["Authorization"], "Bearer ")
		h.Set("X-Codely-Signature", SignLitellm(path, key, time.Now()))
	}
	return h
}

// tokenizerProbeTexts 四段钉死文本（不动！基准依赖逐字节稳定）。
var tokenizerProbeTexts = []struct{ Name, Text string }{
	{"tokenizer_en", "The quick brown fox jumps over the lazy dog while forty-two philosophers quietly debated the nature of consciousness beneath the ancient oak trees yesterday evening."},
	{"tokenizer_cjk", "团结引擎在凌晨三点重启了三台推理主机。质量守恒定律指出孤立系统的总能量保持不变。长江流域的梅雨季节通常持续二十天左右。"},
	{"tokenizer_code", "def quicksort(arr):\n    if len(arr) <= 1:\n        return arr\n    pivot = arr[len(arr) // 2]\n    left = [x for x in arr if x < pivot]\n    return quicksort(left) + [pivot] + quicksort(right)"},
	{Name: "tokenizer_mixed", Text: "ᚠᚢᚦ ⠓⠑⠇⠇⠕ 🌟🌱🚀 مرحبا بالعالم שלום עולם emoji+runes+braille+arabic+hebrew 混合文本"},
}

// probeResult 单个探针的结构化结果：value 为指纹值（token 数/错误文本/
// finish_reason），Status 取 ok/unstable/error（缺失信号如实标 unstable）。
type probeResult struct {
	Name   string `json:"name"`
	Value  string `json:"value"`
	Status string `json:"status"` // ok | unstable | error
	Note   string `json:"note,omitempty"`
}

// probeCall 对单渠道+模型发一次裸请求（无 system prompt），带自定义参数。
// 请求打到哪、带什么头全部由 target 决定；返回
// (promptTokens, completionTokens, finishReason, content, errorText, err)。
func probeCall(ctx context.Context, target *probeTarget, model string, msgs []map[string]any, extra map[string]any) (promptTokens, completionTokens int, finish, content, errText string, err error) {
	// max_tokens 按模型自适应：思考型模型（deepseek/GLM-5.3 等会先出
	// reasoning_content）8 个预算全烧在推理段、content 恒空——给 96 保
	// content 出得来（GLM-5.3 采基准实测 96 出数率约 7/8）。非思考模型
	// 多给的预算用不完，无副作用。
	mt := 8
	lm := strings.ToLower(model)
	if strings.Contains(lm, "deepseek") || strings.Contains(lm, "glm-5") ||
		strings.Contains(lm, "kimi") || strings.Contains(lm, "o1") || strings.Contains(lm, "o3") {
		mt = 96
	}
	body := map[string]any{
		"model":      model,
		"stream":     false,
		"messages":   msgs,
		"max_tokens": mt,
	}
	for k, v := range extra {
		body[k] = v
	}
	b, _ := json.Marshal(body)
	req, reqErr := http.NewRequestWithContext(ctx, http.MethodPost,
		target.BaseURL+"/v1/chat/completions", strings.NewReader(string(b)))
	if reqErr != nil {
		return 0, 0, "", "", "", reqErr
	}
	for k, vs := range target.requestHeaders("/v1/chat/completions") {
		for _, v := range vs {
			req.Header.Add(k, v)
		}
	}
	client := &http.Client{Timeout: probeTimeout, Transport: smartProxyTransport}
	resp, doErr := client.Do(req)
	if doErr != nil {
		return 0, 0, "", "", "", doErr
	}
	defer resp.Body.Close()
	var rr struct {
		Usage struct {
			PromptTokens     int `json:"prompt_tokens"`
			CompletionTokens int `json:"completion_tokens"`
		} `json:"usage"`
		Choices []struct {
			FinishReason string `json:"finish_reason"`
			Message      struct {
				Content string `json:"content"`
			} `json:"message"`
		} `json:"choices"`
		Error json.RawMessage `json:"error"`
	}
	if decErr := json.NewDecoder(resp.Body).Decode(&rr); decErr != nil {
		return 0, 0, "", "", "", decErr
	}
	errText = ""
	if resp.StatusCode != http.StatusOK {
		// 上游错误体原样抓取（scrub 后截断）
		errText = scrubErrorText(string(rr.Error))
		if errText == "" {
			errText = fmt.Sprintf("HTTP %d", resp.StatusCode)
		}
	}
	promptTokens = rr.Usage.PromptTokens
	completionTokens = rr.Usage.CompletionTokens
	if len(rr.Choices) > 0 {
		finish = rr.Choices[0].FinishReason
		content = rr.Choices[0].Message.Content
	}
	return
}

// scrubRe 需要从错误文本里抹掉的易变成分（每次请求都不同，留下游 DNA）。
var (
	scrubUUIDRe = regexp.MustCompile(`[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}`)
	scrubLongRe = regexp.MustCompile(`\d{13,}`)
)

// scrubErrorText 错误原文脱敏：抹 uuid / 13 位以上数字（时间戳类），
// 截断到 300 字符。
func scrubErrorText(s string) string {
	s = strings.TrimSpace(s)
	s = scrubUUIDRe.ReplaceAllString(s, "<uuid>")
	s = scrubLongRe.ReplaceAllString(s, "<num>")
	if len(s) > 300 {
		s = s[:300]
	}
	return s
}

// RunPipelineProbes 第一层管道探针（≤10 个小调用）：
//   - 归一化 tokenizer ×4：每段文本发两次（一致性检查），value =
//     文本 token 数 − "a" 单字符 token 数（抵消宿主隐藏模板开销）
//   - 错误探针 ×2：temperature=2.0 抓校验报错原文；max_tokens=10^9 抓拒绝信息
//   - finish_reason 词汇 ×2：max_tokens=400 正常 stop + max_tokens=6 强制截断
//
// 同文本双发 token 数不一致 → unstable（多主机路由迹象）。
func RunPipelineProbes(ctx context.Context, target *probeTarget, model string) []probeResult {
	var out []probeResult

	// "a" 单字符基准（所有 tokenizer 探针的归一化底座）
	_, aTok, _, _, _, _ := probeCall(ctx, target, model,
		[]map[string]any{{"role": "user", "content": "a"}}, nil)

	for _, t := range tokenizerProbeTexts {
		msgs := []map[string]any{{"role": "user", "content": t.Text}}
		tok1, _, _, _, _, e1 := probeCall(ctx, target, model, msgs, nil)
		tok2, _, _, _, _, e2 := probeCall(ctx, target, model, msgs, nil)
		switch {
		case e1 != nil || e2 != nil:
			out = append(out, probeResult{Name: t.Name, Status: "error",
				Note: firstErrText(e1, e2)})
		case tok1 <= 0 || tok2 <= 0:
			out = append(out, probeResult{Name: t.Name, Status: "unstable",
				Note: "usage 缺失（prompt_tokens=0）"})
		case tok1 != tok2:
			out = append(out, probeResult{Name: t.Name, Value: fmt.Sprintf("%d", tok1-aTok),
				Status: "unstable", Note: fmt.Sprintf("双发不一致 %d vs %d（多主机路由？）", tok1, tok2)})
		default:
			out = append(out, probeResult{Name: t.Name, Value: fmt.Sprintf("%d", tok1-aTok), Status: "ok"})
		}
	}

	// 错误探针 1：temperature=2.0 抓校验报错原文（scrub 后）
	_, _, _, _, errText, e := probeCall(ctx, target, model,
		[]map[string]any{{"role": "user", "content": "hello"}},
		map[string]any{"temperature": 2.0})
	switch {
	case e != nil:
		out = append(out, probeResult{Name: "error_temp2", Status: "error", Note: e.Error()})
	case errText == "":
		out = append(out, probeResult{Name: "error_temp2", Status: "unstable", Note: "未返回错误文本"})
	default:
		out = append(out, probeResult{Name: "error_temp2", Value: errText, Status: "ok"})
	}

	// 错误探针 2：max_tokens=10^9 抓拒绝信息（真实输出上限是厂商 DNA）
	_, _, _, _, errText2, e2 := probeCall(ctx, target, model,
		[]map[string]any{{"role": "user", "content": "hello"}},
		map[string]any{"max_tokens": 1000000000})
	switch {
	case e2 != nil:
		out = append(out, probeResult{Name: "error_maxtok", Status: "error", Note: e2.Error()})
	case errText2 == "":
		out = append(out, probeResult{Name: "error_maxtok", Status: "unstable", Note: "未返回错误文本"})
	default:
		out = append(out, probeResult{Name: "error_maxtok", Value: errText2, Status: "ok"})
	}

	// finish_reason 词汇 1：max_tokens=400 正常说完 → 期望 stop
	_, _, fin1, _, _, e3 := probeCall(ctx, target, model,
		[]map[string]any{{"role": "user", "content": "回复一个字：好"}},
		map[string]any{"max_tokens": 400})
	switch {
	case e3 != nil:
		out = append(out, probeResult{Name: "finish_stop", Status: "error", Note: e3.Error()})
	case fin1 == "":
		out = append(out, probeResult{Name: "finish_stop", Status: "unstable", Note: "finish_reason 缺失"})
	default:
		out = append(out, probeResult{Name: "finish_stop", Value: fin1, Status: "ok"})
	}

	// finish_reason 词汇 2：max_tokens=6 强制截断 → 期望 length
	_, _, fin2, _, _, e4 := probeCall(ctx, target, model,
		[]map[string]any{{"role": "user", "content": "写一篇五百字的散文，主题是秋天。"}},
		map[string]any{"max_tokens": 6})
	switch {
	case e4 != nil:
		out = append(out, probeResult{Name: "finish_length", Status: "error", Note: e4.Error()})
	case fin2 == "":
		out = append(out, probeResult{Name: "finish_length", Status: "unstable", Note: "finish_reason 缺失"})
	default:
		out = append(out, probeResult{Name: "finish_length", Value: fin2, Status: "ok"})
	}
	return out
}

func firstErrText(errs ...error) string {
	for _, e := range errs {
		if e != nil {
			return e.Error()
		}
	}
	return ""
}

// randSource 分布采样用 PRNG（拒绝采样 1..distBuckets 均匀候选）。
var randSource = rand.New(rand.NewSource(time.Now().UnixNano()))
