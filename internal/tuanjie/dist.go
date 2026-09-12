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
	Unanswered  int       `json:"unanswered"`  // 未作答数（空正文/上游未返回正文，加大预算重试后仍未出）
	Stats       distStats `json:"stats"`
	Insufficient bool     `json:"insufficient"` // 有效样本 < 40
	RateLimited bool      `json:"rate_limited,omitempty"` // 因渠道限流（429 累计超阈值）提前中止：采样未完成，不可用于比对/落盘
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
// 探针裸发（无 system prompt），temperature=1.0。max_tokens 首次取 96 而非
// hlwy 的 10：GLM-5.3 等思考型模型推理段常吃掉几十 token，10 会把预算全烧在
// reasoning_content 上、content 恒空（实测 64 出数字率约 1/2，96 约 7/8）。
// 空正文（finish_reason=length 且 content=""，预算全烧推理段）不再记"无效
// 样本"：自动加大预算（512）重试一次；仍空 → 计「未作答」。上游非 200
// （限流/预算拦截等）同样计「未作答」——那是没作答，不是答错。
// 限流早停（2026-09-12 回归修复）：429 累计超阈值（rateLimitStorm）时提前
// 停止投递剩余采样（已投递的 goroutine 收尾即可），结果标 RateLimited——
// 绝不逐个请求慢慢退避把检测拖成十分钟以上。
func collectDistSamples(ctx context.Context, target *probeTarget, model string, n int) *distResult {
	if n < 1 {
		n = 1
	}
	if n > distMaxSamples {
		n = distMaxSamples
	}
	counts := make([]int, distBuckets+1) // 下标 1..355
	valid, invalid, unanswered := 0, 0, 0
	var mu sync.Mutex
	sem := make(chan struct{}, 3) // 并发 3
	var wg sync.WaitGroup
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			sem <- struct{}{}
			defer func() { <-sem }()
			// 渠道限流风暴：剩余采样不再投递（已投递的 goroutine 收尾即可）
			if rateLimitStorm() {
				return
			}
			msgs := []map[string]any{{"role": "user", "content": distPrompt}}
			_, _, st, fin, content, te, e := probeCall(ctx, target, model, msgs,
				map[string]any{"temperature": 1.0, "max_tokens": 96})
			if e != nil {
				mu.Lock()
				invalid++
				log.Printf("[tuanjie] dist 采样失败: %v", e)
				mu.Unlock()
				return
			}
			if st != 200 {
				mu.Lock()
				unanswered++
				log.Printf("[tuanjie] dist 样本未作答（上游 %s）", te)
				mu.Unlock()
				return
			}
			if fin == "length" && content == "" {
				// 思考型预算耗尽正文未出：加大预算重试一次（首次预算 96 不变）
				_, _, st2, fin2, content2, te2, e2 := probeCall(ctx, target, model, msgs,
					map[string]any{"temperature": 1.0, "max_tokens": 512})
				if e2 == nil && st2 == 200 && !(fin2 == "length" && content2 == "") {
					fin, content = fin2, content2
				} else {
					mu.Lock()
					unanswered++
					log.Printf("[tuanjie] dist 样本未作答（加大预算重试仍空: %s）", te2)
					mu.Unlock()
					return
				}
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
	res := &distResult{Counts: counts[1:], Valid: valid, Invalid: invalid, Unanswered: unanswered}
	res.Stats = distStatsOf(counts)
	res.Insufficient = valid < 40
	res.RateLimited = rateLimitStorm() // 因限流提前中止（探针阶段就触发时采样一轮都不投递）
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
