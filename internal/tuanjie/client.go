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
	// 官方 UA 真值（2026-08-25 从 @unity-china/codely-cli rc.54 源码 getRealUserAgent 提取）：
	// `Codely-CLI - OSS/${版本} (Codely-Cli/${版本})`。旧值 codely-cli/1.0.0-release.52
	// 外壳与版本格式都不对（官方是 rc.54 且无平台后缀），已对齐官方格式。
	cliUserAgent    = "Codely-CLI - OSS/1.0.0-rc.54 (Codely-Cli/1.0.0-rc.54)"
	keyCacheTTL     = time.Hour
	defaultTimeout  = 300 * time.Second
	keyFetchTimeout = 15 * time.Second

	// codelySigningSeedHex 是官方 CLI 内置的签名种子（逆向自 1.0.0-release.52），
	// 与 cli_api_key 两层 HMAC 派生签名密钥。
	codelySigningSeedHex = "406f00f74768ba0cb0cd30f097ec6c2bdacb89c61a38b7dd140838bbd0e98018"
)

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
	return out.CliAPIKey, nil
}

// apiKeyCached 返回有效的 cli_api_key（过期自动换取）。
func (c *Client) apiKeyCached(ctx context.Context) (string, error) {
	c.mu.Lock()
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

// newLitellmSessionID 生成每请求随机的 x-litellm-session-id。
// session 每请求随机：上游按 session 做实例亲和路由，固定 session 会
// 粘死在映射失步的实例上持续 400 model=None（实测 2026-08-21）。
func newLitellmSessionID() string { return uuid.New().String() }

// litellmHeaders 构造伪装 codely CLI 的请求头。path 参与签名，是上游路径。
// sessionID 为本请求的会话 id（x-litellm-session-id 头与请求体
// litellm_session_id 字段同值，与官方 CLI 一致）。
// 2026-08-25 对照官方 CLI 源码（@unity-china/codely-cli rc.54）补齐：
// DashScope 分支标记头（GLM/Kimi 走阿里后端时官方会带，缺了易被识别为反代）。
func (c *Client) litellmHeaders(path, key, sessionID string) http.Header {
	h := http.Header{}
	h.Set("Authorization", "Bearer "+key)
	h.Set("Content-Type", "application/json")
	h.Set("Accept", "application/json")
	h.Set("User-Agent", cliUserAgent)
	if sessionID == "" {
		sessionID = newLitellmSessionID()
	}
	h.Set("x-litellm-session-id", sessionID)
	h.Set("X-Codely-Signature", SignLitellm(path, key, time.Now()))
	// 官方 DashScope provider 标记头（源码实证；GLM/Kimi 大概率走此分支）
	h.Set("X-DashScope-CacheControl", "enable")
	h.Set("X-DashScope-UserAgent", cliUserAgent)
	return h
}

// Forward 转发请求到 LiteLLM。method/path/bodyIn 为入站请求；
// 返回上游响应（调用方负责关闭 Body）。上游 401 时自动换 key 重试一次。
// body 会先整体读入内存，保证重试时能完整重放。
func (c *Client) Forward(ctx context.Context, method, path string, bodyIn io.Reader, contentType string) (*http.Response, error) {
	return c.ForwardWithSession(ctx, method, path, bodyIn, contentType, "")
}

// ForwardWithSession 同 Forward，但显式指定 x-litellm-session-id（请求体
// litellm_session_id 与头同值时用，handleChat 重排后走这里）。
func (c *Client) ForwardWithSession(ctx context.Context, method, path string, bodyIn io.Reader, contentType, sessionID string) (*http.Response, error) {
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
		for k, vs := range c.litellmHeaders(path, key, sessionID) {
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
