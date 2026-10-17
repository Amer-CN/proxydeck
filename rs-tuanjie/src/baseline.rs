//! baseline.rs —— 基准比对面（`internal/tuanjie/baseline.go:289-627` 的逐条对应物）。
//!
//! `probes.rs` 已迁 `baseline.go` 的**加载/迁移/查询/落盘**面（`:26-233`）；本模块补
//! 剩下那半：探针值比对规则、多锚比对、综合判定灯色。
//!
//! ## 覆盖
//!
//! * [`probe_value_match`]（tokenizer ±1 容差 / 短值 ±1 字符位 / 数值型不享短值容差）；
//! * [`is_numeric_str`] / [`gateway_wording_probe`]；
//! * [`tokenizer_batch_shift`]（整批同量平移的偏离项形状判定，散播 ≤1）；
//! * [`compare_to_baseline`]（单锚：跨渠道措辞 skip、分布仅参考、灯色只由可比探针决定）；
//! * [`compare_against_anchors`]（多锚：取匹配程度最好的那条，reason 点名来源渠道）；
//! * [`anchor_match_phrase`] / [`anchor_conflict_note`]（锚间一致性独立信号，不参与灯色）。
//!
//! ## 差异（登记）
//!
//! 1. `probeCompare`/`overallVerdict` 的字段与 JSON 键逐字对应 Go，但 Rust 侧
//!    `note` 用 `Option<String>` 承载 Go 的 `omitempty`（空串不写）；
//! 2. `formatPct` 是 `strconv.Itoa(int(p))` —— **截断**而非四舍五入，本模块同款；
//! 3. Go 的 `CompareAgainstAnchors` 形参是 `[]*Baseline`（可含 nil）；Rust 侧
//!    用 `&[Baseline]` + 显式 `None` 语义由 `get_anchors` 保证（它不会产出 nil）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::channels::channel_name_of;
use crate::probes::{Baseline, ProbeResult};

/// tokenizer 归一化值容差 —— `tokenizerDriftTolerance`（`baseline.go:323`）。
pub const TOKENIZER_DRIFT_TOLERANCE: i64 = 1;

/// 短字符探针容差 —— `shortProbeTolerance`（`baseline.go:328`）。
pub const SHORT_PROBE_TOLERANCE: usize = 1;

/// 整批平移的散播上限 —— `batchShiftSpread`（`baseline.go:383`）。
pub const BATCH_SHIFT_SPREAD: i64 = 1;

/// 单探针比对结果 —— `probeCompare`（`baseline.go:290-302`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeCompare {
    pub name: String,
    pub current: String,
    pub baseline: String,
    pub r#match: bool,
    /// 双方探针都 ok 才为 true（false 时 `match` 是零值、不承载语义）。
    pub comparable: bool,
    /// ok | mismatch | unstable | error | skip。
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// 综合判定 —— `overallVerdict`（`baseline.go:306-311`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverallVerdict {
    /// green | yellow | red | grey。
    pub light: String,
    pub label: String,
    pub score: f64,
    pub reason: String,
}

impl Default for OverallVerdict {
    fn default() -> Self {
        OverallVerdict {
            light: String::new(),
            label: String::new(),
            score: 0.0,
            reason: String::new(),
        }
    }
}

/// 分布相似度（`dist.go` 的 `distSimilarity` 已在 `probes.rs`/`dist.rs` 侧定义，
/// 这里只按名字引用，避免重复定义）。
pub use crate::dist::DistSimilarity;

/// `strconv.Atoi` 的成功判据 —— `isNumericStr`（`baseline.go:367-370`）。
pub fn is_numeric_str(s: &str) -> bool {
    go_atoi(s).is_some()
}

/// Go `strconv.Atoi`：可有正负号、只吃十进制、整串消费、溢出报错。
pub fn go_atoi(s: &str) -> Option<i64> {
    let t = s;
    if t.is_empty() {
        return None;
    }
    let (neg, digits) = match t.as_bytes()[0] {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i64>().ok().map(|v| if neg { -v } else { v })
}

/// 网关措辞探针 —— `gatewayWordingProbe`（`baseline.go:374-376`）。
pub fn gateway_wording_probe(name: &str) -> bool {
    name == "error_temp2" || name == "error_maxtok"
}

/// 探针值比对规则 —— `probeValueMatch`（`baseline.go:334-364`）。
///
/// tokenizer 系按差值（≤1 容差）；其余按相等性，`len≤4` 的等长短值允许 1 个字符位差
/// （双方均为数值时不做短值容差）。
pub fn probe_value_match(name: &str, cur: &str, base: &str) -> bool {
    if name.starts_with("tokenizer_") {
        if let (Some(ci), Some(bi)) = (go_atoi(cur), go_atoi(base)) {
            return (ci - bi).abs() <= TOKENIZER_DRIFT_TOLERANCE;
        }
    }
    if cur == base {
        return true;
    }
    if cur.len() <= 4
        && base.len() <= 4
        && cur.len() == base.len()
        && !is_numeric_str(cur)
        && !is_numeric_str(base)
    {
        let mut diff = 0usize;
        for (a, b) in cur.bytes().zip(base.bytes()) {
            if a != b {
                diff += 1;
                if diff > SHORT_PROBE_TOLERANCE {
                    return false;
                }
            }
        }
        return true;
    }
    false
}

/// 整批同量平移判定 —— `tokenizerBatchShift`（`baseline.go:391-412`）。
pub fn tokenizer_batch_shift(cmps: &[ProbeCompare]) -> (bool, i64) {
    let mut deltas: Vec<i64> = Vec::new();
    for c in cmps {
        if !c.comparable || c.r#match || !c.name.starts_with("tokenizer_") {
            continue;
        }
        let (cur, base) = match (go_atoi(&c.current), go_atoi(&c.baseline)) {
            (Some(a), Some(b)) => (a, b),
            _ => return (false, 0), // 形状不可判定：保守不降级
        };
        deltas.push(cur - base);
    }
    if deltas.is_empty() {
        return (false, 0);
    }
    deltas.sort_unstable();
    if deltas[deltas.len() - 1] - deltas[0] > BATCH_SHIFT_SPREAD {
        return (false, 0);
    }
    (true, deltas[deltas.len() / 2])
}

/// `strconv.Itoa(int(p)) + "%"` —— `formatPct`（`baseline.go:626`）。
pub fn format_pct(p: f64) -> String {
    format!("{}%", p as i64)
}

/// 单锚比对 —— `CompareToBaseline`（`baseline.go:425-520`）。
pub fn compare_to_baseline(
    base: Option<&Baseline>,
    probes: &[ProbeResult],
    dist: Option<&crate::probes::DistResult>,
    channel: &str,
) -> (Vec<ProbeCompare>, DistSimilarity, OverallVerdict) {
    let Some(base) = base else {
        return (
            Vec::new(),
            DistSimilarity::default(),
            OverallVerdict {
                light: "grey".into(),
                label: "无基准".into(),
                score: 0.0,
                reason: "该模型无官方基准，先在官方渠道（团结 / Comate / Qoder / TokenRouter）检测一次以自动采集基准".into(),
            },
        );
    };

    let mut base_idx: BTreeMap<&str, &ProbeResult> = BTreeMap::new();
    for p in &base.probes {
        base_idx.insert(p.name.as_str(), p);
    }

    let mut cmps: Vec<ProbeCompare> = Vec::new();
    let (mut mismatch, mut unstable_count, mut comparable, mut cross_skipped) = (0i64, 0i64, 0i64, 0i64);
    for cur in probes {
        let mut c = ProbeCompare {
            name: cur.name.clone(),
            current: cur.value.clone(),
            baseline: String::new(),
            r#match: false,
            comparable: false,
            status: cur.status.clone(),
            note: cur.note.clone(),
        };
        if let Some(bp) = base_idx.get(cur.name.as_str()) {
            c.baseline = bp.value.clone();
            // 跨渠道：网关报错措辞不可比 → 跳过（不比对、不计 mismatch）
            if gateway_wording_probe(&cur.name) && channel != base.channel {
                c.status = "skip".into();
                c.note = "报错原文按各网关口径措辞不同，跨渠道不可比，已跳过".into();
                cross_skipped += 1;
                cmps.push(c);
                continue;
            }
            if cur.status == "ok" && bp.status == "ok" {
                comparable += 1;
                c.comparable = true;
                c.r#match = probe_value_match(&cur.name, &cur.value, &bp.value);
                if !c.r#match {
                    mismatch += 1;
                }
            } else {
                unstable_count += 1;
            }
        } else {
            unstable_count += 1;
        }
        cmps.push(c);
    }

    // 分布相似度（双方都足样本才算）
    let mut sim = DistSimilarity::default();
    let mut dist_ready = false;
    if let (Some(bd), Some(d)) = (base.dist.as_ref(), dist) {
        if !bd.insufficient && !d.insufficient {
            sim = crate::dist::compare_dist(&d.counts, &bd.counts, d.valid, bd.valid);
            dist_ready = true;
        }
    }

    let mut v = OverallVerdict {
        light: "grey".into(),
        label: "无基准".into(),
        score: 0.0,
        reason: String::new(),
    };
    let mut dist_note = "；分布不可比：样本不足".to_string();
    if dist_ready {
        let (pct, _) = crate::dist::dist_verdict(&sim);
        v.score = pct;
        dist_note = format!("；分布相似度 {}（参考项，不参与判定）", format_pct(pct));
    }

    if comparable == 0 {
        v.light = "grey".into();
        v.label = "无法判定".into();
        v.score = 0.0;
        v.reason = format!("管道探针全部未测成/无法归一化，本次不构成比对结论{dist_note}");
    } else if mismatch == 0 && unstable_count > 0 {
        v.light = "grey".into();
        v.label = "无法判定".into();
        v.score = 0.0;
        v.reason = format!(
            "管道探针仅 {comparable} 项可比且一致，本次不构成一致结论{dist_note}"
        );
    } else if mismatch == 0 {
        v.light = "green".into();
        v.label = "一致".into();
        if !dist_ready {
            v.score = 100.0;
        }
        v.reason = format!("管道探针全部匹配{dist_note}");
    } else if mismatch <= 1 {
        v.light = "yellow".into();
        v.label = "轻微偏差".into();
        if !dist_ready {
            v.score = 90.0;
        }
        v.reason = format!("1 个探针不匹配{dist_note}");
    } else {
        v.light = "red".into();
        v.label = "显著偏差".into();
        if !dist_ready {
            v.score = 60.0;
        }
        v.reason = format!("管道探针 {mismatch} 项不匹配（注水嫌疑）{dist_note}");
    }
    if cross_skipped > 0 {
        v.reason += &format!("；报错原文探针 {cross_skipped} 项跨渠道不可比，已跳过");
    }
    if unstable_count > 0 {
        v.reason += &format!("；{unstable_count} 项探针 unstable 未计入比对");
    }
    (cmps, sim, v)
}

/// 多锚比对 —— `CompareAgainstAnchors`（`baseline.go:534-573`）。
///
/// 取匹配程度最好的那条锚（可比偏离最少 → 可比项最多 → 采样新→旧，即输入序先到先得）。
pub fn compare_against_anchors(
    anchors: &[Baseline],
    probes: &[ProbeResult],
    dist: Option<&crate::probes::DistResult>,
    channel: &str,
) -> (Vec<ProbeCompare>, DistSimilarity, OverallVerdict, Option<Baseline>) {
    if anchors.is_empty() {
        let (c, s, v) = compare_to_baseline(None, probes, dist, channel);
        return (c, s, v, None);
    }
    let mut best: Option<(Vec<ProbeCompare>, DistSimilarity, OverallVerdict, Baseline, i64, i64)> =
        None;
    for a in anchors {
        let (cmps, sim, v) = compare_to_baseline(Some(a), probes, dist, channel);
        let (mut mis, mut comparable) = (0i64, 0i64);
        for c in &cmps {
            if c.comparable {
                comparable += 1;
                if !c.r#match {
                    mis += 1;
                }
            }
        }
        let take = match &best {
            None => true,
            Some((_, _, _, _, best_mis, best_comp)) => {
                mis < *best_mis || (mis == *best_mis && comparable > *best_comp)
            }
        };
        if take {
            best = Some((cmps, sim, v, a.clone(), mis, comparable));
        }
    }
    let Some((cmps, sim, mut v, matched, _, _)) = best else {
        let (c, s, v) = compare_to_baseline(None, probes, dist, channel);
        return (c, s, v, None);
    };
    let phrase = anchor_match_phrase(&v.light, Some(&matched));
    if !phrase.is_empty() {
        v.reason = format!("{phrase}：{}", v.reason);
    }
    (cmps, sim, v, Some(matched))
}

/// 匹配锚点名短语 —— `anchorMatchPhrase`（`baseline.go:577-585`）。
pub fn anchor_match_phrase(light: &str, matched: Option<&Baseline>) -> String {
    let Some(m) = matched else {
        return String::new();
    };
    if light == "grey" {
        return String::new();
    }
    if light == "green" {
        return format!("与 {} 官方基准一致", channel_name_of(&m.channel));
    }
    format!("与 {} 官方基准比对", channel_name_of(&m.channel))
}

/// 锚间一致性独立信号 —— `anchorConflictNote`（`baseline.go:593-624`）。
///
/// 某两条锚之间 ≥2 项可比探针超容差 → 返回如实提示；只作独立信息呈现，**不参与灯色**。
pub fn anchor_conflict_note(anchors: &[Baseline]) -> String {
    for i in 0..anchors.len() {
        for j in (i + 1)..anchors.len() {
            let (a, b) = (&anchors[i], &anchors[j]);
            let mut idx_b: BTreeMap<&str, &ProbeResult> = BTreeMap::new();
            for p in &b.probes {
                idx_b.insert(p.name.as_str(), p);
            }
            let mut over = 0i64;
            for pa in &a.probes {
                let Some(pb) = idx_b.get(pa.name.as_str()) else {
                    continue;
                };
                if pa.status != "ok" || pb.status != "ok" {
                    continue;
                }
                if gateway_wording_probe(&pa.name) && a.channel != b.channel {
                    continue;
                }
                if !probe_value_match(&pa.name, &pa.value, &pb.value) {
                    over += 1;
                }
            }
            if over >= 2 {
                return format!(
                    "官方锚点之间不一致：{} vs {} 有 {} 项探针指纹不同——其中至少一条链路可疑（该提示不参与灯色判定）",
                    channel_name_of(&a.channel),
                    channel_name_of(&b.channel),
                    over
                );
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(name: &str, value: &str, status: &str) -> ProbeResult {
        ProbeResult {
            name: name.into(),
            value: value.into(),
            status: status.into(),
            ..Default::default()
        }
    }

    fn bl(channel: &str, probes: Vec<ProbeResult>) -> Baseline {
        Baseline {
            channel: channel.into(),
            probes,
            ..Default::default()
        }
    }

    #[test]
    fn value_match_rules() {
        // tokenizer ±1
        assert!(probe_value_match("tokenizer_en", "26", "26"));
        assert!(probe_value_match("tokenizer_en", "27", "26"));
        assert!(!probe_value_match("tokenizer_en", "28", "26"));
        // 非数值精确相等
        assert!(probe_value_match("finish_stop", "stop", "stop"));
        assert!(!probe_value_match("finish_stop", "stop", "length"));
        // 短值 ±1 字符位（等长、非数值）
        assert!(probe_value_match("finish_stop", "stob", "stop"));
        // 数值型不享短值容差
        assert!(probe_value_match("x", "396", "396"));
        assert!(!probe_value_match("x", "386", "396"));
        // 长值不给短值容差
        assert!(!probe_value_match(
            "error_temp2",
            "invalid temperature value",
            "invalid temperature valuf"
        ));
    }

    #[test]
    fn short_value_tolerance_boundary() {
        assert!(probe_value_match("finish_stop", "stob", "stop"), "1 处差异 → true");
        assert!(
            !probe_value_match("finish_stop", "xtob", "stop"),
            "2 处差异 → false"
        );
        assert!(
            !probe_value_match("finish_stop", "stopx", "stop"),
            "长度不等 → false"
        );
    }

    #[test]
    fn numeric_str_like_go_atoi() {
        assert!(is_numeric_str("396"));
        assert!(is_numeric_str("-5"));
        assert!(is_numeric_str("+5"));
        assert!(!is_numeric_str(""));
        assert!(!is_numeric_str("3.9"));
        assert!(!is_numeric_str(" 396"));
        assert!(!is_numeric_str("abc"));
    }

    #[test]
    fn batch_shift_shape() {
        let mk = |cur: &str, base: &str| ProbeCompare {
            name: "tokenizer_en".into(),
            current: cur.into(),
            baseline: base.into(),
            r#match: false,
            comparable: true,
            status: "ok".into(),
            note: String::new(),
        };
        // 整批同量平移（-6/-8 → 散播 2 > 1 ⇒ 不算平移）
        let cmps = vec![mk("20", "26"), mk("27", "35")];
        assert_eq!(tokenizer_batch_shift(&cmps), (false, 0));
        // 同量平移（-6/-6/-6）
        let cmps = vec![mk("20", "26"), mk("29", "35"), mk("51", "57")];
        assert_eq!(tokenizer_batch_shift(&cmps), (true, -6));
        // 同量平移（-6/-7）：取排序后的**中位数** = 下标 len/2 项 = -6
        let cmps = vec![mk("20", "26"), mk("28", "35")];
        assert_eq!(tokenizer_batch_shift(&cmps), (true, -6));
        // 无可比偏离项 → 不算平移
        assert_eq!(tokenizer_batch_shift(&[]), (false, 0));
        // 值不可解析 → 保守 false
        let cmps = vec![mk("abc", "26")];
        assert_eq!(tokenizer_batch_shift(&cmps), (false, 0));
    }

    #[test]
    fn no_baseline_is_grey_guidance() {
        let (cmps, _sim, v) = compare_to_baseline(None, &[], None, "tuanjie");
        assert!(cmps.is_empty());
        assert_eq!(v.light, "grey");
        assert_eq!(v.label, "无基准");
        assert!(v.reason.contains("先在官方渠道"));
    }

    #[test]
    fn all_match_is_green_without_dist() {
        let base = bl("tuanjie", vec![pr("tokenizer_en", "26", "ok")]);
        let probes = vec![pr("tokenizer_en", "26", "ok")];
        let (_c, _s, v) = compare_to_baseline(Some(&base), &probes, None, "tuanjie");
        assert_eq!(v.light, "green");
        assert_eq!(v.score, 100.0);
        assert!(v.reason.contains("管道探针全部匹配"));
        assert!(v.reason.contains("分布不可比：样本不足"));
    }

    #[test]
    fn one_mismatch_is_yellow_two_is_red() {
        let base = bl(
            "tuanjie",
            vec![pr("tokenizer_en", "26", "ok"), pr("tokenizer_cjk", "35", "ok")],
        );
        let probes = vec![pr("tokenizer_en", "40", "ok"), pr("tokenizer_cjk", "35", "ok")];
        let (_c, _s, v) = compare_to_baseline(Some(&base), &probes, None, "tuanjie");
        assert_eq!(v.light, "yellow");
        assert_eq!(v.score, 90.0);
        let probes = vec![pr("tokenizer_en", "40", "ok"), pr("tokenizer_cjk", "50", "ok")];
        let (_c, _s, v) = compare_to_baseline(Some(&base), &probes, None, "tuanjie");
        assert_eq!(v.light, "red");
        assert_eq!(v.score, 60.0);
        assert!(v.reason.contains("2 项不匹配"));
    }

    #[test]
    fn partial_probe_is_grey_not_green() {
        let base = bl("tuanjie", vec![pr("tokenizer_en", "26", "ok")]);
        // 一条可比一致 + 一条当前侧 error
        let probes = vec![pr("tokenizer_en", "26", "ok"), pr("finish_stop", "x", "error")];
        let (_c, _s, v) = compare_to_baseline(Some(&base), &probes, None, "tuanjie");
        assert_eq!(v.light, "grey");
        assert!(v.reason.contains("仅 1 项可比且一致"));
        assert!(v.reason.contains("1 项探针 unstable 未计入比对"));
    }

    #[test]
    fn no_comparable_probe_is_grey_with_zero_score() {
        let base = bl("tuanjie", vec![pr("tokenizer_en", "26", "ok")]);
        let probes = vec![pr("tokenizer_en", "26", "error")];
        let (_c, _s, v) = compare_to_baseline(Some(&base), &probes, None, "tuanjie");
        assert_eq!(v.light, "grey");
        assert_eq!(v.label, "无法判定");
        assert_eq!(v.score, 0.0);
    }

    #[test]
    fn cross_channel_gateway_wording_is_skipped() {
        // 注意：短值容差按 Go 的 `len()` = **字节**——"温度非法" 是 12 字节，
        // 走不到短值分支；这里两侧都用 >4 字节的不同值，确保是**真不匹配**。
        let base = bl(
            "tuanjie",
            vec![pr("error_temp2", "invalid temperature", "ok")],
        );
        let probes = vec![pr("error_temp2", "温度非法（temperature must be <=2）", "ok")];
        let (cmps, _s, v) = compare_to_baseline(Some(&base), &probes, None, "bai");
        assert_eq!(cmps[0].status, "skip");
        assert!(!cmps[0].comparable);
        // 唯一探针被 skip ⇒ 可比 0 ⇒ grey
        assert_eq!(v.light, "grey");
        assert!(v.reason.contains("报错原文探针 1 项跨渠道不可比，已跳过"));
        // 同渠道则正常比对（1 项不匹配 → yellow）
        let (_c, _s, v2) = compare_to_baseline(Some(&base), &probes, None, "tuanjie");
        assert_eq!(v2.light, "yellow");
    }

    #[test]
    fn anchors_pick_best_and_name_source() {
        // 锚 A（tuanjie）与当前一致；锚 B（bai）不一致 → 取 A
        let a = bl("tuanjie", vec![pr("tokenizer_en", "26", "ok")]);
        let b = bl("bai", vec![pr("tokenizer_en", "99", "ok")]);
        let probes = vec![pr("tokenizer_en", "26", "ok")];
        let (_c, _s, v, matched) = compare_against_anchors(&[a, b], &probes, None, "comate");
        assert_eq!(v.light, "green");
        assert_eq!(matched.as_ref().unwrap().channel, "tuanjie");
        assert!(v.reason.starts_with("与 团结 官方基准一致："), "{}", v.reason);
    }

    #[test]
    fn anchors_empty_is_no_baseline() {
        let (_c, _s, v, matched) = compare_against_anchors(&[], &[], None, "tuanjie");
        assert_eq!(v.light, "grey");
        assert!(matched.is_none());
    }

    #[test]
    fn anchor_conflict_note_needs_two_over() {
        let a = bl(
            "tuanjie",
            vec![pr("tokenizer_en", "26", "ok"), pr("tokenizer_cjk", "35", "ok")],
        );
        let b = bl(
            "bai",
            vec![pr("tokenizer_en", "40", "ok"), pr("tokenizer_cjk", "50", "ok")],
        );
        let note = anchor_conflict_note(&[a.clone(), b.clone()]);
        assert_eq!(
            note,
            "官方锚点之间不一致：团结 vs B.ai 有 2 项探针指纹不同——其中至少一条链路可疑（该提示不参与灯色判定）"
        );
        // 只有 1 项超容差 → 不提示
        let b1 = bl(
            "bai",
            vec![pr("tokenizer_en", "40", "ok"), pr("tokenizer_cjk", "35", "ok")],
        );
        assert!(anchor_conflict_note(&[a, b1]).is_empty());
    }

    #[test]
    fn format_pct_truncates_like_go() {
        assert_eq!(format_pct(96.9), "96%");
        assert_eq!(format_pct(0.0), "0%");
        assert_eq!(format_pct(100.0), "100%");
    }
}
