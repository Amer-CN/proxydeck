package proxy

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// usageSummary aggregates the four upstream usage endpoints into one response.
type usageSummary struct {
	Whoami     json.RawMessage `json:"whoami,omitempty"`
	Credits    json.RawMessage `json:"credits,omitempty"`
	Sub        json.RawMessage `json:"subscription,omitempty"`
	Summary    json.RawMessage `json:"summary,omitempty"`
	Errors     []string        `json:"errors,omitempty"`
	PlanLabel  string          `json:"planLabel,omitempty"`
	PlanID     string          `json:"planId,omitempty"`
	CostUSD    float64         `json:"costUSD"`
	CreditsAvl float64         `json:"creditsAvailable"`
	CreditsTot float64         `json:"creditsTotal"`
	Percent    float64         `json:"percentUsed"`
	DaysLeft   int             `json:"daysLeft"`
	UsageURL   string          `json:"usageUrl,omitempty"`
	// List of recent usage records (from /internal/usage)
	Items       json.RawMessage `json:"items,omitempty"`
	ListCursor  string          `json:"listCursor,omitempty"`
	TotalTokens int64           `json:"totalTokens"`
	TotalRuns   int64           `json:"totalRuns"`
	TokensIn    int64           `json:"tokensIn"`
	TokensOut   int64           `json:"tokensOut"`
	// 官网明细按模型聚合（items 解析，字段缺失则空 map）
	ModelBreakdown map[string]itemTokens `json:"modelBreakdown,omitempty"`
	// 今日（本地时区）总 token（items 聚合）
	TodayTokens int64 `json:"todayTokens"`
	// 用量窗口限额（credits.windowLimits）：5 小时 / 滚动周期（月度）
	FiveHour *winLimit `json:"fiveHour,omitempty"`
	Weekly   *winLimit `json:"weekly,omitempty"`
}

// winLimit 一个用量窗口的已用/上限/重置时间（USD）。
type winLimit struct {
	Used    float64 `json:"used"`
	Cap     float64 `json:"cap"`
	ResetAt int64   `json:"resetAt"` // epoch 毫秒
}

// handleUsage queries CommandCode's usage endpoints (same calls the CLI /usage
// command makes) and returns a normalized summary. Requires a valid key.
//
// 成功结果缓存 usageTTL：GUI 每 8s 轮询一次，若每次都同步拉官网（4 个串行
// 接口 ~2-3s）会阻塞 webview 消息循环造成界面卡顿；缓存后轮询即时返回。
const usageTTL = 20 * time.Second

func (p *Proxy) HandleUsage(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		p.writeOpenAIError(w, http.StatusMethodNotAllowed, "Method not allowed", "invalid_request_error")
		return
	}

	// 缓存命中：直接返回上次成功结果（官网数字更新慢，20s 延迟可接受）
	p.usageMu.Lock()
	if p.usageData != nil && time.Since(p.usageAt) < usageTTL {
		body := p.usageData
		p.usageMu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write(body)
		return
	}
	p.usageMu.Unlock()

	apiKey := r.Header.Get("Authorization")
	if apiKey == "" {
		apiKey = p.APIKey
	}
	apiKey = trimBearer(apiKey)
	if apiKey == "" {
		p.writeOpenAIError(w, http.StatusUnauthorized, "API key required. Set Authorization header.", "authentication_error")
		return
	}

	get := func(endpoint string, params map[string]string) ([]byte, string, error) {
		u := p.BaseURL + endpoint
		if len(params) > 0 {
			q := url.Values{}
			for k, v := range params {
				q.Set(k, v)
			}
			u += "?" + q.Encode()
		}
		req, err := http.NewRequestWithContext(r.Context(), http.MethodGet, u, nil)
		if err != nil {
			return nil, "", err
		}
		req.Header.Set("Authorization", "Bearer "+apiKey)
		req.Header.Set("Content-Type", "application/json")
		resp, err := p.Client.Do(req)
		if err != nil {
			return nil, "", err
		}
		defer resp.Body.Close()
		body, err := io.ReadAll(resp.Body)
		if err != nil {
			return nil, "", err
		}
		if resp.StatusCode != http.StatusOK {
			return nil, "", fmt.Errorf("%s returned %d: %s", endpoint, resp.StatusCode, truncateLog(string(body)))
		}
		return body, "", nil
	}

	var sum usageSummary
	// 1. whoami -> org id（个人账号 org 为 null，其余 /alpha/ 接口无参直调）
	whoamiRaw, _, err := get("/alpha/whoami", nil)
	if err != nil {
		sum.Errors = append(sum.Errors, err.Error())
		sum.UsageURL = "https://commandcode.ai/usage"
		p.writeJSON(w, sum)
		return
	}
	sum.Whoami = whoamiRaw
	var whoami struct {
		Org *struct {
			ID    string `json:"id"`
			Login string `json:"login"`
		} `json:"org"`
		User struct {
			UserName string `json:"userName"`
		} `json:"user"`
	}
	_ = json.Unmarshal(whoamiRaw, &whoami)
	orgID := ""
	if whoami.Org != nil {
		orgID = whoami.Org.ID
	}

	// 2-4. credits / subscription / summary 互相独立，并行拉取。
	// params：有 org 的组织账号传 orgId，个人账号不传（直调）。
	params := func() map[string]string {
		if orgID != "" {
			return map[string]string{"orgId": orgID}
		}
		return nil
	}
	type upRes struct {
		tag string
		raw []byte
		err error
	}
	ch := make(chan upRes, 3)
	fetch := func(tag, endpoint string) {
		raw, _, err := get(endpoint, params())
		ch <- upRes{tag, raw, err}
	}
	go fetch("credits", "/alpha/billing/credits")
	go fetch("sub", "/alpha/billing/subscriptions")
	go fetch("summary", "/alpha/usage/summary")
	for i := 0; i < 3; i++ {
		r := <-ch
		if r.err != nil {
			continue
		}
		switch r.tag {
		case "credits":
			sum.Credits = r.raw
		case "sub":
			sum.Sub = r.raw
		default:
			sum.Summary = r.raw
		}
	}
	// 计划信息（planId / 周期结束）
	var sub struct {
		Data struct {
			PlanID             string `json:"planId"`
			Status             string `json:"status"`
			CurrentPeriodStart string `json:"currentPeriodStart"`
			CurrentPeriodEnd   string `json:"currentPeriodEnd"`
		} `json:"data"`
	}
	if sum.Sub != nil {
		_ = json.Unmarshal(sum.Sub, &sub)
	}
	// summary：账单周期权威汇总（个人账号无参直调同样可用）
	var summary struct {
		TotalCost      float64 `json:"totalCost"`
		TotalCount     int64   `json:"totalCount"`
		TotalTokens    int64   `json:"totalTokens"`
		TotalTokensIn  int64   `json:"totalTokensIn"`
		TotalTokensOut int64   `json:"totalTokensOut"`
	}
	if sum.Summary != nil {
		_ = json.Unmarshal(sum.Summary, &summary)
	}
	// credits 汇总
	var credits struct {
		Credits struct {
			MonthlyCredits   float64 `json:"monthlyCredits"`
			PurchasedCredits float64 `json:"purchasedCredits"`
			FreeCredits      float64 `json:"freeCredits"`
		} `json:"credits"`
		WindowLimits *struct {
			Limited bool `json:"limited"`
			FiveHour *struct {
				Used    float64 `json:"used"`
				Cap     float64 `json:"cap"`
				ResetAt int64   `json:"resetAt"`
			} `json:"fiveHour"`
			Weekly *struct {
				Used    float64 `json:"used"`
				Cap     float64 `json:"cap"`
				ResetAt int64   `json:"resetAt"`
			} `json:"weekly"`
		} `json:"windowLimits"`
	}
	if sum.Credits != nil {
		_ = json.Unmarshal(sum.Credits, &credits)
	}
	// 用量窗口：5 小时 / 滚动周期（月度），暴露给前端显示限额进度
	if w := credits.WindowLimits; w != nil {
		if w.FiveHour != nil {
			sum.FiveHour = &winLimit{Used: w.FiveHour.Used, Cap: w.FiveHour.Cap, ResetAt: w.FiveHour.ResetAt}
		}
		if w.Weekly != nil {
			sum.Weekly = &winLimit{Used: w.Weekly.Used, Cap: w.Weekly.Cap, ResetAt: w.Weekly.ResetAt}
		}
	}
	sum.PlanID = sub.Data.PlanID
	sum.PlanLabel = planLabel(sub.Data.PlanID)
	sum.CreditsAvl = credits.Credits.MonthlyCredits + credits.Credits.PurchasedCredits + credits.Credits.FreeCredits
	sum.CreditsTot = sum.CreditsAvl + summary.TotalCost
	if sum.CreditsTot > 0 {
		sum.Percent = summary.TotalCost / sum.CreditsTot * 100
		if sum.Percent > 100 {
			sum.Percent = 100
		}
	}
	if t, err := time.Parse(time.RFC3339, sub.Data.CurrentPeriodEnd); err == nil {
		sum.DaysLeft = int(time.Until(t).Hours()/24) + 1
		if sum.DaysLeft < 0 {
			sum.DaysLeft = 0
		}
	}
	sum.UsageURL = "https://commandcode.ai/usage"

	// 官方权威 token 汇总（/alpha/usage/summary，个人账号无需 orgId）
	sum.CostUSD = summary.TotalCost
	sum.TotalTokens = summary.TotalTokens
	sum.TotalRuns = summary.TotalCount
	sum.TokensIn = summary.TotalTokensIn
	sum.TokensOut = summary.TotalTokensOut

	// 5. 明细列表（/internal/usage，网页 cookie 会话接口）：
	// 仅组织账号可拉；失败不影响上面的官网汇总（不追加 errors）。
	if orgID != "" {
		limit := r.URL.Query().Get("limit")
		if limit == "" {
			limit = "500"
		}
		listParams := map[string]string{"limit": limit, "orgId": orgID}
		if c := r.URL.Query().Get("cursor"); c != "" {
			listParams["cursor"] = c
		}
		if listRaw, _, err := get("/internal/usage", listParams); err == nil {
			var list struct {
				Items  json.RawMessage `json:"items"`
				Cursor string          `json:"cursor"`
			}
			if uerr := json.Unmarshal(listRaw, &list); uerr == nil {
				sum.Items = list.Items
				sum.ListCursor = list.Cursor
				// 结构化 items：按模型聚合 + 今日聚合（防御式解析，字段缺失即忽略）
				sum.ModelBreakdown = modelBreakdown(list.Items)
				sum.TodayTokens = todayTokens(list.Items)
			}
		}
	}

	p.writeJSON(w, sum)
	if len(sum.Errors) == 0 && sum.TotalTokens > 0 {
		if b, err := json.Marshal(sum); err == nil {
			p.usageMu.Lock()
			p.usageData = b
			p.usageAt = time.Now()
			p.usageMu.Unlock()
		}
	}
}

// itemTokens 从一条 usage 明细里提取 token 数（兼容多种字段名）。
type itemTokens struct {
	model     string
	in        int64
	out       int64
	cacheRead int64
	cost      float64
	day       string // YYYY-MM-DD（本地时区），无时间字段则为空
}

// modelBreakdown 按模型聚合 items 的 token 消耗（官网口径，含缓存）。
func modelBreakdown(itemsRaw json.RawMessage) map[string]itemTokens {
	out := map[string]itemTokens{}
	var items []map[string]json.RawMessage
	if err := json.Unmarshal(itemsRaw, &items); err != nil {
		return out
	}
	for _, it := range items {
		var m string
		if v, ok := it["model"]; ok {
			_ = json.Unmarshal(v, &m)
		}
		if m == "" {
			m = "unknown"
		}
		itm := out[m]
		itm.model = m
		for _, f := range []struct{ dst *int64; keys []string }{
			{&itm.in, []string{"inputTokens", "input_tokens", "tokensIn"}},
			{&itm.out, []string{"outputTokens", "output_tokens", "tokensOut"}},
			{&itm.cacheRead, []string{"cacheReadTokens", "cache_read_tokens"}},
		} {
			for _, k := range f.keys {
				if v, ok := it[k]; ok {
					var n int64
					if json.Unmarshal(v, &n) == nil {
						*f.dst += n
						break
					}
				}
			}
		}
		if v, ok := it["cost"]; ok {
			var c float64
			if json.Unmarshal(v, &c) == nil {
				itm.cost += c
			}
		} else if v, ok := it["amount"]; ok {
			var c float64
			if json.Unmarshal(v, &c) == nil {
				itm.cost += c
			}
		}
		// 时间字段（createdAt / created_at / time / timestamp）
		for _, k := range []string{"createdAt", "created_at", "time", "timestamp"} {
			if v, ok := it[k]; ok {
				var ts string
				if json.Unmarshal(v, &ts) == nil && ts != "" {
					itm.day = tsToDay(ts)
					break
				}
			}
		}
		out[m] = itm
	}
	return out
}

// todayTokens 汇总今天（本地时区）所有模型的 token。
func todayTokens(itemsRaw json.RawMessage) int64 {
	var total int64
	for _, mb := range modelBreakdown(itemsRaw) {
		if mb.day == time.Now().Format("2006-01-02") {
			total += mb.in + mb.out + mb.cacheRead
		}
	}
	return total
}

// tsToDay 把 RFC3339 时间戳转为本地日期 YYYY-MM-DD；解析失败返回空串。
func tsToDay(ts string) string {
	if t, err := time.Parse(time.RFC3339, ts); err == nil {
		return t.Local().Format("2006-01-02")
	}
	return ""
}

func (p *Proxy) writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

// trimBearer 去掉可选的 "Bearer " 前缀。注意：不能对 key 做大小写规范化
// （http.CanonicalHeaderKey 会把 user_ 前缀大写成 User_，破坏 key）。
func trimBearer(key string) string {
	key = strings.TrimSpace(key)
	if len(key) > 7 && strings.EqualFold(key[:7], "Bearer ") {
		key = strings.TrimSpace(key[7:])
	}
	return key
}

// planLabel maps a plan id to a human-readable label. Unknown ids pass through.
func planLabel(planID string) string {
	labels := map[string]string{
		"go":       "Go",
		"goat":     "GOAT",
		"pro":      "Pro",
		"max_10x":  "Max 10x",
		"max_20x":  "Max 20x",
		"team_pro": "Team Pro",
	}
	if l, ok := labels[planID]; ok {
		return l
	}
	if planID == "" {
		return "Unknown plan"
	}
	return planID
}
