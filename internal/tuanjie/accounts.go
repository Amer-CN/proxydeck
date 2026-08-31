// accounts.go —— 团结多账号池（学群友 Codely Relay 的 AccountManager）。
// accounts.json 与 exe 同目录；文件不存在或空列表时回退单账号模式
// （读 ~/.codely-cli 登录态），行为与旧版完全一致。
package tuanjie

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"log"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"
)

// Account 是账号池里的一个团结账号。
type Account struct {
	UserID         string    `json:"user_id"`
	Username       string    `json:"username,omitempty"`
	OrgID          string    `json:"org_id,omitempty"`
	AccessToken    string    `json:"access_token"`
	Source         string    `json:"source,omitempty"` // "pool"=入池账号（默认）；"local"=本地客户端（AccessToken 恒空，实时读 oauth_creds.json）
	LastUsed       string    `json:"last_used,omitempty"`
	UseCount       int64     `json:"use_count,omitempty"`
	Enabled        bool      `json:"enabled"`
	HasGLM53       bool      `json:"has_glm53"`
	BudgetExceeded bool      `json:"budget_exceeded"`
	BudgetResetAt  time.Time `json:"budget_reset_at,omitempty"` // 402 时记录的 5h 窗口重置时刻（自动恢复用）
	Models         []string  `json:"models,omitempty"`          // 该账号认领的模型列表（精确匹配）；空 = 兜底接所有未被认领的模型
}

// AccountStatus 是给前端的账号摘要（不含 token 本体）。
type AccountStatus struct {
	UserID         string  `json:"user_id"`
	Username       string  `json:"username"`
	OrgID          string  `json:"org_id"`
	Enabled        bool    `json:"enabled"`
	UseCount       int64   `json:"use_count"`
	LastUsed       string  `json:"last_used"`
	TokenExpires   string  `json:"token_expires"`
	TokenRemainHrs float64 `json:"token_remaining_hours"`
	HasGLM53       bool    `json:"has_glm53"`
	BudgetExceeded bool    `json:"budget_exceeded"`
	Inflight       int      `json:"inflight"`
	Source         string   `json:"source,omitempty"`
	Models         []string `json:"models"`
}

// AccountPool 管理多账号：轮询选号、402 禁用、GLM 资格路由、负载感知。
type AccountPool struct {
	mu       sync.Mutex
	accounts []*Account
	index    int
	// loads：user_id → 进行中请求数（负载感知选号用，与 RequestRegistry 联动）
	loads map[string]int
}

func accountsFilePath() string { return filepath.Join(exeDirForAccounts(), "tuanjie-accounts.json") }

// NewAccountPool 从 tuanjie-accounts.json 加载账号池（文件缺失 = 空池 = 单账号模式）。
func NewAccountPool() *AccountPool {
	p := &AccountPool{loads: map[string]int{}}
	p.reload()
	p.EnsureLocalAccount()
	return p
}

func (p *AccountPool) reload() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.accounts = nil
	b, err := os.ReadFile(accountsFilePath())
	if err != nil {
		return
	}
	var data struct {
		Accounts []*Account `json:"accounts"`
	}
	if json.Unmarshal(b, &data) != nil {
		return
	}
	for _, a := range data.Accounts {
		if a == nil || a.UserID == "" {
			continue
		}
		// 本地账号（source=local）AccessToken 恒空串（实时读 oauth_creds.json），不能丢弃；
		// 其余账号必须有非空 token 才入池。
		if a.Source != "local" && a.AccessToken == "" {
			continue
		}
		p.accounts = append(p.accounts, a)
	}
}

func (p *AccountPool) saveLocked() {
	out := struct {
		Accounts []*Account `json:"accounts"`
	}{Accounts: p.accounts}
	b, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return
	}
	_ = os.WriteFile(accountsFilePath(), b, 0o600)
}

// Size 返回账号总数（0 = 单账号模式）。
func (p *AccountPool) Size() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.accounts)
}

// Pick 按以下优先级选号（学群友 get_next）：
//  1. 只在 enabled 且非 budget_exceeded 的账号里选
//  2. GLM 系模型 → 只在 has_glm53 的账号里选（有得选时）
//  3. 负载感知：进行中请求最少的账号优先（同负载保持轮询顺序）
//  4. 轮询推进；全员超额时退回轮询起点
func (p *AccountPool) Pick(model string) *Account {
	p.mu.Lock()
	defer p.mu.Unlock()
	if len(p.accounts) == 0 {
		return nil
	}
	enabled := make([]*Account, 0, len(p.accounts))
	for _, a := range p.accounts {
		if a.Enabled && !a.BudgetExceeded {
			enabled = append(enabled, a)
		}
	}
	if len(enabled) == 0 {
		return nil
	}
	// 模型路由：精确匹配账号 Models 列表；空列表 = 兜底接未认领模型
	if model != "" {
		var claimers, fallbacks []*Account
		for _, a := range enabled {
			if len(a.Models) == 0 {
				fallbacks = append(fallbacks, a)
				continue
			}
			for _, m := range a.Models {
				if m == model {
					claimers = append(claimers, a)
					break
				}
			}
		}
		if len(claimers) > 0 {
			enabled = claimers
		} else if len(fallbacks) > 0 {
			enabled = fallbacks
		}
	}
	// GLM 资格路由
	if isGLMModel(model) {
		glm := make([]*Account, 0, len(enabled))
		for _, a := range enabled {
			if a.HasGLM53 {
				glm = append(glm, a)
			}
		}
		if len(glm) > 0 {
			enabled = glm
		}
	}
	// 负载感知：最低负载优先
	minLoad := -1
	for _, a := range enabled {
		l := p.loads[a.UserID]
		if minLoad < 0 || l < minLoad {
			minLoad = l
		}
	}
	if minLoad > 0 {
		light := make([]*Account, 0, len(enabled))
		for _, a := range enabled {
			if p.loads[a.UserID] <= minLoad {
				light = append(light, a)
			}
		}
		if len(light) > 0 {
			enabled = light
		}
	}
	// 轮询
	for i := 0; i < len(enabled); i++ {
		idx := (p.index + i) % len(enabled)
		a := enabled[idx]
		p.index = (idx + 1) % len(enabled)
		a.LastUsed = time.Now().Format("2006-01-02 15:04:05")
		a.UseCount++
		p.saveLocked()
		return a
	}
	return nil
}

// effectiveAccessToken 返回账号实际使用的 access_token：本地账号实时读 oauth_creds.json
// （桌面端切号自动跟随、不因快照过期失效），池账号用入池快照。
func (a *Account) effectiveAccessToken() (string, error) {
	if a.Source == "local" {
		return loadAccessToken()
	}
	return a.AccessToken, nil
}

// PickWithToken 选号并解析可用 token：本地账号实时读 token（读失败换下一账号，最多试池大小次），
// 池账号用快照。无可选账号或全部本地号读不到 token 时返回 (nil,"")。
func (p *AccountPool) PickWithToken(model string) (*Account, string) {
	n := p.Size()
	for i := 0; i < n; i++ {
		a := p.Pick(model)
		if a == nil {
			return nil, ""
		}
		tok, err := a.effectiveAccessToken()
		if err == nil {
			return a, tok
		}
		log.Printf("[tuanjie] 账号 %s 取 token 失败：%v，跳过换下一账号", a.UserID, err)
	}
	return nil, ""
}

// MarkBudgetExceededAt 标记 402 并记录预计重置时刻（resetAt 零值 = 未知，
// 不参与自动恢复）。resetAt 通常取官网 5h 窗口 PeriodEnd，到点由
// ResumeBudgetLoop 复查额度后决定是否自动清标记。
func (p *AccountPool) MarkBudgetExceededAt(userID string, resetAt time.Time) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UserID == userID {
			a.BudgetExceeded = true
			a.BudgetResetAt = resetAt
			if a.Source == "local" {
				log.Printf("[tuanjie] 本地账号 %s 命中预算超额（BudgetExceeded=true 置 UI 标记，不物理删除）", userID)
			}
			p.saveLocked()
			return
		}
	}
}

// MarkBudgetExceeded 标记账号 402 并持久化（选号自动绕过）。
func (p *AccountPool) MarkBudgetExceeded(userID string) {
	p.MarkBudgetExceededAt(userID, time.Time{})
}

// ResumeBudgetLoop 402 自动恢复轮询：每 1 分钟扫一遍池，找出 BudgetExceeded
// 且预计重置时刻（BudgetResetAt，402 时按官网 5h 窗口 PeriodEnd 记录）已过的
// 账号，经 queryQuota 复查官网额度——已回则清标记重新入池（月窗耗尽的误恢复
// 由 remaining<=0 分支拦下：顺延到 nextReset 再查）；查询失败顺延 5 分钟重试。
// queryQuota 由调用方注入（Server 提供 token 解析 + 官网查询）；锁内不调
// queryQuota——先收集候选、解锁再查、再加锁写回。ctx.Done() 退出。
func (p *AccountPool) ResumeBudgetLoop(ctx context.Context, queryQuota func(userID string) (remaining float64, nextReset time.Time, ok bool)) {
	ticker := time.NewTicker(time.Minute)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			p.resumeScanOnce(queryQuota)
		}
	}
}

// resumeScanOnce ResumeBudgetLoop 的单轮扫描体（独立出来便于单测）。
func (p *AccountPool) resumeScanOnce(queryQuota func(userID string) (remaining float64, nextReset time.Time, ok bool)) {
	if queryQuota == nil {
		return
	}
	now := time.Now()
	p.mu.Lock()
	var ids []string
	for _, a := range p.accounts {
		// 恢复只看 BudgetExceeded（不要求 Enabled）：手动停用的账号同样被复查，
		// 但恢复只清标记，绝不把 Enabled 置回 true——是否入池仍由 Pick 的
		// Enabled && !BudgetExceeded 过滤决定。
		if a.BudgetExceeded && !a.BudgetResetAt.IsZero() && now.After(a.BudgetResetAt) {
			ids = append(ids, a.UserID)
		}
	}
	p.mu.Unlock()
	for _, uid := range ids {
		remaining, nextReset, ok := queryQuota(uid)
		now := time.Now()
		p.mu.Lock()
		for _, a := range p.accounts {
			if a.UserID != uid || !a.BudgetExceeded {
				continue // 期间已被 Toggle 清标记：跳过
			}
			switch {
			case ok && remaining > 0:
				a.BudgetExceeded = false
				a.BudgetResetAt = time.Time{}
				p.saveLocked()
				log.Printf("[tuanjie] 账号 %s 额度已恢复（5h 窗口重置），自动恢复轮询", uid)
			case ok:
				// remaining<=0：月窗耗尽等场景额度未回——顺延到下一个重置时刻继续等
				if !nextReset.IsZero() {
					a.BudgetResetAt = nextReset
				}
				if a.BudgetResetAt.IsZero() || !a.BudgetResetAt.After(now) {
					a.BudgetResetAt = now.Add(5 * time.Minute) // 没拿到新时刻：+5min 防死循环快扫
				}
				p.saveLocked()
				log.Printf("[tuanjie] 账号 %s 复查额度仍未恢复（剩余 %.1f），顺延至 %s 再查", uid, remaining, a.BudgetResetAt.Format("2006-01-02 15:04:05"))
			default:
				a.BudgetResetAt = now.Add(5 * time.Minute)
				p.saveLocked()
				log.Printf("[tuanjie] 账号 %s 复查额度查询失败，顺延 5 分钟重试", uid)
			}
			break
		}
		p.mu.Unlock()
	}
}

// Inc/Dec 负载计数（RequestRegistry 联动）。
func (p *AccountPool) IncLoad(userID string) {
	p.mu.Lock()
	p.loads[userID]++
	p.mu.Unlock()
}
func (p *AccountPool) DecLoad(userID string) {
	p.mu.Lock()
	if p.loads[userID] > 0 {
		p.loads[userID]--
	}
	p.mu.Unlock()
}

// Status 返回全部账号摘要（含 JWT 过期解析）。
func (p *AccountPool) Status() []AccountStatus {
	p.mu.Lock()
	defer p.mu.Unlock()
	out := make([]AccountStatus, 0, len(p.accounts))
	for _, a := range p.accounts {
		exp, ok := jwtExpiry(a.AccessToken)
		remain := 0.0
		expires := "未知"
		if ok {
			remain = time.Until(exp).Hours()
			if remain < 0 {
				remain = 0
			}
			expires = exp.Format("2006-01-02 15:04")
		}
		out = append(out, AccountStatus{
			UserID:         a.UserID,
			Username:       a.Username,
			OrgID:          a.OrgID,
			Enabled:        a.Enabled,
			UseCount:       a.UseCount,
			LastUsed:       a.LastUsed,
			TokenExpires:   expires,
			TokenRemainHrs: float64(int(remain*10)) / 10,
			HasGLM53:       a.HasGLM53,
			BudgetExceeded: a.BudgetExceeded,
			Inflight:       p.loads[a.UserID],
			Source:         a.Source,
			Models:         a.Models,
		})
	}
	return out
}

// Add 新增账号（user_id 重复返回 false）。
func (p *AccountPool) Add(userID, token, username, orgID string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UserID == userID {
			return false
		}
	}
	p.accounts = append(p.accounts, &Account{
		UserID: userID, AccessToken: token, Username: username, OrgID: orgID,
		Enabled: true, HasGLM53: true,
	})
	p.saveLocked()
	return true
}

// EnsureLocalAccount 把本地客户端（桌面端登录态）纳入账号池使其参与轮询。
// 本地账号 AccessToken 恒为空串（每次用到时实时 loadAccessToken），Source=local。
// 读 loadAccessToken()+jwtSub 成功则确保池里有 {UserID:<sub>, Source:local, Enabled:true,
// HasGLM53:true, AccessToken:""} 记录（无则加；sub 变了就更新该条 UserID）；
// 本地未登录/解析失败返回 false。
func (p *AccountPool) EnsureLocalAccount() bool {
	token, err := loadAccessToken()
	if err != nil {
		return false
	}
	sub := jwtSub(token)
	if sub == "" {
		return false
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.Source == "local" {
			if a.UserID != sub {
				a.UserID = sub
				p.saveLocked()
			}
			return true
		}
	}
	p.accounts = append(p.accounts, &Account{
		UserID: sub, Source: "local", Enabled: true, HasGLM53: true,
	})
	p.saveLocked()
	return true
}

// Remove 删除账号。
func (p *AccountPool) Remove(userID string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	before := len(p.accounts)
	kept := p.accounts[:0]
	for _, a := range p.accounts {
		if a.UserID != userID {
			kept = append(kept, a)
		}
	}
	p.accounts = kept
	if len(p.accounts) != before {
		p.saveLocked()
		return true
	}
	return false
}

// Toggle 启停账号（重新启用时清 402 标记）。
func (p *AccountPool) Toggle(userID string, enabled bool) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UserID == userID {
			a.Enabled = enabled
			if enabled {
				a.BudgetExceeded = false
			}
			p.saveLocked()
			return true
		}
	}
	return false
}

// SetGLM 标记 GLM 资格。
func (p *AccountPool) SetGLM(userID string, has bool) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UserID == userID {
			a.HasGLM53 = has
			p.saveLocked()
			return true
		}
	}
	return false
}

// SetModels 设置账号认领的模型列表（精确匹配）；空切片等价于兜底。
func (p *AccountPool) SetModels(userID string, models []string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UserID == userID {
			if len(models) == 0 {
				a.Models = nil
			} else {
				a.Models = append([]string(nil), models...)
			}
			p.saveLocked()
			return true
		}
	}
	return false
}

// SortedUserIDs 返回排序后的 user_id 列表（探针遍历用）。
func (p *AccountPool) SortedUserIDs() []string {
	p.mu.Lock()
	defer p.mu.Unlock()
	ids := make([]string, 0, len(p.accounts))
	for _, a := range p.accounts {
		ids = append(ids, a.UserID)
	}
	sort.Strings(ids)
	return ids
}

// Get 按 user_id 取账号（探针直连用）。
func (p *AccountPool) Get(userID string) *Account {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UserID == userID {
			return a
		}
	}
	return nil
}

// isGLMModel 判断是否 GLM 系模型（大小写不敏感）。
func isGLMModel(model string) bool {
	m := model
	for i := 0; i < len(m); i++ {
		c := m[i]
		if c >= 'A' && c <= 'Z' {
			m = m[:i] + string(rune(c-'A'+'a')) + m[i+1:]
		}
	}
	return len(m) >= 3 && m[:3] == "glm"
}

// jwtExpiry 解析 JWT 的 exp（不校验签名，只读 payload）。
func jwtExpiry(token string) (time.Time, bool) {
	parts := splitJWT(token)
	if len(parts) != 3 {
		return time.Time{}, false
	}
	payload, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return time.Time{}, false
	}
	var p struct {
		Exp int64 `json:"exp"`
	}
	if json.Unmarshal(payload, &p) != nil || p.Exp == 0 {
		return time.Time{}, false
	}
	return time.Unix(p.Exp, 0), true
}

func splitJWT(s string) []string {
	var out []string
	start := 0
	for i := 0; i < len(s); i++ {
		if s[i] == '.' {
			out = append(out, s[start:i])
			start = i + 1
		}
	}
	return append(out, s[start:])
}
