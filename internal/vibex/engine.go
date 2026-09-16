// engine.go —— 编排层：provider 解析、沙箱项目复用、一轮对话时序、会话亲和（P0-2）。
package vibex

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net"
	"strings"
	"sync"
	"time"
)

// logf 统一日志前缀（argv/头里的 token 一律不打印——坑 21）。
func logf(format string, args ...any) {
	log.Printf("vibex-plugin: "+format, args...)
}

// Engine 串起 REST/WS 客户端与并发控制。
type Engine struct {
	cfg    *Config
	client *Client
	pool   *Pool

	turnMu   sync.Mutex // 单 app 同时只跑一轮（对齐 turn_lock）
	connMu   sync.Mutex
	connSeq  int
	curProv  string
	lastApp  map[string]any
	sessions *sessionTable
}

// NewEngine 建编排层。
func NewEngine(cfg *Config, client *Client, pool *Pool) *Engine {
	return &Engine{cfg: cfg, client: client, pool: pool, sessions: newSessionTable(sessionIdleTTL)}
}

// nextFirstConnect：connect_mode=="auto" 时首次连接用 init_root，之后用 init。
func (e *Engine) nextFirstConnect() bool {
	if !strings.EqualFold(e.cfg.ConnectMode, "auto") {
		return false
	}
	e.connMu.Lock()
	defer e.connMu.Unlock()
	first := e.connSeq == 0
	e.connSeq++
	return first
}

// resolveModel 按 SPEC §3.8 解析 provider：请求指定 → 配置默认 → 首个 lite → 首个。
func (e *Engine) resolveModel(ctx context.Context, requested string) (map[string]any, error) {
	providers, err := e.client.providers(ctx)
	if err != nil {
		return nil, err
	}
	if len(providers) == 0 {
		return nil, &VibexError{Message: "未能获取 llm-providers 列表(为空)", Status: 502}
	}
	if requested != "" {
		if p := matchProvider(providers, requested); p != nil {
			return p, nil
		}
	}
	if e.cfg.Model != "" {
		if p := matchProvider(providers, e.cfg.Model); p != nil {
			return p, nil
		}
	}
	for _, raw := range providers {
		if m := toMap(raw); m != nil && str(m["billing_tier"]) == "lite" {
			return m, nil
		}
	}
	if m := toMap(providers[0]); m != nil {
		return m, nil
	}
	return nil, &VibexError{Message: "provider 列表元素非法", Status: 502}
}

// matchProvider 先按 id 精确匹配，再按 display_name/label/name 匹配。
func matchProvider(providers []any, want string) map[string]any {
	for _, raw := range providers {
		if m := toMap(raw); m != nil && providerID(m) == want {
			return m
		}
	}
	for _, raw := range providers {
		m := toMap(raw)
		if m == nil {
			continue
		}
		for _, k := range []string{"display_name", "label", "name"} {
			if want != "" && want == str(m[k]) {
				return m
			}
		}
	}
	return nil
}

func providerID(p map[string]any) string {
	if p == nil {
		return ""
	}
	return firstNonEmpty(str(p["id"]), str(p["provider_id"]))
}

// ensureApp 复用/创建沙箱项目 → 启动 → 绑定 provider（SPEC §3.9）。
func (e *Engine) ensureApp(ctx context.Context, provider map[string]any) (map[string]any, error) {
	pid := providerID(provider)
	name := e.cfg.AppName
	var target map[string]any
	apps, err := e.client.appsList(ctx)
	if err != nil {
		// 列表拿不到（网络抖动）不算致命：直接尝试创建会撞重名，故如实报错。
		return nil, err
	}
	for _, raw := range apps {
		m := toMap(raw)
		if m == nil {
			continue
		}
		if str(m["name"]) == name || str(m["app_id"]) == name {
			target = m
			break
		}
	}
	if target == nil {
		created, err := e.client.appCreate(ctx, name, pid)
		if err != nil {
			return nil, err
		}
		target = created
	}
	appID := str(target["app_id"])
	if appID == "" {
		return nil, &VibexError{Message: "无法从沙箱项目对象中确定 app_id", Status: 502}
	}
	started, err := e.client.appStart(ctx, appID)
	if err != nil {
		return nil, err
	}
	if started != nil {
		for k, v := range started {
			target[k] = v
		}
	}
	if e.curProv != pid {
		if err := e.client.setLLMProvider(ctx, appID, pid); err != nil {
			logf("set_llm_provider 失败(忽略): %v", err)
		} else {
			e.curProv = pid
		}
	}
	e.lastApp = target
	return target, nil
}

// ============================== 会话亲和（P0-2） ==============================

const sessionIdleTTL = 30 * time.Minute

type sessionEntry struct {
	sent     int
	lastUsed time.Time
}

// sessionTable 记录每个 session_id 已经发过多少条 messages（下一轮只发增量）。
type sessionTable struct {
	mu  sync.Mutex
	m   map[string]*sessionEntry
	ttl time.Duration
}

func newSessionTable(ttl time.Duration) *sessionTable {
	return &sessionTable{m: map[string]*sessionEntry{}, ttl: ttl}
}

// begin 决定本轮从第几条 message 开始打平、以及是否需要 new_session：
// 已知且未过期的 session → 只发增量且跳过 new_session；否则当新会话处理。
func (s *sessionTable) begin(id string, total int) (start int, fresh bool) {
	if id == "" {
		return 0, true
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	now := time.Now()
	ent := s.m[id]
	if ent != nil && now.Sub(ent.lastUsed) > s.ttl {
		delete(s.m, id)
		ent = nil
	}
	if ent == nil {
		return 0, true
	}
	if ent.sent >= total {
		// 没有增量（客户端重发了同一份历史，或换了更短的历史）：当新会话全量重发，
		// 否则会打出只有【任务】块的空 prompt。
		return 0, true
	}
	return ent.sent, false
}

// commit 记下本轮已发到第几条（只在本轮成功时调用）。
func (s *sessionTable) commit(id string, total int) {
	if id == "" {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.m[id] = &sessionEntry{sent: total, lastUsed: time.Now()}
}

// ============================== 一轮对话 ==============================

// turnRequest 是一轮所需的输入。
type turnRequest struct {
	provider     map[string]any
	prompt       string
	firstConnect bool
	freshSession bool
	sessionID    string
	totalMsgs    int
}

// turnState 承载本轮错误：由生产方在放哨兵**之前**写入，消费方在读到哨兵/通道关闭后读取。
type turnState struct {
	mu  sync.Mutex
	err string
}

func (s *turnState) setErr(msg string) {
	if msg == "" {
		return
	}
	s.mu.Lock()
	if s.err == "" {
		s.err = msg
	}
	s.mu.Unlock()
}

func (s *turnState) getErr() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.err
}

// turn 跑一轮：错误先写入 state，哨兵必须是最后一个动作
// （消费方被哨兵唤醒时 state 已就位，不会丢错误——对齐 python 自测第 8 组钉的时序）。
func (e *Engine) turn(ctx context.Context, req turnRequest, out chan outEvent, state *turnState) {
	defer func() { // 哨兵：最后执行；消费方已退出（ctx 取消）时不再阻塞
		select {
		case out <- outEvent{}:
		case <-ctx.Done():
		}
	}()
	defer func() {
		if r := recover(); r != nil {
			state.setErr(fmt.Sprintf("内部错误: %v", r))
		}
	}()
	e.turnMu.Lock()
	defer e.turnMu.Unlock()
	app, err := e.ensureApp(ctx, req.provider)
	if err != nil {
		state.setErr(err.Error())
		return
	}
	if err := e.pump(ctx, app, req, out); err != nil {
		state.setErr(err.Error())
		return
	}
	e.sessions.commit(req.sessionID, req.totalMsgs)
}

// pump 在 token 池上跑一轮 WS 对话：401/403/412 标死换号，402/429 冷却换号。
func (e *Engine) pump(ctx context.Context, app map[string]any, req turnRequest, out chan outEvent) error {
	appID := str(app["app_id"])
	if appID == "" {
		return &VibexError{Message: "app 对象缺少 app_id", Status: 502}
	}
	n := e.pool.Len()
	if n == 0 {
		return &VibexError{Message: tokenHint, Status: 401, Code: "TOKEN_NOT_CONFIGURED"}
	}
	var lastErr error
	for tried := 0; tried < n; tried++ {
		tok, ok := e.pool.acquire()
		if !ok {
			break
		}
		err := e.pumpWithToken(ctx, tok, appID, app, req, out)
		if err == nil {
			return nil
		}
		var he *wsHandshakeError
		if errors.As(err, &he) {
			switch he.Status {
			case 401, 403, 412:
				e.pool.markDead(tok)
				lastErr = err
				continue
			case 402, 429:
				e.pool.markCooldown(tok)
				lastErr = err
				continue
			}
		}
		return err
	}
	if lastErr != nil {
		return lastErr
	}
	return &VibexError{Message: "token 池中没有可用 token: " + tokenHint, Status: 401, Code: "TOKEN_EXHAUSTED"}
}

// errWSReadTimeout 表示这一小段读窗口内没有消息（不是错误，继续等）。
var errWSReadTimeout = errors.New("ws read timeout")

// pumpWithToken 用指定 token 跑完一轮（SPEC §3.7 时序）。
func (e *Engine) pumpWithToken(ctx context.Context, tok *tokenState, appID string, app map[string]any, req turnRequest, out chan outEvent) error {
	rawURL, err := wsURL(e.cfg.baseURL(), appID)
	if err != nil {
		return &VibexError{Message: err.Error(), Status: 502}
	}
	headers := map[string]string{
		"Cookie":     "Rh-Accesstoken=" + tok.Value,
		"Origin":     e.cfg.baseURL(),
		"User-Agent": upstreamUA,
	}
	ws, err := dialWS(ctx, rawURL, headers, 30*time.Second)
	if err != nil {
		var he *wsHandshakeError
		if errors.As(err, &he) {
			return err
		}
		return &VibexError{Message: "WS 连接失败: " + err.Error(), Status: 502}
	}
	// ping 保活（每 20s）+ 收尾：先停 ping，再关连接。
	pingStop := make(chan struct{})
	var pingWG sync.WaitGroup
	pingWG.Add(1)
	go func() {
		defer pingWG.Done()
		t := time.NewTicker(20 * time.Second)
		defer t.Stop()
		for {
			select {
			case <-pingStop:
				return
			case <-t.C:
				if err := ws.writeText(`{"type":"ping"}`); err != nil {
					return
				}
			}
		}
	}()
	defer func() {
		close(pingStop)
		pingWG.Wait()
		ws.close()
	}()

	// (1) init_root（首次）或 init
	initMsg := map[string]any{"type": "init", "cwd": wsInitCwd, "replay": false}
	if req.firstConnect {
		initMsg = map[string]any{"type": "init_root", "cwd": wsInitCwd, "meta": initRootMeta(app), "replay": false}
	}
	if err := sendWS(ws, initMsg); err != nil {
		return &VibexError{Message: "WS 发送 init 失败: " + err.Error(), Status: 502}
	}

	// (2) 等 ready
	readyDeadline := time.Now().Add(e.cfg.readyTimeout())
	for waiting := true; waiting; {
		remain := time.Until(readyDeadline)
		if remain <= 0 {
			return &VibexError{Message: fmt.Sprintf("WS 未在 %.0fs 内就绪", e.cfg.readyTimeout().Seconds()), Status: 504}
		}
		msg, err := e.readWS(ctx, ws, minDuration(remain, 5*time.Second))
		if errors.Is(err, errWSReadTimeout) {
			continue
		}
		if err != nil {
			if errors.Is(err, errWSClosed) {
				return &VibexError{Message: "WS 在就绪前关闭", Status: 502}
			}
			return &VibexError{Message: "WS 读取失败: " + err.Error(), Status: 502}
		}
		m := toMap(jsonUnmarshal([]byte(msg)))
		if m == nil {
			continue
		}
		switch str(m["type"]) {
		case "ready":
			waiting = false
		case "error":
			return &VibexError{Message: "连接失败: " + firstNonEmpty(str(m["message"]), str(m["code"]), "unknown"), Status: 502}
		}
	}

	// (3) ensure-llm-settings（失败忽略）
	e.client.ensureLLMSettings(ctx, appID)
	// (4) new_session（fresh_session 时）
	if req.freshSession {
		if err := sendWS(ws, map[string]any{"type": "new_session"}); err != nil {
			return &VibexError{Message: "WS 发送 new_session 失败: " + err.Error(), Status: 502}
		}
	}
	// (5) prompt
	if err := sendWS(ws, map[string]any{"type": "prompt", "text": req.prompt, "permission_mode": "dontAsk"}); err != nil {
		return &VibexError{Message: "WS 发送 prompt 失败: " + err.Error(), Status: 502}
	}

	// (6) 事件泵
	deadline := time.Now().Add(e.cfg.turnTimeout())
	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		remain := time.Until(deadline)
		if remain <= 0 {
			_ = sendWS(ws, map[string]any{"type": "cancel"})
			if err := emit(ctx, out, outEvent{Kind: "note", Text: "已达到单轮超时上限, 已发送 cancel"}); err != nil {
				return err
			}
			_ = emit(ctx, out, outEvent{Kind: "done", Flags: map[string]any{"timeout": true}})
			return nil
		}
		msg, err := e.readWS(ctx, ws, minDuration(remain, 5*time.Second))
		if errors.Is(err, errWSReadTimeout) {
			continue
		}
		if err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			if err := emit(ctx, out, outEvent{Kind: "note", Text: "连接关闭"}); err != nil {
				return err
			}
			_ = emit(ctx, out, outEvent{Kind: "done", Flags: map[string]any{"ws_closed": true}})
			return nil
		}
		m := toMap(jsonUnmarshal([]byte(msg)))
		if m == nil {
			continue
		}
		switch str(m["type"]) {
		case "claude_event":
			for _, ev := range claudeEvents(m["event"], e.cfg) {
				if err := emit(ctx, out, ev); err != nil {
					return err
				}
			}
		case "done":
			flags := map[string]any{}
			for _, k := range []string{"context_overflow", "session_poison", "balance_insufficient",
				"rate_limit_failover", "failover_model", "primary_model"} {
				flags[k] = m[k]
			}
			return emit(ctx, out, outEvent{Kind: "done", Flags: flags})
		case "error":
			// ready 之后的 error = 轮内错误，记下继续泵（对齐 SPEC §3.5）
			if err := emit(ctx, out, outEvent{Kind: "error",
				Text: firstNonEmpty(str(m["message"]), str(m["code"]), "unknown error")}); err != nil {
				return err
			}
		}
		// ready / pong / context_compacting / 其它 → 忽略
	}
}

// readWS 读一条消息；读超时归一到 errWSReadTimeout。
func (e *Engine) readWS(ctx context.Context, ws *wsConn, timeout time.Duration) (string, error) {
	if err := ctx.Err(); err != nil {
		return "", err
	}
	msg, err := ws.readMessage(timeout)
	if err != nil {
		var ne net.Error
		if errors.As(err, &ne) && ne.Timeout() {
			return "", errWSReadTimeout
		}
		return "", err
	}
	return msg, nil
}

// sendWS 发一条 JSON 消息。
func sendWS(ws *wsConn, msg map[string]any) error {
	b, err := json.Marshal(msg)
	if err != nil {
		return err
	}
	return ws.writeText(string(b))
}

// emit 投递事件（尊重 ctx 取消，避免消费方退出后生产方卡死）。
func emit(ctx context.Context, out chan<- outEvent, ev outEvent) error {
	select {
	case out <- ev:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func minDuration(a, b time.Duration) time.Duration {
	if a < b {
		return a
	}
	return b
}
