// server.go —— CodeBuddy 本地 OpenAI 兼容服务（Go 重写）。
//
// 端点：
//   GET  /health                诊断（auth 文件/账号/token 状态）
//   GET  /quota                 官方积分快照（get-user-resource，5min 缓存）
//   GET  /v1/models             动态探测的可用模型（1h 缓存）
//   GET  /model/info            模型元数据（Agent 填写指南：上下文/最大输出/思考级别）
//   GET/POST /v1/failover       hy4 限流兜底开关 + fallback 模型（GUI 甲板）
//   POST /v1/chat/completions   OpenAI Chat（流式透传 / 非流式聚合）
package codebuddy

import (
	"bufio"
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"
)

// Server 是 CodeBuddy 转发的本地服务。
type Server struct {
	cred        *Credential
	desensitize bool
	client      *http.Client
	ln          net.Listener
	mu          sync.Mutex
	srv         *http.Server

	probeMu   sync.Mutex
	probeAt   time.Time
	probeList []string

	startedAt  time.Time // 运行时长展示
	statsMu    sync.Mutex
	stats      map[string]*modelStat // 按模型累计（GUI 消耗 TOP）
	statsPath  string

	failMu       sync.Mutex // hy4 兜底配置（GUI 开关 + fallback 模型，落盘 codebuddy-failover.json）
	failEnabled  bool
	failFallback string
	failPath     string

	limitMu    sync.Mutex // hy4-preview 限流窗口（第 43 轮自动故障转移）
	limitUntil time.Time
}

// modelStat 单模型用量累计。
type modelStat struct {
	Calls      int64 `json:"calls"`
	InputTok   int64 `json:"inputTokens"`
	OutputTok  int64 `json:"outputTokens"`
	TotalTok   int64 `json:"totalTokens"`
}

// corsWith 给所有响应加 CORS 头：GUI 页面跑在 localhost:随机端口，
// fetch 127.0.0.1:8787 属跨域，没有这个头前端全部拉不到数据。
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

// NewServer 创建服务。desensitize=true 时对 system/developer/tools 做零宽脱敏。
func NewServer(desensitize bool) (*Server, error) {
	cred := NewCredential("")
	if cred.Path() == "" {
		return nil, fmt.Errorf("未找到 CodeBuddy/WorkBuddy 登录文件，请先在桌面端完成登录")
	}
	s := &Server{
		cred:        cred,
		desensitize: desensitize,
		client:      &http.Client{Timeout: 0}, // 流式长连接不设总超时
		stats:       map[string]*modelStat{},
		startedAt:   time.Now(),
	}
	if exe, err := os.Executable(); err == nil {
		s.statsPath = filepath.Join(filepath.Dir(exe), "codebuddy-stats.json")
		s.loadStats()
		s.failPath = filepath.Join(filepath.Dir(exe), "codebuddy-failover.json")
		s.loadFailover()
	}
	return s, nil
}

// Start 在 host:port 监听（阻塞）。
func (s *Server) Start(host, port string) error {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/quota", s.handleQuota)
	mux.HandleFunc("/v1/models", s.handleModels)
	mux.HandleFunc("/model/info", s.handleModelInfo)
	mux.HandleFunc("/v1/stats", s.handleStats)
	mux.HandleFunc("/v1/failover", s.handleFailover)
	mux.HandleFunc("/v1/chat/completions", s.handleChat)

	ln, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return fmt.Errorf("端口 %s 被占用: %w", port, err)
	}
	s.mu.Lock()
	s.ln = ln
	s.startedAt = time.Now()
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
	resp := map[string]any{
		"status":  "ok",
		"service": "codebuddy-go",
		"backend": backendBase,
		"mode":    "direct-proxy (native function calling)",
	}
	if !s.startedAt.IsZero() {
		resp["uptimeSec"] = int64(time.Since(s.startedAt).Seconds())
	}
	if sum := s.cred.Summary(); sum != nil {
		if err, ok := sum["error"]; ok {
			resp["status"] = "error"
			resp["message"] = fmt.Sprintf("%v", err)
		} else {
			resp["credential"] = sum
		}
	}
	// 第 43 轮：hy4-preview 限流窗口状态（GUI/脚本可观测；到期解除由 hy4Limited 懒处理）
	if lim := s.hy4LimitState(); lim != nil {
		resp["hy4_limit"] = lim
	}
	writeJSON(w, resp)
}

// handleQuota 返回官方积分快照（GUI 积分卡用）。refresh=1 强制实时（手动刷新按钮）。
func (s *Server) handleQuota(w http.ResponseWriter, r *http.Request) {
	if r.URL.Query().Get("refresh") == "1" {
		writeJSON(w, s.FetchQuotaForce(r.Context()))
		return
	}
	writeJSON(w, s.FetchQuota(r.Context()))
}

// handleStats 返回消耗统计 + 运行时长（GUI 视图用）。
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
	log.Printf("[codebuddy] stat model=%s in=%d out=%d", model, in, out)
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

// ============ hy4 兜底配置（开关 + fallback 模型，GUI 甲板可切） ============

// failoverCfg 落盘结构：statsPath 同目录的 codebuddy-failover.json。
type failoverCfg struct {
	Enabled  bool   `json:"enabled"`
	Fallback string `json:"fallback"`
}

// failoverConfig 内存配置快照（handleChat 守卫用）。
type failoverConfig struct {
	enabled  bool
	fallback string
}

func (s *Server) failoverConfig() failoverConfig {
	s.failMu.Lock()
	defer s.failMu.Unlock()
	return failoverConfig{enabled: s.failEnabled, fallback: s.failFallback}
}

// loadFailover 启动读回；文件缺失/损坏（解析失败或 fallback 为空）用缺省值
// （开 + deepseek-v4-pro，即 hy4Fallback 常量降级后的唯一用途）。
func (s *Server) loadFailover() {
	s.failMu.Lock()
	defer s.failMu.Unlock()
	s.failEnabled = true
	s.failFallback = hy4Fallback
	if s.failPath == "" {
		return
	}
	b, err := os.ReadFile(s.failPath)
	if err != nil {
		return
	}
	var c failoverCfg
	if json.Unmarshal(b, &c) != nil || c.Fallback == "" {
		return
	}
	s.failEnabled = c.Enabled
	s.failFallback = c.Fallback
}

// saveFailover 原子落盘（照抄 saveStats）。
func (s *Server) saveFailover() {
	if s.failPath == "" {
		return
	}
	s.failMu.Lock()
	b, err := json.Marshal(failoverCfg{Enabled: s.failEnabled, Fallback: s.failFallback})
	s.failMu.Unlock()
	if err != nil {
		return
	}
	tmp := s.failPath + ".tmp"
	if os.WriteFile(tmp, b, 0o644) == nil {
		_ = os.Rename(tmp, s.failPath)
	}
}

// hy4LimitState 限流窗口快照（无窗口/已到期返回 nil；到期顺带懒清除）。
// handleHealth 与 /v1/failover GET 共用；fallback 字段反映当前配置而非缺省常量。
func (s *Server) hy4LimitState() map[string]any {
	s.limitMu.Lock()
	defer s.limitMu.Unlock()
	if s.limitUntil.IsZero() {
		return nil
	}
	if time.Now().After(s.limitUntil) {
		s.limitUntil = time.Time{}
		return nil
	}
	s.failMu.Lock()
	fb := s.failFallback
	s.failMu.Unlock()
	return map[string]any{
		"active":   true,
		"until":    s.limitUntil.Format("2006-01-02 15:04:05"),
		"fallback": fb,
	}
}

// handleFailover /v1/failover：兜底开关与 fallback 模型读写（GUI 甲板）。
// GET → {enabled, fallback, hy4_limit}；POST body {"enabled":bool,"fallback":"model-id"}
// （两字段均可选）→ fallback 非空且 ∈ modelCandidates，否则 400。
func (s *Server) handleFailover(w http.ResponseWriter, r *http.Request) {
	switch r.Method {
	case http.MethodGet:
		cfg := s.failoverConfig()
		resp := map[string]any{"enabled": cfg.enabled, "fallback": cfg.fallback, "hy4_limit": s.hy4LimitState()}
		writeJSON(w, resp)
	case http.MethodPost:
		var body struct {
			Enabled  *bool   `json:"enabled"`
			Fallback *string `json:"fallback"`
		}
		b, _ := io.ReadAll(io.LimitReader(r.Body, 1<<20))
		if err := json.Unmarshal(b, &body); err != nil {
			writeErr(w, 400, "bad json: "+err.Error())
			return
		}
		if body.Fallback != nil {
			f := *body.Fallback
			valid := false // 必须命中候选池（第 43 轮勘误：曾写成 f != ""，非空即放行）
			for _, m := range modelCandidates {
				if m == f {
					valid = true
					break
				}
			}
			if !valid {
				writeErr(w, 400, "unknown fallback model: "+f)
				return
			}
		}
		s.failMu.Lock()
		if body.Fallback != nil {
			s.failFallback = *body.Fallback
		}
		if body.Enabled != nil {
			s.failEnabled = *body.Enabled
		}
		cfg := failoverConfig{enabled: s.failEnabled, fallback: s.failFallback}
		s.failMu.Unlock()
		s.saveFailover()
		log.Printf("[codebuddy] failover 配置更新: enabled=%v fallback=%s", cfg.enabled, cfg.fallback)
		writeJSON(w, map[string]any{"enabled": cfg.enabled, "fallback": cfg.fallback})
	default:
		writeErr(w, 405, "method not allowed")
	}
}

// failoverDecision 兜底守卫的判定结果（纯逻辑，便于单测）。
type failoverDecision struct {
	rewrite bool      // 请求前改写模型
	trip    bool      // 记限流窗口并重放
	until   time.Time // 窗口重置时点（trip=true 时有效）
}

// failoverDecide 兜底守卫纯逻辑：cfg.enabled=false 时改写与 trip 一律不触发
//（429 如实透传）。limited=是否已处于限流窗口；status/body=上游响应
//（仅 hy4-preview 的 429 配额报文参与 trip 判定）。
func failoverDecide(cfg failoverConfig, sentModel string, limited bool, status int, body string) failoverDecision {
	var d failoverDecision
	if !cfg.enabled || sentModel != hy4Primary {
		return d
	}
	if limited {
		d.rewrite = true
	}
	if status == http.StatusTooManyRequests {
		if until := parseQuotaReset(body); !until.IsZero() {
			d.trip = true
			d.until = until
		}
	}
	return d
}

// ============ 模型列表（动态探测，后端无模型列表端点） ============

// modelCandidates 已知候选池（探测后只返回真实可用的）。上游无模型列表
// 端点（2026-08-28 实测 /models、/v1/models、/v2/models 均 404），名单外
// 模型永远不会进矩阵。
// 名单对齐官方客户端模型列表（2026-08-30 用户实测截图）：Hy4 preview 免费档
// 与付费档同 id；客户端「Kimi-K2.7-Code」即 id kimi-k2.7（kimi-k2.7-code
// 实测 400 11102 不存在）；glm-5.0 / glm-4.7 已被上游下线（400）。不在官方
// 列表的历史候选（kimi-k2.5 / deepseek-v3.2 系 / minimax-m2.7 等）部分仍
// 可用，但按官方口径不进矩阵。
var modelCandidates = []string{
	"auto",
	"hy4-preview", "hy3",
	"glm-5.3", "glm-5.3-flash", "glm-5.2", "glm-5.1", "glm-5v-turbo",
	"minimax-m3",
	"kimi-k3", "kimi-k2.7", "kimi-k2.6",
	"deepseek-v4-flash", "deepseek-v4-pro",
}

// Models 返回真实可用模型（并行最小请求探测，1h 缓存）。
func (s *Server) Models(ctx context.Context) []string {
	s.probeMu.Lock()
	fresh := time.Since(s.probeAt) < time.Hour && len(s.probeList) > 0
	cached := s.probeList
	s.probeMu.Unlock()
	if fresh {
		return cached
	}

	hdr, err := s.cred.Headers(ctx)
	if err == nil {
		var wg sync.WaitGroup
		var mu sync.Mutex
		okSet := map[string]bool{}
		sem := make(chan struct{}, 10)
		for _, m := range modelCandidates {
			wg.Add(1)
			go func(model string) {
				defer wg.Done()
				sem <- struct{}{}
				defer func() { <-sem }()
				if s.probeOne(ctx, hdr, model) {
					mu.Lock()
					okSet[model] = true
					mu.Unlock()
				}
			}(m)
		}
		wg.Wait()
		if len(okSet) > 0 {
			list := make([]string, 0, len(okSet))
			for _, m := range modelCandidates { // 按候选池顺序输出（稳定）
				if okSet[m] {
					list = append(list, m)
				}
			}
			s.probeMu.Lock()
			s.probeList = list
			s.probeAt = time.Now()
			s.probeMu.Unlock()
			log.Printf("[codebuddy] 模型探测完成: %d/%d 可用", len(list), len(modelCandidates))
			return list
		}
	}
	// 全部失败（网络异常）→ 回退候选池，保证列表不空
	s.probeMu.Lock()
	s.probeList = append([]string{}, modelCandidates...)
	s.probeAt = time.Now()
	s.probeMu.Unlock()
	return modelCandidates
}

// probeOne 发最小流式请求探测模型是否可用（读到数据即认为后端接受该模型名）。
func (s *Server) probeOne(ctx context.Context, hdr http.Header, model string) bool {
	pctx, cancel := context.WithTimeout(ctx, 25*time.Second)
	defer cancel()
	body, _ := json.Marshal(map[string]any{
		"model": model, "stream": true,
		"messages": []map[string]string{{"role": "user", "content": "hi"}},
	})
	req, err := http.NewRequestWithContext(pctx, http.MethodPost,
		backendBase+"/v2/chat/completions", bytes.NewReader(body))
	if err != nil {
		return false
	}
	req.Header = cloneHeader(hdr)
	resp, err := s.client.Do(req)
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return false
	}
	buf := make([]byte, 64)
	if _, err := resp.Body.Read(buf); err != nil && err.Error() == "EOF" {
		return false
	}
	return true
}

func cloneHeader(h http.Header) http.Header {
	n := http.Header{}
	for k, vs := range h {
		for _, v := range vs {
			n.Add(k, v)
		}
	}
	return n
}

func (s *Server) handleModels(w http.ResponseWriter, r *http.Request) {
	models := s.Models(r.Context())
	data := make([]map[string]any, 0, len(models))
	for _, m := range models {
		data = append(data, map[string]any{
			"id": m, "object": "model", "created": 1700000000, "owned_by": "codebuddy",
		})
	}
	writeJSON(w, map[string]any{"object": "list", "data": data})
}

// ============ /model/info（Agent 填写指南） ============

// modelMeta 腾讯后端无元数据端点。reasoning 为 2026-09-02 实测矩阵（走后端
// 端口发 1-token 最小请求探测 reasoning_effort 接受档位：deepseek 系与 auto
// 拒绝 off（400 11150），其余五档全接受）。maxInput 仅保留有实测备注的三条
// （glm-5.2、deepseek-v4-pro、deepseek-v4-flash）；其余模型的 maxInput /
// maxOutput 数字来源不可考（Python v3.0.0 抄录，其声称的请求级校验实测
// 不存在），一律不写——前端对缺失字段如实显示「未核实」。
var modelMeta = map[string]map[string]any{
	"auto":              {"reasoning": "low/medium/high/max", "note": "后端自动路由到合适模型"},
	"hy4-preview":       {"reasoning": "off/low/medium/high/max"},
	"hy3":               {"reasoning": "off/low/medium/high/max"},
	"hy3-preview":       {"reasoning": "off/low/medium/high/max"},
	"hy3-preview-agent": {"reasoning": "off/low/medium/high/max"},
	"glm-5.3":           {"reasoning": "off/low/medium/high/max", "note": "新模型"},
	"glm-5.3-flash":     {"reasoning": "off/low/medium/high/max"},
	"glm-5.2":           {"maxInput": 1048576, "reasoning": "off/low/medium/high/max", "note": "上下文实测 1M"},
	"glm-5.1":           {"reasoning": "off/low/medium/high/max"},
	"glm-5v-turbo":      {"reasoning": "off/low/medium/high/max", "note": "视觉模型"},
	"minimax-m3":        {"reasoning": "off/low/medium/high/max"},
	"minimax-m3-pay":    {"reasoning": "off/low/medium/high/max"},
	"kimi-k3":           {"reasoning": "off/low/medium/high/max"},
	"kimi-k2.7":         {"reasoning": "off/low/medium/high/max"},
	"kimi-k2.6":         {"reasoning": "off/low/medium/high/max"},
	"kimi-k2.5":         {"reasoning": "off/low/medium/high/max"},
	"deepseek-v4-pro":   {"maxInput": 1048576, "reasoning": "low/medium/high/max", "note": "上下文实测 1M"},
	"deepseek-v4-flash": {"maxInput": 1048576, "reasoning": "low/medium/high/max", "note": "思考深：思考消耗输出预算，max_tokens 建议 ≥8000"},
	"deepseek-v3.2":     {"reasoning": "low/medium/high/max"},
}

func (s *Server) handleModelInfo(w http.ResponseWriter, r *http.Request) {
	out := make([]map[string]any, 0, len(modelCandidates))
	for _, m := range s.Models(r.Context()) {
		// 未命中 modelMeta 的模型不硬造字段：空 meta 输出，前端如实显示「未核实」。
		meta := modelMeta[m]
		entry := map[string]any{"name": m}
		for k, v := range meta {
			entry[k] = v
		}
		if entry["note"] == nil {
			entry["note"] = ""
		}
		out = append(out, entry)
	}
	writeJSON(w, out)
}

// ============ /v1/chat/completions ============

// passthroughBodyKeys 后端接受的标准字段（与 Python 版一致）。
var passthroughBodyKeys = []string{
	"model", "messages", "tools", "tool_choice", "temperature",
	"max_tokens", "max_completion_tokens", "top_p", "stream",
	"stream_options", "stop", "presence_penalty", "frequency_penalty",
	"n", "response_format", "seed", "user", "reasoning_effort",
	"verbosity", "reasoning_summary",
}

// ============ hy4-preview 限流自动故障转移（第 43 轮） ============
// 用户裁决：hy4-preview 撞上游配额 429 时，代理层自动切换 fallback 并按
// 上游报文里的重置时点计时，到期自动恢复——agent 配置零改动，用户无感。
// 识别当次请求立即用 fallback 重放，客户端拿到的是成功响应（子智能体不死）。
// hy4 兜底可在 GUI 关闭（429 如实透传）、fallback 模型可改（/v1/failover，
// 落盘 codebuddy-failover.json）；hy4Fallback 常量只作缺省值。

const (
	hy4Primary  = "hy4-preview"
	hy4Fallback = "deepseek-v4-pro"
)

// hy4Limited 报告 hy4-preview 是否处于限流窗口；到期即自动解除（懒检查，
// 无后台定时器——解除后的第一个请求自然回到 hy4-preview）。
func (s *Server) hy4Limited() bool {
	s.limitMu.Lock()
	defer s.limitMu.Unlock()
	if s.limitUntil.IsZero() {
		return false
	}
	if time.Now().After(s.limitUntil) {
		log.Printf("[codebuddy] hy4-preview 限流窗口到期，自动恢复")
		s.limitUntil = time.Time{}
		return false
	}
	return true
}

// hy4Trip 记录一次限流窗口（重置时点来自上游报文）。
func (s *Server) hy4Trip(until time.Time) {
	s.limitMu.Lock()
	defer s.limitMu.Unlock()
	s.limitUntil = until
}

// resetTimeRe 上游 429 报文里的重置时点：「将在 2026-09-02 06:20:02 UTC+8 重置」。
var resetTimeRe = regexp.MustCompile(`(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})`)

// parseQuotaReset 从上游 429 报文提取限流重置时间（上游报 UTC+8，按固定东八区
// 解析）。非配额类 429 或解析不出时点返回零值（调用方不触发故障转移，如实透传）。
func parseQuotaReset(body string) time.Time {
	if !strings.Contains(body, "Quota exceeded") && !strings.Contains(body, "频率限制") {
		return time.Time{}
	}
	m := resetTimeRe.FindStringSubmatch(body)
	if m == nil {
		return time.Time{}
	}
	loc := time.FixedZone("UTC+8", 8*3600)
	t, err := time.ParseInLocation("2006-01-02 15:04:05", m[1], loc)
	if err != nil {
		return time.Time{}
	}
	return t
}

func (s *Server) handleChat(w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(io.LimitReader(r.Body, 64<<20))
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err != nil {
		writeErr(w, 400, "bad json: "+err.Error())
		return
	}
	msgs, _ := payload["messages"].([]any)
	if len(msgs) == 0 {
		writeErr(w, 400, "messages is required")
		return
	}
	modelName, _ := payload["model"].(string)
	if modelName == "" {
		modelName = "auto"
	}
	clientWantsStream, _ := payload["stream"].(bool)

	// 构造后端 body：只透传已知合法字段；后端只支持流式
	backendBody := map[string]any{}
	for _, k := range passthroughBodyKeys {
		if v, ok := payload[k]; ok {
			backendBody[k] = v
		}
	}
	if _, ok := backendBody["model"]; !ok {
		backendBody["model"] = "auto"
	}
	backendBody["stream"] = true
	if _, ok := backendBody["stream_options"]; !ok {
		backendBody["stream_options"] = map[string]any{"include_usage": true}
	}
	if s.desensitize {
		backendBody = DesensitizeBody(backendBody,
			[]string{"system", "developer"}, true, true, true)
	}

	hdr, err := s.cred.Headers(r.Context())
	if err != nil {
		writeErr(w, 502, err.Error())
		return
	}
	// 限流窗口内 hy4-preview 请求体层改写 fallback（agent 零感知）；
	// 兜底关闭（GUI 可关）时不改写、不 trip，429 如实透传（现行为）
	cfg := s.failoverConfig()
	sentModel := modelName
	limited := sentModel == hy4Primary && s.hy4Limited()
	if d := failoverDecide(cfg, sentModel, limited, 0, ""); d.rewrite {
		backendBody["model"] = cfg.fallback
		sentModel = cfg.fallback
		log.Printf("[codebuddy] hy4-preview 限流中，本次切换 %s", cfg.fallback)
	}
	start := time.Now()
	var resp *http.Response
	for attempt := 0; ; attempt++ {
		req, err := http.NewRequestWithContext(r.Context(), http.MethodPost,
			backendBase+"/v2/chat/completions", mustJSONReader(backendBody))
		if err != nil {
			writeErr(w, 500, err.Error())
			return
		}
		req.Header = hdr
		resp, err = s.client.Do(req)
		if err != nil {
			log.Printf("[codebuddy] chat model=%s err=%v", sentModel, err)
			writeErr(w, 502, "upstream error: "+err.Error())
			return
		}
		if resp.StatusCode == http.StatusOK {
			break
		}
		errBody, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		resp.Body.Close()
		log.Printf("[codebuddy] chat model=%s status=%d err=%s", sentModel, resp.StatusCode, truncate(string(errBody), 200))
		// hy4-preview 撞配额 429：兜底开启时记限流窗口并立刻用 fallback 重放本次
		// 请求（仅首跳触发，重放自身失败如实透传，不循环）；fallback 请求不触发；
		// 兜底关闭 → 不记窗口不重放，429 如实透传
		if attempt == 0 && resp.StatusCode == http.StatusTooManyRequests {
			if d := failoverDecide(cfg, sentModel, false, resp.StatusCode, string(errBody)); d.trip {
				s.hy4Trip(d.until)
				backendBody["model"] = cfg.fallback
				sentModel = cfg.fallback
				log.Printf("[codebuddy] hy4-preview 限流至 %s，切换 %s 并重放本次请求",
					d.until.Format("01-02 15:04"), cfg.fallback)
				continue
			}
		}
		writeErr(w, resp.StatusCode, string(errBody))
		return
	}
	defer resp.Body.Close()

	if clientWantsStream {
		// 流式：SSE 原样转发（后端已是标准 OpenAI SSE）；
		// 同时行扫描 data: 行提取 usage 入账（消耗 TOP 统计）
		w.Header().Set("Content-Type", "text/event-stream")
		w.Header().Set("Cache-Control", "no-cache")
		w.Header().Set("X-Accel-Buffering", "no")
		flusher, _ := w.(http.Flusher)
		scan := newUsageScanner(sentModel, s)
		buf := make([]byte, 32*1024)
		for {
			n, rerr := resp.Body.Read(buf)
			if n > 0 {
				scan.feed(buf[:n])
				if _, werr := w.Write(buf[:n]); werr != nil {
					return
				}
				if flusher != nil {
					flusher.Flush()
				}
			}
			if rerr != nil {
				return
			}
		}
	}

	// 非流式：聚合后端 SSE 为单个 chat.completion（usage 顺带入账）
	result := collectStream(resp.Body, sentModel)
	log.Printf("[codebuddy] chat model=%s status=200 dur=%s finish=%v", sentModel,
		time.Since(start).Round(time.Millisecond), result["finish_reason_debug"])
	if usage, ok := result["usage"].(map[string]any); ok {
		s.addStat(sentModel, toInt(usage["prompt_tokens"]), toInt(usage["completion_tokens"]), toInt(usage["total_tokens"]))
	}
	writeJSON(w, result)
}

// toInt 宽松取数（JSON 数字是 float64）。
func toInt(v any) int64 {
	if f, ok := v.(float64); ok {
		return int64(f)
	}
	return 0
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

func mustJSONReader(v any) io.Reader {
	b, _ := json.Marshal(v)
	return bytes.NewReader(b)
}

// collectStream 消费 OpenAI SSE 流，聚合为单个非流式 chat.completion 对象
// （合并 content/tool_calls 分片，取 usage/finish_reason）。
func collectStream(body io.Reader, modelName string) map[string]any {
	var contentParts []string
	type toolSlot struct {
		id, name, args string
	}
	toolCalls := map[int]*toolSlot{}
	var order []int
	var model, finish string
	var usage map[string]any

	sc := bufio.NewScanner(body)
	sc.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		data := strings.TrimSpace(line[5:])
		if data == "[DONE]" {
			break
		}
		var chunk map[string]any
		if json.Unmarshal([]byte(data), &chunk) != nil {
			continue
		}
		if m, ok := chunk["model"].(string); ok && m != "" {
			model = m
		}
		if u, ok := chunk["usage"].(map[string]any); ok {
			usage = u
		}
		choices, _ := chunk["choices"].([]any)
		for _, c := range choices {
			cm, ok := c.(map[string]any)
			if !ok {
				continue
			}
			if fr, ok := cm["finish_reason"].(string); ok && fr != "" {
				finish = fr
			}
			delta, _ := cm["delta"].(map[string]any)
			if delta == nil {
				continue
			}
			if ctext, ok := delta["content"].(string); ok && ctext != "" {
				contentParts = append(contentParts, ctext)
			}
			if tcs, ok := delta["tool_calls"].([]any); ok {
				for _, t := range tcs {
					tm, ok := t.(map[string]any)
					if !ok {
						continue
					}
					idx := 0
					switch v := tm["index"].(type) {
					case float64:
						idx = int(v)
					}
					slot := toolCalls[idx]
					if slot == nil {
						slot = &toolSlot{}
						toolCalls[idx] = slot
						order = append(order, idx)
					}
					if id, ok := tm["id"].(string); ok && id != "" {
						slot.id = id
					}
					if fn, ok := tm["function"].(map[string]any); ok {
						if n, ok := fn["name"].(string); ok && n != "" {
							slot.name = n
						}
						if a, ok := fn["arguments"].(string); ok {
							slot.args += a
						}
					}
				}
			}
		}
	}

	message := map[string]any{"role": "assistant"}
	content := strings.Join(contentParts, "")
	if content != "" {
		message["content"] = content
	} else {
		message["content"] = nil
	}
	finReason := finish
	if len(toolCalls) > 0 {
		tcs := make([]any, 0, len(order))
		for _, idx := range order {
			slot := toolCalls[idx]
			tcs = append(tcs, map[string]any{
				"id": slot.id, "type": "function",
				"function": map[string]any{"name": slot.name, "arguments": slot.args},
			})
		}
		message["tool_calls"] = tcs
		if finReason == "" {
			finReason = "tool_calls"
		}
	}
	if finReason == "" {
		finReason = "stop"
	}
	rid := make([]byte, 6)
	_, _ = rand.Read(rid)
	if usage == nil {
		usage = map[string]any{"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
	}
	return map[string]any{
		"id":                 "chatcmpl-" + hex.EncodeToString(rid),
		"object":             "chat.completion",
		"created":            time.Now().Unix(),
		"model":              orDefault(model, modelName),
		"choices":            []any{map[string]any{"index": 0, "message": message, "finish_reason": finReason}},
		"usage":              usage,
		"finish_reason_debug": finReason,
	}
}

func orDefault(s, def string) string {
	if s == "" {
		return def
	}
	return s
}

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, status int, msg string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{
		"error": map[string]any{"message": msg, "type": "upstream_error"},
	})
}
