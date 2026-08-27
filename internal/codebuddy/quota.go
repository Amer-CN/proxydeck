// quota.go —— WorkBuddy 积分（官方 get-user-resource 口径）查询。
//
// 数据源：桌面端源码逆向出的官方积分接口
// POST https://copilot.tencent.com/v2/billing/meter/get-user-resource
// （认证与 chat 转发同一凭证：Authorization Bearer accessToken + X-User-Id）。
// 每条 Account 是一个"资源包"，有自己的周期（CycleStartTime/CycleEndTime）
// 和容量；口径按实测数据整理，见 buildQuota 注释。
package codebuddy

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"sort"
	"sync"
	"time"
)

const (
	quotaURL     = backendBase + "/v2/billing/meter/get-user-resource"
	quotaTimeout = 15 * time.Second
)

// Quota 是积分快照（GUI 链路监测积分卡用，官方口径）。
type Quota struct {
	Total      float64     `json:"total"`      // 总剩余 = 所有未过期包剩余合计
	Packs      int         `json:"packs"`      // 未过期包数
	Active     ActivePack  `json:"active"`     // 当前消耗中的包（used/size）
	NearestEnd NearestPack `json:"nearestEnd"` // 最近过期提醒
	Source     string      `json:"source"`     // live / cache / none
	Err        string      `json:"err,omitempty"`
}

// ActivePack 当前消耗中的包（CycleEndTime 最近且 CapacityUsed>0）。
type ActivePack struct {
	Name string  `json:"name"`
	Used float64 `json:"used"`
	Size float64 `json:"size"`
	End  string  `json:"end"` // 周期结束时间 "2026-08-29 23:59:59"
}

// NearestPack 未过期包里到期最早的包。
type NearestPack struct {
	Name   string  `json:"name"`
	Date   string  `json:"date"` // 周期结束时间
	Remain float64 `json:"remain"`
}

// account 对应响应里的一条资源包（容量字段用 float64 宽松解析）。
type account struct {
	PackageName         string  `json:"PackageName"`
	CycleStartTime      string  `json:"CycleStartTime"`
	CycleEndTime        string  `json:"CycleEndTime"`
	CapacitySize        float64 `json:"CapacitySize"`
	CapacityUsed        float64 `json:"CapacityUsed"`
	CapacityRemain      float64 `json:"CapacityRemain"`
	CycleCapacityRemain float64 `json:"CycleCapacityRemain"`
	CycleCapacitySize   float64 `json:"CycleCapacitySize"`
}

// quotaCache 包级缓存（5 分钟 TTL，模式同 tuanjie/quota.go，独立实现）。
type quotaCache struct {
	mu sync.Mutex
	q  *Quota
	at time.Time
}

var qc quotaCache

const quotaTTL = 5 * time.Minute

// FetchQuota 拉取官方积分快照（带 5 分钟缓存）。
func (s *Server) FetchQuota(ctx context.Context) *Quota {
	qc.mu.Lock()
	if qc.q != nil && time.Since(qc.at) < quotaTTL {
		q := *qc.q
		q.Source = "cache"
		qc.mu.Unlock()
		return &q
	}
	qc.mu.Unlock()

	q, err := s.fetchQuotaLive(ctx)
	if err != nil {
		qc.mu.Lock()
		cached := qc.q
		qc.mu.Unlock()
		if cached != nil {
			cp := *cached
			cp.Source = "cache"
			cp.Err = err.Error()
			return &cp
		}
		return &Quota{Source: "none", Err: err.Error()}
	}
	qc.mu.Lock()
	qc.q = q
	qc.at = time.Now()
	qc.mu.Unlock()
	return q
}

// FetchQuotaForce 强制实时拉取（手动刷新按钮用），绕过缓存并回写。
// 失败时回退缓存，与 FetchQuota 同口径。
func (s *Server) FetchQuotaForce(ctx context.Context) *Quota {
	q, err := s.fetchQuotaLive(ctx)
	if err != nil {
		qc.mu.Lock()
		cached := qc.q
		qc.mu.Unlock()
		if cached != nil {
			cp := *cached
			cp.Source = "cache"
			cp.Err = err.Error()
			return &cp
		}
		return &Quota{Source: "none", Err: err.Error()}
	}
	qc.mu.Lock()
	qc.q = q
	qc.at = time.Now()
	qc.mu.Unlock()
	return q
}

func (s *Server) fetchQuotaLive(ctx context.Context) (*Quota, error) {
	hdr, err := s.cred.Headers(ctx)
	if err != nil {
		return nil, err
	}
	// 固定模板：时间区间 2024-12-01 到现在
	body := map[string]any{
		"PageNumber":                  1,
		"PageSize":                    100,
		"ProductCode":                 "p_tcaca",
		"Status":                      []int{0, 3},
		"PackageStartTimeRangeBegin":  "2024-12-01 21:25:00",
		"PackageStartTimeRangeEnd":    time.Now().Format("2006-01-02 15:04:05"),
	}
	ctx2, cancel := context.WithTimeout(ctx, quotaTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx2, http.MethodPost, quotaURL, mustJSONReader(body))
	if err != nil {
		return nil, err
	}
	req.Header = cloneHeader(hdr)
	resp, err := s.client.Do(req)
	if err != nil {
		return nil, fmt.Errorf("官方积分接口请求失败: %w", err)
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(resp.Body, 2<<20))
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("官方积分接口 HTTP %d: %s", resp.StatusCode, truncate(string(b), 200))
	}
	var out struct {
		Code int    `json:"code"`
		Msg  string `json:"msg"`
		Data struct {
			Response struct {
				Data struct {
					Accounts []account `json:"Accounts"`
				} `json:"Data"`
			} `json:"Response"`
		} `json:"data"`
	}
	if err := json.Unmarshal(b, &out); err != nil {
		return nil, fmt.Errorf("官方积分接口响应解析失败: %w", err)
	}
	if out.Code != 0 {
		return nil, fmt.Errorf("官方积分接口 code=%d: %s", out.Code, truncate(out.Msg, 200))
	}
	return buildQuota(out.Data.Response.Data.Accounts), nil
}

// buildQuota 按实测口径汇总：
//   - 包的剩余 = CycleCapacityRemain（周期剩余；为 0 时用 CapacityRemain）
//   - 总剩余 = 所有过期时间未到的包剩余合计
//   - 当前包 = 未过期包里 CycleEndTime 最近且 CapacityUsed>0 的（used/size）
//   - 最近过期 = 未过期包里 CycleEndTime 最早的（名称+日期+剩余）
func buildQuota(accounts []account) *Quota {
	now := time.Now()
	type pack struct {
		acct account
		rem  float64
		endT time.Time
	}
	var unexpired []pack
	for _, a := range accounts {
		if a.CycleEndTime == "" {
			continue
		}
		endT, err := time.ParseInLocation("2006-01-02 15:04:05", a.CycleEndTime, time.Local)
		if err != nil || !now.Before(endT) {
			continue
		}
		// 官方口径为 CycleCapacityRemain（周期剩余）直接合计——实测体验版包
		// CycleCapacityRemain=0 时 CapacityRemain=500（总授予量未扣），两者
		// 语义不同，不能 fallback：官方 APP 显示 1569.69 = 周期剩余合计，
		// 加了这 500 会虚高成 2069（用户对账确认）。
		rem := a.CycleCapacityRemain
		unexpired = append(unexpired, pack{a, rem, endT})
	}
	q := &Quota{Source: "live"}
	for _, p := range unexpired {
		q.Total += p.rem
	}
	q.Packs = len(unexpired)
	// 按到期时间升序，第一个即最近过期；当前包取已用>0 里的第一个
	sort.Slice(unexpired, func(i, j int) bool { return unexpired[i].endT.Before(unexpired[j].endT) })
	if len(unexpired) > 0 {
		first := unexpired[0]
		q.NearestEnd = NearestPack{
			Name:   first.acct.PackageName,
			Date:   first.acct.CycleEndTime,
			Remain: first.rem,
		}
	}
	for _, p := range unexpired {
		// 跳过已用满的包（used>=size 无剩余额度）——否则"当前消耗包"显示
		// 100/100 已满，与总剩余（其他包还有额度）矛盾，用户看着像积分识别错误
		if p.acct.CapacityUsed <= 0 || p.acct.CapacityUsed >= p.acct.CapacitySize {
			continue
		}
		q.Active = ActivePack{
			Name: p.acct.PackageName,
			Used: p.acct.CapacityUsed,
			Size: p.acct.CapacitySize,
			End:  p.acct.CycleEndTime,
		}
		break
	}
	return q
}
