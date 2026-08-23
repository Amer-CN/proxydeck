// cdp_browser.go —— 从用户浏览器（Chrome/Edge）自动提取团结登录态。
// 链路复用 Notion 插件验证过的 CDP 方案：探测调试口 → WebSocket →
// Network.getCookies(codely.tuanjie.cn) → 解析 token/user_id 自动入池。
//
// 用户浏览器默认不开调试口。探测不到时由前端引导用户确认，
// 再由本模块带 --remote-debugging-port 拉起浏览器（同一用户 profile，
// 登录态/书签/扩展全保留），读取后用户随时可正常重启浏览器恢复普通模式。
package tuanjie

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
	"strings"
	"syscall"
	"time"

	"github.com/google/uuid"
)

// codelyCookieURL 是要读取 cookie 的目标域。
const codelyCookieURL = "https://codely.tuanjie.cn"

// 常见浏览器调试口（Chrome 惯例 9222，Notion 插件用 9224/9225，多留几个探测面）。
var cdpBrowserPorts = []string{"9222", "9223", "9224", "9225"}

// browserPaths 是候选浏览器 exe（按常见安装路径）。
var browserPaths = []string{
	`C:\Program Files\Google\Chrome\Application\chrome.exe`,
	`C:\Program Files (x86)\Google\Chrome\Application\chrome.exe`,
	`C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`,
	`C:\Program Files\Microsoft\Edge\Application\msedge.exe`,
}

// CDPBrowserCreds 是从浏览器提取到的团结凭据。
type CDPBrowserCreds struct {
	AccessToken string `json:"access_token"`
	UserID      string `json:"user_id"`
	Browser     string `json:"browser"` // chrome | edge
	Port        string `json:"port"`
}

// probeCDPBrowser 探测所有候选调试口，找到能连的且已登录团结的浏览器。
// 返回 nil 表示没有浏览器带调试口在跑。
func probeCDPBrowser() *CDPBrowserCreds {
	for _, port := range cdpBrowserPorts {
		creds, err := cdpFetchCodelyCreds(port)
		if err == nil && creds != nil {
			return creds
		}
	}
	return nil
}

// cdpFetchCodelyCreds 连指定端口的 CDP，读 codely 域 cookie 并解析凭据。
// 不需要已打开 codely 页面——Network.getCookies 按 URL 匹配 cookie 存储，
// 只要浏览器里有该域的 cookie（登录过）即可读到。
func cdpFetchCodelyCreds(port string) (*CDPBrowserCreds, error) {
	// 1. 列页面拿一个可用 WebSocket 端点（cookie 读取不需要特定页面，取第一个 page 即可）
	wsURL, err := cdpAnyPageWS(port)
	if err != nil {
		return nil, err
	}
	// 2. WebSocket 读 cookie
	cookies, err := cdpBrowserGetCookies(wsURL)
	if err != nil {
		return nil, err
	}
	// 3. 从 cookie 里找 token（团结的会话 cookie 名含 token/session/auth 之一，
	//    值是 eyJ 开头的 JWT）
	token := ""
	for _, c := range cookies {
		name, _ := c["name"].(string)
		value, _ := c["value"].(string)
		if value == "" || !strings.HasPrefix(value, "eyJ") {
			continue
		}
		ln := strings.ToLower(name)
		if strings.Contains(ln, "token") || strings.Contains(ln, "session") ||
			strings.Contains(ln, "auth") || strings.Contains(ln, "authorization") {
			token = value
			break
		}
	}
	if token == "" {
		// 兜底：任何 eyJ 开头的长 cookie 都试（>=60 字符基本是 JWT）
		for _, c := range cookies {
			value, _ := c["value"].(string)
			if strings.HasPrefix(value, "eyJ") && len(value) >= 60 {
				token = value
				break
			}
		}
	}
	if token == "" {
		// cookie 没读到：可能是页面还停在营销首页（登录态未激活）。
		// 让第一个页面导航到 dashboard/usage 触发 cookie 域激活，等 1.5s 再读一次。
		_ = cdpNavigate(wsURL, "https://codely.tuanjie.cn/dashboard/usage")
		time.Sleep(1500 * time.Millisecond)
		cookies2, err2 := cdpBrowserGetCookies(wsURL)
		if err2 == nil {
			for _, c := range cookies2 {
				value, _ := c["value"].(string)
				if strings.HasPrefix(value, "eyJ") && len(value) >= 60 {
					token = value
					break
				}
			}
		}
	}
	if token == "" {
		return nil, errors.New("cookie 里没找到团结登录态（浏览器里登录 codely.tuanjie.cn 了吗？）")
	}
	// 4. 解 JWT 拿 user_id（sub）
	uid := jwtSub(token)
	if uid == "" {
		return nil, errors.New("token 解析失败（非 JWT？）")
	}
	// 5. 浏览器标识（按端口猜不严谨，从 version info 拿）
	browser := cdpBrowserName(port)
	return &CDPBrowserCreds{AccessToken: token, UserID: uid, Browser: browser, Port: port}, nil
}

// cdpAnyPageWS 连 /json 列表取第一个 page 的 webSocketDebuggerUrl。
func cdpAnyPageWS(port string) (string, error) {
	cl := &http.Client{Timeout: 2 * time.Second}
	r, err := cl.Get("http://127.0.0.1:" + port + "/json")
	if err != nil {
		return "", err
	}
	defer r.Body.Close()
	var targets []struct {
		Type string `json:"type"`
		WS   string `json:"webSocketDebuggerUrl"`
	}
	if err := json.NewDecoder(r.Body).Decode(&targets); err != nil {
		return "", err
	}
	for _, t := range targets {
		if t.Type == "page" && t.WS != "" {
			return t.WS, nil
		}
	}
	return "", errors.New("无可用页面")
}

// cdpBrowserName 从 /json/version 的 Browser 字段判断浏览器名。
func cdpBrowserName(port string) string {
	cl := &http.Client{Timeout: 2 * time.Second}
	r, err := cl.Get("http://127.0.0.1:" + port + "/json/version")
	if err != nil {
		return "browser"
	}
	defer r.Body.Close()
	var v struct {
		Browser string `json:"Browser"`
	}
	_ = json.NewDecoder(r.Body).Decode(&v)
	if strings.Contains(v.Browser, "Edg") {
		return "edge"
	}
	if strings.Contains(v.Browser, "Chrome") {
		return "chrome"
	}
	return "browser"
}

// jwtSub 解 JWT payload 的 sub 字段（不校验签名）。
func jwtSub(token string) string {
	parts := splitJWT(token)
	if len(parts) != 3 {
		return ""
	}
	// base64url 解 payload
	payload := parts[1]
	// 补 padding
	switch len(payload) % 4 {
	case 2:
		payload += "=="
	case 3:
		payload += "="
	}
	decoded, err := base64URLDecode(payload)
	if err != nil {
		return ""
	}
	var p struct {
		Sub string `json:"sub"`
	}
	if json.Unmarshal(decoded, &p) != nil {
		return ""
	}
	return p.Sub
}

// base64URLDecode 标准 base64url 解码（独立小函数，避免依赖混乱）。
func base64URLDecode(s string) ([]byte, error) {
	// 标准库 base64.RawURLEncoding
	const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
	var idx [256]int
	for i := range idx {
		idx[i] = -1
	}
	for i := 0; i < len(alphabet); i++ {
		idx[alphabet[i]] = i
	}
	// 去掉 padding
	s = strings.TrimRight(s, "=")
	n := len(s)
	if n%4 == 1 {
		return nil, errors.New("base64 长度非法")
	}
	out := make([]byte, 0, n*3/4)
	var buf, bits int
	for i := 0; i < n; i++ {
		v := idx[s[i]]
		if v < 0 {
			return nil, fmt.Errorf("base64 非法字符 %q", s[i])
		}
		buf = buf<<6 | v
		bits += 6
		if bits >= 8 {
			bits -= 8
			out = append(out, byte(buf>>bits))
		}
	}
	return out, nil
}

// launchBrowserWithCDP 让浏览器带调试口运行（同一用户 profile）。
// 关键：同 profile 的浏览器已在跑时，新进程的调试参数会被忽略（Windows 实例合并）。
// 做法：先结束现有浏览器进程（Edge 默认"启动继续上次会话"，重开后标签页和登录态都在），
// 再带调试口拉起并打开团结 dashboard。
func launchBrowserWithCDP(port string) (string, error) {
	for _, p := range browserPaths {
		if !fileExistsStr(p) {
			continue
		}
		// 1. 该浏览器是否在跑？（Edge 和 chrome 分别查）
		procName := "chrome.exe"
		if strings.Contains(p, "msedge") {
			procName = "msedge.exe"
		}
		if processRunning(procName) {
			// 结束现有实例（--no-startup-window 的后台进程也算）
			_ = hiddenCmd("taskkill", "/F", "/IM", procName).Run()
			time.Sleep(1500 * time.Millisecond)
		}
		// 2. 带调试口拉起（打开 dashboard 触发 cookie 激活；会话恢复交给浏览器默认行为）
		cmd := exec.Command(p,
			"--remote-debugging-port="+port,
			"--no-first-run",
			"--no-default-browser-check",
			"--restore-last-session",
			"https://codely.tuanjie.cn/dashboard/usage",
		)
		if err := cmd.Start(); err != nil {
			continue
		}
		go func() { _ = cmd.Wait() }()
		return p, nil
	}
	return "", errors.New("未找到 Chrome/Edge（请安装其一或手动添加账号）")
}

// hiddenCmd 创建隐藏窗口的命令（不弹黑窗；netstat/taskkill 等 CLI 工具用）。
func hiddenCmd(name string, args ...string) *exec.Cmd {
	cmd := exec.Command(name, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true, CreationFlags: 0x08000000}
	return cmd
}

// processRunning 检查指定进程名是否有实例在跑。
func processRunning(name string) bool {
	out, err := hiddenCmd("tasklist", "/FI", "IMAGENAME eq "+name).Output()
	if err != nil {
		return false
	}
	return strings.Contains(strings.ToLower(string(out)), strings.ToLower(name))
}

func fileExistsStr(p string) bool {
	_, err := os.Stat(p)
	return err == nil
}

// ---------- 最小 WebSocket 客户端（与 Notion 插件同款实现） ----------

// cdpBrowserGetCookies 连 WebSocket 端点，发 Network.getCookies 读 codely 域 cookie。
func cdpBrowserGetCookies(pageWS string) ([]map[string]any, error) {
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
		return nil, fmt.Errorf("WebSocket 握手失败: %s", string(head[:minInt(120, len(head))]))
	}
	// 多域匹配：团结的登录 cookie 可能挂 codely 子域，也可能挂 tuanjie.cn 父域——
	// 两个根 URL 都传，CDP 按域匹配 cookie 存储全覆盖
	msg := []byte(`{"id":1,"method":"Network.getCookies","params":{"urls":[
		"https://codely.tuanjie.cn",
		"https://tuanjie.cn",
		"https://codely.tuanjie.cn/dashboard/usage"
	]}}`)
	if err := cdpWriteWSFrame(conn, msg); err != nil {
		return nil, err
	}
	var payload []byte
	for {
		frame, err := cdpReadWSFrame(conn)
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

// cdpNavigate 让指定页面导航到目标 URL（触发 cookie 激活用）。
// cdpNavigate 让指定页面导航到目标 URL（触发 cookie 激活用）。
func cdpNavigate(pageWS, targetURL string) error {
	u, err := url.Parse(pageWS)
	if err != nil {
		return err
	}
	host := u.Host
	if !strings.Contains(host, ":") {
		host += ":80"
	}
	conn, err := net.DialTimeout("tcp", host, 5*time.Second)
	if err != nil {
		return err
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(8 * time.Second))
	key := uuid.NewString()[:16] + "=="
	req := "GET " + u.RequestURI() + " HTTP/1.1\r\nHost: " + u.Host + "\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " + key + "\r\nSec-WebSocket-Version: 13\r\n\r\n"
	if _, err := conn.Write([]byte(req)); err != nil {
		return err
	}
	buf := make([]byte, 1)
	var head []byte
	for len(head) < 4 || !bytes.HasSuffix(head, []byte("\r\n\r\n")) {
		if _, err := io.ReadFull(conn, buf); err != nil {
			return err
		}
		head = append(head, buf[0])
		if len(head) > 8192 {
			return errors.New("握手响应过长")
		}
	}
	if !bytes.Contains(head, []byte("101")) {
		return errors.New("WebSocket 握手失败")
	}
	msg, _ := json.Marshal(map[string]any{
		"id": 2, "method": "Page.navigate",
		"params": map[string]any{"url": targetURL},
	})
	if err := cdpWriteWSFrame(conn, msg); err != nil {
		return err
	}
	var payload []byte
	for {
		frame, err := cdpReadWSFrame(conn)
		if err != nil {
			return err
		}
		payload = append(payload, frame...)
		var resp struct {
			ID int `json:"id"`
		}
		if json.Unmarshal(payload, &resp) == nil && resp.ID == 2 {
			return nil
		}
		if len(payload) > 1<<20 {
			return errors.New("CDP 响应过大")
		}
	}
}

func cdpWriteWSFrame(conn net.Conn, data []byte) error {
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

func cdpReadWSFrame(conn net.Conn) ([]byte, error) {
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
	if opcode == 0x8 {
		return nil, errors.New("连接被关闭")
	}
	if opcode == 0x9 {
		return cdpReadWSFrame(conn)
	}
	return data, nil
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}
