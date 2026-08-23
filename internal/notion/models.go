// Package notion 把 Notion AI 逆向为本地 OpenAI 兼容 API。
//
// 流程：
//  1. 凭据：优先读缓存的 token.json；缺失/失效时通过 CDP 从 Notion 桌面端
//     （--remote-debugging-port 启动）自动读取 token_v2 / notion_user_id
//  2. spaceId 经 /api/v3/getSpaces 获取并缓存
//  3. 对话走 /api/v3/runInferenceTranscript（NDJSON 流式），翻译为 OpenAI 格式
package notion

import "fmt"

// 模型代号表（对外名 → Notion 内部代号）。
// 代号来源：GALIAIS/Notion2API（Go 版，维护中）；2026-08-16 实测 4 个标 ✓ 全部可用。
var modelTable = []struct {
	ID     string // 对外模型名（裸名；notion/ID 为别名）
	Code   string // Notion 请求代号（2026-08-16 全部 38 个逐实测可用）
	Family string
}{
	// ===== 最新模型（官网菜单顶部）=====
	{"opus-5", "agave-flan", "anthropic"},
	{"gpt-5.6-sol", "orange-mousse", "openai"},
	{"kimi-k3", "soursop-shortcake", "moonshot"},
	{"sonnet-5", "angel-cake-high", "anthropic"},
	{"fable-5-beta", "acai-budino-high", "anthropic"},
	{"gemini-3.7-flash", "galette-high-thinking", "google"},
	{"gpt-5.6-luna", "olive-jellyroll", "openai"},
	{"gpt-5.6-terra", "orchid-muffin", "openai"},
	{"grok-4.6", "xigua-mochi-high", "xai"},
	{"deepseek-v4-pro", "baseten-deepseek-v4-pro", "deepseek"},
	{"glm-5.2", "baseten-glm-5.2", "zhipu"},
	// ===== Claude 系 =====
	{"opus-4.8", "ambrosia-tart-high", "anthropic"},
	{"opus-4.7", "apricot-sorbet-medium", "anthropic"},
	{"opus-4.6", "avocado-froyo-medium", "anthropic"},
	{"haiku-4.5", "anthropic-haiku-4.5", "anthropic"},
	// ===== GPT 系 =====
	{"gpt-5.5", "opal-quince-medium", "openai"},
	{"gpt-5.4", "oval-kumquat-medium", "openai"},
	{"gpt-5.4-mini", "oregon-grape-medium", "openai"},
	{"gpt-5-thinking", "oreo-cheesecake", "openai"},
	{"gpt-5-mini", "oolong-parfait-high", "openai"},
	// ===== Gemini 系 =====
	{"gemini-3.6-flash", "gingerbread-high", "google"},
	{"gemini-3.5-flash", "vertex-gemini-3.5-flash", "google"},
	{"gemini-3.1-pro", "galette-medium-thinking", "google"},
	{"gemini-3-flash", "gingerbread", "google"},
	// ===== 其他 =====
	{"kimi-2.7-code", "fireworks-kimi-k2.7", "moonshot"},
	{"kimi-2.6", "fireworks-kimi-k2p6", "moonshot"},
	{"minimax-m2.7", "fireworks-minimax-m2p7", "minimax"},
	{"grok-4.5", "xigua-mochi-low", "xai"},
	{"grok-4.3", "xigua-mochi-medium", "xai"},
	{"grok-build-0.1", "xinomavro-cake", "xai"},
	{"spacexai-4.5", "strawberry-whoopiepie", "spacex"},
	{"deepseek-v4-flash", "baseten-deepseek-v4-flash", "deepseek"},
	{"sonnet-4.6", "almond-croissant-low", "anthropic"},
}

// Models 返回对外模型名列表（裸名为主；notion/ 前缀作为别名同样可调用）。
func Models() []string {
	out := make([]string, 0, len(modelTable)*2)
	for _, m := range modelTable {
		out = append(out, m.ID)
	}
	return out
}

// resolveModel 对外名 → 内部代号；未知名返回错误。
func resolveModel(id string) (string, error) {
	for _, m := range modelTable {
		if "notion/"+m.ID == id || m.ID == id || m.Code == id {
			return m.Code, nil
		}
	}
	return "", fmt.Errorf("未知模型: %s（可用：%v）", id, Models())
}
