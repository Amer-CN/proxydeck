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
	"log"
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
	// decAccess/decRefresh 为已解密的明文（loadIfStale 随文件重载时解析一次；
	// 明文原文件直接取，$wbEncrypted 信封走 atrest.go 解；凭据原文不出日志）。
	decAccess  string
	decRefresh string
	client *http.Client
}

// flexString 兼容新版桌面端把 account.nickname 写成加密对象（{"$wbEncrypted":1,
// "envelope":"..."}）的写法：字符串原样收；对象/数组/数字/null 一律收成空串——
// 昵称只是显示名，绝不能因它解析失败丢掉整个登录态。
// 实测（2026-10-12）：双区今日新文件 nickname 均为加密对象，旧解析（string）
// 让 health 双区同报"auth 文件结构异常"、池重扫跳过新文件。
type flexString string

// UnmarshalJSON 字符串原样收，非字符串一律置空且不报错。
func (f *flexString) UnmarshalJSON(b []byte) error {
	var s string
	if err := json.Unmarshal(b, &s); err == nil {
		*f = flexString(s)
		return nil
	}
	*f = ""
	return nil
}

// wbToken 登录态 token 字段：旧版明文字符串原样收；新版 $wbEncrypted 信封
// 存 raw、解密延后到 Resolve（解析期绝不调 exe，假信封单测不受本机环境影响）。
// 昵称仍用 flexString（显示名置空即可，绝不解密）。
type wbToken struct {
	raw       json.RawMessage
	plain     string
	encrypted bool
}

// UnmarshalJSON 字符串原样收；信封记形态；其余（数字/null/数组）置空不报错。
func (t *wbToken) UnmarshalJSON(b []byte) error {
	t.raw = append(t.raw[:0], b...)
	var s string
	if err := json.Unmarshal(b, &s); err == nil {
		t.plain, t.encrypted = s, false
		return nil
	}
	var v any
	if err := json.Unmarshal(b, &v); err == nil && isEncryptedField(v) {
		t.plain, t.encrypted = "", true
		return nil
	}
	t.plain, t.encrypted = "", false
	return nil
}

// IsEncrypted 是否为 $wbEncrypted 信封形态。
func (t *wbToken) IsEncrypted() bool { return t.encrypted }

// Resolve 取明文（明文直接回；信封走 at-rest 解密，失败如实错、不带原文）。
func (t *wbToken) Resolve(region Region) (string, error) {
	if !t.encrypted {
		return t.plain, nil
	}
	return decryptEnvelopeRaw(t.raw, region)
}
// sessionInfo 对应 auth 文件里的顶层结构（宽松解析）。
type sessionInfo struct {
	Auth struct {
		// AccessToken/RefreshToken：旧版明文字符串；新版桌面端（2026-10-12
		// 实测 WorkBuddy 5.7.7 / AI 5.7.6）写加密对象 {"$wbEncrypted":1,...}，
		// wbToken 收形态、明文经 Resolve 走 at-rest 解密；是否可用由
		// authTokenUsable 判定（明文非空或信封可解密）。
		AccessToken     wbToken `json:"accessToken"`
		RefreshToken    wbToken `json:"refreshToken"`
		Domain          string `json:"domain"`
		ExpiresAt       int64  `json:"expiresAt"`
		RefreshExpiresAt int64 `json:"refreshExpiresAt"`
	} `json:"auth"`
	Account struct {
		UID           string `json:"uid"`
		Nickname      flexString `json:"nickname"`
		EnterpriseID  string `json:"enterpriseId"`
		EnterpriseName string `json:"enterpriseName"`
	} `json:"account"`
}

// authTokenUsable 该文件是否含可用 token：明文非空即用；$wbEncrypted 信封
// 能解密出非空明文即用（at-rest 解密，见 atrest.go）。只读字段值做匹配，
// 凭据原文不打印、不落日志。
func authTokenUsable(path string) bool {
	return authTokenUnusableReason(path) == ""
}

// authTokenUnusableReason 不可用时返回短原因（""=可用；扫描日志用，不带原文）。
func authTokenUnusableReason(path string) string {
	b, err := os.ReadFile(path)
	if err != nil {
		return "读文件失败"
	}
	var probe struct {
		Auth struct {
			AccessToken json.RawMessage `json:"accessToken"`
		} `json:"auth"`
	}
	if json.Unmarshal(b, &probe) != nil || len(probe.Auth.AccessToken) == 0 {
		return "auth.accessToken 缺失"
	}
	var s string
	if err := json.Unmarshal(probe.Auth.AccessToken, &s); err == nil {
		if s != "" {
			return ""
		}
		return "明文 token 为空"
	}
	var v any
	if err := json.Unmarshal(probe.Auth.AccessToken, &v); err != nil || !isEncryptedField(v) {
		return "token 形态未知"
	}
	// 信封形态：结构先验（坏信封直接判死，不触发取 key）；结构对才解密。
	if _, err := parseWbEnvelope(probe.Auth.AccessToken); err != nil {
		return "加密信封损坏(" + err.Error() + ")"
	}
	pt, err := decryptEnvelopeRaw(probe.Auth.AccessToken, regionOfAuth(b))
	if err != nil {
		return "加密 token 解密失败(" + err.Error() + ")"
	}
	if pt == "" {
		return "加密 token 解密后为空"
	}
	return ""
}

// regionOfAuth 按 auth.domain 定区域（解密候选 exe 排序用；未知按 CN——
// 解密以 envelope.keyId 匹配为准，排错序只多一次子进程，不误用）。
func regionOfAuth(b []byte) Region {
	var probe struct {
		Auth struct {
			Domain string `json:"domain"`
		} `json:"auth"`
	}
	if json.Unmarshal(b, &probe) == nil &&
		containsDomain(probe.Auth.Domain, regionConfigs[RegionINTL].DomainSuffix) {
		return RegionINTL
	}
	return RegionCN
}

// countEncryptedFiles 该区域下"可解析、区域对、但 token 不可用"的文件数
//（诊断用：区分"一个文件都没有"和"有文件但全解不出来"；可解密的加密文件
// 已是可用文件，不计入）。
func countEncryptedFiles(region Region) int {
	suffix := regionConfigs[region].DomainSuffix
	n := 0
	for _, d := range findAuthDirs() {
		ents, err := os.ReadDir(d)
		if err != nil {
			continue
		}
		for _, e := range ents {
			if e.IsDir() || !strings.HasSuffix(e.Name(), ".info") {
				continue
			}
			p := filepath.Join(d, e.Name())
			if authFileDomainMatches(p, suffix) && !authTokenUsable(p) {
				n++
			}
		}
	}
	return n
}

// findAuthDirs 返回候选 auth 目录（Windows/macOS/Linux，与 Python 版一致）。
// 优先级：CODEBUDDY_AUTH_DIR 环境变量（逃生口，命中即唯一）> 本机多候选目录。
// 多候选：第 1 位沿用既有默认（%LOCALAPPDATA%\CodeBuddyExtension\...）；其后沿用
// macOS/Linux 位。2026-10-12 本机实测：注册表卸载表两条目（WorkBuddy 5.7.7 /
// WorkBuddy AI 5.7.6）InstallLocation 均空，auth 实际仍在默认目录（双区当日均有
// 新文件），注册表推导指不到 auth，故不采用；漫游位（%APPDATA%\...）本机根本
// 不存在（.work/roaming_verdict.txt），无命中实证，不保留，只做多候选目录。
func findAuthDirs() []string {
	if d := os.Getenv("CODEBUDDY_AUTH_DIR"); d != "" {
		return []string{d}
	}
	home, _ := os.UserHomeDir()
	return candidateAuthDirs(os.Getenv("LOCALAPPDATA"), home, os.Getenv("XDG_DATA_HOME"))
}

// candidateAuthDirs 纯函数：按传入的根拼多候选目录（去重、去空；单测喂假根，
// 不碰真实环境）。
func candidateAuthDirs(localAppData, home, xdg string) []string {
	var dirs []string
	add := func(d string) {
		if d == "" {
			return
		}
		for _, e := range dirs {
			if e == d {
				return
			}
		}
		dirs = append(dirs, d)
	}
	if localAppData == "" && home != "" {
		localAppData = filepath.Join(home, "AppData", "Local")
	}
	if localAppData != "" {
		add(filepath.Join(localAppData, "CodeBuddyExtension", "Data", "Public", "auth"))
	}
	if home != "" {
		add(filepath.Join(home, "Library", "Application Support", "CodeBuddyExtension", "Data", "Public", "auth"))
	}
	if xdg == "" && home != "" {
		xdg = filepath.Join(home, ".local", "share")
	}
	if xdg != "" {
		add(filepath.Join(xdg, "CodeBuddyExtension", "Data", "Public", "auth"))
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
// 可用性优先：只选 token 可用的文件（明文非空或 $wbEncrypted 信封可解密）；
// 同目录内按 mtime 取最新（目录优先级保持旧语义：默认目录优先）。
// 零可用时如实区分"一个文件都没有"与"有文件但全解不出来"，日志写清去哪找过。
func FindAuthFileForRegion(region Region) string {
	suffix := regionConfigs[region].DomainSuffix
	dirs := findAuthDirs()
	encrypted := 0
	for _, d := range dirs {
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
			if !authTokenUsable(p) {
				encrypted++
				log.Printf("[codebuddy] 跳过不可用登录文件 %s（%s，用最后可用文件）",
					e.Name(), authTokenUnusableReason(p))
				continue
			}
			files = append(files, fi{p, info.ModTime()})
		}
		if len(files) > 0 {
			sort.Slice(files, func(i, j int) bool { return files[i].mtime.After(files[j].mtime) })
			return files[0].path
		}
	}
	if encrypted > 0 {
		log.Printf("[codebuddy] %s 有 %d 个登录文件但 token 均不可用（加密解不出来或已损坏），已搜索目录：%s",
			region, encrypted, strings.Join(dirs, "; "))
	} else {
		log.Printf("[codebuddy] 未找到 %s 登录文件，已搜索目录：%s", region, strings.Join(dirs, "; "))
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

// loadIfStale 文件 mtime 变了（桌面端外部刷新过）→ 重新加载并解密 token。
// 解密一次、随文件缓存：信封解密只在文件变化时发生，不在每请求重复调 exe。
// 成功日志只打脱敏描述（JWT 头部前 20 字＋长度），凭据原文不出日志。
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
		c.decAccess, c.decRefresh = "", ""
		s, err := c.session()
		if err != nil {
			return err
		}
		access, err := s.Auth.AccessToken.Resolve(c.region)
		if err != nil {
			return fmt.Errorf("登录文件 %s 的加密 token 无法解密（%s）",
				filepath.Base(c.path), err.Error())
		}
		refresh, err := s.Auth.RefreshToken.Resolve(c.region)
		if err != nil {
			return fmt.Errorf("登录文件 %s 的加密 token 无法解密（%s）",
				filepath.Base(c.path), err.Error())
		}
		c.decAccess, c.decRefresh = access, refresh
		if s.Auth.AccessToken.IsEncrypted() || s.Auth.RefreshToken.IsEncrypted() {
			log.Printf("[codebuddy] at-rest 加密登录态解密成功：文件=%s %s",
				filepath.Base(c.path), desensToken(access))
		}
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
// 回写形态与原文件一致：原 token 是 $wbEncrypted 信封时，新 token 同样加密
// 回写（nonce 随机，桌面端可读，登录态不被破坏）；明文原文件保持明文。
func (c *Credential) refresh(ctx context.Context, s *sessionInfo) error {
	refreshToken, err := s.Auth.RefreshToken.Resolve(c.region)
	if err != nil {
		return fmt.Errorf("登录文件 %s 的加密 token 无法解密（%s）",
			filepath.Base(c.path), err.Error())
	}
	h := c.buildHeaders(s, c.decAccess)
	h.Set("X-Refresh-Token", refreshToken)
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
	// 加密回写：原文件 token 是信封形态才加密（形态保持，桌面端可读）。
	// 刷新拿到的永远是明文（出站语义不变）：先记下明文同步解密缓存，再换成
	// 信封写回，写回后再读文件（mtime 已变）不会因"密文对不上缓存"误事。
	newAccess, _ := newAuth["accessToken"].(string)
	newRefresh, _ := newAuth["refreshToken"].(string)
	if s.Auth.AccessToken.IsEncrypted() || s.Auth.RefreshToken.IsEncrypted() {
		if newAccess != "" {
			enc, err := EncryptLike(s.Auth.AccessToken.raw, newAccess, c.region)
			if err != nil {
				return fmt.Errorf("刷新成功但加密回写失败（%s），未动登录文件", err.Error())
			}
			newAuth["accessToken"] = enc
		}
		if newRefresh != "" {
			enc, err := EncryptLike(s.Auth.RefreshToken.raw, newRefresh, c.region)
			if err != nil {
				return fmt.Errorf("刷新成功但加密回写失败（%s），未动登录文件", err.Error())
			}
			newAuth["refreshToken"] = enc
		}
	}
	if newAccess != "" {
		c.decAccess = newAccess
	}
	if newRefresh != "" {
		c.decRefresh = newRefresh
	}
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

// buildHeaders 构造后端请求头（access 取已解密的明文；出站语义不变）。
func (c *Credential) buildHeaders(s *sessionInfo, access string) http.Header {
	domain := s.Auth.Domain
	if domain == "" {
		domain = defaultDomain
	}
	h := http.Header{}
	h.Set("Content-Type", "application/json")
	h.Set("Accept", "application/json")
	h.Set("Authorization", "Bearer "+access)
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
	// 兜底：不可用文件绝不允许走到出站（选文件层已过滤；显式路径传入时在这里拦）。
	// 错误只带文件名与原因类别，不带全路径、不带 token。
	if c.decAccess == "" {
		return nil, fmt.Errorf("登录文件 %s 的加密 token 无法解密（解密后为空）",
			filepath.Base(c.path))
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
	return c.buildHeaders(s, c.decAccess), nil
}

// Summary 返回诊断信息（uid/昵称/企业/token 过期状态）。
func (c *Credential) Summary() map[string]any {
	c.mu.Lock()
	defer c.mu.Unlock()
	// 无登录文件：如实报去哪找过（health 直接透出，用户照着能自查）；
	// 有文件但全是新版加密时说清"不是没文件，是解密不了"。
	if c.path == "" {
		if n := countEncryptedFiles(c.region); n > 0 {
			return map[string]any{"error": fmt.Sprintf(
				"找到 %s 区域 %d 个登录文件但 token 均为新版加密形态（桌面端升级后加密存储，代理无法解密），已搜索目录：%s",
				c.region, n, strings.Join(findAuthDirs(), "; "))}
		}
		return map[string]any{"error": fmt.Sprintf("未找到登录文件，已搜索目录：%s", strings.Join(findAuthDirs(), "; "))}
	}
	if err := c.loadIfStale(); err != nil {
		return map[string]any{"error": err.Error()}
	}
	s, err := c.session()
	if err != nil {
		return map[string]any{"error": err.Error()}
	}
	return map[string]any{
		"uid":             s.Account.UID,
		"nickname":        string(s.Account.Nickname),
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
