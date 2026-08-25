// handlers_extra.go —— 进行中请求 / 注水探针 / 媒体配置的 HTTP 端点。
// 均带 CORS（GUI 跨域拉取）。
package tuanjie

import (
	"encoding/json"
	"log"
	"net/http"
	"strconv"
)

// handleAccounts GET=单账号状态+被动注水事件+改路由计数。
func (s *Server) handleAccounts(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, map[string]any{
		"ok":             true,
		"mode":           "single",
		"passive":        s.water.PassiveEvents(),
		"media_reroutes": s.mediaReroutes.Load(),
	})
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

// handleWaterProbe 注水检测三 action（action 字段分发）：
//   - quick（默认，兼容旧调用）：现有金丝雀探针（漂移+答题）+ 第一层管道探针入结果
//   - deep：第一层 + 第二层分布采样 + 与基准库比对（综合灯色）
//   - baseline：跑第一层全部探针 + 第二层分布采样（N=60），落盘为该模型官方基准
func (s *Server) handleWaterProbe(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	var req struct {
		Action  string `json:"action"` // quick | deep | baseline（缺省=quick，旧调用兼容）
		UserID  string `json:"user_id"`
		Model   string `json:"model"`
		Samples int    `json:"samples"` // baseline/deep 的分布采样数（默认 60，上限 200）
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
	// deep/baseline：在单账号模式下直接跑
	if req.Action == "deep" || req.Action == "baseline" {
		s.handleWaterDeepOrBaseline(w, r, req.Action, "", req.Model, req.Samples)
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
		probes = RunPipelineProbes(r.Context(), key, req.Model)
	}
	writeJSON(w, map[string]any{"ok": true, "action": "quick", "results": []*WaterProbeResult{res}, "probes": probes})
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

	if action == "baseline" {
		bl, err := s.baselines.CollectBaseline(r.Context(), key, model, userID, samples)
		if err != nil {
			writeJSON(w, map[string]any{"ok": false, "msg": err.Error()})
			return
		}
		writeJSON(w, map[string]any{
			"ok": true, "action": "baseline", "model": model,
			"baseline": bl,
			"msg": "基准已采集落盘（tuanjie-baselines.json）",
		})
		return
	}

	// action=deep：第一层 + 第二层 + 基准比对
	probes := RunPipelineProbes(r.Context(), key, model)
	dist := collectDistSamples(r.Context(), key, model, samples)
	base := s.baselines.Get(model)
	cmps, sim, verdict := CompareToBaseline(base, probes, dist)
	writeJSON(w, map[string]any{
		"ok": true, "action": "deep", "model": model, "user_id": userID,
		"probes": probes, "probe_compare": cmps,
		"dist": dist, "dist_similarity": sim,
		"verdict": verdict,
		"has_baseline": base != nil,
	})
}
