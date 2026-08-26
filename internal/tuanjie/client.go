// Package tuanjie 把团结 Cowork (Codely) 的登录态转成本地 OpenAI 兼容 API。
// 流程与原 codely2api.py 一致：
//  1. 读取 ~/.codely-cli/oauth_creds.json 的 access_token
//  2. 用 access_token 调 codely.tuanjie.cn/api/api-token/cli-api-key 换取 cli_api_key（缓存 1 小时）
//  3. 用 cli_api_key + x-litellm-session-id + CLI User-Agent 转发请求到 LiteLLM
package tuanjie

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"
)

const (
	codelyAPIBase   = "https://codely.tuanjie.cn"
	litellmAPIBase  = "https://codely-litellm.tuanjie.cn"
	cliAPIKeyURL    = codelyAPIBase + "/api/api-token/cli-api-key"
	keyCacheTTL     = time.Hour
	defaultTimeout  = 300 * time.Second
	keyFetchTimeout = 15 * time.Second

	// codelySigningSeedHex 是官方 CLI 内置的签名种子（逆向自 1.0.0-release.52），
	// 与 cli_api_key 两层 HMAC 派生签名密钥。
	codelySigningSeedHex = "406f00f74768ba0cb0cd30f097ec6c2bdacb89c61a38b7dd140838bbd0e98018"

	// cliUserAgentFallback 探测不到本机 CLI 版本时的兜底 UA（构建时最新版）。
	// 格式：fmt.Sprintf("codely-cli/%s (%s; %s)", cliVersion, platform, arch)。
	cliUserAgentFallback = "codely-cli/1.0.0-rc.55 (win32; x64)"
)

// cliUserAgent 启动时解析：优先读本机 npm 全局安装的 codely-cli 实际版本
// （CLI 升级后代理自动跟随，不用改代码）；读不到用兜底值。小写、带 Node
// platform/arch 枚举；本产品仅跑 Windows，win32/x64 写死。
var cliUserAgent = resolveCLIUserAgent()

func resolveCLIUserAgent() string {
	if v := detectLocalCliVersion(); v != "" {
		return "codely-cli/" + v + " (win32; x64)"
	}
	return cliUserAgentFallback
}

// detectLocalCliVersion 读本机 npm 全局 codely-cli 的 package.json 版本号。
// 候选路径：APPDATA 环境变量与 home\AppData\Roaming\npm（Windows npm
// 用户级全局安装标准位置）。版本需形如 x.y.z[-prerelease] 才采信。
func detectLocalCliVersion() string {
	var candidates []string
	if appdata := os.Getenv("APPDATA"); appdata != "" {
		candidates = append(candidates, filepath.Join(appdata, "npm", "node_modules", "@unity-china", "codely-cli", "package.json"))
	}
	if home, err := os.UserHomeDir(); err == nil {
		candidates = append(candidates, filepath.Join(home, "AppData", "Roaming", "npm", "node_modules", "@unity-china", "codely-cli", "package.json"))
	}
	for _, p := range candidates {
		b, err := os.ReadFile(p)
		if err != nil {
			continue
		}
		var pkg struct {
			Version string `json:"version"`
		}
		if json.Unmarshal(b, &pkg) == nil && cliVersionRe.MatchString(pkg.Version) {
			return pkg.Version
		}
	}
	return ""
}

// cliVersionRe 合法 semver（含 prerelease 后缀，如 1.0.0-rc.55）。
var cliVersionRe = regexp.MustCompile(`^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$`)

// smartProxyTransport 读系统代理设置（环境变量 + Windows 注册表），
// 代理不可达时自动回退直连——Clash/v2ray 没开时不崩。
// TLS 握手层换成 utls 指纹（HelloChrome_Auto，对齐官方 CLI 的 BoringSSL 系
// ClientHello）；代理场景（Clash）维持 CONNECT 隧道内嵌 utls。
// socks5 系代理（标准库 Transport 与 utls 自管路径都不支持）回落老的标准
// Transport 行为，保证不比基线差。
var smartProxyTransport = pickTransport()

// pickTransport 按系统代理类型选 transport：socks5 → 标准 Transport；
// 其余（http/https 代理或无代理）→ utls 指纹 Transport。
func pickTransport() *http.Transport {
	if u := proxyURLForAddr(smartProxy, "codely-litellm.tuanjie.cn:443"); u != nil && isSocksProxy(u) {
		return newUtlsTransportKeepStdlib(smartProxy)
	}
	return newUtlsTransport(smartProxy)
}

// noProxyTransport 强制直连，用于连本地 CDP（127.0.0.1 永远不该走代理）。
var noProxyTransport = &http.Transport{
	Proxy: nil,
}

// smartProxy 解析代理地址：优先环境变量 HTTP_PROXY/HTTPS_PROXY，
// 其次 Windows 注册表 Internet Settings（Clash/v2ray 写入处）。
// localhost / 127.0.0.1 / 内网地址自动跳过代理。
func smartProxy(req *http.Request) (*url.URL, error) {
	host := req.URL.Hostname()
	// 本地地址永远不走代理
	if host == "localhost" || host == "127.0.0.1" || host == "::1" ||
		strings.HasPrefix(host, "192.168.") || strings.HasPrefix(host, "10.") ||
		strings.HasPrefix(host, "172.16.") || strings.HasPrefix(host, "172.17.") ||
		strings.HasPrefix(host, "172.18.") || strings.HasPrefix(host, "172.19.") ||
		strings.HasPrefix(host, "172.2") || strings.HasPrefix(host, "172.3") {
		return nil, nil
	}
	// 1. 环境变量
	if u, err := http.ProxyFromEnvironment(req); err != nil || u != nil {
		return u, err
	}
	// 2. Windows 注册表（Clash/v2ray 系统代理设置）
	proxyURL := readWindowsProxy()
	if proxyURL == "" {
		return nil, nil
	}
	return url.Parse(proxyURL)
}

// Client 是团结 LiteLLM 的转发客户端（线程安全）。
type Client struct {
	httpClient *http.Client

	mu       sync.Mutex
	apiKey   string
	keyAt    time.Time
	keyFetch bool // 正在换取，防止并发重复换取

	tokenFP string // 换取 key 所用 access_token 的指纹（mtime+size）；
	// 桌面端切换账号会更新 ~/.codely-cli/oauth_creds.json，指纹变化=账号变了，
	// 缓存里的 key 作废重换，避免继续用旧账号的 cli_api_key（否则官方踢号后
	// 返回 400 而非 401，InvalidateKey 触发不了，1 小时内容器一直被旧 key 卡死）。
	fpOK    bool // 首次未采样前不做失效判断（避免误杀首次换取）
}

// NewClient 创建客户端。
func NewClient() *Client {
	return &Client{httpClient: &http.Client{Timeout: defaultTimeout, Transport: smartProxyTransport}}
}

// oauthCredsPath 返回 oauth 凭据文件路径。
func oauthCredsPath() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return ".codely-cli/oauth_creds.json"
	}
	return filepath.Join(home, ".codely-cli", "oauth_creds.json")
}

type oauthCreds struct {
	AccessToken string `json:"access_token"`
	UserID      int    `json:"user_id"`
}

// accessTokenFP 返回 oauth 凭据文件的轻量指纹（mtime+size）。
// 桌面端切换账号会重写该文件——指纹变化即账号变化（不读内容，避免每次请求解析 JWT）。
func accessTokenFP() (string, bool) {
	b, err := os.Stat(oauthCredsPath())
	if err != nil {
		return "", false
	}
	return fmt.Sprintf("%d-%d", b.ModTime().UnixNano(), b.Size()), true
}

// loadAccessToken 读取本地登录态。
func loadAccessToken() (string, error) {
	b, err := os.ReadFile(oauthCredsPath())
	if err != nil {
		return "", fmt.Errorf("读取团结登录态失败（请先登录团结 Cowork 桌面端）: %w", err)
	}
	var c oauthCreds
	if err := json.Unmarshal(b, &c); err != nil {
		return "", fmt.Errorf("解析 oauth_creds.json 失败: %w", err)
	}
	if c.AccessToken == "" {
		return "", errors.New("oauth_creds.json 里没有 access_token")
	}
	return c.AccessToken, nil
}

// fetchKey 用 access_token 换取 cli_api_key。
func (c *Client) fetchKey(ctx context.Context) (string, error) {
	at, err := loadAccessToken()
	if err != nil {
		return "", err
	}
	ctx2, cancel := context.WithTimeout(ctx, keyFetchTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(ctx2, http.MethodGet, cliAPIKeyURL, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+at)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", cliUserAgent)
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return "", fmt.Errorf("换取 cli_api_key 网络失败: %w", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("换取 cli_api_key 返回 %d: %s", resp.StatusCode, truncate(string(body), 200))
	}
	var out struct {
		CliAPIKey string `json:"cli_api_key"`
		UserID    int    `json:"user_id"`
	}
	if err := json.Unmarshal(body, &out); err != nil || out.CliAPIKey == "" {
		return "", fmt.Errorf("换取 cli_api_key 响应异常: %s", truncate(string(body), 200))
	}
	// 记录本次换取所用 access_token 的指纹（换 key 成功才更新）
	if fp, ok := accessTokenFP(); ok {
		c.mu.Lock()
		c.tokenFP = fp
		c.fpOK = true
		c.mu.Unlock()
	}
	return out.CliAPIKey, nil
}

// apiKeyCached 返回有效的 cli_api_key（过期自动换取）。
func (c *Client) apiKeyCached(ctx context.Context) (string, error) {
	c.mu.Lock()
	// 账号指纹变化（桌面端切换了登录账号）→ 缓存 key 作废
	if c.apiKey != "" && c.fpOK {
		if fp, ok := accessTokenFP(); ok && fp != c.tokenFP {
			c.apiKey = ""
			c.keyAt = time.Time{}
		}
	}
	if c.apiKey != "" && time.Since(c.keyAt) < keyCacheTTL {
		k := c.apiKey
		c.mu.Unlock()
		return k, nil
	}
	c.mu.Unlock()

	// 换取（简单串行化：并发时后来者等待后重新检查）
	k, err := c.fetchKey(ctx)
	if err != nil {
		return "", err
	}
	c.mu.Lock()
	c.apiKey = k
	c.keyAt = time.Now()
	c.mu.Unlock()
	return k, nil
}

// InvalidateKey 清除缓存的 key（401 时调用，下次重新换取）。
func (c *Client) InvalidateKey() {
	c.mu.Lock()
	c.apiKey = ""
	c.keyAt = time.Time{}
	c.mu.Unlock()
}

func hmacSHA256(key, msg []byte) []byte {
	m := hmac.New(sha256.New, key)
	m.Write(msg)
	return m.Sum(nil)
}

// codelySigningKey 从 cli_api_key 派生签名密钥（两层 HMAC，与官方 CLI 一致）。
func codelySigningKey(cliAPIKey string) []byte {
	seed, _ := hex.DecodeString(codelySigningSeedHex)
	k1 := hmacSHA256(seed, []byte("codely-signing-v1"))
	return hmacSHA256(k1, []byte(cliAPIKey))
}

// SignLitellm 生成 X-Codely-Signature 头值：v1.<秒级时间戳>.<base64url 签名>。
// 消息体为 "v1\n<path>\n<timestamp>"（只签 path），时间戳每请求实时取。
func SignLitellm(path, cliAPIKey string, now time.Time) string {
	ts := strconv.FormatInt(now.Unix(), 10)
	msg := "v1\n" + path + "\n" + ts
	sig := hmacSHA256(codelySigningKey(cliAPIKey), []byte(msg))
	return "v1." + ts + "." + base64.RawURLEncoding.EncodeToString(sig)
}

// LitellmSession 一次上游会话，语义对齐官方 CLI 的「一个 CLI 窗口」：
// 窗口内所有请求复用同一 session id；窗口关闭（闲置清理）后消失。
type LitellmSession struct {
	ID string // x-litellm-session-id（同时进请求体 litellm_session_id）

	inUse     int       // 并发占用计数（>0 = 忙）
	createdAt time.Time // 创建时间（4h 轮换依据）
	lastUsed  time.Time // 最近归还时间（30min 闲置清理依据）
}

// 会话池参数（对齐官方 CLI 行为画像，社区逆向情报 2026-08-26）：
//   - 串行请求自然复用同一空闲会话（单窗口连续对话）
//   - 并发全忙时新建（模拟再开一个 CLI 窗口）
//   - 闲置 30 分钟清理（模拟关闭窗口）
//   - 会话最长存活 4 小时（模拟 CLI 重启）
//   - 池上限 16：并发爆发超限时共享最闲会话（官方单窗口本来也允许
//     并发请求共享同一 session），避免短时间冒出几十上百个"会话"
//     的反代特征
var (
	sessionPoolMu      sync.Mutex
	sessionPool        []*LitellmSession
	sessionIdleTTL     = 30 * time.Minute
	sessionMaxAge      = 4 * time.Hour
	sessionPoolMaxSize = 16
)

// AcquireLitellmSession 从会话池租借一个会话：
// 优先复用最近使用的空闲会话（串行请求 → 恒同一会话，贴近官方单窗口）；
// 全忙且未达上限 → 新建；达上限 → 共享占用最少的。
// 取用时顺带清理闲置超时/超龄的空闲会话。
func AcquireLitellmSession() *LitellmSession {
	sessionPoolMu.Lock()
	defer sessionPoolMu.Unlock()
	now := time.Now()
	// 清理：空闲（inUse==0）且（闲置>30min 或 年龄>4h）
	kept := sessionPool[:0]
	for _, s := range sessionPool {
		if s.inUse == 0 && (now.Sub(s.lastUsed) > sessionIdleTTL || now.Sub(s.createdAt) > sessionMaxAge) {
			continue
		}
		kept = append(kept, s)
	}
	sessionPool = kept
	// 1) 最近使用的空闲会话
	var idle *LitellmSession
	for _, s := range sessionPool {
		if s.inUse == 0 && (idle == nil || s.lastUsed.After(idle.lastUsed)) {
			idle = s
		}
	}
	if idle != nil {
		idle.inUse++
		idle.lastUsed = now
		return idle
	}
	// 2) 全忙：未达上限 → 新建
	if len(sessionPool) < sessionPoolMaxSize {
		s := newLitellmSession()
		s.inUse = 1
		s.lastUsed = now
		sessionPool = append(sessionPool, s)
		return s
	}
	// 3) 达上限：共享占用最少的（官方单窗口本就并发共享同一 session）
	var share *LitellmSession
	for _, s := range sessionPool {
		if share == nil || s.inUse < share.inUse {
			share = s
		}
	}
	share.inUse++
	share.lastUsed = now
	return share
}

// ReleaseLitellmSession 归还会话（handleChat defer 调用，响应写完才算闲置）。
func ReleaseLitellmSession(s *LitellmSession) {
	if s == nil {
		return
	}
	sessionPoolMu.Lock()
	defer sessionPoolMu.Unlock()
	if s.inUse > 0 {
		s.inUse--
	}
	s.lastUsed = time.Now()
}

// newLitellmSession 生成一个全新会话。
func newLitellmSession() *LitellmSession {
	return &LitellmSession{
		ID:        uuid.New().String(),
		createdAt: time.Now(),
	}
}

// newLitellmSessionID 生成不入池的一次性会话 id（reshape 兜底/旧路径兼容）。
func newLitellmSessionID() string { return newLitellmSession().ID }

// litellmHeaders 构造伪装 codely CLI 的请求头。path 参与签名，是上游路径。
// sess 为本请求的会话（x-litellm-session-id 头与请求体 litellm_session_id
// 字段同值）；sess 为 nil 时现场造一次性会话。
	// 头集合 = 官方 CLI 实际发送的最小集（rc.55 源码逐项实证）：
	//   - DashScope 头不带：官方仅 isDashScopeProvider() 路线发，团结
	//     LiteLLM 路线官方不发（2026-08-25 实证）
	//   - B3 追踪头不带：telemetry 默认 enabled:!1 且 SDK 未注册 B3
	//     propagator，官方默认不发（2026-08-27 实证 + A/B 大小请求均 200；
	//     401 修复靠 UA 版本号，与 B3 无关）
func (c *Client) litellmHeaders(path, key string, sess *LitellmSession) http.Header {
	if sess == nil {
		sess = newLitellmSession()
	}
	h := http.Header{}
	h.Set("Authorization", "Bearer "+key)
	h.Set("Content-Type", "application/json")
	h.Set("Accept", "application/json")
	h.Set("User-Agent", cliUserAgent)
	h.Set("x-litellm-session-id", sess.ID)
	h.Set("X-Codely-Signature", SignLitellm(path, key, time.Now()))
	return h
}

// Forward 转发请求到 LiteLLM。method/path/bodyIn 为入站请求；
// 返回上游响应（调用方负责关闭 Body）。上游 401 时自动换 key 重试一次。
// body 会先整体读入内存，保证重试时能完整重放。
func (c *Client) Forward(ctx context.Context, method, path string, bodyIn io.Reader, contentType string) (*http.Response, error) {
	return c.ForwardWithSession(ctx, method, path, bodyIn, contentType, nil)
}

// ForwardWithSession 同 Forward，但显式指定会话（请求体 litellm_session_id
// 与头同值时用，handleChat 从会话池租借后走这里；重试共用同一会话）。
func (c *Client) ForwardWithSession(ctx context.Context, method, path string, bodyIn io.Reader, contentType string, sess *LitellmSession) (*http.Response, error) {
	var body []byte
	if bodyIn != nil {
		b, err := io.ReadAll(bodyIn)
		if err != nil {
			return nil, err
		}
		body = b
	}
	doForward := func() (*http.Response, error) {
		key, err := c.apiKeyCached(ctx)
		if err != nil {
			return nil, err
		}
		req, err := http.NewRequestWithContext(ctx, method, litellmAPIBase+path, bytes.NewReader(body))
		if err != nil {
			return nil, err
		}
		for k, vs := range c.litellmHeaders(path, key, sess) {
			for _, v := range vs {
				req.Header.Add(k, v)
			}
		}
		return c.httpClient.Do(req)
	}

	resp, err := doForward()
	if err != nil {
		return nil, err
	}
	// key 过期 → 重换一次
	if resp.StatusCode == http.StatusUnauthorized {
		resp.Body.Close()
		c.InvalidateKey()
		return doForward()
	}
	return resp, nil
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

// readWindowsProxy 读注册表 HKCU\...\Internet Settings 的代理配置。
// ProxyEnable=1 时返回 "http://<ProxyServer>"（Clash/v2ray 写入处）；
// 否则返回空串。非 Windows 平台始终返回空串。
func readWindowsProxy() string {
	// runtime.GOOS != "windows" 时 golang.org/x/sys/windows 不可用，
	// 用反射式读取：注册表仅在 Windows 上有意义
	if regReadProxy == nil {
		return ""
	}
	return regReadProxy()
}
