// client.go —— VibeX 协议客户端：REST（net/http，带 token 池换号轮换）+ 零依赖手写 WS（RFC6455）。
package vibex

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha1"
	"crypto/tls"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"
)

// VibexError 是带 HTTP 状态码与错误码的统一业务异常。
type VibexError struct {
	Message string
	Status  int
	Code    string
}

func (e *VibexError) Error() string { return e.Message }

// tokenHint 是「没 token」时的统一指引（验收要求 message 含 Rh-Accesstoken 字样）。
const tokenHint = "未配置 VibeX token: 请重新从浏览器提取 Rh-Accesstoken, " +
	"写入 vibex-config.json 的 tokens 数组, 或设置环境变量 RH_ACCESSTOKEN, 或命令行 --vibex-token"

// restTimeout 是 REST 单请求总超时（本包自定；SPEC-T1 只规定 WS 连接 30s 与
// turn_timeout 单轮预算，无 120s 之说）。
const restTimeout = 120 * time.Second

// authFailHint 是上游拒绝 token 时的指引（同样要含 Rh-Accesstoken）。
const authFailHint = "鉴权失败: VibeX 拒绝了请求, 请重新从浏览器提取 Rh-Accesstoken 后更新配置"

// Client 持有配置、token 池与 HTTP 客户端。
type Client struct {
	cfg  *Config
	pool *Pool
	http *http.Client
}

// NewClient 建客户端。
func NewClient(cfg *Config, pool *Pool) *Client {
	return &Client{cfg: cfg, pool: pool, http: &http.Client{Timeout: restTimeout}}
}

// headers 构造 REST 鉴权头（SPEC-T1 §3.3）；token 不落日志（坑 21）。
func (c *Client) headers(tok *tokenState) map[string]string {
	token := ""
	if tok != nil {
		token = tok.Value
	}
	return c.headersFor(token)
}

// headersFor 用指定 token 串构造鉴权头（入池前预验证用，那时还没有 tokenState）。
func (c *Client) headersFor(token string) map[string]string {
	h := map[string]string{
		"Content-Type":   "application/json",
		"Authorization":  "Bearer " + token,
		"RH-TOKEN":       token,
		"X-Tenant-Id":    c.cfg.TenantID,
		"X-Vibex-Locale": "zh-CN",
		"Origin":         c.cfg.baseURL(),
		"Referer":        c.cfg.baseURL() + "/",
		"User-Agent":     upstreamUA,
	}
	if info := decodeJWT(token); info != nil && info.Sub != "" {
		h["userId"] = info.Sub
		h["username"] = info.Username
	}
	return h
}

// validateToken 入池前预验证：用给定 token 打一次 GET /api/llm-providers，
// 200 即有效（简报口径：JWT 解码 + providers 探活）。无效返回 *VibexError（401 级）。
// 不登记调用计数——该 token 此时还没入池。
func (c *Client) validateToken(ctx context.Context, token string) error {
	reqCtx, cancel := context.WithTimeout(ctx, restTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(reqCtx, http.MethodGet,
		c.cfg.baseURL()+"/api/llm-providers", nil)
	if err != nil {
		return &VibexError{Message: err.Error(), Status: 500}
	}
	for k, v := range c.headersFor(token) {
		req.Header.Set(k, v)
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return &VibexError{Message: "探活请求失败: " + err.Error(), Status: 502}
	}
	defer func() { _ = resp.Body.Close() }()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8<<10))
	if err := unauthErr(resp.StatusCode, raw); err != nil {
		return err
	}
	if resp.StatusCode >= 400 {
		return &VibexError{
			Message: fmt.Sprintf("探活失败 HTTP %d: %s", resp.StatusCode, clip(string(raw), 200)),
			Status:  502,
		}
	}
	return nil
}

// api 在 token 池上跑一次 REST 调用，必要时换号：
// 401/403/412 → 标死该 token 换下一个；402/429 → 冷却该 token 换下一个。
func (c *Client) api(ctx context.Context, method, path string, body any) (*tokenState, any, error) {
	n := c.pool.Len()
	if n == 0 {
		return nil, nil, &VibexError{Message: tokenHint, Status: 401, Code: "TOKEN_NOT_CONFIGURED"}
	}
	var lastErr error
	for tried := 0; tried < n; tried++ {
		tok, ok := c.pool.acquire()
		if !ok {
			break
		}
		v, err := c.do(ctx, tok, method, path, body)
		if err == nil {
			c.pool.incCalls(tok)
			return tok, v, nil
		}
		var ve *VibexError
		if errors.As(err, &ve) {
			switch ve.Status {
			case 401:
				c.pool.markDead(tok)
				lastErr = err
				continue
			case 402, 429:
				c.pool.markCooldown(tok)
				lastErr = err
				continue
			}
		}
		return nil, nil, err
	}
	if lastErr == nil {
		lastErr = &VibexError{
			Message: "token 池中没有可用 token（全部标死/冷却中/已过期）: " + tokenHint,
			Status:  401, Code: "TOKEN_EXHAUSTED",
		}
	}
	return nil, nil, lastErr
}

// do 单 token 发一次请求并做容错解包 + 未授权识别（SPEC-T1 §3.4）。
func (c *Client) do(ctx context.Context, tok *tokenState, method, path string, body any) (any, error) {
	var payload []byte
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return nil, &VibexError{Message: err.Error(), Status: 500}
		}
		payload = b
	}
	reqCtx, cancel := context.WithTimeout(ctx, restTimeout)
	defer cancel()
	var rd io.Reader
	if payload != nil {
		rd = bytes.NewReader(payload)
	}
	req, err := http.NewRequestWithContext(reqCtx, method, c.cfg.baseURL()+path, rd)
	if err != nil {
		return nil, &VibexError{Message: err.Error(), Status: 500}
	}
	for k, v := range c.headers(tok) {
		req.Header.Set(k, v)
	}
	resp, err := c.http.Do(req)
	if err != nil {
		if errors.Is(err, context.DeadlineExceeded) {
			return nil, &VibexError{Message: fmt.Sprintf("请求超时 %s %s", method, path), Status: 504}
		}
		return nil, &VibexError{Message: fmt.Sprintf("请求失败 %s %s: %v", method, path, err), Status: 502}
	}
	defer func() { _ = resp.Body.Close() }()
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	parsed := jsonUnmarshal(raw)

	// 未授权在 body 里：{"code":403,"msg":"TOKEN_MISSION"} 之类（HTTP 可能仍是 200）
	if err := unauthErr(resp.StatusCode, raw); err != nil {
		return nil, err
	}
	switch {
	case resp.StatusCode == 402 || resp.StatusCode == 429:
		return nil, &VibexError{
			Message: fmt.Sprintf("上游返回 HTTP %d: %s", resp.StatusCode, clip(string(raw), 200)),
			Status:  resp.StatusCode, Code: "BALANCE_OR_LIMIT",
		}
	case resp.StatusCode >= 400:
		return nil, &VibexError{
			Message: fmt.Sprintf("接口 %s 返回 HTTP %d: %s", path, resp.StatusCode, clip(string(raw), 200)),
			Status:  502,
		}
	}
	return parsed, nil
}

// unauthErr 识别未授权（SPEC-T1 §3.4）：body 里的 code 401/403/412 或含 TOKEN 的 msg
// （HTTP 可能仍是 200），以及 HTTP 401/403/412 本身。命中返回 401 级 *VibexError。
// do（单 token 请求）与 validateToken（入池前预验证）共用，保证两条路径口径一致。
func unauthErr(status int, raw []byte) error {
	if m := toMap(jsonUnmarshal(raw)); m != nil {
		code := str(m["code"])
		msg := str(m["msg"])
		if msg == "" {
			msg = str(m["message"])
		}
		switch code {
		case "401", "403", "412":
			return &VibexError{Message: authFailHint + "（" + firstNonEmpty(msg, code) + "）", Status: 401, Code: "TOKEN_INVALID"}
		case "402":
			return &VibexError{Message: "上游返回额度不足: " + clip(msg, 200), Status: 402, Code: "BALANCE_INSUFFICIENT"}
		case "429":
			return &VibexError{Message: "上游限流: " + clip(msg, 200), Status: 429, Code: "RATE_LIMITED"}
		}
		if strings.Contains(strings.ToUpper(msg), "TOKEN") {
			return &VibexError{Message: authFailHint + "（" + clip(msg, 200) + "）", Status: 401, Code: "TOKEN_INVALID"}
		}
	}
	if status == 401 || status == 403 || status == 412 {
		return &VibexError{Message: authFailHint, Status: 401, Code: "TOKEN_INVALID"}
	}
	return nil
}

// providers 取 llm-providers 列表（容错解包 .providers/.items/.data）。
func (c *Client) providers(ctx context.Context) ([]any, error) {
	_, v, err := c.api(ctx, http.MethodGet, "/api/llm-providers", nil)
	if err != nil {
		return nil, err
	}
	if list, ok := v.([]any); ok {
		return list, nil
	}
	if m := toMap(v); m != nil {
		for _, k := range []string{"providers", "items", "data"} {
			if list, ok := m[k].([]any); ok {
				return list, nil
			}
			// 二层兜底：形如 {"code":0,"data":{"providers":[...]}}（appsFrom 同款容错）
			if inner := toMap(m[k]); inner != nil {
				for _, k2 := range []string{"providers", "items", "data", "list"} {
					if list, ok := inner[k2].([]any); ok {
						return list, nil
					}
				}
			}
		}
	}
	// 未解出列表：记顶层结构自证（只记键名/类型与数量，绝不记 body 与 token），
	// “0 MODELS”时看这条即知是上游真空还是嵌套格式漏网。
	m := toMap(v)
	logf("providers 上游 200 但未解出列表（结构=%s code=%v msg=%.120s）", shapeOf(v), m["code"], str(m["msg"]))
	return nil, nil
}

// appsList 取沙箱项目列表。
func (c *Client) appsList(ctx context.Context) ([]any, error) {
	_, v, err := c.api(ctx, http.MethodGet, "/api/apps", nil)
	if err != nil {
		return nil, err
	}
	return appsFrom(v), nil
}

// appGet 取单个沙箱项目对象。
func (c *Client) appGet(ctx context.Context, appID string) (map[string]any, error) {
	_, v, err := c.api(ctx, http.MethodGet, "/api/apps/"+url.PathEscape(appID), nil)
	if err != nil {
		return nil, err
	}
	return toMap(unwrapObj(v)), nil
}

// appCreate 新建沙箱项目；FREE_PROJECT_LIMIT → 409。
func (c *Client) appCreate(ctx context.Context, name, providerID string) (map[string]any, error) {
	body := map[string]any{
		"name": name, "pinned_host_ip": nil, "llm_provider_id": providerID,
		"app_type": c.cfg.AppType, "flow": nil, "enabled_capabilities": nil,
	}
	_, v, err := c.api(ctx, http.MethodPost, "/api/apps", body)
	if err != nil {
		return nil, err
	}
	if m := toMap(v); m != nil {
		if code := firstNonEmpty(str(m["code"]), "0"); code != "0" {
			dump, _ := json.Marshal(m)
			if strings.Contains(string(dump), "FREE_PROJECT_LIMIT") {
				return nil, &VibexError{Message: "免费项目已达上限(约10个), 请在 VibeX 网页端删除旧项目后重试", Status: 409, Code: "FREE_PROJECT_LIMIT"}
			}
			return nil, &VibexError{Message: "创建项目失败: " + clip(firstNonEmpty(str(m["msg"]), str(m["message"]), code), 200), Status: 502}
		}
	}
	return toMap(unwrapObj(v)), nil
}

// appStart POST start 后轮询到 running 且端口就绪（每 2s，最长 240s；超时但已 running 也放行）。
func (c *Client) appStart(ctx context.Context, appID string) (map[string]any, error) {
	if _, _, err := c.api(ctx, http.MethodPost, "/api/apps/"+url.PathEscape(appID)+"/start", map[string]any{}); err != nil {
		return nil, err
	}
	deadline := time.Now().Add(240 * time.Second)
	var last map[string]any
	for time.Now().Before(deadline) {
		app, err := c.appGet(ctx, appID)
		if err == nil && app != nil {
			last = app
			if liveStatus(app) == "running" && portsOK(app) {
				return app, nil
			}
		}
		if err := sleepCtx(ctx, 2*time.Second); err != nil {
			return nil, &VibexError{Message: err.Error(), Status: 502}
		}
	}
	if last != nil && liveStatus(last) == "running" {
		return last, nil
	}
	return nil, &VibexError{Message: fmt.Sprintf("项目 %s 启动超时(240s)", appID), Status: 504, Code: "APP_START_TIMEOUT"}
}

// setLLMProvider 绑定模型通道。
func (c *Client) setLLMProvider(ctx context.Context, appID, providerID string) error {
	_, _, err := c.api(ctx, http.MethodPatch, "/api/apps/"+url.PathEscape(appID)+"/llm-provider",
		map[string]any{"provider_id": providerID, "llm_test": false})
	return err
}

// ensureLLMSettings 让上游把 LLM 配置写进沙箱（失败仅告警，不抛）。
func (c *Client) ensureLLMSettings(ctx context.Context, appID string) {
	_, _, err := c.api(ctx, http.MethodPost, "/api/apps/"+url.PathEscape(appID)+"/ensure-llm-settings", map[string]any{})
	if err != nil {
		logf("ensure-llm-settings 调用失败(忽略): %v", err)
	}
}

// ============================== WS 客户端（RFC6455，零依赖手写） ==============================

const (
	wsOpContinuation = 0x0
	wsOpText         = 0x1
	wsOpBinary       = 0x2
	wsOpClose        = 0x8
	wsOpPing         = 0x9
	wsOpPong         = 0xA

	wsMaxPayload = 64 << 20 // 单帧/单消息上限，防上游异常帧撑爆内存
	wsGUID       = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
)

// errWSClosed 表示对端发了 close 帧或连接已断。
var errWSClosed = errors.New("ws 连接关闭")

// wsHandshakeError 是握手阶段非 101 的应答（401 → 该 token 标死换号）。
type wsHandshakeError struct {
	Status int
	Body   string
}

func (e *wsHandshakeError) Error() string {
	return fmt.Sprintf("WS 握手失败: HTTP %d %s", e.Status, clip(e.Body, 200))
}

// wsConn 是手写的最小 WS 客户端连接。
type wsConn struct {
	conn net.Conn
	br   *bufio.Reader
	wmu  sync.Mutex
}

// wsURL 由 base_url 推导 WS 地址（SPEC §3.5：wss://<host>/app-ws/{app_id}/ws，不带 base path）。
func wsURL(baseURL, appID string) (string, error) {
	u, err := url.Parse(baseURL)
	if err != nil {
		return "", err
	}
	if u.Host == "" {
		return "", fmt.Errorf("base_url 非法: %s", baseURL)
	}
	scheme := "ws"
	if u.Scheme == "https" || u.Scheme == "wss" {
		scheme = "wss"
	}
	return scheme + "://" + u.Host + "/app-ws/" + url.PathEscape(appID) + "/ws", nil
}

// dialWS 建立 WS 连接（握手鉴权只带 Cookie: Rh-Accesstoken）。
func dialWS(ctx context.Context, rawURL string, header map[string]string, timeout time.Duration) (*wsConn, error) {
	u, err := url.Parse(rawURL)
	if err != nil {
		return nil, err
	}
	addr := u.Host
	if u.Port() == "" {
		port := "80"
		if u.Scheme == "wss" {
			port = "443"
		}
		addr = net.JoinHostPort(u.Hostname(), port)
	}
	d := net.Dialer{Timeout: timeout}
	conn, err := d.DialContext(ctx, "tcp", addr)
	if err != nil {
		return nil, err
	}
	if u.Scheme == "wss" {
		tc := tls.Client(conn, &tls.Config{ServerName: u.Hostname(), MinVersion: tls.VersionTLS12})
		if err := tc.HandshakeContext(ctx); err != nil {
			_ = conn.Close()
			return nil, err
		}
		conn = tc
	}
	keyRaw := make([]byte, 16)
	if _, err := rand.Read(keyRaw); err != nil {
		_ = conn.Close()
		return nil, err
	}
	key := base64.StdEncoding.EncodeToString(keyRaw)
	path := u.RequestURI()
	if path == "" {
		path = "/"
	}
	var sb strings.Builder
	fmt.Fprintf(&sb, "GET %s HTTP/1.1\r\nHost: %s\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"+
		"Sec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n", path, u.Host, key)
	for k, v := range header {
		fmt.Fprintf(&sb, "%s: %s\r\n", k, v)
	}
	sb.WriteString("\r\n")
	_ = conn.SetDeadline(time.Now().Add(timeout))
	if _, err := io.WriteString(conn, sb.String()); err != nil {
		_ = conn.Close()
		return nil, err
	}
	br := bufio.NewReaderSize(conn, 32*1024)
	resp, err := http.ReadResponse(br, &http.Request{Method: http.MethodGet, URL: u, Host: u.Host, Header: http.Header{}})
	if err != nil {
		_ = conn.Close()
		return nil, err
	}
	if resp.StatusCode != http.StatusSwitchingProtocols {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 512))
		_ = resp.Body.Close()
		_ = conn.Close()
		return nil, &wsHandshakeError{Status: resp.StatusCode, Body: string(body)}
	}
	if got := resp.Header.Get("Sec-WebSocket-Accept"); got != wsAccept(key) {
		_ = conn.Close()
		return nil, fmt.Errorf("ws 握手校验失败: Sec-WebSocket-Accept=%q", got)
	}
	_ = conn.SetDeadline(time.Time{})
	return &wsConn{conn: conn, br: br}, nil
}

func wsAccept(key string) string {
	h := sha1.Sum([]byte(key + wsGUID))
	return base64.StdEncoding.EncodeToString(h[:])
}

// writeText 发一帧 masked text（客户端必须 mask）。
func (c *wsConn) writeText(s string) error {
	c.wmu.Lock()
	defer c.wmu.Unlock()
	return writeFrame(c.conn, wsOpText, []byte(s), true)
}

// writeControl 发控制帧（pong/close）。
func (c *wsConn) writeControl(op byte, payload []byte) error {
	c.wmu.Lock()
	defer c.wmu.Unlock()
	return writeFrame(c.conn, op, payload, true)
}

// readMessage 读一条完整消息（自动回 pong、拼续帧）；timeout<=0 表示不设读超时。
func (c *wsConn) readMessage(timeout time.Duration) (string, error) {
	if timeout > 0 {
		_ = c.conn.SetReadDeadline(time.Now().Add(timeout))
	} else {
		_ = c.conn.SetReadDeadline(time.Time{})
	}
	var payload []byte
	for {
		op, fin, data, err := readFrame(c.br)
		if err != nil {
			return "", err
		}
		switch op {
		case wsOpPing:
			_ = c.writeControl(wsOpPong, data)
		case wsOpPong:
			// 忽略
		case wsOpClose:
			_ = c.writeControl(wsOpClose, nil)
			return "", errWSClosed
		case wsOpText, wsOpBinary, wsOpContinuation:
			payload = append(payload, data...)
			if len(payload) > wsMaxPayload {
				return "", fmt.Errorf("ws 消息超过 %d 字节上限", wsMaxPayload)
			}
			if !fin {
				continue
			}
			return string(payload), nil
		}
	}
}

// close 发 close 帧并关闭底层连接。
func (c *wsConn) close() {
	_ = c.writeControl(wsOpClose, nil)
	_ = c.conn.Close()
}

// writeFrame 写一帧（mask=true 时按客户端要求加掩码）。
func writeFrame(w io.Writer, op byte, payload []byte, mask bool) error {
	var hdr []byte
	hdr = append(hdr, 0x80|op&0x0f) // FIN=1
	n := len(payload)
	maskBit := byte(0)
	if mask {
		maskBit = 0x80
	}
	switch {
	case n < 126:
		hdr = append(hdr, maskBit|byte(n))
	case n <= 0xffff:
		hdr = append(hdr, maskBit|126, byte(n>>8), byte(n))
	default:
		hdr = append(hdr, maskBit|127)
		var lenBuf [8]byte
		binary.BigEndian.PutUint64(lenBuf[:], uint64(n))
		hdr = append(hdr, lenBuf[:]...)
	}
	buf := bytes.NewBuffer(hdr)
	if mask {
		var key [4]byte
		if _, err := rand.Read(key[:]); err != nil {
			return err
		}
		buf.Write(key[:])
		masked := make([]byte, n)
		for i := 0; i < n; i++ {
			masked[i] = payload[i] ^ key[i%4]
		}
		buf.Write(masked)
	} else {
		buf.Write(payload)
	}
	_, err := w.Write(buf.Bytes())
	return err
}

// readFrame 读一帧；对端 mask 时自动去掩码（服务端帧不 mask，测试假服务端读客户端帧则需要）。
func readFrame(r *bufio.Reader) (op byte, fin bool, payload []byte, err error) {
	var hdr [2]byte
	if _, err = io.ReadFull(r, hdr[:]); err != nil {
		return 0, false, nil, err
	}
	fin = hdr[0]&0x80 != 0
	op = hdr[0] & 0x0f
	masked := hdr[1]&0x80 != 0
	n := int64(hdr[1] & 0x7f)
	switch n {
	case 126:
		var b [2]byte
		if _, err = io.ReadFull(r, b[:]); err != nil {
			return 0, false, nil, err
		}
		n = int64(binary.BigEndian.Uint16(b[:]))
	case 127:
		var b [8]byte
		if _, err = io.ReadFull(r, b[:]); err != nil {
			return 0, false, nil, err
		}
		n = int64(binary.BigEndian.Uint64(b[:]))
	}
	if n < 0 || n > wsMaxPayload {
		return 0, false, nil, fmt.Errorf("ws 帧长度非法: %d", n)
	}
	var key [4]byte
	if masked {
		if _, err = io.ReadFull(r, key[:]); err != nil {
			return 0, false, nil, err
		}
	}
	buf := make([]byte, n)
	if _, err = io.ReadFull(r, buf); err != nil {
		return 0, false, nil, err
	}
	if masked {
		for i := range buf {
			buf[i] ^= key[i%4]
		}
	}
	return op, fin, buf, nil
}

// ============================== 小工具 ==============================

// clip 截断字符串（错误信息里带上游原文时用）。
func clip(s string, n int) string {
	s = strings.TrimSpace(s)
	if len(s) <= n {
		return s
	}
	return s[:n]
}

// firstNonEmpty 返回第一个非空串。
func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if v != "" {
			return v
		}
	}
	return ""
}

// sleepCtx 可被 ctx 打断的 sleep。
func sleepCtx(ctx context.Context, d time.Duration) error {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}
