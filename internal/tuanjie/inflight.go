// inflight.go —— 进行中请求注册表（学群友 RequestRegistry）。
// 流式请求开始时 Register，每个 chunk 到达时 Touch，结束/异常时 Finish；
// Inflight() 给 GUI"进行中请求"面板用：模型/账号/耗时/已收字节/静默秒数。
package tuanjie

import (
	"sync"
	"time"
)

// InflightReq 是一条进行中请求的快照。
type InflightReq struct {
	RID     string  `json:"rid"`
	Model   string  `json:"model"`
	UserID  string  `json:"user_id"`
	Stream  bool    `json:"stream"`
	Elapsed float64 `json:"elapsed"` // 总耗时秒
	Idle    float64 `json:"idle"`    // 距上个 chunk 的静默秒数
	Bytes   int64   `json:"bytes"`   // 已收字节
}

type registryEntry struct {
	rid       string
	model     string
	userID    string
	stream    bool
	started   time.Time
	lastChunk time.Time
	bytes     int64
}

// RequestRegistry 线程安全的进行中请求注册表（容量防御：超限丢最老）。
type RequestRegistry struct {
	mu       sync.Mutex
	reqs     map[string]*registryEntry
	order    []string
	capacity int
}

// NewRegistry 创建注册表。
func NewRegistry() *RequestRegistry {
	return &RequestRegistry{reqs: map[string]*registryEntry{}, capacity: 500}
}

// Register 登记一条新请求，返回 rid。
func (reg *RequestRegistry) Register(model, userID string, stream bool) string {
	rid := newRID()
	reg.mu.Lock()
	defer reg.mu.Unlock()
	if len(reg.order) >= reg.capacity { // 丢最老
		oldest := reg.order[0]
		reg.order = reg.order[1:]
		delete(reg.reqs, oldest)
	}
	now := time.Now()
	reg.reqs[rid] = &registryEntry{rid: rid, model: model, userID: userID,
		stream: stream, started: now, lastChunk: now}
	reg.order = append(reg.order, rid)
	return rid
}

// Touch 记录一个 chunk 到达（nbytes 累计）。
func (reg *RequestRegistry) Touch(rid string, nbytes int64) {
	reg.mu.Lock()
	defer reg.mu.Unlock()
	if r := reg.reqs[rid]; r != nil {
		r.lastChunk = time.Now()
		r.bytes += nbytes
	}
}

// Finish 移除一条请求。
func (reg *RequestRegistry) Finish(rid string) {
	reg.mu.Lock()
	defer reg.mu.Unlock()
	if _, ok := reg.reqs[rid]; ok {
		delete(reg.reqs, rid)
		for i, o := range reg.order {
			if o == rid {
				reg.order = append(reg.order[:i], reg.order[i+1:]...)
				break
			}
		}
	}
}

// Inflight 返回进行中请求快照（按耗时降序）。
func (reg *RequestRegistry) Inflight() []InflightReq {
	reg.mu.Lock()
	defer reg.mu.Unlock()
	now := time.Now()
	out := make([]InflightReq, 0, len(reg.reqs))
	for _, r := range reg.reqs {
		out = append(out, InflightReq{
			RID: r.rid, Model: r.model, UserID: r.userID, Stream: r.stream,
			Elapsed: now.Sub(r.started).Seconds(),
			Idle:    now.Sub(r.lastChunk).Seconds(),
			Bytes:   r.bytes,
		})
	}
	// 耗时降序
	for i := 1; i < len(out); i++ {
		for j := i; j > 0 && out[j].Elapsed > out[j-1].Elapsed; j-- {
			out[j], out[j-1] = out[j-1], out[j]
		}
	}
	return out
}

// LoadOf 某账号当前进行中请求数（负载感知选号联动）。
func (reg *RequestRegistry) LoadOf(userID string) int {
	reg.mu.Lock()
	defer reg.mu.Unlock()
	n := 0
	for _, r := range reg.reqs {
		if r.userID == userID {
			n++
		}
	}
	return n
}

var ridCounter struct {
	mu sync.Mutex
	n  uint64
}

func newRID() string {
	ridCounter.mu.Lock()
	ridCounter.n++
	n := ridCounter.n
	ridCounter.mu.Unlock()
	// 12 位十六进制风格（与群友一致的观感）
	const hex = "0123456789abcdef"
	buf := make([]byte, 12)
	for i := 11; i >= 0; i-- {
		buf[i] = hex[n&0xf]
		n >>= 4
	}
	return string(buf)
}

// ActivityEvent 实时动态的一条事件（学群友 ActivityLog）。
type ActivityEvent struct {
	Seq       int    `json:"seq"`
	Time      string `json:"time"` // HH:MM:SS
	Kind      string `json:"kind"` // ok | error | info
	Message   string `json:"message"`
	Model     string `json:"model,omitempty"`
	UserID    string `json:"user_id,omitempty"`
	LatencyMs int64  `json:"latency_ms,omitempty"`
	Tokens    int64  `json:"tokens,omitempty"`
	Status    int    `json:"status,omitempty"`
}

// ActivityLog 内存环形缓冲（最近 200 条，重启即清，不落盘）。
type ActivityLog struct {
	mu     sync.Mutex
	events []ActivityEvent // 新的在前
	seq    int
}

// NewActivityLog 创建动态日志。
func NewActivityLog() *ActivityLog {
	return &ActivityLog{events: make([]ActivityEvent, 0, 64)}
}

// Add 记一条事件（新的插最前，超 200 丢最老）。
func (al *ActivityLog) Add(kind, message, model, userID string, latencyMs, tokens int64, status int) {
	al.mu.Lock()
	defer al.mu.Unlock()
	al.seq++
	ev := ActivityEvent{
		Seq: al.seq, Time: time.Now().Format("15:04:05"), Kind: kind, Message: message,
		Model: model, UserID: userID, LatencyMs: latencyMs, Tokens: tokens, Status: status,
	}
	al.events = append([]ActivityEvent{ev}, al.events...)
	if len(al.events) > 200 {
		al.events = al.events[:200]
	}
}

// List 返回最近 limit 条（默认 100）。
func (al *ActivityLog) List(limit int) []ActivityEvent {
	al.mu.Lock()
	defer al.mu.Unlock()
	if limit <= 0 || limit > len(al.events) {
		limit = len(al.events)
	}
	out := make([]ActivityEvent, limit)
	copy(out, al.events[:limit])
	return out
}
