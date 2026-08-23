// Package lingxi 把 WPS 灵犀（lingxi.wps.cn/cowork）转成本地 OpenAI 兼容 API。
//
// 流程（2026-08-16 实测全通）：
//  1. 凭据：CDP 从灵犀桌面端（--remote-debugging-port=5237）读 wps_sid 等 cookie
//  2. 模型：GET /api/aioffice/v1/sessions/plans（模型清单+灵点倍率）
//  3. 对话：POST /cowork/sessions 建会话 → WSS /sessions/{id}/completions/ws
//     发 {"event":"user.input",...} → 收 component.content(delta)/component.end(full_content)
package lingxi

import (
	"fmt"
	"sync"
	"time"
)

// ModelEntry plans 接口的模型项。
type ModelEntry struct {
	Key        string `json:"key"`
	Name       string `json:"name"`
	Multiplier string `json:"multiplier"`
	Tag        string `json:"tag"`
}

var (
	modelsMu     sync.RWMutex
	modelsCache  []ModelEntry
	modelsAt     time.Time
)

// Models 返回模型清单（10 分钟缓存；失败回落内置表）。
func Models(c *Client) []ModelEntry {
	modelsMu.RLock()
	if modelsCache != nil && time.Since(modelsAt) < 10*time.Minute {
		out := append([]ModelEntry{}, modelsCache...)
		modelsMu.RUnlock()
		return out
	}
	modelsMu.RUnlock()
	if list, err := c.FetchModels(); err == nil && len(list) > 0 {
		modelsMu.Lock()
		modelsCache = list
		modelsAt = time.Now()
		modelsMu.Unlock()
		return append([]ModelEntry{}, list...)
	}
	modelsMu.RLock()
	defer modelsMu.RUnlock()
	if modelsCache != nil {
		return append([]ModelEntry{}, modelsCache...)
	}
	// 内置回落（2026-08-16 实测清单）
	return []ModelEntry{
		{"deepseek-v4-flash-0731", "DeepSeek V4 Flash", "0.05x", "正式版"},
		{"deepseek-v4-pro-0813", "DeepSeek V4 Pro", "0.15x", "正式版"},
		{"mimo-v2.5-pro", "Xiaomi MiMo-V2.5 Pro", "0.10x", ""},
		{"kimi-k3", "Kimi-K3", "1.60x", ""},
		{"GLM-5.2", "GLM-5.2", "0.75x", ""},
		{"minimax_m3", "MiniMax-M3", "0.20x", ""},
		{"qwen3.8_max", "Qwen3.8-Max", "~0.80x", ""},
		{"doubao_seed_2_1_pro_260628", "Doubao-Seed-2.1 Pro", "~0.50x", ""},
		{"GLM-5.3", "GLM-5.3", "~0.75x", "首发接入"},
	}
}

// ResolveModel 对外名（lingxi/ 前缀可选）→ plans 的 key；未知名给默认。
func ResolveModel(id string, c *Client) string {
	list := Models(c)
	strip := func(s string) string {
		if len(s) > 7 && s[:7] == "lingxi/" {
			return s[7:]
		}
		return s
	}
	want := strip(id)
	for _, m := range list {
		if m.Key == want {
			return m.Key
		}
	}
	// 大小写不敏感二次匹配（GLM-5.2 vs glm-5.2）
	for _, m := range list {
		if equalFold(m.Key, want) {
			return m.Key
		}
	}
	if want == "" {
		return "deepseek-v4-flash-0731" // 灵犀默认（BL 常量）
	}
	return ""
}

func equalFold(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		ca, cb := a[i], b[i]
		if 'A' <= ca && ca <= 'Z' {
			ca += 32
		}
		if 'A' <= cb && cb <= 'Z' {
			cb += 32
		}
		if ca != cb {
			return false
		}
	}
	return true
}

// ErrUnknownModel 未知模型错误。
func ErrUnknownModel(id string, c *Client) error {
	keys := make([]string, 0, 8)
	for _, m := range Models(c) {
		keys = append(keys, m.Key)
	}
	return fmt.Errorf("未知模型: %s（可用: %v）", id, keys)
}
