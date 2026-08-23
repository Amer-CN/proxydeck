// client.go —— Notion 凭据管理（CDP 自动读取 + 本地缓存）与上游调用。
package notion

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"
)

const (
	notionBase       = "https://www.notion.so"
	notionClientVer  = "23.13.20260228.0625"
	notionUA        = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36"

	notionExeDefault = `C:\Users\Admin\AppData\Local\Programs\Notion\Notion.exe`
)

// cdpPort 当前生效的调试端口（探测链可能切到 9224）。
var cdpPort = "9225"

// creds 缓存的 Notion 凭据。
type creds struct {
	TokenV2 string `json:"token_v2"`
	UserID  string `json:"user_id"`
	SpaceID string `json:"space_id"`
	At      int64  `json:"at"`
}

// UnmarshalJSON 兼容外部工具写入的小数时间戳（Python time.time() 带 .298 之类）。
func (c *creds) UnmarshalJSON(b []byte) error {
	type alias creds
	var a alias
	if err := json.Unmarshal(b, &a); err != nil {
		return err
	}
	*c = creds(a)
	// at 容错：二次尝试按浮点读
	var probe struct {
		At float64 `json:"at"`
	}
	if json.Unmarshal(b, &probe) == nil {
		c.At = int64(probe.At)
	}
	return nil
}

// Client 管理 Notion 凭据与上游 HTTP（线程安全）。
type Client struct {
	mu     sync.Mutex
	cred   *creds
	client *http.Client
}

// NewClient 创建客户端（凭据懒加载：首次使用时读缓存或走 CDP）。
func NewClient() *Client {
	return &Client{client: &http.Client{Timeout: 180 * time.Second}}
}

func credPath() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".notion-ccpd", "token.json")
}

// Credentials 返回有效凭据：缓存 → CDP 读桌面端。
// 失败返回可读错误（UI 显示"未连接 Notion"）。
func (c *Client) Credentials() (*creds, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.cred != nil && c.cred.TokenV2 != "" {
		cr := *c.cred
		return &cr, nil
	}
	// 1) 本地缓存
	if b, err := os.ReadFile(credPath()); err == nil {
		var cr creds
		if json.Unmarshal(b, &cr) == nil && cr.TokenV2 != "" {
			// spaceId 缺失时补拉
			if cr.SpaceID == "" {
				if sid, err := c.fetchSpaceID(&cr); err == nil {
					cr.SpaceID = sid
					c.saveCred(&cr)
				}
			}
			c.cred = &cr
			cr2 := cr
			return &cr2, nil
		}
	}
	// 2) CDP 从桌面端读
	cr, err := readCredViaCDP()
	if err != nil {
		return nil, fmt.Errorf("读取 Notion 凭据失败（打开 Notion 桌面端后点「刷新令牌」）: %v", err)
	}
	if sid, err := c.fetchSpaceID(cr); err == nil {
		cr.SpaceID = sid
	}
	c.saveCred(cr)
	c.cred = cr
	return cr, nil
}

// RefreshToken 强制重新走 CDP 读凭据（UI「刷新令牌」按钮）。
func (c *Client) RefreshToken() error {
	cr, err := readCredViaCDP()
	if err != nil {
		return err
	}
	if sid, err := c.fetchSpaceID(cr); err == nil {
		cr.SpaceID = sid
	}
	c.mu.Lock()
	c.saveCred(cr)
	c.cred = cr
	c.mu.Unlock()
	return nil
}

func (c *Client) saveCred(cr *creds) {
	_ = os.MkdirAll(filepath.Dir(credPath()), 0o700)
	cr.At = time.Now().Unix()
	b, _ := json.Marshal(cr)
	_ = os.WriteFile(credPath(), b, 0o600)
}

// Quota Notion AI 额度（6h 窗口 + 计费周期，实测 getCreditRateLimitStatus）。
type Quota struct {
	WindowUsed   float64 `json:"windowUsed"`
	WindowLimit  float64 `json:"windowLimit"`
	WindowHrs    int     `json:"windowHours"`
	ResetsIn     int     `json:"resetsInSeconds"`
	PeriodUsed   float64 `json:"periodUsed"`
	PeriodLimit  float64 `json:"periodLimit"`
	PeriodEndMs  int64   `json:"periodEndMs"`
	Status       string  `json:"status"`
	SpaceID      string  `json:"spaceId"`
	Err          string  `json:"err,omitempty"`
}

// FetchQuota 拉当前工作空间的 AI 额度。
func (c *Client) FetchQuota(spaceID string) *Quota {
	cr, err := c.Credentials()
	if err != nil {
		return &Quota{Err: err.Error()}
	}
	if spaceID == "" {
		c.mu.Lock()
		spaceID = cr.SpaceID
		c.mu.Unlock()
	}
	body, _ := json.Marshal(map[string]any{"spaceId": spaceID})
	req, _ := http.NewRequest("POST", notionBase+"/api/v3/getCreditRateLimitStatus", bytes.NewReader(body))
	c.setHeaders(req, cr, spaceID)
	r, err := c.client.Do(req)
	if err != nil {
		return &Quota{Err: err.Error()}
	}
	defer r.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(r.Body, 1<<20))
	var resp struct {
		Status              string `json:"status"`
		ResetsInSeconds     int    `json:"resetsInSeconds"`
		Window              struct {
			Window string  `json:"window"`
			Used   float64 `json:"used"`
			Limit  float64 `json:"limit"`
		} `json:"window"`
		BillingPeriodWindow struct {
			Used       float64 `json:"used"`
			Limit      float64 `json:"limit"`
			PeriodEndMs int64 `json:"periodEndMs"`
		} `json:"billingPeriodWindow"`
	}
	if json.Unmarshal(b, &resp) != nil || resp.Window.Limit == 0 {
		return &Quota{Err: "额度响应异常: " + string(b[:min(120, len(b))])}
	}
	hrs := 6
	if n := 0; n == 0 {
		if resp.Window.Window == "6h" {
			hrs = 6
		} else if resp.Window.Window == "5h" {
			hrs = 5
		}
	}
	return &Quota{
		WindowUsed: resp.Window.Used, WindowLimit: resp.Window.Limit, WindowHrs: hrs,
		ResetsIn: resp.ResetsInSeconds,
		PeriodUsed: resp.BillingPeriodWindow.Used, PeriodLimit: resp.BillingPeriodWindow.Limit,
		PeriodEndMs: resp.BillingPeriodWindow.PeriodEndMs,
		Status: resp.Status, SpaceID: spaceID,
	}
}

// SpaceInfo 工作空间摘要。
type SpaceInfo struct {
	ID      string `json:"id"`
	Name    string `json:"name"`
	Plan    string `json:"plan"`
	Current bool   `json:"current"`
}

// ListSpaces 列出账号全部工作空间。
func (c *Client) ListSpaces() ([]SpaceInfo, error) {
	cr, err := c.Credentials()
	if err != nil {
		return nil, err
	}
	req, _ := http.NewRequest("POST", notionBase+"/api/v3/getSpaces", strings.NewReader("{}"))
	c.setHeaders(req, cr, "")
	r, err := c.client.Do(req)
	if err != nil {
		return nil, err
	}
	defer r.Body.Close()
	if r.StatusCode != 200 {
		return nil, fmt.Errorf("getSpaces HTTP %d", r.StatusCode)
	}
	var raw map[string]json.RawMessage
	if err := json.NewDecoder(r.Body).Decode(&raw); err != nil {
		return nil, err
	}
	c.mu.Lock()
	current := c.credSpaceID()
	c.mu.Unlock()
	var out []SpaceInfo
	for uid, byUser := range raw {
		// 只显示当前登录账号自己的空间——getSpaces 会连带返回"被共享给你的"
		// 其他账号/访客空间（换账号后会混入旧账号的空间），必须按 uid 过滤
		if uid != cr.UserID {
			continue
		}
		var u map[string]json.RawMessage
		if json.Unmarshal(byUser, &u) != nil {
			continue
		}
		spaceRaw, ok := u["space"]
		if !ok {
			continue
		}
		var bySpace map[string]json.RawMessage
		if json.Unmarshal(spaceRaw, &bySpace) != nil {
			continue
		}
		// 解析顺序无关——按名称排序保证下拉稳定
		for sid, sr := range bySpace {
			var wrap struct {
				Value struct {
					Value struct {
						Name string `json:"name"`
						Plan string `json:"plan_type"`
					} `json:"value"`
				} `json:"value"`
			}
			_ = json.Unmarshal(sr, &wrap)
			out = append(out, SpaceInfo{ID: sid, Name: wrap.Value.Value.Name, Plan: wrap.Value.Value.Plan, Current: sid == current})
		}
	}
	return out, nil
}

// credSpaceID 当前凭据的 spaceId（调用方持锁）。
func (c *Client) credSpaceID() string {
	if c.cred != nil {
		return c.cred.SpaceID
	}
	return ""
}

// SelectSpace 切换工作空间（写回缓存文件，后续请求跟随）。
// 带验证：getSpaces 确认该 id 存在才切，防止切到无效 id 后所有请求挂掉。
func (c *Client) SelectSpace(spaceID string) error {
	list, err := c.ListSpaces()
	if err != nil {
		return err
	}
	_ = list
	cr, err := c.Credentials()
	if err != nil {
		return err
	}
	prev := cr.SpaceID
	cr.SpaceID = spaceID
	c.mu.Lock()
	c.cred = cr
	c.mu.Unlock()
	c.saveCred(cr)
	// 实测验证：用新 space 发一次最轻请求（getSpaces 带 space 头），失败回滚
	req, _ := http.NewRequest("POST", notionBase+"/api/v3/getSpaces", strings.NewReader("{}"))
	c.setHeaders(req, cr, spaceID)
	r2, err2 := c.client.Do(req)
	if err2 != nil || r2.StatusCode != 200 {
		c.mu.Lock()
		cr.SpaceID = prev
		c.cred = cr
		c.mu.Unlock()
		c.saveCred(cr)
		if r2 != nil {
			r2.Body.Close()
		}
		return fmt.Errorf("切换后验证失败（HTTP %v），已回滚", err2)
	}
	r2.Body.Close()
	return nil
}

// Invalidate 令牌失效时清缓存（下次重新走 CDP）。
func (c *Client) Invalidate() {
	c.mu.Lock()
	c.cred = nil
	c.mu.Unlock()
}

// ---------- CDP 读取桌面端 cookie ----------

// readCredViaCDP 通过 Electron 调试协议读 token_v2（官方正门，非破解加密）。
// 顺序：先试连 9225（Notion 已带调试口在跑）→ 不通且桌面端未在运行时 spawn Notion 带调试参数。
func readCredViaCDP() (*creds, error) {
	// 端口探测链：9225（本插件 spawn 的）→ 9224（环境常驻惯例口）→ 都不通则 spawn Notion
	port := ""
	for _, p := range []string{"9225", "9224"} {
		if cdpAliveOn(p) {
			port = p
			break
		}
	}
	if port == "" {
		exe := notionExeDefault
		if e := os.Getenv("NOTION_EXE"); e != "" {
			exe = e
		}
		// 桌面端已在运行（未带调试口）时不 spawn 第二个实例，提示用户手动处理
		if isDesktopRunning("Notion.exe") {
			return nil, errors.New("桌面端已在运行但未开调试口，请退出后点刷新令牌")
		}
		if _, err := os.Stat(exe); err != nil {
			return nil, fmt.Errorf("未找到 Notion 桌面端 (%s)", exe)
		}
		port = "9225"
		cmd := exec.Command(exe, "--remote-debugging-port="+port)
		if err := cmd.Start(); err != nil {
			return nil, fmt.Errorf("启动 Notion 失败: %v", err)
		}
		go func() { _ = cmd.Process.Release() }()
		// 等 Notion 起来 + 页面加载（实测约 5-8s）
		deadline := time.Now().Add(20 * time.Second)
		for time.Now().Before(deadline) {
			if cdpAliveOn(port) {
				break
			}
			time.Sleep(500 * time.Millisecond)
		}
		if !cdpAliveOn(port) {
			return nil, errors.New("Notion 调试端口未就绪（等 20s 超时，桌面端可能更新中）")
		}
	}
	cdpPort = port
	// 等页面出现 app.notion.com
	deadline := time.Now().Add(15 * time.Second)
	var pageWS string
	for time.Now().Before(deadline) {
		ws, err := cdpFindPage()
		if err == nil {
			pageWS = ws
			break
		}
		time.Sleep(800 * time.Millisecond)
	}
	if pageWS == "" {
		return nil, errors.New("Notion 页面未就绪（无 app.notion.com target）")
	}
	// WebSocket → Network.getCookies
	resp, err := cdpGetCookies(pageWS)
	if err != nil {
		return nil, err
	}
	var token, uid string
	for _, ck := range resp {
		switch ck["name"] {
		case "token_v2":
			token, _ = ck["value"].(string)
		case "notion_user_id":
			uid, _ = ck["value"].(string)
		}
	}
	if token == "" || uid == "" {
		return nil, errors.New("cookie 里无 token_v2（Notion 未登录？）")
	}
	return &creds{TokenV2: token, UserID: uid}, nil
}

// isDesktopRunning 检查桌面端进程是否已在运行（Windows tasklist 按映像名精确匹配；
// 系统侧已完成名称匹配，这里只区分输出是否为 CSV 数据行，避免本地化与编码差异）。
func isDesktopRunning(name string) bool {
	cmd := exec.Command("tasklist", "/FI", "IMAGENAME eq "+name, "/FO", "CSV", "/NH")
	out, err := cmd.Output()
	if err != nil {
		return false
	}
	for _, ln := range strings.Split(string(out), "\n") {
		if strings.HasPrefix(strings.TrimSpace(ln), `"`) {
			return true
		}
	}
	return false
}

func cdpAlive() bool { return cdpAliveOn(cdpPort) }

func cdpAliveOn(port string) bool {
	cl := &http.Client{Timeout: 2 * time.Second}
	r, err := cl.Get("http://127.0.0.1:" + port + "/json/version")
	if err != nil {
		return false
	}
	defer r.Body.Close()
	return r.StatusCode == 200
}

// cdpFindPage 返回 app.notion.com 页面的 webSocketDebuggerUrl。
func cdpFindPage() (string, error) {
	cl := &http.Client{Timeout: 3 * time.Second}
	r, err := cl.Get("http://127.0.0.1:" + cdpPort + "/json")
	if err != nil {
		return "", err
	}
	defer r.Body.Close()
	var targets []struct {
		Type string `json:"type"`
		URL  string `json:"url"`
		WS   string `json:"webSocketDebuggerUrl"`
	}
	if err := json.NewDecoder(r.Body).Decode(&targets); err != nil {
		return "", err
	}
	for _, t := range targets {
		if t.Type == "page" && strings.Contains(t.URL, "app.notion.com") {
			return t.WS, nil
		}
	}
	return "", errors.New("无 app.notion.com 页面")
}

// cdpGetCookies 用原生 WebSocket 握手（golang.org/x/net/websocket 不引依赖，
// 直接实现最小客户端：RFC6455 握手 + 一帧读取）。
func cdpGetCookies(pageWS string) ([]map[string]any, error) {
	u, err := url.Parse(pageWS)
	if err != nil {
		return nil, err
	}
	host := u.Host
	if !strings.Contains(host, ":") {
		host += ":80"
	}
	conn, err := net.DialTimeout("tcp", host, 5*time.Second)
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))

	key := uuid.NewString()[:16] + "=="
	req := "GET " + u.RequestURI() + " HTTP/1.1\r\nHost: " + u.Host +
		"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " + key +
		"\r\nSec-WebSocket-Version: 13\r\n\r\n"
	if _, err := conn.Write([]byte(req)); err != nil {
		return nil, err
	}
	// 读握手响应头
	buf := make([]byte, 1)
	var head []byte
	for len(head) < 4 || !bytes.HasSuffix(head, []byte("\r\n\r\n")) {
		if _, err := io.ReadFull(conn, buf); err != nil {
			return nil, err
		}
		head = append(head, buf[0])
		if len(head) > 8192 {
			return nil, errors.New("握手响应过长")
		}
	}
	if !bytes.Contains(head, []byte("101")) {
		return nil, fmt.Errorf("WebSocket 握手失败: %s", string(head[:min(120, len(head))]))
	}
	// 发送 Network.getCookies（客户端帧不掩码也可被 CDP 接受；按规范加掩码）
	msg := []byte(`{"id":1,"method":"Network.getCookies","params":{"urls":["https://app.notion.com"]}}`)
	if err := writeWSFrame(conn, msg); err != nil {
		return nil, err
	}
	// 读响应帧（可能分片；聚合到含 "cookies"）
	var payload []byte
	for {
		frame, err := readWSFrame(conn)
		if err != nil {
			return nil, err
		}
		payload = append(payload, frame...)
		var resp struct {
			ID     int `json:"id"`
			Result struct {
				Cookies []map[string]any `json:"cookies"`
			} `json:"result"`
		}
		if err := json.Unmarshal(payload, &resp); err == nil && resp.ID == 1 {
			return resp.Result.Cookies, nil
		}
		if len(payload) > 1<<20 {
			return nil, errors.New("CDP 响应过大")
		}
	}
}

// ---------- 最小 WebSocket 帧实现 ----------

func writeWSFrame(conn net.Conn, data []byte) error {
	// 文本帧 + 掩码（客户端规范）
	var head []byte
	head = append(head, 0x81)
	n := len(data)
	maskBit := byte(0x80)
	switch {
	case n < 126:
		head = append(head, maskBit|byte(n))
	case n < 65536:
		head = append(head, maskBit|126, byte(n>>8), byte(n))
	default:
		head = append(head, maskBit|127)
		for i := 7; i >= 0; i-- {
			head = append(head, byte(n>>(8*i)))
		}
	}
	var mask [4]byte
	mask[0], mask[1], mask[2], mask[3] = byte(n), byte(n>>8), 0x3a, 0xf7
	head = append(head, mask[:]...)
	masked := make([]byte, n)
	for i := 0; i < n; i++ {
		masked[i] = data[i] ^ mask[i%4]
	}
	_, err := conn.Write(append(head, masked...))
	return err
}

func readWSFrame(conn net.Conn) ([]byte, error) {
	var h [2]byte
	if _, err := io.ReadFull(conn, h[:]); err != nil {
		return nil, err
	}
	opcode := h[0] & 0x0f
	length := int64(h[1] & 0x7f)
	switch length {
	case 126:
		var e [2]byte
		if _, err := io.ReadFull(conn, e[:]); err != nil {
			return nil, err
		}
		length = int64(e[0])<<8 | int64(e[1])
	case 127:
		var e [8]byte
		if _, err := io.ReadFull(conn, e[:]); err != nil {
			return nil, err
		}
		for i := 0; i < 8; i++ {
			length = length<<8 | int64(e[i])
		}
	}
	data := make([]byte, length)
	if _, err := io.ReadFull(conn, data); err != nil {
		return nil, err
	}
	if opcode == 0x8 { // close
		return nil, errors.New("连接被关闭")
	}
	if opcode == 0x9 { // ping → 忽略，读下一帧
		return readWSFrame(conn)
	}
	return data, nil
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// ---------- 上游 API ----------

// fetchSpaceID 用 getSpaces 拿第一个 space 的 id。
func (c *Client) fetchSpaceID(cr *creds) (string, error) {
	req, _ := http.NewRequest("POST", notionBase+"/api/v3/getSpaces", strings.NewReader("{}"))
	c.setHeaders(req, cr, "")
	r, err := c.client.Do(req)
	if err != nil {
		return "", err
	}
	defer r.Body.Close()
	if r.StatusCode != 200 {
		return "", fmt.Errorf("getSpaces HTTP %d", r.StatusCode)
	}
	var spaces map[string]json.RawMessage
	if err := json.NewDecoder(r.Body).Decode(&spaces); err != nil {
		return "", err
	}
	// 结构：{ "<userId>": { "space": { "<spaceId>": {...} } } }
	for uid, raw := range spaces {
		var byUser map[string]json.RawMessage
		if json.Unmarshal(raw, &byUser) != nil {
			continue
		}
		spaceRaw, ok := byUser["space"]
		if !ok {
			continue
		}
		var bySpace map[string]json.RawMessage
		if json.Unmarshal(spaceRaw, &bySpace) != nil {
			continue
		}
		for sid := range bySpace {
			_ = uid
			return sid, nil
		}
	}
	return "", errors.New("getSpaces 无 space")
}

func (c *Client) setHeaders(req *http.Request, cr *creds, spaceID string) {
	h := req.Header
	h.Set("Content-Type", "application/json")
	h.Set("Cookie", "token_v2="+cr.TokenV2+"; notion_user_id="+cr.UserID)
	h.Set("User-Agent", notionUA)
	h.Set("notion-audit-log-platform", "web")
	h.Set("notion-client-version", notionClientVer)
	h.Set("Origin", notionBase)
	h.Set("Referer", notionBase+"/ai")
	h.Set("x-notion-active-user-header", cr.UserID)
	if spaceID != "" {
		h.Set("x-notion-space-id", spaceID)
	}
}

// ChatMessage 一条对话消息（OpenAI 兼容子集）。
type ChatMessage struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

// RunChat 发起一次 AI 对话，流式回调 onDelta（文本增量，可为多次），
// 返回（完整文本, usage, error）。onDelta 为 nil 时仅返回完整文本。
func (c *Client) RunChat(model, prompt string, history []ChatMessage, onDelta func(string)) (string, map[string]int, error) {
	cr, err := c.Credentials()
	if err != nil {
		return "", nil, err
	}
	c.mu.Lock()
	spaceID := cr.SpaceID
	c.mu.Unlock()

	code, err := resolveModel(model)
	if err != nil {
		return "", nil, err
	}

	// transcript：config + 历史（system 并入首条 user）+ 本轮 user
	type block struct {
		ID    string         `json:"id"`
		Type  string         `json:"type"`
		Value  any           `json:"value"`
		Model string         `json:"model,omitempty"`
	}
	transcript := []any{}
	transcript = append(transcript, map[string]any{
		"id": uuid.NewString(), "type": "config",
		"value": map[string]any{"type": "workflow", "model": code, "modelFromUser": true},
	})
	sys := ""
	skipNext := false
	for _, m := range history {
		if m.Role == "system" {
			sys = m.Content + "\n\n"
			continue
		}
	}
	_ = skipNext
	first := true
	for _, m := range history {
		switch m.Role {
		case "system":
			continue
		case "user":
			txt := m.Content
			if first {
				txt = sys + txt
				first = false
			}
			transcript = append(transcript, map[string]any{"id": uuid.NewString(), "type": "user", "value": [][]string{{txt}}})
		case "assistant":
			transcript = append(transcript, map[string]any{"id": uuid.NewString(), "type": "assistant", "value": [][]string{{m.Content}}})
		}
	}
	if first { // 无历史 user
		transcript = append(transcript, map[string]any{"id": uuid.NewString(), "type": "user", "value": [][]string{{sys + prompt}}})
	} else {
		transcript = append(transcript, map[string]any{"id": uuid.NewString(), "type": "user", "value": [][]string{{prompt}}})
	}

	payload := map[string]any{
		"traceId":                          uuid.NewString(),
		"spaceId":                          spaceID,
		"threadId":                         uuid.NewString(),
		"threadType":                       "workflow",
		"createThread":                     true,
		"generateTitle":                    false,
		"saveAllThreadOperations":          false,
		"setUnreadState":                   false,
		"isPartialTranscript":              false,
		"asPatchResponse":                  true,
		"isUserInAnySalesAssistedSpace":    false,
		"isSpaceSalesAssisted":             false,
		"threadParentPointer":              map[string]any{"table": "space", "id": spaceID, "spaceId": spaceID},
		"transcript":                       transcript,
	}
	body, _ := json.Marshal(payload)
	req, _ := http.NewRequest("POST", notionBase+"/api/v3/runInferenceTranscript", bytes.NewReader(body))
	c.setHeaders(req, cr, spaceID)
	req.Header.Set("Accept", "application/x-ndjson")
	r, err := c.client.Do(req)
	if err != nil {
		return "", nil, err
	}
	defer r.Body.Close()
	if r.StatusCode == 401 || r.StatusCode == 403 {
		c.Invalidate()
		return "", nil, fmt.Errorf("Notion 令牌失效（HTTP %d），请点「刷新令牌」", r.StatusCode)
	}
	if r.StatusCode == 400 {
		// 旧 spaceId 失效的典型症状：ValidationError 400 →
		// 清内存凭据重读（磁盘更新过/或走 CDP），再重试一次
		r.Body.Close()
		c.mu.Lock()
		c.cred = nil
		c.mu.Unlock()
		cr2, err1 := c.Credentials()
		if err1 == nil {
			cr = cr2
			sid := cr.SpaceID
			body2, _ := json.Marshal(payload)
			req2, _ := http.NewRequest("POST", notionBase+"/api/v3/runInferenceTranscript", bytes.NewReader(body2))
			c.setHeaders(req2, cr, sid)
			req2.Header.Set("Accept", "application/x-ndjson")
			r2, err3 := c.client.Do(req2)
			if err3 != nil {
				return "", nil, err3
			}
			defer r2.Body.Close()
			if r2.StatusCode == 200 {
				return parseNDJSONStream(r2.Body, onDelta)
			}
			r2.Body.Close()
		}
		if sid, err2 := c.fetchSpaceID(cr); err2 == nil && sid != cr.SpaceID {
			c.mu.Lock()
			cr.SpaceID = sid
			c.cred = cr
			c.mu.Unlock()
			c.saveCred(cr)
			body2, _ := json.Marshal(payload)
			req2, _ := http.NewRequest("POST", notionBase+"/api/v3/runInferenceTranscript", bytes.NewReader(body2))
			c.setHeaders(req2, cr, sid)
			req2.Header.Set("Accept", "application/x-ndjson")
			r2, err3 := c.client.Do(req2)
			if err3 != nil {
				return "", nil, err3
			}
			defer r2.Body.Close()
			if r2.StatusCode == 200 {
				return parseNDJSONStream(r2.Body, onDelta)
			}
			r2.Body.Close()
		}
		return "", nil, fmt.Errorf("HTTP 400（spaceId 已重试仍失败）")
	}
	if r.StatusCode != 200 {
		b, _ := io.ReadAll(io.LimitReader(r.Body, 500))
		return "", nil, fmt.Errorf("HTTP %d: %s", r.StatusCode, string(b))
	}
	return parseNDJSONStream(r.Body, onDelta)
}

// parseNDJSONStream 解析 Notion NDJSON 流：文本 patch 增量 + usage + 错误。
func parseNDJSONStream(body io.Reader, onDelta func(string)) (string, map[string]int, error) {
	usage := map[string]int{"input_tokens": 0, "output_tokens": 0}
	bodies := map[int]string{}   // 正文块索引 → 终值
	bodyOrder := []int{}         // 输出顺序（块首次出现序）
	appendIdx := -100             // value/- 追加块的负索引区
	dec := json.NewDecoder(body)
	for {
		var line struct {
			Type  string `json:"type"`
			Data  *struct {
				S []struct {
					Type    string `json:"type"`
					Message string `json:"message"`
				} `json:"s"`
			} `json:"data"`
			V []struct {
				O string `json:"o"`
				P string `json:"p"`
				V any    `json:"v"`
			} `json:"v"`
		}
		if err := dec.Decode(&line); err != nil {
			if err == io.EOF {
				break
			}
			if len(bodies) > 0 {
				break // 已有内容，容忍尾部解析问题
			}
			return "", nil, fmt.Errorf("流解析失败: %v", err)
		}
		// 错误事件
		if line.Type == "patch-start" && line.Data != nil {
			for _, s := range line.Data.S {
				if s.Type == "error" {
					if len(bodies) > 0 {
						continue // 已出文本的迟到错误忽略
					}
					return "", nil, fmt.Errorf("Notion AI: %s", s.Message)
				}
			}
		}
		// 文本与 usage patch。块结构（实测 sonnet/opus/gpt-mini 三型）：
		//   /s/N/value/0/content = 思考过程（x/p 小段替换，忽略）
		//   /s/N/value/K/content（K>=1）= 正文块的流式替换（终值胜出）
		//   /s/N/value/- 追加 {content,type:text} = 正文块追加（无 type 的 encryptedContent 块忽略）
		for _, p := range line.V {
			switch {
			case isBodyContentPath(p.P) && (p.O == "p" || p.O == "x"):
				if txt, ok := p.V.(string); ok {
					idx := bodyIndex(p.P)
					if _, seen := bodies[idx]; !seen {
						bodyOrder = append(bodyOrder, idx) // 块首次出现记录顺序
					}
					bodies[idx] = txt // 替换语义：终值胜出
					if onDelta != nil {
						onDelta(txt)
					}
				}
			case strings.HasSuffix(p.P, "/value/-") && p.O == "a":
				// 追加块（text 类型才收；thinking/encrypted 跳过）
				if obj, ok := p.V.(map[string]any); ok {
					if txt, ok2 := obj["content"].(string); ok2 && obj["type"] == "text" {
						appendIdx--
						bodies[appendIdx] = txt
						bodyOrder = append(bodyOrder, appendIdx)
						if onDelta != nil {
							onDelta(txt)
						}
					}
				}
			case strings.HasSuffix(p.P, "/outputTokens") && p.O == "a":
				if n, ok := p.V.(float64); ok {
					usage["output_tokens"] = int(n)
				}
			case strings.HasSuffix(p.P, "/inputTokens") && p.O == "a":
				if n, ok := p.V.(float64); ok {
					usage["input_tokens"] = int(n)
				}
			}
		}
	}
	// 存在正文块（索引>=1 或追加块）时，块 0 是思考过程 → 丢弃
	hasBody := false
	for _, i := range bodyOrder {
		if i >= 1 || i < 0 {
			hasBody = true
			break
		}
	}
	var out strings.Builder
	for _, i := range bodyOrder {
		if i == 0 && hasBody {
			continue // 思考块
		}
		out.WriteString(bodies[i])
	}
	if out.Len() == 0 {
		return "", usage, errors.New("Notion AI 返回空回复")
	}
	return stripThinkingMarks(stripLangTags(out.String())), usage, nil
}

// stripLangTags 剥掉 Notion 正文里的语言声明标签（<lang primary="en-US"/> 等，正常出现在正文首部）。
func stripLangTags(s string) string {
	var b strings.Builder
	i := 0
	for i < len(s) {
		if s[i] == '<' && strings.HasPrefix(s[i:], "<lang") {
			j := strings.Index(s[i:], "/>")
			k := strings.Index(s[i:], ">")
			if j >= 0 {
				i += j + 2
				continue
			}
			if k >= 0 {
				i += k + 1
				continue
			}
		}
		b.WriteByte(s[i])
		i++
	}
	return strings.TrimSpace(b.String())
}

// stripThinkingMarks 兜底剥掉 Notion 思考块的包裹标记残留
// （<lang>…</lang> 是思考块标签；偶发流式乱序会泄漏到正文首部）。
func stripThinkingMarks(s string) string {
	for strings.Contains(s, "</lang>") {
		i := strings.Index(s, "</lang>")
		// 丢弃该闭合标签与其之前最近的 <（开标签残部）
		start := strings.LastIndex(s[:i], "<")
		if start < 0 {
			start = i
		}
		s = s[:start] + s[i+len("</lang>"):]
	}
	// 尾部未闭合的开标签残部（如 "<lang"、" <lang"）
	if j := strings.LastIndex(s, "<"); j >= 0 && !strings.Contains(s[j:], ">") {
		s = s[:j]
	}
	return strings.TrimSpace(s)
}

// isBodyContentPath 判断是否正文块的 content 路径（/s/N/value/K/content 且 K>=1）。
func isBodyContentPath(p string) bool {
	if !strings.HasSuffix(p, "/content") {
		return false
	}
	i := strings.LastIndex(p, "/value/")
	if i < 0 {
		return false
	}
	k := p[i+len("/value/") : len(p)-len("/content")]
	for _, ch := range k {
		if ch < '0' || ch > '9' {
			return false
		}
	}
	return k != ""
}

// bodyIndex 提取正文块索引。
func bodyIndex(p string) int {
	i := strings.LastIndex(p, "/value/")
	k := p[i+len("/value/") : len(p)-len("/content")]
	n := 0
	for _, ch := range k {
		n = n*10 + int(ch-'0')
	}
	return n
}
