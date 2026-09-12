// water.go —— 注水检测（学群友 water.py 的金丝雀探针思路）。
// 识别上游"挂顶级模型的名、跑便宜模型"：
//
//	一、被动观测：转发时比对请求模型名与上游响应 model 字段（mismatch 记录）
//	二、主动探针：固定提示词的 tokenizer 指纹（同模型 prompt_tokens 必须逐次一致，
//	    漂移超阈值=分词器变了=换模型了）+ 固定算术/常识题（答错=能力降级）
//
// 探针逐账号直连上游（绕过轮询），精确定位哪个账号被注水。
package tuanjie

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/google/uuid"
	"time"
)

// fingerprintDriftPct 指纹漂移阈值（prompt_tokens 偏移超过 8% 判分词器变了）。
const fingerprintDriftPct = 8.0

// canaryQuestions 金丝雀题库（固定不动！prompt_tokens 指纹依赖题面逐字节稳定）。
var canaryQuestions = []struct {
	ID     string `json:"id"`
	Title  string `json:"title"`
	Prompt string `json:"prompt"`
	Expect string `json:"expect"`
}{
	{"repeat", "指令复读", "请原样输出以下内容，不要添加任何解释或标点：QX7-8842 光子 torpedo 0.73 E=mc^2", "QX7-8842"},
	{"math", "算术", "计算 17×23+5 的值。只输出最终数字，不要输出任何其他字符。", "396"},
	{"knowledge", "常识", "中国的陆地国土面积大约是多少万平方公里？只输出数字，保留整数。", "960"},
}

// WaterProbeResult 单账号单模型的探针结果。
type WaterProbeResult struct {
	UserID     string              `json:"user_id"`
	Model      string              `json:"model"`
	Pass       bool                `json:"pass"`
	PromptTok  int                 `json:"prompt_tokens"` // 本次指纹
	BaseTok    int                 `json:"base_tokens"`   // 历史基线（0=首次无基线）
	DriftPct   float64             `json:"drift_pct"`     // 指纹漂移百分比
	Answers    map[string]bool     `json:"answers"`       // 各题对错（仅实际作答的题）
	Unanswered map[string]string   `json:"unanswered,omitempty"` // 未作答各题及原因（网络/上游错误、空正文重试后仍空）——不算答错
	Detail     string              `json:"detail,omitempty"`
	At         string              `json:"at"`
}

// passiveEvent 被动观测记录（模型名不符）。
type passiveEvent struct {
	At       string `json:"at"`
	Model    string `json:"model"`
	Returned string `json:"returned"`
	UserID   string `json:"user_id"`
}

// WaterCheck 注水检测器（被动事件 + 探针基线，持久化 water-check.json）。
type WaterCheck struct {
	mu       sync.Mutex
	Baseline map[string]int    `json:"baseline"` // "uid|model" -> prompt_tokens 基线
	Passive  []passiveEvent    `json:"passive"`
	Resolved map[string]string `json:"resolved"` // 归一化请求名 -> 最近返回名（别名解析结果）
	path     string
}

func waterFilePath() string { return filepath.Join(exeDirForAccounts(), "tuanjie-water.json") }

// LoadWater 从磁盘恢复检测器状态。
func LoadWater() *WaterCheck {
	w := &WaterCheck{Baseline: map[string]int{}, Resolved: map[string]string{}, path: waterFilePath()}
	if b, err := os.ReadFile(w.path); err == nil {
		_ = json.Unmarshal(b, w)
		if w.Baseline == nil {
			w.Baseline = map[string]int{}
		}
		if w.Resolved == nil {
			w.Resolved = map[string]string{}
		}
	}
	return w
}

func (w *WaterCheck) save() {
	b, err := json.MarshalIndent(w, "", "  ")
	if err != nil {
		return
	}
	_ = os.WriteFile(w.path, b, 0o600)
}

// RecordPassive 被动观测：请求模型 vs 上游返回模型。
func (w *WaterCheck) RecordPassive(requested, returned, userID string) {
	if requested == "" || returned == "" {
		return
	}
	if normalizeModelName(requested) == normalizeModelName(returned) {
		return
	}
	w.mu.Lock()
	defer w.mu.Unlock()
	w.Passive = append(w.Passive, passiveEvent{
		At: time.Now().Format("2006-01-02 15:04:05"), Model: requested, Returned: returned, UserID: userID,
	})
	if len(w.Passive) > 50 {
		w.Passive = w.Passive[len(w.Passive)-50:]
	}
	w.save()
}

// RecordResolution 记录别名解析结果（归一化请求名 → 最近返回名）：上游把
// 别名（codely-core 等）解析成真实模型后，响应 model 字段返回真实名。
// 返回 (旧值, 是否变化)——首次落基线、与上次相同不落盘、变化落盘返回旧值，
// 官方调整映射当天打日志可见。
func (w *WaterCheck) RecordResolution(requested, returned string) (from string, changed bool) {
	if requested == "" || returned == "" {
		return "", false
	}
	key, val := normalizeModelName(requested), normalizeModelName(returned)
	if key == val {
		return "", false // 直连真实模型（仅大小写/路由前缀差异），无别名解析
	}
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.Resolved == nil {
		w.Resolved = map[string]string{}
	}
	from = w.Resolved[key]
	changed = from != val
	if changed {
		w.Resolved[key] = val
		w.save()
	}
	return from, changed
}

// normalizeModelName 归一化：小写、去路由前缀（z-ai/ 等）。
func normalizeModelName(name string) string {
	s := name
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c >= 'A' && c <= 'Z' {
			s = s[:i] + string(rune(c-'A'+'a')) + s[i+1:]
		}
	}
	for i := len(s) - 1; i >= 0; i-- {
		if s[i] == '/' {
			return s[i+1:]
		}
	}
	return s
}

// PassiveEvents 返回被动观测记录。
func (w *WaterCheck) PassiveEvents() []passiveEvent {
	w.mu.Lock()
	defer w.mu.Unlock()
	out := make([]passiveEvent, len(w.Passive))
	copy(out, w.Passive)
	return out
}

// ProbeAccountTarget 金丝雀探针的渠道无关版：按 probeTarget 发题（多渠道
// 注水检测用）。tuanjie 渠道请继续用 ProbeAccount（含账号漂移基线记账）。
// 未作答语义（第 33 轮）：网络/上游错误、空正文（思考型预算全烧 reasoning、
// 加大预算 800 重试一次仍空）→ 记独立字段 Unanswered，不再写进 Answers——
// 没作答不是答错；Pass 要求全部作答且全对。
func (w *WaterCheck) ProbeAccountTarget(ctx context.Context, target *probeTarget, userID, model string) (*WaterProbeResult, error) {
	result := &WaterProbeResult{UserID: userID, Model: model,
		Answers:    map[string]bool{},
		Unanswered: map[string]string{},
		At:         time.Now().Format("2006-01-02 15:04:05")}
	for _, q := range canaryQuestions {
		msgs := []map[string]any{{"role": "user", "content": q.Prompt}}
		pt, _, st, fin, answer, te, err := probeCall(ctx, target, model, msgs, nil)
		switch {
		case err != nil:
			result.Unanswered[q.ID] = "未作答（上游请求失败：" + err.Error() + "）"
			continue
		case st != 200:
			result.Unanswered[q.ID] = "未作答（上游未返回正文：" + firstNonEmpty(te, "HTTP "+itoa(st)) + "）"
			continue
		}
		if q.ID == canaryQuestions[0].ID {
			result.PromptTok = pt // 指纹题：只记 prompt_tokens（usage 与正文无关）
		}
		if fin == "length" && answer == "" {
			// 思考型预算耗尽正文未出：加大预算重试一次（首次预算维持原值）
			_, _, st2, fin2, answer2, te2, err2 := probeCall(ctx, target, model, msgs,
				map[string]any{"max_tokens": 800})
			if err2 == nil && st2 == 200 && !(fin2 == "length" && answer2 == "") {
				fin, answer = fin2, answer2
			} else {
				result.Unanswered[q.ID] = "未作答（上游未返回正文：" + firstNonEmpty(te2, "重试未返回正文") + "）"
				continue
			}
		}
		result.Answers[q.ID] = containsCI(answer, q.Expect)
	}
	// 判定：作答题全对且无未作答才算过；detail 按实情拼（错答/未作答分开说）
	result.Pass = len(result.Answers) > 0 && len(result.Unanswered) == 0
	for _, ok := range result.Answers {
		if !ok {
			result.Pass = false
			break
		}
	}
	if !result.Pass {
		var parts []string
		for _, ok := range result.Answers {
			if !ok {
				parts = append(parts, "金丝雀答题有错误")
				break
			}
		}
		if len(result.Unanswered) > 0 {
			parts = append(parts, itoa(len(result.Unanswered))+" 题未作答（上游未返回正文）")
		}
		if len(parts) == 0 {
			parts = append(parts, "金丝雀答题有错误")
		}
		result.Detail = strings.Join(parts, "；")
	}
	return result, nil
}

// ProbeAccount 对单账号跑金丝雀探针（直连上游，不经轮询）。
// model 通常探测 GLM-5.3（最贵的、最可能被注水的）。
// 未作答语义（第 33 轮）：网络/上游错误、空正文（加大预算 800 重试一次
// 仍空）→ 记独立字段 Unanswered，不再算进 Answers——没作答不是答错。
func (w *WaterCheck) ProbeAccount(ctx context.Context, accessToken, userID, model string) (*WaterProbeResult, error) {
	// 直连换取该账号的 cli_api_key（独立请求，不动 Client 缓存）
	key, err := fetchKeyWithToken(ctx, accessToken)
	if err != nil {
		return nil, fmt.Errorf("换取 key 失败: %w", err)
	}

	result := &WaterProbeResult{UserID: userID, Model: model,
		Answers:    map[string]bool{},
		Unanswered: map[string]string{},
		At:         time.Now().Format("2006-01-02 15:04:05")}

	// 指纹题（repeat）：只关心 prompt_tokens（usage 与正文无关，正文空不影响指纹）
	fp, err := probeOnce(ctx, key, model, canaryQuestions[0].Prompt, false, 0)
	if err != nil {
		return nil, err
	}
	result.PromptTok = fp.promptTokens

	// 其余题：答案比对；空正文（length 且空）加大预算重试一次，仍空 → 未作答
	for _, q := range canaryQuestions[1:] {
		r, err := probeOnce(ctx, key, model, q.Prompt, false, 0)
		if err != nil {
			result.Unanswered[q.ID] = "未作答（上游请求失败：" + err.Error() + "）"
			continue
		}
		answer := r.answer
		if r.finish == "length" && answer == "" {
			r2, err2 := probeOnce(ctx, key, model, q.Prompt, false, 800)
			if err2 == nil && !(r2.finish == "length" && r2.answer == "") {
				answer = r2.answer
			} else {
				result.Unanswered[q.ID] = "未作答（上游未返回正文）"
				continue
			}
		}
		result.Answers[q.ID] = containsCI(answer, q.Expect)
	}

	// 指纹基线比对
	w.mu.Lock()
	baseKey := userID + "|" + model
	base := w.Baseline[baseKey]
	if base > 0 {
		result.BaseTok = base
		drift := float64(result.PromptTok-base) / float64(base) * 100
		if drift < 0 {
			drift = -drift
		}
		result.DriftPct = float64(int(drift*10)) / 10
	} else {
		w.Baseline[baseKey] = result.PromptTok // 首次建立基线
		result.BaseTok = result.PromptTok
	}
	w.save()
	w.mu.Unlock()

	// 判定：指纹漂移超阈值 或 任一作答题答错 → 注水嫌疑；未作答不算答错，
	// 但有未作答时本次金丝雀不算干净通过（测不了≠通过）
	fingerprintOK := result.BaseTok == 0 || result.DriftPct <= fingerprintDriftPct
	answersOK := true
	for _, ok := range result.Answers {
		if !ok {
			answersOK = false
		}
	}
	result.Pass = fingerprintOK && answersOK && len(result.Unanswered) == 0 && len(result.Answers) > 0
	if !result.Pass {
		reason := ""
		if !fingerprintOK {
			reason += fmt.Sprintf("指纹漂移 %.1f%%（基线 %d → %d）", result.DriftPct, result.BaseTok, result.PromptTok)
		}
		if !answersOK {
			if reason != "" {
				reason += "；"
			}
			reason += "金丝雀答题有错误"
		}
		if len(result.Unanswered) > 0 {
			if reason != "" {
				reason += "；"
			}
			reason += itoa(len(result.Unanswered)) + " 题未作答（上游未返回正文）"
		}
		result.Detail = reason
	}
	return result, nil
}

// probeOnce 发一次最小请求（stream=false），返回 prompt_tokens、finish_reason
// 和回答文本。maxTokens>0 时才带 max_tokens（0=上游默认；空正文加大预算重试
// 用 800）。先判状态码再解析正文——429 空体不再被吞成裸 "EOF"。
func probeOnce(ctx context.Context, cliKey, model, prompt string, stream bool, maxTokens int) (struct {
	promptTokens int
	finish       string
	answer       string
}, error) {
	var out struct {
		promptTokens int
		finish       string
		answer       string
	}
	body := map[string]any{
		"model":    model,
		"stream":   stream,
		"messages": []map[string]any{{"role": "user", "content": prompt}},
	}
	if maxTokens > 0 {
		body["max_tokens"] = maxTokens
	}
	b, _ := json.Marshal(body)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		litellmAPIBase+"/v1/chat/completions", bytes.NewReader(b))
	if err != nil {
		return out, err
	}
	req.Header.Set("Authorization", "Bearer "+cliKey)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", cliUserAgent)
	req.Header.Set("x-litellm-session-id", uuid.NewString())
	req.Header.Set("X-Codely-Signature", SignLitellm("/v1/chat/completions", cliKey, time.Now()))

	client := &http.Client{Timeout: 120 * time.Second, Transport: smartProxyTransport}
	resp, err := client.Do(req)
	if err != nil {
		return out, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
		return out, fmt.Errorf("上游 %d", resp.StatusCode)
	}
	var rr struct {
		Usage struct {
			PromptTokens int `json:"prompt_tokens"`
		} `json:"usage"`
		Choices []struct {
			FinishReason string `json:"finish_reason"`
			Message      struct {
				Content string `json:"content"`
			} `json:"message"`
		} `json:"choices"`
		Error json.RawMessage `json:"error"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&rr); err != nil {
		return out, err
	}
	out.promptTokens = rr.Usage.PromptTokens
	if len(rr.Choices) > 0 {
		out.finish = rr.Choices[0].FinishReason
		out.answer = rr.Choices[0].Message.Content
	}
	return out, nil
}

// fetchKeyWithToken 用指定 access_token 直连换取 cli_api_key（探针专用，
// 不经 Client 缓存。
func fetchKeyWithToken(ctx context.Context, accessToken string) (string, error) {
	ctx2, cancel := context.WithTimeout(ctx, keyFetchTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx2, http.MethodGet, cliAPIKeyURL, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+accessToken)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", cliUserAgent)
	client := &http.Client{Timeout: keyFetchTimeout, Transport: smartProxyTransport}
	resp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("换取 key 返回 %d", resp.StatusCode)
	}
	var out struct {
		CliAPIKey string `json:"cli_api_key"`
	}
	if json.Unmarshal(body, &out) != nil || out.CliAPIKey == "" {
		return "", fmt.Errorf("cli_api_key 响应异常")
	}
	return out.CliAPIKey, nil
}

func containsCI(haystack, needle string) bool {
	if haystack == "" || needle == "" {
		return false
	}
	h, n := normalizeModelName(haystack), normalizeModelName(needle)
	// 简单子串（不依赖 strings 包的小写差异）
	for i := 0; i+len(n) <= len(h); i++ {
		if h[i:i+len(n)] == n {
			return true
		}
	}
	return false
}
