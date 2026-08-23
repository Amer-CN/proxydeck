package tuanjie

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// KIMI-K3 节奏器（pacing）：上游对 KIMI-K3 有个人滑动窗口限流
// （TPM cap=200000/60s，used 随静置平滑衰减，静置 ~4 分钟归零放行），
// 单个 agent 请求即可打爆窗口导致客户端 429 重试风暴。
// 开启时：本地按 60s 滑动窗口记账，预算不足排队到点放行；上游 429 时代为
// 等待自动重发，客户端永不（在 30 分钟总预算内）收到 KIMI 的 429。
// 关闭时：调用方（handleChat）短路判定，行为与无节奏器完全一致。
const (
	pacingUpstreamCap = 200000              // 上游 TPM cap（窗口回补速率 = cap/60）
	pacingEffectiveCap = 180000             // 有效预算（留安全余量）
	pacingMargin       = 20000              // 放行余量（吸收上游计数偏差）
	pacingWindow       = 60 * time.Second   // 滑动窗口
	pacingMaxWait      = 30 * time.Minute   // 单请求总等待上限
	pacingFallbackWait = 65 * time.Second   // 429 错误体解析失败时的固定等待
	pacing429Extra     = 5 * time.Second    // 429 等待换算的固定附加
)

// pacingRefillPerSec 窗口回补速率（token/秒）：上游 cap/60。
const pacingRefillPerSec = float64(pacingUpstreamCap) / 60.0

// pacingRateLimitRe 匹配上游 429 错误体里的 `used=(\d+), cap=(\d+)`。
var pacingRateLimitRe = regexp.MustCompile(`used=(\d+), cap=(\d+)`)

// tokenUse 滑窗内一次放行请求的 token 记录。
type tokenUse struct {
	at     time.Time
	tokens int64
}

// Pacer 是 KIMI-K3 节奏器（线程安全）。
type Pacer struct {
	enabled atomic.Bool
	path    string // 持久化路径（exe 同目录 tuanjie-pacing.json）

	mu     sync.Mutex
	window []tokenUse // 最近 60s 放行请求的 token 记录（滑动）
	pending atomic.Int64
}

// NewPacer 创建节奏器并从 exe 同目录读回开关状态。
func NewPacer() *Pacer {
	p := &Pacer{}
	if exe, err := os.Executable(); err == nil {
		p.path = filepath.Join(filepath.Dir(exe), "tuanjie-pacing.json")
		p.load()
	}
	return p
}

// Enabled 返回开关状态。
func (p *Pacer) Enabled() bool { return p.enabled.Load() }

// SetEnabled 切换开关并持久化。
func (p *Pacer) SetEnabled(v bool) {
	p.enabled.Store(v)
	p.save()
}

// Pending 返回当前在节奏器内排队/放行中的请求数。
func (p *Pacer) Pending() int64 { return p.pending.Load() }

// IsPacingModel 判断 chat 请求是否走节奏器：仅 model 名含 "KIMI"（不分大小写）；
// GLM/codely 等一律直通，零影响。
func IsPacingModel(model string) bool {
	return strings.Contains(strings.ToUpper(model), "KIMI")
}

// EstimateTokens 粗估请求体 token 数（中英混合 len/3，够用）。
func EstimateTokens(body []byte) int64 {
	return int64(len(body)) / 3
}

// WaitSeconds 按滑窗预算换算还需等待的秒数（纯函数）：
// 预算 = effectiveCap - used；est+margin 超预算时按缺口 token/(cap/60) 秒
// 等待（如缺 60K → 等 18s），窗口随时间回补。
func WaitSeconds(est, used int64) time.Duration {
	deficit := est + pacingMargin - (pacingEffectiveCap - used)
	if deficit <= 0 {
		return 0
	}
	secs := float64(deficit) / pacingRefillPerSec
	return time.Duration(secs * float64(time.Second))
}

// RateLimitWait 解析上游 429 错误体换算等待时长（纯函数）：
// 匹配 `used=(\d+), cap=(\d+)` 时按超出量/(cap/60)+5s；解析失败固定 65s。
func RateLimitWait(errBody string) time.Duration {
	m := pacingRateLimitRe.FindStringSubmatch(errBody)
	if m == nil {
		return pacingFallbackWait
	}
	used, err1 := strconv.ParseInt(m[1], 10, 64)
	capv, err2 := strconv.ParseInt(m[2], 10, 64)
	if err1 != nil || err2 != nil || capv <= 0 {
		return pacingFallbackWait
	}
	over := used - capv
	if over < 0 {
		over = 0
	}
	secs := float64(over)/(float64(capv)/60.0) + pacing429Extra.Seconds()
	return time.Duration(secs * float64(time.Second))
}

// Acquire 放行前按滑窗预算排队：预算不足时循环等待（缺口按 WaitSeconds
// 换算，窗口随时间回补），直至预算足够或到达 30 分钟总上限（上限到了直接
// 放行，交由 429 兜底）。放行时立即把估算值记入滑窗。返回总等待预算的
// 截止时刻（供 429 重发共用同一预算）。
func (p *Pacer) Acquire(ctx context.Context, est int64) time.Time {
	deadline := time.Now().Add(pacingMaxWait)
	p.pending.Add(1)
	defer p.pending.Add(-1)
	for {
		now := time.Now()
		wait := WaitSeconds(est, p.windowUsedAt(now))
		if wait <= 0 {
			break
		}
		if now.Add(wait).After(deadline) {
			break // 30 分钟上限：放行交由 429 兜底
		}
		select {
		case <-ctx.Done():
			return deadline
		case <-time.After(wait):
		}
	}
	p.recordAt(est, time.Now())
	return deadline
}

// WindowUsed 返回滑窗内已记 token 总量（状态展示用）。
func (p *Pacer) WindowUsed() int64 {
	return p.windowUsedAt(time.Now())
}

// windowUsedAt 清掉窗口外记录并返回窗口内 token 总量。
func (p *Pacer) windowUsedAt(now time.Time) int64 {
	p.mu.Lock()
	defer p.mu.Unlock()
	keep := p.window[:0]
	var sum int64
	for _, u := range p.window {
		if now.Sub(u.at) < pacingWindow {
			keep = append(keep, u)
			sum += u.tokens
		}
	}
	p.window = keep
	return sum
}

// recordAt 把一次放行的 token 估算记入滑窗。
func (p *Pacer) recordAt(est int64, at time.Time) {
	p.mu.Lock()
	p.window = append(p.window, tokenUse{at: at, tokens: est})
	p.mu.Unlock()
}

// load 启动时读回开关状态。
func (p *Pacer) load() {
	if p.path == "" {
		return
	}
	b, err := os.ReadFile(p.path)
	if err != nil {
		return
	}
	var v struct {
		Enabled bool `json:"enabled"`
	}
	if json.Unmarshal(b, &v) == nil {
		p.enabled.Store(v.Enabled)
	}
}

// save 原子落盘（仿 saveStats 的写法）。
func (p *Pacer) save() {
	if p.path == "" {
		return
	}
	b, err := json.Marshal(map[string]bool{"enabled": p.enabled.Load()})
	if err != nil {
		return
	}
	tmp := p.path + ".tmp"
	if os.WriteFile(tmp, b, 0o644) == nil {
		_ = os.Rename(tmp, p.path)
	}
}
