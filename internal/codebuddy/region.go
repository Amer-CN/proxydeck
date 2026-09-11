// region.go —— WorkBuddy 区域维度（CN 国内 / INTL 国际）。
//
// 简报（.work/task-workbuddy-intl.md）：在 WorkBuddy 甲板内做「国内版 ⇄ 国际版」
// 切换，不新增甲板/模式键。后端以 Region 承载全部差异：base URL、凭据 domain
// 过滤、模型候选池、是否强制 system、是否刷新 token、是否脱敏、模型探测。
// CN 各字段 = 引入区域前的现行为（默认参数逐字节保持）。
package codebuddy

import (
	"fmt"
	"strings"
)

// Region 服务区域。
type Region string

const (
	RegionCN   Region = "CN"
	RegionINTL Region = "INTL"
)

// ParseRegion 宽松解析区域标识（空/未知 → CN，即默认行为不变）。
func ParseRegion(s string) Region {
	switch s {
	case "INTL", "intl", "international":
		return RegionINTL
	default:
		return RegionCN
	}
}

// regionCfg 一个区域的全部后端差异。
type regionCfg struct {
	BaseURL      string   // 官方后端（chat/quota/probe 全走它）
	DomainSuffix string   // 凭据过滤：auth.domain 必须含此后缀（region 隔离，修 FindAuthFile 拿错文件的 bug）
	UserAgent    string   // 出站 User-Agent
	ForceSystem  bool     // 国际版 11128：首条消息必须是 system → 客户端没带时自动补一条
	NoRefresh    bool     // 国际版独立 Keycloak realm：不刷新 token，每次请求重读 auth 文件
	NoProbe      bool     // 关掉每小时自动探测（国际版：池内 id 已逐条实测，对付费模型探测等于白花钱）
	Candidates   []string // 模型候选池（CN 经探测只留真实可用的）
}

// intlDefaultSystem INTL 客户端消息缺 system 时自动补的最小中性 system。
const intlDefaultSystem = "You are a helpful assistant."

var regionConfigs = map[Region]regionCfg{
	RegionCN: {
		BaseURL:      "https://copilot.tencent.com",
		DomainSuffix: "codebuddy.cn",
		UserAgent:    "codebuddy2openai-go/1.0",
		ForceSystem:  false,
		NoRefresh:    false,
		Candidates:   modelCandidates,
	},
	RegionINTL: {
		// 2026-09-11 实测两 host 均 200；选 codebuddy.ai（与国内域名同形，客户端默认）
		BaseURL:      "https://www.codebuddy.ai",
		DomainSuffix: "workbuddy.ai",
		UserAgent:    "CLI/2.63.2 CodeBuddy/2.63.2",
		ForceSystem:  true,
		NoRefresh:    true,
		NoProbe:      true,
		// 池内 id 全部由主智能体 2026-09-11 客户端实测逐条验证（每条一条最小请求，无效 id 免费报错）：
		//   免费 3（倍率 0.00x，均 200）：deepseek-v4.1-flash / hy4-preview / hy3
		//   付费 10（均 200）：gpt-5.6-sol / gpt-5.6-terra / gpt-5.6-luna / gpt-5.5 / gpt-5.4 /
		//                     gpt-5.3-codex / glm-5.3 / glm-5.2 / kimi-k3 / kimi-k2.6
		//   官方 CLI 的角色别名 4 个（fast/balanced/primary/deep-model，实测均 200）**不入池**：
		//     它们与具体模型是同一批东西的两个名字，进池会在模型矩阵里多出四个无法辨认的
		//     伪厂商分组（用户 2026-09-11 反馈"有些不认识的模型"）。需要时再单独加回。
		//   写此注释时上游临时故障、但 id 有效：gpt-6-astra（500/11134）、gemini-3.5-flash（429/14003）
		// NoProbe=true：不做每小时自动探测——付费模型探测会持续扣费（用户 2026-09-11 裁决关掉）；
		// 池内 id 已人工验证过，失效时按实际报错处理。
		Candidates: []string{
			"deepseek-v4.1-flash", "hy4-preview", "hy3",
			"gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4", "gpt-5.3-codex",
			"glm-5.3", "glm-5.2", "kimi-k3", "kimi-k2.6",
			"gpt-6-astra", "gemini-3.5-flash",
		},
	},
}

// cfg 返回本服务的区域配置（零值 Server / 未设区域一律按 CN——默认行为不变）。
func (s *Server) cfg() regionCfg {
	r := s.region
	if r != RegionINTL {
		return regionConfigs[RegionCN]
	}
	return regionConfigs[r]
}

// validateRegion 区域要求的凭据文件缺失时如实报错（不许回退到另一区域的凭据）。
func validateRegion(r Region, domain string) error {
	c := regionConfigs[r]
	if domain != "" && containsDomain(domain, c.DomainSuffix) {
		return nil
	}
	return fmt.Errorf("未找到 %s 区域凭据（需要 auth.domain 含 %s 的登录文件）", r, c.DomainSuffix)
}

// containsDomain domain 匹配（子串包含即可：CN→codebuddy.cn，INTL→workbuddy.ai；
// 两者互不包含，不会误配）。
func containsDomain(domain, suffix string) bool {
	return domain != "" && suffix != "" && strings.Contains(domain, suffix)
}
