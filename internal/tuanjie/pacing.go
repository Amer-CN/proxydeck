package tuanjie

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
)

// KIMI-K3 429 兜底（pacing）：2026-08 下旬上游改限流口径，429 错误体不再携带
// used/cap 数字，改为 `litellm.RateLimitError: Sliding window TPM limit exceeded`
// （provider_ref 滑窗），多数 429 带 Retry-After 头。原本地滑窗记账（cap 硬编码
// + 主动排队）前提已失效，废除为纯响应式兜底：上游 429 → 按 Retry-After 等待
// （缺省 60s = 一个窗口）→ 原样重发，直至 200 或 30 分钟总预算耗尽。
// 关闭时：调用方（handleChat）短路判定，行为与无节奏器完全一致。
const (
	pacingRetryDefault = 60 * time.Second // Retry-After 缺省：一个窗口
	pacingMaxWait      = 30 * time.Minute // 单请求总兜底预算
)

// Pacer 是 KIMI-K3 429 兜底开关（线程安全）。
type Pacer struct {
	enabled atomic.Bool
	path    string // 持久化路径（exe 同目录 tuanjie-pacing.json）

	pending atomic.Int64
}

// NewPacer 创建兜底开关并从 exe 同目录读回开关状态。
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

// Pending 返回当前被 429 兜底接管的进行中请求数。
func (p *Pacer) Pending() int64 { return p.pending.Load() }

// IsPacingModel 判断 chat 请求是否走 429 兜底：仅 model 名含 "KIMI"（不分大小写）；
// GLM/codely 等一律直通，零影响。
func IsPacingModel(model string) bool {
	return strings.Contains(strings.ToUpper(model), "KIMI")
}

// RetryWait 解析上游 Retry-After 头换算等待时长（纯函数）：
// TrimSpace 后为 >0 的整数秒 → 该秒数；空/非数字/0/负数 → 缺省 60s（一个窗口）。
func RetryWait(retryAfter string) time.Duration {
	secs, err := strconv.Atoi(strings.TrimSpace(retryAfter))
	if err != nil || secs <= 0 {
		return pacingRetryDefault
	}
	return time.Duration(secs) * time.Second
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
