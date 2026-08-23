package proxy

import "strings"

// normalizedModelID 把客户端传入的模型名映射为 CommandCode 官方完整模型 ID。
// 规则（与官方 https://commandcode.ai/docs/reference/cli/models 一致）：
//   - 完整 ID（如 "moonshotai/Kimi-K3"）原样透传；
//   - 短名（如 "kimi-k3"）通过别名表解析 —— 大小写不敏感、忽略厂商前缀与分隔符；
//   - 未知名称原样透传（由上游 CommandCode 决定是否接受）。
func MapModel(name string) string {
	key := normalizeAlias(name)
	if v, ok := modelAliases[key]; ok {
		return v
	}
	return name // pass through as-is
}

// normalizeAlias 把任意输入归一化成别名键：小写、去厂商前缀、去非字母数字。
func normalizeAlias(name string) string {
	s := strings.ToLower(strings.TrimSpace(name))
	// 去掉厂商前缀（"deepseek/..."、"moonshotai/..." 等）
	if i := strings.IndexByte(s, '/'); i >= 0 {
		s = s[i+1:]
	}
	// 去掉所有非字母数字字符（-、.、_、空格等）
	var b strings.Builder
	for _, r := range s {
		if (r >= 'a' && r <= 'z') || (r >= '0' && r <= '9') {
			b.WriteRune(r)
		}
	}
	return b.String()
}

// modelAliases 短名 → 官方完整 ID 别名表（覆盖全部 55 个模型）。
// 键为 normalizeAlias 的输出；每个模型至少一个别名，常见变体尽量收录。
// modelAliases 短名/旧前缀名 → 官方模型 ID（2026-08-22 官方目录改用裸名格式）。
// 归一化短名（小写去符号）可直接输入；旧前缀全名（如 "moonshotai/Kimi-K3"）
// 作为兼容别名保留，统一映射到新裸名。
var modelAliases = map[string]string{
	"claudefable5": "claude-fable-5",
	"claudehaiku45": "claude-haiku-4-5",
	"claudeopus46": "claude-opus-4-6",
	"claudeopus47": "claude-opus-4-7",
	"claudeopus48": "claude-opus-4-8",
	"claudeopus5": "claude-opus-5",
	"claudesonnet45": "claude-sonnet-4-5",
	"claudesonnet46": "claude-sonnet-4-6",
	"claudesonnet5": "claude-sonnet-5",
	"deepseekv4flash": "deepseek-v4-flash",
	"deepseekv4flashvisionexp": "deepseek-v4-flash-vision-exp",
	"deepseekv4pro": "deepseek-v4-pro",
	"fuguultra": "fugu-ultra",
	"gemini31flashlite": "gemini-3.1-flash-lite",
	"gemini35flash": "gemini-3.5-flash",
	"gemini35flashlite": "gemini-3.5-flash-lite",
	"gemini36flash": "gemini-3.6-flash",
	"gemini37flash": "gemini-3.7-flash",
	"glm5": "glm-5",
	"glm51": "glm-5.1",
	"glm53": "glm-5.3",
	"gpt53codex": "gpt-5.3-codex",
	"gpt54": "gpt-5.4",
	"gpt54mini": "gpt-5.4-mini",
	"gpt55": "gpt-5.5",
	"gpt56luna": "gpt-5.6-luna",
	"gpt56sol": "gpt-5.6-sol",
	"gpt56terra": "gpt-5.6-terra",
	"grok45": "grok-4.5",
	"grok46": "grok-4.6",
	"hy3paid": "tencent/hy3-paid",
	"inkling": "inkling",
	"inklingsmall": "inkling-small",
	"kimik25": "kimi-k2.5",
	"kimik26": "kimi-k2.6",
	"kimik27code": "kimi-k2.7-code",
	"kimik27codehighspeed": "kimi-k2.7-code-highspeed",
	"kimik3": "kimi-k3",
	"lagunas21free": "laguna-s-2.1-free",
	"ling30flashfree": "ling-3.0-flash-free",
	"mimov25": "mimo-v2.5",
	"mimov25pro": "mimo-v2.5-pro",
	"minimaxm25": "minimax-m2.5",
	"minimaxm27": "minimax-m2.7",
	"minimaxm3": "minimax-m3",
	"musespark11": "muse-spark-1.1",
	"musespark12": "muse-spark-1.2",
	"musespark12contributor": "muse-spark-1.2-contributor",
	"nemotron3ultra": "nemotron-3-ultra",
	"nemotron3ultra550ba55b": "nemotron-3-ultra",
	"oxalpha": "ox-alpha",
	"qwen36max": "qwen-3.6-max",
	"qwen36plus": "qwen-3.6-plus",
	"qwen37flash": "qwen-3.7-flash",
	"qwen37max": "qwen-3.7-max",
	"qwen37plus": "qwen-3.7-plus",
	"qwen3827b": "qwen-3.8-27b",
	"qwen38max": "qwen-3.8-max",
	"step35flash": "step-3.5-flash",
	"step37flash": "step-3.7-flash",
	"mimo": "mimo-v2.5",

	// 惯用超短名（人工补充：opus/sonnet/kimi/glm/ling 等人类惯用叫法）
	"lagunas21": "laguna-s-2.1-free",
	"laguna": "laguna-s-2.1-free",
	"fugu": "fugu-ultra",
	"hy3": "tencent/hy3-paid",
	"minimax3": "minimax-m3",
	"nemotron": "nemotron-3-ultra",
	"deepseekflash": "deepseek-v4-flash",
	"deepseekpro": "deepseek-v4-pro",
	"deepseekv4": "deepseek-v4-pro",
	"deepseekvision": "deepseek-v4-flash-vision-exp",
	"fable5": "claude-fable-5",
	"glm": "glm-5.3",
	"glm52": "glm-5.2",
	"glm52fast": "glm-5.2-fast",
	"grok": "grok-4.6",
	"haiku45": "claude-haiku-4-5",
	"kimi": "kimi-k3",
	"ling": "ling-3.0-flash-free",
	"ling30flash": "ling-3.0-flash-free",
	"luna": "gpt-5.6-luna",
	"opus": "claude-opus-5",
	"opus46": "claude-opus-4-6",
	"opus47": "claude-opus-4-7",
	"opus48": "claude-opus-4-8",
	"qwen36": "qwen-3.6-plus",
	"sol": "gpt-5.6-sol",
	"sonnet": "claude-sonnet-5",
	"sonnet45": "claude-sonnet-4-5",
	"sonnet46": "claude-sonnet-4-6",
	"sonnet5": "claude-sonnet-5",
	"terra": "gpt-5.6-terra",
}
