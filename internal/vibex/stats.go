// stats.go —— 本地消耗统计：GET /v1/stats + vibex-stats.json 落盘
// （照抄 internal/bai 的四件套：map+mutex / 完成时入账 / 原子落盘 / 路由）。
package vibex

import (
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"time"
)

// modelStat 单模型用量累计（字段名与 bai/tuanjie/codebuddy 同形，GUI 共用一套渲染）。
type modelStat struct {
	Calls     int64 `json:"calls"`
	InputTok  int64 `json:"inputTokens"`
	OutputTok int64 `json:"outputTokens"`
	TotalTok  int64 `json:"totalTokens"`
}

// statsFilePath 统计文件路径：exe 同目录（同 bai-stats.json 的落盘口径）。
func statsFilePath() string { return filepath.Join(exeDir(), "vibex-stats.json") }

// handleStats 返回消耗统计 + 运行时长（GUI 消耗 TOP 用，结构与 B.AI 一字对齐）。
// 统计文件缺失/损坏时 stats 是空表，仍 200（不 500）。
func (s *Server) handleStats(w http.ResponseWriter, r *http.Request) {
	s.statsMu.Lock()
	out := make(map[string]*modelStat, len(s.stats))
	for k, v := range s.stats {
		cp := *v
		out[k] = &cp
	}
	s.statsMu.Unlock()
	writeJSON(w, http.StatusOK, map[string]any{
		"models":    out,
		"uptimeSec": int64(time.Since(s.startedAt).Seconds()),
	})
}

// recordUsage 把一轮 result 事件的 usage 真值入账（nil / 无字段即 0，addStat 自己跳过）。
// 口径与响应体 usage 一致：total = input + output（Claude usage 无 total 字段）。
func (s *Server) recordUsage(model string, usage map[string]any) {
	if usage == nil {
		return
	}
	in, out := int64(intOf(usage["input_tokens"])), int64(intOf(usage["output_tokens"]))
	s.addStat(model, in, out, in+out)
}

// addStat 累计一次调用的用量并落盘。
// 三个计数全 0（本轮没有 result 真 usage）时直接不入账——不估算，避免脏数据。
func (s *Server) addStat(model string, in, out, total int64) {
	if total == 0 && in == 0 && out == 0 {
		return
	}
	s.statsMu.Lock()
	if s.stats == nil {
		s.stats = map[string]*modelStat{}
	}
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
	logf("stat model=%s in=%d out=%d", model, in, out)
}

// loadStats 启动时读回历史统计（文件缺失/损坏即空表，不阻断启动）。
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

// saveStats 原子落盘（tmp + rename）。
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
