// client.go —— 灵犀凭据（CDP 读 cookie）+ 会话 + WebSocket 对话。
package lingxi

import (
	"bytes"
	"crypto/rand"
	"crypto/tls"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"

	"github.com/google/uuid"
)

const (
	lingxiBase    = "https://lingxi.wps.cn"
	lingxiUA      = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) lingxi-desktop/1.2.31 Chrome/144.0.7559.236 Electron/37.2.0"
	cdpPort       = "5237"
	lingxiExeDefault = `D:\Program Files (x86)\lingxi-desktop\WPS 灵犀.exe`
)

// Client 灵犀客户端（cookie 缓存 + 会话管理）。
type Client struct {
	mu       sync.Mutex
	cookie   string
	cookieAt time.Time
	http     *http.Client
}

// NewClient 创建客户端。
func NewClient() *Client {
	return &Client{http: &http.Client{Timeout: 60 * time.Second}}
}

func cookiePath() string {
	home, _ := os.UserHomeDir()
	return home + string(os.PathSeparator) + ".lingxi-ccpd" + string(os.PathSeparator) + "cookie.txt"
}

// Cookie 返回有效 cookie 串（缓存 30 分钟 → 磁盘 → CDP 读灵犀桌面端）。
func (c *Client) Cookie() (string, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.cookie != "" && time.Since(c.cookieAt) < 30*time.Minute {
		return c.cookie, nil
	}
	// 磁盘缓存
	if b, err := os.ReadFile(cookiePath()); err == nil && len(bytes.TrimSpace(b)) > 50 {
		c.cookie = strings.TrimSpace(string(b))
		c.cookieAt = time.Now()
		return c.cookie, nil
	}
	return c.refreshLocked()
}

// RefreshToken 强制重读（UI「刷新令牌」）。
func (c *Client) RefreshToken() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	_, err := c.refreshLocked()
	return err
}

func (c *Client) refreshLocked() (string, error) {
	ck, err := readCookieViaCDP()
	if err != nil {
		return "", fmt.Errorf("读取灵犀凭据失败（打开灵犀桌面端后点「刷新令牌」）: %v", err)
	}
	c.cookie = ck
	c.cookieAt = time.Now()
	_ = os.MkdirAll(strings.TrimSuffix(cookiePath(), "cookie.txt"), 0o700)
	_ = os.WriteFile(cookiePath(), []byte(ck), 0o600)
	return ck, nil
}

// ---------- HTTP（plans / createSession / quota）----------

func (c *Client) do(method, path string, body []byte) ([]byte, int, error) {
	ck, err := c.Cookie()
	if err != nil {
		return nil, 0, err
	}
	req, _ := http.NewRequest(method, lingxiBase+path, bytes.NewReader(body))
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Cookie", ck)
	req.Header.Set("User-Agent", lingxiUA)
	req.Header.Set("Origin", "https://lingxi.wps.cn")
	req.Header.Set("Referer", "https://lingxi.wps.cn/cowork/")
	r, err := c.http.Do(req)
	if err != nil {
		return nil, 0, err
	}
	defer r.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(r.Body, 4<<20))
	return b, r.StatusCode, nil
}

// FetchModels 拉模型清单（sessions/plans）。
func (c *Client) FetchModels() ([]ModelEntry, error) {
	b, code, err := c.do("GET", "/api/aioffice/v1/sessions/plans", nil)
	if err != nil || code != 200 {
		return nil, fmt.Errorf("plans HTTP %d: %v", code, err)
	}
	var resp struct {
		Data struct {
			Models []ModelEntry `json:"models"`
			Plans  []struct {
				Key  string `json:"key"`
				Name string `json:"name"`
			} `json:"plans"`
		} `json:"data"`
	}
	if json.Unmarshal(b, &resp) != nil || len(resp.Data.Models) == 0 {
		return nil, fmt.Errorf("plans 响应异常: %s", truncateStr(string(b), 120))
	}
	return resp.Data.Models, nil
}

// CreditPackage 赠送灵点包。
type CreditPackage struct {
	Balance    int    `json:"balance"`
	Value      int    `json:"value"`
	Consumed   int    `json:"consumed"`
	ExpireTime string `json:"expire_time"`
}

// FetchCredits 灵点余额（credits/balance，实测结构）。
func (c *Client) FetchCredits() (total int, packs []CreditPackage, err error) {
	b, code, err := c.do("GET", "/api/public/v1/credits/balance", nil)
	if err != nil || code != 200 {
		return 0, nil, fmt.Errorf("balance HTTP %d: %v", code, err)
	}
	var resp struct {
		Data struct {
			Enabled bool             `json:"enabled"`
			Bonus   []CreditPackage  `json:"bonus_credits"`
		} `json:"data"`
	}
	if json.Unmarshal(b, &resp) != nil {
		return 0, nil, fmt.Errorf("balance 解析失败: %s", truncateStr(string(b), 100))
	}
	for _, p := range resp.Data.Bonus {
		total += p.Balance
	}
	return total, resp.Data.Bonus, nil
}

// FetchPlans 拉套餐模式名（UI 显示 Pro/Max）。
func (c *Client) FetchPlans() []string {
	b, _, err := c.do("GET", "/api/aioffice/v1/sessions/plans", nil)
	if err != nil {
		return nil
	}
	var resp struct {
		Data struct {
			Plans []struct {
				Name string `json:"name"`
			} `json:"plans"`
		} `json:"data"`
	}
	if json.Unmarshal(b, &resp) != nil {
		return nil
	}
	out := make([]string, 0, len(resp.Data.Plans))
	for _, p := range resp.Data.Plans {
		out = append(out, p.Name)
	}
	return out
}

// CreateSession 建会话（实测最小 body：仅 session_type）。
func (c *Client) CreateSession() (string, error) {
	b, code, err := c.do("POST", "/api/aioffice/v1/cowork/sessions",
		[]byte(`{"session_type":"cowork_client"}`))
	if err != nil {
		return "", err
	}
	if code != 200 {
		return "", fmt.Errorf("createSession HTTP %d: %s", code, truncateStr(string(b), 150))
	}
	var resp struct {
		Data struct {
			SessionID json.Number `json:"session_id"`
		} `json:"data"`
		Result string `json:"result"`
	}
	dec := json.NewDecoder(bytes.NewReader(b))
	dec.UseNumber()
	if dec.Decode(&resp) != nil || resp.Data.SessionID == "" {
		return "", fmt.Errorf("createSession 响应异常: %s", truncateStr(string(b), 150))
	}
	return resp.Data.SessionID.String(), nil
}

// ---------- WebSocket 对话 ----------

// WSMessage WS 事件（宽松解析：只取关心的字段）。
type WSMessage struct {
	Event string `json:"event"`
	Data  struct {
		Type        string `json:"type"`
		Delta       string `json:"delta"`
		FullContent string `json:"full_content"`
		Message     string `json:"message"`
	} `json:"data"`
}

// Chat 发起一轮对话：model 为空用默认；onDelta 流式增量；返回完整文本。
// 多轮：灵犀会话有状态——每轮独立建会话，历史拼进 prompt（简单可靠）。
func (c *Client) Chat(model, prompt string, history []ChatMessage, onDelta func(string)) (string, error) {
	sid, err := c.CreateSession()
	if err != nil {
		return "", err
	}
	ck, err := c.Cookie()
	if err != nil {
		return "", err
	}
	// 历史拼进本轮 prompt（灵犀 agent 上下文由服务端会话管理，独立会话需自带历史）
	full := buildPrompt(history, prompt)
	input := map[string]any{"text": full, "refs": []any{}}
	if model != "" {
		input["expect_model"] = model
	}
	sendMsg := map[string]any{
		"event": "user.input",
		"data": map[string]any{
			"agent_input": input,
			"agent":       "main",
			"headers":     map[string]any{"client_type": "lingxi_claw", "open_position": "clawapp"},
		},
	}
	return c.wsChat(sid, ck, sendMsg, onDelta)
}

// ChatMessage OpenAI 兼容消息。
type ChatMessage struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

func buildPrompt(history []ChatMessage, prompt string) string {
	if len(history) == 0 {
		return prompt
	}
	var b strings.Builder
	for _, m := range history {
		switch m.Role {
		case "system":
			b.WriteString("[system]\n" + m.Content + "\n\n")
		case "user":
			b.WriteString("[user]\n" + m.Content + "\n\n")
		case "assistant":
			b.WriteString("[assistant]\n" + m.Content + "\n\n")
		}
	}
	b.WriteString("[user]\n" + prompt)
	return b.String()
}

// wsChat 连 WS → 发消息 → 聚合 component.content/end 的文本。
func (c *Client) wsChat(sid, cookie string, sendMsg map[string]any, onDelta func(string)) (string, error) {
	wsURL := "wss://lingxi.wps.cn/api/aioffice/v1/cowork/sessions/" + sid + "/completions/ws"
	conn, err := dialWS(wsURL, cookie)
	if err != nil {
		return "", fmt.Errorf("WS 连接失败: %v", err)
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(180 * time.Second))

	// 发消息
	body, _ := json.Marshal(sendMsg)
	if err := writeWSFrame(conn, body); err != nil {
		return "", err
	}
	// 收消息：聚合文本
	var acc strings.Builder
	var lastFull strings.Builder
	buf := make([]byte, 0, 65536)
	tmp := make([]byte, 32768)
	for {
		n, err := conn.Read(tmp)
		if err != nil {
			break // 连接关闭（正常结束）或超时
		}
		buf = append(buf, tmp[:n]...)
		// 拆帧（可能粘包）
		for len(buf) > 2 {
			plen := int(buf[1] & 0x7f)
			off := 2
			if plen == 126 {
				if len(buf) < 4 {
					break
				}
				plen = int(buf[2])<<8 | int(buf[3])
				off = 4
			} else if plen == 127 {
				if len(buf) < 10 {
					break
				}
				plen = 0
				for i := 0; i < 8; i++ {
					plen = plen<<8 | int(buf[2+i])
				}
				off = 10
			}
			if len(buf) < off+plen {
				break // 半包
			}
			payload := buf[off : off+plen]
			buf = buf[off+plen:]
			var msg WSMessage
			if json.Unmarshal(payload, &msg) != nil {
				continue
			}
			switch msg.Event {
			case "component.content":
				if msg.Data.Delta != "" {
					acc.WriteString(msg.Data.Delta)
					if onDelta != nil {
						onDelta(msg.Data.Delta)
					}
				}
			case "component.end":
				if msg.Data.FullContent != "" && (msg.Data.Type == "c_text" || msg.Data.Type == "") {
					lastFull.Reset()
					lastFull.WriteString(msg.Data.FullContent)
				}
			case "error":
				if acc.Len() == 0 {
					return "", fmt.Errorf("灵犀错误: %s", msg.Data.Message)
				}
			}
		}
		// 结束条件：full_content 已到且后续静默由 deadline 兜底；简化：收到 component.end(c_text) 即可等更多，
		// 由外层 deadline 收割。这里在 lastFull 非空且无新 delta 一段时间后返回——
		// 简化实现：lastFull 非空直接再读 800ms 无新帧就结束
		if lastFull.Len() > 0 {
			_ = conn.SetReadDeadline(time.Now().Add(1500 * time.Millisecond))
		}
	}
	if lastFull.Len() > 0 {
		return lastFull.String(), nil
	}
	if acc.Len() > 0 {
		return acc.String(), nil
	}
	return "", errors.New("灵犀无回复（会话可能超限）")
}

// ---------- 原生 WS 客户端（wss，复用 notion 包的最小实现思路）----------

func dialWS(url, cookie string) (net.Conn, error) {
	host := strings.TrimPrefix(strings.TrimPrefix(url, "wss://"), "https://")
	path := "/"
	if i := strings.Index(host, "/"); i >= 0 {
		path = host[i:]
		host = host[:i]
	}
	raw, err := net.DialTimeout("tcp", host+":443", 10*time.Second)
	if err != nil {
		return nil, err
	}
	tlsConn := tls.Client(raw, &tls.Config{ServerName: host})
	if err := tlsConn.Handshake(); err != nil {
		raw.Close()
		return nil, fmt.Errorf("TLS handshake fail: %v", err)
	}
	key := make([]byte, 16)
	_, _ = rand.Read(key)
	req := "GET " + path + " HTTP/1.1\r\nHost: " + host +
		"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " + base64.StdEncoding.EncodeToString(key) +
		"\r\nSec-WebSocket-Version: 13\r\nOrigin: https://lingxi.wps.cn\r\nUser-Agent: " + lingxiUA +
		"\r\nCookie: " + cookie + "\r\n\r\n"
	if _, err := tlsConn.Write([]byte(req)); err != nil {
		tlsConn.Close()
		return nil, err
	}
	head := make([]byte, 1)
	var got []byte
	for len(got) < 4 || !bytes.HasSuffix(got, []byte("\r\n\r\n")) {
		if _, err := io.ReadFull(tlsConn, head); err != nil {
			tlsConn.Close()
			return nil, err
		}
		got = append(got, head[0])
		if len(got) > 8192 {
			tlsConn.Close()
			return nil, errors.New("handshake too long")
		}
	}
	if !bytes.Contains(got, []byte("101")) {
		tlsConn.Close()
		return nil, fmt.Errorf("WS rejected: %s", truncateStr(string(got), 120))
	}
	return tlsConn, nil
}

func writeWSFrame(conn net.Conn, data []byte) error {
	var head []byte
	head = append(head, 0x81)
	n := len(data)
	mask := [4]byte{0x5a, 0x3c, 0x7e, 0x11}
	mb := byte(0x80)
	switch {
	case n < 126:
		head = append(head, mb|byte(n))
	case n < 65536:
		head = append(head, mb|126, byte(n>>8), byte(n))
	default:
		head = append(head, mb|127)
		for i := 7; i >= 0; i-- {
			head = append(head, byte(n>>(8*i)))
		}
	}
	head = append(head, mask[:]...)
	masked := make([]byte, n)
	for i := 0; i < n; i++ {
		masked[i] = data[i] ^ mask[i%4]
	}
	_, err := conn.Write(append(head, masked...))
	return err
}

func truncateStr(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

// ---------- CDP 读 cookie（复用 notion 的最小 WS 实现）----------

func readCookieViaCDP() (string, error) {
	if !cdpAlive() {
		exe := lingxiExeDefault
		if e := os.Getenv("LINGXI_EXE"); e != "" {
			exe = e
		}
		// 桌面端已在运行（未带调试口）时不 spawn 第二个实例，提示用户手动处理
		if isDesktopRunning("WPS 灵犀.exe") {
			return "", errors.New("桌面端已在运行但未开调试口，请退出后点刷新令牌")
		}
		if _, err := os.Stat(exe); err != nil {
			return "", fmt.Errorf("未找到灵犀 (%s)", exe)
		}
		cmd := exec.Command(exe, "--remote-debugging-port="+cdpPort)
		if err := cmd.Start(); err != nil {
			return "", err
		}
		go func() { _ = cmd.Process.Release() }()
		deadline := time.Now().Add(20 * time.Second)
		for time.Now().Before(deadline) {
			if cdpAlive() {
				break
			}
			time.Sleep(500 * time.Millisecond)
		}
		if !cdpAlive() {
			return "", errors.New("灵犀调试端口未就绪（可能已在运行且未带调试参数——请退出灵犀后点刷新）")
		}
	}
	// 找 lingxi.wps.cn 页面
	wsURL, err := cdpFindPage()
	if err != nil {
		return "", err
	}
	// WS 到灵犀的 CDP（http:// 可直接明文）
	conn, err := dialCDP(wsURL)
	if err != nil {
		return "", err
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(15 * time.Second))
	msg, _ := json.Marshal(map[string]any{
		"id": 1, "method": "Network.getCookies",
		"params": map[string]any{"urls": []string{"https://lingxi.wps.cn", "https://www.wps.cn"}},
	})
	if err := writeWSFrame(conn, msg); err != nil {
		return "", err
	}
	// 读响应（聚合分片）
	var payload []byte
	readBuf := make([]byte, 0, 65536)
	tmp := make([]byte, 32768)
	for {
		n, err := conn.Read(tmp)
		if err != nil {
			return "", errors.New("CDP 响应读取失败")
		}
		readBuf = append(readBuf, tmp[:n]...)
		for len(readBuf) > 2 {
			plen := int(readBuf[1] & 0x7f)
			off := 2
			if plen == 126 {
				if len(readBuf) < 4 {
					break
				}
				plen = int(readBuf[2])<<8 | int(readBuf[3])
				off = 4
			} else if plen == 127 {
				if len(readBuf) < 10 {
					break
				}
				plen = 0
				for i := 0; i < 8; i++ {
					plen = plen<<8 | int(readBuf[2+i])
				}
				off = 10
			}
			if len(readBuf) < off+plen {
				break
			}
			payload = append(payload, readBuf[off:off+plen]...)
			readBuf = readBuf[off+plen:]
			var resp struct {
				ID     int `json:"id"`
				Result struct {
					Cookies []struct {
						Name  string `json:"name"`
						Value string `json:"value"`
					} `json:"cookies"`
				} `json:"result"`
			}
			if json.Unmarshal(payload, &resp) == nil && resp.ID == 1 {
				parts := make([]string, 0, len(resp.Result.Cookies))
				for _, ck := range resp.Result.Cookies {
					parts = append(parts, ck.Name+"="+ck.Value)
				}
				return strings.Join(parts, "; "), nil
			}
		}
	}
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

func cdpAlive() bool {
	cl := &http.Client{Timeout: 2 * time.Second}
	r, err := cl.Get("http://127.0.0.1:" + cdpPort + "/json/version")
	if err != nil {
		return false
	}
	r.Body.Close()
	return r.StatusCode == 200
}

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
		if t.Type == "page" && strings.Contains(t.URL, "lingxi.wps.cn") {
			return t.WS, nil
		}
	}
	return "", errors.New("无 lingxi.wps.cn 页面（灵犀可能未登录/未打开）")
}

// dialCDP CDP 的 ws 是 127.0.0.1 明文 http 升级（非 wss）。
func dialCDP(wsURL string) (net.Conn, error) {
	u := strings.TrimPrefix(wsURL, "ws://")
	host := u
	path := "/"
	if i := strings.Index(u, "/"); i >= 0 {
		path = u[i:]
		host = u[:i]
	}
	conn, err := net.DialTimeout("tcp", host, 5*time.Second)
	if err != nil {
		return nil, err
	}
	key := uuid.NewString()[:16]
	req := "GET " + path + " HTTP/1.1\r\nHost: " + host +
		"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " + key +
		"\r\nSec-WebSocket-Version: 13\r\n\r\n"
	if _, err := conn.Write([]byte(req)); err != nil {
		conn.Close()
		return nil, err
	}
	head := make([]byte, 1)
	var got []byte
	for len(got) < 4 || !bytes.HasSuffix(got, []byte("\r\n\r\n")) {
		if _, err := io.ReadFull(conn, head); err != nil {
			conn.Close()
			return nil, err
		}
		got = append(got, head[0])
		if len(got) > 8192 {
			conn.Close()
			return nil, errors.New("握手响应过长")
		}
	}
	if !bytes.Contains(got, []byte("101")) {
		conn.Close()
		return nil, fmt.Errorf("CDP 握手失败: %s", truncateStr(string(got), 100))
	}
	return conn, nil
}
