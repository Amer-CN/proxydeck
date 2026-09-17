// config.go —— vibex 插件配置：默认值（SPEC-T1 §3.1）+ token 三选一来源 + tenant_id 生成落盘。
package vibex

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"flag"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"
)

// 缺省值（对齐 SPEC-T1 §3.1 的 DEFAULTS；监听地址/端口由 app 的 --host/--port 提供）。
//
// DEFAULT_INSTRUCTION 用简体中文（对方实现是繁体，不照抄——见任务简报硬约束 5）。
const (
	defaultBaseURL      = "https://vibex.runninghub.cn/vc"
	defaultAppName      = "vibex2api"
	defaultAppType      = "web"
	defaultTurnTimeout  = 900
	defaultReadyTimeout = 90
	defaultQueueLimit   = 4
	defaultHeartbeatSec = 15
	wsInitCwd           = "/workspace/app"
	imagePlaceholder    = "[图片内容不支持, 已忽略]"
	defaultInstruction  = "请直接以「助手」的身份回答最后一条用户消息: 直接给出回答内容本身; " +
		"这是一次纯问答, 无需创建或修改任何项目文件。"

	// tokenCooldown 是 402/429 命中后该 token 的冷却时长（额度按结算周计，冷却到下周才恢复，
	// 这里只做短期隔离，到期自动再试——不落盘，随进程内存态）。
	tokenCooldown = 10 * time.Minute

	// 上游 UA（照抄 SPEC-T1 §3.3）。
	upstreamUA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 " +
		"(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
)

// 本包自注册两个 flag（默认 CommandLine，与 app 的 flag.Parse() 同一集合）：
// app 侧无需改动即可用 --vibex-token / --vibex-config 传参。
var (
	flagVibexToken  = flag.String("vibex-token", "", "VibeX Rh-Accesstoken（单 token；也可用环境变量 RH_ACCESSTOKEN 或配置文件的 tokens 数组）")
	flagVibexConfig = flag.String("vibex-config", "", "VibeX 插件配置文件路径（缺省 <exe 目录>/vibex-config.json，权限 0600）")
)

// Config 是插件配置；JSON 字段名与 SPEC-T1 §3.1 的 config.json 对齐。
type Config struct {
	BaseURL         string   `json:"base_url"`
	AppName         string   `json:"app_name"`
	AppType         string   `json:"app_type"`
	Model           string   `json:"model"`
	TenantID        string   `json:"tenant_id"`
	Tokens          []string `json:"tokens"`
	FreshSession    bool     `json:"fresh_session"`
	TurnTimeoutSec  int      `json:"turn_timeout"`
	ReadyTimeoutSec int      `json:"ready_timeout"`
	QueueLimit      int      `json:"queue_limit"`
	HeartbeatSec    int      `json:"heartbeat_s"`
	IncludeThinking bool     `json:"include_thinking"`
	ExposeTools     bool     `json:"expose_tools"`
	ChatInstruction string   `json:"chat_instruction"`
	ConnectMode     string   `json:"connect_mode"`

	path string `json:"-"` // 配置文件路径（tenant_id 生成后回写用）
}

func (c *Config) turnTimeout() time.Duration  { return time.Duration(c.TurnTimeoutSec) * time.Second }
func (c *Config) readyTimeout() time.Duration { return time.Duration(c.ReadyTimeoutSec) * time.Second }
func (c *Config) heartbeat() time.Duration    { return time.Duration(c.HeartbeatSec) * time.Second }
func (c *Config) baseURL() string             { return strings.TrimRight(c.BaseURL, "/") }

// configPath 返回配置文件路径：--vibex-config 优先，其次 exe 同目录的 vibex-config.json。
func configPath() string {
	if p := strings.TrimSpace(*flagVibexConfig); p != "" {
		return p
	}
	return filepath.Join(exeDir(), "vibex-config.json")
}

// exeDir 返回 exe 所在目录（go run 时退回工作目录），与 app/exeDir 同口径。
func exeDir() string {
	if p, err := os.Executable(); err == nil {
		if rp, err := filepath.EvalSymlinks(p); err == nil {
			p = rp
		}
		return filepath.Dir(p)
	}
	if wd, err := os.Getwd(); err == nil {
		return wd
	}
	return "."
}

// LoadConfig 读取配置：缺省值 ← 配置文件（只认已知键，多余键忽略）← 环境变量覆盖 token。
// tenant_id 为空 → 生成 32 位 hex 并立即落盘（0600），下次复用。
func LoadConfig() *Config {
	cfg := &Config{
		BaseURL:         defaultBaseURL,
		AppName:         defaultAppName,
		AppType:         defaultAppType,
		FreshSession:    true,
		TurnTimeoutSec:  defaultTurnTimeout,
		ReadyTimeoutSec: defaultReadyTimeout,
		QueueLimit:      defaultQueueLimit,
		HeartbeatSec:    defaultHeartbeatSec,
		ConnectMode:     "auto",
	}
	cfg.path = configPath()
	if b, err := os.ReadFile(cfg.path); err == nil {
		_ = json.Unmarshal(b, cfg) // 解析失败就用缺省值，不阻断启动
	}
	cfg.normalize()
	if cfg.TenantID == "" {
		cfg.TenantID = randHex(16)
		_ = cfg.persist() // 落盘失败不阻断（只影响 X-Tenant-Id 的稳定性）
	}
	return cfg
}

// normalize 把空值/非法值收敛回缺省，避免配置文件里的空串把默认值冲掉。
func (c *Config) normalize() {
	if strings.TrimSpace(c.BaseURL) == "" {
		c.BaseURL = defaultBaseURL
	}
	// REST 网关要 /vc 前缀（README 配置示例口径；WS 由 _ws_url 取 netloc 直连，不受影响）：
	// 裸域（无 path）自动补 /vc，自带 path 的自定义地址保持原样。
	if u, err := url.Parse(strings.TrimRight(c.BaseURL, "/")); err == nil && (u.Path == "" || u.Path == "/") {
		u.Path = "/vc"
		c.BaseURL = u.String()
	}
	if strings.TrimSpace(c.AppName) == "" {
		c.AppName = defaultAppName
	}
	if strings.TrimSpace(c.AppType) == "" {
		c.AppType = defaultAppType
	}
	if c.TurnTimeoutSec <= 0 {
		c.TurnTimeoutSec = defaultTurnTimeout
	}
	if c.ReadyTimeoutSec <= 0 {
		c.ReadyTimeoutSec = defaultReadyTimeout
	}
	if c.QueueLimit <= 0 {
		c.QueueLimit = defaultQueueLimit
	}
	if c.HeartbeatSec <= 0 {
		c.HeartbeatSec = defaultHeartbeatSec
	}
	if strings.TrimSpace(c.ConnectMode) == "" {
		c.ConnectMode = "auto"
	}
}

// persist 原子落盘（tmp + rename），权限 0600（token 在里面，禁止宽权限）。
// Windows 说明：Go 在 Windows 上没有 POSIX 权限位，0600 只决定「不设只读属性」，
// 实际访问控制由父目录 ACL 决定（实测：本机 %APPDATA% / exe 目录下只有本用户与
// Administrators 有写权，回读结果为 0666）。保留 0600 的写法以便跨平台一致，
// 不额外加 ACL 调用——那超出本插件的职责，且会让同目录其它配置文件的处置不一致。
func (c *Config) persist() error {
	if c.path == "" {
		return os.ErrInvalid
	}
	b, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return err
	}
	tmp := c.path + ".tmp"
	if err := os.WriteFile(tmp, b, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, c.path)
}

// persistToken 把新 token 追加进配置文件（0600，tmp + rename），并同步内存里的
// cfg.Tokens（下一个 token 落盘时才能带上本次这个）。环境变量/flag 来源的 token 不在
// cfg.Tokens 里，故不会被动写进文件——那两种来源跟启动参数走，写进文件会在用户撤销后残留。
func (c *Config) persistToken(token string) error {
	if c.path == "" {
		return os.ErrInvalid
	}
	token = strings.TrimSpace(token)
	if token == "" {
		return os.ErrInvalid
	}
	cp := *c
	cp.Tokens = make([]string, 0, len(c.Tokens)+1)
	for _, t := range c.Tokens {
		if strings.TrimSpace(t) != "" {
			cp.Tokens = append(cp.Tokens, t)
		}
	}
	if !slices.Contains(cp.Tokens, token) {
		cp.Tokens = append(cp.Tokens, token)
	}
	// 同号去重落盘：同 sub 只留最后一条（最新登录串），无 sub 的按串保留。
	// 否则每次登录永久多一条过期串，文件越烂越大（内存池有 addWithState 兜底，
	// 但文件是持久池，必须在这里收敛）。
	seen := make(map[string]bool, len(cp.Tokens))
	out := make([]string, 0, len(cp.Tokens))
	for i := len(cp.Tokens) - 1; i >= 0; i-- {
		t := cp.Tokens[i]
		sub := ""
		if info := decodeJWT(t); info != nil {
			sub = info.Sub
		}
		if sub == "" {
			out = append(out, t)
			continue
		}
		if seen[sub] {
			continue
		}
		seen[sub] = true
		out = append(out, t)
	}
	for i, j := 0, len(out)-1; i < j; i, j = i+1, j-1 {
		out[i], out[j] = out[j], out[i]
	}
	cp.Tokens = out
	if err := cp.persist(); err != nil {
		return err
	}
	c.Tokens = cp.Tokens
	return nil
}

// tokenSources 汇总 token 来源：环境变量 RH_ACCESSTOKEN 优先（SPEC-T1 §3.1），
// 其次 --vibex-token，最后配置文件 tokens 数组；去重保序（池按此顺序轮换）。
func (c *Config) tokenSources() []string {
	out := make([]string, 0, len(c.Tokens)+2)
	add := func(v string) {
		v = strings.TrimSpace(v)
		if v != "" && !slices.Contains(out, v) {
			out = append(out, v)
		}
	}
	add(os.Getenv("RH_ACCESSTOKEN"))
	add(*flagVibexToken)
	for _, t := range c.Tokens {
		add(t)
	}
	return out
}

// randHex 返回 n 字节随机数的 hex 串（tenant_id 用）。
func randHex(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		return ""
	}
	return hex.EncodeToString(b)
}
