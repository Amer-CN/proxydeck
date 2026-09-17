// pool.go —— 多 token 账号池（P0-1）：401 标死、402/429 冷却、按 JWT exp 判过期。
package vibex

import (
	"strings"
	"sync"
	"time"
)

// tokenState 单个 Rh-Accesstoken 的运行时状态（Dead/冷却/调用计数是进程内存态，重启清零）。
type tokenState struct {
	Value         string
	Dead          bool
	CooldownUntil time.Time
	Calls         int64
	Exp           time.Time // 由 JWT exp 解出；零值表示未知
	HasExp        bool
	Sub           string // JWT sub（同号去重键；空表示未知，走串去重兜底）
}

// usable 报告该 token 现在能不能用。
func (t *tokenState) usable(now time.Time) bool {
	if t.Dead {
		return false
	}
	if t.CooldownUntil.After(now) {
		return false
	}
	if t.HasExp && now.After(t.Exp) {
		return false // 过期就不必再浪费一次 401
	}
	return true
}

// Pool 是 token 池，线程安全。
type Pool struct {
	mu       sync.Mutex
	list     []*tokenState
	cooldown time.Duration
}

// NewPool 用给定顺序建池（去重）；cooldown 为 402/429 命中的冷却时长。
func NewPool(values []string, cooldown time.Duration) *Pool {
	p := &Pool{cooldown: cooldown}
	for _, v := range values {
		v = strings.TrimSpace(v)
		if v == "" {
			continue
		}
		st := &tokenState{Value: v}
		if info := decodeJWT(v); info != nil {
			st.Exp, st.HasExp = info.Exp, info.hasExp()
			st.Sub = info.Sub
		}
		p.addWithState(st) // 走统一去重（含同 sub 替换）：重启加载即自愈文件里的重复串
	}
	return p
}

// Len 返回池中 token 数量（不区分死活）。
func (p *Pool) Len() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.list)
}

// addrOf 返回 token 的稳定身份地址：Rh-Accesstoken 在响应 Set-Cookie 里可能带
// 属性尾巴（值与 cookie 值同源，故按首个分号前那一段比对，避免重复入池）。
func addrOf(v string) string {
	v = strings.TrimSpace(v)
	if i := strings.IndexByte(v, ';'); i >= 0 {
		v = strings.TrimSpace(v[:i])
	}
	return v
}

// add 把一个 token 串追加进池（按 addrOf 去重，JWT exp 现解）；已存在返回 existing=true。
// 供 POST /accounts action=add 热加载用：不重启插件即可用新号（P0-账号体验）。
func (p *Pool) add(v string) (*tokenState, bool) {
	v = strings.TrimSpace(v)
	if v == "" {
		return nil, false
	}
	st := &tokenState{Value: v}
	if info := decodeJWT(v); info != nil {
		st.Exp, st.HasExp = info.Exp, info.hasExp()
		st.Sub = info.Sub
	}
	return p.addWithState(st)
}

// addWithState 直接把建好的 tokenState 追加进池，已存在返回池里那一个。
// 自动探测路径用：凭据已经在手，不必再解一次 JWT。
// 去重两档：①同串（addrOf）→ 已存在，直接返回；②同 sub 不同串
// （每次登录签发新 JWT）→ 旧串让位给新串，保证同号在池里永远只有一条。
// sub 为空的老条目按需现解 JWT 回填，保证替换不漏网。
func (p *Pool) addWithState(st *tokenState) (*tokenState, bool) {
	if st == nil || strings.TrimSpace(st.Value) == "" {
		return nil, false
	}
	key := addrOf(st.Value)
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, t := range p.list {
		if addrOf(t.Value) == key {
			return t, true
		}
	}
	if st.Sub != "" {
		kept := make([]*tokenState, 0, len(p.list)+1)
		for _, t := range p.list {
			if t.Sub == "" {
				if info := decodeJWT(t.Value); info != nil {
					t.Sub, t.Exp, t.HasExp = info.Sub, info.Exp, info.hasExp()
				}
			}
			if t.Sub == "" || t.Sub != st.Sub {
				kept = append(kept, t)
			}
		}
		p.list = kept
	}
	p.list = append(p.list, st)
	return st, false
}

// acquire 取第一个可用 token（存活、非冷却、未过期）；没有可用则返回 false。
func (p *Pool) acquire() (*tokenState, bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	now := time.Now()
	for _, t := range p.list {
		if t.usable(now) {
			return t, true
		}
	}
	return nil, false
}

// markDead 标死（401/403/412：token 失效，需要重新从浏览器提取）。
func (p *Pool) markDead(t *tokenState) {
	if t == nil {
		return
	}
	p.mu.Lock()
	t.Dead = true
	p.mu.Unlock()
}

// markCooldown 冷却（402/429：额度/限流，换下一个 token）。
func (p *Pool) markCooldown(t *tokenState) {
	if t == nil {
		return
	}
	p.mu.Lock()
	t.CooldownUntil = time.Now().Add(p.cooldown)
	p.mu.Unlock()
}

// incCalls 记一次成功调用。
func (p *Pool) incCalls(t *tokenState) {
	if t == nil {
		return
	}
	p.mu.Lock()
	t.Calls++
	p.mu.Unlock()
}

// snapshot 返回脱敏状态（只给序号/死活/冷却/过期时间/调用数，绝不含 token 本体）。
func (p *Pool) snapshot() []map[string]any {
	p.mu.Lock()
	defer p.mu.Unlock()
	now := time.Now()
	out := make([]map[string]any, 0, len(p.list))
	for i, t := range p.list {
		item := map[string]any{
			"index":     i,
			"dead":      t.Dead,
			"calls":     t.Calls,
			"available": t.usable(now),
		}
		if t.CooldownUntil.After(now) {
			item["cooldown_until"] = t.CooldownUntil.Format(time.RFC3339)
		}
		if t.HasExp {
			item["exp"] = t.Exp.Format(time.RFC3339)
			item["expired"] = now.After(t.Exp)
			item["days_left"] = daysLeft(t.Exp, now) // 甲板账号行要用（到期前 7~30 天）
		}
		out = append(out, item)
	}
	return out
}

// daysLeft 返回距 exp 的剩余天数（向上取整：不足一天也算 1 天；已过期给负数）。
func daysLeft(exp, now time.Time) int {
	d := exp.Sub(now)
	if d > 0 && d < 24*time.Hour {
		return 1 // 剩 0.5 天显示「剩 1 天」，不显示「剩 0 天」
	}
	if d <= 0 {
		return -1 // 已过期：甲板只看「已过期」三态，负数取 -1 即可
	}
	return int((d + 24*time.Hour - 1) / (24 * time.Hour))
}

// maxDaysLeft 返回池中所有带 exp 的 token 里最大的剩余天数（无 exp 或无 token 返回 false）。
func (p *Pool) maxDaysLeft() (int, bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	now := time.Now()
	best, ok := 0, false
	for _, t := range p.list {
		if !t.HasExp {
			continue
		}
		d := daysLeft(t.Exp, now)
		if !ok || d > best {
			best, ok = d, true
		}
	}
	return best, ok
}
