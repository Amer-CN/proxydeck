// quota_test.go —— 积分按账号查询单测：缓存按 Server 实例 + uid 分键（CN/INTL
// 两实例、区内多号互不串）/ PathOf 完整 uid 精确匹配（前 8 位碰撞不算命中）/
// NewestPath 池内第一号（mtime 最新）/ handleQuota 的 ?uid= 接参与默认号选择。
// auth 文件一律 t.TempDir() 造假（CODEBUDDY_AUTH_DIR 不落真实目录），上游用
// httptest mock 按 X-User-Id 区分账号，不碰真实 auth 目录、不占真实端口。
package codebuddy

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// quotaMockUpstream 官方 get-user-resource mock：按请求头 X-User-Id 区分账号，
// remain[uid] 为该号唯一资源包的 CycleCapacityRemain（总量即该号的可辨识指纹，
// 两个号给不同数值即可断言"没串号"）。
func quotaMockUpstream(t *testing.T, remain map[string]float64, hits *atomic.Int32) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if hits != nil {
			hits.Add(1)
		}
		uid := r.Header.Get("X-User-Id")
		rem, ok := remain[uid]
		if !ok {
			w.WriteHeader(http.StatusForbidden)
			io.WriteString(w, `{"code":40300,"msg":"unknown uid"}`)
			return
		}
		body, _ := json.Marshal(map[string]any{
			"code": 0,
			"data": map[string]any{"Response": map[string]any{"Data": map[string]any{
				"Accounts": []map[string]any{{
					"PackageName": "P-" + uid, "CycleStartTime": "2026-01-01 00:00:00",
					"CycleEndTime": "2099-01-01 00:00:00", "CapacitySize": 1000,
					"CapacityUsed": 10, "CapacityRemain": rem,
					"CycleCapacityRemain": rem, "CycleCapacitySize": 1000,
				}},
			}}},
		})
		_, _ = w.Write(body)
	}))
}

// overlayBaseURL 临时把区域 BaseURL 指向 mock（t.Cleanup 恢复，不影响其他用例）。
func overlayBaseURL(t *testing.T, r Region, url string) {
	t.Helper()
	old := regionConfigs[r]
	ov := old
	ov.BaseURL = url
	regionConfigs[r] = ov
	t.Cleanup(func() { regionConfigs[r] = old })
}

// 缓存分键：同一 Server 内两个 uid 互不串（B 查不得返回 A 的缓存）；CN/INTL
// 两个 Server 实例（模拟双区）互不串（INTL 首查不得吃到 CN 写的缓存——修串区）；
// 默认号（空 uid）走 NewestPath 且用自己的固定缓存键；未知 uid 如实 source:none。
func TestQuotaCachePerServerPerUID(t *testing.T) {
	dir := t.TempDir()
	base := time.Now()
	pa := writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "A", base)
	pb := writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "B", base.Add(-time.Minute))
	pi := writeAuthInfo(t, dir, "i.info", "www.workbuddy.ai", "uid-i", "I", base)

	up := quotaMockUpstream(t, map[string]float64{"uid-a": 100, "uid-b": 200, "uid-i": 500}, nil)
	defer up.Close()
	overlayBaseURL(t, RegionCN, up.URL)
	overlayBaseURL(t, RegionINTL, up.URL)

	// 双 Server 模拟 CN/INTL 两实例（各自 cred/pool，各自 quotaCache map）
	sCN := &Server{cred: NewCredential(pa, RegionCN), pool: &AccountPool{accounts: []*Account{
		{UID: "uid-a", Nickname: "A", AuthPath: pa},
		{UID: "uid-b", Nickname: "B", AuthPath: pb},
	}}, region: RegionCN, client: up.Client(), stats: map[string]*modelStat{}}
	sINTL := &Server{cred: NewCredential(pi, RegionINTL), pool: &AccountPool{accounts: []*Account{
		{UID: "uid-i", Nickname: "I", AuthPath: pi},
	}}, region: RegionINTL, client: up.Client(), stats: map[string]*modelStat{}}

	ctx := context.Background()
	// A 首查 live，响应带账号标识
	qa := sCN.FetchQuota(ctx, "uid-a")
	if qa.Source != "live" || qa.Total != 100 || qa.UID != "uid-a" || qa.Nickname != "A" {
		t.Fatalf("uid-a 首查不符: %+v", qa)
	}
	// B 首查必须也是 live（不得吃 A 的缓存）——区内多号分键
	qb := sCN.FetchQuota(ctx, "uid-b")
	if qb.Source != "live" || qb.Total != 200 || qb.UID != "uid-b" || qb.Nickname != "B" {
		t.Fatalf("uid-b 首查不得串 A 的缓存: %+v", qb)
	}
	// 二查各命中自己的缓存
	if q := sCN.FetchQuota(ctx, "uid-a"); q.Source != "cache" || q.Total != 100 || q.UID != "uid-a" {
		t.Fatalf("uid-a 二查应命中自己的缓存: %+v", q)
	}
	if q := sCN.FetchQuota(ctx, "uid-b"); q.Source != "cache" || q.Total != 200 || q.UID != "uid-b" {
		t.Fatalf("uid-b 二查应命中自己的缓存: %+v", q)
	}
	// force 绕过缓存
	if q := sCN.FetchQuotaForce(ctx, "uid-a"); q.Source != "live" || q.Total != 100 {
		t.Fatalf("force 应实拉: %+v", q)
	}
	// 串区修复实证：CN 查过 uid-a 之后，INTL 实例首查必须是 live（不返回 CN 的缓存）
	qi := sINTL.FetchQuota(ctx, "uid-i")
	if qi.Source != "live" || qi.Total != 500 || qi.UID != "uid-i" || qi.Nickname != "I" {
		t.Fatalf("INTL 实例首查不得串 CN 的缓存: %+v", qi)
	}
	// 默认号：空 uid → 池内第一号（mtime 最新 = uid-a），且 "_default" 键独立于 "uid-a" 键
	qd := sCN.FetchQuota(ctx, "")
	if qd.Source != "live" || qd.Total != 100 || qd.UID != "uid-a" || qd.Nickname != "A" {
		t.Fatalf("默认号应选池内最新号 uid-a: %+v", qd)
	}
	if q := sCN.FetchQuota(ctx, ""); q.Source != "cache" || q.Total != 100 {
		t.Fatalf("默认号二查应命中自己的缓存: %+v", q)
	}
	// 未知 uid：source:none，err 只含 uidShort（不打完整 uid）
	bad := sCN.FetchQuota(ctx, "uid-notexist99")
	if bad.Source != "none" || !strings.Contains(bad.Err, "账号不在池中: uid-note") {
		t.Fatalf("未知 uid 应如实 source:none: %+v", bad)
	}
	if strings.Contains(bad.Err, "uid-notexist99") {
		t.Fatalf("err 不得含完整 uid: %s", bad.Err)
	}
}

// PathOf 完整 uid 精确匹配：前 8 位相同的不同号（同前缀不同尾）只命中目标；
// uidShort 等值不算命中；找不到 ok=false。
func TestPoolPathOfExactUID(t *testing.T) {
	p := &AccountPool{accounts: []*Account{
		{UID: "uid-1234567890ab", Nickname: "甲", AuthPath: "a.info"},
		{UID: "uid-1234567890cd", Nickname: "乙", AuthPath: "b.info"},
	}}
	path, nick, uid, ok := p.PathOf("uid-1234567890cd")
	if !ok || path != "b.info" || nick != "乙" || uid != "uid-1234567890cd" {
		t.Fatalf("完整 uid 应精确命中目标号: path=%s nick=%s uid=%s ok=%v", path, nick, uid, ok)
	}
	if path, _, _, ok := p.PathOf("uid-1234567890ab"); !ok || path != "a.info" {
		t.Fatalf("同前缀另一号应精确命中: %s %v", path, ok)
	}
	// uidShort 等值（前 8 位相同）不得命中
	if _, _, _, ok := p.PathOf("uid-1234"); ok {
		t.Fatal("uidShort 前缀不得命中（前缀陷阱）")
	}
	if _, _, _, ok := p.PathOf("uid-nope"); ok {
		t.Fatal("未知 uid 应 ok=false")
	}
}

// NewestPath：多号时返回 mtime 最新（建池新→旧排序的第一号）；空池 ok=false。
func TestPoolNewestPath(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	base := time.Now()
	writeAuthInfo(t, dir, "old.info", "www.codebuddy.cn", "uid-old", "旧", base.Add(-2*time.Hour))
	writeAuthInfo(t, dir, "mid.info", "www.codebuddy.cn", "uid-mid", "中", base.Add(-1*time.Hour))
	newest := writeAuthInfo(t, dir, "new.info", "www.codebuddy.cn", "uid-new", "新", base)

	p := NewAccountPool(RegionCN)
	if p == nil {
		t.Fatal("应建出池")
	}
	path, nick, uid, ok := p.NewestPath()
	if !ok || path != newest || nick != "新" || uid != "uid-new" {
		t.Fatalf("NewestPath 应返回 mtime 最新号: path=%s nick=%s uid=%s ok=%v", path, nick, uid, ok)
	}
	if _, _, _, ok := (&AccountPool{}).NewestPath(); ok {
		t.Fatal("空池 NewestPath 应 ok=false")
	}
}

// handleQuota：?uid= 选号查询（换号返回各自数据，不吃别的号的缓存）；不带 uid
// 走默认号（池内最新）；未知 uid 返回 source:none 且 err 只给 uidShort、不打上游；
// refresh=1 与 uid 组合可用（force 绕缓存）。
func TestHandleQuotaUIDParam(t *testing.T) {
	dir := t.TempDir()
	base := time.Now()
	pa := writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "A", base)
	pb := writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "B", base.Add(-time.Minute))

	var hits atomic.Int32
	up := quotaMockUpstream(t, map[string]float64{"uid-a": 100, "uid-b": 200}, &hits)
	defer up.Close()
	overlayBaseURL(t, RegionCN, up.URL)

	s := &Server{cred: NewCredential(pa, RegionCN), pool: &AccountPool{accounts: []*Account{
		{UID: "uid-a", Nickname: "A", AuthPath: pa},
		{UID: "uid-b", Nickname: "B", AuthPath: pb},
	}}, region: RegionCN, client: up.Client(), stats: map[string]*modelStat{}}

	doQuota := func(url string) Quota {
		t.Helper()
		rr := httptest.NewRecorder()
		s.handleQuota(rr, httptest.NewRequest(http.MethodGet, url, nil))
		var q Quota
		if err := json.Unmarshal(rr.Body.Bytes(), &q); err != nil {
			t.Fatalf("%s 响应解析失败: %v", url, err)
		}
		return q
	}

	// 默认号路径：不带 uid → 池内第一号（uid-a，mtime 最新）
	if q := doQuota("/quota"); q.Source != "live" || q.UID != "uid-a" || q.Total != 100 || q.Nickname != "A" {
		t.Fatalf("默认号应选池内最新号: %+v", q)
	}
	// 指定号：返回该号自己的数据（不吃默认号的缓存）
	if q := doQuota("/quota?uid=uid-b"); q.Source != "live" || q.UID != "uid-b" || q.Total != 200 || q.Nickname != "B" {
		t.Fatalf("?uid=uid-b 应返回 b 号数据: %+v", q)
	}
	if n := hits.Load(); n != 2 {
		t.Fatalf("两个号首查应各打一次上游（分键不串），got %d", n)
	}
	// 同号二查命中后端缓存（上游零新请求）
	if q := doQuota("/quota?uid=uid-b"); q.Source != "cache" || q.Total != 200 {
		t.Fatalf("同号二查应命中缓存: %+v", q)
	}
	if n := hits.Load(); n != 2 {
		t.Fatalf("命中缓存不应再打上游，got %d", n)
	}
	// 未知 uid：source:none，err 只含 uidShort；且不打上游
	bad := doQuota("/quota?uid=uid-zzzzzzzz123")
	if bad.Source != "none" || !strings.Contains(bad.Err, "账号不在池中: uid-zzzz") {
		t.Fatalf("未知 uid 应 source:none: %+v", bad)
	}
	if strings.Contains(bad.Err, "uid-zzzzzzzz123") {
		t.Fatalf("err 不得含完整 uid: %s", bad.Err)
	}
	if n := hits.Load(); n != 2 {
		t.Fatalf("账号不在池中不应打上游，got %d", n)
	}
	// refresh=1 与 uid 组合：force 绕缓存再打一次上游
	if q := doQuota("/quota?uid=uid-b&refresh=1"); q.Source != "live" || q.Total != 200 {
		t.Fatalf("refresh=1+uid 应 force 实拉: %+v", q)
	}
	if n := hits.Load(); n != 3 {
		t.Fatalf("force 应再打一次上游，got %d", n)
	}
}
