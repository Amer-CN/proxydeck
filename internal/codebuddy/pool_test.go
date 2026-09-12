// pool_test.go —— 账号池单测：建池去重（同 uid 留 mtime 最新）/ 坏文件跳过 /
// Pick 轮换跳冷却 / 全池冷却返 nil / parseQuotaReset 提 6004 英文报文时点。
// auth 目录一律用 t.TempDir() 造假，经 CODEBUDDY_AUTH_DIR 指给 findAuthDirs，
// 绝不读写真实 auth 目录。
package codebuddy

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// writeAuthInfo 在 dir 写一个假 auth .info 文件并指定 mtime（建池按 mtime
// 去重/排序），返回路径。结构对齐桌面端真实文件（auth.domain / account.uid）。
func writeAuthInfo(t *testing.T, dir, name, domain, uid, nick string, mtime time.Time) string {
	t.Helper()
	b, err := json.Marshal(map[string]any{
		"auth":    map[string]any{"accessToken": "tok-" + uid, "domain": domain, "expiresAt": 4102416000000},
		"account": map[string]any{"uid": uid, "nickname": nick, "enterpriseId": "ent"},
	})
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, name)
	if err := os.WriteFile(path, b, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(path, mtime, mtime); err != nil {
		t.Fatal(err)
	}
	return path
}

// 建池：同 uid 去重留 mtime 最新、坏 JSON 文件跳过不整池报错、
// 异区域 domain 不入池；池内 mtime 新的在前（第一个 Pick = 最新号）。
func TestPoolScanDedupe(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	base := time.Now()

	// 同 uid 两个文件（模拟国际版 5 个 amer-cn 旧文件 + 新文件）：只留最新的
	writeAuthInfo(t, dir, "amer-old.info", "www.workbuddy.ai", "uid-amer", "amer-cn", base.Add(-48*time.Hour))
	newFile := writeAuthInfo(t, dir, "amer-new.info", "www.workbuddy.ai", "uid-amer", "amer-cn", base)
	writeAuthInfo(t, dir, "swch.info", "www.workbuddy.ai", "uid-swch", "swch-cn", base.Add(-1*time.Hour))
	// 坏文件（真实目录实测存在 PARSE-FAIL 半截文件）：跳过，不报错
	if err := os.WriteFile(filepath.Join(dir, "broken.info"), []byte(`{"auth":`), 0o644); err != nil {
		t.Fatal(err)
	}
	// 异区域文件：国际版建池不得入池
	writeAuthInfo(t, dir, "cn.info", "www.codebuddy.cn", "uid-cn", "cn-user", base.Add(-2*time.Hour))

	p := NewAccountPool(RegionINTL)
	if p == nil {
		t.Fatal("应建出国际区池")
	}
	if p.Size() != 2 {
		t.Fatalf("同 uid 去重后应剩 2 个号，got %d", p.Size())
	}
	seen := map[string]string{}
	for i := 0; i < p.Size(); i++ {
		a := p.Pick()
		if a == nil {
			t.Fatal("有可用号时 Pick 不应返回 nil")
		}
		seen[a.UID] = a.AuthPath
	}
	if seen["uid-amer"] != newFile {
		t.Fatalf("同 uid 应留 mtime 最新的文件: got %s want %s", seen["uid-amer"], newFile)
	}
	if _, ok := seen["uid-cn"]; ok {
		t.Fatal("异区域文件不应入池")
	}
	// 第一个 Pick 应是 mtime 最新的号（与旧行为"绑定最新文件"起点一致）
	p2 := NewAccountPool(RegionINTL)
	if a := p2.Pick(); a.UID != "uid-amer" {
		t.Fatalf("第一个 Pick 应是 mtime 最新的号: got %s", a.UID)
	}
	// 同一目录建 CN 池：只收 codebuddy.cn 的号
	pc := NewAccountPool(RegionCN)
	if pc == nil || pc.Size() != 1 {
		t.Fatalf("CN 池应只含 1 个国内号，got %v", pc)
	}
	if a := pc.Pick(); a.UID != "uid-cn" {
		t.Fatalf("CN 池成员不符: got %s", a.UID)
	}
}

// 坏文件独占目录：全部不可解析 → 建池返回 nil（调用方走现有"未找到登录文件"报错）。
func TestPoolScanAllBadReturnsNil(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	if err := os.WriteFile(filepath.Join(dir, "broken.info"), []byte("not json at all"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "nouid.info"),
		[]byte(`{"auth":{"domain":"www.workbuddy.ai"},"account":{"uid":""}}`), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "readme.txt"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	if p := NewAccountPool(RegionINTL); p != nil {
		t.Fatalf("全部坏文件应建池失败返回 nil，got %d 个号", p.Size())
	}
}

// Pick 轮换：跳过冷却中的号、到期即自动可选（无后台循环）、全池冷却返 nil。
func TestPoolPickRoundRobinAndCooldown(t *testing.T) {
	p := &AccountPool{accounts: []*Account{
		{UID: "uid-a", Nickname: "A", AuthPath: "a.info"},
		{UID: "uid-b", Nickname: "B", AuthPath: "b.info"},
	}}
	// 轮询推进
	if a := p.Pick(); a.UID != "uid-a" {
		t.Fatalf("第 1 次 Pick 应选 a: got %s", a.UID)
	}
	if a := p.Pick(); a.UID != "uid-b" {
		t.Fatalf("第 2 次 Pick 应选 b: got %s", a.UID)
	}
	if a := p.Pick(); a.UID != "uid-a" {
		t.Fatalf("第 3 次 Pick 应回卷到 a: got %s", a.UID)
	}
	// a 冷却中：跳过，b 连选
	p.MarkLimited("uid-a", time.Now().Add(time.Hour))
	for i := 0; i < 3; i++ {
		if a := p.Pick(); a.UID != "uid-b" {
			t.Fatalf("冷却中的 a 应被跳过: got %s", a.UID)
		}
	}
	// 到期即自动可选（懒判定，无需后台循环），恢复后轮询继续推进
	p.MarkLimited("uid-a", time.Now().Add(-time.Minute))
	if a := p.Pick(); a.UID != "uid-a" {
		t.Fatalf("a 冷却已到期应可选: got %s", a.UID)
	}
	if a := p.Pick(); a.UID != "uid-b" {
		t.Fatalf("到期恢复后轮询应推进到 b: got %s", a.UID)
	}
	// 全池冷却 → nil（调用方如实透传上游 429，不编造响应）
	p.MarkLimited("uid-a", time.Now().Add(time.Hour))
	p.MarkLimited("uid-b", time.Now().Add(time.Hour))
	if a := p.Pick(); a != nil {
		t.Fatalf("全池冷却应返回 nil, got %s", a.UID)
	}
	// MarkLimited 未知 uid：静默不炸
	p.MarkLimited("uid-nope", time.Now().Add(time.Hour))
}

// Rescan：新文件落地（第二个 .info，新 uid）→ Size 增 1；同 uid 新文件 mtime
// 更新 → 该 uid 仍只 1 个号、AuthPath 换新文件、既有 LimitedUntil 冷却继承。
func TestPoolRescan(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	base := time.Now()

	writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "甲", base.Add(-2*time.Hour))
	p := NewAccountPool(RegionCN)
	if p == nil || p.Size() != 1 {
		t.Fatalf("建池应含 1 个号, got %v", p)
	}
	// uid-a 记冷却（rescan 不得丢）
	marked := base.Add(time.Hour)
	p.MarkLimited("uid-a", marked)

	// 新文件落地（第二个 .info，新 uid）→ Rescan 后 Size 增 1
	writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "乙", base.Add(-1*time.Hour))
	p.Rescan(RegionCN)
	if p.Size() != 2 {
		t.Fatalf("rescan 后应 2 个号, got %d", p.Size())
	}
	// 冷却继承：uid-a 仍冷却中，Pick 连选 uid-b
	for i := 0; i < 3; i++ {
		if a := p.Pick(); a == nil || a.UID != "uid-b" {
			t.Fatalf("uid-a 冷却应被跳过: got %v", a)
		}
	}

	// 同 uid 新文件 mtime 更新（桌面端重登同一账号）→ 仍 1 个号、AuthPath 换新
	// 文件、LimitedUntil 继承
	writeAuthInfo(t, dir, "a-new.info", "www.codebuddy.cn", "uid-a", "甲", base)
	p.Rescan(RegionCN)
	if p.Size() != 2 {
		t.Fatalf("同 uid 换文件后仍应 2 个号, got %d", p.Size())
	}
	var va *AccountView
	for _, v := range p.Snapshot() {
		if v.UID == "uid-a" {
			vv := v
			va = &vv
		}
	}
	if va == nil {
		t.Fatal("rescan 后应仍有 uid-a")
	}
	if va.File != "a-new.info" {
		t.Fatalf("同 uid 应换绑 mtime 最新的文件: got %s want a-new.info", va.File)
	}
	if !va.Cooling || !va.LimitedUntil.Equal(marked) {
		t.Fatalf("rescan 应继承既有冷却: cooling=%v until=%v want %v", va.Cooling, va.LimitedUntil, marked)
	}
}

// ClearCool：MarkLimited 后 ClearCool → Pick 立即可选；未知 uid → ok=false。
func TestPoolClearCool(t *testing.T) {
	p := &AccountPool{accounts: []*Account{
		{UID: "uid-a", Nickname: "A", AuthPath: "a.info"},
		{UID: "uid-b", Nickname: "B", AuthPath: "b.info"},
	}}
	p.MarkLimited("uid-a", time.Now().Add(time.Hour))
	p.MarkLimited("uid-b", time.Now().Add(time.Hour))
	if a := p.Pick(); a != nil {
		t.Fatalf("全池冷却应返回 nil, got %s", a.UID)
	}
	if p.ClearCool("uid-nope") {
		t.Fatal("未知 uid 的 clear_cool 应返回 false")
	}
	if !p.ClearCool("uid-a") {
		t.Fatal("池内 uid 的 clear_cool 应返回 true")
	}
	a := p.Pick()
	if a == nil || a.UID != "uid-a" {
		t.Fatalf("清冷却后 uid-a 应立即可选: got %v", a)
	}
}

// Snapshot：不泄漏 token（JSON 里无 accessToken 字段）、含完整 uid 字段供
// 前端回传；AuthPath 只给文件名不给全路径。
func TestPoolSnapshot(t *testing.T) {
	p := &AccountPool{accounts: []*Account{
		{UID: "uid-1234567890ab", Nickname: "甲", AuthPath: filepath.Join("some-secret-dir", "x.info"),
			LimitedUntil: time.Now().Add(time.Hour)},
		{UID: "uid-cccccccccccc", Nickname: "乙", AuthPath: filepath.Join("some-secret-dir", "y.info")},
	}}
	snap := p.Snapshot()
	if len(snap) != 2 {
		t.Fatalf("快照应含 2 个号, got %d", len(snap))
	}
	if snap[0].UID != "uid-1234567890ab" {
		t.Fatalf("快照应含完整 uid 字段: got %s", snap[0].UID)
	}
	if snap[0].UIDShort != "uid-1234" || snap[1].UIDShort != "uid-cccc" {
		t.Fatalf("uidShort 应取前 8 位: got %s / %s", snap[0].UIDShort, snap[1].UIDShort)
	}
	if snap[0].File != "x.info" || snap[1].File != "y.info" {
		t.Fatalf("快照只给文件名: got %s / %s", snap[0].File, snap[1].File)
	}
	if !snap[0].Cooling || snap[1].Cooling {
		t.Fatalf("冷却标记不符: got %v / %v", snap[0].Cooling, snap[1].Cooling)
	}
	b, err := json.Marshal(snap)
	if err != nil {
		t.Fatal(err)
	}
	s := string(b)
	if strings.Contains(s, "accessToken") || strings.Contains(s, "tok-") {
		t.Fatalf("快照 JSON 不得含 token 字段: %s", s)
	}
	if strings.Contains(s, "some-secret-dir") {
		t.Fatalf("快照 JSON 不得含完整路径: %s", s)
	}
	if !strings.Contains(s, "uid-1234567890ab") {
		t.Fatalf("快照 JSON 应含完整 uid: %s", s)
	}
}

// INTL 6004 英文报文：关键字 "frequency limit"（大小写不敏感）+ UTC+8 重置时点。
// 报文引用用户实测原句："usage exceeds frequency limit" +
// "your usage will reset at 2026-09-12 19:37:59 UTC+8"。
func TestParseQuotaReset6004(t *testing.T) {
	body := `{"code":6004,"message":"your usage exceeds frequency limit, your usage will reset at 2026-09-12 19:37:59 UTC+8"}`
	until := parseQuotaReset(body)
	if until.IsZero() {
		t.Fatal("6004 英文频控报文应解析出重置时点")
	}
	want := time.Date(2026, 9, 12, 19, 37, 59, 0, time.FixedZone("UTC+8", 8*3600))
	if !until.Equal(want) {
		t.Fatalf("重置时点不符: got %v want %v", until, want)
	}
	// 大小写不敏感
	if u := parseQuotaReset(strings.Replace(body, "frequency limit", "FREQUENCY LIMIT", 1)); u.IsZero() {
		t.Fatal("frequency limit 应大小写不敏感命中")
	}
	// 含 frequency limit 但无时点 → 零值（不瞎猜窗口）
	if u := parseQuotaReset(`{"code":6004,"message":"usage exceeds frequency limit"}`); !u.IsZero() {
		t.Fatal("无时点的频控报文应返回零值")
	}
	// 其他 4xx/5xx 报文（11134/11128 等）不误触发
	for _, b := range []string{
		`{"code":11134,"message":"server error"}`,
		`{"code":11128,"message":"Illegal API invocation from an unapproved channel"}`,
	} {
		if u := parseQuotaReset(b); !u.IsZero() {
			t.Fatalf("非频控报文不应解析出时点: %q -> %v", b, u)
		}
	}
}

// TestChatConstant429Terminates 终止性回归（第二轮审查驳回项）：守卫
// (attempt == 0 || accountResent) 曾对 trip 后的下一轮 429 不设防——当
// cfg.fallback 自指成 hy4-preview 时，trip 把 model 改写成它自己原地重放，
// 恒 429 下无限 continue（审查员 mock 实测 15 秒 91 万次请求不返回）。
// 修复双保险：自指 fallback 拒绝 trip + hy4Tripped 一次上限。上游 mock 用
// httptest + 临时 overlay regionConfigs 的 BaseURL，auth 用 t.TempDir() 假目录，
// 不碰真实 auth 目录、不占真实端口。
func TestChatConstant429Terminates(t *testing.T) {
	const quota429 = `{"code":6004,"message":"your usage exceeds frequency limit, your usage will reset at 2026-09-12 19:37:59 UTC+8"}`

	for name, fallback := range map[string]string{
		// 驳回项原样复现：fallback=hy4-preview（自指）→ 拒绝 trip，2 发后如实透传
		"self-referential-fallback": hy4Primary,
		// 非自指 fallback：trip 重放一次后 hy4Tripped 兜住，3 发后如实透传
		"normal-fallback": hy4Fallback,
	} {
		t.Run(name, func(t *testing.T) {
			// 双账号假 auth 目录（token 2100 年到期 → 不会触发刷新的网络调用）
			dir := t.TempDir()
			base := time.Now()
			p1 := writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "A", base)
			p2 := writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "B", base.Add(-time.Minute))

			var hits atomic.Int32
			srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				hits.Add(1)
				w.WriteHeader(http.StatusTooManyRequests)
				io.WriteString(w, quota429)
			}))
			defer srv.Close()

			// overlay：CN 区 BaseURL 指向 mock（t.Cleanup 恢复，不影响其他用例）
			oldCfg := regionConfigs[RegionCN]
			ov := oldCfg
			ov.BaseURL = srv.URL
			regionConfigs[RegionCN] = ov
			t.Cleanup(func() { regionConfigs[RegionCN] = oldCfg })

			s := &Server{
				cred: NewCredential(p1, RegionCN),
				pool: &AccountPool{accounts: []*Account{
					{UID: "uid-a", Nickname: "A", AuthPath: p1},
					{UID: "uid-b", Nickname: "B", AuthPath: p2},
				}},
				region: RegionCN,
				client: srv.Client(),
				stats:  map[string]*modelStat{},
			}
			s.failEnabled = true
			s.failFallback = fallback

			// 看门狗：handleChat 若死循环，10 秒内必被逮住（带缓冲 channel 防泄漏；
			// srv.Close 后死循环的 client.Do 会报错退出，goroutine 不悬空）
			done := make(chan int, 1)
			go func() {
				rr := httptest.NewRecorder()
				s.handleChat(rr, httptest.NewRequest(http.MethodPost, "/v1/chat/completions",
					strings.NewReader(`{"model":"hy4-preview","messages":[{"role":"user","content":"hi"}]}`)))
				done <- rr.Code
			}()
			select {
			case code := <-done:
				if code != http.StatusTooManyRequests {
					t.Fatalf("恒 429 应如实透传 429，got %d", code)
				}
			case <-time.After(10 * time.Second):
				t.Fatalf("handleChat 未返回（疑似死循环），上游已被打 %d 次", hits.Load())
			}
			// 终止性契约：每请求最多 原始 + 换号重发 1 次 + hy4 兜底重放 1 次 = 3 发
			if n := hits.Load(); n == 0 || n > 3 {
				t.Fatalf("上游请求数应为 1..3 发，got %d（死循环会爆到天文数字）", n)
			}
		})
	}
}
