package proxy

import "testing"

func TestMapModel(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		// ---- 全部 55 个模型的短名映射 ----
		// Alibaba (Qwen)
		{"qwen-3.6-max-preview", "qwen-3.6-max-preview"}, // 已下架：透传
		{"qwen3.6-max-preview", "qwen3.6-max-preview"}, // 已下架：透传
		{"qwen-3.6-plus", "qwen-3.6-plus"},
		{"qwen3.6", "qwen-3.6-plus"},
		{"qwen-3.7-flash", "qwen-3.7-flash"},
		{"qwen-3.7-max", "qwen-3.7-max"},
		{"qwen3.7-max", "qwen-3.7-max"},
		{"qwen-3.7-plus", "qwen-3.7-plus"},
		{"qwen-3.8-max", "qwen-3.8-max"},
		{"QWEN-3.8-MAX", "qwen-3.8-max"}, // 大小写不敏感
		// Anthropic
		{"claude-fable-5", "claude-fable-5"},
		{"fable-5", "claude-fable-5"},
		{"claude-haiku-4-5", "claude-haiku-4-5"},
		{"haiku-4-5", "claude-haiku-4-5"},
		{"claude-opus-4-6", "claude-opus-4-6"},
		{"opus-4-6", "claude-opus-4-6"},
		{"claude-opus-4-7", "claude-opus-4-7"},
		{"claude-opus-4-8", "claude-opus-4-8"},
		{"claude-opus-5", "claude-opus-5"},
		{"opus", "claude-opus-5"}, // 含糊短名归于最新
		{"claude-sonnet-4-5", "claude-sonnet-4-5"},
		{"claude-sonnet-4-6", "claude-sonnet-4-6"},
		{"claude-sonnet-5", "claude-sonnet-5"},
		{"sonnet", "claude-sonnet-5"},
		// DeepSeek
		{"deepseek-v4-flash", "deepseek-v4-flash"},
		{"deepseek-v4-pro", "deepseek-v4-pro"},
		{"deepseek-v4", "deepseek-v4-pro"},
		{"deepseek-pro", "deepseek-v4-pro"},
		// Google
		{"gemini-3.1-flash-lite", "gemini-3.1-flash-lite"},
		{"gemini-3.5-flash", "gemini-3.5-flash"},
		{"gemini-3.5-flash-lite", "gemini-3.5-flash-lite"},
		{"gemini-3.6-flash", "gemini-3.6-flash"},
		// InclusionAI
		{"ling-3.0-flash", "ling-3.0-flash-free"},
		{"ling-3.0-flash-free", "ling-3.0-flash-free"},
		{"ling", "ling-3.0-flash-free"},
		// Meta
		{"muse-spark-1.1", "muse-spark-1.1"},
		{"muse-spark-1.2", "muse-spark-1.2"},
		{"muse-spark-1.2-contributor", "muse-spark-1.2-contributor"},
		// MiniMax
		{"minimax-m2.5", "minimax-m2.5"},
		{"minimax-m2.7", "minimax-m2.7"},
		{"minimax-m3", "minimax-m3"},
		{"minimax3", "minimax-m3"},
		// Moonshot
		{"kimi-k2.5", "kimi-k2.5"},
		{"kimi-k2.6", "kimi-k2.6"},
		{"kimi-k2.7-code", "kimi-k2.7-code"},
		{"kimi-k2.7-code-highspeed", "kimi-k2.7-code-highspeed"},
		{"kimi-k3", "kimi-k3"},
		// NVIDIA
		{"nemotron-3-ultra", "nemotron-3-ultra"},
		{"nemotron", "nemotron-3-ultra"},
		// OpenAI
		{"gpt-5.3-codex", "gpt-5.3-codex"},
		{"gpt-5.4", "gpt-5.4"},
		{"gpt-5.4-mini", "gpt-5.4-mini"},
		{"gpt-5.5", "gpt-5.5"},
		{"gpt-5.6-luna", "gpt-5.6-luna"},
		{"gpt-5.6-sol", "gpt-5.6-sol"},
		{"gpt-5.6-terra", "gpt-5.6-terra"},
		// Poolside
		{"laguna-s-2.1", "laguna-s-2.1-free"},
		{"laguna-s-2.1-free", "laguna-s-2.1-free"},
		{"laguna", "laguna-s-2.1-free"},
		// Sakana
		{"fugu-ultra", "fugu-ultra"},
		{"fugu", "fugu-ultra"},
		// StepFun
		{"step-3.5-flash", "step-3.5-flash"},
		{"step-3.7-flash", "step-3.7-flash"},
		// Tencent
		{"hy3", "tencent/hy3-paid"},
		{"hy3-paid", "tencent/hy3-paid"},
		// Thinking Machines
		{"inkling", "inkling"},
		{"inkling-small", "inkling-small"},
		// xAI
		{"grok-4.5", "grok-4.5"},
		{"grok", "grok-4.6"},
		// Xiaomi
		{"mimo-v2.5", "mimo-v2.5"},
		{"mimo-v2.5-pro", "mimo-v2.5-pro"},
		{"mimo", "mimo-v2.5"},
		// Z AI
		{"glm-5", "glm-5"},
		{"glm-5.1", "glm-5.1"},
		{"glm-5.2", "glm-5.2"},
		{"glm-5.2-fast", "glm-5.2-fast"},

		// ---- 完整 ID 原样透传 ----
		{"qwen-3.8-max", "qwen-3.8-max"},
		{"claude-opus-5", "claude-opus-5"},
		{"deepseek-v4-flash", "deepseek-v4-flash"},
		{"kimi-k3", "kimi-k3"},
		{"glm-5.2", "glm-5.2"},
		// 旧版 Qwen3.7-Max-Free 已下架，保留兼容别名
		{"qwen-3.7-max-free", "qwen-3.7-max-free"}, // 已下架：透传,

		// ---- 未知名称原样透传 ----
		{"some/unknown-model", "some/unknown-model"},
		{"", ""},
	}
	for _, c := range cases {
		got := MapModel(c.in)
		if got != c.want {
			t.Errorf("MapModel(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

// TestMapModelAllCatalogModels 确保 55 个模型每个都有至少一个可解析的短名。
func TestMapModelAllCatalogModels(t *testing.T) {
	for _, m := range catalogModels {
		// 用模型短名（厂商前缀后部分）解析，必须映射回自身
		id := m.ID
		short := id
		if i := lastSlash(id); i >= 0 {
			short = id[i+1:]
		}
		got := MapModel(short)
		if got != id {
			t.Errorf("MapModel(%q) = %q, want %q (self)", short, got, id)
		}
	}
}

func lastSlash(s string) int {
	for i := len(s) - 1; i >= 0; i-- {
		if s[i] == '/' {
			return i
		}
	}
	return -1
}