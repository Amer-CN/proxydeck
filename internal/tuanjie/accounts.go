// accounts.go —— 团结多账号池（学群友 Codely Relay 的 AccountManager）。
// accounts.json 与 exe 同目录；文件不存在或空列表时回退单账号模式
// （读 ~/.codely-cli 登录态），行为与旧版完全一致。
package tuanjie

import (
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"
)

// Account 是账号池里的一个团结账号。
type Account struct {
	UserID         string `json:"user_id"`
	Username       string `json:"username,omitempty"`
	OrgID          string `json:"org_id,omitempty"`
	AccessToken    string `json:"access_token"`
	LastUsed       string `json:"last_used,omitempty"`
	UseCount       int64  `json:"use_count,omitempty"`
	Enabled        bool   `json:"enabled"`
	HasGLM53       bool   `json:"has_glm53"`
	BudgetExceeded bool   `json:"budget_exceeded"`
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
	Inflight       int     `json:"inflight"`
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
		if a == nil || a.UserID == "" || a.AccessToken == "" {
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

// MarkBudgetExceeded 标记账号 402 并持久化（选号自动绕过）。
func (p *AccountPool) MarkBudgetExceeded(userID string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, a := range p.accounts {
		if a.UserID == userID {
			a.BudgetExceeded = true
			p.saveLocked()
			return
		}
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
