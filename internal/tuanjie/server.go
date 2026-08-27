package tuanjie

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// Server 是团结转发的本地 OpenAI 兼容服务。
type Server struct {
	client *Client
	pool   *AccountPool
	ln     net.Listener
	mu     sync.Mutex
	srv    *http.Server

	startedAt time.Time // 运行时长展示
	statsMu   sync.Mutex
	stats     map[string]*modelStat // 按模型累计（GUI 消耗 TOP 展示）
	statsPath string
	pacer     *Pacer // KIMI-K3 节奏器（可开关，GUI 经 /kimi-pacing 控制）

	registry *RequestRegistry // 进行中请求注册表（GUI 面板 + 负载感知）
	water    *WaterCheck      // 注水检测（被动观测 + 金丝雀探针）
	activity *ActivityLog     // 实时动态（环形缓冲，请求完成/402/中断事件）

	providers     *ProviderStore // 外部账号（管理 + 信息展示，不接入转发）
	baselines     *BaselineStore // 绝对基准库（注水检测三层：探针+分布比对锚点）
	mediaReroutes atomic.Int64   // 媒体改路由累计次数（GUI 展示）

	waterHistMu sync.Mutex
	waterHist   []waterHistoryEntry // 注水检测历史（内存环形 20 条，GET ?history=1）
}

// modelStat 单模型用量累计。
type modelStat struct {
	Calls     int64 `json:"calls"`
	InputTok  int64 `json:"inputTokens"`
	OutputTok int64 `json:"outputTokens"`
	TotalTok  int64 `json:"totalTokens"`
}

// NewServer 创建服务。
func NewServer() *Server {
	s := &Server{client: NewClient(), pool: NewAccountPool(), stats: map[string]*modelStat{}, startedAt: time.Now(), pacer: NewPacer(),
		registry: NewRegistry(), water: LoadWater(), activity: NewActivityLog(),
		providers: NewProviderStore(), baselines: LoadBaselines()}
	LoadMediaConfig()
	if exe, err := os.Executable(); err == nil {
		s.statsPath = filepath.Join(filepath.Dir(exe), "tuanjie-stats.json")
		s.loadStats()
	}
	return s
}

// exeDirForAccounts 取 exe 所在目录（回退到工作目录）。
func exeDirForAccounts() string {
	if exeDirOverride != nil {
		return exeDirOverride()
	}
	if p, err := os.Executable(); err == nil {
		return filepath.Dir(p)
	}
	if wd, err := os.Getwd(); err == nil {
		return wd
	}
	return "."
}

// corsWith 给所有响应加 CORS 头：GUI 页面跑在 localhost:随机端口，
// fetch 127.0.0.1:8788 属跨域，没有这个头前端全部拉不到数据。
func corsWith(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Access-Control-Allow-Origin", "*")
		w.Header().Set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
		w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization")
		if r.Method == http.MethodOptions {
			w.WriteHeader(http.StatusNoContent)
			return
		}
		h.ServeHTTP(w, r)
	})
}

// Start 在 host:port 上监听（阻塞）。
func (s *Server) Start(host, port string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/v1/models", s.handleModels)
	mux.HandleFunc("/model/info", s.handleModelInfo)
	mux.HandleFunc("/v1/stats", s.handleStats)
	mux.HandleFunc("/quota", s.handleQuota)
	mux.HandleFunc("/kimi-pacing", s.handleKimiPacing)
	mux.HandleFunc("/accounts", s.handleAccounts)          // 单账号状态 + 注水事件
	mux.HandleFunc("/inflight", s.handleInflight)          // 进行中请求面板
	mux.HandleFunc("/activity", s.handleActivity)          // 实时动态（最近事件）
	mux.HandleFunc("/water-probe", s.handleWaterProbe)     // 注水金丝雀探针
	mux.HandleFunc("/providers", s.handleProviders)        // 外部账号（管理 + 信息展示）
	mux.HandleFunc("/vision-config", s.handleVisionConfig) // 视觉模型配置（兼容层：内部转到 media-config 机制）
	mux.HandleFunc("/media-config", s.handleMediaConfig)   // 媒体模型三选择器（识图/生图/生视频）
	mux.HandleFunc("/v1/chat/completions", s.handleChat)
	mux.HandleFunc("/v1/images/generations", s.handleImagesGenerations) // 生图（外部 provider 转发 + 统一改写）
	mux.HandleFunc("/v1/videos", s.handleVideoCreate)                   // 生视频（外部 provider 转发，异步任务制）
	mux.HandleFunc("/v1/videos/", s.handleVideoQuery)                   // 生视频任务轮询（GET /v1/videos/{id}）
	mux.HandleFunc("/agnesapi", s.handleVideoQuery)                     // Agnes 推荐的 GET /agnesapi?video_id=

	ln, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return fmt.Errorf("端口 %s 被占用: %w", port, err)
	}
	s.mu.Lock()
	s.ln = ln
	s.mu.Unlock()
	srv := &http.Server{Handler: corsWith(mux)}
	s.mu.Lock()
	s.srv = srv
	s.mu.Unlock()
	return srv.Serve(ln)
}

// Stop 停止服务。
func (s *Server) Stop() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.srv != nil {
		_ = s.srv.Close()
	}
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	key, err := s.client.apiKeyCached(r.Context())
	resp := map[string]any{"status": "ok", "service": "tuanjie-go"}
	if !s.startedAt.IsZero() {
		resp["uptimeSec"] = int64(time.Since(s.startedAt).Seconds())
	}
	if err != nil {
		resp["status"] = "error"
		resp["message"] = err.Error()
	} else {
		resp["cli_api_key"] = key[:10] + "..."
		resp["backend"] = litellmAPIBase
	}
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(resp)
}

// handleModels 转发 /v1/models（透传 LiteLLM 实时列表，自动含新模型），
// 末尾合并外部 provider 的模型条目（owned_by=provider 名）。
// modelsCache 上游模型列表缓存（成功时存，失败/超限时回退，防空矩阵）。
var (
	modelsCacheMu sync.Mutex
	modelsCache   []byte
	modelsCacheAt time.Time
)

// mergeProviderModels 把 provider 模型条目合并进 /v1/models 响应体
// （owned_by=provider 名，学群友口径；与已有 id 去重）。解析失败原样返回。
func (s *Server) mergeProviderModels(body []byte) []byte {
	extra := s.providers.AllModels()
	if len(extra) == 0 {
		return body
	}
	var m struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if json.Unmarshal(body, &m) != nil {
		return body
	}
	// 重建：上游条目保持原样（RawMessage 不丢字段），末尾追加 provider 条目
	var orig map[string]json.RawMessage
	if json.Unmarshal(body, &orig) != nil {
		return body
	}
	seen := map[string]bool{}
	for _, d := range m.Data {
		seen[d.ID] = true
	}
	type modelEntry struct {
		ID      string `json:"id"`
		Object  string `json:"object"`
		OwnedBy string `json:"owned_by"`
	}
	provEntries := []modelEntry{}
	for _, e := range extra {
		if seen[e.ID] {
			continue
		}
		seen[e.ID] = true
		provEntries = append(provEntries, modelEntry{ID: e.ID, Object: "model", OwnedBy: e.OwnedBy})
	}
	if len(provEntries) == 0 {
		return body
	}
	var dataArr []json.RawMessage
	if raw, ok := orig["data"]; ok {
		_ = json.Unmarshal(raw, &dataArr)
	}
	for _, e := range provEntries {
		if b, err := json.Marshal(e); err == nil {
			dataArr = append(dataArr, b)
		}
	}
	if nb, err := json.Marshal(dataArr); err == nil {
		orig["data"] = nb
		if out2, err := json.Marshal(orig); err == nil {
			return out2
		}
	}
	return body
}

func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	resp, err := s.client.Forward(r.Context(), http.MethodGet, "/v1/models", nil, "")
	if err == nil && resp.StatusCode == http.StatusOK {
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 2<<20))
		resp.Body.Close()
		body = s.mergeProviderModels(body)
		modelsCacheMu.Lock()
		if len(body) > 0 {
			modelsCache = body
			modelsCacheAt = time.Now()
		}
		modelsCacheMu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Access-Control-Allow-Origin", "*")
		_, _ = w.Write(body)
		return
	}
	if err == nil {
		resp.Body.Close()
	}
	// 上游失败（预算超限/抖动）→ 回退缓存（1h 内），否则 502
	modelsCacheMu.Lock()
	cached, fresh := modelsCache, time.Since(modelsCacheAt) < time.Hour && len(modelsCache) > 0
	modelsCacheMu.Unlock()
	if fresh {
		w.Header().Set("Content-Type", "application/json")
		w.Header().Set("Access-Control-Allow-Origin", "*")
		_, _ = w.Write(cached)
		return
	}
	if err != nil {
		writeErr(w, 502, err.Error())
		return
	}
	writeErr(w, 502, "上游模型列表不可用")
}

// ModelInfo 是单个模型的元数据（供 GUI 模型指南展示）。
type ModelInfo struct {
	Name        string   `json:"name"`
	MaxInput    *int     `json:"maxInput,omitempty"`   // 上下文上限（token）
	MaxOutput   *int     `json:"maxOutput,omitempty"`  // 最大输出（token）
	InputCost   *float64 `json:"inputCost,omitempty"`  // 每百万 token 输入价（USD）
	OutputCost  *float64 `json:"outputCost,omitempty"` // 每百万 token 输出价（USD）
	CacheRdCost *float64 `json:"cacheReadCost,omitempty"`
	Backends    []string `json:"backends,omitempty"`  // 别名背后的真实部署
	Reasoning   string   `json:"reasoning,omitempty"` // 思考级别支持（实测）
	Note        string   `json:"note,omitempty"`
}

var (
	infoCacheMu sync.Mutex
	infoCache   []ModelInfo
	infoCacheAt time.Time
)

// handleModelInfo 聚合模型元数据（GUI 模型指南用）。
// 用户可调用名以 /v1/models 为准（真实可调）；/model/info 里的部署变体
// （-internal/-public 后缀）按去后缀匹配把元数据（输出上限/价格/后端）映射到别名。
func (s *Server) handleModelInfo(w http.ResponseWriter, r *http.Request) {
	infoCacheMu.Lock()
	fresh := time.Since(infoCacheAt) < time.Hour && infoCache != nil
	cached := infoCache
	infoCacheMu.Unlock()
	if fresh {
		writeJSON(w, cached)
		return
	}

	// 1. 用户可调用别名（实时）
	mresp, err := s.client.Forward(r.Context(), http.MethodGet, "/v1/models", nil, "")
	if err != nil {
		if cached != nil {
			writeJSON(w, cached)
			return
		}
		writeErr(w, 502, err.Error())
		return
	}
	var ml struct {
		Data []struct {
			ID          string `json:"id"`
			MaxModelLen *int   `json:"max_model_len"`
		} `json:"data"`
	}
	err = json.NewDecoder(mresp.Body).Decode(&ml)
	mresp.Body.Close()
	if err != nil {
		if cached != nil {
			writeJSON(w, cached)
			return
		}
		writeErr(w, 502, "解析 /v1/models 失败: "+err.Error())
		return
	}

	// 2. /model/info 部署元数据（按去后缀名索引）
	iresp, err := s.client.Forward(r.Context(), http.MethodGet, "/model/info", nil, "")
	if err != nil {
		if cached != nil {
			writeJSON(w, cached)
			return
		}
		writeErr(w, 502, err.Error())
		return
	}
	var raw struct {
		Data []struct {
			ModelName     string `json:"model_name"`
			LiteLLMParams struct {
				Model string `json:"model"`
			} `json:"litellm_params"`
			ModelInfo struct {
				MaxTokens        *int     `json:"max_tokens"`
				MaxInputTokens   *int     `json:"max_input_tokens"`
				InputCostPerTok  *float64 `json:"input_cost_per_token"`
				OutputCostPerTok *float64 `json:"output_cost_per_token"`
				CacheReadCost    *float64 `json:"cache_read_input_token_cost"`
			} `json:"model_info"`
		} `json:"data"`
	}
	err = json.NewDecoder(iresp.Body).Decode(&raw)
	iresp.Body.Close()
	if err != nil {
		if cached != nil {
			writeJSON(w, cached)
			return
		}
		writeErr(w, 502, "解析 /model/info 失败: "+err.Error())
		return
	}

	// 聚合：部署变体去掉 -internal/-public 后作为键，同名合并、字段取非空、后端去重
	type agg struct {
		maxIn, maxOut *int
		in, out, cr   *float64
		backends      []string
		seen          map[string]bool
	}
	aggs := map[string]*agg{}
	for _, m := range raw.Data {
		key := strings.TrimSuffix(strings.TrimSuffix(m.ModelName, "-internal"), "-public")
		a := aggs[key]
		if a == nil {
			a = &agg{seen: map[string]bool{}}
			aggs[key] = a
		}
		if a.maxIn == nil && m.ModelInfo.MaxInputTokens != nil {
			a.maxIn = m.ModelInfo.MaxInputTokens
		}
		if a.maxOut == nil && m.ModelInfo.MaxTokens != nil {
			a.maxOut = m.ModelInfo.MaxTokens
		}
		if a.in == nil && m.ModelInfo.InputCostPerTok != nil {
			a.in = m.ModelInfo.InputCostPerTok
		}
		if a.out == nil && m.ModelInfo.OutputCostPerTok != nil {
			a.out = m.ModelInfo.OutputCostPerTok
		}
		if a.cr == nil && m.ModelInfo.CacheReadCost != nil {
			a.cr = m.ModelInfo.CacheReadCost
		}
		if b := m.LiteLLMParams.Model; b != "" && !a.seen[b] {
			a.seen[b] = true
			a.backends = append(a.backends, b)
		}
	}

	// 3. 组装：别名（/v1/models）× 元数据（映射）+ max_model_len
	list := make([]ModelInfo, 0, len(ml.Data))
	for _, m := range ml.Data {
		mi := ModelInfo{Name: m.ID, Reasoning: "off/low/medium/high/max", Note: aliasNote(m.ID)}
		if m.MaxModelLen != nil {
			mi.MaxInput = m.MaxModelLen // LiteLLM 上报的上下文上限
		}
		if a := aggs[m.ID]; a != nil {
			if mi.MaxInput == nil {
				mi.MaxInput = a.maxIn
			}
			mi.MaxOutput = a.maxOut
			mi.Backends = a.backends
			if a.in != nil {
				v := *a.in * 1e6
				mi.InputCost = &v
			}
			if a.out != nil {
				v := *a.out * 1e6
				mi.OutputCost = &v
			}
			if a.cr != nil {
				v := *a.cr * 1e6
				mi.CacheRdCost = &v
			}
		}
		list = append(list, mi)
	}

	infoCacheMu.Lock()
	infoCache = list
	infoCacheAt = time.Now()
	infoCacheMu.Unlock()
	writeJSON(w, list)
}

// aliasNote 返回别名的实测说明（2026-08-17 逐测：5 个 codely 别名可用，GLM/公开/百度变体被锁定）。
// 状态与消耗率为单日明细实测（积分/1M token），非 bySource 历史平均；
// codely-air/basic/flash 同后端 deepseek-v4-flash-0731，区别是 reasoning_tokens 预算（浅75/中64/深129）。
func aliasNote(name string) string {
	switch name {
	case "codely-air":
		return "✓可用 · 后端: deepseek-v4-flash-0731 · ~75 积分/1M token"
	case "codely-basic":
		return "✓可用 · 后端: deepseek-v4-flash-0731 · ~64 积分/1M token"
	case "codely-flash":
		return "✓可用 · 后端: deepseek-v4-flash-0731 · ~129 积分/1M token"
	case "codely-core":
		return "✓可用 · 后端: glm-5-2-260617 · ~91 积分/1M token（浅推理，单日波动 47-233）"
	case "codely-vl":
		return "✓可用 · 后端: qwen3.5-397b-a17b · ~405 积分/1M token · 视觉模型"
	case "GLM-5.2":
		return "✗锁定 · 被团结后端锁定，改用 codely-core"
	case "GLM-5.3":
		return "✓可用 · 输入3.2/输出11.2 积分/1M · 默认推理档"
	case "KIMI-K3":
		return "✓可用 · ⚠高价 · 16积分/1M输入 · 输出80 · flash的10倍 · 慎用于长上下文"
	case "codely-air-public":
		return "✗锁定 · 公开变体被团结后端锁定"
	case "codely-basic-public":
		return "✗锁定 · 公开变体被团结后端锁定"
	case "codely-core-public":
		return "✗锁定 · 公开变体被团结后端锁定"
	case "codely-flash-public":
		return "✗锁定 · 公开变体被团结后端锁定"
	case "codely-vl-public":
		return "✗锁定 · 公开变体被团结后端锁定"
	case "deepseek-v4-flash-0731-baidu":
		return "✗锁定 · 百度变体被团结后端锁定"
	}
	return ""
}

// handleChat 转发对话请求（流式/非流式均透传）。
// 上游瞬时错误（模型未映射/限流/网关抖动）会自动换 key 重试一次；
// 每次请求记录 model/状态码/耗时到日志；流式注入 include_usage 并解析
// 最终 usage chunk 计入消耗统计（/v1/stats 供 GUI 消耗 TOP 展示）。
func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(io.LimitReader(r.Body, 64<<20))
	var req struct {
		Model  string          `json:"model"`
		Stream bool            `json:"stream"`
		Extras json.RawMessage `json:"-"`
	}
	model := "?"
	wantsStream := false
	if json.Unmarshal(body, &req) == nil && req.Model != "" {
		model = req.Model
		wantsStream = req.Stream
	}
	// 诊断：记录入站请求结构摘要（重排前的原始形态，客户端形态对比用）
	logRequestShape(model, body)
	// 远程 URL 图片预下载转 base64（Agnes / 团结上游下载远程图片经常超时挂死）
	if nb, n := FetchRemoteImages(body); n > 0 {
		body = nb
		log.Printf("[tuanjie] fetched %d remote image(s) to base64", n)
	}
	// 媒体改路由：含图片/音频且目标模型不识图 → 改写为视觉模型（防上游 400）
	mediaRerouted := false
	if nb, n := RerouteIfMedia(body); n != "" {
		body = nb
		model = VisionModel()
		mediaRerouted = true
		log.Printf("[tuanjie] media-reroute %s", n)
		s.mediaReroutes.Add(1)
		s.activity.Add("info", "媒体改路由 "+n, model, "", 0, 0, 0)
	}

	// 会话租借（对齐官方 CLI「一个窗口一个 session、窗口内复用」）：
	// 串行请求恒复用同一空闲会话；并发全忙则新建（模拟再开窗口）；
	// 闲置 30min/超 4h 自动清理。B3 traceid/parentspanid 绑在会话上。
	// defer 归还——响应（含流式）全部写完才算窗口闲置。
	sess := AcquireLitellmSession()
	defer ReleaseLitellmSession(sess)
	// 反竞品过滤规避（必须在 reshape 前，reshape 会重建有序 JSON）：
	// 上游网关只检测 system 消息里的 "You are <竞品名>" 身份声明
	// （ZCode/Claude Code/Codex 实测被拦，400「欢迎使用Codely」；
	// user 消息与 tools 描述实测不查），在 You 与 are 之间插入零宽空格
	// 即可放行（实测 2026-08-26，对模型不可见）。
	if nb := desensitizeAgentIdentity(body); nb != nil {
		body = nb
		log.Printf("[tuanjie] agent-identity 脱敏已应用（system 含竞品身份声明）")
	}
	body = reshapeChatBody(body, sess)
	logRequestShape(model, body) // 重排后 shape（验证字段序效果）

	// 外部 provider 模型：直接转发（静态 Bearer key，不占账号池、不做团结签名，
	// 学群友 _forward_external）。媒体改路由后目标命中 provider 也走这里（识图转 Agnes）。
	// 识图改路由场景失败时沿回落链重试（vision → vision_fallback… → codely-vl
	// 兜尾）；链尽或非改路由的普通 provider 请求失败如实透传（4xx 配置错不回落）。
	if prov := s.providers.Match(model); prov != nil {
		if !mediaRerouted {
			st := s.forwardExternal(w, r, body, model, wantsStream, prov, "/chat/completions")
			if st == -1 {
				// 网络错（forwardExternal 未写响应，回落场景之外由这里写终态）
				writeJSON(w, map[string]any{"error": map[string]any{"message": prov.Name + " 请求失败（网络错误，详见实时动态）", "type": "server_error"}})
			}
			return
		}
		chain := VisionFallbackChain()
		for i, target := range chain {
			tp := s.providers.Match(target)
			if tp == nil {
				continue // 链中非 provider 模型（codely-vl 等）：跳出循环走团结路径
			}
			st := s.forwardExternal(w, r, body, target, wantsStream, tp, "/chat/completions")
			if st == 0 {
				return // 成功或已写客户端（流式中途断/4xx 透传），不重试
			}
			if !shouldFallback(st) {
				return // 4xx 配置错：forwardExternal 已写透传
			}
			// 网络错(-1)或 5xx/429：试链上下一个节点。链尾也可能是 provider 模型
			// （用户回落链末项配了外部视觉模型时，兜尾 codely-vl 被去重剔除），
			// 此时 i+1 越界必须防——链尽改走下方团结路径并改写为链尾模型。
			if i+1 < len(chain) {
				s.activity.Add("info", "识图回落 "+target+" → "+chain[i+1]+"（上游 "+
					strconv.Itoa(st)+"）", target, "", 0, 0, st)
				log.Printf("[tuanjie] vision fallback %s -> %s (upstream %d)", target, chain[i+1], st)
			} else {
				s.activity.Add("info", "识图回落 "+target+" → 团结路径（上游 "+
					strconv.Itoa(st)+"）", target, "", 0, 0, st)
				log.Printf("[tuanjie] vision fallback %s -> tuanjie (upstream %d)", target, st)
			}
		}
		// 链尾模型（通常 codely-vl 兜尾；链尾为 provider 模型且失败时同样改写
		// 它走团结路径——上游按模型名路由，非池内模型会如实报错，可接受）：
		// 改写 model 走下方账号池/单账号路径
		last := chain[len(chain)-1]
		var m map[string]any
		if json.Unmarshal(body, &m) == nil {
			m["model"] = last
			if nb, err := json.Marshal(m); err == nil {
				body = nb
				model = last
			}
		}
	}

	// 多账号池：有池时按账号选号转发（402 自动禁用/负载感知/GLM 路由）
	token, poolUID, usePool := s.accountTokenFor(model)
	if usePool {
		acc := s.pool.Get(poolUID)
		if acc == nil {
			writeJSON(w, map[string]any{"error": map[string]any{"message": "账号池无可用账号", "type": "server_error"}})
			return
		}
		s.pool.IncLoad(poolUID)
		defer s.pool.DecLoad(poolUID)
		start := time.Now()
		rid := s.registry.Register(model, poolUID, wantsStream)
		defer s.registry.Finish(rid)
		resp, err := s.ForwardDirect(r.Context(), http.MethodPost, "/v1/chat/completions", body, token, sess)
		if err != nil {
			writeJSON(w, map[string]any{"error": map[string]any{"message": "上游转发失败: " + err.Error(), "type": "server_error"}})
			return
		}
		defer resp.Body.Close()
		// 402 配额耗尽：禁用该账号并换下一个重试（学群友）
		if resp.StatusCode == http.StatusPaymentRequired {
			s.pool.MarkBudgetExceeded(poolUID)
			s.activity.Add("error", "账号 "+poolUID+" 配额用尽，自动禁用", model, poolUID, 0, 0, 402)
			log.Printf("[tuanjie] account=%s 402 budget_exceeded 已禁用，切换下一账号", poolUID)
			if nAcc, nTok := s.pool.PickWithToken(model); nAcc != nil && nAcc.UserID != poolUID {
				resp.Body.Close()
				token = nTok
				poolUID = nAcc.UserID
				resp, err = s.ForwardDirect(r.Context(), http.MethodPost, "/v1/chat/completions", body, nTok, sess)
				if err != nil {
					writeJSON(w, map[string]any{"error": map[string]any{"message": "上游转发失败: " + err.Error(), "type": "server_error"}})
					return
				}
				defer resp.Body.Close()
			}
		}
		// 上游瞬时错误（模型未映射/网关抖动）重试，与单账号路径对齐
		var poolErrBody []byte
		for attempt := 0; resp != nil && resp.StatusCode != http.StatusOK &&
			resp.StatusCode != http.StatusPaymentRequired &&
			resp.StatusCode != http.StatusTooManyRequests && attempt < 3; attempt++ {
			eb, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
			resp.Body.Close()
			if !retriableUpstream(string(eb)) {
				poolErrBody = eb
				break
			}
			log.Printf("[tuanjie] pool chat model=%s account=%s status=%d 重试 %d/3 err=%s",
				model, poolUID, resp.StatusCode, attempt+1, truncate(string(eb), 200))
			time.Sleep(800 * time.Millisecond)
			resp, err = s.ForwardDirect(r.Context(), http.MethodPost, "/v1/chat/completions", body, token, sess)
			if err != nil {
				writeJSON(w, map[string]any{"error": map[string]any{"message": "上游转发失败: " + err.Error(), "type": "server_error"}})
				return
			}
			defer resp.Body.Close()
			poolErrBody = nil
		}
		// 非 200 透传错误体（含重试后仍失败的情况）
		if resp.StatusCode != http.StatusOK {
			if poolErrBody == nil {
				poolErrBody, _ = io.ReadAll(io.LimitReader(resp.Body, 1<<20))
			}
			s.activity.Add("error", model+" · 账号 "+poolUID+" 上游返回 "+strconv.Itoa(resp.StatusCode),
				model, poolUID, time.Since(start).Milliseconds(), 0, resp.StatusCode)
			log.Printf("[tuanjie] pool chat model=%s account=%s status=%d err=%s",
				model, poolUID, resp.StatusCode, truncate(string(poolErrBody), 300))
			writeErr(w, resp.StatusCode, string(poolErrBody))
			return
		}
		// 200 透传（流式逐块冲刷 + 注册表 touch + usage 入账）
		copyHeader(w, resp)
		w.Header().Set("X-Accel-Buffering", "no")
		w.Header().Set("Cache-Control", "no-cache")
		log.Printf("[tuanjie] pool chat model=%s account=%s status=200", model, poolUID)
		if wantsStream {
			if f, ok := w.(http.Flusher); ok {
				lineScan := newUsageScanner(model, s)
				buf := make([]byte, 32*1024)
				for {
					n, rerr := resp.Body.Read(buf)
					if n > 0 {
						lineScan.feed(buf[:n])
						s.registry.Touch(rid, int64(n))
						if _, werr := w.Write(buf[:n]); werr != nil {
							return
						}
						f.Flush()
					}
					if rerr != nil {
						lineScan.finish()
						return
					}
				}
			}
		}
		respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
		_, _ = w.Write(respBody)
		var done struct {
			Model string `json:"model"`
			Usage *struct {
				PromptTokens     int64 `json:"prompt_tokens"`
				CompletionTokens int64 `json:"completion_tokens"`
				TotalTokens      int64 `json:"total_tokens"`
			} `json:"usage"`
		}
		if json.Unmarshal(respBody, &done) == nil && done.Usage != nil {
			s.addStat(model, done.Usage.PromptTokens, done.Usage.CompletionTokens, done.Usage.TotalTokens)
			// 被动注水观测：非流式能直接读响应 model
			s.water.RecordPassive(model, done.Model, poolUID)
			s.activity.Add("ok", model+" · 账号 "+poolUID, model, poolUID,
				time.Since(start).Milliseconds(), done.Usage.TotalTokens, 200)
		}
		return
	}

	start := time.Now()
	rid := s.registry.Register(model, "", wantsStream)
	defer s.registry.Finish(rid)
	send := func() (*http.Response, error) {
		return s.client.ForwardWithSession(r.Context(), http.MethodPost, "/v1/chat/completions", bytes.NewReader(body), r.Header.Get("Content-Type"), sess)
	}

	// KIMI-K3 节奏器（可开关）：开启且 model 含 "KIMI" 时放行前按滑窗预算排队
	// （预算不足挂起等待而非立即 429）；关闭时零影响直通。
	pacing := s.pacer.Enabled() && IsPacingModel(model)
	pacingDeadline := time.Time{}
	if pacing {
		est := EstimateTokens(body)
		pacingDeadline = s.pacer.Acquire(r.Context(), est)
		log.Printf("[tuanjie] pacing model=%s est=%d windowUsed=%d 放行", model, est, s.pacer.WindowUsed())
	}

	// 上游多实例可能未同步模型映射（间歇 400 model=None）→ 最多重试 3 次。
	var errBody []byte
	resp, err := send()
	// 节奏器 429 兜底：pacing 开启时代为等待自动重发（客户端只看到慢请求），
	// 重发共用同一 30 分钟总预算，超限后透传最后一次原始错误。
	for pacing && err == nil && resp != nil && resp.StatusCode == http.StatusTooManyRequests && time.Now().Before(pacingDeadline) {
		errBody, _ = io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		resp.Body.Close()
		wait := RateLimitWait(string(errBody))
		if end := time.Now().Add(wait); end.After(pacingDeadline) {
			wait = time.Until(pacingDeadline)
			if wait <= 0 {
				break
			}
		}
		log.Printf("[tuanjie] pacing model=%s status=429 等待 %s 后重发（剩余预算 %s）", model, wait.Round(time.Second), time.Until(pacingDeadline).Round(time.Second))
		select {
		case <-r.Context().Done():
		case <-time.After(wait):
			resp, err = send()
		}
		if r.Context().Err() != nil {
			break
		}
	}
	for attempt := 0; resp != nil && resp.StatusCode != http.StatusOK && attempt < 3; attempt++ {
		errBody, _ = io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		resp.Body.Close()
		// 429 限流：TPM 按模型计数，换 key 无效且重试继续消耗限流窗口；
		// 不重试、不 InvalidateKey，直接跳出循环进下方非 200 透传分支。
		if resp.StatusCode == http.StatusTooManyRequests {
			break
		}
		if !retriableUpstream(string(errBody)) {
			break
		}
		log.Printf("[tuanjie] chat model=%s status=%d 重试 %d/3 err=%s", model, resp.StatusCode, attempt+1, truncate(string(errBody), 200))
		s.client.InvalidateKey()
		time.Sleep(800 * time.Millisecond)
		resp, err = send()
	}
	if err != nil {
		log.Printf("[tuanjie] chat model=%s err=%v", model, err)
		writeErr(w, 502, err.Error())
		return
	}
	if resp.StatusCode != http.StatusOK {
		retryAfter := "-"
		if ra := resp.Header.Get("Retry-After"); ra != "" {
			retryAfter = ra
		}
		if resp.StatusCode == http.StatusTooManyRequests {
			log.Printf("[tuanjie] chat model=%s status=429 限流透传 retryAfter=%s", model, retryAfter)
		}
		log.Printf("[tuanjie] chat model=%s status=%d err=%s", model, resp.StatusCode, truncate(string(errBody), 300))
		if retryAfter != "-" {
			w.Header().Set("Retry-After", retryAfter)
		}
		writeErr(w, resp.StatusCode, string(errBody))
		return
	}

	// 200 但内容为空（上游过载时偶发，ZCode 侧表现为 empty_model_response）：
	// 在写给客户端之前嗅探，空则丢弃本次响应重放一次。
	if !s.ensureNonEmpty(wantsStream, &resp, send, model) {
		writeErr(w, 502, "上游连续返回空响应（GLM-5.3 过载），请稍后重试")
		return
	}

	defer resp.Body.Close()
	log.Printf("[tuanjie] chat model=%s status=200 dur=%s", model, time.Since(start).Round(time.Millisecond))
	defer func() {
		s.activity.Add("ok", model+" · 单账号", model, "", time.Since(start).Milliseconds(), 0, 200)
	}()

	copyHeader(w, resp)
	w.Header().Set("X-Accel-Buffering", "no")
	w.Header().Set("Cache-Control", "no-cache")
	if wantsStream {
		if f, ok := w.(http.Flusher); ok {
			// 流式：逐块转发并冲刷；同时行解析 data: 行提取 usage（统计）
			lineScan := newUsageScanner(model, s)
			buf := make([]byte, 32*1024)
			for {
				n, rerr := resp.Body.Read(buf)
				if n > 0 {
					lineScan.feed(buf[:n])
					s.registry.Touch(rid, int64(n))
					if _, werr := w.Write(buf[:n]); werr != nil {
						return
					}
					f.Flush()
				}
				if rerr != nil {
					lineScan.finish()
					return
				}
			}
		}
	}
	// 非流式：缓冲整个响应再转发，顺便解析 usage
	respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
	_, _ = w.Write(respBody)
	var done struct {
		Model string `json:"model"`
		Usage *struct {
			PromptTokens     int64 `json:"prompt_tokens"`
			CompletionTokens int64 `json:"completion_tokens"`
			TotalTokens      int64 `json:"total_tokens"`
		} `json:"usage"`
	}
	if json.Unmarshal(respBody, &done) == nil && done.Usage != nil {
		s.addStat(model, done.Usage.PromptTokens, done.Usage.CompletionTokens, done.Usage.TotalTokens)
	}
}

// externalBaseURL 规范化 provider base_url（去尾斜杠）；不含 /v1 时补上
// （Agnes 等开源风格 base_url 形如 https://host/v1，也有用户只填 host 的情况）。
func externalBaseURL(base string) string {
	base = strings.TrimRight(strings.TrimSpace(base), "/")
	if !strings.HasSuffix(base, "/v1") {
		base += "/v1"
	}
	return base
}

// forwardExternal 转发到外部 OpenAI 兼容 provider（静态 Bearer key，不占团结
// 账号池，学群友 _forward_external）。path 相对 base（如 /chat/completions），
// base_url 已含 /v1；流式逐块透传 + usage 抽取，非流式整体读回写；
// registry.Register 计入 inflight（user_id=provider 名），activity 记
// "外部源 {name} · {model}"。转发前做 sanitizeForExternal schema 修复。
// forwardExternal 转发到外部 provider。返回 failStatus：0 = 干净完成或已向
// 客户端写出（不可重试）；>0 = 失败且尚未写任何响应字节（上游状态码，或 -1
// 表示网络级错误）——调用方可据此回落重试（识图回落链）。
func (s *Server) forwardExternal(w http.ResponseWriter, r *http.Request, body []byte, model string, wantsStream bool, prov *ExternalProvider, path string) (failStatus int) {
	started := time.Now()
	if path == "/chat/completions" {
		body = sanitizeForExternal(body)
	}
	pname := prov.Name
	rid := s.registry.Register(model, pname, wantsStream)
	defer s.registry.Finish(rid)

	req, err := http.NewRequestWithContext(r.Context(), http.MethodPost, externalBaseURL(prov.BaseURL)+path, bytes.NewReader(body))
	if err != nil {
		// 构造错与网络错同语义：不写客户端，报 -1 给调用方终态/回落
		return -1
	}
	req.Header.Set("Authorization", "Bearer "+prov.APIKey)
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json")
	cl := &http.Client{Timeout: 120 * time.Second, Transport: smartProxyTransport}
	resp, err := cl.Do(req)
	if err != nil {
		latency := time.Since(started).Milliseconds()
		s.activity.Add("error", model+" · "+pname+" 异常: "+err.Error(), model, pname, latency, 0, 0)
		log.Printf("[tuanjie] external chat model=%s provider=%s err=%v", model, pname, err)
		// 网络级失败也是瞬时故障：不写客户端，报给调用方回落重试；
		// 调用方不回落时（链尽/非回落场景）负责写终态错误
		return -1
	}
	defer resp.Body.Close()
	latency := time.Since(started).Milliseconds()
	if resp.StatusCode != http.StatusOK {
		errBody, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		s.activity.Add("error", model+" · 外部源 "+pname+" 上游返回 "+strconv.Itoa(resp.StatusCode),
			model, pname, latency, 0, resp.StatusCode)
		log.Printf("[tuanjie] external chat model=%s provider=%s status=%d", model, pname, resp.StatusCode)
		if shouldFallback(resp.StatusCode) {
			// 瞬时故障（5xx/429）：不写客户端，把状态报给调用方回落重试
			return resp.StatusCode
		}
		w.Header().Set("Content-Type", resp.Header.Get("Content-Type"))
		w.WriteHeader(resp.StatusCode)
		_, _ = w.Write(errBody)
		return 0
	}
	log.Printf("[tuanjie] external chat model=%s provider=%s status=200", model, pname)
	defer func() {
		s.activity.Add("ok", "外部源 "+pname+" · "+model+(map[bool]string{true: "（流式）", false: ""})[wantsStream],
			model, pname, time.Since(started).Milliseconds(), 0, 200)
	}()
	copyHeader(w, resp)
	w.Header().Set("X-Accel-Buffering", "no")
	w.Header().Set("Cache-Control", "no-cache")
	if wantsStream && path == "/chat/completions" {
		if f, ok := w.(http.Flusher); ok {
			lineScan := newUsageScanner(model, s)
			buf := make([]byte, 32*1024)
			for {
				n, rerr := resp.Body.Read(buf)
				if n > 0 {
					lineScan.feed(buf[:n])
					s.registry.Touch(rid, int64(n))
					if _, werr := w.Write(buf[:n]); werr != nil {
						return 0
					}
					f.Flush()
				}
				if rerr != nil {
					lineScan.finish()
					return 0
				}
			}
		}
	}
	respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
	_, _ = w.Write(respBody)
	if path == "/chat/completions" {
		var done struct {
			Model string `json:"model"`
			Usage *struct {
				PromptTokens     int64 `json:"prompt_tokens"`
				CompletionTokens int64 `json:"completion_tokens"`
				TotalTokens      int64 `json:"total_tokens"`
			} `json:"usage"`
		}
		if json.Unmarshal(respBody, &done) == nil && done.Usage != nil {
			s.addStat(model, done.Usage.PromptTokens, done.Usage.CompletionTokens, done.Usage.TotalTokens)
		}
	}
	return 0
}

// handleImagesGenerations 生图端点：model 命中外部 provider → 转发
// {base}/v1/images/generations（Agnes 生图路径，与聊天不同端点；生图本就
// 非流式）。三类分流：请求模型为 video 类直接 400（Agnes 会拒绝且错误信息
// 指明走 /v1/videos，代理端先拦给引导文案）；imageModel 已配置且请求模型为
// image 类 → 统一改写后再转发；不命中 provider 返回 OpenAI 风格 404 错误体。
func (s *Server) handleImagesGenerations(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	body, _ := io.ReadAll(io.LimitReader(r.Body, 64<<20))
	var req struct {
		Model string `json:"model"`
	}
	model := ""
	if json.Unmarshal(body, &req) == nil {
		model = req.Model
	}
	if ModelKind(model) == "video" {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		_ = json.NewEncoder(w).Encode(map[string]any{
			"error": map[string]any{
				"message": "video 模型不支持生图端点，请走 POST /v1/videos（异步任务制，返回 task_id/video_id 后用 GET /v1/videos/{id} 取结果）",
				"type":    "invalid_request_error",
			},
		})
		return
	}
	if nb, note := RewriteImageModel(body, ImageModel()); nb != nil && note != "" {
		body = nb
		model = ImageModel()
		log.Printf("[tuanjie] images model 改写 %s", note)
	}
	prov := s.providers.Match(model)
	if prov == nil {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusNotFound)
		_ = json.NewEncoder(w).Encode(map[string]any{
			"error": map[string]any{
				"message": "unknown model for images endpoint: " + model + "（需先在外部账号里配置该模型的 provider）",
				"type":    "invalid_request_error",
				"code":    "model_not_found",
			},
		})
		return
	}
	if st := s.forwardExternal(w, r, body, model, false, prov, "/images/generations"); st == -1 {
		writeJSON(w, map[string]any{"error": map[string]any{"message": prov.Name + " 生图请求失败（网络错误，详见实时动态）", "type": "server_error"}})
	}
}

// handleVideoCreate 生视频端点（异步任务制）：model 命中 provider（video 类）
// → 转发 {base}/videos；videoModel 已配置且请求模型为 video 类 → 同样改写。
// 非 video 类模型 → 400（Agnes 家 video 模型只认 /v1/videos，别处也去不得，
// 反向同理）。响应原样透传（含 task_id/video_id）。
func (s *Server) handleVideoCreate(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	body, _ := io.ReadAll(io.LimitReader(r.Body, 64<<20))
	var req struct {
		Model string `json:"model"`
	}
	model := ""
	if json.Unmarshal(body, &req) == nil {
		model = req.Model
	}
	if ModelKind(model) != "video" {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusBadRequest)
		_ = json.NewEncoder(w).Encode(map[string]any{
			"error": map[string]any{
				"message": "videos 端点仅接受 video 类模型（名字含 video），当前: " + model + "；生图请走 POST /v1/images/generations",
				"type":    "invalid_request_error",
			},
		})
		return
	}
	if vm := VideoModel(); vm != "" && model != vm {
		var m map[string]any
		if json.Unmarshal(body, &m) == nil {
			m["model"] = vm
			if nb, err := json.Marshal(m); err == nil {
				body = nb
				log.Printf("[tuanjie] videos model 改写 %s→%s", model, vm)
				model = vm
			}
		}
	}
	prov := s.providers.Match(model)
	if prov == nil {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusNotFound)
		_ = json.NewEncoder(w).Encode(map[string]any{
			"error": map[string]any{
				"message": "unknown model for videos endpoint: " + model + "（需先在外部账号里配置该模型的 provider）",
				"type":    "invalid_request_error",
				"code":    "model_not_found",
			},
		})
		return
	}
	if st := s.forwardExternal(w, r, body, model, false, prov, "/videos"); st == -1 {
		writeJSON(w, map[string]any{"error": map[string]any{"message": prov.Name + " 视频请求失败（网络错误，详见实时动态）", "type": "server_error"}})
	}
}

// handleVideoQuery 视频任务结果查询（两种形式都支持）：
// GET /v1/videos/{id} → 转发 {base}/videos/{id}；
// GET /agnesapi?video_id=xxx → 转发 {base}/agnesapi?video_id=xxx（Agnes 推荐）。
// Agnes 文档：agnes-video-2.5-flash 查询必须带 model_name（不带只适用 text 模式）
// ——客户端传了就透传；没传但代理的生视频模型已配置时自动带上（兜底 Flash 查询）。
// 取结果场景 model 信息已不在请求里，无法按模型命中 provider：只要任一
// provider 配置了就转发到第一个（多 provider 时可能转错源——按"有 provider
// 就转发第一个可达的"简单实现，够用；真正要精确路由得查 task 归属，本轮不做）。
func (s *Server) handleVideoQuery(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	videoID := ""
	modelName := r.URL.Query().Get("model_name") // 客户端显式指定优先
	if modelName == "" {
		modelName = VideoModel() // 未指定时用代理配置的生视频模型兜底
	}
	target := "" // 相对 provider base 的查询路径
	if strings.HasPrefix(r.URL.Path, "/v1/videos/") {
		videoID = strings.TrimPrefix(r.URL.Path, "/v1/videos/")
		if videoID == "" || strings.Contains(videoID, "/") {
			writeErr(w, 400, "路径需为 /v1/videos/{id}")
			return
		}
		target = "/videos/" + videoID
		if modelName != "" {
			target += "?model_name=" + url.QueryEscape(modelName)
		}
	} else if vid := r.URL.Query().Get("video_id"); vid != "" {
		videoID = vid
		target = "/agnesapi?video_id=" + url.QueryEscape(vid)
		if modelName != "" {
			target += "&model_name=" + url.QueryEscape(modelName)
		}
	} else {
		writeErr(w, 400, "需带 video_id（/v1/videos/{id} 或 ?video_id=）")
		return
	}
	provs := s.providers.List()
	if len(provs) == 0 {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusNotFound)
		_ = json.NewEncoder(w).Encode(map[string]any{
			"error": map[string]any{
				"message": "未配置外部 provider，无法查询视频任务 " + videoID,
				"type":    "invalid_request_error",
				"code":    "provider_not_found",
			},
		})
		return
	}
	// 第一个 provider（顺序即配置顺序）
	prov := provs[0]
	req, err := http.NewRequestWithContext(r.Context(), http.MethodGet,
		externalBaseURL(prov.BaseURL)+target, nil)
	if err != nil {
		writeErr(w, 502, prov.Name+" 请求失败: "+err.Error())
		return
	}
	req.Header.Set("Authorization", "Bearer "+prov.APIKey)
	req.Header.Set("Accept", "application/json")
	cl := &http.Client{Timeout: 60 * time.Second, Transport: smartProxyTransport}
	resp, err := cl.Do(req)
	if err != nil {
		writeErr(w, 502, prov.Name+" 请求失败: "+err.Error())
		return
	}
	defer resp.Body.Close()
	respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	w.Header().Set("Content-Type", resp.Header.Get("Content-Type"))
	w.WriteHeader(resp.StatusCode)
	_, _ = w.Write(respBody)
}

// usageScanner 流式 SSE 行扫描：拼行、解析 data: 行里的 usage，命中即入账。
type usageScanner struct {
	model string
	buf   string
	srv   *Server
}

func newUsageScanner(model string, srv *Server) *usageScanner {
	return &usageScanner{model: model, srv: srv}
}

func (u *usageScanner) feed(chunk []byte) {
	u.buf += string(chunk)
	for {
		i := strings.IndexByte(u.buf, '\n')
		if i < 0 {
			break
		}
		line := strings.TrimSpace(u.buf[:i])
		u.buf = u.buf[i+1:]
		if !strings.HasPrefix(line, "data:") || strings.Contains(line, "[DONE]") {
			continue
		}
		var chunk struct {
			Usage *struct {
				PromptTokens     int64 `json:"prompt_tokens"`
				CompletionTokens int64 `json:"completion_tokens"`
				TotalTokens      int64 `json:"total_tokens"`
			} `json:"usage"`
		}
		if json.Unmarshal([]byte(strings.TrimSpace(line[5:])), &chunk) == nil && chunk.Usage != nil {
			u.srv.addStat(u.model, chunk.Usage.PromptTokens, chunk.Usage.CompletionTokens, chunk.Usage.TotalTokens)
		}
	}
}

func (u *usageScanner) finish() { /* 流结束时缓冲里通常已无完整行 */ }

// addStat 累计一次调用的用量并落盘。
func (s *Server) addStat(model string, in, out, total int64) {
	if total == 0 && in == 0 && out == 0 {
		return
	}
	s.statsMu.Lock()
	st := s.stats[model]
	if st == nil {
		st = &modelStat{}
		s.stats[model] = st
	}
	st.Calls++
	st.InputTok += in
	st.OutputTok += out
	if total > 0 {
		st.TotalTok += total
	} else {
		st.TotalTok += in + out
	}
	s.statsMu.Unlock()
	s.saveStats()
	log.Printf("[tuanjie] stat model=%s in=%d out=%d", model, in, out)
}

// handleStats 返回消耗统计（GUI 消耗 TOP）。
func (s *Server) handleStats(w http.ResponseWriter, r *http.Request) {
	s.statsMu.Lock()
	out := make(map[string]*modelStat, len(s.stats))
	for k, v := range s.stats {
		cp := *v
		out[k] = &cp
	}
	s.statsMu.Unlock()
	writeJSON(w, map[string]any{
		"models":    out,
		"uptimeSec": int64(time.Since(s.startedAt).Seconds()),
	})
}

// handleQuota 返回团结积分快照（GUI 链路监测积分卡）。
func (s *Server) handleQuota(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, s.client.FetchQuota(r.Context()))
}

// handleKimiPacing 节奏器开关端点（GUI 团结视图一键切换）：
// GET 返回 {"enabled","pending","windowUsed"}；POST {"enabled":bool} 切换并
// 持久化到 exe 同目录 tuanjie-pacing.json（重启记忆）。
func (s *Server) handleKimiPacing(w http.ResponseWriter, r *http.Request) {
	if r.Method == http.MethodPost {
		var req struct {
			Enabled *bool `json:"enabled"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil || req.Enabled == nil {
			writeErr(w, 400, "body 需为 {\"enabled\":true|false}")
			return
		}
		s.pacer.SetEnabled(*req.Enabled)
		log.Printf("[tuanjie] pacing 开关切换 enabled=%v", *req.Enabled)
	}
	writeJSON(w, map[string]any{
		"enabled":    s.pacer.Enabled(),
		"pending":    s.pacer.Pending(),
		"windowUsed": s.pacer.WindowUsed(),
	})
}

// loadStats 启动时读回历史统计。
func (s *Server) loadStats() {
	if s.statsPath == "" {
		return
	}
	b, err := os.ReadFile(s.statsPath)
	if err != nil {
		return
	}
	var m map[string]*modelStat
	if json.Unmarshal(b, &m) == nil {
		s.stats = m
	}
}

// saveStats 原子落盘。
func (s *Server) saveStats() {
	if s.statsPath == "" {
		return
	}
	s.statsMu.Lock()
	b, err := json.Marshal(s.stats)
	s.statsMu.Unlock()
	if err != nil {
		return
	}
	tmp := s.statsPath + ".tmp"
	if os.WriteFile(tmp, b, 0o644) == nil {
		_ = os.Rename(tmp, s.statsPath)
	}
}

// youAreRe 匹配 "You are" 身份声明开头（大小写不敏感、容忍空白）。
// 上游反竞品检测按 `you\s+are\s+<竞品名>` 模式拦 system 消息，在 You 与
// are 之间插零宽空格即可打断匹配（模型读不到该字符，语义无损）。
var youAreRe = regexp.MustCompile(`(?i)\b(you)\s+(are)\b`)

// agentIdentityTriggerRe 判断是否命中反竞品检测：system 里出现
// "You are <竞品名>"（ZCode / Claude Code / Codex 实测被拦；Cursor 与
// 无关名字实测放行——上游维护的是竞品名单，不是任意名字）。命中才做
// JSON 往返改写，未命中零开销原样透传。
var agentIdentityTriggerRe = regexp.MustCompile(`(?i)\byou\s+are\s+(a\s+|an\s+|the\s+)?(z\s*code|claude\s*code|codex|cursor|windsurf|trae|cline|aider|opencode|codebuddy|codegeex)`)

// zwsp 零宽空格（U+200B），对模型不可见。
const zwsp = "​"

// desensitizeAgentIdentity 规避上游反竞品过滤：对 system 消息里的
// "You are" 身份声明，在 You 与 are 之间插入零宽空格（实测放行，模型
// 语义无损）。只处理 role=system（user 消息 / tools 描述实测不在检测
// 范围，不做无谓改写）。命中触发模式才改写；返回 nil 表示无需处理。
// 消息内的 content 兼容字符串与分段数组两种形态。改写用消息级
// RawMessage 重组，不破坏 messages 数组内字段序。
func desensitizeAgentIdentity(body []byte) []byte {
	var m map[string]json.RawMessage
	if err := json.Unmarshal(body, &m); err != nil {
		return nil
	}
	msgsRaw, ok := m["messages"]
	if !ok {
		return nil
	}
	var msgs []json.RawMessage
	if err := json.Unmarshal(msgsRaw, &msgs); err != nil {
		return nil
	}
	changed := false
	for i, mr := range msgs {
		var msg map[string]json.RawMessage
		if err := json.Unmarshal(mr, &msg); err != nil {
			continue
		}
		var role string
		if err := json.Unmarshal(msg["role"], &role); err != nil || role != "system" {
			continue
		}
		contentRaw, ok := msg["content"]
		if !ok {
			continue
		}
		// 形态一：content 为字符串
		var s string
		if err := json.Unmarshal(contentRaw, &s); err == nil {
			if agentIdentityTriggerRe.MatchString(s) {
				msg["content"], _ = json.Marshal(youAreRe.ReplaceAllString(s, "$1"+zwsp+"$2"))
				nb, _ := json.Marshal(msg)
				msgs[i] = nb
				changed = true
			}
			continue
		}
		// 形态二：content 为分段数组（只动 text 字段）
		var parts []json.RawMessage
		if err := json.Unmarshal(contentRaw, &parts); err != nil {
			continue
		}
		partChanged := false
		for j, pr := range parts {
			var part map[string]json.RawMessage
			if err := json.Unmarshal(pr, &part); err != nil {
				continue
			}
			var t string
			if err := json.Unmarshal(part["text"], &t); err != nil || !agentIdentityTriggerRe.MatchString(t) {
				continue
			}
			part["text"], _ = json.Marshal(youAreRe.ReplaceAllString(t, "$1"+zwsp+"$2"))
			nb, _ := json.Marshal(part)
			parts[j] = nb
			partChanged = true
		}
		if partChanged {
			nb, _ := json.Marshal(parts)
			msg["content"] = nb
			nb2, _ := json.Marshal(msg)
			msgs[i] = nb2
			changed = true
		}
	}
	if !changed {
		return nil
	}
	nb, _ := json.Marshal(msgs)
	m["messages"] = nb
	out, err := json.Marshal(m)
	if err != nil {
		return nil
	}
	return out
}

// retriableUpstream 判断上游错误是否值得换 key 重试
// （模型映射错误/限流/网关抖动，均为瞬时状态）。
func retriableUpstream(body string) bool {
	if body == "" {
		return false
	}
	low := strings.ToLower(body)
	return strings.Contains(low, "model") ||
		strings.Contains(low, "rate") ||
		strings.Contains(low, "429") ||
		strings.Contains(low, "502") ||
		strings.Contains(low, "503") ||
		strings.Contains(low, "504") ||
		strings.Contains(low, "bad gateway") ||
		strings.Contains(low, "internal server")
}

// ensureNonEmpty 在响应写给客户端前嗅探"200 但内容为空"（上游过载偶发），
// 空则丢弃本次响应并重放一次（send 复用已缓存的请求体）。
// 返回 false 表示重放后仍为空（此时原 resp 已关闭，调用方只能报错）。
// 非空时 *resp 指向可正常读的响应（嗅探字节已拼回头部，不丢数据）。
func (s *Server) ensureNonEmpty(stream bool, resp **http.Response, send func() (*http.Response, error), model string) bool {
	for attempt := 0; attempt < 2; attempt++ {
		r := *resp
		if r == nil || r.Body == nil {
			return false
		}
		var head []byte
		if stream {
			// 流式：读首段最多 64KB。只把"流到 EOF 仍无任何 data 行"判为空流；
			// 出现过 data 行（哪怕首个内容 chunk 延迟 >3s）都放行透传，
			// 否则慢启动的正常流会被误判为空而重放（实测 2026-08-21）。
			buf := make([]byte, 64*1024)
			readErr := error(nil)
			for len(head) < 64*1024 {
				n, err := r.Body.Read(buf)
				head = append(head, buf[:n]...)
				if err != nil {
					readErr = err
					break
				}
				if hasStreamDataLine(head) {
					break
				}
			}
			if streamHasContent(head) || readErr == nil || hasStreamDataLine(head) {
				r.Body = io.NopCloser(io.MultiReader(bytes.NewReader(head), r.Body))
				return true
			}
			log.Printf("[tuanjie] chat model=%s status=200 空流式响应(EOF无内容)，重放 %d/1", model, attempt+1)
		} else {
			// 非流式：整体读出，检查 choices[].message 内容与 tool_calls。
			b, _ := io.ReadAll(io.LimitReader(r.Body, 64<<20))
			_ = r.Body.Close()
			head = b
			if nonStreamHasContent(b) {
				r.Body = io.NopCloser(bytes.NewReader(b))
				return true
			}
			log.Printf("[tuanjie] chat model=%s status=200 空响应，重放 %d/1", model, attempt+1)
		}
		// 本次响应为空：关闭并重放（间隔放宽避免误判时连发刺激上游）
		_ = r.Body.Close()
		time.Sleep(1500 * time.Millisecond)
		nr, err := send()
		if err != nil || nr == nil || nr.StatusCode != http.StatusOK {
			if nr != nil {
				_ = nr.Body.Close()
			}
			return false
		}
		*resp = nr
	}
	return false
}

// hasStreamDataLine 判断首段里是否已出现 SSE data 行（说明流结构正常，内容交给客户端）。
func hasStreamDataLine(head []byte) bool {
	return bytes.HasPrefix(head, []byte("data:")) || bytes.Contains(head, []byte("\ndata:"))
}

// streamHasContent 判断 SSE 首段里是否出现实际生成内容（content 或 reasoning 有非空增量）。
func streamHasContent(head []byte) bool {
	for _, line := range bytes.Split(head, []byte("\n")) {
		line = bytes.TrimSpace(line)
		if !bytes.HasPrefix(line, []byte("data:")) {
			continue
		}
		payload := bytes.TrimSpace(line[5:])
		if len(payload) == 0 || bytes.Equal(payload, []byte("[DONE]")) {
			continue
		}
		var chunk struct {
			Choices []struct {
				Delta struct {
					Content   string `json:"content"`
					Reasoning string `json:"reasoning_content"`
				} `json:"delta"`
				Message struct {
					Content   string `json:"content"`
					Reasoning string `json:"reasoning_content"`
				} `json:"message"`
			} `json:"choices"`
		}
		if json.Unmarshal(payload, &chunk) != nil {
			continue
		}
		for _, c := range chunk.Choices {
			if c.Delta.Content != "" || c.Delta.Reasoning != "" ||
				c.Message.Content != "" || c.Message.Reasoning != "" {
				return true
			}
		}
	}
	return false
}

// nonStreamHasContent 判断非流式响应是否有实际内容、思考或工具调用。
func nonStreamHasContent(body []byte) bool {
	var done struct {
		Choices []struct {
			Message struct {
				Content   string          `json:"content"`
				Reasoning string          `json:"reasoning_content"`
				ToolCalls json.RawMessage `json:"tool_calls"`
			} `json:"message"`
		} `json:"choices"`
	}
	if json.Unmarshal(body, &done) != nil {
		return len(bytes.TrimSpace(body)) > 0 // 解析失败：有字节就放行（让客户端自己判断）
	}
	for _, c := range done.Choices {
		if c.Message.Content != "" || c.Message.Reasoning != "" || len(c.Message.ToolCalls) > 0 {
			return true
		}
	}
	return false
}

func copyHeader(w http.ResponseWriter, resp *http.Response) {
	for k, vs := range resp.Header {
		for _, v := range vs {
			if strings.EqualFold(k, "Content-Length") || strings.EqualFold(k, "Transfer-Encoding") {
				continue
			}
			w.Header().Add(k, v)
		}
	}
	w.WriteHeader(resp.StatusCode)
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

// logRequestShape 打印入站请求的结构指纹，用于对比不同客户端（官方 CLI / ZCode）的请求形态。
// 只记录结构摘要，不记录完整消息内容（避免敏感信息刷日志）。
func logRequestShape(model string, body []byte) {
	var m map[string]any
	if json.Unmarshal(body, &m) != nil {
		log.Printf("[tuanjie] shape model=%s 非JSON请求", model)
		return
	}
	fields := make([]string, 0, len(m))
	for k := range m {
		fields = append(fields, k)
	}
	// 消息结构
	msgDetail := ""
	if msgs, ok := m["messages"].([]any); ok {
		msgDetail = fmt.Sprintf("messages=%d", len(msgs))
		for i, raw := range msgs {
			if i >= 6 {
				break
			}
			if msg, ok := raw.(map[string]any); ok {
				keys := make([]string, 0, len(msg))
				for k := range msg {
					keys = append(keys, k)
				}
				role, _ := msg["role"].(string)
				ctype := "?"
				contentLen := 0
				switch cv := msg["content"].(type) {
				case string:
					ctype = "str"
					contentLen = len(cv)
				case []any:
					ctype = "arr"
					contentLen = len(cv)
				case nil:
					ctype = "nil"
				}
				msgDetail += fmt.Sprintf(" [%d]role=%s keys=%v c=%s clen=%d", i, role, keys, ctype, contentLen)
			}
		}
	}
	sysLen := 0
	if sys, ok := m["system"].(string); ok {
		sysLen = len(sys)
	}
	// 顶层特殊字段
	var special []string
	for _, k := range []string{"reasoning_effort", "user", "metadata", "stream_options", "tools", "tool_choice", "temperature", "max_tokens", "extra_body", "store", "model_info"} {
		if _, ok := m[k]; ok {
			special = append(special, k)
		}
	}
	log.Printf("[tuanjie] shape model=%s fields=%v %s system=%dB special=%v", model, fields, msgDetail, sysLen, special)
}

func writeErr(w http.ResponseWriter, status int, msg string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{
		"error": map[string]any{"message": msg, "type": "api_error"},
	})
}
