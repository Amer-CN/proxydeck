//! `internal/proxy/model.go`：短名 → CommandCode 官方完整模型 ID 的别名表。
//!
//! 规则（与官方 <https://commandcode.ai/docs/reference/cli/models> 一致）：
//!   - 完整 ID（如 `kimi-k3`）原样透传；
//!   - 短名（如 `kimi`）通过别名表解析 —— 大小写不敏感、忽略厂商前缀与分隔符；
//!   - 未知名称原样透传（由上游决定是否接受）。
//!
//! 表本身逐条来自 Go `internal/proxy/model.go` 的 `modelAliases`（键为 `normalizeAlias` 的输出），
//! 并由黄金门禁 `golden_proxy.rs::alias_table_matches_go` 与真 Go 采收值**逐键逐值**对照。

/// `MapModel`：把客户端传入的模型名映射为上游完整 ID。
pub fn map_model(name: &str) -> String {
    let key = normalize_alias(name);
    match lookup(&key) {
        Some(v) => v.to_string(),
        None => name.to_string(),
    }
}

/// `normalizeAlias`：小写、去厂商前缀、去所有非字母数字字符。
pub fn normalize_alias(name: &str) -> String {
    let s = name.trim().to_lowercase();
    // 去掉厂商前缀（`deepseek/...`、`moonshotai/...` 等）
    let s = match s.find('/') {
        Some(i) => &s[i + 1..],
        None => &s[..],
    };
    s.chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// `modelAliases` 的 (归一化键, 完整 ID) 表。
///
/// 用**有序数组 + 二分查找**而不是 HashMap：表是编译期常量、键有序，
/// 门禁可以直接逐条比数组（HashMap 的迭代序随机，不方便逐条对照）。
pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("claudefable5", "claude-fable-5"),
    ("claudehaiku45", "claude-haiku-4-5"),
    ("claudeopus46", "claude-opus-4-6"),
    ("claudeopus47", "claude-opus-4-7"),
    ("claudeopus48", "claude-opus-4-8"),
    ("claudeopus5", "claude-opus-5"),
    ("claudesonnet45", "claude-sonnet-4-5"),
    ("claudesonnet46", "claude-sonnet-4-6"),
    ("claudesonnet5", "claude-sonnet-5"),
    ("deepseekflash", "deepseek-v4-flash"),
    ("deepseekpro", "deepseek-v4-pro"),
    ("deepseekv4", "deepseek-v4-pro"),
    ("deepseekv4flash", "deepseek-v4-flash"),
    ("deepseekv4flashvisionexp", "deepseek-v4-flash-vision-exp"),
    ("deepseekv4pro", "deepseek-v4-pro"),
    ("deepseekvision", "deepseek-v4-flash-vision-exp"),
    ("fable5", "claude-fable-5"),
    ("fugu", "fugu-ultra"),
    ("fuguultra", "fugu-ultra"),
    ("gemini31flashlite", "gemini-3.1-flash-lite"),
    ("gemini35flash", "gemini-3.5-flash"),
    ("gemini35flashlite", "gemini-3.5-flash-lite"),
    ("gemini36flash", "gemini-3.6-flash"),
    ("gemini37flash", "gemini-3.7-flash"),
    ("glm", "glm-5.3"),
    ("glm5", "glm-5"),
    ("glm51", "glm-5.1"),
    ("glm52", "glm-5.2"),
    ("glm52fast", "glm-5.2-fast"),
    ("glm53", "glm-5.3"),
    ("gpt53codex", "gpt-5.3-codex"),
    ("gpt54", "gpt-5.4"),
    ("gpt54mini", "gpt-5.4-mini"),
    ("gpt55", "gpt-5.5"),
    ("gpt56luna", "gpt-5.6-luna"),
    ("gpt56sol", "gpt-5.6-sol"),
    ("gpt56terra", "gpt-5.6-terra"),
    ("grok", "grok-4.6"),
    ("grok45", "grok-4.5"),
    ("grok46", "grok-4.6"),
    ("haiku45", "claude-haiku-4-5"),
    ("hy3", "tencent/hy3-paid"),
    ("hy3paid", "tencent/hy3-paid"),
    ("inkling", "inkling"),
    ("inklingsmall", "inkling-small"),
    ("kimi", "kimi-k3"),
    ("kimik25", "kimi-k2.5"),
    ("kimik26", "kimi-k2.6"),
    ("kimik27code", "kimi-k2.7-code"),
    ("kimik27codehighspeed", "kimi-k2.7-code-highspeed"),
    ("kimik3", "kimi-k3"),
    ("laguna", "laguna-s-2.1-free"),
    ("lagunas21", "laguna-s-2.1-free"),
    ("lagunas21free", "laguna-s-2.1-free"),
    ("ling", "ling-3.0-flash-free"),
    ("ling30flash", "ling-3.0-flash-free"),
    ("ling30flashfree", "ling-3.0-flash-free"),
    ("luna", "gpt-5.6-luna"),
    ("mimo", "mimo-v2.5"),
    ("mimov25", "mimo-v2.5"),
    ("mimov25pro", "mimo-v2.5-pro"),
    ("minimax3", "minimax-m3"),
    ("minimaxm25", "minimax-m2.5"),
    ("minimaxm27", "minimax-m2.7"),
    ("minimaxm3", "minimax-m3"),
    ("musespark11", "muse-spark-1.1"),
    ("musespark12", "muse-spark-1.2"),
    ("musespark12contributor", "muse-spark-1.2-contributor"),
    ("nemotron", "nemotron-3-ultra"),
    ("nemotron3ultra", "nemotron-3-ultra"),
    ("nemotron3ultra550ba55b", "nemotron-3-ultra"),
    ("opus", "claude-opus-5"),
    ("opus46", "claude-opus-4-6"),
    ("opus47", "claude-opus-4-7"),
    ("opus48", "claude-opus-4-8"),
    ("oxalpha", "ox-alpha"),
    ("qwen36", "qwen-3.6-plus"),
    ("qwen36max", "qwen-3.6-max"),
    ("qwen36plus", "qwen-3.6-plus"),
    ("qwen37flash", "qwen-3.7-flash"),
    ("qwen37max", "qwen-3.7-max"),
    ("qwen37plus", "qwen-3.7-plus"),
    ("qwen3827b", "qwen-3.8-27b"),
    ("qwen38max", "qwen-3.8-max"),
    ("sol", "gpt-5.6-sol"),
    ("sonnet", "claude-sonnet-5"),
    ("sonnet45", "claude-sonnet-4-5"),
    ("sonnet46", "claude-sonnet-4-6"),
    ("sonnet5", "claude-sonnet-5"),
    ("step35flash", "step-3.5-flash"),
    ("step37flash", "step-3.7-flash"),
    ("terra", "gpt-5.6-terra"),
];

fn lookup(key: &str) -> Option<&'static str> {
    MODEL_ALIASES
        .binary_search_by(|(k, _)| (*k).cmp(key))
        .ok()
        .map(|i| MODEL_ALIASES[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_unique() {
        // 二分查找的前提：严格升序（无重复键）
        for w in MODEL_ALIASES.windows(2) {
            assert!(w[0].0 < w[1].0, "别名表未严格升序：{:?} / {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn normalize_alias_rules() {
        assert_eq!(normalize_alias("QWEN-3.8-MAX"), "qwen38max");
        assert_eq!(normalize_alias("  glm-5.3  "), "glm53");
        assert_eq!(normalize_alias("moonshotai/Kimi-K3"), "kimik3");
        assert_eq!(normalize_alias("deepseek/flash"), "flash");
        assert_eq!(normalize_alias(""), "");
    }

    #[test]
    fn unknown_names_pass_through() {
        assert_eq!(map_model("some/unknown-model"), "some/unknown-model");
        assert_eq!(map_model(""), "");
        assert_eq!(map_model("qwen-3.6-max-preview"), "qwen-3.6-max-preview");
    }
}
