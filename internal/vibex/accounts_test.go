// accounts_test.go —— 账号接口（action=auto 三段式 / action=add 手动粘贴）与
// /health 的 days_left 字段。自动探测的 CDP 部分用本地【假 CDP 服务端】回放
// （/json 列表 + WS 的 Network.getCookies / Runtime.evaluate），不需要真浏览器。
package vibex

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

// ---------- 假 JWT / 测试服务 ----------

// fakeJWT 造一个结构合法的假 JWT（header.payload.sig，payload 里带 exp/nickName）。
// 签名段无所谓——上游校验留给真站，本插件只解 payload 与探活。
func fakeJWT(t *testing.T, exp time.Time, nick string) string {
	t.Helper()
	payload, _ := json.Marshal(map[string]any{
		"sub": "u-1", "nickName": nick, "exp": exp.Unix(),
	})
	enc := func(s string) string { return base64.RawURLEncoding.EncodeToString([]byte(s)) }
	return enc(`{"alg":"HS256","typ":"JWT"}`) + "." + enc(string(payload)) + ".sig"
}

// newAccountTestServer 建一个指向假上游、并把配置文件指到临时目录的服务
// （落盘断言用；探活打的就是这个假上游）。
func newAccountTestServer(t *testing.T) (*Server, *fakeUpstream, string) {
	t.Helper()
	up := newFakeUpstream(t)
	s := newTestServer(t, up.url())
	cfgPath := filepath.Join(t.TempDir(), "vibex-config.json")
	s.cfg.path = cfgPath
	return s, up, cfgPath
}

// postAccounts 发一次 POST /accounts。
func postAccounts(t *testing.T, url, body string) (int, string) {
	t.Helper()
	resp, err := http.Post(url+"/accounts", "application/json", strings.NewReader(body))
	if err != nil {
		t.Fatalf("POST /accounts 失败: %v", err)
	}
	defer func() { _ = resp.Body.Close() }()
	b, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(b)
}

// ---------- 假 CDP 服务端 ----------

// fakeCDP 是一个假浏览器调试口：/json 给 page 列表，WS 上按方法回放
// Network.getCookies（给 cookie 名→值）与 Runtime.evaluate（给 localStorage 值）。
// cookies=nil 表示浏览器里没有 VibeX 登录态（走超时路径）。
type fakeCDP struct {
	srv    *httptest.Server
	mu     sync.Mutex
	method []string // 收到过的 CDP 方法名（按到达顺序）
	// cookie 名 → 值；为空表示读不到
	cookies map[string]string
	local   map[string]string
}

func newFakeCDP(t *testing.T, cookies, local map[string]string) *fakeCDP {
	t.Helper()
	f := &fakeCDP{cookies: cookies, local: local}
	mux := http.NewServeMux()
	mux.HandleFunc("/json", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, []map[string]any{{
			"type":                 "page",
			"webSocketDebuggerUrl": "ws://" + f.srv.Listener.Addr().String() + "/devtools/page/fake-1",
		}})
	})
	mux.HandleFunc("/json/version", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, map[string]any{"Browser": "Chrome/136.0.0.0"})
	})
	mux.HandleFunc("/devtools/page/fake-1", f.handleWS)
	f.srv = httptest.NewServer(mux)
	t.Cleanup(f.srv.Close)
	return f
}

// port 返回假 CDP 的端口（探测函数会去连它）。
func (f *fakeCDP) port() string {
	_, port, _ := net.SplitHostPort(f.srv.Listener.Addr().String())
	return port
}

// calls 返回收到过的 CDP 方法名（按到达顺序）。
func (f *fakeCDP) calls() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.method...)
}

func (f *fakeCDP) handleWS(w http.ResponseWriter, r *http.Request) {
	hj, ok := w.(http.Hijacker)
	if !ok {
		return
	}
	conn, rw, err := hj.Hijack()
	if err != nil {
		return
	}
	defer func() { _ = conn.Close() }()
	fmt.Fprintf(rw, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"+
		"Sec-WebSocket-Accept: %s\r\n\r\n", wsAccept(r.Header.Get("Sec-WebSocket-Key")))
	if err := rw.Flush(); err != nil {
		return
	}
	for {
		_ = conn.SetReadDeadline(time.Now().Add(10 * time.Second))
		op, _, payload, err := readFrame(rw.Reader)
		if err != nil {
			return
		}
		if op == wsOpClose {
			return
		}
		if op != wsOpText {
			continue
		}
		var req struct {
			ID     int            `json:"id"`
			Method string         `json:"method"`
			Params map[string]any `json:"params"`
		}
		if json.Unmarshal(payload, &req) != nil {
			continue
		}
		f.mu.Lock()
		f.method = append(f.method, req.Method)
		cookies, local := f.cookies, f.local
		f.mu.Unlock()
		switch req.Method {
		case "Network.getCookies":
			list := []map[string]any{}
			for k, v := range cookies {
				list = append(list, map[string]any{"name": k, "value": v, "domain": ".runninghub.cn"})
			}
			_ = writeFrame(rw, wsOpText, mustJSON(map[string]any{
				"id": req.ID, "result": map[string]any{"cookies": list}}), false)
		case "Runtime.evaluate":
			val := ""
			if expr, _ := req.Params["expression"].(string); strings.Contains(expr, vibexCookieName) {
				val = local[vibexCookieName]
			}
			_ = writeFrame(rw, wsOpText, mustJSON(map[string]any{
				"id": req.ID, "result": map[string]any{"result": map[string]any{"value": val}}}), false)
		default:
			_ = writeFrame(rw, wsOpText, mustJSON(map[string]any{
				"id": req.ID, "result": map[string]any{}}), false)
		}
		_ = rw.Flush()
	}
}

func mustJSON(v any) []byte {
	b, _ := json.Marshal(v)
	return b
}

// withFakeCDP 把包级探测/拉起钩子换成假实现：探测=连假 CDP 读一次，
// 拉起=只记下被拉起的目录（不真开浏览器）。返回被拉起目录的记录。
func withFakeCDP(t *testing.T, cdp *fakeCDP) *[]string {
	t.Helper()
	oldFetch, oldLaunch, oldTimeout, oldPorts := cdpFetchCreds, cdpLaunchBrowser, autoWaitTimeout, cdpBrowserPorts
	var launched []string
	cdpFetchCreds = func(port string) (*vibexCreds, error) {
		return probeCDPPort(port)
	}
	cdpLaunchBrowser = func(port, profileDir string) (string, error) {
		launched = append(launched, profileDir)
		_ = os.MkdirAll(profileDir, 0o700)
		return `C:\Program Files\Google\Chrome\Application\chrome.exe`, nil
	}
	autoWaitTimeout = 2 * time.Second // 单测不必真等 150 秒
	cdpBrowserPorts = []string{cdp.port()}
	t.Cleanup(func() {
		cdpFetchCreds, cdpLaunchBrowser = oldFetch, oldLaunch
		autoWaitTimeout, cdpBrowserPorts = oldTimeout, oldPorts
		// 假拉起也会在 %TEMP% 下建出 profile 目录（生产行为），测试自己收尾，
		// 免得每跑一次测试就在系统临时目录里留一堆空目录
		for _, dir := range launched {
			_ = os.RemoveAll(dir)
		}
	})
	return &launched
}

// ---------- 验收 1：假 CDP 回放 → 读到 Rh-Accesstoken → 入池 ----------

// TestAutoDetectReadsCookieFromCDP：cookie 里有 Rh-Accesstoken →
// action=auto 直接探测成功，token 入池并落盘、/health tokens[] 含 days_left、
// days_left_max 正确。
func TestAutoDetectReadsCookieFromCDP(t *testing.T) {
	s, _, cfgPath := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	tok := fakeJWT(t, time.Now().Add(10*24*time.Hour), "小明")
	cdp := newFakeCDP(t, map[string]string{vibexCookieName: tok}, nil)
	withFakeCDP(t, cdp)

	code, body := postAccounts(t, ts.URL, `{"action":"auto"}`)
	if code != http.StatusOK {
		t.Fatalf("自动探测应 200，得到 %d：%s", code, body)
	}
	var got map[string]any
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatalf("响应不是 JSON: %v (%s)", err, body)
	}
	if got["ok"] != true || got["user_id"] != "u-1" {
		t.Fatalf("自动探测应成功且带 user_id: %s", body)
	}
	if s.pool.Len() != 1 {
		t.Fatalf("池里应有一个 token，实际 %d", s.pool.Len())
	}
	if calls := cdp.calls(); len(calls) == 0 || calls[0] != "Network.getCookies" {
		t.Fatalf("应通过 CDP Network.getCookies 读 cookie，实际调用 %v", calls)
	}
	// 落盘：cookie 路径读到的 token 也要写进配置文件（重启后才不用重登）
	if raw, err := os.ReadFile(cfgPath); err != nil || !strings.Contains(string(raw), tok) {
		t.Fatalf("读到的 token 未落盘: err=%v", err)
	}

	_, hbody := getBody(t, ts.URL+"/health")
	var h map[string]any
	if err := json.Unmarshal([]byte(hbody), &h); err != nil {
		t.Fatalf("/health 不是 JSON: %v (%s)", err, hbody)
	}
	if h["token_configured"] != true {
		t.Fatalf("token_configured 应为 true: %s", hbody)
	}
	if d, _ := h["days_left_max"].(float64); d < 9 || d > 10 {
		t.Fatalf("/health days_left_max 应约 10，得到 %v（%s）", h["days_left_max"], hbody)
	}
	toks, _ := h["tokens"].([]any)
	if len(toks) != 1 {
		t.Fatalf("tokens 应 1 条: %s", hbody)
	}
	first, _ := toks[0].(map[string]any)
	if first["days_left"] == nil || first["exp"] == nil {
		t.Fatalf("tokens[0] 缺 days_left/exp: %s", hbody)
	}
	if d, _ := first["days_left"].(float64); d < 9 || d > 10 {
		t.Fatalf("tokens[0].days_left 应约 10，得到 %v", first["days_left"])
	}
	if strings.Contains(hbody, tok) {
		t.Fatalf("/health 泄漏了 token 本体")
	}

	// 同账号再探一次：已在池里 → fall through 到弹窗流程，不重复入池
	code2, body2 := postAccounts(t, ts.URL, `{"action":"auto"}`)
	if code2 != http.StatusOK {
		t.Fatalf("重复探测应 200，得到 %d：%s", code2, body2)
	}
	if s.pool.Len() != 1 {
		t.Fatalf("重复探测不应再入池，实际 %d", s.pool.Len())
	}
}

// TestAutoDetectLocalStorageFallback：cookie 里没有、localStorage 里有 →
// 兜底读到并入池（上游 README 写的是 Local Storage，两条路都要能走通）。
func TestAutoDetectLocalStorageFallback(t *testing.T) {
	s, _, _ := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	tok := fakeJWT(t, time.Now().Add(20*24*time.Hour), "只存 LS")
	cdp := newFakeCDP(t, map[string]string{"other": "x"}, map[string]string{vibexCookieName: tok})
	withFakeCDP(t, cdp)

	code, body := postAccounts(t, ts.URL, `{"action":"auto"}`)
	if code != http.StatusOK || !strings.Contains(body, `"ok":true`) {
		t.Fatalf("localStorage 兜底应成功，得到 %d：%s", code, body)
	}
	if s.pool.Len() != 1 {
		t.Fatalf("池里应有一个 token，实际 %d", s.pool.Len())
	}
	calls := cdp.calls()
	if len(calls) < 2 || calls[0] != "Network.getCookies" || calls[1] != "Runtime.evaluate" {
		t.Fatalf("应先读 cookie 再兜底读 localStorage，实际调用 %v", calls)
	}
}

// TestAutoDetectInvalidTokenFallsThrough：cookie 里是无效 token（探活 401）→
// 不入池，fall through 到弹窗流程（拉起浏览器），等登录到期回可读文案。
func TestAutoDetectInvalidTokenFallsThrough(t *testing.T) {
	s, up, _ := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	tok := fakeJWT(t, time.Now().Add(time.Hour), "被吊销")
	up.reject[tok] = 401
	cdp := newFakeCDP(t, map[string]string{vibexCookieName: tok}, nil)
	launched := withFakeCDP(t, cdp)

	code, body := postAccounts(t, ts.URL, `{"action":"auto"}`)
	if code != http.StatusOK {
		t.Fatalf("应回 200 + ok:false，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, `"ok":false`) || !strings.Contains(body, "超时") {
		t.Fatalf("无效 token 应 fall through 到等登录并回超时文案: %s", body)
	}
	if s.pool.Len() != 0 {
		t.Fatalf("无效 token 不应入池，实际 %d", s.pool.Len())
	}
	if len(*launched) == 0 {
		t.Fatalf("fall through 时应拉起浏览器（未拉起）")
	}
	if p := (*launched)[0]; !strings.Contains(p, "proxydeck-vibex-probe-") {
		t.Fatalf("profile 目录名不合约定: %s", p)
	}
}

// TestAutoDetectTimeoutNoLogin：假 CDP 里没有任何 VibeX 登录态 →
// 拉起浏览器后（测试缩短的）预算内无登录 → 超时文案，不入池。
func TestAutoDetectTimeoutNoLogin(t *testing.T) {
	s, _, _ := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	cdp := newFakeCDP(t, nil, nil)
	launched := withFakeCDP(t, cdp)

	code, body := postAccounts(t, ts.URL, `{"action":"auto"}`)
	if code != http.StatusOK {
		t.Fatalf("超时应回 200 + ok:false，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, `"ok":false`) || !strings.Contains(body, "超时") ||
		!strings.Contains(body, "150 秒") {
		t.Fatalf("超时文案不对: %s", body)
	}
	if s.pool.Len() != 0 {
		t.Fatalf("没登录不应入池，实际 %d", s.pool.Len())
	}
	if len(*launched) == 0 {
		t.Fatalf("应拉起浏览器（未拉起）")
	}
}

// ---------- 验收 2：action=add 手动粘贴（合法 / 非法 / 重复）----------

// TestAddAccountValidJWT：合法 JWT + 探活 200 → 入池 + 落盘 + days_left。
func TestAddAccountValidJWT(t *testing.T) {
	s, _, cfgPath := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	tok := fakeJWT(t, time.Now().Add(7*24*time.Hour), "手动号")

	code, body := postAccounts(t, ts.URL, `{"action":"add","token":"`+tok+`"}`)
	if code != http.StatusOK {
		t.Fatalf("合法 token 应 200，得到 %d：%s", code, body)
	}
	var got map[string]any
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatalf("响应不是 JSON: %v (%s)", err, body)
	}
	if got["ok"] != true || got["existing"] != false || got["persisted"] != true {
		t.Fatalf("响应字段不对: %s", body)
	}
	if got["user"] != "手动号" {
		t.Fatalf("user 应取自 JWT nickName: %s", body)
	}
	if d, _ := got["days_left"].(float64); d < 6 || d > 7 {
		t.Fatalf("days_left 应约 7，得到 %v（%s）", got["days_left"], body)
	}
	if s.pool.Len() != 1 {
		t.Fatalf("池里应有一个 token，实际 %d", s.pool.Len())
	}
	raw, err := os.ReadFile(cfgPath)
	if err != nil {
		t.Fatalf("配置文件未落盘: %v", err)
	}
	if !strings.Contains(string(raw), tok) {
		t.Fatalf("配置文件里没写入 token")
	}
	// 落盘文件可再写（Windows 上 0600 表现为「非只读」；POSIX 位在 Windows 无意义）
	if fi, err := os.Stat(cfgPath); err == nil && fi.Mode().Perm()&0o222 == 0 {
		t.Fatalf("配置文件被标成只读，后续无法再追加 token: %v", fi.Mode().Perm())
	}
}

// TestAddAccountRejectsInvalid：非法串 / 空串 / 探活 401 → 400（带 Rh-Accesstoken 指引），
// 非 JSON 请求体 → 400；两种都不入池、不落盘。
func TestAddAccountRejectsInvalid(t *testing.T) {
	s, up, cfgPath := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	revoked := fakeJWT(t, time.Now().Add(time.Hour), "被吊销")
	up.reject[revoked] = 401

	for name, body := range map[string]string{
		"not-a-jwt":  `{"action":"add","token":"hello-world"}`,
		"empty":      `{"action":"add","token":""}`,
		"bad-base64": `{"action":"add","token":"aaa.@@@@.ccc"}`,
		"probe-401":  `{"action":"add","token":"` + revoked + `"}`,
	} {
		code, resp := postAccounts(t, ts.URL, body)
		if code != http.StatusBadRequest {
			t.Fatalf("%s 应 400，得到 %d：%s", name, code, resp)
		}
		if !strings.Contains(resp, "Rh-Accesstoken") {
			t.Fatalf("%s 的 400 文案应给 Rh-Accesstoken 指引: %s", name, resp)
		}
	}
	// 非 JSON 请求体：与旧 POST /token 同口径回 400
	if code, resp := postAccounts(t, ts.URL, "nope"); code != http.StatusBadRequest {
		t.Fatalf("非 JSON 体应 400，得到 %d：%s", code, resp)
	}
	if s.pool.Len() != 0 {
		t.Fatalf("非法 token 不应入池，实际 %d", s.pool.Len())
	}
	if _, err := os.Stat(cfgPath); !os.IsNotExist(err) {
		t.Fatalf("非法 token 不应落盘配置文件")
	}
}

// TestAddAccountDuplicate：重复粘贴同一 token → ok:false + existing:true，不重复入池。
func TestAddAccountDuplicate(t *testing.T) {
	s, _, _ := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	tok := fakeJWT(t, time.Now().Add(5*24*time.Hour), "重复号")
	bodyIn := `{"action":"add","token":"` + tok + `"}`

	if code, body := postAccounts(t, ts.URL, bodyIn); code != http.StatusOK {
		t.Fatalf("首次添加应 200，得到 %d：%s", code, body)
	}
	code, body := postAccounts(t, ts.URL, bodyIn)
	if code != http.StatusOK {
		t.Fatalf("重复添加应 200，得到 %d：%s", code, body)
	}
	if !strings.Contains(body, `"existing":true`) || !strings.Contains(body, "已在池里") {
		t.Fatalf("重复添加应提示已存在: %s", body)
	}
	if s.pool.Len() != 1 {
		t.Fatalf("重复添加不应再入池，实际 %d", s.pool.Len())
	}
}

// TestAddAccountExpiredJWT：过期的合法 JWT 也 200 + expired:true（甲板要显示「已过期」），
// days_left 为负、days_left_max = -1。
func TestAddAccountExpiredJWT(t *testing.T) {
	s, _, _ := newAccountTestServer(t)
	ts := newTestHTTP(t, s)
	tok := fakeJWT(t, time.Now().Add(-48*time.Hour), "老号")

	code, body := postAccounts(t, ts.URL, `{"action":"add","token":"`+tok+`"}`)
	if code != http.StatusOK {
		t.Fatalf("过期 JWT 也应 200（如实登记），得到 %d：%s", code, body)
	}
	if !strings.Contains(body, `"expired":true`) {
		t.Fatalf("过期 token 应回 expired:true: %s", body)
	}
	_, hbody := getBody(t, ts.URL+"/health")
	if !strings.Contains(hbody, `"days_left_max":-1`) {
		t.Fatalf("全池过期时 days_left_max 应为 -1: %s", hbody)
	}
}

// TestAccountsListAndUnknownAction：GET 快照脱敏；未知 action 回 ok:false。
func TestAccountsListAndUnknownAction(t *testing.T) {
	tok := fakeJWT(t, time.Now().Add(3*24*time.Hour), "列表号")
	s, _, _ := newAccountTestServer(t)
	ts := newTestHTTP(t, s)

	code, body := getBody(t, ts.URL+"/accounts")
	if code != http.StatusOK {
		t.Fatalf("GET /accounts 应 200，得到 %d", code)
	}
	if !strings.Contains(body, `"pool_size":0`) || !strings.Contains(body, `"token_configured":false`) {
		t.Fatalf("空池快照不对: %s", body)
	}

	if code, body := postAccounts(t, ts.URL, `{"action":"add","token":"`+tok+`"}`); code != http.StatusOK {
		t.Fatalf("添加应 200，得到 %d：%s", code, body)
	}
	_, body = getBody(t, ts.URL+"/accounts")
	if !strings.Contains(body, `"pool_size":1`) || strings.Contains(body, tok) {
		t.Fatalf("快照应含 1 个 token 且不带 token 本体: %s", body)
	}
	if _, body := postAccounts(t, ts.URL, `{"action":"remove"}`); !strings.Contains(body, "未知 action") {
		t.Fatalf("未知 action 应如实回错: %s", body)
	}
}

// TestDaysLeft：边界（向上取整、过期给 -1）。
func TestDaysLeft(t *testing.T) {
	now := time.Now()
	cases := []struct {
		name string
		exp  time.Time
		want int
	}{
		{"剩 10 天整", now.Add(10 * 24 * time.Hour), 10},
		{"剩 9.5 天", now.Add(9*24*time.Hour + 12*time.Hour), 10},
		{"剩 1 小时", now.Add(time.Hour), 1},
		{"已过期", now.Add(-time.Minute), -1},
	}
	for _, c := range cases {
		if got := daysLeft(c.exp, now); got != c.want {
			t.Fatalf("%s：daysLeft=%d，want %d", c.name, got, c.want)
		}
	}
}

// TestAddrOfDedup：cookie 值带属性尾巴时按首段去重（不成双份）。
func TestAddrOfDedup(t *testing.T) {
	p := NewPool(nil, tokenCooldown)
	if _, existing := p.add("aaa.bbb.ccc"); existing {
		t.Fatalf("首次添加不应算已存在")
	}
	if _, existing := p.add("aaa.bbb.ccc; Path=/; HttpOnly"); !existing {
		t.Fatalf("带属性尾巴的同值 token 应算已存在")
	}
	if p.Len() != 1 {
		t.Fatalf("池里应只有 1 个 token，实际 %d", p.Len())
	}
}
