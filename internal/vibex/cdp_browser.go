// cdp_browser.go —— 从用户浏览器（Chrome/Edge）自动提取 VibeX 登录态。
//
// 结构逐字照抄 internal/tuanjie/cdp_browser.go（零依赖手写 CDP：探测调试口 → /json
// 取 page WS → Network.getCookies → 找登录态 cookie），只有三处按 vibex 改：
// 域名 / cookie 名 / 拉起 URL。跨包抽取被简报明令禁止（tuanjie 包零改动），
// 故重复约 300 行——记一笔技术债，将来可抽到 internal/cdpbrowser 共享包。
//
// 实测差异（据实记录）：上游 README 写「F12 → Application → Local Storage →
// Rh-Accesstoken」，SPEC-T1 写「WS 鉴权只带 Cookie: Rh-Accesstoken」——两处说法不一致。
// 故本模块 cookie 与 localStorage 两条路都读（cookie 优先，同一次探测），
// 避免只认一条时自动探测在真机上直接失效。
//
// 测试钩子：cdpFetchCreds 与 cdpLaunchBrowser 是包级变量，单测可替换成假 CDP
// 服务端（不必真拉起浏览器）。
package vibex

import (
	"bytes"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// vibexCookieName 是登录态 cookie 名（SPEC-T1 §3.5 的 WS 鉴权同名）。
const vibexCookieName = "Rh-Accesstoken"

// vibexCookieURL 是要读取 cookie 的目标域（REST 与 WS 同一个域）。
const vibexCookieURL = "https://vibex.runninghub.cn"

// cdpBrowserPorts 是常见浏览器调试口（Chrome 惯例 9222；多留几个探测面）。
// 独立端口段起点取 9222-9229，与团结的探测范围互不干扰。
var cdpBrowserPorts = []string{"9222", "9223", "9224", "9225"}

// browserPaths 是候选浏览器 exe（按常见安装路径）。
var browserPaths = []string{
	`C:\Program Files\Google\Chrome\Application\chrome.exe`,
	`C:\Program Files (x86)\Google\Chrome\Application\chrome.exe`,
	`C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`,
	`C:\Program Files\Microsoft\Edge\Application\msedge.exe`,
}

// vibexCreds 是从浏览器提取到的 VibeX 登录态。
type vibexCreds struct {
	Token   string // Rh-Accesstoken 整串
	UserID  string // JWT sub
	Source  string // cookie | localstorage（如实记录取到的地方）
	Browser string // chrome | edge | browser
	Port    string // 取到它的调试口
}

// cdpFetchCreds 是探测单个调试口的实现（包级变量：单测替换成假 CDP 回放）。
var cdpFetchCreds = probeCDPPort

// cdpLaunchBrowser 是拉起带调试口的浏览器（包级变量：单测替换成假实现）。
var cdpLaunchBrowser = launchBrowserWithCDP

// probeCDPBrowser 探测所有候选调试口，找到能连的且已登录 VibeX 的浏览器。
// 返回 nil 表示没有浏览器带调试口在跑（或都没登录）。
func probeCDPBrowser() *vibexCreds {
	for _, port := range cdpBrowserPorts {
		if creds, err := cdpFetchCreds(port); err == nil && creds != nil {
			return creds
		}
	}
	return nil
}

// probeCDPBrowserQuiet 只探测单个端口的 CDP，读到登录态返回凭据，否则 nil。
// 安静版：不报错、不遍历其它端口——只盯本次拉起的会话窗口，避免读到别的窗口的旧登录态。
func probeCDPBrowserQuiet(port string) *vibexCreds {
	creds, err := cdpFetchCreds(port)
	if err != nil {
		return nil
	}
	return creds
}

// probeCDPPort 连指定端口的 CDP，读 VibeX 登录态并解析凭据。
// 不需要已打开 VibeX 页面——Network.getCookies 按 URL 匹配 cookie 存储，
// 只要浏览器里有该域的 cookie（登录过）即可读到；localStorage 兜底需要页面
// 在该域上（一次性 profile 打开的就是该域，故成立）。
func probeCDPPort(port string) (*vibexCreds, error) {
	wsURL, err := cdpAnyPageWS(port)
	if err != nil {
		return nil, err
	}
	u, err := parseWSURL(wsURL)
	if err != nil {
		return nil, err
	}
	conn, err := dialCDP(u)
	if err != nil {
		return nil, err
	}
	defer func() { _ = conn.Close() }()

	token, source := "", ""
	if c, err := cdpCookies(conn, u); err == nil {
		token = findVibexCookie(c)
		if token != "" {
			source = "cookie"
		}
	}
	if token == "" {
		if v, err := cdpLocalStorage(conn, u); err == nil && isJWTLooking(v) {
			token, source = strings.TrimSpace(v), "localstorage"
		}
	}
	if token == "" {
		return nil, errors.New("浏览器里没找到 VibeX 登录态（Rh-Accesstoken）——登录 vibex.runninghub.cn 了吗？")
	}
	info := decodeJWT(token)
	if info == nil || info.Sub == "" {
		return nil, errors.New("Rh-Accesstoken 解析失败（不是合法 JWT？）")
	}
	return &vibexCreds{
		Token: token, UserID: info.Sub, Source: source,
		Browser: cdpBrowserName(port), Port: port,
	}, nil
}

// findVibexCookie 按 cookie 名找 Rh-Accesstoken（只认该名，不做模糊猜测；
// 值须是 eyJ 开头的 JWT——同名 localStorage 项与 cookie 同源，格式一致）。
func findVibexCookie(cookies []map[string]any) string {
	for _, c := range cookies {
		name, _ := c["name"].(string)
		value, _ := c["value"].(string)
		if !strings.EqualFold(strings.TrimSpace(name), vibexCookieName) {
			continue
		}
		if isJWTLooking(value) {
			return value
		}
	}
	return ""
}

// isJWTLooking 报告一个串是否像 JWT（三段、首段 eyJ）。
func isJWTLooking(v string) bool {
	v = strings.TrimSpace(v)
	if !strings.HasPrefix(v, "eyJ") || len(v) < 40 {
		return false
	}
	return decodeJWT(v) != nil
}

// cdpAnyPageWS 连 /json 列表取第一个 page 的 webSocketDebuggerUrl。
func cdpAnyPageWS(port string) (string, error) {
	cl := &http.Client{Timeout: 2 * time.Second}
	r, err := cl.Get("http://127.0.0.1:" + port + "/json")
	if err != nil {
		return "", err
	}
	defer func() { _ = r.Body.Close() }()
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
	defer func() { _ = r.Body.Close() }()
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

// launchBrowserWithCDP 用【独立一次性 profile】拉起 Edge/Chrome 带调试口。
// 关键（实测）：Edge/Chrome 136+ 对默认 profile 忽略 --remote-debugging-port
// （安全变更），必须指定独立的 --user-data-dir 调试口才会开。
// profileDir 由调用方传入——每次探测一个全新目录，登录态不跨会话复用，
// 保证连续探测读到的是刚登录的账号而不是上一次的旧登录态。
// 打开的是 VibeX 首页（登录入口），与团结的 dashboard 对应。
func launchBrowserWithCDP(port, profileDir string) (string, error) {
	for _, p := range browserPaths {
		if !fileExistsStr(p) {
			continue
		}
		cmd := exec.Command(p,
			"--remote-debugging-port="+port,
			"--user-data-dir="+profileDir,
			"--no-first-run",
			"--no-default-browser-check",
			"--no-experiments",
			vibexCookieURL+"/",
		)
		if err := cmd.Start(); err != nil {
			continue
		}
		go func() { _ = cmd.Wait() }()
		return p, nil
	}
	return "", errors.New("未找到 Chrome/Edge（请安装其一，或在甲板上手动粘贴 token）")
}

// newProbeSessionDir 为每次探测生成一个全新的临时 profile 目录（带 UnixNano 后缀保证唯一）。
// 一次性会话：登录态只进本次目录，不跨探测复用，避免读到上一次账号。
func newProbeSessionDir() string {
	return filepath.Join(os.TempDir(), fmt.Sprintf("proxydeck-vibex-probe-%d", time.Now().UnixNano()))
}

// cleanupStaleProbeProfiles 清理 %TEMP% 下 mtime 超 24h 的 proxydeck-vibex-probe 前缀目录。
// 只清过期的：用户可能还没登录完（新目录未过期），交给本函数惰性清理。
func cleanupStaleProbeProfiles() {
	entries, err := os.ReadDir(os.TempDir())
	if err != nil {
		return
	}
	cutoff := time.Now().Add(-24 * time.Hour)
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		name := e.Name()
		if !strings.HasPrefix(name, "proxydeck-vibex-probe") {
			continue
		}
		info, err := e.Info()
		if err != nil {
			continue
		}
		if info.ModTime().Before(cutoff) {
			_ = os.RemoveAll(filepath.Join(os.TempDir(), name))
		}
	}
}

// freeCDPPort 从 9222 起到 9229 找第一个当前可监听的空闲端口，监听后立即关闭并返回。
// 使每个探测会话使用独立调试口，避免与其它在跑窗口/浏览器冲突
// （与团结的 9222-9229 同段：两插件不会同时探测，端口占用与否由 Listen 实测决定）。
func freeCDPPort() string {
	for i := 9222; i <= 9229; i++ {
		port := strconv.Itoa(i)
		ln, err := net.Listen("tcp", "127.0.0.1:"+port)
		if err != nil {
			continue
		}
		_ = ln.Close()
		return port
	}
	return "9222" // 兜底：全部占用时退回默认口（极少发生）
}

// hiddenCmd 创建隐藏窗口的命令（不弹黑窗）。
func hiddenCmd(name string, args ...string) *exec.Cmd {
	cmd := exec.Command(name, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true, CreationFlags: 0x08000000}
	return cmd
}

// fileExistsStr 报告路径是否存在（与团结同名小工具，各包各留一份）。
func fileExistsStr(p string) bool {
	_, err := os.Stat(p)
	return err == nil
}

// ---------- 最小 WebSocket 客户端（与团结 cdp_browser.go 同款实现） ----------

// dialCDP 建一条到 CDP WebSocket 端点的连接（含握手校验）。
func dialCDP(u *urlParts) (net.Conn, error) {
	host := u.host
	if !strings.Contains(host, ":") {
		host += ":80"
	}
	conn, err := net.DialTimeout("tcp", host, 5*time.Second)
	if err != nil {
		return nil, err
	}
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	req := "GET " + u.requestURI + " HTTP/1.1\r\nHost: " + u.host +
		"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: " + wsKey() +
		"\r\nSec-WebSocket-Version: 13\r\n\r\n"
	if _, err := conn.Write([]byte(req)); err != nil {
		_ = conn.Close()
		return nil, err
	}
	buf := make([]byte, 1)
	var head []byte
	for len(head) < 4 || !bytes.HasSuffix(head, []byte("\r\n\r\n")) {
		if _, err := io.ReadFull(conn, buf); err != nil {
			_ = conn.Close()
			return nil, err
		}
		head = append(head, buf[0])
		if len(head) > 8192 {
			_ = conn.Close()
			return nil, errors.New("握手响应过长")
		}
	}
	if !bytes.Contains(head, []byte("101")) {
		_ = conn.Close()
		return nil, fmt.Errorf("WebSocket 握手失败: %s", string(head[:minInt(120, len(head))]))
	}
	return conn, nil
}

// cdpCall 在一条连接上发一条 CDP 消息并读回 id 相同的那条响应。
func cdpCall(conn net.Conn, id int, method string, params map[string]any) ([]byte, error) {
	msg, err := json.Marshal(map[string]any{"id": id, "method": method, "params": params})
	if err != nil {
		return nil, err
	}
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
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
			ID int `json:"id"`
		}
		if json.Unmarshal(payload, &resp) == nil && resp.ID == id {
			return payload, nil
		}
		if len(payload) > 1<<20 {
			return nil, errors.New("CDP 响应过大")
		}
	}
}

// cdpCookies 读 VibeX 域 cookie。多域匹配：登录 cookie 可能挂
// vibex.runninghub.cn，也可能挂父域 runninghub.cn——把两个根 URL 都传，
// CDP 按域匹配 cookie 存储全覆盖。
func cdpCookies(conn net.Conn, u *urlParts) ([]map[string]any, error) {
	raw, err := cdpCall(conn, 1, "Network.getCookies", map[string]any{
		"urls": []string{vibexCookieURL, "https://runninghub.cn", vibexCookieURL + "/"},
	})
	if err != nil {
		return nil, err
	}
	var resp struct {
		Result struct {
			Cookies []map[string]any `json:"cookies"`
		} `json:"result"`
	}
	if err := json.Unmarshal(raw, &resp); err != nil {
		return nil, err
	}
	return resp.Result.Cookies, nil
}

// cdpLocalStorage 读该域页面的 localStorage 里同名键（上游 README 的提取位置；
// 与 cookie 两条路都读，见文件头说明）。页面不在该域时结果为空串。
func cdpLocalStorage(conn net.Conn, u *urlParts) (string, error) {
	expr := "String(localStorage.getItem('" + vibexCookieName + "') || '')"
	raw, err := cdpCall(conn, 2, "Runtime.evaluate", map[string]any{
		"expression": expr, "returnByValue": true,
	})
	if err != nil {
		return "", err
	}
	var resp struct {
		Result struct {
			Result struct {
				Value string `json:"value"`
			} `json:"result"`
		} `json:"result"`
	}
	if err := json.Unmarshal(raw, &resp); err != nil {
		return "", err
	}
	return resp.Result.Result.Value, nil
}

// urlParts 是 CDP WS 端点的极简解析结果（避免为一个 URL 引入更多分支）。
type urlParts struct {
	host       string
	requestURI string
}

// parseWSURL 解 ws://127.0.0.1:9222/devtools/page/XXX 形状的地址。
func parseWSURL(raw string) (*urlParts, error) {
	s := strings.TrimPrefix(strings.TrimPrefix(raw, "ws://"), "wss://")
	i := strings.Index(s, "/")
	if i < 0 {
		return nil, fmt.Errorf("CDP 端点非法: %s", raw)
	}
	host, path := s[:i], s[i:]
	if host == "" || path == "" {
		return nil, fmt.Errorf("CDP 端点非法: %s", raw)
	}
	return &urlParts{host: host, requestURI: path}, nil
}

// wsKey 生成握手用的 Sec-WebSocket-Key（随机 16 字节 base64；CDP 不校验内容）。
func wsKey() string {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		return "AAAAAAAAAAAAAAAAAAAAAA=="
	}
	return base64.StdEncoding.EncodeToString(b)
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
