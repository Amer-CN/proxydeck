// pool.go —— WorkBuddy/CodeBuddy 双区账号池（CN + INTL 后端自动轮换）。
//
// 骨架照抄 internal/tuanjie/accounts.go，按本任务做减法：
//   - 账号来源是桌面端 auth 目录的 *.info 文件（不是手填 token 串）：扫描入池、
//     同 uid 留 mtime 最新；auth 目录是桌面端的地盘，全程只读——不落盘、
//     不回写、不改名、不删除。新账号登录 = 新文件落地 = 下次启动入池。
//   - 限流信号是上游 429/6004（frequency limit），重置时点直接在报文里——
//     MarkLimited 只记冷却到期时点，到期即自动可选（Pick 懒判定），
//     不需要 tuanjie 的 ResumeBudgetLoop 后台循环。
//   - 不做模型认领（Models）、负载感知（UseCount/loads）、Enabled 手动开关：
//     用户场景是"哪个能跑用哪个"。
package codebuddy

import (
	"encoding/json"
	"log"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

// Account 账号池里的一个登录态（对应 auth 目录里一个 *.info 文件）。
type Account struct {
	UID          string    // 桌面端账号 uid（X-User-Id）
	Nickname     string    // 显示名（日志用）
	AuthPath     string    // *.info 文件路径（只读）
	LimitedUntil time.Time // 6004 冷却到期时点；零值 = 可用
}

// AccountPool 多账号轮询：单锁 + 切片 + 轮询指针（照抄 tuanjie 的并发模型）。
type AccountPool struct {
	mu       sync.Mutex
	accounts []*Account
	index    int
}

// NewAccountPool 扫描 auth 目录建池：命中区域 domain 的全部 *.info 文件入池；
// 解析失败的（读不了 / JSON 坏 / 无 uid）跳过，不整池报错；同 uid 只留 mtime
// 最新的一个（旧文件是历史登录态，token 可能已吊销）。目录里一个可用号都
// 没有 → 返回 nil（调用方保持"未找到登录文件"的现有报错行为）。
// 池内按 mtime 新→旧排序：第一次 Pick 即 mtime 最新的号，与引入池之前
// "绑定 mtime 最新文件"的旧行为起点一致。
func NewAccountPool(region Region) *AccountPool {
	accounts := scanAuthFiles(region)
	if len(accounts) == 0 {
		return nil
	}
	return &AccountPool{accounts: accounts}
}

// scanAuthFiles 扫描 auth 目录里命中区域 domain 的全部 *.info 文件（NewAccountPool
// 建池与 Rescan 重扫共用同一套扫描逻辑，不复制扫描体）。解析失败的（读不了 /
// JSON 坏 / 无 uid）或区域不符的跳过，不整池报错；同 uid 只留 mtime 最新的一个。
// 返回按 mtime 新→旧排序（池内顺序契约：第一次 Pick 即 mtime 最新的号）。
func scanAuthFiles(region Region) []*Account {
	suffix := regionConfigs[region].DomainSuffix
	type cand struct {
		acct  Account
		mtime time.Time
	}
	var found []cand
	for _, dir := range findAuthDirs() {
		ents, err := os.ReadDir(dir)
		if err != nil {
			continue
		}
		for _, e := range ents {
			if e.IsDir() || !strings.HasSuffix(e.Name(), ".info") {
				continue
			}
			info, err := e.Info()
			if err != nil {
				continue
			}
			path := filepath.Join(dir, e.Name())
			domain, uid, nick, ok := parseAuthInfo(path)
			if !ok || !containsDomain(domain, suffix) {
				log.Printf("[codebuddy] 账号池扫描：跳过无法解析或区域不符的文件 %s", e.Name())
				continue
			}
			found = append(found, cand{Account{UID: uid, Nickname: nick, AuthPath: path}, info.ModTime()})
		}
	}
	latest := map[string]cand{}
	for _, f := range found {
		if old, ok := latest[f.acct.UID]; !ok || f.mtime.After(old.mtime) {
			latest[f.acct.UID] = f
		}
	}
	uniq := make([]cand, 0, len(latest))
	for _, f := range latest {
		uniq = append(uniq, f)
	}
	sort.Slice(uniq, func(i, j int) bool { return uniq[i].mtime.After(uniq[j].mtime) })
	accounts := make([]*Account, 0, len(uniq))
	for _, f := range uniq {
		a := f.acct
		accounts = append(accounts, &a)
	}
	return accounts
}

// parseAuthInfo 读 *.info 文件的 domain/uid/昵称（建池用；token 等其余字段
// 不单独处理、不落任何输出）。读不了 / JSON 坏 / 无 uid 均视为解析失败
// （真实目录里实测存在半截坏文件，必须跳过不能整池报错）。
func parseAuthInfo(path string) (domain, uid, nickname string, ok bool) {
	b, err := os.ReadFile(path)
	if err != nil {
		return "", "", "", false
	}
	var s sessionInfo
	if json.Unmarshal(b, &s) != nil || s.Account.UID == "" {
		return "", "", "", false
	}
	return s.Auth.Domain, s.Account.UID, s.Account.Nickname, true
}

// Size 返回池内账号数。
func (p *AccountPool) Size() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.accounts)
}

// Pick 轮询选一个可用号：跳过 LimitedUntil 未到期的号，轮询推进；全池冷却
// → 返回 nil（调用方如实透传上游 429，不编造响应）。到期自动可选（懒判定，
// 无后台恢复循环——上游报文给了明确 reset 时点，到期必恢复）。
func (p *AccountPool) Pick() *Account {
	p.mu.Lock()
	defer p.mu.Unlock()
	now := time.Now()
	n := len(p.accounts)
	for i := 0; i < n; i++ {
		idx := (p.index + i) % n
		a := p.accounts[idx]
		if now.Before(a.LimitedUntil) {
			continue
		}
		p.index = (idx + 1) % n
		return a
	}
	log.Printf("[codebuddy] 账号池 %d 个号全部冷却中，本次无可用号", n)
	return nil
}

// MarkLimited 收到 6004/配额 429 时给当前号记冷却（until = 上游报文里的
// 重置时点）；到期即自动可选。
func (p *AccountPool) MarkLimited(uid string, until time.Time) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.LimitedUntil = until
			log.Printf("[codebuddy] 账号 %s 限流冷却至 %s", uidShort(uid), until.Format("01-02 15:04:05"))
			return
		}
	}
}

// AccountView 池成员快照（GET /v1/accounts 响应用）：完整 uid 字段供 GUI 回传
// clear_cool（端点用完整 uid 匹配，避免前 8 位碰撞）——只在响应体里出现，不进
// 日志；屏显用 UIDShort；AuthPath 只给文件名（filepath.Base），不给全路径；
// 不含 token（Account 本身就不存 token）。
type AccountView struct {
	UID          string    `json:"uid"`
	UIDShort     string    `json:"uid_short"`
	Nickname     string    `json:"nickname"`
	File         string    `json:"file"`
	LimitedUntil time.Time `json:"limited_until"`
	Cooling      bool      `json:"cooling"`
}

// Snapshot 锁内复制池成员视图（GET /v1/accounts 用）；顺序同池内 mtime 新→旧。
func (p *AccountPool) Snapshot() []AccountView {
	p.mu.Lock()
	defer p.mu.Unlock()
	now := time.Now()
	out := make([]AccountView, 0, len(p.accounts))
	for _, a := range p.accounts {
		out = append(out, AccountView{
			UID:          a.UID,
			UIDShort:     uidShort(a.UID),
			Nickname:     a.Nickname,
			File:         filepath.Base(a.AuthPath),
			LimitedUntil: a.LimitedUntil,
			Cooling:      now.Before(a.LimitedUntil),
		})
	}
	return out
}

// Rescan 按 NewAccountPool 的同套扫描逻辑重扫（GUI「重新扫描」按钮：桌面端刚
// 登完号不用重启即可入池）。同 uid 命中则继承既有 LimitedUntil 冷却，新号零值
// 冷却；文件已删的号出池；rescan 后池内顺序重新按 mtime 排。扫描在锁外做
//（文件 IO），合并进池在锁内做。
func (p *AccountPool) Rescan(region Region) {
	fresh := scanAuthFiles(region)
	p.mu.Lock()
	defer p.mu.Unlock()
	cool := map[string]time.Time{}
	for _, a := range p.accounts {
		cool[a.UID] = a.LimitedUntil
	}
	for _, a := range fresh {
		if until, ok := cool[a.UID]; ok {
			a.LimitedUntil = until
		}
	}
	p.accounts = fresh
	if p.index >= len(p.accounts) {
		p.index = 0
	}
}

// ClearCool 把指定号的 LimitedUntil 置零值（GUI「提前恢复」按钮，冷却到期前
// 手动解除）。uid 匹配用完整 uid（GUI 回传 Snapshot 里的完整 uid）；未知 uid
// 返回 false。
func (p *AccountPool) ClearCool(uid string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.LimitedUntil = time.Time{}
			return true
		}
	}
	return false
}

// uidShort 日志用 uid 前 8 位（不打 token、不打完整 uid）。
func uidShort(uid string) string {
	if len(uid) > 8 {
		return uid[:8]
	}
	return uid
}
