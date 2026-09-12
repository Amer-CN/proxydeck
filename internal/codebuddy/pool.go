// pool.go —— WorkBuddy/CodeBuddy 双区账号池（CN + INTL 后端自动轮换）。
//
// 骨架照抄 internal/tuanjie/accounts.go，按本任务做减法：
//   - 账号来源是桌面端 auth 目录的 *.info 文件（不是手填 token 串）：扫描入池、
//     同 uid 留 mtime 最新；auth 目录是桌面端的地盘，全程只读——不落盘、
//     不回写、不改名、不删除。新账号登录 = 新文件落地 = 下次启动入池。
//   - 限流信号是上游 429/6004（frequency limit），重置时点直接在报文里——
//     MarkLimited 只记冷却到期时点，到期即自动可选（Pick 懒判定），
//     不需要 tuanjie 的 ResumeBudgetLoop 后台循环。
//   - 状态三件套（对齐团结账号矩阵）：Enabled 手动停用 / Removed 移出（uid
//     键持久排除，绝不碰 auth 文件）/ UseCount·LastUsed·Inflight 调用统计。
//     持久化只落 Enabled/Removed（exe 同目录 codebuddy-pool(-intl).json）；
//     统计与 Dead 一律内存态（Dead 的 rescan 复位语义不变）。
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
	Dead         bool      // 凭据失效（上游 401/403）：无限期标记，Pick 永不再选；复活只靠 Rescan
	Enabled      bool      // 手动停用开关（GUI「停用/启用」）；建池缺省 true
	Removed      bool      // 从池中移出（GUI「移除」）：uid 键持久排除；恢复靠 Restore（可撤销）
	UseCount     int64     // 调用次数（Pick 选中后由调用方 Touch 计数）
	LastUsed     time.Time // 最近使用时点（Touch 维护）
	Inflight     int64     // 进行中请求数（handleChat Inc/Dec，GUI「进行中 N」）
}

// AccountPool 多账号轮询：单锁 + 切片 + 轮询指针（照抄 tuanjie 的并发模型）。
type AccountPool struct {
	mu        sync.Mutex
	accounts  []*Account
	index     int
	statePath string // Enabled/Removed 持久化文件（exe 同目录；空 = 不落盘）
}

// poolStateEntry 状态文件里单 uid 的持久条目（只存用户意图：停用/移出）。
type poolStateEntry struct {
	Enabled bool `json:"enabled"`
	Removed bool `json:"removed"`
}

// poolState 状态文件整体结构：
// {"version":1,"accounts":{"<完整uid>":{"enabled":bool,"removed":bool}}}。
type poolState struct {
	Version  int                      `json:"version"`
	Accounts map[string]poolStateEntry `json:"accounts"`
}

// poolStatePath 状态文件落 exe 同目录（与 stats/failover 同规矩）：CN 用
// codebuddy-pool.json，INTL 沿 -intl 后缀惯例用 codebuddy-pool-intl.json（两区
// 实例可能同时跑，共用一个文件会互相覆盖写）。CODEBUDDY_POOL_DIR 环境变量可
// 重定向目录（单测隔离与临时实例用；真实运行由 GUI spawn，不带该变量）。
func poolStatePath(region Region) string {
	dir := ""
	if d := os.Getenv("CODEBUDDY_POOL_DIR"); d != "" {
		dir = d
	} else if exe, err := os.Executable(); err == nil {
		dir = filepath.Dir(exe)
	} else {
		return ""
	}
	suffix := ""
	if region == RegionINTL {
		suffix = "-intl"
	}
	return filepath.Join(dir, "codebuddy-pool"+suffix+".json")
}

// loadPoolState 读回停用/移出状态；文件缺失/损坏返回 nil（建池零障碍，不报错）。
func loadPoolState(path string) map[string]poolStateEntry {
	if path == "" {
		return nil
	}
	b, err := os.ReadFile(path)
	if err != nil {
		return nil
	}
	var st poolState
	if json.Unmarshal(b, &st) != nil {
		return nil
	}
	return st.Accounts
}

// saveStateLocked 全量落盘（须持锁调用）：0600 + tmp+rename 原子写（参考
// saveStats 的落盘写法）。只写池内成员——磁盘上历史残留的 uid 条目就此清除。
// 写失败静默（状态文件是记忆不是命脉，池本身不受影响）。
func (p *AccountPool) saveStateLocked() {
	if p.statePath == "" {
		return
	}
	st := poolState{Version: 1, Accounts: map[string]poolStateEntry{}}
	for _, a := range p.accounts {
		st.Accounts[a.UID] = poolStateEntry{Enabled: a.Enabled, Removed: a.Removed}
	}
	b, err := json.Marshal(st)
	if err != nil {
		return
	}
	tmp := p.statePath + ".tmp"
	if os.WriteFile(tmp, b, 0o600) == nil {
		_ = os.Rename(tmp, p.statePath)
	}
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
	p := &AccountPool{accounts: accounts, statePath: poolStatePath(region)}
	// 建池时读状态文件：按 uid 合并停用/移出（磁盘上有条目的号恢复用户意图，
	// 没条目的保持扫描缺省 Enabled=true / Removed=false）；磁盘上已消失的 uid
	// 条目在合并时自然落空，忽略不报错。
	if st := loadPoolState(p.statePath); st != nil {
		for _, a := range p.accounts {
			if e, ok := st[a.UID]; ok {
				a.Enabled, a.Removed = e.Enabled, e.Removed
			}
		}
	}
	return p
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
			found = append(found, cand{Account{UID: uid, Nickname: nick, AuthPath: path, Enabled: true}, info.ModTime()})
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

// Pick 轮询选一个可用号：跳过冷却中（LimitedUntil 未到期）、凭据失效
//（Dead）、手动停用（!Enabled）、已移除（Removed）的号，轮询推进；全池
// 不可用 → 返回 nil（调用方如实透传上游响应，不编造）。冷却到期自动可选
//（懒判定，无后台恢复循环）；Dead 无限期不自动恢复；停用/移出只靠用户动作。
func (p *AccountPool) Pick() *Account {
	p.mu.Lock()
	defer p.mu.Unlock()
	now := time.Now()
	n := len(p.accounts)
	for i := 0; i < n; i++ {
		idx := (p.index + i) % n
		a := p.accounts[idx]
		if a.Dead || !a.Enabled || a.Removed || now.Before(a.LimitedUntil) {
			continue
		}
		p.index = (idx + 1) % n
		return a
	}
	log.Printf("[codebuddy] 账号池 %d 个号全部不可用（冷却/停用/移除/凭据失效），本次无可用号", n)
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

// MarkDead 凭据失效标记（上游 401/403）：token 被官方吊销不会自愈（重新登录
// 才有救），无限期标记，Pick 永不再选；复活只靠 Rescan（用户桌面端重新登录
// → 新文件落地 → 点重新扫描 → 死号复活再实测）。找不到 uid 静默
// （与 MarkLimited 同款）。
func (p *AccountPool) MarkDead(uid string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.Dead = true
			log.Printf("[codebuddy] 账号 %s 凭据失效(401)，入池但不再轮到它", uidShort(uid))
			return
		}
	}
}

// Toggle 手动停用/启用（GUI「停用/启用」按钮）：改内存 + 全量落盘，Pick 即时
// 生效。完整 uid 匹配（GUI 回传 Snapshot 里的完整 uid）；找不到返回 false。
// 只改 Enabled，不碰 Removed/Dead（三态互不越权）。
func (p *AccountPool) Toggle(uid string, enabled bool) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.Enabled = enabled
			p.saveStateLocked()
			log.Printf("[codebuddy] 账号 %s 手动%s", uidShort(uid), map[bool]string{true: "启用", false: "停用"}[enabled])
			return true
		}
	}
	return false
}

// Remove 从池中移出（GUI「移除」按钮）：Removed=true，Pick 永不再选；uid 键
// 持久排除——状态文件落盘，rescan 不复活。**不碰 auth 文件**（桌面端地盘
// 红线：移出只是代理轮换层面的排除，登录文件原样留着）。完整 uid 匹配，
// 找不到返回 false。
func (p *AccountPool) Remove(uid string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.Removed = true
			p.saveStateLocked()
			log.Printf("[codebuddy] 账号 %s 已移出账号池（可恢复，不动登录文件）", uidShort(uid))
			return true
		}
	}
	return false
}

// Restore 恢复被移出的号（GUI「恢复」按钮，可撤销）：Removed=false。完整 uid
// 匹配，找不到返回 false；不是移出态的号调它无副作用（幂等清标记）。
func (p *AccountPool) Restore(uid string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.Removed = false
			p.saveStateLocked()
			log.Printf("[codebuddy] 账号 %s 已恢复入池", uidShort(uid))
			return true
		}
	}
	return false
}

// IncLoad 进行中计数 +1（handleChat Pick 选中后调，GUI「进行中 N」）。
func (p *AccountPool) IncLoad(uid string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.Inflight++
			return
		}
	}
}

// DecLoad 进行中计数 -1（handleChat 请求结束 defer 调；换号分支先 Dec 旧号）。
// 钳在 0（与 tuanjie DecLoad 同款，防负数）。
func (p *AccountPool) DecLoad(uid string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			if a.Inflight > 0 {
				a.Inflight--
			}
			return
		}
	}
}

// Touch 使用计数：UseCount+1 + LastUsed=now（Pick 选中后由调用方调；
// 与 tuanjie 在 Pick 内计数同口径——选了就算用，401 换号双方各计一次）。
func (p *AccountPool) Touch(uid string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			a.UseCount++
			a.LastUsed = time.Now()
			return
		}
	}
}

// PathOf 按完整 uid 精确取号（积分面板等按账号查询用）。完整 uid 匹配，
// 不用 uidShort——前 8 位会碰撞，串号比查不到更糟。找不到 ok=false。
func (p *AccountPool) PathOf(uid string) (path, nickname, uidOut string, ok bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UID == uid {
			return a.AuthPath, a.Nickname, a.UID, true
		}
	}
	return "", "", "", false
}

// NewestPath 池内排序第一号（建池按 mtime 新→旧，第一号=当前最新登录）——
// 不带 uid 的默认查询用，与旧行为"绑 mtime 最新文件"语义一致。空池 ok=false。
func (p *AccountPool) NewestPath() (path, nickname, uid string, ok bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if len(p.accounts) == 0 {
		return "", "", "", false
	}
	a := p.accounts[0]
	return a.AuthPath, a.Nickname, a.UID, true
}

// AccountView 池成员快照（GET /v1/accounts 响应用）：完整 uid 字段供 GUI 回传
// clear_cool/toggle/remove/restore（端点用完整 uid 匹配，避免前 8 位碰撞）——
// 只在响应体里出现，不进日志；屏显用 UIDShort；AuthPath 只给文件名
//（filepath.Base），不给全路径；不含 token（Account 本身就不存 token）。
// last_used 为 RFC3339 字符串（GUI 截 MM-dd HH:mm，团结同款）；未使用给空串。
type AccountView struct {
	UID          string    `json:"uid"`
	UIDShort     string    `json:"uid_short"`
	Nickname     string    `json:"nickname"`
	File         string    `json:"file"`
	LimitedUntil time.Time `json:"limited_until"`
	Cooling      bool      `json:"cooling"`
	Dead         bool      `json:"dead"`
	Enabled      bool      `json:"enabled"`
	Removed      bool      `json:"removed"`
	UseCount     int64     `json:"use_count"`
	LastUsed     string    `json:"last_used"`
	Inflight     int64     `json:"inflight"`
}

// Snapshot 锁内复制池成员视图（GET /v1/accounts 用）；顺序同池内 mtime 新→旧
//（removed 卡排最后是 GUI 的展示排序，快照保持池内真实顺序）。
func (p *AccountPool) Snapshot() []AccountView {
	p.mu.Lock()
	defer p.mu.Unlock()
	now := time.Now()
	out := make([]AccountView, 0, len(p.accounts))
	for _, a := range p.accounts {
		lastUsed := ""
		if !a.LastUsed.IsZero() {
			lastUsed = a.LastUsed.Format(time.RFC3339)
		}
		out = append(out, AccountView{
			UID:          a.UID,
			UIDShort:     uidShort(a.UID),
			Nickname:     a.Nickname,
			File:         filepath.Base(a.AuthPath),
			LimitedUntil: a.LimitedUntil,
			Cooling:      now.Before(a.LimitedUntil),
			Dead:         a.Dead,
			Enabled:      a.Enabled,
			Removed:      a.Removed,
			UseCount:     a.UseCount,
			LastUsed:     lastUsed,
			Inflight:     a.Inflight,
		})
	}
	return out
}

// Rescan 按 NewAccountPool 的同套扫描逻辑重扫（GUI「重新扫描」按钮：桌面端刚
// 登完号不用重启即可入池）。同 uid 命中则继承既有 LimitedUntil 冷却与
// Enabled/Removed（停用/移出是用户意图，rescan 不得复活），新号零值冷却 +
// 缺省启用；Dead 标记不继承——rescan 即复位，全部死号复活（凭据失效不会自愈，
// 但重新登录落地新文件后应再实测，代价 = 死号可能再吃一枪 401 再标记）。
// 文件已删的号出池；rescan 后池内顺序重新按 mtime 排。扫描在锁外做（文件 IO），
// 合并进池在锁内做。
func (p *AccountPool) Rescan(region Region) {
	fresh := scanAuthFiles(region)
	p.mu.Lock()
	defer p.mu.Unlock()
	type keep struct {
		until   time.Time
		enabled bool
		removed bool
	}
	old := map[string]keep{}
	for _, a := range p.accounts {
		old[a.UID] = keep{a.LimitedUntil, a.Enabled, a.Removed}
	}
	for _, a := range fresh {
		if k, ok := old[a.UID]; ok {
			a.LimitedUntil, a.Enabled, a.Removed = k.until, k.enabled, k.removed
		}
	}
	p.accounts = fresh
	if p.index >= len(p.accounts) {
		p.index = 0
	}
}

// ClearCool 把指定号的 LimitedUntil 置零值（GUI「提前恢复」按钮，冷却到期前
// 手动解除）。只清冷却，不清 Dead——提前恢复按钮不该复活死号。uid 匹配用
// 完整 uid（GUI 回传 Snapshot 里的完整 uid）；未知 uid 返回 false。
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
