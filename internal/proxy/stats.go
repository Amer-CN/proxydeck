package proxy

import (
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

// UsageStats is a thread-safe local counter of tokens that flowed through the
// proxy. It accumulates real usage reported by CommandCode in stream events
// and persists to disk so counts survive proxy restarts.
type UsageStats struct {
	mu      sync.Mutex
	file    string                `json:"-"`
	Models  map[string]*ModelStat `json:"models"`
	Started int64                 `json:"started"` // unix seconds
}

// ModelStat tracks token counts for one model.
type ModelStat struct {
	Runs             int64 `json:"runs"`
	InputTokens      int64 `json:"inputTokens"`
	OutputTokens     int64 `json:"outputTokens"`
	CacheReadTokens  int64 `json:"cacheReadTokens"`
	CacheWriteTokens int64 `json:"cacheWriteTokens"`
	// Per-day breakdown, keyed by YYYY-MM-DD (local time).
	Days map[string]*DayStat `json:"days,omitempty"`
}

// DayStat tracks one calendar day's usage.
type DayStat struct {
	InputTokens      int64 `json:"inputTokens"`
	OutputTokens     int64 `json:"outputTokens"`
	CacheReadTokens  int64 `json:"cacheReadTokens"`
	CacheWriteTokens int64 `json:"cacheWriteTokens"`
}

func NewUsageStats(file string) *UsageStats {
	s := &UsageStats{Models: map[string]*ModelStat{}, file: file}
	s.load()
	return s
}

// dayKey returns today's key in local time, e.g. "2026-08-06".
func dayKey() string {
	return time.Now().Format("2006-01-02")
}

// Record adds one completed run's usage for a model.
func (s *UsageStats) Record(model string, input, output, cacheRead, cacheWrite int64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	ms := s.Models[model]
	if ms == nil {
		ms = &ModelStat{}
		s.Models[model] = ms
	}
	ms.Runs++
	ms.InputTokens += input
	ms.OutputTokens += output
	ms.CacheReadTokens += cacheRead
	ms.CacheWriteTokens += cacheWrite
	if ms.Days == nil {
		ms.Days = map[string]*DayStat{}
	}
	dk := dayKey()
	ds := ms.Days[dk]
	if ds == nil {
		ds = &DayStat{}
		ms.Days[dk] = ds
	}
	ds.InputTokens += input
	ds.OutputTokens += output
	ds.CacheReadTokens += cacheRead
	ds.CacheWriteTokens += cacheWrite
	s.save()
}

// Today returns aggregate usage for today across all models.
func (s *UsageStats) Today() (in, out, cacheRead, cacheWrite int64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	dk := dayKey()
	for _, ms := range s.Models {
		if ds := ms.Days[dk]; ds != nil {
			in += ds.InputTokens
			out += ds.OutputTokens
			cacheRead += ds.CacheReadTokens
			cacheWrite += ds.CacheWriteTokens
		}
	}
	return
}

// Snapshot returns a copy for JSON output.
func (s *UsageStats) Snapshot() *UsageStats {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := &UsageStats{Started: s.Started, Models: map[string]*ModelStat{}}
	for k, v := range s.Models {
		c := *v
		if v.Days != nil {
			c.Days = map[string]*DayStat{}
			for dk, dv := range v.Days {
				d := *dv
				c.Days[dk] = &d
			}
		}
		out.Models[k] = &c
	}
	return out
}

// load reads persisted stats from disk, if present.
func (s *UsageStats) load() {
	if s.file == "" {
		return
	}
	data, err := os.ReadFile(s.file)
	if err != nil {
		return
	}
	_ = json.Unmarshal(data, s)
	if s.Models == nil {
		s.Models = map[string]*ModelStat{}
	}
	if s.Started == 0 { // 首次运行（或旧数据无 started）：记录统计起点
		s.Started = time.Now().Unix()
		s.save()
	}
}

// save writes stats to disk atomically.
func (s *UsageStats) save() {
	if s.file == "" {
		return
	}
	data, err := json.Marshal(s)
	if err != nil {
		return
	}
	tmp := s.file + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return
	}
	_ = os.Rename(tmp, s.file)
}

// HandleStats returns accumulated local usage as JSON.
func (p *Proxy) HandleStats(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		p.writeOpenAIError(w, http.StatusMethodNotAllowed, "Method not allowed", "invalid_request_error")
		return
	}
	snap := p.Stats.Snapshot()

	// 覆盖式校准：有校准的模型，其本地总 token（含缓存命中）被官网值覆盖（以后新请求继续累加）。
	calib := p.calibration()
	calibrated := map[string]bool{}
	for name, official := range calib {
		ms, ok := snap.Models[name]
		if !ok || official <= 0 {
			continue
		}
		// 保持本地各分项比例，把总和（输入+输出+缓存读+缓存写）缩放到官网值
		sum := ms.InputTokens + ms.OutputTokens + ms.CacheReadTokens + ms.CacheWriteTokens
		if sum > 0 {
			scale := float64(official) / float64(sum)
			ms.InputTokens = int64(float64(ms.InputTokens) * scale)
			ms.OutputTokens = int64(float64(ms.OutputTokens) * scale)
			ms.CacheReadTokens = int64(float64(ms.CacheReadTokens) * scale)
			ms.CacheWriteTokens = official - ms.InputTokens - ms.OutputTokens - ms.CacheReadTokens
		} else {
			ms.InputTokens = official
		}
		calibrated[name] = true
	}

	todayIn, todayOut, todayCacheRead, todayCacheWrite := p.Stats.Today()
	var totalIn, totalOut, totalCacheRead, totalCacheWrite int64
	for _, ms := range snap.Models {
		totalIn += ms.InputTokens
		totalOut += ms.OutputTokens
		totalCacheRead += ms.CacheReadTokens
		totalCacheWrite += ms.CacheWriteTokens
	}

	// 金额估算（官方定价 per 1M tokens，USD；无价格的模型按 0 计）
	cost := estimateCost(snap.Models)

	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(map[string]any{
		"started":   snap.Started,
		"models":    snap.Models,
		"total": map[string]int64{
			"input": totalIn, "output": totalOut,
			"cacheRead": totalCacheRead, "cacheWrite": totalCacheWrite,
			"total": totalIn + totalOut + totalCacheRead + totalCacheWrite,
		},
		"today": map[string]int64{
			"input": todayIn, "output": todayOut,
			"cacheRead": todayCacheRead, "cacheWrite": todayCacheWrite,
			"total": todayIn + todayOut + todayCacheRead + todayCacheWrite,
		},
		"statsFile":  filepath.Base(p.Stats.file),
		"cost":       cost, // 估算金额（按官方单价 × 本地 token）
		"calibrated": calibrated, // 哪些模型被官网校准覆盖
	})
}

// pricing 按模型 ID（短名或全名）返回每百万 token 的输入/输出价格（USD）。
// 来源：https://commandcode.ai/docs/resources/pricing-limits（2026-08 官方定价）
var pricing = map[string][2]float64{ // [输入, 输出] per 1M tokens
	"deepseek-v4-flash":   {0.14, 0.28},
	"deepseek/deepseek-v4-flash": {0.14, 0.28},
	"deepseek-v4-pro":     {0.435, 0.87},
	"deepseek/deepseek-v4-pro":   {0.435, 0.87},
	"kimi-k2.6":           {0.95, 4.00},
	"moonshotai/kimi-k2.6": {0.95, 4.00},
	"kimi-k2.5":           {0.60, 3.00},
	"moonshotai/kimi-k2.5": {0.60, 3.00},
	"glm-5.1":             {1.40, 4.40},
	"zai-org/glm-5.1":     {1.40, 4.40},
	"glm-5":               {1.00, 3.20},
	"zai-org/glm-5":       {1.00, 3.20},
	"minimax-m3":          {0.30, 1.20},
	"minimaxai/minimax-m3": {0.30, 1.20},
	"minimax-m2.7":        {0.30, 1.20},
	"minimaxai/minimax-m2.7": {0.30, 1.20},
	"minimax-m2.5":        {0.30, 1.20},
	"minimaxai/minimax-m2.5": {0.30, 1.20},
	"qwen-3.7-max":        {2.50, 7.50},
	"qwen/qwen3.7-max":    {2.50, 7.50},
	"qwen-3.7-max-free":   {0.0, 0.0},
	"qwen/qwen3.7-max-free": {0.0, 0.0},
	"qwen-3.6-max":        {1.30, 7.80},
	"qwen/qwen3.6-max-preview": {1.30, 7.80},
	"qwen-3.6-plus":       {0.50, 3.00},
	"qwen/qwen3.6-plus":   {0.50, 3.00},
	"step-3.7-flash":      {0.20, 1.15},
	"stepfun/step-3.7-flash": {0.20, 1.15},
	"step-3.5-flash":      {0.10, 0.30},
	"stepfun/step-3.5-flash": {0.10, 0.30},
	"mimo-v2.5-pro":       {0.435, 0.87},
	"xiaomi/mimo-v2.5-pro": {0.435, 0.87},
	"mimo-v2.5":           {0.14, 0.28},
	"xiaomi/mimo-v2.5":    {0.14, 0.28},
	"gemini-3.1-flash-lite": {0.0, 0.0},
	"google/gemini-3.1-flash-lite": {0.0, 0.0},
}

// cacheReadPrices 各模型缓存命中读取每百万 token 的价格（USD）。
// 来源：官方定价页。未列出的模型按输入价的 2% 估算（deepseek-v4-flash 实际为 $0.0028/1M）。
var cacheReadPrices = map[string]float64{
	"deepseek-v4-flash": 0.0028,
}

// estimateCost 按模型价格表估算总金额（USD），缓存命中按缓存读取价计算。
// 缓存写入不单独计费（已含在输入价内）。
func estimateCost(models map[string]*ModelStat) float64 {
	var total float64
	for name, ms := range models {
		pr := pricing[name]
		if pr[0] == 0 && pr[1] == 0 {
			pr = pricing[shortName(name)]
		}
		inPrice, outPrice := pr[0], pr[1]
		cr := cacheReadPrices[name]
		if cr == 0 {
			cr = cacheReadPrices[shortName(name)]
		}
		if cr == 0 {
			cr = inPrice * 0.02 // 默认按输入价 2% 估算缓存读取
		}
		total += float64(ms.InputTokens)/1e6*inPrice +
			float64(ms.OutputTokens)/1e6*outPrice +
			float64(ms.CacheReadTokens)/1e6*cr
	}
	return total
}

// shortName 从全名（如 "deepseek/deepseek-v4-flash"）取短名。
func shortName(full string) string {
	if i := strings.LastIndex(full, "/"); i >= 0 {
		return full[i+1:]
	}
	return full
}

// calibration 读取按模型的官网 token 校准值，文件与 stats.json 同目录。
// 格式：{"模型名": 官网token数, ...}；空串=清除某模型。
func (p *Proxy) calibration() map[string]int64 {
	dir := filepath.Dir(p.Stats.file)
	if dir == "." {
		dir = "."
	}
	b, err := os.ReadFile(filepath.Join(dir, "calibration.json"))
	if err != nil {
		return map[string]int64{}
	}
	var v map[string]int64
	if err := json.Unmarshal(b, &v); err != nil || v == nil {
		return map[string]int64{}
	}
	return v
}

// setCalibration 写入某模型的官网校准 token 数；token<=0 表示清除。
func (p *Proxy) setCalibration(model string, token int64) error {
	dir := filepath.Dir(p.Stats.file)
	if dir == "." {
		dir = "."
	}
	calib := p.calibration()
	if token <= 0 {
		delete(calib, model)
	} else {
		calib[model] = token
	}
	data, err := json.Marshal(calib)
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(dir, "calibration.json"), data, 0o600)
}

// HandleCalibration 接收 GUI 传来的模型校准写入（POST model=xxx&tokens=NNN）。
func (p *Proxy) HandleCalibration(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		p.writeOpenAIError(w, http.StatusMethodNotAllowed, "Method not allowed", "invalid_request_error")
		return
	}
	model := r.FormValue("model")
	tokensStr := r.FormValue("tokens")
	if model == "" {
		p.writeOpenAIError(w, http.StatusBadRequest, "missing model", "invalid_request_error")
		return
	}
	var token int64
	if tokensStr != "" {
		n, err := strconv.ParseInt(tokensStr, 10, 64)
		if err != nil {
			p.writeOpenAIError(w, http.StatusBadRequest, "tokens must be a number", "invalid_request_error")
			return
		}
		token = n
	}
	if err := p.setCalibration(model, token); err != nil {
		p.writeOpenAIError(w, http.StatusInternalServerError, err.Error(), "server_error")
		return
	}
	w.Header().Set("Content-Type", "application/json")
	_, _ = w.Write([]byte(`{"ok":true,"msg":"校准已保存"}`))
}
