//! 随机数分布指纹的**纯函数面** —— `internal/tuanjie/dist.go` 的对应物（P8b 切片）。
//!
//! 本模块只搬 `dist.go` 里不依赖网络/探针的部分：`parseDistAnswer`（`:60-90`）、
//! `distStatsOf`（`:176-221`）、`CompareDist`（`:225-270`）、`padCounts`（`:273-277`）、
//! `abs`/`round2`/`round4`（`:279-287`）、`distVerdict`/`distVerdictLabel`（`:290-312`）、
//! `fmtDistSampleProgress`（`:315-317`），以及四个常量（`:18-24`）。
//!
//! **`collectDistSamples`（`:102-173`）不在本切片**：它驱动 `probeCall`/`probeTarget`/
//! `rateLimitStorm`（`probes.go` + `channels.go` 的网络面），属 P8b 后续小轮。
//!
//! 浮点口径：Go 的 `round2`/`round4` 是 `math.Round(v*100)/100`（**half away from zero**），
//! 与 Rust `f64::round()` 同语义；乘法顺序逐条照抄，保证同一输入得到同一 f64。

/// 分布桶数（1..355）—— `dist.go:18`。
pub const DIST_BUCKETS: usize = 355;

/// 固定采样 prompt（**不动！基准依赖逐字节稳定**）—— `dist.go:21`。
pub const DIST_PROMPT: &str =
    "请从1到355之间随机选择一个数字，只输出这个数字，不要有任何其他内容。";

/// 采样上限 —— `dist.go:24`。
pub const DIST_MAX_SAMPLES: usize = 200;

/// 分布统计摘要 —— `distStats`（`dist.go:27-33`）。
///
/// `Deserialize` 给基准库加载用（`Baseline.dist.stats` 要从 `tuanjie-baselines.json`
/// 读回，P8b 第五片的基线内存态）。
#[derive(Debug, Clone, Serialize, serde::Deserialize, Default, PartialEq)]
pub struct DistStats {
    pub mean: f64,
    pub median: f64,
    pub std_dev: f64,
    pub mode: i64,
    pub mode_count: i64,
}

use serde::Serialize;

/// 两分布的相似度 —— `distSimilarity`（`dist.go:50-56`）。
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct DistSimilarity {
    pub cosine: f64,
    pub js_divergence: f64,
    pub distrib_score: f64,
    pub mode_a: i64,
    pub mode_b: i64,
    pub mode_score: f64,
    pub overall: f64,
}

/// 解析模型回答为 1..355 的数字 —— `parseDistAnswer`（`dist.go:60-90`）。
///
/// 无效样本返回 0。逐条对齐 Go：
/// 1. `TrimSpace` → `Trim("。，.,、 \n\t\"'")`；
/// 2. 取**首个连续 ASCII 数字段**（按 Go 的字节下标语义）；
/// 3. 前一个字节是 `-` 视为负数 → 0；
/// 4. `Atoi` 失败或越界（<1 或 >355）→ 0。
pub fn parse_dist_answer(s: &str) -> i64 {
    let t = s.trim();
    let t = t.trim_matches(|c: char| {
        matches!(c, '。' | '，' | '.' | ',' | '、' | ' ' | '\n' | '\t' | '"' | '\'')
    });
    if t.is_empty() {
        return 0;
    }
    let bytes = t.as_bytes();
    let mut start: Option<usize> = None;
    let mut end = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if b.is_ascii_digit() {
            if start.is_none() {
                start = Some(i);
            }
            end = i + 1;
        } else if start.is_some() {
            break;
        }
    }
    let Some(start) = start else {
        return 0;
    };
    if start > 0 && bytes[start - 1] == b'-' {
        return 0;
    }
    // 数字段全是 ASCII，切片安全
    let lit = &t[start..end];
    match lit.parse::<i64>() {
        Ok(n) if (1..=DIST_BUCKETS as i64).contains(&n) => n,
        _ => 0,
    }
}

/// 从 counts（**下标 1..355**，长度 356）算 mean/median/stdDev/mode —— `distStatsOf`
/// （`dist.go:176-221`）。
pub fn dist_stats_of(counts: &[i64]) -> DistStats {
    let mut total: i64 = 0;
    let mut sum = 0.0f64;
    let mut mode: i64 = 0;
    let mut mode_kw: i64 = 0;
    for v in 1..=DIST_BUCKETS {
        let c = counts.get(v).copied().unwrap_or(0);
        if c == 0 {
            continue;
        }
        total += c;
        sum += (v as i64 * c) as f64;
        if c > mode_kw {
            mode = v as i64;
            mode_kw = c;
        }
    }
    if total == 0 {
        return DistStats::default();
    }
    let mean = sum / total as f64;

    // median：按次数展开后取中位（样本量 ≤200，展开无压力）—— Go 展开后 sort.Ints
    let mut expanded: Vec<i64> = Vec::with_capacity(total as usize);
    for v in 1..=DIST_BUCKETS {
        let c = counts.get(v).copied().unwrap_or(0);
        for _ in 0..c {
            expanded.push(v as i64);
        }
    }
    let n = expanded.len();
    let median = if n % 2 == 1 {
        expanded[n / 2] as f64
    } else {
        (expanded[n / 2 - 1] + expanded[n / 2]) as f64 / 2.0
    };

    // stdDev（总体方差）
    let mut var_sq = 0.0f64;
    for v in 1..=DIST_BUCKETS {
        let c = counts.get(v).copied().unwrap_or(0);
        if c == 0 {
            continue;
        }
        let d = v as f64 - mean;
        var_sq += c as f64 * d * d;
    }
    let std_dev = (var_sq / total as f64).sqrt();
    DistStats {
        mean: round2(mean),
        median: round2(median),
        std_dev: round2(std_dev),
        mode,
        mode_count: mode_kw,
    }
}

/// 当前分布 vs 基准分布的相似度 —— `CompareDist`（`dist.go:225-270`）。
///
/// `a` / `b` 是**下标 0..354** 的频率数组（下标 i ↔ 数字 i+1，即 `distResult.Counts` 的形态）。
pub fn compare_dist(a: &[i64], b: &[i64], valid_a: i64, valid_b: i64) -> DistSimilarity {
    let mut s = DistSimilarity::default();
    if valid_a < 40 || valid_b < 40 || a.len() != DIST_BUCKETS || b.len() != DIST_BUCKETS {
        return s;
    }
    // 余弦相似度（355 维频率向量）
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..DIST_BUCKETS {
        let x = a[i] as f64;
        let y = b[i] as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na > 0.0 && nb > 0.0 {
        s.cosine = round4(dot / (na.sqrt() * nb.sqrt()));
    }
    // JS 散度（m=(P+Q)/2；零概率项按 0·log0=0 处理）
    let ta = valid_a as f64;
    let tb = valid_b as f64;
    let mut js_div = 0.0f64;
    for i in 0..DIST_BUCKETS {
        let p = a[i] as f64 / ta;
        let q = b[i] as f64 / tb;
        let m = (p + q) / 2.0;
        if p > 0.0 {
            js_div += p * (p / m).log2();
        }
        if q > 0.0 {
            js_div += q * (q / m).log2();
        }
    }
    js_div /= 2.0;
    s.js_divergence = round4(js_div);
    // 注意：Go 用的是**已 round4 的** Cosine 与**未 round 的** jsDiv（`dist.go:257`）
    s.distrib_score = round4(s.cosine * (-js_div).exp());

    // 众数分：1−|diff|/50，相等=1.0（负值截 0）
    let stats_a = dist_stats_of(&pad_counts(a));
    let stats_b = dist_stats_of(&pad_counts(b));
    s.mode_a = stats_a.mode;
    s.mode_b = stats_b.mode;
    let mut mode_score = 1.0 - (abs_i64(s.mode_a - s.mode_b) as f64) / 50.0;
    if mode_score < 0.0 {
        mode_score = 0.0;
    }
    s.mode_score = round4(mode_score);
    s.overall = round4(0.5 * s.distrib_score + 0.5 * s.mode_score);
    s
}

/// `counts[0..354]` → 下标 1..355 的完整数组 —— `padCounts`（`dist.go:273-277`）。
pub fn pad_counts(c: &[i64]) -> Vec<i64> {
    let mut out = vec![0i64; DIST_BUCKETS + 1];
    for (i, v) in c.iter().take(DIST_BUCKETS).enumerate() {
        out[i + 1] = *v;
    }
    out
}

/// `abs`（`dist.go:279-284`）。
pub fn abs_i64(x: i64) -> i64 {
    if x < 0 {
        -x
    } else {
        x
    }
}

/// `round2`（`dist.go:286`）—— `math.Round(v*100)/100`。
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// `round4`（`dist.go:287`）—— `math.Round(v*10000)/10000`。
pub fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

/// 相似度 → 判定档 —— `distVerdict`（`dist.go:290-300`）。返回 `(pct, verdict)`。
pub fn dist_verdict(sim: &DistSimilarity) -> (f64, &'static str) {
    let pct = (sim.overall * 100.0).round();
    if pct >= 96.0 {
        (pct, "一致")
    } else if pct >= 90.0 {
        (pct, "轻微偏差")
    } else {
        (pct, "显著偏差")
    }
}

/// 判定档 → 灯色 —— `distVerdictLabel`（`dist.go:303-312`）。
pub fn dist_verdict_label(verdict: &str) -> &'static str {
    match verdict {
        "一致" => "green",
        "轻微偏差" => "yellow",
        _ => "red",
    }
}

/// 采样进度文案 —— `fmtDistSampleProgress`（`dist.go:315-317`）。
pub fn fmt_dist_sample_progress(done: i64, total: i64) -> String {
    format!("分布采样 {done}/{total}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_table_matches_go() {
        for (input, want) in [
            ("", 0i64),
            (" ", 0),
            ("123", 123),
            ("355", 355),
            ("356", 0),
            ("0", 0),
            ("1", 1),
            ("007", 7),
            ("  42  ", 42),
            ("42。", 42),
            ("答案是 77", 77),
            ("-5", 0),
            ("- 5", 5),
            ("3.5", 3),
            ("abc", 0),
            ("2a", 2),
            ("1 2", 1),
            ("\"5\"", 5),
            ("３５５", 0),
            ("3555", 0),
            ("99999999999999999999", 0),
            ("100\n", 100),
            ("第 200 个", 200),
            ("1,2", 1),
            ("＋7", 7),
        ] {
            assert_eq!(parse_dist_answer(input), want, "parseDistAnswer({input:?})");
        }
    }

    #[test]
    fn stats_table_matches_go() {
        let mut counts = vec![0i64; DIST_BUCKETS + 1];
        counts[1] = 1;
        counts[2] = 2;
        counts[3] = 3;
        let s = dist_stats_of(&counts);
        assert_eq!(
            (s.mean, s.median, s.std_dev, s.mode, s.mode_count),
            (2.33, 2.5, 0.75, 3, 3)
        );

        let mut c2 = vec![0i64; DIST_BUCKETS + 1];
        c2[100] = 5;
        c2[200] = 3;
        let s2 = dist_stats_of(&c2);
        assert_eq!(
            (s2.mean, s2.median, s2.std_dev, s2.mode, s2.mode_count),
            (137.5, 100.0, 48.41, 100, 5)
        );

        assert_eq!(dist_stats_of(&vec![0i64; DIST_BUCKETS + 1]), DistStats::default());
    }

    #[test]
    fn compare_table_matches_go() {
        let mut a = vec![0i64; DIST_BUCKETS];
        let mut b = vec![0i64; DIST_BUCKETS];
        a[0] = 10;
        a[1] = 30;
        b[0] = 10;
        b[1] = 30;
        let s = compare_dist(&a, &b, 40, 40);
        assert_eq!((s.cosine, s.distrib_score, s.js_divergence, s.overall), (1.0, 1.0, 0.0, 1.0));

        // insufficient：任一侧 valid < 40 → 全 0
        let z = compare_dist(&a, &b, 39, 40);
        assert_eq!(z, DistSimilarity::default());

        let mut c = vec![0i64; DIST_BUCKETS];
        let mut d = vec![0i64; DIST_BUCKETS];
        c[0] = 20;
        d[1] = 20;
        let s2 = compare_dist(&c, &d, 40, 40);
        assert_eq!((s2.js_divergence, s2.mode_a, s2.mode_b, s2.mode_score, s2.overall), (0.5, 1, 2, 0.98, 0.49));
    }

    #[test]
    fn verdict_and_rounding_match_go() {
        assert_eq!(round2(1.005), 1.0);
        assert_eq!(round4(0.12345), 0.1235);
        assert_eq!(round2(0.125), 0.13);
        assert_eq!(round4(0.00005), 0.0001);
        assert_eq!(2.5f64.round(), 3.0);
        let (pct, v) = dist_verdict(&DistSimilarity {
            overall: 0.955,
            ..Default::default()
        });
        assert_eq!((pct, v), (96.0, "一致"));
        assert_eq!(dist_verdict_label(v), "green");
        assert_eq!(dist_verdict_label("不是档位"), "red");
        assert_eq!(fmt_dist_sample_progress(3, 60), "分布采样 3/60");
    }
}
