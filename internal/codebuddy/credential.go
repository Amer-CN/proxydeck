// Package codebuddy 把 CodeBuddy / WorkBuddy 桌面端的登录态转成本地
// OpenAI 兼容 API（Go 重写版，协议与 plugins/codebuddy2api 一致）。
//
// 流程：
//  1. 读取 CodeBuddy 桌面端 auth 文件（*.info，取 mtime 最新的一个）
//  2. 用 accessToken + X-User-Id / X-Enterprise-Id 等头直连
//     copilot.tencent.com/v2/chat/completions（后端已是 OpenAI 协议，原生 tools）
//  3. token 临近过期自动调 /v2/plugin/auth/token/refresh 刷新并回写 auth 文件
package codebuddy

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

const (
	defaultDomain = "www.codebuddy.cn"
)

// Credential 管理桌面端凭据：读取、缓存、过期刷新、回写。
type Credential struct {
	mu     sync.Mutex
	region Region
	path   string
	cached map[string]any
	mtime  time.Time
	client *http.Client
}

// authAccount 对应 auth 文件里的顶层结构（宽松解析）。
type sessionInfo struct {
	Auth struct {
		AccessToken     string `json:"accessToken"`
		RefreshToken    string `json:"refreshToken"`
		Domain          string `json:"domain"`
		ExpiresAt       int64  `json:"expiresAt"`
		RefreshExpiresAt int64 `json:"refreshExpiresAt"`
	} `json:"auth"`
	Account struct {
		UID           string `json:"uid"`
		Nickname      string `json:"nickname"`
		EnterpriseID  string `json:"enterpriseId"`
		EnterpriseName string `json:"enterpriseName"`
	} `json:"account"`
}

// findAuthDirs 返回候选 auth 目录（Windows/macOS/Linux，与 Python 版一致）。
func findAuthDirs() []string {
	if d := os.Getenv("CODEBUDDY_AUTH_DIR"); d != "" {
		return []string{d}
	}
	home, _ := os.UserHomeDir()
	var dirs []string
	// Windows: %LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth
	if la := os.Getenv("LOCALAPPDATA"); la != "" {
		dirs = append(dirs, filepath.Join(la, "CodeBuddyExtension", "Data", "Public", "auth"))
	} else if home != "" {
		dirs = append(dirs, filepath.Join(home, "AppData", "Local", "CodeBuddyExtension", "Data", "Public", "auth"))
	}
	// macOS
	dirs = append(dirs, filepath.Join(home, "Library", "Application Support", "CodeBuddyExtension", "Data", "Public", "auth"))
	// Linux (XDG)
	xdg := os.Getenv("XDG_DATA_HOME")
	if xdg == "" && home != "" {
		xdg = filepath.Join(home, ".local", "share")
	}
	if xdg != "" {
		dirs = append(dirs, filepath.Join(xdg, "CodeBuddyExtension", "Data", "Public", "auth"))
	}
	return dirs
}

// FindAuthFile 返回最新登录的 auth 文件（按 mtime；目录里可能残留多个历史登录态，
// 旧 token 可能已被吊销，必须用最新的）。找不到返回空。
//
// 2026-09-11 region 修复：国际版登录会往同一目录写 workbuddy-desktop-ai.info，
// 只按 mtime 取最新会让国内 8787 拿国际 token 打 copilot.tencent.com（反之亦然）。
// 必须先读每个 .info 的 auth.domain 按 region 过滤，再在命中的文件里取 mtime
// 最新；没有对应区域的文件就返回空（如实报错，不许回退到另一区域的凭据）。
func FindAuthFile() string {
	return FindAuthFileForRegion(RegionCN)
}

// FindAuthFileForRegion 同 FindAuthFile，但按区域过滤 auth.domain
//（CN → 含 codebuddy.cn；INTL → 含 workbuddy.ai）。
func FindAuthFileForRegion(region Region) string {
	suffix := regionConfigs[region].DomainSuffix
	for _, d := range findAuthDirs() {
		ents, err := os.ReadDir(d)
		if err != nil {
			continue
		}
		type fi struct {
			path  string
			mtime time.Time
		}
		var files []fi
		for _, e := range ents {
			if e.IsDir() || !strings.HasSuffix(e.Name(), ".info") {
				continue
			}
			info, err := e.Info()
			if err != nil {
				continue
			}
			p := filepath.Join(d, e.Name())
			if !authFileDomainMatches(p, suffix) {
				continue
			}
			files = append(files, fi{p, info.ModTime()})
		}
		if len(files) > 0 {
			sort.Slice(files, func(i, j int) bool { return files[i].mtime.After(files[j].mtime) })
			return files[0].path
		}
	}
	return ""
}

// authFileDomainMatches 读 auth 文件的 auth.domain 字段判断是否命中区域后缀。
// 只取字段值，token 等其余内容不读入内存、不落任何输出。
func authFileDomainMatches(path, suffix string) bool {
	b, err := os.ReadFile(path)
	if err != nil {
		return false
	}
	var probe struct {
		Auth struct {
			Domain string `json:"domain"`
		} `json:"auth"`
	}
	if json.Unmarshal(b, &probe) != nil {
		return false
	}
	return containsDomain(probe.Auth.Domain, suffix)
}

// NewCredential 创建凭据管理器（path 为空则按 region 自动查找）。
func NewCredential(path string, region Region) *Credential {
	if path == "" {
		path = FindAuthFileForRegion(region)
	}
	return &Credential{
		region: region,
		path:   path,
		client: &http.Client{Timeout: 20 * time.Second},
	}
}

// loadIfStale 文件 mtime 变了（桌面端外部刷新过）→ 重新加载。
func (c *Credential) loadIfStale() error {
	st, err := os.Stat(c.path)
	if err != nil {
		return fmt.Errorf("无法读取 auth 文件 %s: %w", c.path, err)
	}
	if c.cached == nil || !st.ModTime().Equal(c.mtime) {
		b, err := os.ReadFile(c.path)
		if err != nil {
			return err
		}
		var m map[string]any
		if err := json.Unmarshal(b, &m); err != nil {
			return fmt.Errorf("auth 文件格式错误: %w", err)
		}
		c.cached = m
		c.mtime = st.ModTime()
	}
	return nil
}

// session 从缓存的 map 中解析结构化信息。
func (c *Credential) session() (*sessionInfo, error) {
	b, _ := json.Marshal(c.cached)
	var s sessionInfo
	if err := json.Unmarshal(b, &s); err != nil {
		return nil, fmt.Errorf("auth 文件结构异常: %w", err)
	}
	return &s, nil
}

// expired 提前 60s 判定过期。
func (c *Credential) expired(s *sessionInfo) bool {
	return time.Now().UnixMilli() >= s.Auth.ExpiresAt-60_000
}

// refresh 调后端刷新 token，写回 auth 文件与缓存。
func (c *Credential) refresh(ctx context.Context, s *sessionInfo) error {
	h := c.buildHeaders(s)
	h.Set("X-Refresh-Token", s.Auth.RefreshToken)
	h.Set("X-Auth-Refresh-Source", "plugin")

	req, err := http.NewRequestWithContext(ctx, http.MethodPost,
		regionConfigs[c.region].BaseURL+"/v2/plugin/auth/token/refresh", bytes.NewReader([]byte("{}")))
	if err != nil {
		return err
	}
	req.Header = h
	resp, err := c.client.Do(req)
	if err != nil {
		return fmt.Errorf("刷新 token 网络失败: %w", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	var out struct {
		Code int    `json:"code"`
		Msg  string `json:"msg"`
		Data json.RawMessage `json:"data"`
	}
	if err := json.Unmarshal(body, &out); err != nil || out.Code != 0 || len(out.Data) == 0 {
		return fmt.Errorf("刷新 token 失败(code=%d): %s", out.Code, truncate(string(out.Data), 200))
	}
	var newAuth map[string]any
	if err := json.Unmarshal(out.Data, &newAuth); err != nil {
		return fmt.Errorf("刷新响应异常: %w", err)
	}
	// 继承部分字段 + 计算 expiresAt（若后端没直接给）
	if newAuth["domain"] == nil && s.Auth.Domain != "" {
		newAuth["domain"] = s.Auth.Domain
	}
	newAuth["lastRefreshTime"] = time.Now().UnixMilli()
	fillExpires(newAuth, s.Auth.ExpiresAt)
	c.cached["auth"] = newAuth

	// 原子写回
	b, _ := json.MarshalIndent(c.cached, "", "  ")
	tmp := c.path + ".tmp"
	if err := os.WriteFile(tmp, b, 0o644); err != nil {
		return err
	}
	if err := os.Rename(tmp, c.path); err != nil {
		return err
	}
	if st, err := os.Stat(c.path); err == nil {
		c.mtime = st.ModTime()
	}
	return nil
}

// fillExpires 缺 expiresAt 时按 expiresIn 推算（刷新失败保护）。
func fillExpires(auth map[string]any, oldExpiresAt int64) {
	now := time.Now().UnixMilli()
	if auth["expiresAt"] == nil {
		if v, ok := toFloat(auth["expiresIn"]); ok {
			auth["expiresAt"] = now + int64(v*1000)
		} else if oldExpiresAt > 0 {
			auth["expiresAt"] = oldExpiresAt
		}
	}
	if auth["refreshExpiresAt"] == nil {
		if v, ok := toFloat(auth["refreshExpiresIn"]); ok {
			auth["refreshExpiresAt"] = now + int64(v*1000)
		}
	}
}

func toFloat(v any) (float64, bool) {
	f, ok := v.(float64)
	return f, ok
}

// buildHeaders 构造后端请求头。
func (c *Credential) buildHeaders(s *sessionInfo) http.Header {
	domain := s.Auth.Domain
	if domain == "" {
		domain = defaultDomain
	}
	h := http.Header{}
	h.Set("Content-Type", "application/json")
	h.Set("Accept", "application/json")
	h.Set("Authorization", "Bearer "+s.Auth.AccessToken)
	h.Set("X-User-Id", s.Account.UID)
	h.Set("X-Enterprise-Id", s.Account.EnterpriseID)
	h.Set("X-Tenant-Id", s.Account.EnterpriseID)
	h.Set("X-Domain", domain)
	// User-Agent 按区域取：CN 沿用原常量值；INTL 用官方 CLI 形（实测生效）
	h.Set("User-Agent", regionConfigs[c.region].UserAgent)
	return h
}

// Headers 返回带最新 token 的后端请求头；必要时先刷新（线程安全）。
// INTL 特化（NoRefresh）：国际版是独立 Keycloak realm，代理侧刷新会被拒——
// 每次调用重读 auth 文件（桌面端自己更新 token），过期就如实报错。
func (c *Credential) Headers(ctx context.Context) (http.Header, error) {
	return c.headersFor(ctx, "")
}

// HeadersFor 按指定 auth 文件取请求头（账号池选号后切换凭据）：path 与当前
// 不同时切换并清 cached/mtime，复用 loadIfStale/refresh 全套现有逻辑；path
// 为空 = 当前文件（与 Headers 完全一致）。简报给出的两种最小改法里选了
// 「Headers 接受 path 参数」这一种：切换与读取必须在同一次加锁内完成，
// 若拆成 SetPath + 两次加锁的 Headers，并发请求各持不同账号时会互相串号。
func (c *Credential) HeadersFor(ctx context.Context, path string) (http.Header, error) {
	return c.headersFor(ctx, path)
}

// headersFor Headers/HeadersFor 的公共实现（单锁内完成切文件 + 读取 + 刷新）。
func (c *Credential) headersFor(ctx context.Context, path string) (http.Header, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if path != "" && path != c.path {
		c.path = path
		c.cached = nil
		c.mtime = time.Time{}
	}
	if err := c.loadIfStale(); err != nil {
		return nil, err
	}
	s, err := c.session()
	if err != nil {
		return nil, err
	}
	if c.expired(s) {
		if regionConfigs[c.region].NoRefresh {
			return nil, fmt.Errorf("国际版 token 已过期（桌面端会自动续期，请打开 WorkBuddy 桌面端刷新登录态）")
		}
		if err := c.refresh(ctx, s); err != nil {
			return nil, err
		}
		s, err = c.session()
		if err != nil {
			return nil, err
		}
	}
	return c.buildHeaders(s), nil
}

// Summary 返回诊断信息（uid/昵称/企业/token 过期状态）。
func (c *Credential) Summary() map[string]any {
	c.mu.Lock()
	defer c.mu.Unlock()
	if err := c.loadIfStale(); err != nil {
		return map[string]any{"error": err.Error()}
	}
	s, err := c.session()
	if err != nil {
		return map[string]any{"error": err.Error()}
	}
	return map[string]any{
		"uid":             s.Account.UID,
		"nickname":        s.Account.Nickname,
		"enterpriseName":  s.Account.EnterpriseName,
		"token_expires_at": s.Auth.ExpiresAt,
		"token_expired":   c.expired(s),
		"auth_file":       c.path,
	}
}

// Path 返回当前使用的 auth 文件路径。
func (c *Credential) Path() string { return c.path }

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
