// handlers_extra.go —— 进行中请求 / 注水探针 / 媒体配置的 HTTP 端点。
// 均带 CORS（GUI 跨域拉取）。
package tuanjie

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log"
	"math"
	"net/http"
	"strconv"
	"strings"
	"time"
)

// handleAccounts GET=账号列表+被动注水事件+改路由计数；POST=增删/启停/GLM 标记（action 字段分发）。
func (s *Server) handleAccounts(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		s.pool.EnsureLocalAccount()
		writeJSON(w, map[string]any{
			"ok":             true,
			"accounts":       s.accountList(),
			"mode":           map[bool]string{true: "pool", false: "single"}[s.pool.Size() > 0],
			"passive":        s.water.PassiveEvents(),
			"media_reroutes": s.mediaReroutes.Load(),
		})
		return
	}
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Action   string   `json:"action"` // add | remove | toggle | setglm | setmodels | auto
		UserID   string   `json:"user_id"`
		Token    string   `json:"token"`
		Username string   `json:"username"`
		OrgID    string   `json:"org_id"`
		Enabled  bool     `json:"enabled"`
		HasGLM53 bool     `json:"has_glm53"`
		Models   []string `json:"models"`
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
		if accountTokenInvalid(r.Context(), req.Token) {
			writeJSON(w, map[string]any{"ok": false, "msg": invalidTokenMsg})
			return
		}
		if s.pool.Add(req.UserID, req.Token, req.Username, req.OrgID) {
			writeJSON(w, map[string]any{"ok": true, "msg": "账号已添加"})
		} else {
			writeJSON(w, map[string]any{"ok": false, "msg": "账号已存在"})
		}
	case "remove":
		if req.UserID == localAccountSub() {
			writeJSON(w, map[string]any{"ok": false, "msg": "本地账号不可删除"})
			return
		}
		writeJSON(w, map[string]any{"ok": s.pool.Remove(req.UserID)})
	case "toggle":
		writeJSON(w, map[string]any{"ok": s.pool.Toggle(req.UserID, req.Enabled)})
	case "setglm":
		writeJSON(w, map[string]any{"ok": s.pool.SetGLM(req.UserID, req.HasGLM53)})
	case "setmodels":
		if req.UserID == "" {
			writeJSON(w, map[string]any{"ok": false, "msg": "缺少 user_id"})
			return
		}
		writeJSON(w, map[string]any{"ok": s.pool.SetModels(req.UserID, req.Models)})
	case "auto":
		s.handleAccountAuto(w, r)
		return
	default:
		writeJSON(w, map[string]any{"ok": false, "msg": "未知 action"})
	}
}

// handleAccountAuto 自动探测：连浏览器调试口读团结 cookie → 解析入池。
// 浏览器没开调试口时 spawn（带 --remote-debugging-port，专用 profile，
// 登录态保留），等就绪后重试。探测/入池全程只针对 codely.tuanjie.cn 域。
func (s *Server) handleAccountAuto(w http.ResponseWriter, r *http.Request) {
	// 1. 直接探测（浏览器可能已带调试口在跑）
	//    读到已在池的账号时不拦截——fall through 到弹窗流程，让用户可以登新号或查余额。
	//    token 无效（过期/被吊销）也不拦截——同样 fall through 到弹窗让用户重新登录。
	if creds := probeCDPBrowser(); creds != nil {
		if accountTokenInvalid(r.Context(), creds.AccessToken) {
			log.Printf("[tuanjie] 直接探测到 %s 但 token 无效，继续弹窗流程以便用户重新登录", creds.UserID)
		} else if s.pool.Add(creds.UserID, creds.AccessToken, creds.UserID, "") {
			writeJSON(w, map[string]any{"ok": true, "msg": "已从浏览器读取并添加账号", "user_id": creds.UserID, "browser": creds.Browser})
			return
		} else {
			log.Printf("[tuanjie] 直接探测到 %s 已在池里，继续弹窗流程以便用户登新号/查余额", creds.UserID)
		}
	}
	// 2. 探测不到 → 拉起【一次性独立 profile】浏览器带调试口（136+ 版本安全限制：
	//    默认 profile 忽略调试参数，必须独立 user-data-dir），打开团结 dashboard。
	//    每次探测用全新目录 + 独立端口，登录态不跨会话复用。
	cleanupStaleProbeProfiles()
	port := freeCDPPort()
	path, err := launchBrowserWithCDP(port, newProbeSessionDir())
	if err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "未探测到调试口，且启动浏览器失败: " + err.Error()})
		return
	}
	// 3. 长轮询等登录：一次性 profile 是全新环境，需要在弹出的窗口里登录一次；
	//    登录后 cookie 写入该 profile，读取入池。只盯本会话端口，避免读到其它窗口旧登录态。
	//    最长 150 秒，每 2 秒查一次。
	deadline := time.Now().Add(150 * time.Second)
	for time.Now().Before(deadline) {
		time.Sleep(2 * time.Second)
		if creds := probeCDPBrowserQuiet(port); creds != nil {
			if accountTokenInvalid(r.Context(), creds.AccessToken) {
				writeJSON(w, map[string]any{"ok": false, "msg": invalidTokenMsg})
				return
			}
			if s.pool.Add(creds.UserID, creds.AccessToken, creds.UserID, "") {
				log.Printf("[tuanjie] 自动探测成功：探测窗口可以关了（port=%s user_id=%s）", port, creds.UserID)
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
	log.Printf("[tuanjie] 自动探测超时：探测窗口可以关了（port=%s，150 秒未读到期）", port)
	writeJSON(w, map[string]any{"ok": false, "msg": "等待登录超时（150 秒）——请在弹出的浏览器窗口里登录团结账号后再点一次探测"})
}

// invalidTokenMsg 是入池前预验证失败（token 无效）的固定报错文案。
const invalidTokenMsg = "该账号凭据无效（换取 key 401）——请确认浏览器已登录 codely.tuanjie.cn 且会话未过期后重试"

// accountTokenInvalid 入池前预验证 access_token：能换取 cli_api_key 才算有效。
// 命中 401（token 无效）等错误返回 true，应拒绝入池。
func accountTokenInvalid(ctx context.Context, token string) bool {
	_, err := fetchKeyWithToken(ctx, token)
	return err != nil
}

// localAccountSub 返回本地登录态 JWT 的 sub（user_id）；未登录或解析失败返回空串。
func localAccountSub() string {
	token, err := loadAccessToken()
	if err != nil {
		return ""
	}
	return jwtSub(token)
}

// accountList 组装 GET /accounts 的账号数组：池内账号统一补 source 字段（local=本地客户端，
// pool=入池账号）。本地账号由 Source=="local" 识别（已纳入池），username 显示为本地客户端。
func (s *Server) accountList() []map[string]any {
	statuses := s.pool.Status()
	accts := make([]map[string]any, 0, len(statuses))
	for _, st := range statuses {
		source := st.Source
		if source == "" {
			source = "pool"
		}
		username := st.Username
		if source == "local" {
			username = "本地客户端"
		}
		accts = append(accts, map[string]any{
			"user_id":               st.UserID,
			"username":              username,
			"org_id":                st.OrgID,
			"enabled":               st.Enabled,
			"use_count":             st.UseCount,
			"last_used":             st.LastUsed,
			"token_expires":         st.TokenExpires,
			"token_remaining_hours": st.TokenRemainHrs,
			"has_glm53":             st.HasGLM53,
			"budget_exceeded":       st.BudgetExceeded,
			"inflight":              st.Inflight,
			"source":                source,
			"models":                func() []string { if st.Models == nil { return []string{} }; return st.Models }(),
		})
	}
	return accts
}

// accountTokenFor 返回当前请求应使用的 access_token（多账号池选号或单账号回退）。
// 多账号模式返回 (token, userID, true)；单账号返回 ("", "", false) 表示走 Client 原路径。
// token 经过 PickWithToken 解析：本地账号实时读 oauth_creds.json，池账号用快照。
func (s *Server) accountTokenFor(model string) (string, string, bool) {
	if s.pool.Size() == 0 {
		return "", "", false
	}
	acc, token := s.pool.PickWithToken(model)
	if acc == nil {
		return "", "", false
	}
	return token, acc.UserID, true
}

// ForwardDirect 用指定 access_token 直连转发（多账号池的 chat 转发用：
// key 换取按账号独立，不经 Client 单账号缓存）。sess 为本请求会话
// （请求体 litellm_session_id 与头同值，上游会话亲和路由靠它）。
func (s *Server) ForwardDirect(ctx context.Context, method, path string, body []byte, accessToken string, sess *LitellmSession) (*http.Response, error) {
	key, err := fetchKeyWithToken(ctx, accessToken)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, method, litellmAPIBase+path, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	for k, vs := range s.client.litellmHeaders(path, key, sess) {
		for _, v := range vs {
			req.Header.Add(k, v)
		}
	}
	return s.client.httpClient.Do(req)
}

// handleProviders 外部账号：GET=信息列表（计费缓存 120s）；POST=增删。
// 响应绝不包含 api_key。
func (s *Server) handleProviders(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		writeJSON(w, map[string]any{"ok": true, "providers": s.providers.Infos()})
		return
	}
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Action  string   `json:"action"` // add | remove | addmodel | removemodel
		Name    string   `json:"name"`
		BaseURL string   `json:"base_url"`
		APIKey  string   `json:"api_key"`
		Models  []string `json:"models"` // 参与转发的模型（可空=仅展示）
		Model   string   `json:"model"`  // addmodel/removemodel 的单个模型
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "请求体解析失败"})
		return
	}
	switch req.Action {
	case "add":
		if s.providers.Add(ExternalProvider{Name: req.Name, BaseURL: req.BaseURL, APIKey: req.APIKey, Models: req.Models}) {
			s.providers.Invalidate()
			writeJSON(w, map[string]any{"ok": true, "msg": "外部账号已添加"})
		} else {
			writeJSON(w, map[string]any{"ok": false, "msg": "添加失败（名称/base_url/key 不能为空，或名称已存在）"})
		}
	case "remove":
		if s.providers.Remove(req.Name) {
			s.providers.Invalidate()
			writeJSON(w, map[string]any{"ok": true, "msg": "外部账号已删除"})
		} else {
			writeJSON(w, map[string]any{"ok": false, "msg": "未找到该外部账号"})
		}
	case "addmodel":
		if s.providers.AddModel(req.Name, req.Model) {
			s.providers.Invalidate()
			writeJSON(w, map[string]any{"ok": true, "msg": "模型已添加"})
		} else {
			writeJSON(w, map[string]any{"ok": false, "msg": "添加失败（账号不存在或模型已存在/为空）"})
		}
	case "removemodel":
		if s.providers.RemoveModel(req.Name, req.Model) {
			s.providers.Invalidate()
			writeJSON(w, map[string]any{"ok": true, "msg": "模型已删除"})
		} else {
			writeJSON(w, map[string]any{"ok": false, "msg": "删除失败（账号或模型不存在）"})
		}
	default:
		writeJSON(w, map[string]any{"ok": false, "msg": "未知 action"})
	}
}

// handleVisionConfig 视觉模型配置（媒体改路由目标）：GET 返回 {"model"}；
// POST {"model":"..."} 更新并持久化。兼容层：内部转到新 media 机制
// （vision 值与 /media-config 双向同步，一个机制两个视图），行为不变。
func (s *Server) handleVisionConfig(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodPost {
		var req struct {
			Model string `json:"model"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil || req.Model == "" {
			writeErr(w, 400, "body 需为 {\"model\":\"...\"}")
			return
		}
		// 校验：识图模型必须是理解类（chat）——image/video 类模型走错端点，直接拒。
		// 前端下拉已过滤，这里拦手工 curl 的错配。
		if k := ModelKind(req.Model); k != "chat" {
			writeErr(w, 400, "识图模型需为理解类（chat）模型，"+req.Model+" 是 "+k+" 类，请走对应的选择器")
			return
		}
		SetVisionModel(req.Model)
		if err := SaveVisionConfig(); err != nil {
			writeErr(w, 500, "持久化失败: "+err.Error())
			return
		}
		log.Printf("[tuanjie] vision model 配置更新 model=%s", req.Model)
	}
	writeJSON(w, map[string]any{"model": VisionModel()})
}

// handleMediaConfig 三类媒体模型统一配置（识图/生图/生视频）：
// GET 返回 {"vision":"...","image":"","video":""}；
// POST 部分更新——传了的字段才更新（指针区分"没传"与"空串"），
// 空串 = 清空该选择器回到默认（vision 回 codely-vl，image/video 回不改写）。
func (s *Server) handleMediaConfig(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodPost {
		var req struct {
			Vision *string `json:"vision"`
			Image  *string `json:"image"`
			Video  *string `json:"video"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil ||
			(req.Vision == nil && req.Image == nil && req.Video == nil) {
			writeErr(w, 400, "body 需为 {\"vision\"|\"image\"|\"video\"}（可只传变化项）")
			return
		}
		// 类型校验：三个选择器各只收对应类模型（空串=清空回默认，跳过校验）。
		// 前端下拉已过滤，这里拦手工 curl 的错配（错类模型会转发到错误端点）。
		if req.Vision != nil && *req.Vision != "" && ModelKind(*req.Vision) != "chat" {
			writeErr(w, 400, "识图选择器需为理解类（chat）模型，"+*req.Vision+" 是 "+ModelKind(*req.Vision)+" 类")
			return
		}
		if req.Image != nil && *req.Image != "" && ModelKind(*req.Image) != "image" {
			writeErr(w, 400, "生图选择器需为生图类（image）模型，"+*req.Image+" 是 "+ModelKind(*req.Image)+" 类")
			return
		}
		if req.Video != nil && *req.Video != "" && ModelKind(*req.Video) != "video" {
			writeErr(w, 400, "生视频选择器需为视频类（video）模型，"+*req.Video+" 是 "+ModelKind(*req.Video)+" 类")
			return
		}
		if req.Vision != nil {
			SetVisionModel(*req.Vision) // 空串回默认 codely-vl
		}
		if req.Image != nil {
			SetImageModel(*req.Image)
		}
		if req.Video != nil {
			SetVideoModel(*req.Video)
		}
		if err := SaveMediaConfig(); err != nil {
			writeErr(w, 500, "持久化失败: "+err.Error())
			return
		}
		log.Printf("[tuanjie] media config 配置更新 vision=%s image=%s video=%s",
			VisionModel(), ImageModel(), VideoModel())
	}
	writeJSON(w, map[string]any{
		"vision":          VisionModel(),
		"image":           ImageModel(),
		"video":           VideoModel(),
		"vision_fallback": VisionFallbackChain()[1:], // 回落链去掉首个（=vision 本身）
	})
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

// waterHistoryEntry 一条注水检测历史（内存环形 20 条）。
type waterHistoryEntry struct {
	At      string  `json:"at"`
	Channel string  `json:"channel,omitempty"`
	Model   string  `json:"model"`
	Account string  `json:"account"`
	Light   string  `json:"verdict_light"`
	Score   float64 `json:"verdict_score"`
}

// pushWaterHistory 检测历史入列（环形 20 条，超限丢最旧）。
func (s *Server) pushWaterHistory(e waterHistoryEntry) {
	s.waterHistMu.Lock()
	defer s.waterHistMu.Unlock()
	if len(s.waterHist) >= 20 {
		s.waterHist = append(s.waterHist[1:], e)
	} else {
		s.waterHist = append(s.waterHist, e)
	}
}

// waterHistory 返回检测历史副本（旧→新）。
func (s *Server) waterHistory() []waterHistoryEntry {
	s.waterHistMu.Lock()
	defer s.waterHistMu.Unlock()
	out := make([]waterHistoryEntry, len(s.waterHist))
	copy(out, s.waterHist)
	return out
}

// handleWaterProbe 注水检测（action 字段分发）：
//   - GET ?history=1：返回检测历史（最近 20 条）
	//   - GET ?channels=1：返回六渠道列表（含各渠道 models，3s 缓存）
//   - check（前端唯一入口）：一键全流程——有基准直接比对出报告；
//     无基准自动采集官方基准后同渠道比对自检（报告标注首次检测）；
//     channel 缺省 tuanjie，非 tuanjie 渠道探针直打该渠道本地端点
//   - quick（默认，兼容旧调用）：现有金丝雀探针（漂移+答题）+ 第一层管道探针入结果
//   - deep：第一层 + 第二层分布采样 + 与基准库比对（综合灯色）
//   - baseline：跑第一层全部探针 + 第二层分布采样（N=60），落盘为该模型官方基准
func (s *Server) handleWaterProbe(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodGet {
		if r.URL.Query().Get("history") == "1" {
			writeJSON(w, map[string]any{"ok": true, "history": s.waterHistory()})
			return
		}
		if r.URL.Query().Get("channels") == "1" {
			s.handleChannels(w, r)
			return
		}
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Action       string `json:"action"` // check | quick | deep | baseline（缺省=quick，旧调用兼容）
		UserID       string `json:"user_id"`
		Model        string `json:"model"`
		Channel      string `json:"channel"`        // 渠道（缺省 tuanjie）
		ChannelKeyID string `json:"channel_key_id"` // setkey：渠道 id
		ChannelKey   string `json:"channel_key"`    // setkey：key 明文
		Samples      int    `json:"samples"`        // baseline/deep/check 的分布采样数（默认 60，上限 200）
	}
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "请求体解析失败"})
		return
	}
	if req.Model == "" {
		req.Model = "GLM-5.3" // 默认探测最贵的
	}
	if req.Samples <= 0 {
		req.Samples = 60
	}
	if req.Action == "check" {
		s.handleWaterCheck(w, r, req.Channel, req.UserID, req.Model, req.Samples)
		return
	}
	// deep/baseline：在单账号模式下直接跑
	if req.Action == "setkey" {
		if err := SaveChannelKey(req.ChannelKeyID, req.ChannelKey); err != nil {
			writeJSON(w, map[string]any{"ok": false, "msg": err.Error()})
			return
		}
		writeJSON(w, map[string]any{"ok": true, "msg": "渠道 key 已保存"})
		return
	}
	if req.Action == "deep" || req.Action == "baseline" {
		s.handleWaterDeepOrBaseline(w, r, req.Action, req.UserID, req.Model, req.Samples)
		return
	}
	// action=quick：单账号金丝雀探针 + 管道探针
	accessToken, atErr := loadAccessToken()
	if atErr != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "未找到团结登录态（请先登录团结 Cowork 桌面端）"})
		return
	}
	res, err := s.water.ProbeAccount(r.Context(), accessToken, "local", req.Model)
	if err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": err.Error()})
		return
	}
	// 管道探针（tokenizer×4/错误×2/finish×2，10 个小调用）
	key, kerr := fetchKeyWithToken(r.Context(), accessToken)
	var probes []probeResult
	if kerr == nil {
		probes = RunPipelineProbes(r.Context(), tuanjieTarget(key), req.Model)
	}
	writeJSON(w, map[string]any{"ok": true, "action": "quick", "results": []*WaterProbeResult{res}, "probes": probes})
}

// handleWaterCheck action=check：一键全流程出报告。
//   - 有基准：跑金丝雀 + 管道探针 + 分布采样（N=60）+ CompareToBaseline
//   - 无基准：先 CollectBaseline（无感自动采集）再同渠道跑一遍比对自检，
//     报告标注 first_time=true（本次为新模型首次检测，基准已自动采集）
//   - channel：缺省 tuanjie（走账号池 fetchKeyWithToken 换 key 直探上游）；
//     非 tuanjie 渠道探针直打该渠道本地端点（probeTarget），不走账号池。
//
// 响应：{ok, action:"check", report:{model,channel,account,at,verdict:{light,
// score,reason}, items:[{name,result,detail}...], first_time}, probes,
// probe_compare, dist, dist_similarity, canary}。
func (s *Server) handleWaterCheck(w http.ResponseWriter, r *http.Request, channel, userID, model string, samples int) {
	account := userID
	if account == "" {
		account = "38261" // 单账号模式账号标识（与既有官方基准采集账号一致）
	}
	if channel == "" {
		channel = "tuanjie"
	}

	// 构造探针 target：tuanjie 走账号池换 key；其余渠道走本地端点
	accessToken := ""
	var target *probeTarget
	if channel == "tuanjie" {
		var atErr error
		accessToken, atErr = loadAccessToken()
		if atErr != nil {
			writeJSON(w, map[string]any{"ok": false, "msg": "未找到团结登录态（请先登录团结 Cowork 桌面端）"})
			return
		}
		key, err := fetchKeyWithToken(r.Context(), accessToken)
		if err != nil {
			writeJSON(w, map[string]any{"ok": false, "msg": "换取 key 失败: " + err.Error()})
			return
		}
		target = tuanjieTarget(key)
	} else {
		t, err := s.channelTarget(channel)
		if err != nil {
			writeJSON(w, map[string]any{"ok": false, "msg": err.Error()})
			return
		}
		target = t
	}

	base := s.baselines.Get(channel, model)
	firstTime := base == nil

	var (
		probes  []probeResult
		dist    *distResult
		cmps    []probeCompare
		sim     distSimilarity
		verdict overallVerdict
	)
	if firstTime {
		// 无基准：先自动采集官方基准（无感模式），再同渠道比对自检
		bl, blErr := s.baselines.CollectBaseline(r.Context(), target, channel, model, account, samples)
		if blErr != nil {
			writeJSON(w, map[string]any{"ok": false, "msg": blErr.Error()})
			return
		}
		probes = RunPipelineProbes(r.Context(), target, model)
		dist = collectDistSamples(r.Context(), target, model, samples)
		cmps, sim, verdict = CompareToBaseline(bl, probes, dist)
		base = bl
		verdict.Light = "grey"
		verdict.Label = "首次"
		verdict.Reason = plainVerdictReason("grey", isStrongChannel(channel))
	} else {
		probes = RunPipelineProbes(r.Context(), target, model)
		dist = collectDistSamples(r.Context(), target, model, samples)
		cmps, sim, verdict = CompareToBaseline(base, probes, dist)
		verdict.Reason = plainVerdictReason(verdict.Light, isStrongChannel(channel))
	}

	// 上游 402 预算拦截检测：LiteLLM 团队预算受限（Max budget -1 异常）时
	// 所有探针都会带 Budget exceeded 错误——此时如实显示"账号被网关限制"，
	// 绝不能再判"疑似注水"（此前 38261 账号被 402 拦截时就误报过红灯）。
	// 多数探针（≥3/8）命中 402 预算错误即判网关限制——finish_* 等探针
	// 在 402 下只报"缺失"不会带 Budget 字样，全探针判定太严会漏判
	hitBudget := 0
	for _, p := range probes {
		txt := p.Value + p.Note
		if strings.Contains(txt, "Budget has been exceeded") || strings.Contains(txt, "`402`") || strings.Contains(txt, "402") {
			hitBudget++
		}
	}
	quotaBlocked := hitBudget >= 3
	if quotaBlocked {
		verdict.Light = "grey"
		verdict.Score = 0
		verdict.Reason = "账号在网关侧被预算限制（402 Budget exceeded）——非注水信号，请到团结侧恢复或更换账号"
	}

	// 金丝雀答题（能力项；失败不阻断报告，item 标 ⚠）
	var canary *WaterProbeResult
	if channel == "tuanjie" {
		canary, _ = s.water.ProbeAccount(r.Context(), accessToken, "local", model)
	} else {
		canary, _ = s.water.ProbeAccountTarget(r.Context(), target, "local", model)
	}
	if canary == nil {
		log.Printf("[tuanjie] check canary 失败 channel=%s model=%s", channel, model)
	}

	// 指纹误报降级（2026-08-30 codely-basic 案例）：上游多变体随机路由/模板
	// 按天平移会让 tokenizer 指纹整批漂移，但分词器本身没换（当时原始 token
	// 逐位一致已证）——仅 tokenizer 系探针不匹配、分布形状仍高度相似、金丝雀
	// 全对时，判"疑似模板漂移"（黄灯复测）而非"注水嫌疑"（红灯）。
	if verdict.Light == "red" && canary != nil && canary.Pass && sim.Cosine >= 0.9 {
		tokMis, otherMis := 0, 0
		for _, c := range cmps {
			if c.Status == "ok" && !c.Match {
				if strings.HasPrefix(c.Name, "tokenizer_") {
					tokMis++
				} else {
					otherMis++
				}
			}
		}
		if tokMis > 0 && otherMis == 0 {
			verdict.Light, verdict.Label = "yellow", "疑似模板漂移"
			verdict.Reason = "仅 " + itoa(tokMis) + " 项 tokenizer 指纹漂移，但分布形状相似度 " +
				formatPct(sim.Cosine*100) + "、金丝雀全对——更像上游模板变动，建议复测而非判注水"
		}
	}

	// 金丝雀错答回灌（第 32 轮）：探针/分布判 green 但金丝雀答错 ≥2 时降级
	// （错答 ≥2 降 yellow、=3 降 red），只在 green 时降——本就 red/yellow 不动。
	verdict = applyCanaryFeedback(verdict, channel, canary)

	at := time.Now().Format("2006-01-02 15:04:05")
	report := map[string]any{
		"model":      model,
		"channel":    channel,
		"account":    account,
		"at":         at,
		"verdict":    map[string]any{"light": verdict.Light, "score": verdict.Score, "reason": verdict.Reason},
		"items":      buildWaterReportItems(channel, canary, cmps, sim, base, firstTime),
		"first_time": firstTime,
	}
	s.pushWaterHistory(waterHistoryEntry{At: at, Channel: channel, Model: model, Account: account, Light: verdict.Light, Score: verdict.Score})
	writeJSON(w, map[string]any{
		"ok": true, "action": "check", "report": report,
		"probes": probes, "probe_compare": cmps,
		"dist": dist, "dist_similarity": sim, "canary": canary,
	})
}

// plainVerdictReason 综合灯 → 一句话人话结论（专业词只进折叠详情）。
// 基准口径按渠道 strong 标记诚实化（第 32 轮）：strong 渠道=「官方链路基准」
// （凭证链完整、指纹源自厂商），弱渠道=「首测锚定（无官方链路，弱判）」。
func plainVerdictReason(light string, strong bool) string {
	term := "官方链路基准"
	if !strong {
		term = "首测锚定（无官方链路，弱判）"
	}
	switch light {
	case "green":
		return "模型一致，未发现注水"
	case "yellow":
		return "有轻微偏差，建议复测"
	case "red":
		return "指纹与" + term + "不符，疑似注水"
	default:
		return "已自动采集" + term + "并完成首次检测"
	}
}

// applyCanaryFeedback 金丝雀错答回灌（纯函数，可单测；第 32 轮裁决）：
// 探针/分布判 green 但金丝雀答错 ≥2 的，此前直接绿灯放行（回执绿但
// 「能力答题 ✖」证据矛盾的根因）。错答 ≥2 降 yellow、=3 降 red，reason
// 补错答说明；只在 green 时降级（本就 red/yellow 不动——取更严者）。
// 错答数只统计实际作答的题（tuanjie 走 ProbeAccount 不记 repeat 题的作答，
// 缺题不算错答），文案分母 3 = 金丝雀题库总题数。
func applyCanaryFeedback(v overallVerdict, channel string, canary *WaterProbeResult) overallVerdict {
	if v.Light != "green" || canary == nil {
		return v
	}
	wrong := 0
	for _, q := range canaryQuestions {
		if ok, answered := canary.Answers[q.ID]; answered && !ok {
			wrong++
		}
	}
	if wrong < 2 {
		return v
	}
	if wrong >= 3 {
		v.Light, v.Label = "red", "显著偏差"
		v.Reason = plainVerdictReason("red", isStrongChannel(channel)) + "（金丝雀全错）"
		return v
	}
	v.Light, v.Label = "yellow", "轻微偏差"
	v.Reason = plainVerdictReason("yellow", isStrongChannel(channel)) + "（金丝雀错答 " + itoa(wrong) + "/3）"
	return v
}

// waterReportItem 一个人话检测项（前端直接渲染）。
type waterReportItem struct {
	Name   string `json:"name"`
	Result string `json:"result"` // ✔ | ✖ | ⚠ | — | 🆕
	Detail string `json:"detail"`
}

// buildWaterReportItems 组装四个人话检测项（纯函数，可单测）：
// ①身份指纹（管道探针比对）②权重指纹（分布相似度+众数）③能力答题（金丝雀）
// ④基准状态（有基准=采样时间+账号；无=首次自动采集说明）。文案带渠道人话名。
func buildWaterReportItems(channel string, canary *WaterProbeResult, cmps []probeCompare, sim distSimilarity, base *Baseline, firstTime bool) []waterReportItem {
	items := make([]waterReportItem, 0, 4)
	chName := channelNameOf(channel) + "渠道"

	// ①身份指纹：tokenizer 4 值 + 错误文本 + finish_reason 与基准比对
	comparable, mismatch := 0, 0
	for _, c := range cmps {
		if c.Status == "ok" {
			comparable++
			if !c.Match {
				mismatch++
			}
		}
	}
	switch {
	case comparable == 0:
		items = append(items, waterReportItem{Name: "身份指纹", Result: "—", Detail: "探针值缺失（unstable/error 过多），无法与" + chName + "官方基准比对"})
	case mismatch == 0:
		items = append(items, waterReportItem{Name: "身份指纹", Result: "✔", Detail: "管道指纹（分词器 / 报错原文 / 完停词）与" + chName + "官方基准全部一致"})
	case mismatch == 1:
		items = append(items, waterReportItem{Name: "身份指纹", Result: "⚠", Detail: "1 项管道指纹偏离" + chName + "官方基准，其余一致"})
	default:
		items = append(items, waterReportItem{Name: "身份指纹", Result: "✖", Detail: strconv.Itoa(mismatch) + " 项管道指纹偏离" + chName + "官方基准（疑似换模型）"})
	}

	// ②权重指纹：分布相似度百分比 + 众数比对（双方样本不足则不可比）
	distReady := base != nil && base.Dist != nil && !base.Dist.Insufficient && sim.Cosine > 0
	if !distReady {
		items = append(items, waterReportItem{Name: "权重指纹", Result: "—", Detail: "分布有效样本不足 40，无法统计比对"})
	} else {
		pct := math.Round(sim.Overall * 100)
		switch {
		case pct >= 96:
			items = append(items, waterReportItem{Name: "权重指纹", Result: "✔",
				Detail: fmt.Sprintf("分布相似度 %d%%（众数 %d vs 基准 %d），与官方基准一致", int(pct), sim.ModeA, sim.ModeB)})
		case pct >= 90:
			items = append(items, waterReportItem{Name: "权重指纹", Result: "⚠",
				Detail: fmt.Sprintf("分布相似度 %d%%（众数 %d vs 基准 %d），有轻微偏差", int(pct), sim.ModeA, sim.ModeB)})
		default:
			items = append(items, waterReportItem{Name: "权重指纹", Result: "✖",
				Detail: fmt.Sprintf("分布相似度 %d%%（众数 %d vs 基准 %d），显著偏离官方基准", int(pct), sim.ModeA, sim.ModeB)})
		}
	}

	// ③能力答题：金丝雀答题对错（repeat 题只采 tokenizer 指纹无 answer，
	// 已并入①身份指纹，这里只数有答案的算术/常识两题）
	if canary == nil {
		items = append(items, waterReportItem{Name: "能力答题", Result: "⚠", Detail: "金丝雀答题未完成（网络或上游异常），未纳入本次判定"})
	} else {
		var wrong []string
		for _, q := range canaryQuestions[1:] {
			if !canary.Answers[q.ID] {
				wrong = append(wrong, q.Title)
			}
		}
		if len(wrong) == 0 {
			items = append(items, waterReportItem{Name: "能力答题", Result: "✔", Detail: "金丝雀答题（算术 / 常识）全部答对"})
		} else {
			items = append(items, waterReportItem{Name: "能力答题", Result: "✖",
				Detail: "金丝雀答题有错：「" + strings.Join(wrong, "」「") + "」与预期不符"})
		}
	}

	// ④基准状态：有基准=采样时间+账号；无=首次自动采集说明
	switch {
	case firstTime:
		items = append(items, waterReportItem{Name: "基准状态", Result: "🆕", Detail: "新模型首次检测：官方基准已自动采集（本次即基准）"})
	case base != nil:
		items = append(items, waterReportItem{Name: "基准状态", Result: "✔",
			Detail: "官方基准采样于 " + base.SampledAt + "（账号 " + base.Account + "）"})
	default:
		items = append(items, waterReportItem{Name: "基准状态", Result: "—", Detail: "无官方基准"})
	}
	return items
}

// handleWaterDeepOrBaseline deep/baseline 的公共骨架：换 key → 跑第一层
// （+ deep 时第二层采样+比对 / baseline 时采样落盘）。
func (s *Server) handleWaterDeepOrBaseline(w http.ResponseWriter, r *http.Request, action, userID, model string, samples int) {
	accessToken, atErr := loadAccessToken()
	if atErr != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "未找到团结登录态: " + atErr.Error()})
		return
	}
	key, err := fetchKeyWithToken(r.Context(), accessToken)
	if err != nil {
		writeJSON(w, map[string]any{"ok": false, "msg": "换取 key 失败: " + err.Error()})
		return
	}

	tgt := tuanjieTarget(key)
	if action == "baseline" {
		bl, err := s.baselines.CollectBaseline(r.Context(), tgt, "tuanjie", model, userID, samples)
		if err != nil {
			writeJSON(w, map[string]any{"ok": false, "msg": err.Error()})
			return
		}
		writeJSON(w, map[string]any{
			"ok": true, "action": "baseline", "model": model,
			"baseline": bl,
			"msg":      "基准已采集落盘（tuanjie-baselines.json）",
		})
		return
	}

	// action=deep：第一层 + 第二层 + 基准比对
	probes := RunPipelineProbes(r.Context(), tgt, model)
	dist := collectDistSamples(r.Context(), tgt, model, samples)
	base := s.baselines.Get("tuanjie", model)
	cmps, sim, verdict := CompareToBaseline(base, probes, dist)
	writeJSON(w, map[string]any{
		"ok": true, "action": "deep", "model": model, "user_id": userID,
		"probes": probes, "probe_compare": cmps,
		"dist": dist, "dist_similarity": sim,
		"verdict":      verdict,
		"has_baseline": base != nil,
	})
}
