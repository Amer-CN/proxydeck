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
	"log"
	"net/http"
	"sort"
	"time"
)

const (
	quotaPath    = "/v2/billing/meter/get-user-resource" // 挂在区域 base URL 下（CN copilot.tencent.com；INTL 未实测，失败如实报错）
	quotaTimeout = 15 * time.Second
)

// Quota 是积分快照（GUI 链路监测积分卡用，官方口径）。
type Quota struct {
	Total      float64     `json:"total"`      // 总剩余 = 所有未过期包剩余合计
	Packs      int         `json:"packs"`      // 未过期包数
	Active     ActivePack  `json:"active"`     // 当前消耗中的包（used/size）
	NearestEnd NearestPack `json:"nearestEnd"` // 最近过期提醒
	UID        string      `json:"uid"`        // 本次快照属于哪个账号（完整 uid；GUI 副行显示"谁的积分"）
	Nickname   string      `json:"nickname"`   // 账号昵称（默认号也填）
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

// quotaEntry 单账号的缓存条目（快照 + 写入时点）。
type quotaEntry struct {
	q  *Quota
	at time.Time
}

// quotaDefaultKey 默认号（不带 uid 的查询）的缓存键。
const quotaDefaultKey = "_default"

const quotaTTL = 5 * time.Minute

// quotaCacheKey uid → 缓存键（空 uid 用固定键；真实 uid 来自桌面端，
// 不会撞 "_default" 字面量）。
func quotaCacheKey(uid string) string {
	if uid == "" {
		return quotaDefaultKey
	}
	return uid
}

// quotaResolve uid → auth 文件路径 + 账号信息。uid 空 = 默认号：池内 mtime 最新
//（建池新→旧排序，第一号=当前最新登录，与旧行为"绑 mtime 最新文件"语义一致）；
// 非空按完整 uid 精确查（不碰 uidShort 前缀）。未知 uid ok=false——积分是按账号
// 看的，静默回退别的号比报错更糟。
func (s *Server) quotaResolve(uid string) (path, nickname, uidOut string, ok bool) {
	if s.pool == nil {
		return "", "", "", true // 零值 Server（无池）：沿用当前凭据文件（旧行为）
	}
	if uid == "" {
		return s.pool.NewestPath()
	}
	return s.pool.PathOf(uid)
}

// FetchQuota 拉取指定账号的官方积分快照（带 5 分钟缓存；uid 空 = 默认号）。
// 命中缓存返回 source:"cache"；失败回退该 uid 自己的缓存（绝不串号）。
func (s *Server) FetchQuota(ctx context.Context, uid string) *Quota {
	key := quotaCacheKey(uid)
	s.quotaMu.Lock()
	e := s.quotaCache[key]
	if e != nil && time.Since(e.at) < quotaTTL {
		q := *e.q
		q.Source = "cache"
		s.quotaMu.Unlock()
		return &q
	}
	s.quotaMu.Unlock()
	return s.fetchQuotaFresh(ctx, uid, key)
}

// FetchQuotaForce 强制实时拉取指定账号（手动刷新按钮用），绕过缓存并回写。
// 失败时回退该 uid 自己的缓存，与 FetchQuota 同口径。
func (s *Server) FetchQuotaForce(ctx context.Context, uid string) *Quota {
	return s.fetchQuotaFresh(ctx, uid, quotaCacheKey(uid))
}

// quotaHTTPError 官方积分接口的非 200 响应（typed error：上游状态码可判定，
// 不再从错误字符串里猜）。401/403 = 凭据失效——与聊天路径同一信号，调用方
// 据此 MarkDead（手动刷新积分 = 免费验活）。
type quotaHTTPError struct {
	status int
	body   string
}

func (e *quotaHTTPError) Error() string {
	return fmt.Sprintf("官方积分接口 HTTP %d: %s", e.status, truncate(e.body, 200))
}

// fetchQuotaFresh 实拉一次并回写缓存；失败回退该 key 自己的缓存。
// FetchQuota（TTL 过期后）与 FetchQuotaForce（绕过 TTL）共用同一套拉取/回退口径。
// 积分路径 401/403 标死（与矩阵观感一致）：quotaResolve 在本入口把查询 uid 解析
// 成实际选中的号（默认查询空 uid → 池内最新号，uidOut 即真实 uid），上游 401/403
// 时 MarkDead 落到它——聊天流量之外的免费验活通道。缓存键仍按调用方口径
//（默认号固定 "_default"），与既有按账号分键/回退语义互不影响。
func (s *Server) fetchQuotaFresh(ctx context.Context, uid, key string) *Quota {
	path, nick, uidOut, ok := s.quotaResolve(uid)
	if !ok {
		return &Quota{Source: "none", Err: "账号不在池中: " + uidShort(uid)}
	}
	q, err := s.fetchQuotaLive(ctx, path)
	if err != nil {
		if he, isHTTP := err.(*quotaHTTPError); isHTTP && s.pool != nil && uidOut != "" &&
			(he.status == http.StatusUnauthorized || he.status == http.StatusForbidden) {
			s.pool.MarkDead(uidOut)
			log.Printf("[codebuddy] 账号 %s 积分查询 HTTP %d，标记凭据失效（手动刷新积分=免费验活）",
				uidShort(uidOut), he.status)
		}
		s.quotaMu.Lock()
		e := s.quotaCache[key]
		s.quotaMu.Unlock()
		if e != nil {
			cp := *e.q
			cp.Source = "cache"
			cp.Err = err.Error()
			return &cp
		}
		return &Quota{Source: "none", Err: err.Error()}
	}
	q.UID, q.Nickname = uidOut, nick
	s.quotaMu.Lock()
	if s.quotaCache == nil {
		s.quotaCache = map[string]*quotaEntry{}
	}
	s.quotaCache[key] = &quotaEntry{q: q, at: time.Now()}
	s.quotaMu.Unlock()
	return q
}

func (s *Server) fetchQuotaLive(ctx context.Context, path string) (*Quota, error) {
	hdr, err := s.cred.HeadersFor(ctx, path)
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
	req, err := http.NewRequestWithContext(ctx2, http.MethodPost,
		s.cfg().BaseURL+quotaPath, mustJSONReader(body))
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
		return nil, &quotaHTTPError{resp.StatusCode, string(b)}
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
