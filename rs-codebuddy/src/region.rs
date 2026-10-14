//! region.rs —— WorkBuddy 区域维度（CN 国内 / INTL 国际），对应 Go `internal/codebuddy/region.go`。
//!
//! 本模块只承载**请求/合同侧**的区域差异（P5a 范围）：base URL、模型候选池、是否强制首条
//! system、是否做在线探测、出站 User-Agent 常量。凭据过滤（domain 后缀）、token 刷新策略
//! （`NoRefresh`）与凭据文件搜索属 P5b，它们的常量在此登记供 P5b 使用，但本轮不驱动任何行为。

/// 服务区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Cn,
    Intl,
}

impl Region {
    /// Go `ParseRegion`：空/未知 → CN（默认行为不变）。
    pub fn parse(s: &str) -> Region {
        match s {
            "INTL" | "intl" | "international" => Region::Intl,
            _ => Region::Cn,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Region::Cn => "CN",
            Region::Intl => "INTL",
        }
    }

    pub fn cfg(self) -> &'static RegionCfg {
        match self {
            Region::Cn => &REGION_CN,
            Region::Intl => &REGION_INTL,
        }
    }
}

/// 一个区域的全部后端差异（字段与 Go `regionCfg` 同名同义）。
pub struct RegionCfg {
    /// 官方后端（chat/quota/probe 全走它）。
    pub base_url: &'static str,
    /// 凭据过滤：auth.domain 必须含其中任一条后缀即命中（**P5b 使用**）。
    pub domain_suffixes: &'static [&'static str],
    /// 出站 User-Agent（**P5b 的 buildHeaders 使用**；本模块只登记常量）。
    pub user_agent: &'static str,
    /// 国际版 11128：首条消息必须是 system → 客户端没带时自动补一条。
    pub force_system: bool,
    /// 国际版独立 Keycloak realm：不刷新 token（**P5b 使用**）。
    pub no_refresh: bool,
    /// 关掉每小时自动探测（国际版：池内 id 已逐条实测，对付费模型探测等于白花钱）。
    pub no_probe: bool,
    /// 模型候选池（CN 经探测只留真实可用的）。
    pub candidates: &'static [&'static str],
}

/// INTL 客户端消息缺 system 时自动补的最小中性 system（Go `intlDefaultSystem`）。
pub const INTL_DEFAULT_SYSTEM: &str = "You are a helpful assistant.";

/// Go `modelCandidates`（逐条照抄，含注释里的实测结论见 crate README）。
pub const MODEL_CANDIDATES: &[&str] = &[
    "auto",
    "hy4-preview",
    "hy3",
    "glm-5.3",
    "glm-5.3-flash",
    "glm-5.2",
    "glm-5.1",
    "glm-5v-turbo",
    "minimax-m3",
    "kimi-k3",
    "kimi-k2.7",
    "kimi-k2.6",
    "deepseek-v4.1-flash",
    "deepseek-v4-flash",
    "deepseek-v4-pro",
];

/// Go `regionConfigs[RegionINTL].Candidates`（池内 id 全部由客户端实测逐条验证过）。
pub const INTL_CANDIDATES: &[&str] = &[
    "deepseek-v4.1-flash",
    "hy4-preview",
    "hy3",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.3-codex",
    "glm-5.3",
    "glm-5.2",
    "kimi-k3",
    "kimi-k2.6",
    "gpt-6-astra",
    "gemini-3.5-flash",
];

pub static REGION_CN: RegionCfg = RegionCfg {
    base_url: "https://copilot.tencent.com",
    // 2026-10-13：WorkBuddy CN 桌面端 5.7.x 把 auth.domain 从 www.codebuddy.cn 改成
    // www.workbuddy.cn——两条并认（P5b 使用）。
    domain_suffixes: &["codebuddy.cn", "workbuddy.cn"],
    user_agent: "codebuddy2openai-go/1.0",
    force_system: false,
    no_refresh: false,
    no_probe: false,
    candidates: MODEL_CANDIDATES,
};

pub static REGION_INTL: RegionCfg = RegionCfg {
    base_url: "https://www.codebuddy.ai",
    domain_suffixes: &["workbuddy.ai"],
    user_agent: "CLI/2.63.2 CodeBuddy/2.63.2",
    force_system: true,
    no_refresh: true,
    no_probe: true,
    candidates: INTL_CANDIDATES,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_region_matches_go() {
        assert_eq!(Region::parse("INTL"), Region::Intl);
        assert_eq!(Region::parse("intl"), Region::Intl);
        assert_eq!(Region::parse("international"), Region::Intl);
        assert_eq!(Region::parse(""), Region::Cn);
        assert_eq!(Region::parse("CN"), Region::Cn);
        assert_eq!(Region::parse("weird"), Region::Cn);
    }

    #[test]
    fn candidates_and_cfg_match_go() {
        assert_eq!(Region::Cn.cfg().candidates.len(), 15);
        assert_eq!(Region::Intl.cfg().candidates.len(), 15);
        assert_eq!(Region::Cn.cfg().base_url, "https://copilot.tencent.com");
        assert_eq!(Region::Intl.cfg().base_url, "https://www.codebuddy.ai");
        assert!(!Region::Cn.cfg().force_system && Region::Intl.cfg().force_system);
        assert!(!Region::Cn.cfg().no_probe && Region::Intl.cfg().no_probe);
    }
}
