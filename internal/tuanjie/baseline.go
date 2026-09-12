// baseline.go —— 绝对基准库（第三层）：按「规范化模型名|来源渠道」存官方锚
// （管道探针值 + 355 维分布 + 统计），检测时逐探针比对输出综合灯色（分布相似度
// 仅参考展示，第 38 轮判别力实验后不参与判定）。
// 存储 tuanjie-baselines.json（模型|渠道 → 官方锚；同一渠道重复采集覆盖自己、
// 以最新为准，不同渠道各存一条——同一模型可有多条官方锚，第 39 轮多锚）。
// 基准归属（第 33 轮）：基准只认官方渠道（tuanjie/comate/qoder/tokenrouter，
// 复用 isStrongChannel）采集的合格样本，非官方渠道（bai/command/workbuddy）
// 不再自采基准、不再用自采基准判定——其他渠道检测时按模型查官方锚比对。
// 多锚比对（第 39 轮，用户裁决「认可多锚」）：与任一锚一致即视为一致，
// 锚间不一致只作独立提示、不参与灯色。
package tuanjie

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Baseline 单模型的官方锚：第一层探针值 + 第二层分布。
type Baseline struct {
	Model         string          `json:"model"`
	Channel       string          `json:"channel,omitempty"`          // 来源渠道（官方渠道，tuanjie/comate/qoder/tokenrouter）
	ReturnedModel string          `json:"returned_model,omitempty"`   // 上游自报模型名（响应 model 字段；与登记名不一致时报告如实显示——键仍按登记名建，绝不按返回名）
	Official      bool            `json:"official"`                   // 官方渠道基准（库内条目恒为 true，加载时迁移补记）
	SchemaVersion int             `json:"schema_version"`             // 基准数据格式版本（门禁见 baselineSchemaVersion）
	Probes        []probeResult   `json:"probes"`                     // tokenizer 4 值 / 错误文本 2 / finish 2
	Dist          *distResult     `json:"dist,omitempty"`             // 355 维分布 + 统计（不足 40 样本时缺省）
	SampledAt     string          `json:"sampled_at"`
	Account       string          `json:"account"`
	SampleCount   int             `json:"sample_count"`
}

// baselineSchemaVersion 基准数据格式当前版本（第 39 轮多锚存储起为 3）。
// 3 = 多锚存储：键由「规范化模型名」（v2）升级为「规范化模型名|来源渠道id」，
// 同一模型可存多条不同渠道的官方锚；加载迁移：内容版本 ≥2 的旧键（v2 裸模型
// 名键、更早的 v1「渠道|模型」键）按各自 Channel 字段重键为 模型|渠道 并把
// 条目升版本为 3（内容本身未变，仅键格式升级）。
// 2 = tokenizer 归一化底座取自 probeCall 第 1 个返回值 prompt_tokens 之后的
// 格式（内容可信边界）；低于 2 的存量（含无版本号条目——底座误取第 2 个
// 返回值 completion_tokens）归一化值整体不可信、偏移量未知，不迁移不加载，
// 加载时一律丢弃——迁移后仍低于 3 的正是这一类（沿用既有版本门禁机制）。
const baselineSchemaVersion = 3

// baselineMigratableVersion 可迁移（可加载）的最低内容版本：≥2 的条目 tokenizer
// 底座正确、内容可信，加载时重键为 模型|渠道 并升版本为 3；<2 的一律不加载。
const baselineMigratableVersion = 2

// BaselineStore 基准库（键 = 规范化模型名（normalizeModelName：小写+去路由
// 前缀）+ "|" + 来源渠道id → 官方锚；同一模型多条锚按 GetAnchors 取回）。
type BaselineStore struct {
	mu        sync.Mutex
	path      string
	Baselines map[string]*Baseline `json:"baselines"`
}

// baselineKey 多锚存储键：规范化模型名|来源渠道id。键永远按登记名（采集时
// 传入的 model）建——用户拿什么名字认证就记在什么名字下，上游自报名
// （ReturnedModel）只作元数据显示，绝不参与键位（按返回名建键会查不到锚）。
func baselineKey(model, channel string) string {
	return normalizeModelName(model) + "|" + channel
}

func baselinesFilePath() string {
	return filepath.Join(exeDirForAccounts(), "tuanjie-baselines.json")
}

// baselineQualityOK 基准质量门槛：管道探针 ok ≥6/8、分布有效样本 ≥40、
// 全部 ok 的 tokenizer 探针值为正整数（不达标不能当判定锚点——B.ai 第二层
// 0/60 之类的采集正是缺这道门槛）。
// tokenizer ≥1 是物理下限：归一化值 = 文本 token 数 − "a" token 数，
// 不可能为负或 0（存量污染条目 "a" 报 ~88 → 值 -57/-48/-26/-20，正是这道
// 门槛此前缺失才让两条官方污染锚点存活）。
func baselineQualityOK(bl *Baseline) bool {
	if bl == nil {
		return false
	}
	ok := 0
	for _, p := range bl.Probes {
		if p.Status != "ok" {
			continue
		}
		ok++
		if strings.HasPrefix(p.Name, "tokenizer_") {
			n, err := strconv.Atoi(p.Value)
			if err != nil || n < 1 {
				return false
			}
		}
	}
	return ok >= 6 && bl.Dist != nil && bl.Dist.Valid >= 40
}

// LoadBaselines 从磁盘恢复（缺失 = 空库）。旧键/旧版本迁移（只改加载逻辑，
// 磁盘数据由代码重写，不手工编辑）：
//   - 内容版本 ≥2（tokenizer 底座正确，可信）且键为旧格式——v2 裸模型名键 /
//     v1「渠道|模型」键——按各自 Channel 字段重键为「模型|渠道」（多锚键），
//     条目升版本为 3；同一模型撞键（同模型同渠道多条）取 SampledAt 最新；
//   - 内容版本 <2（无版本号 / 旧版本）：归一化底座不可信，不迁移不加载，
//     一律丢弃（迁移后仍低于 3 的正是这一类）；
//   - 仅官方渠道（isStrongChannel）且通过质量门槛的条目保留，其余（非官方
//     渠道自采 / 劣质采集）忽略（非官方渠道自采基准一律不再有效）。
//
// 同一渠道重复采集覆盖自己（CollectBaseline 同键覆写），不同渠道互不影响。
func LoadBaselines() *BaselineStore {
	bs := &BaselineStore{path: baselinesFilePath(), Baselines: map[string]*Baseline{}}
	if b, err := os.ReadFile(bs.path); err == nil {
		_ = json.Unmarshal(b, bs)
		if bs.Baselines == nil {
			bs.Baselines = map[string]*Baseline{}
		}
		if changed := bs.migrateLegacyKeys(); changed {
			bs.save()
		}
	}
	return bs
}

// migrateLegacyKeys 就地迁移旧键（返回是否有结构变化需落盘）。map 遍历无序，
// 按 key 排序后处理保证多次加载结果确定。
func (bs *BaselineStore) migrateLegacyKeys() bool {
	keys := make([]string, 0, len(bs.Baselines))
	for k := range bs.Baselines {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	out := map[string]*Baseline{}
	changed := false
	for _, k := range keys {
		v := bs.Baselines[k]
		if v == nil {
			changed = true
			continue
		}
		// 版本门禁（沿用既有机制）：内容版本低于可迁移下限（无版本号/旧格式，
		// tokenizer 归一化底座不可信）一律不加载——只丢弃，不手工改磁盘文件
		// （重写由 save 完成）；迁移后仍低于 3 的正是这一类。
		if v.SchemaVersion < baselineMigratableVersion {
			changed = true
			continue
		}
		var model, channel string
		if v.SchemaVersion >= baselineSchemaVersion {
			// v3 新键格式 = 模型|渠道（新代码只写这种键；无 "|" 的脏键按
			// 数据体 Channel 兜底）。模型名本身可含 "|"，按最后一个 "|" 分割。
			model, channel = k, v.Channel
			if i := strings.LastIndex(k, "|"); i >= 0 {
				model, channel = k[:i], k[i+1:]
			}
		} else {
			// 旧键格式：v1「渠道|模型」（渠道在键前缀）、v2 裸模型名（来源
			// 渠道以数据体 Channel 字段为准，缺省按团结处理）
			channel, model = "tuanjie", k
			if i := strings.Index(k, "|"); i >= 0 {
				channel, model = k[:i], k[i+1:]
			} else if v.Channel != "" {
				channel = v.Channel
			}
		}
		if v.Model != "" {
			model = v.Model // 数据体里的 Model 字段优先（键是历史格式）
		}
		// 仅官方渠道且过质量门槛才保留；其余（非官方渠道自采/劣质采集）丢弃
		if !isStrongChannel(channel) || !baselineQualityOK(v) {
			changed = true
			continue
		}
		// 内容可信的旧格式条目：升版本为 3（内容未变，仅键格式升级为多锚）
		if v.SchemaVersion < baselineSchemaVersion {
			v.SchemaVersion = baselineSchemaVersion
			changed = true
		}
		nk := baselineKey(model, channel)
		if k != nk {
			changed = true
		}
		if !v.Official {
			v.Official = true
			changed = true
		}
		if v.Channel == "" {
			v.Channel = channel
			changed = true
		}
		// 同模型同渠道撞键（旧格式多条归并到同一新键）取 SampledAt 最新
		// （与 CollectBaseline「以最新为准」同口径）
		if cur, exists := out[nk]; !exists || v.SampledAt > cur.SampledAt {
			out[nk] = v
		}
	}
	bs.Baselines = out
	return changed
}

func (bs *BaselineStore) save() {
	b, err := json.MarshalIndent(bs, "", "  ")
	if err != nil {
		return
	}
	_ = os.WriteFile(bs.path, b, 0o600)
}

// GetAnchors 返回该模型的全部官方锚（多锚存储：键前缀 = 规范化模型名|，
// 按 SampledAt 新→旧排序）。多锚比对（CompareAgainstAnchors）与锚间一致性
// 提示（anchorConflictNote）以此为输入。
func (bs *BaselineStore) GetAnchors(model string) []*Baseline {
	bs.mu.Lock()
	defer bs.mu.Unlock()
	prefix := normalizeModelName(model) + "|"
	var out []*Baseline
	for k, v := range bs.Baselines {
		if strings.HasPrefix(k, prefix) && v != nil {
			out = append(out, v)
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].SampledAt > out[j].SampledAt })
	return out
}

// Get 返回该模型最新的官方锚（多锚存储下取 SampledAt 最新的一条；无锚返回 nil）。
func (bs *BaselineStore) Get(model string) *Baseline {
	anchors := bs.GetAnchors(model)
	if len(anchors) == 0 {
		return nil
	}
	return anchors[0]
}

// CollectBaseline 采集基准：跑第一层全部探针 + 第二层分布采样（N 默认 60），
// 落盘为该模型该渠道的官方锚（多锚存储：键 = 模型|渠道，同一渠道重复采集
// 覆盖自己、以最新为准，不影响其他渠道的锚）。质量门槛（第 33 轮）：探针
// ok <6/8 或分布有效样本 <40 → 拒绝落盘并返回原因；非官方渠道不再采集。
// 限流中止（2026-09-12 回归修复）：429 累计超阈值导致采集提前中止 → 同样
// 返回错误、不落盘（半截基准绝不能当判定锚点）。
// 登记名 vs 自报名（第 39 轮）：探针请求按登记名（model 参数）发出，
// probeCall 解析上游响应的 model 字段回填 target，采集时记进 ReturnedModel；
// 键仍按登记名建（用户拿什么名字认证就记在什么名字下），自报名只作元数据。
func (bs *BaselineStore) CollectBaseline(ctx context.Context, target *probeTarget, channel, model, account string, n int) (*Baseline, error) {
	if !isStrongChannel(channel) {
		return nil, fmt.Errorf("非官方渠道（%s）不再采集基准：请用官方渠道（团结 / Comate / Qoder / TokenRouter）采集后再检测其他渠道", channel)
	}
	probes := RunPipelineProbes(ctx, target, model)
	dist := collectDistSamples(ctx, target, model, n)
	// 限流早停：采集未完成（探针提前 break、采样提前停投递），如实报错、绝不落盘
	if rateLimitStorm() {
		return nil, fmt.Errorf("渠道限流（HTTP 429 累计 %d 次），本次基准采集未完成，已中止且不落盘，请稍后重试", probe429Count.Load())
	}
	okCount := 0
	for _, p := range probes {
		if p.Status == "ok" {
			okCount++
		}
	}
	if okCount < 6 || dist == nil || dist.Valid < 40 {
		valid := 0
		if dist != nil {
			valid = dist.Valid
		}
		return nil, fmt.Errorf("基准质量门槛未过：探针 ok %d/8（需 ≥6）、分布有效样本 %d（需 ≥40），已拒绝落盘", okCount, valid)
	}
	if !baselineQualityOK(&Baseline{Probes: probes, Dist: dist}) {
		return nil, fmt.Errorf("基准质量门槛未过：tokenizer 归一化值必须为正整数（物理下限：文本 token 数 − \"a\" token 数不可能 ≤0），已拒绝落盘")
	}
	bl := &Baseline{
		Model:         model,
		Channel:       channel,
		Official:      true,
		SchemaVersion: baselineSchemaVersion,
		Probes:        probes,
		Dist:          dist,
		SampledAt:     time.Now().Format("2006-01-02 15:04:05"),
		Account:       account,
		SampleCount:   dist.Valid,
		ReturnedModel: target.returnedModelOf(), // 上游自报模型名（与登记名不一致时报告如实显示）
	}
	bs.mu.Lock()
	bs.Baselines[baselineKey(model, channel)] = bl
	bs.save()
	bs.mu.Unlock()
	return bl, nil
}

// probeCompare 单探针比对结果。
type probeCompare struct {
	Name     string `json:"name"`
	Current  string `json:"current"`
	Baseline string `json:"baseline"`
	Match    bool   `json:"match"`
	// Comparable 该项是否真正可比：双方（当前侧与基准侧）探针都 ok 才为 true。
	// false 时 Match 是零值、不承载任何语义（基准侧 error/缺失或当前侧
	// unstable/error）——消费方统计「可比 / 偏离 / 未纳入比对」一律以本字段
	// 为准，绝不能拿 Status=="ok" && !Match 当偏离（条目假红，终审第 37 轮）。
	Comparable bool   `json:"comparable"`
	Status     string `json:"status"` // ok | mismatch | unstable | error | skip（当前探针状态；skip=跨渠道不可比已跳过）
	Note       string `json:"note,omitempty"`
}

// overallVerdict 综合判定：一致（green）/ 轻微偏差（yellow）/
// 显著偏差（red=注水嫌疑）/ 无基准（grey=先跑 baseline）。
type overallVerdict struct {
	Light  string `json:"light"` // green | yellow | red | grey
	Label  string `json:"label"`
	Score  float64 `json:"score"` // 综合相似度百分比
	Reason string `json:"reason"`
}

// tokenizerDriftTolerance 归一化 token 数比对容差：±1（保留 1 个 token 余量，
// 不收到 0，差值比对、非绝对相等）。旧值 ±8 是在「"a" 底座取错返回值
// （completion_tokens）导致整批伪漂移」的年代标定的——当时归一化底座随运行
// 漂移、整批指纹跟着平移；该根因已修（归一化底座 = prompt_tokens），实测
// 4 组跨渠道/跨运行对照 Δ=0（团结 GLM-5.3-FLASH 两次独立采集 26/35/57/63、
// B.ai glm-5.3-flash 对团结官方 FLASH raw 39/48/70/71、团结 KIMI-K3 对
// workbuddy kimi-k3 26/29/58/55），故收紧到 ±1：KIMI-K3 与 GLM-5.3-FLASH
// 四项差值 0/6/1/8 全落在旧 ±8 内，会把两家不同模型判成「全部匹配」假绿。
// 安全阀仍在："a" 底座离群/无法归一化时整批 tokenizer 探针转 unstable →
// 压灰不判红，不会因收紧而误伤。
const tokenizerDriftTolerance = 1

// shortProbeTolerance 短字符探针（len≤4）允许的最大字符位差（±1）：上游模板
// 微抖时 2/4 字符探针（错误文本/finish 系）单字符位漂移不再误报 mismatch；
// 「a」单字符探针 v3.6.5 已做 3 次中位数，此处补齐其余短探针。数值型探针不受影响。
const shortProbeTolerance = 1

// probeValueMatch 探针值比对规则：tokenizer 系按差值（≤1 容差，依据与
// 安全阀说明见 tokenizerDriftTolerance 注释）；错误文本/finish_reason 系按相等性（报错原文与完停词是
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

// gatewayWordingProbe 网关措辞探针（error_temp2/error_maxtok）：报错原文是
// 各网关自己的说法（实测团结与 B.ai 不同），逐字符指纹只在同渠道可比。
func gatewayWordingProbe(name string) bool {
	return name == "error_temp2" || name == "error_maxtok"
}

// batchShiftSpread 「整批同量平移」形状判定的偏离项带符号差值允许散布：
// 偏离项 delta 集合的 max−min ≤ 1 视为整批一致平移（逐项 ±1 分词噪声内）；
// 散布 ≥2 视为换模型形状——实测 KIMI-K3（26/29/58/55）冒充 GLM-5.3-FLASH
// （26/35/57/63）时偏离项 delta 为 −6/−8，散布恰为 2，跨厂商偷换不得被
// 模板漂移降级护栏吞成 yellow。
const batchShiftSpread = 1

// tokenizerBatchShift 漂移形状判定：tokenizer 偏离项（可比且不匹配）的带符号
// 差值 delta = 当前值 − 基准值 是否呈「整批同量平移」。真·模板平移（上游
// 隐藏模板变动）会令所有偏离项同量平移（delta 近似相等）；换模型则各文本
// 分词不同、delta 不一致（如 −6 与 −8 之差，或正负混杂）→ 不算平移。
// 返回 (是否整批同量平移, 平移量中位数)。任一偏离项的值无法解析为整数 →
// 形状不可判定，返回 false（保守取严，不降级）。
func tokenizerBatchShift(cmps []probeCompare) (bool, int) {
	var deltas []int
	for _, c := range cmps {
		if !c.Comparable || c.Match || !strings.HasPrefix(c.Name, "tokenizer_") {
			continue
		}
		cur, err1 := strconv.Atoi(c.Current)
		base, err2 := strconv.Atoi(c.Baseline)
		if err1 != nil || err2 != nil {
			return false, 0 // 形状不可判定：保守不降级
		}
		deltas = append(deltas, cur-base)
	}
	if len(deltas) == 0 {
		return false, 0
	}
	sort.Ints(deltas)
	if deltas[len(deltas)-1]-deltas[0] > batchShiftSpread {
		return false, 0 // 偏离项 delta 不一致（换模型形状）
	}
	return true, deltas[len(deltas)/2]
}

// CompareToBaseline 当前探针集 + 分布 vs 官方基准（channel 为本次检测的目标渠道）：
//   - 探针比对：tokenizer 4 值差值（≤1 容差 match，依据见 tokenizerDriftTolerance 注释）；
//     错误文本 2 相等性——仅当目标渠道与基准来源渠道相同才比对（跨渠道措辞
//     不可比，跳过并提示，不计 mismatch）；finish 2 相等性
//   - 分布：distribScore/modeScore 加权 overall 仍照算并随返回值给出（展示与
//     历史用）——第 38 轮判别力实验证明现有判档线（≥96%/≥90%）落在抽样噪声
//     带内（同模型隔天自比仅 0.535、两个真不同模型可达 0.981），当前样本量下
//     分布相似度只是参考项，不参与灯色
//   - 综合：灯色只由可比探针决定——无可比项 → grey「无法判定」；部分未测成
//     不判 green（局部证据不构成一致结论，分布再像也不行）；全部匹配 green；
//     1 项偏离 yellow；≥2 项偏离 red
func CompareToBaseline(base *Baseline, probes []probeResult, dist *distResult, channel string) ([]probeCompare, distSimilarity, overallVerdict) {
	// 无基准：灰灯引导（先采基准），直接返回
	if base == nil {
		v := overallVerdict{Light: "grey", Label: "无基准",
			Reason: "该模型无官方基准，先在官方渠道（团结 / Comate / Qoder / TokenRouter）检测一次以自动采集基准"}
		return nil, distSimilarity{}, v
	}
	// 基准探针索引
	baseIdx := map[string]probeResult{}
	for _, p := range base.Probes {
		baseIdx[p.Name] = p
	}
	var cmps []probeCompare
	mismatch, unstableCount, comparable, crossSkipped := 0, 0, 0, 0
	for _, cur := range probes {
		c := probeCompare{Name: cur.Name, Current: cur.Value, Baseline: "", Status: cur.Status, Note: cur.Note}
		if bp, ok := baseIdx[cur.Name]; ok {
			c.Baseline = bp.Value
			// 跨渠道：网关报错措辞不可比 → 跳过（不比对、不计 mismatch）
			if gatewayWordingProbe(cur.Name) && channel != base.Channel {
				c.Status = "skip"
				c.Note = "报错原文按各网关口径措辞不同，跨渠道不可比，已跳过"
				crossSkipped++
				cmps = append(cmps, c)
				continue
			}
			if cur.Status == "ok" && bp.Status == "ok" {
				comparable++
				c.Comparable = true // 只有双方都 ok 才真正可比，Match 才有语义
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

	// 综合判定（第 38 轮）：灯色只由可比探针决定，分布相似度不参与——
	// 只作为 score（展示/历史）与 reason 尾注（参考项说明）随返回值给出
	v := overallVerdict{Light: "grey", Label: "无基准"}
	distNote := "；分布不可比：样本不足"
	if distReady {
		pct, _ := distVerdict(sim)
		v.Score = pct
		distNote = "；分布相似度 " + formatPct(pct) + "（参考项，不参与判定）"
	}
	switch {
	case comparable == 0:
		// 探针一项都没测成：什么都没比对到 ≠ 确认一致，压灰不产出结论
		// （假绿修复，终审第 36 轮——与分布可用与否无关）
		v.Light, v.Label, v.Score = "grey", "无法判定", 0
		v.Reason = "管道探针全部未测成/无法归一化，本次不构成比对结论" + distNote
	case mismatch == 0 && unstableCount > 0:
		// 部分探针未测成：可比的都一致也只是局部证据，绝不写「全部匹配」假绿
		v.Light, v.Label, v.Score = "grey", "无法判定", 0
		v.Reason = "管道探针仅 " + itoa(comparable) + " 项可比且一致，本次不构成一致结论" + distNote
	case mismatch == 0:
		v.Light, v.Label = "green", "一致"
		if !distReady {
			v.Score = 100
		}
		v.Reason = "管道探针全部匹配" + distNote
	case mismatch <= 1:
		v.Light, v.Label = "yellow", "轻微偏差"
		if !distReady {
			v.Score = 90
		}
		v.Reason = "1 个探针不匹配" + distNote
	default:
		v.Light, v.Label = "red", "显著偏差"
		if !distReady {
			v.Score = 60
		}
		v.Reason = "管道探针 " + itoa(mismatch) + " 项不匹配（注水嫌疑）" + distNote
	}
	if crossSkipped > 0 {
		v.Reason += "；报错原文探针 " + itoa(crossSkipped) + " 项跨渠道不可比，已跳过"
	}
	if unstableCount > 0 {
		v.Reason += "；" + itoa(unstableCount) + " 项探针 unstable 未计入比对"
	}
	return cmps, sim, v
}

// CompareAgainstAnchors 多锚比对（第 39 轮，用户裁决「认可多锚」）：同一模型
// 可有多条官方锚（不同渠道采集），与任一锚一致即视为一致。
//   - 对每条锚跑现有单锚比对（CompareToBaseline 原样复用——假绿/假红封堵、
//     429 早停后的压灰、±1 容差、跨渠道 error 探针 skip、分布仅参考等单锚
//     语义全部不变）；
//   - 取匹配程度最好的那条锚（可比偏离最少 → 可比项最多 → 采样新→旧）的
//     结论作为最终灯色；
//   - reason 点名匹配锚的来源渠道（否则用户不知道结论从哪来）；
//   - 一条都不匹配 → 维持单锚 red 语义；锚列表为空 → 无官方基准
//     （grey 引导采集，不采集、不写库由调用方负责）。
//
// 返回值比单锚比对多一个 matched（匹配到的那条锚，供报告④锚来源/自报名显示）。
func CompareAgainstAnchors(anchors []*Baseline, probes []probeResult, dist *distResult, channel string) ([]probeCompare, distSimilarity, overallVerdict, *Baseline) {
	if len(anchors) == 0 {
		cmps, sim, v := CompareToBaseline(nil, probes, dist, channel)
		return cmps, sim, v, nil
	}
	var (
		bestCmps    []probeCompare
		bestSim     distSimilarity
		bestV       overallVerdict
		best        *Baseline
		bestMis     = -1
		bestComparable = -1
	)
	for _, a := range anchors { // GetAnchors 已按 SampledAt 新→旧；同分时先到先得=取最新
		if a == nil {
			continue
		}
		cmps, sim, v := CompareToBaseline(a, probes, dist, channel)
		mis, comparable := 0, 0
		for _, c := range cmps {
			if c.Comparable {
				comparable++
				if !c.Match {
					mis++
				}
			}
		}
		if bestMis == -1 || mis < bestMis || (mis == bestMis && comparable > bestComparable) {
			bestCmps, bestSim, bestV, best, bestMis, bestComparable = cmps, sim, v, a, mis, comparable
		}
	}
	if best == nil { // 锚列表全是 nil：按无官方基准处理
		cmps, sim, v := CompareToBaseline(nil, probes, dist, channel)
		return cmps, sim, v, nil
	}
	if p := anchorMatchPhrase(bestV.Light, best); p != "" {
		bestV.Reason = p + "：" + bestV.Reason
	}
	return bestCmps, bestSim, bestV, best
}

// anchorMatchPhrase 匹配锚点名短语（多锚比对用）：green=「与 X 官方基准一致」，
// yellow/red=「与 X 官方基准比对」；grey（无法判定类）与无锚不点名，返回空串。
func anchorMatchPhrase(light string, matched *Baseline) string {
	if matched == nil || light == "grey" {
		return ""
	}
	if light == "green" {
		return "与 " + channelNameOf(matched.Channel) + " 官方基准一致"
	}
	return "与 " + channelNameOf(matched.Channel) + " 官方基准比对"
}

// anchorConflictNote 锚间一致性独立信号（第 39 轮多锚）：同一模型 ≥2 条官方锚
// 时，用现有 probeValueMatch 规则两两比对锚的探针值——双方都 ok 才可比
// （Comparable 口径同单锚比对），跨渠道时网关报错措辞探针沿用 skip 语义不
// 比对；某两条锚之间 ≥2 项可比探针超容差 → 返回如实提示（点名两条锚的渠道
// 与项数）。该提示只作独立信息呈现，绝不参与灯色（锚之间打架 ≠ 被测渠道
// 注水）；无可提示时返回空串（报告不占位）。
func anchorConflictNote(anchors []*Baseline) string {
	for i := 0; i < len(anchors); i++ {
		for j := i + 1; j < len(anchors); j++ {
			a, b := anchors[i], anchors[j]
			if a == nil || b == nil {
				continue
			}
			idxB := map[string]probeResult{}
			for _, p := range b.Probes {
				idxB[p.Name] = p
			}
			over := 0
			for _, pa := range a.Probes {
				pb, ok := idxB[pa.Name]
				if !ok || pa.Status != "ok" || pb.Status != "ok" {
					continue // 任一侧未测成/缺失：无可比性
				}
				if gatewayWordingProbe(pa.Name) && a.Channel != b.Channel {
					continue // 报错原文按各网关口径措辞不同，跨渠道不可比（沿用 skip 语义）
				}
				if !probeValueMatch(pa.Name, pa.Value, pb.Value) {
					over++
				}
			}
			if over >= 2 {
				return "官方锚点之间不一致：" + channelNameOf(a.Channel) + " vs " + channelNameOf(b.Channel) +
					" 有 " + itoa(over) + " 项探针指纹不同——其中至少一条链路可疑（该提示不参与灯色判定）"
			}
		}
	}
	return ""
}

func formatPct(p float64) string { return strconv.Itoa(int(p)) + "%" }
func itoa(n int) string          { return strconv.Itoa(n) }
