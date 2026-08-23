// handlers_extra.go —— 多账号池 / 进行中请求 / 注水探针的 HTTP 端点。
// 均带 CORS（GUI 跨域拉取），路径与群友 Codely Relay 的管理语义对齐。
package tuanjie

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"strconv"
	"time"
)

// handleAccounts GET=账号列表+被动注水事件；POST=增删/启停/GLM 标记（action 字段分发）。
func (s *Server) handleAccounts(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		writeJSON(w, map[string]any{
			"ok":       true,
			"accounts": s.pool.Status(),
			"mode":     map[bool]string{true: "pool", false: "single"}[s.pool.Size() > 0],
			"passive":  s.water.PassiveEvents(),
		})
		return
	}
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Action   string `json:"action"` // add | remove | toggle | setglm
		UserID   string `json:"user_id"`
		Token    string `json:"token"`
		Username string `json:"username"`
		OrgID    string `json:"org_id"`
		Enabled  bool   `json:"enabled"`
		HasGLM53 bool   `json:"has_glm53"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "请求体解析失败"})
		return
	}
	switch req.Action {
	case "add":
		if req.UserID == "" || req.Token == "" {
			writeJSON(w, map[string]any{"ok": false, "msg": "缺少 user_id 或 token"})
			return
		}
		if s.pool.Add(req.UserID, req.Token, req.Username, req.OrgID) {
			writeJSON(w, map[string]any{"ok": true, "msg": "账号已添加"})
		} else {
			writeJSON(w, map[string]any{"ok": false, "msg": "账号已存在"})
		}
	case "remove":
		writeJSON(w, map[string]any{"ok": s.pool.Remove(req.UserID)})
	case "toggle":
		writeJSON(w, map[string]any{"ok": s.pool.Toggle(req.UserID, req.Enabled)})
	case "setglm":
		writeJSON(w, map[string]any{"ok": s.pool.SetGLM(req.UserID, req.HasGLM53)})
	case "auto":
		s.handleAccountAuto(w, r)
		return
	default:
		writeJSON(w, map[string]any{"ok": false, "msg": "未知 action"})
	}
}

// handleAccountAuto 自动探测：连浏览器调试口读团结 cookie → 解析入池。
// 浏览器没开调试口时 spawn（带 --remote-debugging-port，同一用户 profile，
// 登录态保留），等就绪后重试。探测/入池全程只针对 codely.tuanjie.cn 域。
func (s *Server) handleAccountAuto(w http.ResponseWriter, r *http.Request) {
	// 1. 直接探测（浏览器可能已带调试口在跑）
	if creds := probeCDPBrowser(); creds != nil {
		if s.pool.Add(creds.UserID, creds.AccessToken, creds.UserID, "") {
			writeJSON(w, map[string]any{"ok": true, "msg": "已从浏览器读取并添加账号", "user_id": creds.UserID, "browser": creds.Browser})
		} else {
			writeJSON(w, map[string]any{"ok": false, "msg": "该账号已在池里（user_id " + creds.UserID + "）", "user_id": creds.UserID})
		}
		return
	}
	// 2. 探测不到 → 拉起【专用 profile】浏览器带调试口（136+ 版本安全限制：
	//    默认 profile 忽略调试参数，必须独立 user-data-dir），打开团结 dashboard。
	path, err := launchBrowserWithCDP("9222")
	if err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "未探测到调试口，且启动浏览器失败: " + err.Error()})
		return
	}
	// 3. 长轮询等登录：专用 profile 是全新环境，需要在弹出的窗口里登录一次；
	//    登录后 cookie 写入该 profile，读取入池。最长 150 秒，每 2 秒查一次。
	deadline := time.Now().Add(150 * time.Second)
	for time.Now().Before(deadline) {
		time.Sleep(2 * time.Second)
		if creds := probeCDPBrowser(); creds != nil {
			if s.pool.Add(creds.UserID, creds.AccessToken, creds.UserID, "") {
				writeJSON(w, map[string]any{"ok": true, "msg": "已读取登录态并入池（弹出的浏览器窗口可以关了）", "user_id": creds.UserID, "browser": path})
			} else {
				writeJSON(w, map[string]any{"ok": false, "msg": "该账号已在池里（user_id " + creds.UserID + "）", "user_id": creds.UserID})
			}
			return
		}
		if r.Context().Err() != nil {
			return // 客户端断开，不再等
		}
	}
	writeJSON(w, map[string]any{"ok": false, "msg": "等待登录超时（150 秒）——请在弹出的浏览器窗口里登录团结账号后再点一次探测"})
}

// handleActivity 实时动态（最近事件，GUI 轮询；学群友 /api/activity）。
func (s *Server) handleActivity(w http.ResponseWriter, r *http.Request) {
	limit := 60
	if l := r.URL.Query().Get("limit"); l != "" {
		if n, err := strconv.Atoi(l); err == nil && n > 0 && n <= 200 {
			limit = n
		}
	}
	writeJSON(w, map[string]any{"ok": true, "events": s.activity.List(limit)})
}

// handleInflight 进行中请求快照（GUI 面板轮询）。
func (s *Server) handleInflight(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, map[string]any{"ok": true, "requests": s.registry.Inflight()})
}

// handleWaterProbe 金丝雀探针：POST {user_id, model} → 直连该账号探测。
func (s *Server) handleWaterProbe(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		UserID string `json:"user_id"`
		Model  string `json:"model"`
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "请求体解析失败"})
		return
	}
	if req.Model == "" {
		req.Model = "GLM-5.3" // 默认探测最贵的
	}
	if req.UserID == "" || req.UserID == "all" {
		// 全部账号逐个探（上限防失控）
		ids := s.pool.SortedUserIDs()
		if len(ids) == 0 {
			writeJSON(w, map[string]any{"ok": false, "msg": "账号池为空（单账号模式请指定 user_id）"})
			return
		}
		if len(ids) > 10 {
			ids = ids[:10]
		}
		results := []*WaterProbeResult{}
		for _, uid := range ids {
			res, err := s.water.ProbeAccount(r.Context(), s.pool, uid, req.Model)
			if err != nil {
				results = append(results, &WaterProbeResult{UserID: uid, Model: req.Model, Pass: false, Detail: err.Error()})
				continue
			}
			results = append(results, res)
		}
		writeJSON(w, map[string]any{"ok": true, "results": results})
		return
	}
	res, err := s.water.ProbeAccount(r.Context(), s.pool, req.UserID, req.Model)
	if err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": err.Error()})
		return
	}
	writeJSON(w, map[string]any{"ok": true, "results": []*WaterProbeResult{res}})
}

// accountTokenFor 返回当前请求应使用的 access_token（多账号池选号或单账号回退）。
// 多账号模式返回 (token, userID, true)；单账号返回 ("", "", false) 表示走 Client 原路径。
func (s *Server) accountTokenFor(model string) (string, string, bool) {
	if s.pool.Size() == 0 {
		return "", "", false
	}
	acc := s.pool.Pick(model)
	if acc == nil {
		return "", "", false
	}
	return acc.AccessToken, acc.UserID, true
}

// ForwardDirect 用指定 access_token 直连转发（多账号池的 chat 转发用：
// key 换取按账号独立，不经 Client 单账号缓存）。
func (s *Server) ForwardDirect(ctx context.Context, method, path string, body []byte, accessToken string) (*http.Response, error) {
	key, err := fetchKeyWithToken(ctx, accessToken)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, method, litellmAPIBase+path, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	for k, vs := range s.client.litellmHeaders(path, key) {
		for _, v := range vs {
			req.Header.Add(k, v)
		}
	}
	return s.client.httpClient.Do(req)
}
