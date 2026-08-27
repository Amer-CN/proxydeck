// quota.go —— 团结积分（官网 dashboard 口径）查询。
//
// 数据源：官网 dashboard 同款接口
// GET https://codely.tuanjie.cn/api/user/billing/usage/summary
// （Bearer access_token 认证，直连官网不走 LiteLLM Forward）。
// 总积分 = 今日配额剩余 + 赠送剩余 + 有效赠送余额；今日配额/月度统计/
// 消耗 TOP 均来自该接口，口径与官网一致（LiteLLM /team/info 的
// budget/spend 是 30 天滚动，与官网不一致，已弃用）。
package tuanjie

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sort"
	"strconv"
	"sync"
	"time"
)

const (
	billingSummaryURL = codelyAPIBase + "/api/user/billing/usage/summary"
	billingTimeout    = 15 * time.Second
)

// Quota 是积分快照（GUI 链路监测积分卡用，官网口径）。
type Quota struct {
	Total    float64       `json:"total"`              // 总积分 = 今日剩余 + 赠送剩余 + 有效赠送
	Daily    DailyQuota    `json:"daily"`              // 今日配额（北京时间 0-24 点）
	Gift     GiftQuota     `json:"gift"`               // 赠送积分
	Month    MonthQuota    `json:"month"`              // 月度统计
	BySource []SourceSpend `json:"bySource,omitempty"` // 消耗 TOP（按积分降序）
	Plan     PlanQuota      `json:"plan,omitempty"`  // 套餐窗口（coding_plan，LITE 三档）
	Source   string        `json:"source"`             // live / cache / none
	Err      string        `json:"err,omitempty"`
}

// DailyQuota 今日配额。
type DailyQuota struct {
	Quota     float64 `json:"quota"`
	Used      float64 `json:"used"`
	Remaining float64 `json:"remaining"`
	PeriodEnd string  `json:"periodEnd,omitempty"` // UTC 时间，界面按北京时间显示
}

// GiftQuota 赠送积分。
type GiftQuota struct {
	Remaining float64 `json:"remaining"`
}

// PlanQuota 套餐窗口（官网 coding_plan 字段——此前漏解析，日配额被误当
// 套餐总量显示，实际套餐是三窗口：5h/周/月，如 LITE 800/4000/16000）。
type PlanQuota struct {
	EffectiveRemaining float64     `json:"effective_remaining"`
	NextBoundaryAt     string      `json:"next_boundary_at,omitempty"`
	Windows            []PlanWindow `json:"windows,omitempty"`
}

// PlanWindow 单窗口：type=usage_5h|subscription_week|subscription_month。
type PlanWindow struct {
	Type      string  `json:"type"`
	Quota     float64 `json:"quota"`
	Used      float64 `json:"used"`
	Remaining float64 `json:"remaining"`
	Exhausted bool    `json:"exhausted"`
	PeriodEnd string  `json:"period_end,omitempty"`
}

// MonthQuota 月度统计。
type MonthQuota struct {
	Points           float64 `json:"points"`
	TotalTokens      int64   `json:"totalTokens"`
	PromptTokens     int64   `json:"promptTokens"`
	CompletionTokens int64   `json:"completionTokens"`
	Requests         int64   `json:"requests"`
	StartDate        string  `json:"startDate,omitempty"`
	EndDate          string  `json:"endDate,omitempty"`
}

// SourceSpend 单来源消耗（label 即模型/路由名）。
type SourceSpend struct {
	Label    string  `json:"label"`
	Points   float64 `json:"points"`
	Tokens   int64   `json:"tokens"`
	Requests int64   `json:"requests"`
}

type quotaCacheEntry struct {
	q  *Quota
	at time.Time
}

// quotaCache 按 token 哈希键控的积分缓存。官网对同 token 高频调用会 429
// 限流（2026-08-27 事故：GUI 每 3s 轮询 /quota × FetchQuotaFor 无缓存 =
// 每分钟 20 次直打官网，用户官网查积分直接 429），故两条查询路径统一走这里。
type quotaCache struct {
	mu sync.Mutex
	m  map[string]quotaCacheEntry
}

var qc = quotaCache{m: map[string]quotaCacheEntry{}}

const (
	quotaTTL    = 5 * time.Minute
	quotaErrTTL = 60 * time.Second // 失败负缓存：短时间内不重试，避免持续打官网
)

// quotaCacheKey 对 token 做 sha256 截断做缓存键（明文 token 不进内存键）。
func quotaCacheKey(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:8])
}

// fetchQuotaCached 带 per-token 缓存的积分拉取（成功 5 分钟，失败负缓存 60 秒；
// 失败但有旧成功缓存时回退旧值）。force=true 跳过缓存直拉官网并刷新缓存
// （GUI「↻ 刷新」按钮用）。
func (c *Client) fetchQuotaCached(ctx context.Context, at string, force bool) *Quota {
	key := quotaCacheKey(at)
	if !force {
		qc.mu.Lock()
		if e, ok := qc.m[key]; ok {
			ttl := quotaTTL
			if e.q.Source == "none" {
				ttl = quotaErrTTL
			}
			if time.Since(e.at) < ttl {
				cp := *e.q
				if e.q.Source == "live" {
					cp.Source = "cache"
				}
				qc.mu.Unlock()
				return &cp
			}
		}
		qc.mu.Unlock()
	}

	q, err := c.fetchQuotaWithToken(ctx, at)
	qc.mu.Lock()
	defer qc.mu.Unlock()
	if err != nil {
		if e, ok := qc.m[key]; ok && e.q.Source == "live" {
			cp := *e.q
			cp.Source = "cache"
			cp.Err = err.Error()
			return &cp
		}
		eq := &Quota{Source: "none", Err: err.Error()}
		qc.m[key] = quotaCacheEntry{q: eq, at: time.Now()}
		return &Quota{Source: "none", Err: err.Error()}
	}
	qc.m[key] = quotaCacheEntry{q: q, at: time.Now()}
	return q
}

// FetchQuota 拉取官网积分快照（本地账号 token，带缓存）。
func (c *Client) FetchQuota(ctx context.Context) *Quota {
	at, err := loadAccessToken()
	if err != nil {
		return &Quota{Source: "none", Err: err.Error()}
	}
	return c.fetchQuotaCached(ctx, at, false)
}

// FetchQuotaFor 用指定 access_token 查积分（账号池切换账号用），同走缓存。
func (c *Client) FetchQuotaFor(ctx context.Context, accessToken string) *Quota {
	return c.fetchQuotaCached(ctx, accessToken, false)
}

// FetchQuotaForce 强制实时拉取并刷新缓存（GUI「↻ 刷新」按钮用）。
// accessToken 为空时用本地客户端 token。
func (c *Client) FetchQuotaForce(ctx context.Context, accessToken string) *Quota {
	at := accessToken
	if at == "" {
		t, err := loadAccessToken()
		if err != nil {
			return &Quota{Source: "none", Err: err.Error()}
		}
		at = t
	}
	return c.fetchQuotaCached(ctx, at, true)
}

func (c *Client) fetchQuotaWithToken(ctx context.Context, at string) (*Quota, error) {
	ctx2, cancel := context.WithTimeout(ctx, billingTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx2, http.MethodGet, billingSummaryURL, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+at)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", cliUserAgent)
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("官网积分接口请求失败: %w", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 2<<20))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("官网积分接口 HTTP %d: %s", resp.StatusCode, truncate(string(body), 200))
	}
	var s summaryResp
	if err := json.Unmarshal(body, &s); err != nil {
		return nil, fmt.Errorf("官网积分接口响应解析失败: %w", err)
	}

	q := &Quota{Source: "live"}
	q.Daily.Quota, _ = strconv.ParseFloat(s.DailyAllowance.QuotaPoints, 64)
	q.Daily.Used, _ = strconv.ParseFloat(s.DailyAllowance.UsedPoints, 64)
	q.Daily.Remaining, _ = strconv.ParseFloat(s.DailyAllowance.RemainingPoints, 64)
	q.Daily.PeriodEnd = s.DailyAllowance.PeriodEndAt
	q.Gift.Remaining, _ = strconv.ParseFloat(s.GiftCredits.RemainingPoints, 64)
	q.Plan.EffectiveRemaining, _ = strconv.ParseFloat(s.CodingPlan.EffectiveRemainingPoints, 64)
	q.Plan.NextBoundaryAt = s.CodingPlan.NextBoundaryAt
	for _, w := range s.CodingPlan.Windows {
		pw := PlanWindow{Type: w.WindowType, Exhausted: w.Exhausted, PeriodEnd: w.Period.EndAt}
		pw.Quota, _ = strconv.ParseFloat(w.QuotaPoints, 64)
		pw.Used, _ = strconv.ParseFloat(w.UsedPoints, 64)
		pw.Remaining, _ = strconv.ParseFloat(w.RemainingPoints, 64)
		q.Plan.Windows = append(q.Plan.Windows, pw)
	}
	q.Total = q.Daily.Remaining + q.Gift.Remaining + s.Billing.EffectiveAvailablePoints
	q.Month = MonthQuota{
		Points:           s.Totals.RecordedPoints,
		TotalTokens:      s.Totals.TotalTokens,
		PromptTokens:     s.Totals.PromptTokens,
		CompletionTokens: s.Totals.CompletionTokens,
		Requests:         s.Totals.SettlementCount,
		StartDate:        s.Period.StartDate,
		EndDate:          s.Period.EndDate,
	}
	// 合并同名 label：后端按 source_type 拆多条（如 codely-core×2），界面只按模型名显示
	merged := map[string]*SourceSpend{}
	for _, src := range s.Totals.BySource {
		if m, ok := merged[src.Label]; ok {
			m.Points += src.RecordedPoints
			m.Tokens += src.TotalTokens
			m.Requests += src.SettlementCount
		} else {
			merged[src.Label] = &SourceSpend{Label: src.Label, Points: src.RecordedPoints, Tokens: src.TotalTokens, Requests: src.SettlementCount}
		}
	}
	for _, m := range merged {
		q.BySource = append(q.BySource, *m)
	}
	sort.Slice(q.BySource, func(i, j int) bool { return q.BySource[i].Points > q.BySource[j].Points })
	return q, nil
}

// summaryResp 官网 /api/user/billing/usage/summary 响应（只取界面用到的字段）。
// 注意：points 类字段是字符串（"10000.000000000"），token 类是数字。
type summaryResp struct {
	Billing struct {
		EffectiveAvailablePoints float64 `json:"effective_available_points"`
	} `json:"billing"`
	DailyAllowance struct {
		QuotaPoints     string `json:"quota_points"`
		UsedPoints      string `json:"used_points"`
		RemainingPoints string `json:"remaining_points"`
		PeriodEndAt     string `json:"period_end_at"`
	} `json:"daily_allowance"`
	GiftCredits struct {
		RemainingPoints string `json:"remaining_points"`
	} `json:"gift_credits"`
	CodingPlan struct {
		EffectiveRemainingPoints string `json:"effective_remaining_points"`
		NextBoundaryAt           string `json:"next_boundary_at"`
		Windows                  []struct {
			WindowType      string `json:"window_type"`
			QuotaPoints     string `json:"quota_points"`
			UsedPoints      string `json:"used_points"`
			RemainingPoints string `json:"remaining_points"`
			Exhausted       bool   `json:"exhausted"`
			Period          struct {
				EndAt string `json:"end_at"`
			} `json:"period"`
		} `json:"windows"`
	} `json:"coding_plan"`
	Period struct {
		StartDate string `json:"start_date"`
		EndDate   string `json:"end_date"`
	} `json:"period"`
	Totals struct {
		RecordedPoints   float64 `json:"recorded_points"`
		SettlementCount  int64   `json:"settlement_count"`
		PromptTokens     int64   `json:"prompt_tokens"`
		CompletionTokens int64   `json:"completion_tokens"`
		TotalTokens      int64   `json:"total_tokens"`
		BySource         []struct {
			Label           string  `json:"label"`
			RecordedPoints  float64 `json:"recorded_points"`
			SettlementCount int64   `json:"settlement_count"`
			TotalTokens     int64   `json:"total_tokens"`
		} `json:"by_source"`
	} `json:"totals"`
}
