package proxy

import (
	"encoding/json"
	"net/http"
	"sort"

	"github.com/Amer-CN/proxydeck/internal/api"
)

// modelInfo 描述一个模型及其所属套餐。
// Go 套餐字段与官方 https://commandcode.ai/docs/plans/go 的 32 个模型表保持一致。
type modelInfo struct {
	ID      string
	OwnedBy string
	OnGo    bool // 是否包含在 Go 套餐（32 个）内
}

// catalogModels 官方完整模型目录（61 个），按厂商 A→Z 分组。
// 2026-08-22 同步自 https://commandcode.ai/docs/plans/go（Go 套餐 36 个）。
// 价格为官方 $/1M（in/out），ox-alpha / laguna / ling 为免费。
var catalogModels = []modelInfo{
	// alibaba
	{ID: "qwen-3.6-max", OwnedBy: "alibaba", OnGo: true}, // $1.3/$7.8 per 1M
	{ID: "qwen-3.6-plus", OwnedBy: "alibaba", OnGo: true}, // $0.5/$3 per 1M · vision
	{ID: "qwen-3.7-flash", OwnedBy: "alibaba", OnGo: true}, // $0.03/$0.13 per 1M · vision
	{ID: "qwen-3.7-max", OwnedBy: "alibaba", OnGo: true}, // $2.5/$7.5 per 1M
	{ID: "qwen-3.7-plus", OwnedBy: "alibaba", OnGo: true}, // $0.4/$1.6 per 1M · vision
	{ID: "qwen-3.8-27b", OwnedBy: "alibaba", OnGo: true}, // $0.4/$3 per 1M · vision
	{ID: "qwen-3.8-max", OwnedBy: "alibaba", OnGo: true}, // $2/$6 per 1M · vision
	// anthropic
	{ID: "claude-fable-5", OwnedBy: "anthropic", OnGo: false}, // $10/$50 per 1M · vision
	{ID: "claude-haiku-4-5", OwnedBy: "anthropic", OnGo: false}, // $1/$5 per 1M · vision
	{ID: "claude-opus-4-6", OwnedBy: "anthropic", OnGo: false}, // $5/$25 per 1M · vision
	{ID: "claude-opus-4-7", OwnedBy: "anthropic", OnGo: false}, // $5/$25 per 1M · vision
	{ID: "claude-opus-4-8", OwnedBy: "anthropic", OnGo: false}, // $5/$25 per 1M · vision
	{ID: "claude-opus-5", OwnedBy: "anthropic", OnGo: false}, // $5/$25 per 1M · vision
	{ID: "claude-sonnet-4-5", OwnedBy: "anthropic", OnGo: false}, // $3/$15 per 1M · vision
	{ID: "claude-sonnet-4-6", OwnedBy: "anthropic", OnGo: false}, // $3/$15 per 1M · vision
	{ID: "claude-sonnet-5", OwnedBy: "anthropic", OnGo: false}, // $2/$10 per 1M · vision
	// deepseek
	{ID: "deepseek-v4-flash", OwnedBy: "deepseek", OnGo: true}, // $0.22/$0.66 per 1M
	{ID: "deepseek-v4-flash-vision-exp", OwnedBy: "deepseek", OnGo: true}, // $0.22/$0.66 per 1M · vision
	{ID: "deepseek-v4-pro", OwnedBy: "deepseek", OnGo: true}, // $0.66/$1.98 per 1M
	// google
	{ID: "gemini-3.1-flash-lite", OwnedBy: "google", OnGo: false}, // $0.25/$1.5 per 1M · vision
	{ID: "gemini-3.5-flash", OwnedBy: "google", OnGo: false}, // $1.5/$9 per 1M · vision
	{ID: "gemini-3.5-flash-lite", OwnedBy: "google", OnGo: false}, // $0.3/$2.5 per 1M · vision
	{ID: "gemini-3.6-flash", OwnedBy: "google", OnGo: false}, // $1.5/$7.5 per 1M · vision
	{ID: "gemini-3.7-flash", OwnedBy: "google", OnGo: false}, // $0.75/$3.75 per 1M · vision
	// inclusionai
	{ID: "ling-3.0-flash-free", OwnedBy: "inclusionai", OnGo: false}, // $0/$0 per 1M
	// meta
	{ID: "muse-spark-1.1", OwnedBy: "meta", OnGo: false}, // $1.25/$4.25 per 1M · vision
	{ID: "muse-spark-1.2", OwnedBy: "meta", OnGo: false}, // $1.25/$4.25 per 1M · vision
	{ID: "muse-spark-1.2-contributor", OwnedBy: "meta", OnGo: true}, // $0.1/$0.2 per 1M · vision
	// minimaxai
	{ID: "minimax-m2.5", OwnedBy: "minimaxai", OnGo: true}, // $0.3/$1.2 per 1M
	{ID: "minimax-m2.7", OwnedBy: "minimaxai", OnGo: true}, // $0.3/$1.2 per 1M
	{ID: "minimax-m3", OwnedBy: "minimaxai", OnGo: true}, // $0.3/$1.2 per 1M · vision
	// moonshotai
	{ID: "kimi-k2.5", OwnedBy: "moonshotai", OnGo: true}, // $0.6/$3 per 1M · vision
	{ID: "kimi-k2.6", OwnedBy: "moonshotai", OnGo: true}, // $0.95/$4 per 1M · vision
	{ID: "kimi-k2.7-code", OwnedBy: "moonshotai", OnGo: true}, // $0.95/$4 per 1M · vision
	{ID: "kimi-k2.7-code-highspeed", OwnedBy: "moonshotai", OnGo: true}, // $1.9/$8 per 1M · vision
	{ID: "kimi-k3", OwnedBy: "moonshotai", OnGo: true}, // $3/$15 per 1M · vision
	// nvidia
	{ID: "nemotron-3-ultra", OwnedBy: "nvidia", OnGo: true}, // $0.6/$2.4 per 1M
	// openai
	{ID: "gpt-5.3-codex", OwnedBy: "openai", OnGo: false}, // $2/$8 per 1M · vision
	{ID: "gpt-5.4", OwnedBy: "openai", OnGo: false}, // $2.5/$15 per 1M · vision
	{ID: "gpt-5.4-mini", OwnedBy: "openai", OnGo: false}, // $0.75/$4.5 per 1M · vision
	{ID: "gpt-5.5", OwnedBy: "openai", OnGo: false}, // $5/$30 per 1M · vision
	{ID: "gpt-5.6-luna", OwnedBy: "openai", OnGo: true}, // $0.2/$1.2 per 1M · vision
	{ID: "gpt-5.6-sol", OwnedBy: "openai", OnGo: false}, // $5/$30 per 1M · vision
	{ID: "gpt-5.6-terra", OwnedBy: "openai", OnGo: false}, // $2/$12 per 1M · vision
	// poolside
	{ID: "laguna-s-2.1-free", OwnedBy: "poolside", OnGo: true}, // $0/$0 per 1M
	// sakana
	{ID: "fugu-ultra", OwnedBy: "sakana", OnGo: false}, // $5/$30 per 1M · vision
	// stealth
	{ID: "ox-alpha", OwnedBy: "stealth", OnGo: true}, // $0/$0 per 1M · vision
	// stepfun
	{ID: "step-3.5-flash", OwnedBy: "stepfun", OnGo: true}, // $0.1/$0.3 per 1M
	{ID: "step-3.7-flash", OwnedBy: "stepfun", OnGo: true}, // $0.2/$1.15 per 1M · vision
	// tencent
	{ID: "tencent/hy3-paid", OwnedBy: "tencent", OnGo: true}, // $0.14/$0.58 per 1M
	// thinkingmachines
	{ID: "inkling", OwnedBy: "thinkingmachines", OnGo: true}, // $1/$4.05 per 1M · vision
	{ID: "inkling-small", OwnedBy: "thinkingmachines", OnGo: true}, // $0.5/$1.2 per 1M · vision
	// xai
	{ID: "grok-4.5", OwnedBy: "xai", OnGo: true}, // $2/$6 per 1M · vision
	{ID: "grok-4.6", OwnedBy: "xai", OnGo: false}, // $2/$6 per 1M
	// xiaomi
	{ID: "mimo-v2.5", OwnedBy: "xiaomi", OnGo: true}, // $0.14/$0.28 per 1M · vision
	{ID: "mimo-v2.5-pro", OwnedBy: "xiaomi", OnGo: true}, // $0.435/$0.87 per 1M
	// zai-org
	{ID: "glm-5", OwnedBy: "zai-org", OnGo: true}, // $1/$3.2 per 1M
	{ID: "glm-5.1", OwnedBy: "zai-org", OnGo: true}, // $1.4/$4.4 per 1M
	{ID: "glm-5.2", OwnedBy: "zai-org", OnGo: true}, // $1.4/$4.4 per 1M
	{ID: "glm-5.2-fast", OwnedBy: "zai-org", OnGo: true}, // $3/$10.25 per 1M
	{ID: "glm-5.3", OwnedBy: "zai-org", OnGo: true}, // $1.4/$4.4 per 1M
}

// HandleModels handles the /v1/models endpoint.
// 支持 ?plan=go 只返回 Go 套餐内模型（32 个），默认返回全部 55 个——与官方目录一致。
func (p *Proxy) HandleModels(w http.ResponseWriter, r *http.Request) {
	plan := r.URL.Query().Get("plan")
	items := catalogModels
	if plan == "go" {
		items = make([]modelInfo, 0, 32)
		for _, m := range catalogModels {
			if m.OnGo {
				items = append(items, m)
			}
		}
	}
	// 按厂商分组排序，保持目录整洁
	sort.SliceStable(items, func(i, j int) bool {
		if items[i].OwnedBy != items[j].OwnedBy {
			return items[i].OwnedBy < items[j].OwnedBy
		}
		return items[i].ID < items[j].ID
	})

	models := api.OpenAIModelList{
		Object: "list",
		Data:   make([]api.OpenAIModel, 0, len(items)),
	}
	for _, m := range items {
		models.Data = append(models.Data, api.OpenAIModel{
			ID:      m.ID,
			Object:  "model",
			Created: 0,
			OwnedBy: m.OwnedBy,
		})
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(models)
}