package qoder

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"time"
)

// 实时额度：Qoder CN 官方 App 同款接口（app.asar getAccountUsage 实录）：
// GET https://openapi.qoder.com.cn/api/v2/quota/usage
// 头：Authorization: Bearer <token> / Accept: application/json / Cosy-ClientType / User-Agent: Qoder。
// 注意域名是 CN 版 .com.cn——国际版 openapi.qoder.sh 会拒这个 token（TOKEN_INVALID）。

// usageQuota 是 /api/v2/quota/usage 响应里我们关心的字段。
type usageQuota struct {
	IsQuotaExceeded bool `json:"isQuotaExceeded"`
	UserQuota       *struct {
		Total     float64 `json:"total"`
		Used      float64 `json:"used"`
		Remaining float64 `json:"remaining"`
		Unit      string  `json:"unit"`
	} `json:"userQuota"`
	DedicatedResourcePackages []struct {
		Total     float64 `json:"total"`
		Used      float64 `json:"used"`
		Remaining float64 `json:"remaining"`
		Available bool    `json:"available"`
		ExpiresAt int64   `json:"expiresAt"`
	} `json:"dedicatedResourcePackages"`
}

// fetchUsage 用登录态 token 拉实时额度（套餐 + 专属包）。
func fetchUsage(token string) (*usageQuota, error) {
	req, err := http.NewRequest(http.MethodGet, "https://openapi.qoder.com.cn/api/v2/quota/usage", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Cosy-ClientType", "1")
	req.Header.Set("User-Agent", "Qoder")
	client := &http.Client{Timeout: 10 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 1024))
		return nil, fmt.Errorf("额度接口 HTTP %d: %s", resp.StatusCode, string(b))
	}
	var u usageQuota
	if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&u); err != nil {
		return nil, fmt.Errorf("额度响应解析失败: %v", err)
	}
	return &u, nil
}

// usageView 把 usageQuota 转成 /health 的 credits.realtime 视图：
// 套餐 {total,used,remaining} + 专属包数组（Qwen 系等，按有效期排序）+ 汇总剩余。
func usageView(u *usageQuota) map[string]any {
	if u == nil || u.UserQuota == nil {
		return nil
	}
	view := map[string]any{
		"plan": map[string]any{
			"total":     u.UserQuota.Total,
			"used":      u.UserQuota.Used,
			"remaining": u.UserQuota.Remaining,
			"unit":      u.UserQuota.Unit,
		},
		"isQuotaExceeded": u.IsQuotaExceeded,
	}
	packs := make([]map[string]any, 0, len(u.DedicatedResourcePackages))
	var availRemaining float64
	for _, p := range u.DedicatedResourcePackages {
		if p.Available {
			availRemaining += p.Remaining
		}
		packs = append(packs, map[string]any{
			"total":     p.Total,
			"used":      p.Used,
			"remaining": p.Remaining,
			"available": p.Available,
			"expiresAt": p.ExpiresAt,
		})
	}
	view["packages"] = packs
	view["packagesRemaining"] = availRemaining
	view["totalRemaining"] = u.UserQuota.Remaining + availRemaining
	return view
}

// cachedUsage 额度获取策略（用户明确要求：绝不自动轮询官方接口）：
// force=false（普通 /health 轮询）→ 只回已有缓存，从不访问官方接口；
// force=true（/health?refresh=1，积分卡「↻ 刷新」按钮）→ 现拉一次并更新缓存；
// 拉取失败回落旧缓存（有总比没有强）。缓存长期有效，直到下次手动刷新。
func (s *Server) cachedUsage(token string, force bool) *usageQuota {
	s.usageMu.Lock()
	defer s.usageMu.Unlock()
	if !force {
		return s.usageCache
	}
	u, err := fetchUsage(token)
	if err != nil {
		return s.usageCache
	}
	s.usageCache = u
	s.usageAt = time.Now()
	return u
}
