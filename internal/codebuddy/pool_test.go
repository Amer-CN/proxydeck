// pool_test.go —— 账号池单测：建池去重（同 uid 留 mtime 最新）/ 坏文件跳过 /
// Pick 轮换跳冷却 / 全池冷却返 nil / parseQuotaReset 提 6004 英文报文时点 /
// 死号（凭据失效 401）：MarkDead 跳过·全死 nil·Rescan 复位·ClearCool 不清死·
// handleChat 401 换号重发与全死透传；状态三件套：toggle/remove/restore 落盘-
// 重载往返 / Pick 跳过停用移除 / rescan 合并保留 / inflight 并发归零 / Touch
// 计数 / 积分路径 401 标死（免费验活）。
// auth 目录一律用 t.TempDir() 造假，经 CODEBUDDY_AUTH_DIR 指给 findAuthDirs，
// 状态文件目录经 CODEBUDDY_POOL_DIR 同款隔离，绝不读写真实 auth 目录。
package codebuddy

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
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
		{UID: "uid-a", Nickname: "A", AuthPath: "a.info", Enabled: true},
		{UID: "uid-b", Nickname: "B", AuthPath: "b.info", Enabled: true},
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
		{UID: "uid-a", Nickname: "A", AuthPath: "a.info", Enabled: true},
		{UID: "uid-b", Nickname: "B", AuthPath: "b.info", Enabled: true},
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
			LimitedUntil: time.Now().Add(time.Hour), Enabled: true},
		{UID: "uid-cccccccccccc", Nickname: "乙", AuthPath: filepath.Join("some-secret-dir", "y.info"), Enabled: true},
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
					{UID: "uid-a", Nickname: "A", AuthPath: p1, Enabled: true},
					{UID: "uid-b", Nickname: "B", AuthPath: p2, Enabled: true},
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

// MarkDead/Pick：4 号池标死 3 个 → 连 Pick 4 次全是活号；全死 → Pick nil；
// MarkDead 未知 uid 静默不炸（与 MarkLimited 同款）。
func TestPoolMarkDeadSkip(t *testing.T) {
	p := &AccountPool{accounts: []*Account{
		{UID: "uid-a", Nickname: "A", AuthPath: "a.info", Enabled: true},
		{UID: "uid-b", Nickname: "B", AuthPath: "b.info", Enabled: true},
		{UID: "uid-c", Nickname: "C", AuthPath: "c.info", Enabled: true},
		{UID: "uid-d", Nickname: "D", AuthPath: "d.info", Enabled: true},
	}}
	p.MarkDead("uid-a")
	p.MarkDead("uid-b")
	p.MarkDead("uid-c")
	p.MarkDead("uid-nope") // 未知 uid：静默
	for i := 0; i < 4; i++ {
		if a := p.Pick(); a == nil || a.UID != "uid-d" {
			t.Fatalf("死号应被 Pick 跳过，第 %d 次应选 uid-d: got %v", i, a)
		}
	}
	p.MarkDead("uid-d")
	if a := p.Pick(); a != nil {
		t.Fatalf("全死池 Pick 应返回 nil（调用方如实透传），got %s", a.UID)
	}
}

// Rescan 清死号：标死后 Rescan → Dead 全部为 false（复位 = 重新扫描）；
// 同 uid 继承冷却的既有断言不回退（uid-b 仍冷却，Pick 只出复活的 uid-a）。
func TestPoolRescanClearsDead(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	base := time.Now()
	writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "甲", base.Add(-2*time.Hour))
	writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "乙", base.Add(-1*time.Hour))
	p := NewAccountPool(RegionCN)
	if p == nil || p.Size() != 2 {
		t.Fatalf("建池应含 2 个号, got %v", p)
	}
	until := base.Add(time.Hour)
	p.MarkLimited("uid-b", until)
	p.MarkDead("uid-a")
	p.MarkDead("uid-b")
	p.Rescan(RegionCN)
	if p.Size() != 2 {
		t.Fatalf("rescan 后应仍 2 个号, got %d", p.Size())
	}
	for _, v := range p.Snapshot() {
		if v.Dead {
			t.Fatalf("rescan 应清除全部 Dead 标记: %s 仍死", v.UID)
		}
	}
	// 冷却继承不回退：uid-b 仍冷却，Pick 跳过它只出复活的 uid-a
	if a := p.Pick(); a == nil || a.UID != "uid-a" {
		t.Fatalf("uid-b 冷却应被跳过、uid-a 复活可选: got %v", a)
	}
	for _, v := range p.Snapshot() {
		if v.UID == "uid-b" && (!v.Cooling || !v.LimitedUntil.Equal(until)) {
			t.Fatalf("rescan 应继承 uid-b 冷却: cooling=%v until=%v want %v", v.Cooling, v.LimitedUntil, until)
		}
	}
}

// ClearCool 不清 Dead（提前恢复按钮不该复活死号）；Snapshot 含 dead 字段
//（JSON 小写 dead）。
func TestPoolDeadSnapshotAndClearCool(t *testing.T) {
	p := &AccountPool{accounts: []*Account{
		{UID: "uid-a", Nickname: "A", AuthPath: "a.info", Enabled: true},
		{UID: "uid-b", Nickname: "B", AuthPath: "b.info", Enabled: true},
	}}
	p.MarkDead("uid-a")
	p.MarkLimited("uid-a", time.Now().Add(time.Hour))
	if !p.ClearCool("uid-a") {
		t.Fatal("池内 uid 的 clear_cool 应返回 true（只清冷却）")
	}
	snap := p.Snapshot()
	if len(snap) != 2 || !snap[0].Dead || snap[0].Cooling || snap[1].Dead {
		t.Fatalf("快照不符（uid-a 应 dead=true cooling=false，uid-b 应 dead=false）: %+v", snap)
	}
	b, err := json.Marshal(snap)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(b), `"dead":true`) {
		t.Fatalf("快照 JSON 应含小写 dead 字段: %s", b)
	}
	// ClearCool 后 uid-a 仍死：Pick 只出 uid-b
	if a := p.Pick(); a == nil || a.UID != "uid-b" {
		t.Fatalf("ClearCool 不清 Dead，uid-a 应仍被 Pick 跳过: got %v", a)
	}
}

// TestChat401MarkDeadRetry handleChat 401 换号重发：mock 上游 A 号恒 401、
// B 号 200 → 客户端收到 200 且上游 A/B 各打一次，A 号被 MarkDead；
// 恒 401（换完全池后全死）→ 401 如实透传且上游有界（池大小发，不死循环）。
// 上游 mock 用 httptest + 临时 overlay regionConfigs 的 BaseURL（既有手法），
// auth 用 t.TempDir() 假目录，不碰真实 auth 目录、不占真实端口。
func TestChat401MarkDeadRetry(t *testing.T) {
	// 双账号假 auth 目录（token 2100 年到期 → 不会触发刷新的网络调用）
	dir := t.TempDir()
	base := time.Now()
	pa := writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "A", base)
	pb := writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "B", base.Add(-time.Minute))

	// 场景一：A 号 401、B 号 200 → 客户端 200，上游各一次，A 被标死
	t.Run("retry-next-account", func(t *testing.T) {
		var hitsA, hitsB atomic.Int32
		srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.Header.Get("X-User-Id") == "uid-a" {
				hitsA.Add(1)
				w.WriteHeader(http.StatusUnauthorized)
				io.WriteString(w, `{"code":40100,"message":"token expired or revoked"}`)
				return
			}
			hitsB.Add(1)
			w.Header().Set("Content-Type", "text/event-stream")
			io.WriteString(w, `data: {"choices":[{"delta":{"content":"ok"}}]}`+"\n\n")
			io.WriteString(w, `data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}`+"\n\n")
			io.WriteString(w, "data: [DONE]\n\n")
		}))
		defer srv.Close()

		oldCfg := regionConfigs[RegionCN]
		ov := oldCfg
		ov.BaseURL = srv.URL
		regionConfigs[RegionCN] = ov
		t.Cleanup(func() { regionConfigs[RegionCN] = oldCfg })

		s := &Server{
			cred: NewCredential(pa, RegionCN),
			pool: &AccountPool{accounts: []*Account{
				{UID: "uid-a", Nickname: "A", AuthPath: pa, Enabled: true},
				{UID: "uid-b", Nickname: "B", AuthPath: pb, Enabled: true},
			}},
			region: RegionCN,
			client: srv.Client(),
			stats:  map[string]*modelStat{},
		}
		rr := httptest.NewRecorder()
		s.handleChat(rr, httptest.NewRequest(http.MethodPost, "/v1/chat/completions",
			strings.NewReader(`{"model":"hy4-preview","messages":[{"role":"user","content":"hi"}]}`)))
		if rr.Code != http.StatusOK {
			t.Fatalf("A 号 401 后应换 B 号重发拿到 200，got %d body=%s", rr.Code, rr.Body.String())
		}
		if hitsA.Load() != 1 || hitsB.Load() != 1 {
			t.Fatalf("上游应 A/B 各打一次: A=%d B=%d", hitsA.Load(), hitsB.Load())
		}
		for _, v := range s.pool.Snapshot() {
			if v.UID == "uid-a" && !v.Dead {
				t.Fatal("A 号 401 后应被 MarkDead")
			}
			if v.UID == "uid-b" && v.Dead {
				t.Fatal("B 号 200 不应被标死")
			}
		}
	})

	// 场景二：恒 401（A、B 全死）→ 401 如实透传；上游有界 = 池大小 2 发
	t.Run("all-dead-passthrough", func(t *testing.T) {
		var hits atomic.Int32
		srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			hits.Add(1)
			w.WriteHeader(http.StatusUnauthorized)
			io.WriteString(w, `{"code":40100,"message":"token expired or revoked"}`)
		}))
		defer srv.Close()

		oldCfg := regionConfigs[RegionCN]
		ov := oldCfg
		ov.BaseURL = srv.URL
		regionConfigs[RegionCN] = ov
		t.Cleanup(func() { regionConfigs[RegionCN] = oldCfg })

		s := &Server{
			cred: NewCredential(pa, RegionCN),
			pool: &AccountPool{accounts: []*Account{
				{UID: "uid-a", Nickname: "A", AuthPath: pa, Enabled: true},
				{UID: "uid-b", Nickname: "B", AuthPath: pb, Enabled: true},
			}},
			region: RegionCN,
			client: srv.Client(),
			stats:  map[string]*modelStat{},
		}
		// 看门狗：换号逻辑若不终止，10 秒内必被逮住
		done := make(chan int, 1)
		go func() {
			rr := httptest.NewRecorder()
			s.handleChat(rr, httptest.NewRequest(http.MethodPost, "/v1/chat/completions",
				strings.NewReader(`{"model":"hy4-preview","messages":[{"role":"user","content":"hi"}]}`)))
			done <- rr.Code
		}()
		select {
		case code := <-done:
			if code != http.StatusUnauthorized {
				t.Fatalf("全死应 401 如实透传, got %d", code)
			}
		case <-time.After(10 * time.Second):
			t.Fatalf("handleChat 未返回（疑似死循环），上游已被打 %d 次", hits.Load())
		}
		// 有界契约：2 号池全死 = A、B 各打一次共 2 发（Pick nil 即停，不循环）
		if n := hits.Load(); n != 2 {
			t.Fatalf("上游请求数应为池大小 2 发, got %d", n)
		}
		for _, v := range s.pool.Snapshot() {
			if !v.Dead {
				t.Fatalf("全死场景 401 过的号都应被 MarkDead: %+v", v)
			}
		}
	})
}

// ===== 状态三件套（停用/移除/恢复 + 持久化 + 进行中计数 + 积分 401 标死） =====

// toggle/remove/restore：改内存 + 状态文件落盘；重建池（建池读状态文件）按
// uid 合并恢复停用/移出；Pick 跳过停用/移除的号；restore 后恢复可选；rescan
// 合并保留（停用/移出不因重扫复活）；未知 uid 返回 false；磁盘上已消失的 uid
// 条目忽略不报错。状态文件目录用 CODEBUDDY_POOL_DIR 指到 t.TempDir()
//（与 CODEBUDDY_AUTH_DIR 同款隔离手法，绝不在测试间共享状态）。
func TestPoolToggleRemoveRestorePersist(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	stateDir := t.TempDir()
	t.Setenv("CODEBUDDY_POOL_DIR", stateDir)
	base := time.Now()
	writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "甲", base.Add(-2*time.Hour))
	writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "乙", base.Add(-1*time.Hour))

	p := NewAccountPool(RegionCN)
	if p == nil || p.Size() != 2 {
		t.Fatalf("建池应含 2 个号, got %v", p)
	}
	// 建池缺省：全部启用、未移除
	for _, v := range p.Snapshot() {
		if !v.Enabled || v.Removed {
			t.Fatalf("建池缺省应 enabled=true removed=false: %+v", v)
		}
	}
	// toggle 停用 uid-a：Pick 跳过（只剩 uid-b 可选）
	if !p.Toggle("uid-a", false) {
		t.Fatal("池内 uid 的 toggle 应返回 true")
	}
	if a := p.Pick(); a == nil || a.UID != "uid-b" {
		t.Fatalf("停用号应被 Pick 跳过: got %v", a)
	}
	// remove uid-b：全池不可用（uid-a 停用、uid-b 移除）
	if !p.Remove("uid-b") {
		t.Fatal("池内 uid 的 remove 应返回 true")
	}
	if a := p.Pick(); a != nil {
		t.Fatalf("停用+移除后全池不可用应 Pick nil, got %s", a.UID)
	}
	// restore uid-b：恢复可选（uid-a 仍停用）
	if !p.Restore("uid-b") {
		t.Fatal("池内 uid 的 restore 应返回 true")
	}
	if a := p.Pick(); a == nil || a.UID != "uid-b" {
		t.Fatalf("restore 后 uid-b 应恢复可选: got %v", a)
	}
	// rescan 合并保留：同 uid 继承停用/移出（登录文件都在，重扫会再扫进来）
	if !p.Remove("uid-b") {
		t.Fatal("remove 应成功")
	}
	p.Rescan(RegionCN)
	for _, v := range p.Snapshot() {
		if v.UID == "uid-a" && v.Enabled {
			t.Fatal("rescan 不应复活停用状态")
		}
		if v.UID == "uid-b" && !v.Removed {
			t.Fatal("rescan 不应复活移出状态")
		}
	}
	// 未知 uid：三个动作都返回 false
	if p.Toggle("uid-nope", true) || p.Remove("uid-nope") || p.Restore("uid-nope") {
		t.Fatal("未知 uid 的 toggle/remove/restore 应返回 false")
	}

	// 落盘-重载往返：停用态落盘后重建池（模拟重启），停用/移出按 uid 保持
	if !p.Toggle("uid-b", false) {
		t.Fatal("toggle uid-b 应成功")
	}
	p2 := NewAccountPool(RegionCN)
	if p2 == nil || p2.Size() != 2 {
		t.Fatalf("重建池应含 2 个号, got %v", p2)
	}
	got := map[string]AccountView{}
	for _, v := range p2.Snapshot() {
		got[v.UID] = v
	}
	if got["uid-a"].Enabled || got["uid-a"].Removed {
		t.Fatalf("重建池 uid-a 应保持停用（未移除）: %+v", got["uid-a"])
	}
	if got["uid-b"].Enabled || !got["uid-b"].Removed {
		t.Fatalf("重建池 uid-b 应保持停用+移除: %+v", got["uid-b"])
	}
	if a := p2.Pick(); a != nil {
		t.Fatalf("重建后全池不可用（停用/移除）应 Pick nil, got %s", a.UID)
	}
	// 状态文件已落盘且格式为 {"version":1,"accounts":{"<uid>":{"enabled":…,"removed":…}}}
	b, err := os.ReadFile(filepath.Join(stateDir, "codebuddy-pool.json"))
	if err != nil {
		t.Fatalf("状态文件应已落盘: %v", err)
	}
	var st struct {
		Version  int `json:"version"`
		Accounts map[string]struct {
			Enabled bool `json:"enabled"`
			Removed bool `json:"removed"`
		} `json:"accounts"`
	}
	if json.Unmarshal(b, &st) != nil || st.Version != 1 || !st.Accounts["uid-b"].Removed {
		t.Fatalf("状态文件格式/内容不符: %s", b)
	}
	// 磁盘上已消失的 uid 条目（幽灵条目）：建池忽略不报错，池内号不受影响
	ghost := `{"version":1,"accounts":{"uid-ghost":{"enabled":false,"removed":true}}}`
	if err := os.WriteFile(filepath.Join(stateDir, "codebuddy-pool.json"), []byte(ghost), 0o600); err != nil {
		t.Fatal(err)
	}
	p3 := NewAccountPool(RegionCN)
	if p3 == nil || p3.Size() != 2 {
		t.Fatalf("幽灵条目不应影响建池, got %v", p3)
	}
	for _, v := range p3.Snapshot() {
		if !v.Enabled || v.Removed {
			t.Fatalf("幽灵条目不应套到池内号（重载后保持缺省）: %+v", v)
		}
	}
}

// IncLoad/DecLoad 并发（-race 下 inflight 归零）：多 goroutine 同时增减同一批
// 号，结束后全池 Inflight 必须为 0；Touch 累计 UseCount 并置 LastUsed（快照
// last_used 非空）；未知 uid 静默；DecLoad 钳在 0 不出负数。
func TestPoolInflightConcurrentAndTouch(t *testing.T) {
	p := &AccountPool{accounts: []*Account{
		{UID: "uid-a", Nickname: "A", AuthPath: "a.info", Enabled: true},
		{UID: "uid-b", Nickname: "B", AuthPath: "b.info", Enabled: true},
	}}
	const workers, rounds = 16, 50
	var wg sync.WaitGroup
	for i := 0; i < workers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < rounds; j++ {
				p.IncLoad("uid-a")
				p.IncLoad("uid-b")
				p.DecLoad("uid-a")
				p.DecLoad("uid-b")
			}
		}()
	}
	wg.Wait()
	for _, v := range p.Snapshot() {
		if v.Inflight != 0 {
			t.Fatalf("并发 inc/dec 后 inflight 应归零: %+v", v)
		}
	}
	const n = 5
	for i := 0; i < n; i++ {
		p.Touch("uid-a")
	}
	snap := p.Snapshot()
	if snap[0].UseCount != n || snap[0].LastUsed == "" {
		t.Fatalf("Touch 应累计 UseCount 并置 LastUsed: %+v", snap[0])
	}
	if snap[1].UseCount != 0 || snap[1].LastUsed != "" {
		t.Fatalf("未 Touch 的号应保持零值: %+v", snap[1])
	}
	// 未知 uid：静默不炸
	p.Touch("uid-nope")
	p.IncLoad("uid-nope")
	p.DecLoad("uid-nope")
	// DecLoad 钳 0：对 0 计数连 dec 不出负数
	for i := 0; i < 3; i++ {
		p.DecLoad("uid-b")
	}
	if v := p.Snapshot()[1]; v.Inflight != 0 {
		t.Fatalf("DecLoad 应钳在 0: %+v", v)
	}
}

// 积分路径 401 标死（手动刷新积分 = 免费验活，与聊天路径同一信号）：mock 上游
// 对 uid-a 恒 401 → 查询后该号 Snapshot dead=true（无须聊天流量）；指定 uid 与
// 默认号（空 uid → NewestPath 解析成池内最新号）两条路都标得到真实 uid；
// 200 的号不被标死；err 字符串保留 "HTTP 401"（GUI 凭据失效态按它识别）。
func TestQuota401MarksDead(t *testing.T) {
	dir := t.TempDir()
	base := time.Now()
	pa := writeAuthInfo(t, dir, "a.info", "www.codebuddy.cn", "uid-a", "A", base)
	pb := writeAuthInfo(t, dir, "b.info", "www.codebuddy.cn", "uid-b", "B", base.Add(-time.Minute))

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("X-User-Id") == "uid-a" {
			w.WriteHeader(http.StatusUnauthorized)
			io.WriteString(w, `{"code":40100,"msg":"token expired or revoked"}`)
			return
		}
		_, _ = w.Write([]byte(`{"code":0,"data":{"Response":{"Data":{"Accounts":[{"PackageName":"P","CycleStartTime":"2026-01-01 00:00:00","CycleEndTime":"2099-01-01 00:00:00","CapacitySize":1000,"CapacityUsed":10,"CapacityRemain":100,"CycleCapacityRemain":100,"CycleCapacitySize":1000}]}}}}`))
	}))
	defer srv.Close()
	overlayBaseURL(t, RegionCN, srv.URL)
	ctx := context.Background()

	// 场景一：指定 uid 查询命中 401 → 该号 dead=true，另一号不受牵连
	t.Run("explicit-uid", func(t *testing.T) {
		s := &Server{cred: NewCredential(pa, RegionCN), pool: &AccountPool{accounts: []*Account{
			{UID: "uid-a", Nickname: "A", AuthPath: pa, Enabled: true},
			{UID: "uid-b", Nickname: "B", AuthPath: pb, Enabled: true},
		}}, region: RegionCN, client: srv.Client(), stats: map[string]*modelStat{}}
		q := s.FetchQuota(ctx, "uid-a")
		if q.Source != "none" || !strings.Contains(q.Err, "HTTP 401") {
			t.Fatalf("uid-a 401 应如实 source:none + err 含 HTTP 401: %+v", q)
		}
		for _, v := range s.pool.Snapshot() {
			if v.UID == "uid-a" && !v.Dead {
				t.Fatal("积分查询 401 后 uid-a 应被 MarkDead")
			}
			if v.UID == "uid-b" && v.Dead {
				t.Fatal("uid-b 不应被牵连标死")
			}
		}
	})

	// 场景二：默认号（空 uid）→ NewestPath 解析成 uid-a（mtime 最新），401 标死
	// 落到真实 uid 上（缓存键 _default 语义不变，标死目标必须是实际号）
	t.Run("default-uid-resolves-newest", func(t *testing.T) {
		s := &Server{cred: NewCredential(pa, RegionCN), pool: &AccountPool{accounts: []*Account{
			{UID: "uid-a", Nickname: "A", AuthPath: pa, Enabled: true},
			{UID: "uid-b", Nickname: "B", AuthPath: pb, Enabled: true},
		}}, region: RegionCN, client: srv.Client(), stats: map[string]*modelStat{}}
		q := s.FetchQuotaForce(ctx, "")
		if q.Source != "none" || !strings.Contains(q.Err, "HTTP 401") {
			t.Fatalf("默认号 401 应如实 source:none + err 含 HTTP 401: %+v", q)
		}
		for _, v := range s.pool.Snapshot() {
			if v.UID == "uid-a" && !v.Dead {
				t.Fatal("默认号 401 应 MarkDead 到实际选中的 uid-a")
			}
			if v.UID == "uid-b" && v.Dead {
				t.Fatal("uid-b 不应被牵连标死")
			}
		}
		// 200 的号查询正常且不标死
		qb := s.FetchQuota(ctx, "uid-b")
		if qb.Source != "live" || qb.Total != 100 {
			t.Fatalf("uid-b 200 应正常返回: %+v", qb)
		}
		for _, v := range s.pool.Snapshot() {
			if v.UID == "uid-b" && v.Dead {
				t.Fatal("200 查询不应标死")
			}
		}
	})
}
