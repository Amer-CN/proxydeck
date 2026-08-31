// baseline.go —— 绝对基准库（第三层）：按模型名存官方基准（管道探针值 +
// 355 维分布 + 统计），检测时逐探针比对 + 分布相似度，输出综合灯色。
// 存储 tuanjie-baselines.json（model 名 → 基准；
// 以最新为准）。
package tuanjie

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Baseline 单模型的官方基准：第一层探针值 + 第二层分布。
type Baseline struct {
	Model       string          `json:"model"`
	Channel     string          `json:"channel,omitempty"` // 所属渠道（tuanjie/command/workbuddy/bai）
	Probes      []probeResult   `json:"probes"`      // tokenizer 4 值 / 错误文本 2 / finish 2
	Dist        *distResult     `json:"dist,omitempty"` // 355 维分布 + 统计（不足 40 样本时缺省）
	SampledAt   string          `json:"sampled_at"`
	Account     string          `json:"account"`
	SampleCount int             `json:"sample_count"`
}

// BaselineStore 基准库（"渠道|模型" → 基准）。
type BaselineStore struct {
	mu        sync.Mutex
	path      string
	Baselines map[string]*Baseline `json:"baselines"`
}

func baselinesFilePath() string {
	return filepath.Join(exeDirForAccounts(), "tuanjie-baselines.json")
}

// LoadBaselines 从磁盘恢复（缺失 = 空库）。旧版键为裸模型名（"GLM-5.3"），
// 加载时自动迁移为渠道前缀键（"tuanjie|GLM-5.3"）并落盘一次。
func LoadBaselines() *BaselineStore {
	bs := &BaselineStore{path: baselinesFilePath(), Baselines: map[string]*Baseline{}}
	if b, err := os.ReadFile(bs.path); err == nil {
		_ = json.Unmarshal(b, bs)
		if bs.Baselines == nil {
			bs.Baselines = map[string]*Baseline{}
		}
		migrated := false
		for k, v := range bs.Baselines {
			if !strings.Contains(k, "|") {
				bs.Baselines["tuanjie|"+k] = v
				delete(bs.Baselines, k)
				migrated = true
			}
		}
		if migrated {
			bs.save()
		}
	}
	return bs
}

func (bs *BaselineStore) save() {
	b, err := json.MarshalIndent(bs, "", "  ")
	if err != nil {
		return
	}
	_ = os.WriteFile(bs.path, b, 0o600)
}

// Get 返回渠道+模型的基准（无基准返回 nil）。
func (bs *BaselineStore) Get(channel, model string) *Baseline {
	bs.mu.Lock()
	defer bs.mu.Unlock()
	return bs.Baselines[channel+"|"+model]
}

// CollectBaseline 采集基准：跑第一层全部探针 + 第二层分布采样（N 默认 60），
// 落盘为该渠道+模型的官方基准（以最新一次为准）。
func (bs *BaselineStore) CollectBaseline(ctx context.Context, target *probeTarget, channel, model, account string, n int) (*Baseline, error) {
	probes := RunPipelineProbes(ctx, target, model)
	dist := collectDistSamples(ctx, target, model, n)
	bl := &Baseline{
		Model:       model,
		Channel:     channel,
		Probes:      probes,
		Dist:        dist,
		SampledAt:   time.Now().Format("2006-01-02 15:04:05"),
		Account:     account,
		SampleCount: dist.Valid,
	}
	bs.mu.Lock()
	bs.Baselines[channel+"|"+model] = bl
	bs.save()
	bs.mu.Unlock()
	return bl, nil
}

// probeCompare 单探针比对结果。
type probeCompare struct {
	Name       string `json:"name"`
	Current    string `json:"current"`
	Baseline   string `json:"baseline"`
	Match      bool   `json:"match"`
	Status     string `json:"status"` // ok | mismatch | unstable | error（当前探针状态）
	Note       string `json:"note,omitempty"`
}

// overallVerdict 综合判定：一致（green）/ 轻微偏差（yellow）/
// 显著偏差（red=注水嫌疑）/ 无基准（grey=先跑 baseline）。
type overallVerdict struct {
	Light       string `json:"light"`  // green | yellow | red | grey
	Label       string `json:"label"`
	Score       float64 `json:"score"` // 综合相似度百分比
	Reason      string `json:"reason"`
}

// tokenizerDriftTolerance 归一化 token 数比对容差：宿主隐藏模板实测会整批
// 平移（同轮 4 探针同时 ±8），±8 内视为同分词器（差值比对，非绝对相等）。
const tokenizerDriftTolerance = 8

// shortProbeTolerance 短字符探针（len≤4）允许的最大字符位差（±1）：上游模板
// 微抖时 2/4 字符探针（错误文本/finish 系）单字符位漂移不再误报 mismatch；
// 「a」单字符探针 v3.6.5 已做 3 次中位数，此处补齐其余短探针。数值型探针不受影响。
const shortProbeTolerance = 1

// probeValueMatch 探针值比对规则：tokenizer 系按差值（≤8 容差，隐藏模板
// 平移不误报）；错误文本/finish_reason 系按相等性（报错原文与完停词是
// 逐字符指纹），len≤4 的等长短值允许 1 个字符位差（双方均为数值时不做
// 短值容差，仍按严格相等）。
func probeValueMatch(name, cur, base string) bool {
	if strings.HasPrefix(name, "tokenizer_") {
		ci, err1 := strconv.Atoi(cur)
		bi, err2 := strconv.Atoi(base)
		if err1 == nil && err2 == nil {
			d := ci - bi
			if d < 0 {
				d = -d
			}
			return d <= tokenizerDriftTolerance
		}
	}
	if cur == base {
		return true
	}
	// 短字符探针 ±1 容差：len≤4、等长且双方均非数值，允许 1 个字符位差
	if len(cur) <= 4 && len(base) <= 4 && len(cur) == len(base) &&
		!isNumericStr(cur) && !isNumericStr(base) {
		diff := 0
		for i := 0; i < len(cur); i++ {
			if cur[i] != base[i] {
				diff++
				if diff > shortProbeTolerance {
					return false
				}
			}
		}
		return true
	}
	return false
}

// isNumericStr 字符串是否可解析为数字（数值型探针不做短值容差）。
func isNumericStr(s string) bool {
	_, err := strconv.Atoi(s)
	return err == nil
}

// CompareToBaseline 当前探针集 + 分布 vs 基准：
//   - 探针比对：tokenizer 4 值差值（≤8 容差 match——同模型两轮可能整批平移，
//     错误文本 2 相等性；finish 2 相等性
//   - 分布：distribScore/modeScore 加权 overall → 档位
//   - 综合：探针 mismatch 数与分布档位合成灯色
func CompareToBaseline(base *Baseline, probes []probeResult, dist *distResult) ([]probeCompare, distSimilarity, overallVerdict) {
	// 无基准：灰灯引导（先采基准），直接返回
	if base == nil {
		v := overallVerdict{Light: "grey", Label: "无基准",
			Reason: "该模型无基准，先采集基准（action=baseline），检测才有绝对锚点"}
		return nil, distSimilarity{}, v
	}
	// 基准探针索引
	baseIdx := map[string]probeResult{}
	for _, p := range base.Probes {
		baseIdx[p.Name] = p
	}
	var cmps []probeCompare
	mismatch, unstableCount, comparable := 0, 0, 0
	for _, cur := range probes {
		c := probeCompare{Name: cur.Name, Current: cur.Value, Baseline: "", Status: cur.Status, Note: cur.Note}
		if bp, ok := baseIdx[cur.Name]; ok {
			c.Baseline = bp.Value
			if cur.Status == "ok" && bp.Status == "ok" {
				comparable++
				c.Match = probeValueMatch(cur.Name, cur.Value, bp.Value)
				if !c.Match {
					mismatch++
				}
			} else {
				unstableCount++
			}
		} else {
			unstableCount++
		}
		cmps = append(cmps, c)
	}

	// 分布相似度（双方都足样本才算）
	var sim distSimilarity
	distReady := false
	if base.Dist != nil && !base.Dist.Insufficient && dist != nil && !dist.Insufficient {
		sim = CompareDist(dist.Counts, base.Dist.Counts, dist.Valid, base.Dist.Valid)
		distReady = true
	}

	// 综合判定
	v := overallVerdict{Light: "grey", Label: "无基准"}
	if !distReady {
		// 分布不可比（样本不足或基准没采分布）：只看探针
		switch {
		case comparable == 0:
			v.Light, v.Label = "grey", "样本不足"
			v.Reason = "探针无可比对值（unstable/error 过多），建议重新检测"
		case mismatch == 0:
			v.Light, v.Label, v.Score = "green", "一致", 100
			v.Reason = "管道探针全部匹配（分布不可比：样本不足）"
		case mismatch <= 1:
			v.Light, v.Label, v.Score = "yellow", "轻微偏差", 90
			v.Reason = "1 个探针不匹配（分布不可比：样本不足）"
		default:
			v.Light, v.Label, v.Score = "red", "显著偏差", 60
			v.Reason = "多个探针不匹配（分布不可比：样本不足）"
		}
		return cmps, sim, v
	}
	pct, verdict := distVerdict(sim)
	v.Score = pct
	// 探针 mismatch 数叠加：0=沿用分布档；1 个升黄；≥2 或分布显著偏差→红
	switch {
	case mismatch == 0 && pct >= 96:
		v.Light, v.Label = "green", "一致"
		v.Reason = "管道探针全部匹配，分布相似度 " + formatPct(pct)
	case mismatch == 0 && pct >= 90:
		v.Light, v.Label = "yellow", "轻微偏差"
		v.Reason = "探针匹配，但分布相似度 " + formatPct(pct) + "（" + verdict + "）"
	case mismatch >= 2:
		v.Light, v.Label = "red", "显著偏差"
		v.Reason = "管道探针 " + itoa(mismatch) + " 项不匹配（注水嫌疑）；分布相似度 " + formatPct(pct)
	case pct >= 90:
		v.Light, v.Label = "yellow", "轻微偏差"
		v.Reason = "1 个探针不匹配或分布相似度 " + formatPct(pct)
	default:
		v.Light, v.Label = "red", "显著偏差"
		v.Reason = "分布相似度仅 " + formatPct(pct) + "（" + verdict + "，注水嫌疑）"
	}
	if unstableCount > 0 {
		v.Reason += "；" + itoa(unstableCount) + " 项探针 unstable 未计入比对"
	}
	return cmps, sim, v
}

func formatPct(p float64) string { return strconv.Itoa(int(p)) + "%" }
func itoa(n int) string          { return strconv.Itoa(n) }
