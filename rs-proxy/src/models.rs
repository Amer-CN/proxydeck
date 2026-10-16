//! `internal/proxy/models.go`：官方模型目录与 `/v1/models`。
//!
//! 目录（61 个，Go 套餐 36 个）逐条来自 Go `catalogModels`，
//! 由黄金门禁 `golden_proxy.rs::catalog_matches_go` 与真 Go 采收值逐条对照。
//! 数组顺序即 Go 里的分组顺序（alibaba → anthropic → …）；`sort_catalog` 复刻
//! `sort.SliceStable` 的「先 owned_by 再 id」排序。

/// 一个模型及其所属套餐（对应 Go 的 `modelInfo`）。
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    pub id: &'static str,
    pub owned_by: &'static str,
    /// 是否包含在 Go 套餐内（官方 <https://commandcode.ai/docs/plans/go>）。
    pub on_go: bool,
}

/// `catalogModels`：官方完整模型目录（按厂商 A→Z 分组）。
pub const CATALOG_MODELS: &[ModelInfo] = &[
    ModelInfo { id: "qwen-3.6-max", owned_by: "alibaba", on_go: true },
    ModelInfo { id: "qwen-3.6-plus", owned_by: "alibaba", on_go: true },
    ModelInfo { id: "qwen-3.7-flash", owned_by: "alibaba", on_go: true },
    ModelInfo { id: "qwen-3.7-max", owned_by: "alibaba", on_go: true },
    ModelInfo { id: "qwen-3.7-plus", owned_by: "alibaba", on_go: true },
    ModelInfo { id: "qwen-3.8-27b", owned_by: "alibaba", on_go: true },
    ModelInfo { id: "qwen-3.8-max", owned_by: "alibaba", on_go: true },
    ModelInfo { id: "claude-fable-5", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-haiku-4-5", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-opus-4-6", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-opus-4-7", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-opus-4-8", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-opus-5", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-sonnet-4-5", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-sonnet-4-6", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "claude-sonnet-5", owned_by: "anthropic", on_go: false },
    ModelInfo { id: "deepseek-v4-flash", owned_by: "deepseek", on_go: true },
    ModelInfo { id: "deepseek-v4-flash-vision-exp", owned_by: "deepseek", on_go: true },
    ModelInfo { id: "deepseek-v4-pro", owned_by: "deepseek", on_go: true },
    ModelInfo { id: "gemini-3.1-flash-lite", owned_by: "google", on_go: false },
    ModelInfo { id: "gemini-3.5-flash", owned_by: "google", on_go: false },
    ModelInfo { id: "gemini-3.5-flash-lite", owned_by: "google", on_go: false },
    ModelInfo { id: "gemini-3.6-flash", owned_by: "google", on_go: false },
    ModelInfo { id: "gemini-3.7-flash", owned_by: "google", on_go: false },
    ModelInfo { id: "ling-3.0-flash-free", owned_by: "inclusionai", on_go: false },
    ModelInfo { id: "muse-spark-1.1", owned_by: "meta", on_go: false },
    ModelInfo { id: "muse-spark-1.2", owned_by: "meta", on_go: false },
    ModelInfo { id: "muse-spark-1.2-contributor", owned_by: "meta", on_go: true },
    ModelInfo { id: "minimax-m2.5", owned_by: "minimaxai", on_go: true },
    ModelInfo { id: "minimax-m2.7", owned_by: "minimaxai", on_go: true },
    ModelInfo { id: "minimax-m3", owned_by: "minimaxai", on_go: true },
    ModelInfo { id: "kimi-k2.5", owned_by: "moonshotai", on_go: true },
    ModelInfo { id: "kimi-k2.6", owned_by: "moonshotai", on_go: true },
    ModelInfo { id: "kimi-k2.7-code", owned_by: "moonshotai", on_go: true },
    ModelInfo { id: "kimi-k2.7-code-highspeed", owned_by: "moonshotai", on_go: true },
    ModelInfo { id: "kimi-k3", owned_by: "moonshotai", on_go: true },
    ModelInfo { id: "nemotron-3-ultra", owned_by: "nvidia", on_go: true },
    ModelInfo { id: "gpt-5.3-codex", owned_by: "openai", on_go: false },
    ModelInfo { id: "gpt-5.4", owned_by: "openai", on_go: false },
    ModelInfo { id: "gpt-5.4-mini", owned_by: "openai", on_go: false },
    ModelInfo { id: "gpt-5.5", owned_by: "openai", on_go: false },
    ModelInfo { id: "gpt-5.6-luna", owned_by: "openai", on_go: true },
    ModelInfo { id: "gpt-5.6-sol", owned_by: "openai", on_go: false },
    ModelInfo { id: "gpt-5.6-terra", owned_by: "openai", on_go: false },
    ModelInfo { id: "laguna-s-2.1-free", owned_by: "poolside", on_go: true },
    ModelInfo { id: "fugu-ultra", owned_by: "sakana", on_go: false },
    ModelInfo { id: "ox-alpha", owned_by: "stealth", on_go: true },
    ModelInfo { id: "step-3.5-flash", owned_by: "stepfun", on_go: true },
    ModelInfo { id: "step-3.7-flash", owned_by: "stepfun", on_go: true },
    ModelInfo { id: "tencent/hy3-paid", owned_by: "tencent", on_go: true },
    ModelInfo { id: "inkling", owned_by: "thinkingmachines", on_go: true },
    ModelInfo { id: "inkling-small", owned_by: "thinkingmachines", on_go: true },
    ModelInfo { id: "grok-4.5", owned_by: "xai", on_go: true },
    ModelInfo { id: "grok-4.6", owned_by: "xai", on_go: false },
    ModelInfo { id: "mimo-v2.5", owned_by: "xiaomi", on_go: true },
    ModelInfo { id: "mimo-v2.5-pro", owned_by: "xiaomi", on_go: true },
    ModelInfo { id: "glm-5", owned_by: "zai-org", on_go: true },
    ModelInfo { id: "glm-5.1", owned_by: "zai-org", on_go: true },
    ModelInfo { id: "glm-5.2", owned_by: "zai-org", on_go: true },
    ModelInfo { id: "glm-5.2-fast", owned_by: "zai-org", on_go: true },
    ModelInfo { id: "glm-5.3", owned_by: "zai-org", on_go: true },
];

/// `sort.SliceStable` 的等价物：先 `owned_by`、再 `id`。
///
/// ⚠ Rust 的 `slice::sort_by` 是**稳定**排序（与 Go 的 SliceStable 同），
/// 且比较函数已把两个键都覆盖，故两份实现给出同一序列。
pub fn sort_catalog(items: &mut [ModelInfo]) {
    items.sort_by(|a, b| {
        a.owned_by
            .as_bytes()
            .cmp(b.owned_by.as_bytes())
            .then_with(|| a.id.as_bytes().cmp(b.id.as_bytes()))
    });
}

/// Go 套餐内的模型（`?plan=go`）。
pub fn go_plan_models() -> Vec<ModelInfo> {
    let mut v: Vec<ModelInfo> = CATALOG_MODELS.iter().copied().filter(|m| m.on_go).collect();
    sort_catalog(&mut v);
    v
}

/// 全部模型，按 Go 的排序口径返回。
pub fn all_models() -> Vec<ModelInfo> {
    let mut v: Vec<ModelInfo> = CATALOG_MODELS.to_vec();
    sort_catalog(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_size_matches_go() {
        assert_eq!(CATALOG_MODELS.len(), 61, "官方目录 61 个模型");
        assert_eq!(
            CATALOG_MODELS.iter().filter(|m| m.on_go).count(),
            36,
            "Go 套餐模型数"
        );
    }

    #[test]
    fn catalog_ids_are_unique() {
        let mut ids: Vec<&str> = CATALOG_MODELS.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "目录里有重复 id");
    }

    #[test]
    fn sort_is_by_owner_then_id() {
        let sorted = all_models();
        for w in sorted.windows(2) {
            let ok = (w[0].owned_by, w[0].id) <= (w[1].owned_by, w[1].id);
            assert!(ok, "排序错位：{:?} / {:?}", w[0], w[1]);
        }
    }
}
