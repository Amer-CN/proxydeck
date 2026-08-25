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
	UserID    string          `json:"user_id"`
	Model     string          `json:"model"`
	Pass      bool            `json:"pass"`
	PromptTok int             `json:"prompt_tokens"` // 本次指纹
	BaseTok   int             `json:"base_tokens"`   // 历史基线（0=首次无基线）
	DriftPct  float64         `json:"drift_pct"`     // 指纹漂移百分比
	Answers   map[string]bool `json:"answers"`       // 各题对错
	Detail    string          `json:"detail,omitempty"`
	At        string          `json:"at"`
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
	Baseline map[string]int `json:"baseline"` // "uid|model" -> prompt_tokens 基线
	Passive  []passiveEvent `json:"passive"`
	path     string
}

func waterFilePath() string { return filepath.Join(exeDirForAccounts(), "tuanjie-water.json") }

// LoadWater 从磁盘恢复检测器状态。
func LoadWater() *WaterCheck {
	w := &WaterCheck{Baseline: map[string]int{}, path: waterFilePath()}
	if b, err := os.ReadFile(w.path); err == nil {
		_ = json.Unmarshal(b, w)
		if w.Baseline == nil {
			w.Baseline = map[string]int{}
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

// ProbeAccount 对单账号跑金丝雀探针（直连上游，不经轮询）。
// model 通常探测 GLM-5.3（最贵的、最可能被注水的）。
func (w *WaterCheck) ProbeAccount(ctx context.Context, accessToken, userID, model string) (*WaterProbeResult, error) {
	// 直连换取该账号的 cli_api_key（独立请求，不动 Client 缓存）
	key, err := fetchKeyWithToken(ctx, accessToken)
	if err != nil {
		return nil, fmt.Errorf("换取 key 失败: %w", err)
	}

	result := &WaterProbeResult{UserID: userID, Model: model, Answers: map[string]bool{},
		At: time.Now().Format("2006-01-02 15:04:05")}

	// 指纹题（repeat）：只关心 prompt_tokens
	fp, err := probeOnce(ctx, key, model, canaryQuestions[0].Prompt, false)
	if err != nil {
		return nil, err
	}
	result.PromptTok = fp.promptTokens

	// 其余题：答案比对
	for _, q := range canaryQuestions[1:] {
		r, err := probeOnce(ctx, key, model, q.Prompt, false)
		if err != nil {
			result.Answers[q.ID] = false
			result.Detail = err.Error()
			continue
		}
		result.Answers[q.ID] = containsCI(r.answer, q.Expect)
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

	// 判定：指纹漂移超阈值 或 任一题答错 → 注水嫌疑
	fingerprintOK := result.BaseTok == 0 || result.DriftPct <= fingerprintDriftPct
	answersOK := true
	for _, ok := range result.Answers {
		if !ok {
			answersOK = false
		}
	}
	result.Pass = fingerprintOK && answersOK
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
		result.Detail = reason
	}
	return result, nil
}

// probeOnce 发一次最小请求（stream=false），返回 prompt_tokens 和回答文本。
func probeOnce(ctx context.Context, cliKey, model, prompt string, stream bool) (struct {
	promptTokens int
	answer       string
}, error) {
	var out struct {
		promptTokens int
		answer       string
	}
	body := map[string]any{
		"model":    model,
		"stream":   stream,
		"messages": []map[string]any{{"role": "user", "content": prompt}},
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
	var rr struct {
		Usage struct {
			PromptTokens int `json:"prompt_tokens"`
		} `json:"usage"`
		Choices []struct {
			Message struct {
				Content string `json:"content"`
			} `json:"message"`
		} `json:"choices"`
		Error json.RawMessage `json:"error"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&rr); err != nil {
		return out, err
	}
	if resp.StatusCode != http.StatusOK {
		return out, fmt.Errorf("上游 %d", resp.StatusCode)
	}
	out.promptTokens = rr.Usage.PromptTokens
	if len(rr.Choices) > 0 {
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
