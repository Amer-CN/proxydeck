// server.go —— 插件 HTTP 层：/health、/v1/models（5 分钟缓存 + ?refresh=1 旁路）、
// /v1/chat/completions（OpenAI 兼容，SSE 真流式）。协议见 SPEC-T1 §3.11。
package vibex

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"
)

const (
	// pluginVersion 是插件自身版本（与 ProxyDeck 版本串无关，不发版不动）。
	pluginVersion = "1.0.0"
	// modelsTTL 是 /v1/models 的内存缓存时长（客户端会高频 poll，绝不透传上游）。
	modelsTTL = 5 * time.Minute
)

// modelItem 是 /v1/models 的单条（字段照抄 SPEC §3.11）。
type modelItem struct {
	ID      string         `json:"id"`
	Object  string         `json:"object"`
	Created int            `json:"created"`
	OwnedBy string         `json:"owned_by"`
	Vibex   map[string]any `json:"vibex"`
}

// Server 是本插件的服务壳。
type Server struct {
	cfg    *Config
	pool   *Pool
	client *Client
	engine *Engine
	queue  chan struct{}

	startedAt time.Time

	tokenMu sync.Mutex // token 入池落盘串行化（配置文件整份读写）

	modelsMu sync.Mutex
	models   []modelItem
	modelsAt time.Time

	ln  net.Listener
	srv *http.Server
}

// NewServer 读配置建服务（token 三选一：flag / env RH_ACCESSTOKEN / 配置文件）。
func NewServer() *Server {
	cfg := LoadConfig()
	pool := NewPool(cfg.tokenSources(), tokenCooldown)
	client := NewClient(cfg, pool)
	return &Server{
		cfg:       cfg,
		pool:      pool,
		client:    client,
		engine:    NewEngine(cfg, client, pool),
		queue:     make(chan struct{}, cfg.QueueLimit),
		startedAt: time.Now(),
	}
}

// corsWith 给所有响应加 CORS 头（GUI 从 localhost:随机端口 fetch 本端口属跨域）。
func corsWith(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization, X-Vibex-Session")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		h.ServeHTTP(w, r)
	})
}

// handler 组装路由（Start 与单测共用）。
func (s *Server) handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/accounts", s.handleAccounts)
	mux.HandleFunc("/v1/models", s.handleModels)
	mux.HandleFunc("/v1/chat/completions", s.handleChat)
	return corsWith(mux)
}

// Start 监听并服务（阻塞）。日志一律不含 token 本体。
func (s *Server) Start(host, port string) error {
	ln, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return fmt.Errorf("端口 %s 被占用: %w", port, err)
	}
	s.ln = ln
	s.srv = &http.Server{Handler: s.handler()}
	logf("listening on %s:%s (base_url=%s tokens=%d token_configured=%v)",
		host, port, s.cfg.baseURL(), s.pool.Len(), s.pool.Len() > 0)
	return s.srv.Serve(ln)
}

// Close 停服（测试与 GUI 熄火用）。
func (s *Server) Close() {
	if s.srv != nil {
		_ = s.srv.Close()
	}
}

// handleHealth 常驻 200（无 token 也是 200，token_configured:false），字段脱敏。
func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	tokens := s.pool.snapshot()
	out := map[string]any{
		"status":           "ok",
		"service":          "vibex-go",
		"version":          pluginVersion,
		"token_configured": s.pool.Len() > 0,
		"tokens":           tokens,
		"base_url":         s.cfg.baseURL(),
		"model_default":    nonEmptyOrNil(s.cfg.Model),
		"app_name":         s.cfg.AppName,
		"uptimeSec":        int64(time.Since(s.startedAt).Seconds()),
	}
	// days_left_max 是甲板账号行的三态依据（未配置/剩 X 天/已过期），
	// 取池中未过期 token 的最大剩余天数；无带 exp 的 token 时省略该键。
	if d, ok := s.pool.maxDaysLeft(); ok {
		out["days_left_max"] = d
		out["days_left"] = d // 别名：单 token 场景下两者同值
	}
	writeJSON(w, http.StatusOK, out)
}

// invalidTokenMsg 是入池前预验证失败（探活不通/401）的固定报错文案。
const invalidTokenMsg = "该 token 无效或已过期（GET /api/llm-providers 探活失败）" +
	"——请在浏览器登录 vibex.runninghub.cn 后重新提取 Rh-Accesstoken"

// autoWaitTimeout 是自动探测第③段「等用户登录」的长轮询预算（照抄团结的 150 秒）。
// 包级变量：单测改成毫秒级，不必真等两分半。
var autoWaitTimeout = 150 * time.Second

// handleAccounts 账号管理（与团结 handleAccounts 同构）：
//
//	GET  → 池快照（脱敏，绝不含 token 本体）+ days_left
//	POST → {"action":"auto"} 自动探测入池；{"action":"add","token":"..."} 手动粘贴兜底
func (s *Server) handleAccounts(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		out := map[string]any{
			"ok": true, "action": "list", "tokens": s.pool.snapshot(),
			"pool_size": s.pool.Len(), "token_configured": s.pool.Len() > 0,
		}
		if d, ok := s.pool.maxDaysLeft(); ok {
			out["days_left"] = d
		}
		writeJSON(w, http.StatusOK, out)
		return
	}
	if r.Method != http.MethodPost {
		writeJSON(w, http.StatusMethodNotAllowed, map[string]any{"ok": false, "msg": "只接受 GET/POST"})
		return
	}
	var req struct {
		Action string `json:"action"` // auto | add（缺省 add）
		Token  string `json:"token"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		// 请求体不是 JSON 对象 = 请求错误，与旧 POST /token 同口径回 400
		writeJSON(w, http.StatusBadRequest, map[string]any{"ok": false, "msg": "请求体解析失败"})
		return
	}
	switch req.Action {
	case "auto":
		s.handleAccountAuto(w, r)
	case "add", "":
		s.addAccount(w, r, strings.TrimSpace(req.Token))
	default:
		writeJSON(w, http.StatusOK, map[string]any{"ok": false, "msg": "未知 action"})
	}
}

// addAccount 手动粘贴兜底：JWT 解码 → providers 探活 → 入池 → 落盘（0600）→ 热加载。
// 非法 token 一律 400（与 POST /token 的旧行为一致）；日志不打印 token 本体（坑 21）。
func (s *Server) addAccount(w http.ResponseWriter, r *http.Request, token string) {
	info := decodeJWT(token)
	if token == "" || info == nil {
		writeOAIError(w, http.StatusBadRequest,
			"不是合法的 JWT（须是 Rh-Accesstoken 整串，形如 aaa.bbb.ccc）", "INVALID_TOKEN")
		return
	}
	if err := s.client.validateToken(r.Context(), token); err != nil {
		writeOAIError(w, http.StatusBadRequest, invalidTokenMsg+"（"+err.Error()+"）", "INVALID_TOKEN")
		return
	}
	st := &tokenState{Value: token, Exp: info.Exp, HasExp: info.hasExp()}
	_, existing := s.pool.addWithState(st)
	if existing {
		writeJSON(w, http.StatusOK, map[string]any{
			"ok": false, "existing": true, "msg": "该 token 已在池里",
			"user": info.Username, "pool_size": s.pool.Len(),
		})
		return
	}
	resp := map[string]any{
		"ok": true, "existing": false, "msg": "账号已添加",
		"user": info.Username, "pool_size": s.pool.Len(),
	}
	// tokenMu：落盘串行化（配置文件是整份读写，并发写会互相盖掉）
	s.tokenMu.Lock()
	err := s.cfg.persistToken(token)
	s.tokenMu.Unlock()
	if err != nil {
		// 落盘失败不回滚内存态（本次能用），但如实告知——重启后会丢
		resp["persisted"] = false
		resp["warning"] = "token 已热加载，但写入配置文件失败: " + err.Error()
		logf("token 落盘失败（重启后会丢，token 本体不打印）: %v", err)
	} else {
		resp["persisted"] = true
		logf("账号已入池（手动粘贴，池 %d 个，token 本体不打印）", s.pool.Len())
	}
	now := time.Now()
	if info.hasExp() {
		resp["exp"] = info.Exp.Format(time.RFC3339)
		resp["days_left"] = daysLeft(info.Exp, now)
		resp["expired"] = now.After(info.Exp)
	}
	writeJSON(w, http.StatusOK, resp)
}

// handleAccountAuto 三段式自动探测（照抄团结 handleAccountAuto）：
// ①直接探测（浏览器已带调试口在跑）；②探不到则一次性 profile＋独立端口拉浏览器开登录页；
// ③150 秒长轮询等登录（2 秒一次，只盯本会话端口，客户端断开即停）。
// vibex 换掉的两处：cookie 名 Rh-Accesstoken + 域 vibex.runninghub.cn；
// 校验用 GET /api/llm-providers 探活（不用团结的 cli-api-key 换取那套）。
func (s *Server) handleAccountAuto(w http.ResponseWriter, r *http.Request) {
	// 1. 直接探测（浏览器可能已带调试口在跑）。
	//    读到已在池的账号时不拦截——fall through 到弹窗流程，让用户可以登新号。
	//    token 无效（过期/被吊销）也不拦截——同样 fall through 让用户重新登录。
	if creds := probeCDPBrowser(); creds != nil {
		if err := s.client.validateToken(r.Context(), creds.Token); err != nil {
			logf("自动探测到 VibeX 登录态但探活失败，继续弹窗流程以便用户重新登录: %v", err)
		} else if _, existing := s.pool.addWithState(tokenStateOf(creds.Token)); !existing {
			s.persistNewToken(creds.Token)
			logf("自动探测成功（浏览器已开调试口，来源=%s 池 %d 个）", creds.Source, s.pool.Len())
			writeJSON(w, http.StatusOK, map[string]any{
				"ok": true, "msg": "已从浏览器读取并添加账号",
				"user_id": creds.UserID, "browser": creds.Browser, "pool_size": s.pool.Len(),
			})
			return
		} else {
			logf("自动探测到 %s 已在池里，继续弹窗流程以便用户登新号", creds.UserID)
		}
	}
	// 2. 探测不到 → 拉起【一次性独立 profile】浏览器带调试口（136+ 版本安全限制：
	//    默认 profile 忽略调试参数，必须独立 user-data-dir），打开 VibeX 登录页。
	//    每次探测用全新目录 + 独立端口，登录态不跨会话复用。
	cleanupStaleProbeProfiles()
	port := freeCDPPort()
	path, err := cdpLaunchBrowser(port, newProbeSessionDir())
	if err != nil {
		writeJSON(w, http.StatusOK, map[string]any{
			"ok": false, "msg": "未探测到调试口，且启动浏览器失败: " + err.Error()})
		return
	}
	// 3. 长轮询等登录：一次性 profile 是全新环境，需要在弹出的窗口里登录一次；
	//    登录后 cookie/localStorage 写入该 profile，读取入池。只盯本会话端口，
	//    避免读到其它窗口旧登录态。最长 autoWaitTimeout（缺省 150 秒），每 2 秒查一次。
	deadline := time.Now().Add(autoWaitTimeout)
	for time.Now().Before(deadline) {
		if err := sleepCtx(r.Context(), 2*time.Second); err != nil {
			return // 客户端断开，不再等
		}
		creds := probeCDPBrowserQuiet(port)
		if creds == nil {
			continue
		}
		if err := s.client.validateToken(r.Context(), creds.Token); err != nil {
			logf("探测窗口读到的登录态探活失败（等下一次轮询）: %v", err)
			continue
		}
		if _, existing := s.pool.addWithState(tokenStateOf(creds.Token)); existing {
			writeJSON(w, http.StatusOK, map[string]any{
				"ok": false, "msg": "该账号已在池里（user_id " + creds.UserID + "）",
				"user_id": creds.UserID, "pool_size": s.pool.Len(),
			})
			return
		}
		s.persistNewToken(creds.Token)
		logf("自动探测成功：探测窗口可以关了（port=%s user_id=%s）", port, creds.UserID)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok": true, "msg": "已读取登录态并入池（弹出的浏览器窗口可以关了）",
			"user_id": creds.UserID, "browser": path, "pool_size": s.pool.Len(),
		})
		return
	}
	logf("自动探测超时：探测窗口可以关了（port=%s，150 秒未读到期）", port)
	writeJSON(w, http.StatusOK, map[string]any{
		"ok": false,
		"msg": "等待登录超时（150 秒）——请在弹出的浏览器窗口里登录 VibeX 账号后再点一次探测"})
}

// tokenStateOf 用一个 token 串建 tokenState（JWT exp 现解）。
func tokenStateOf(token string) *tokenState {
	st := &tokenState{Value: token}
	if info := decodeJWT(token); info != nil {
		st.Exp, st.HasExp = info.Exp, info.hasExp()
	}
	return st
}

// persistNewToken 把刚入池的 token 落盘（失败只记日志，不回滚内存态）。
func (s *Server) persistNewToken(token string) {
	s.tokenMu.Lock()
	err := s.cfg.persistToken(token)
	s.tokenMu.Unlock()
	if err != nil {
		logf("token 落盘失败（重启后会丢，token 本体不打印）: %v", err)
	}
}

// handleModels 返回 provider 列表（OpenAI models 形状），带 5 分钟缓存。
func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	if s.pool.Len() == 0 {
		writeOAIError(w, http.StatusUnauthorized, tokenHint, "TOKEN_NOT_CONFIGURED")
		return
	}
	refresh := r.URL.Query().Get("refresh") == "1"
	items, err := s.modelsCached(r.Context(), refresh)
	if err != nil {
		writeVibexErr(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"object": "list", "data": items})
}

// modelsCached 命中缓存直接回；否则现拉一次 provider 列表并刷新缓存（错误不写缓存）。
func (s *Server) modelsCached(ctx context.Context, refresh bool) ([]modelItem, error) {
	s.modelsMu.Lock()
	if !refresh && s.models != nil && time.Since(s.modelsAt) < modelsTTL {
		out := s.models
		s.modelsMu.Unlock()
		return out, nil
	}
	s.modelsMu.Unlock()

	providers, err := s.client.providers(ctx)
	if err != nil {
		return nil, err
	}
	if len(providers) == 0 {
		return nil, &VibexError{Message: "provider 列表为空", Status: 502}
	}
	items := make([]modelItem, 0, len(providers))
	for _, raw := range providers {
		p := toMap(raw)
		id := providerID(p)
		if id == "" {
			continue
		}
		items = append(items, modelItem{
			ID: id, Object: "model", Created: 0, OwnedBy: "vibex",
			Vibex: map[string]any{
				"display_name": firstNonEmpty(str(p["display_name"]), str(p["label"]), str(p["name"])),
				"billing_tier": p["billing_tier"],
				"description":  p["description"],
			},
		})
	}
	s.modelsMu.Lock()
	s.models, s.modelsAt = items, time.Now()
	s.modelsMu.Unlock()
	return items, nil
}

// handleChat 跑一轮对话（流式 / 非流式）。
func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	raw, _ := io.ReadAll(io.LimitReader(r.Body, 32<<20))
	if len(strings.TrimSpace(string(raw))) == 0 {
		writeOAIError(w, http.StatusBadRequest, "请求体必须是 JSON 对象", "BAD_REQUEST")
		return
	}
	var body map[string]any
	if json.Unmarshal(raw, &body) != nil || body == nil {
		writeOAIError(w, http.StatusBadRequest, "请求体必须是 JSON 对象", "BAD_REQUEST")
		return
	}
	messages, ok := body["messages"].([]any)
	if !ok || len(messages) == 0 {
		writeOAIError(w, http.StatusBadRequest, "messages 必须是非空数组", "BAD_REQUEST")
		return
	}
	stream, _ := body["stream"].(bool)
	if s.pool.Len() == 0 {
		writeOAIError(w, http.StatusUnauthorized, tokenHint, "TOKEN_NOT_CONFIGURED")
		return
	}

	requested, _ := body["model"].(string)
	provider, err := s.engine.resolveModel(r.Context(), requested)
	if err != nil {
		writeVibexErr(w, err)
		return
	}
	modelName := firstNonEmpty(requested, providerID(provider), "vibex")

	// 排队上限：带计数的通道信号量做非阻塞取槽（无先判后取的 TOCTOU）。
	select {
	case s.queue <- struct{}{}:
		defer func() { <-s.queue }()
	default:
		writeOAIError(w, http.StatusTooManyRequests, "当前排队已达上限, 请稍后重试", "QUEUE_FULL")
		return
	}

	// 会话亲和（P0-2）：有 session_id 且已知 → 跳过 new_session、只发增量 messages。
	sessionID := strings.TrimSpace(r.Header.Get("X-Vibex-Session"))
	if sessionID == "" {
		if v, ok := body["session_id"].(string); ok {
			sessionID = strings.TrimSpace(v)
		}
	}
	startIdx, fresh := s.engine.sessions.begin(sessionID, len(messages))
	if startIdx > len(messages) {
		startIdx = 0
	}
	prompt := buildPrompt(messages[startIdx:], s.cfg.ChatInstruction)

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()
	out := make(chan outEvent)
	state := &turnState{}
	go func() {
		defer close(out)
		s.engine.turn(ctx, turnRequest{
			provider:     provider,
			prompt:       prompt,
			firstConnect: s.engine.nextFirstConnect(),
			freshSession: fresh && s.cfg.FreshSession,
			sessionID:    sessionID,
			totalMsgs:    len(messages),
		}, out, state)
	}()

	if stream {
		s.streamOut(w, ctx, cancel, out, state, modelName)
		return
	}
	s.syncOut(w, out, state, modelName)
}

// syncOut 非流式：聚合文本返回标准 completion（SPEC §3.11）。
func (s *Server) syncOut(w http.ResponseWriter, out chan outEvent, state *turnState, modelName string) {
	var events []outEvent
	for ev := range out {
		if ev.Kind == "" {
			break
		}
		events = append(events, ev)
	}
	content, usage, flags, lastErr := foldSync(events)
	if e := state.getErr(); e != "" && lastErr == "" {
		lastErr = e
	}
	if content == "" {
		if truthy(flags["balance_insufficient"]) {
			writeOAIError(w, http.StatusPaymentRequired, "上游余额不足, 无法完成本轮对话", "BALANCE_INSUFFICIENT")
			return
		}
		if lastErr != "" {
			writeOAIError(w, http.StatusBadGateway, lastErr, "UPSTREAM_ERROR")
			return
		}
		writeOAIError(w, http.StatusBadGateway, "本轮未产生任何内容", "EMPTY_TURN")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"id": "chatcmpl-" + randHex(12), "object": "chat.completion",
		"created": time.Now().Unix(), "model": modelName,
		"choices": []any{map[string]any{
			"index": 0, "message": map[string]any{"role": "assistant", "content": content},
			"finish_reason": "stop",
		}},
		"usage": oaiUsage(usage),
	})
}

// streamOut 流式：SSE 输出（首块 role → 增量 → 尾块 finish stop + usage → [DONE]）。
func (s *Server) streamOut(w http.ResponseWriter, ctx context.Context, cancel context.CancelFunc,
	out chan outEvent, state *turnState, modelName string) {
	flusher, _ := w.(http.Flusher)
	w.Header().Set("Content-Type", "text/event-stream; charset=utf-8")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("X-Accel-Buffering", "no")
	w.WriteHeader(http.StatusOK)
	if flusher != nil {
		flusher.Flush()
	}

	cid := "chatcmpl-" + randHex(12)
	created := time.Now().Unix()
	chunk := func(delta map[string]any, finish any) map[string]any {
		return map[string]any{
			"id": cid, "object": "chat.completion.chunk", "created": created, "model": modelName,
			"choices": []any{map[string]any{"index": 0, "delta": delta, "finish_reason": finish}},
		}
	}
	send := func(obj map[string]any) error {
		b, err := json.Marshal(obj)
		if err != nil {
			return err
		}
		if _, err := fmt.Fprintf(w, "data: %s\n\n", b); err != nil {
			return err
		}
		if flusher != nil {
			flusher.Flush()
		}
		return nil
	}
	// 写失败 = 客户端断了：取消本轮并排空事件通道，避免生产方卡在 send 上。
	abort := func() {
		cancel()
		go func() {
			for range out {
			}
		}()
	}

	if err := send(chunk(map[string]any{"role": "assistant"}, nil)); err != nil {
		abort()
		return
	}

	produced := false
	var assistantBuf []string
	var usage map[string]any
	flags := map[string]any{}
	lastErr := ""

	hb := s.cfg.heartbeat()
	timer := time.NewTimer(hb)
	defer timer.Stop()

loop:
	for {
		select {
		case ev, ok := <-out:
			if !ok || ev.Kind == "" {
				break loop
			}
			var werr error
			switch ev.Kind {
			case "text":
				if ev.Text != "" {
					produced = true
					werr = send(chunk(map[string]any{"content": ev.Text}, nil))
				}
			case "assistant_text":
				if ev.Text != "" {
					assistantBuf = append(assistantBuf, ev.Text) // text_delta 未覆盖时的兜底
				}
			case "thinking":
				if s.cfg.IncludeThinking && ev.Text != "" {
					werr = send(chunk(map[string]any{"reasoning_content": ev.Text}, nil))
				}
			case "tool", "note", "error_text":
				if ev.Text != "" {
					produced = true
					werr = send(chunk(map[string]any{"content": ev.Text}, nil))
				}
			case "result":
				usage = ev.Usage
			case "done":
				if ev.Flags != nil {
					flags = ev.Flags
				}
			case "error":
				if lastErr == "" {
					lastErr = ev.Text
				}
			}
			if werr != nil {
				abort()
				return
			}
		case <-timer.C:
			if _, err := io.WriteString(w, ": hb\n\n"); err != nil {
				abort()
				return
			}
			if flusher != nil {
				flusher.Flush()
			}
			timer.Reset(hb)
		case <-ctx.Done():
			break loop
		}
	}

	if e := state.getErr(); e != "" && lastErr == "" {
		lastErr = e
	}
	if !produced && len(assistantBuf) > 0 {
		produced = true
		if err := send(chunk(map[string]any{"content": strings.Join(assistantBuf, "")}, nil)); err != nil {
			return
		}
	}
	if !produced && lastErr != "" {
		_ = send(map[string]any{"error": map[string]any{
			"message": lastErr, "type": "vibex_error", "code": "UPSTREAM_ERROR"}})
		_, _ = io.WriteString(w, "data: [DONE]\n\n")
		return
	}
	if truthy(flags["balance_insufficient"]) {
		if err := send(chunk(map[string]any{"content": "\n[上游余额不足]"}, nil)); err != nil {
			return
		}
	}
	final := chunk(map[string]any{}, "stop")
	final["usage"] = oaiUsage(usage)
	if err := send(final); err != nil {
		return
	}
	if flusher != nil {
		flusher.Flush()
	}
	_, _ = io.WriteString(w, "data: [DONE]\n\n")
	if flusher != nil {
		flusher.Flush()
	}
}

// writeJSON 输出 JSON（UTF-8 原样，不转义中文）。
func writeJSON(w http.ResponseWriter, status int, v any) {
	b, err := json.Marshal(v)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	_, _ = w.Write(b)
}

// writeOAIError 输出统一的 OpenAI 风格错误（SPEC §3.11）。
func writeOAIError(w http.ResponseWriter, status int, message, code string) {
	writeJSON(w, status, map[string]any{"error": map[string]any{
		"message": message, "type": "vibex_error", "code": code}})
}

// writeVibexErr 把业务异常翻成 OpenAI 错误响应。
func writeVibexErr(w http.ResponseWriter, err error) {
	var ve *VibexError
	if errors.As(err, &ve) {
		status := ve.Status
		if status == 0 {
			status = http.StatusInternalServerError
		}
		writeOAIError(w, status, ve.Message, ve.Code)
		return
	}
	writeOAIError(w, http.StatusInternalServerError, err.Error(), "")
}

func nonEmptyOrNil(s string) any {
	if s == "" {
		return nil
	}
	return s
}
