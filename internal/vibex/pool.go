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
		if v == "" || p.has(v) {
			continue
		}
		st := &tokenState{Value: v}
		if info := decodeJWT(v); info != nil {
			st.Exp, st.HasExp = info.Exp, info.hasExp()
		}
		p.list = append(p.list, st)
	}
	return p
}

func (p *Pool) has(v string) bool {
	for _, t := range p.list {
		if t.Value == v {
			return true
		}
	}
	return false
}

// Len 返回池中 token 数量（不区分死活）。
func (p *Pool) Len() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.list)
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
		}
		out = append(out, item)
	}
	return out
}
