// dist.go —— 随机数分布指纹（第二层，学 hlwy-ai-checker）：固定 prompt
// 让模型从 1..355 随机选数，采样 N 次，比对 355 维频率分布与基准。
// 厂商的采样实现/隐藏模板会让分布带"家庭指纹"——换模型必现偏差。
package tuanjie

import (
	"context"
	"fmt"
	"log"
	"math"
	"sort"
	"strconv"
	"strings"
	"sync"
)

// distBuckets 分布桶数（1..355，hlwy 口径）。
const distBuckets = 355

// distPrompt 固定采样 prompt（不动！基准依赖逐字节稳定）。
const distPrompt = "请从1到355之间随机选择一个数字，只输出这个数字，不要有任何其他内容。"

// distMaxSamples 采样上限（hlwy 默认 200，代理场景保守取 60 默认/200 上限）。
const distMaxSamples = 200

// distStats 分布统计摘要。
type distStats struct {
	Mean   float64 `json:"mean"`
	Median float64 `json:"median"`
	StdDev float64 `json:"std_dev"`
	Mode   int     `json:"mode"`
	ModeKw int     `json:"mode_count"`
}

// distResult 一次分布采样的结果：355 维频率（counts）+ 统计 + 样本数。
type distResult struct {
	Counts      []int     `json:"counts"`      // 1..355 各数字出现次数
	Valid       int       `json:"valid"`       // 有效样本数（解析出 1..355 的）
	Invalid     int       `json:"invalid"`     // 无效样本数（越界/非数字）
	Stats       distStats `json:"stats"`
	Insufficient bool     `json:"insufficient"` // 有效样本 < 40
}

// distSimilarity 两分布的相似度得分：余弦 + JS 散度合成 distribScore、
// 众数分 modeScore，overall = 两者 0.5 加权。
type distSimilarity struct {
	Cosine      float64 `json:"cosine"`
	JSDiv       float64 `json:"js_divergence"`
	DistribScore float64 `json:"distrib_score"` // cos·exp(−jsDiv)
	ModeA       int     `json:"mode_a"`
	ModeB       int     `json:"mode_b"`
	ModeScore   float64 `json:"mode_score"` // 1−|diff|/50，相等=1.0
	Overall     float64 `json:"overall"`    // 0.5·distrib + 0.5·mode
}

// parseDistAnswer 解析模型回答为 1..355 的数字（剥空白/标点；越界或非数字
// 返回 0 = 无效样本）。
func parseDistAnswer(s string) int {
	s = strings.TrimSpace(s)
	s = strings.Trim(s, "。，.,、 \n\t\"'")
	if s == "" {
		return 0
	}
	// 只取首个连续数字段（模型偶尔带前缀文字）
	start, end := -1, -1
	for i, r := range s {
		if r >= '0' && r <= '9' {
			if start < 0 {
				start = i
			}
			end = i + 1
		} else if start >= 0 {
			break
		}
	}
	if start < 0 {
		return 0
	}
	// 前导 '-' 视为负数（越界无效）
	if start > 0 && s[start-1] == '-' {
		return 0
	}
	n, err := strconv.Atoi(s[start:end])
	if err != nil || n < 1 || n > distBuckets {
		return 0
	}
	return n
}

// collectDistSamples 并发采样（并发克制：concurrency 3，别把账号打出限流）。
// 探针裸发（无 system prompt），temperature=1.0。max_tokens 取 96 而非
// hlwy 的 10：GLM-5.3 等思考型模型推理段常吃掉几十 token，10 会把预算全烧在
// reasoning_content 上、content 恒空（实测 64 出数字率约 1/2，96 约 7/8）。
func collectDistSamples(ctx context.Context, cliKey, model string, n int) *distResult {
	if n < 1 {
		n = 1
	}
	if n > distMaxSamples {
		n = distMaxSamples
	}
	counts := make([]int, distBuckets+1) // 下标 1..355
	valid, invalid := 0, 0
	var mu sync.Mutex
	sem := make(chan struct{}, 3) // 并发 3
	var wg sync.WaitGroup
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			sem <- struct{}{}
			defer func() { <-sem }()
			_, _, _, content, _, e := probeCall(ctx, cliKey, model,
				[]map[string]any{{"role": "user", "content": distPrompt}},
				map[string]any{"temperature": 1.0, "max_tokens": 96})
			if e != nil {
				mu.Lock()
				invalid++
				log.Printf("[tuanjie] dist 采样失败: %v", e)
				mu.Unlock()
				return
			}
			v := parseDistAnswer(content)
			mu.Lock()
			defer mu.Unlock()
			if v == 0 {
				invalid++
				log.Printf("[tuanjie] dist 样本无效: content=%q", content)
				return
			}
			counts[v]++
			valid++
		}()
	}
	wg.Wait()
	res := &distResult{Counts: counts[1:], Valid: valid, Invalid: invalid}
	res.Stats = distStatsOf(counts)
	res.Insufficient = valid < 40
	return res
}

// distStatsOf 从 counts（下标 1..355）算 mean/median/stdDev/mode。
func distStatsOf(counts []int) distStats {
	total, sum := 0, 0.0
	mode, modeKw := 0, 0
	for v := 1; v <= distBuckets; v++ {
		c := counts[v]
		if c == 0 {
			continue
		}
		total += c
		sum += float64(v * c)
		if c > modeKw {
			mode, modeKw = v, c
		}
	}
	if total == 0 {
		return distStats{}
	}
	mean := sum / float64(total)

	// median：按次数展开后取中位（样本量 ≤200，展开无压力）
	expanded := make([]int, 0, total)
	for v := 1; v <= distBuckets; v++ {
		for j := 0; j < counts[v]; j++ {
			expanded = append(expanded, v)
		}
	}
	sort.Ints(expanded)
	median := 0.0
	if len(expanded)%2 == 1 {
		median = float64(expanded[len(expanded)/2])
	} else {
		median = float64(expanded[len(expanded)/2-1]+expanded[len(expanded)/2]) / 2
	}

	// stdDev（总体方差）
	varSq := 0.0
	for v := 1; v <= distBuckets; v++ {
		if counts[v] == 0 {
			continue
		}
		d := float64(v) - mean
		varSq += float64(counts[v]) * d * d
	}
	stdDev := math.Sqrt(varSq / float64(total))
	return distStats{Mean: round2(mean), Median: round2(median), StdDev: round2(stdDev), Mode: mode, ModeKw: modeKw}
}

// CompareDist 当前分布 vs 基准分布的相似度（两边都需 ≥40 有效样本，
// 否则各分置 0，由调用方按 insufficient 处理）。
func CompareDist(a, b []int, validA, validB int) distSimilarity {
	s := distSimilarity{}
	if validA < 40 || validB < 40 || len(a) != distBuckets || len(b) != distBuckets {
		return s
	}
	// 余弦相似度（355 维频率向量）
	var dot, na, nb float64
	for i := 0; i < distBuckets; i++ {
		x, y := float64(a[i]), float64(b[i])
		dot += x * y
		na += x * x
		nb += y * y
	}
	if na > 0 && nb > 0 {
		s.Cosine = round4(dot / (math.Sqrt(na) * math.Sqrt(nb)))
	}
	// JS 散度（概率分布，m=(P+Q)/2；零概率项按 0·log0=0 处理）
	ta, tb := float64(validA), float64(validB)
	jsDiv := 0.0
	for i := 0; i < distBuckets; i++ {
		p := float64(a[i]) / ta
		q := float64(b[i]) / tb
		m := (p + q) / 2
		if p > 0 {
			jsDiv += p * math.Log2(p/m)
		}
		if q > 0 {
			jsDiv += q * math.Log2(q/m)
		}
	}
	jsDiv /= 2
	s.JSDiv = round4(jsDiv)
	s.DistribScore = round4(s.Cosine * math.Exp(-jsDiv))

	// 众数分：1−|diff|/50，相等=1.0（负值截 0）
	statsA := distStatsOf(padCounts(a))
	statsB := distStatsOf(padCounts(b))
	s.ModeA, s.ModeB = statsA.Mode, statsB.Mode
	modeScore := 1 - float64(abs(s.ModeA-s.ModeB))/50
	if modeScore < 0 {
		modeScore = 0
	}
	s.ModeScore = round4(modeScore)
	s.Overall = round4(0.5*s.DistribScore + 0.5*s.ModeScore)
	return s
}

// padCounts counts[0..354] → 下标 1..355 的完整数组（复用 distStatsOf）。
func padCounts(c []int) []int {
	out := make([]int, distBuckets+1)
	copy(out[1:], c)
	return out
}

func abs(x int) int {
	if x < 0 {
		return -x
	}
	return x
}

func round2(v float64) float64  { return math.Round(v*100) / 100 }
func round4(v float64) float64  { return math.Round(v*10000) / 10000 }

// distVerdict 相似度 → 判定档（≥96% 一致 / ≥90% 轻微偏差 / <90% 显著偏差）。
func distVerdict(sim distSimilarity) (pct float64, verdict string) {
	pct = math.Round(sim.Overall * 100)
	switch {
	case pct >= 96:
		return pct, "一致"
	case pct >= 90:
		return pct, "轻微偏差"
	default:
		return pct, "显著偏差"
	}
}

// distVerdictLabel 判定档 → 灯色（green/yellow/red；grey 由无基准场景单独给）。
func distVerdictLabel(verdict string) string {
	switch verdict {
	case "一致":
		return "green"
	case "轻微偏差":
		return "yellow"
	default:
		return "red"
	}
}

// fmtDistSampleProgress 采样进度文案（前端轮询展示用）。
func fmtDistSampleProgress(done, total int) string {
	return fmt.Sprintf("分布采样 %d/%d", done, total)
}
