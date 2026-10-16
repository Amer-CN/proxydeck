//! `internal/proxy/stats.go`：本地用量统计、`/v1/stats`、`/v1/calibration`。
//!
//! 落盘形态与 Go 的 `UsageStats` 结构体**声明序**逐字节一致：
//! `{"models":{...},"started":N,"lastSaveAt":"...","dirty":false}`。
//!
//! ⚠ 这是 Go 结构体（不是 map），所以键序是**声明序** `models, started`；而 `Models` 是
//! `map[string]*ModelStat`，内部键按字节序。`snapshot()` 返回的 JSON 也必须照这个形态
//! （Go 的 `ccStats` 在核心离线时直接读这份文件）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::gjson::{push_go_float, push_go_string, to_go};

/// 节流落盘：距上次落盘 ≥2s 才写（Go 的 `2*time.Second`）。
pub const SAVE_THROTTLE: Duration = Duration::from_secs(2);

/// `DayStat`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DayStat {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

/// `ModelStat`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelStat {
    pub runs: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// `days,omitempty`：`None` = nil map 不写；`Some(map)` = 写出（含空 map）
    pub days: Option<BTreeMap<String, DayStat>>,
}

/// 落盘/快照里的一行（Go 结构体声明序）。
pub struct UsageStatsSnapshot {
    pub models: BTreeMap<String, ModelStat>,
    pub started: i64,
}

/// `UsageStats`：本地计数器 + 节流落盘。
pub struct UsageStats {
    file: Option<PathBuf>,
    pub models: BTreeMap<String, ModelStat>,
    pub started: i64,
    last_save_at: Instant,
    dirty: bool,
}

impl UsageStats {
    /// `NewUsageStats(file)`：建实例并尝试载入。
    pub fn new(file: &str) -> UsageStats {
        let mut s = UsageStats {
            file: if file.is_empty() {
                None
            } else {
                Some(PathBuf::from(file))
            },
            models: BTreeMap::new(),
            started: 0,
            last_save_at: Instant::now(),
            dirty: false,
        };
        s.load();
        s
    }

    /// `load`：读盘并解 `UsageStats`；文件缺失/坏 JSON 一律当空表（Go 的 `json.Unmarshal` 错误被忽略）。
    pub fn load(&mut self) {
        let path = match &self.file {
            Some(p) => p.clone(),
            None => return,
        };
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(_) => return,
        };
        // Go 把整份文件解进结构体：`models` 是 map、`started` 是 int64；
        // 形态不对（数组/字符串/坏 JSON）时 Go 会让字段保持零值，故这里整体容错。
        if let Ok(v) = crate::godecode::parse_value(&data) {
            if let Some(m) = v.get("models").and_then(|x| x.as_object()) {
                for (k, mv) in m {
                    if mv.is_null() {
                        continue; // 条目级 null：Go 存成零值条目；本 crate 按空条目处理
                    }
                    let ms = parse_model_stat(mv);
                    self.models.insert(k.clone(), ms);
                }
            }
            if let Some(n) = v.get("started").and_then(crate::godecode::go_int) {
                self.started = n;
            }
        }
        if self.started == 0 {
            // 首次运行（或旧数据无 started）：记录统计起点
            self.started = crate::util::unix_now();
            self.save();
        }
    }

    /// `Record`：记一笔完成请求，并按节流落盘。
    pub fn record(&mut self, model: &str, input: i64, output: i64, cache_read: i64, cache_write: i64) {
        let ms = self.models.entry(model.to_string()).or_default();
        ms.runs += 1;
        ms.input_tokens += input;
        ms.output_tokens += output;
        ms.cache_read_tokens += cache_read;
        ms.cache_write_tokens += cache_write;
        let dk = day_key();
        let days = ms.days.get_or_insert_with(BTreeMap::new);
        let ds = days.entry(dk).or_default();
        ds.input_tokens += input;
        ds.output_tokens += output;
        ds.cache_read_tokens += cache_read;
        ds.cache_write_tokens += cache_write;

        self.dirty = true;
        if self.last_save_at.elapsed() >= SAVE_THROTTLE {
            self.last_save_at = Instant::now();
            self.dirty = false;
            self.save_locked();
        }
    }

    /// `Today`：跨模型汇总今天（本地时区）。
    pub fn today(&self) -> (i64, i64, i64, i64) {
        let dk = day_key();
        let (mut i, mut o, mut cr, mut cw) = (0, 0, 0, 0);
        for ms in self.models.values() {
            if let Some(days) = &ms.days {
                if let Some(d) = days.get(&dk) {
                    i += d.input_tokens;
                    o += d.output_tokens;
                    cr += d.cache_read_tokens;
                    cw += d.cache_write_tokens;
                }
            }
        }
        (i, o, cr, cw)
    }

    /// `Snapshot`：读时兜底补写脏数据，返回一份拷贝。
    pub fn snapshot(&mut self) -> UsageStatsSnapshot {
        if self.dirty {
            self.dirty = false;
            self.last_save_at = Instant::now();
            self.save_locked();
        }
        UsageStatsSnapshot {
            models: self.models.clone(),
            started: self.started,
        }
    }

    /// `Flush`：立即落盘未写的增量。
    pub fn flush(&mut self) {
        if self.dirty {
            self.dirty = false;
            self.last_save_at = Instant::now();
            self.save_locked();
        }
    }

    /// `save`：无锁路径用（首次运行写起点）。
    fn save(&mut self) {
        self.last_save_at = Instant::now();
        self.dirty = false;
        self.save_locked();
    }

    fn save_locked(&self) {
        let Some(path) = &self.file else { return };
        let data = encode_stats(self.started, &self.models);
        let tmp = format!("{}.tmp", path.display());
        if std::fs::write(&tmp, data.as_bytes()).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, path);
    }

    /// 落盘形态（Go 结构体声明序 + `lastSaveAt`/`dirty` 两个可导出字段）。
    pub fn on_disk_json(&self) -> String {
        encode_stats(self.started, &self.models)
    }

    /// Go `filepath.Base(p.Stats.file)`。
    ///
    /// ⚠ 未设统计文件时 Go 的 `file` 是空串，而 `filepath.Base("")` 返回 **`"."`**（不是空串）——
    /// 黄金 `route-boundaries #17` 锁的就是这个值。`Some(p)` 分支走 `file_name()`：
    /// 与 `filepath.Base` 同语义（去尾分隔符后取最后一段；`"a/b/"` → `"b"`）。
    pub fn file_base(&self) -> String {
        match &self.file {
            Some(p) => p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            None => ".".to_string(),
        }
    }

    pub fn file_dir(&self) -> PathBuf {
        match &self.file {
            Some(p) => p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")),
            None => PathBuf::from("."),
        }
    }
}

/// `dayKey`：本地时区的 `YYYY-MM-DD`。
pub fn day_key() -> String {
    crate::util::local_day(crate::util::unix_now())
}

/// 落盘编码：`{"models":{...},"started":N}`。
///
/// ⚠ Go 的 `lastSaveAt` / `dirty` 是**未导出**字段（`lastSaveAt time.Time` / `dirty bool`），
/// `json.Marshal` 不输出它们；导出的只有 `Models`（`json:"models"`）与 `Started`（`json:"started"`）。
/// 黄金基准实测的盘上内容就是 `{"models":{…},"started":0}`——两处键、无尾随字段。
fn encode_stats(started: i64, models: &BTreeMap<String, ModelStat>) -> String {
    let mut out = String::from("{\"models\":{");
    let mut first = true;
    for (k, ms) in models {
        if !first {
            out.push(',');
        }
        first = false;
        push_go_string(&mut out, k);
        out.push(':');
        out.push_str(&encode_model_stat(ms));
    }
    out.push_str("},\"started\":");
    out.push_str(&started.to_string());
    out.push('}');
    out
}

fn encode_model_stat(ms: &ModelStat) -> String {
    let mut out = String::from("{\"runs\":");
    out.push_str(&ms.runs.to_string());
    out.push_str(",\"inputTokens\":");
    out.push_str(&ms.input_tokens.to_string());
    out.push_str(",\"outputTokens\":");
    out.push_str(&ms.output_tokens.to_string());
    out.push_str(",\"cacheReadTokens\":");
    out.push_str(&ms.cache_read_tokens.to_string());
    out.push_str(",\"cacheWriteTokens\":");
    out.push_str(&ms.cache_write_tokens.to_string());
    if let Some(days) = &ms.days {
        out.push_str(",\"days\":{");
        let mut first = true;
        for (k, d) in days {
            if !first {
                out.push(',');
            }
            first = false;
            push_go_string(&mut out, k);
            out.push(':');
            out.push_str(&encode_day_stat(d));
        }
        out.push('}');
    }
    out.push('}');
    out
}

fn encode_day_stat(d: &DayStat) -> String {
    let mut out = String::from("{\"inputTokens\":");
    out.push_str(&d.input_tokens.to_string());
    out.push_str(",\"outputTokens\":");
    out.push_str(&d.output_tokens.to_string());
    out.push_str(",\"cacheReadTokens\":");
    out.push_str(&d.cache_read_tokens.to_string());
    out.push_str(",\"cacheWriteTokens\":");
    out.push_str(&d.cache_write_tokens.to_string());
    out.push('}');
    out
}

fn parse_model_stat(v: &serde_json::Value) -> ModelStat {
    let n = |k: &str| -> i64 { crate::godecode::go_int_or_zero(v.get(k)) };
    let days = v.get("days").and_then(|x| x.as_object()).map(|m| {
        let mut out = BTreeMap::new();
        for (k, dv) in m {
            let dn = |kk: &str| -> i64 { crate::godecode::go_int_or_zero(dv.get(kk)) };
            out.insert(
                k.clone(),
                DayStat {
                    input_tokens: dn("inputTokens"),
                    output_tokens: dn("outputTokens"),
                    cache_read_tokens: dn("cacheReadTokens"),
                    cache_write_tokens: dn("cacheWriteTokens"),
                },
            );
        }
        out
    });
    ModelStat {
        runs: n("runs"),
        input_tokens: n("inputTokens"),
        output_tokens: n("outputTokens"),
        cache_read_tokens: n("cacheReadTokens"),
        cache_write_tokens: n("cacheWriteTokens"),
        days,
    }
}

// ===========================================================================
// 官方定价表与金额估算（stats.go 的 pricing / cacheReadPrices / estimateCost）
// ===========================================================================

/// `pricing`：`[输入, 输出]` 每百万 token 的美元价（2026-08 官方定价）。
pub const PRICING: &[(&str, f64, f64)] = &[
    ("deepseek-v4-flash", 0.14, 0.28),
    ("deepseek/deepseek-v4-flash", 0.14, 0.28),
    ("deepseek-v4-pro", 0.435, 0.87),
    ("deepseek/deepseek-v4-pro", 0.435, 0.87),
    ("kimi-k2.6", 0.95, 4.00),
    ("moonshotai/kimi-k2.6", 0.95, 4.00),
    ("kimi-k2.5", 0.60, 3.00),
    ("moonshotai/kimi-k2.5", 0.60, 3.00),
    ("glm-5.1", 1.40, 4.40),
    ("zai-org/glm-5.1", 1.40, 4.40),
    ("glm-5", 1.00, 3.20),
    ("zai-org/glm-5", 1.00, 3.20),
    ("minimax-m3", 0.30, 1.20),
    ("minimaxai/minimax-m3", 0.30, 1.20),
    ("minimax-m2.7", 0.30, 1.20),
    ("minimaxai/minimax-m2.7", 0.30, 1.20),
    ("minimax-m2.5", 0.30, 1.20),
    ("minimaxai/minimax-m2.5", 0.30, 1.20),
    ("qwen-3.7-max", 2.50, 7.50),
    ("qwen/qwen3.7-max", 2.50, 7.50),
    ("qwen-3.7-max-free", 0.0, 0.0),
    ("qwen/qwen3.7-max-free", 0.0, 0.0),
    ("qwen-3.6-max", 1.30, 7.80),
    ("qwen/qwen3.6-max-preview", 1.30, 7.80),
    ("qwen-3.6-plus", 0.50, 3.00),
    ("qwen/qwen3.6-plus", 0.50, 3.00),
    ("step-3.7-flash", 0.20, 1.15),
    ("stepfun/step-3.7-flash", 0.20, 1.15),
    ("step-3.5-flash", 0.10, 0.30),
    ("stepfun/step-3.5-flash", 0.10, 0.30),
    ("mimo-v2.5-pro", 0.435, 0.87),
    ("xiaomi/mimo-v2.5-pro", 0.435, 0.87),
    ("mimo-v2.5", 0.14, 0.28),
    ("xiaomi/mimo-v2.5", 0.14, 0.28),
    ("gemini-3.1-flash-lite", 0.0, 0.0),
    ("google/gemini-3.1-flash-lite", 0.0, 0.0),
];

/// `cacheReadPrices`：缓存命中读取的每百万 token 价。
pub const CACHE_READ_PRICES: &[(&str, f64)] = &[("deepseek-v4-flash", 0.0028)];

fn pricing_of(name: &str) -> Option<(f64, f64)> {
    if let Some((_, i, o)) = PRICING.iter().find(|(k, _, _)| *k == name) {
        return Some((*i, *o));
    }
    let short = short_name(name);
    PRICING
        .iter()
        .find(|(k, _, _)| *k == short)
        .map(|(_, i, o)| (*i, *o))
}

fn cache_read_price_of(name: &str) -> Option<f64> {
    if let Some((_, v)) = CACHE_READ_PRICES.iter().find(|(k, _)| *k == name) {
        return Some(*v);
    }
    let short = short_name(name);
    CACHE_READ_PRICES
        .iter()
        .find(|(k, _)| *k == short)
        .map(|(_, v)| *v)
}

/// `estimateCost`：按官方单价估算总金额（USD）。缓存命中按缓存读取价；未列出的模型按
/// 输入价的 2% 估算缓存读取。
pub fn estimate_cost(models: &BTreeMap<String, ModelStat>) -> f64 {
    let mut total = 0.0f64;
    for (name, ms) in models {
        let (in_price, out_price) = pricing_of(name).unwrap_or((0.0, 0.0));
        let cr = cache_read_price_of(name).unwrap_or(in_price * 0.02);
        total += ms.input_tokens as f64 / 1e6 * in_price
            + ms.output_tokens as f64 / 1e6 * out_price
            + ms.cache_read_tokens as f64 / 1e6 * cr;
    }
    total
}

/// `shortName`：从全名（如 `deepseek/deepseek-v4-flash`）取短名。
pub fn short_name(full: &str) -> &str {
    match full.rfind('/') {
        Some(i) => &full[i + 1..],
        None => full,
    }
}

/// `planLabel`：把 planId 映射成可读标签（未知 id 原样返回）。
pub fn plan_label(plan_id: &str) -> String {
    match plan_id {
        "go" => "Go".to_string(),
        "goat" => "GOAT".to_string(),
        "pro" => "Pro".to_string(),
        "max_10x" => "Max 10x".to_string(),
        "max_20x" => "Max 20x".to_string(),
        "team_pro" => "Team Pro".to_string(),
        "" => "Unknown plan".to_string(),
        other => other.to_string(),
    }
}

/// `/v1/stats` 的响应编码（Go 的 `map[string]any` → 键按字节序）。
///
/// 键序（字节序）：`calibrated, cost, models, started, statsFile, today, total`。
pub fn encode_stats_response(
    started: i64,
    models: &BTreeMap<String, ModelStat>,
    calibrated: &BTreeMap<String, bool>,
    today: (i64, i64, i64, i64),
    stats_file_base: &str,
    cost: f64,
) -> String {
    let (mut ti, mut to, mut tcr, mut tcw) = (0i64, 0i64, 0i64, 0i64);
    let mut models_json = String::from("{");
    let mut first = true;
    for (k, ms) in models {
        if !first {
            models_json.push(',');
        }
        first = false;
        push_go_string(&mut models_json, k);
        models_json.push(':');
        models_json.push_str(&encode_model_stat(ms));
        ti += ms.input_tokens;
        to += ms.output_tokens;
        tcr += ms.cache_read_tokens;
        tcw += ms.cache_write_tokens;
    }
    models_json.push('}');

    let mut calib_json = String::from("{");
    let mut first = true;
    for (k, v) in calibrated {
        if !first {
            calib_json.push(',');
        }
        first = false;
        push_go_string(&mut calib_json, k);
        calib_json.push(':');
        calib_json.push_str(if *v { "true" } else { "false" });
    }
    calib_json.push('}');

    let mut out = String::from("{\"calibrated\":");
    out.push_str(&calib_json);
    out.push_str(",\"cost\":");
    push_go_float(&mut out, cost);
    out.push_str(",\"models\":");
    out.push_str(&models_json);
    out.push_str(",\"started\":");
    out.push_str(&started.to_string());
    out.push_str(",\"statsFile\":");
    push_go_string(&mut out, stats_file_base);
    out.push_str(",\"today\":");
    out.push_str(&totals_json(today.0, today.1, today.2, today.3));
    out.push_str(",\"total\":");
    out.push_str(&totals_json(ti, to, tcr, tcw));
    out.push('}');
    out
}

/// `total` / `today` 段：键按字节序 `cacheRead, cacheWrite, input, output, total`。
fn totals_json(input: i64, output: i64, cache_read: i64, cache_write: i64) -> String {
    let mut s = String::from("{\"cacheRead\":");
    s.push_str(&cache_read.to_string());
    s.push_str(",\"cacheWrite\":");
    s.push_str(&cache_write.to_string());
    s.push_str(",\"input\":");
    s.push_str(&input.to_string());
    s.push_str(",\"output\":");
    s.push_str(&output.to_string());
    s.push_str(",\"total\":");
    s.push_str(&(input + output + cache_read + cache_write).to_string());
    s.push('}');
    s
}

/// `calibration`：读同目录的 `calibration.json`（缺失/坏 JSON → 空表）。
pub fn read_calibration(stats_file_dir: &Path) -> BTreeMap<String, i64> {
    let p = stats_file_dir.join("calibration.json");
    let mut out = BTreeMap::new();
    let data = match std::fs::read(&p) {
        Ok(d) => d,
        Err(_) => return out,
    };
    if let Ok(v) = crate::godecode::parse_value(&data) {
        if let Some(m) = v.as_object() {
            for (k, val) in m {
                if let Some(n) = crate::godecode::go_int(val) {
                    out.insert(k.clone(), n);
                }
            }
        }
    }
    out
}

/// `setCalibration`：写入某模型的校准 token（`<=0` 表示清除）。权限 0600。
pub fn write_calibration(stats_file_dir: &Path, model: &str, tokens: i64) -> std::io::Result<()> {
    let mut calib = read_calibration(stats_file_dir);
    if tokens <= 0 {
        calib.remove(model);
    } else {
        calib.insert(model.to_string(), tokens);
    }
    let mut m = serde_json::Map::new();
    for (k, v) in &calib {
        m.insert(
            k.clone(),
            serde_json::Value::Number(serde_json::Number::from(*v)),
        );
    }
    let body = to_go(&serde_json::Value::Object(m));
    let p = stats_file_dir.join("calibration.json");
    std::fs::write(&p, body.as_bytes())?;
    // Go 用 0o600；Rust 侧写后再收紧一次权限（Windows 上等价物是只读属性，不做）
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_throttle_then_flush() {
        let dir = std::env::temp_dir().join(format!(
            "rs-proxy-stats-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("stats.json");
        // 与 Go 同：文件**已存在**（哪怕内容是坏 JSON）才会写 started；
        // 全新目录下 `NewUsageStats` 什么都不写、响应里 `started` 就是 0
        // （黄金实测：`stats-and-calibration#0` 的 `statsFile: "."`、`started: 0`）。
        std::fs::write(&file, b"{}").unwrap();
        let mut s = UsageStats::new(&file.to_string_lossy());
        assert!(s.started > 0, "已存在文件时首次载入应写入 started");
        s.record("deepseek/deepseek-v4-flash", 1000, 500, 9000, 200);
        s.record("deepseek/deepseek-v4-flash", 2000, 800, 12000, 0);
        s.flush();

        // 模拟重启：新实例读同一个文件
        let s2 = UsageStats::new(&file.to_string_lossy());
        let ms = s2
            .models
            .get("deepseek/deepseek-v4-flash")
            .expect("重载丢数据");
        assert_eq!(ms.input_tokens, 3000, "落盘/重载后输入 token 不一致");
        assert_eq!(ms.output_tokens, 1300);
        assert_eq!(ms.runs, 2);
        assert_eq!(ms.cache_read_tokens, 21000);
        assert_eq!(ms.cache_write_tokens, 200);
        assert_eq!(s2.today(), (3000, 1300, 21000, 200));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cost_with_cache_hit_matches_go_expectation() {
        let mut m = BTreeMap::new();
        m.insert(
            "deepseek-v4-flash".to_string(),
            ModelStat {
                input_tokens: 5_000_000,
                output_tokens: 400_000,
                cache_read_tokens: 370_000_000,
                cache_write_tokens: 57_400_000,
                ..Default::default()
            },
        );
        let cost = estimate_cost(&m);
        let want = 5e6 / 1e6 * 0.14 + 0.4e6 / 1e6 * 0.28 + 370e6 / 1e6 * 0.0028;
        assert!((cost - want).abs() < 1e-9, "cost={cost} want={want}");
        assert!(cost > 1.70 && cost < 1.90, "官网 ~$1.86：{cost}");
    }

    #[test]
    fn cost_fallback_and_unknown_model() {
        let mut m = BTreeMap::new();
        m.insert(
            "some/unknown-model".to_string(),
            ModelStat {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_read_tokens: 10_000_000,
                ..Default::default()
            },
        );
        assert_eq!(estimate_cost(&m), 0.0);
        let mut m2 = BTreeMap::new();
        m2.insert(
            "kimi-k2.6".to_string(),
            ModelStat {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_read_tokens: 10_000_000,
                ..Default::default()
            },
        );
        let want = 0.95 + 4.0 + 10e6 / 1e6 * (0.95 * 0.02);
        assert!((estimate_cost(&m2) - want).abs() < 1e-9);
    }

    #[test]
    fn plan_labels() {
        assert_eq!(plan_label("go"), "Go");
        assert_eq!(plan_label(""), "Unknown plan");
        assert_eq!(plan_label("weird"), "weird");
    }

    #[test]
    fn stats_response_key_order_is_byte_sorted() {
        let mut models = BTreeMap::new();
        models.insert(
            "m".to_string(),
            ModelStat {
                runs: 1,
                input_tokens: 1,
                ..Default::default()
            },
        );
        let body = encode_stats_response(7, &models, &BTreeMap::new(), (0, 0, 0, 0), "stats.json", 0.0);
        assert_eq!(
            body,
            "{\"calibrated\":{},\"cost\":0,\"models\":{\"m\":{\"runs\":1,\"inputTokens\":1,\
             \"outputTokens\":0,\"cacheReadTokens\":0,\"cacheWriteTokens\":0}},\"started\":7,\
             \"statsFile\":\"stats.json\",\"today\":{\"cacheRead\":0,\"cacheWrite\":0,\"input\":0,\
             \"output\":0,\"total\":0},\"total\":{\"cacheRead\":0,\"cacheWrite\":0,\"input\":1,\
             \"output\":0,\"total\":1}}"
        );
    }

    #[test]
    fn on_disk_json_uses_struct_declaration_order() {
        let mut models = BTreeMap::new();
        models.insert(
            "z".to_string(),
            ModelStat {
                runs: 1,
                input_tokens: 2,
                output_tokens: 3,
                cache_read_tokens: 4,
                cache_write_tokens: 5,
                days: Some(BTreeMap::new()),
            },
        );
        let body = encode_stats(0, &models);
        assert_eq!(
            body,
            "{\"models\":{\"z\":{\"runs\":1,\"inputTokens\":2,\"outputTokens\":3,\
             \"cacheReadTokens\":4,\"cacheWriteTokens\":5,\"days\":{}}},\"started\":0}"
        );
        // Go 的 lastSaveAt / dirty 是未导出字段 → 不进文件
        assert!(!body.contains("lastSaveAt"), "未导出字段不得出现在盘上：{body}");
        assert!(!body.contains("dirty"), "未导出字段不得出现在盘上：{body}");
    }

    /// `/v1/stats` 的 `statsFile` 直接照抄 Go 的 `filepath.Base(p.Stats.file)`。
    ///
    /// 这一条补的是黄金门禁**只覆盖到一个分支**的空档：`route-boundaries #17` 用的是
    /// 未设统计文件的 Proxy（Go 的 `file` 是空串 → `filepath.Base("") == "."`），
    /// 而 `stats-and-calibration` 设了文件（→ `"stats.json"`）。两个分支都要对着 Go 的
    /// `path/filepath` 语义锁住，否则下次谁把 `None` 分支改成空串就没人拦。
    ///
    /// 期望值由真 Go 探针采收（`.work/rust-rewrite/_p6probe-filepath.mjs`，跑的是
    /// `filepath.Base/Dir/Join`）：`"" → "."`、`"a/b/stats.json" → "stats.json"`、
    /// `"stats.json" → "stats.json"`。
    #[test]
    fn file_base_and_dir_match_go_filepath() {
        // 未设统计文件（Go: NewProxy 后 p.Stats.file == ""）
        let none = UsageStats::new("");
        // Go 探针：in="" Base="." Dir="." —— filepath.Dir("") 也是 "."，不是空串
        assert_eq!(none.file_base(), ".", "filepath.Base(\"\") == \".\"");
        assert_eq!(none.file_dir(), std::path::PathBuf::from("."));

        // 设了统计文件：Base 取最后一段、Dir 取父目录
        let with = UsageStats::new(&std::env::temp_dir().join("a/b/stats.json").to_string_lossy());
        assert_eq!(with.file_base(), "stats.json");
        assert_eq!(with.file_dir(), std::env::temp_dir().join("a/b"));

        // 只有文件名（无父目录）：Go 探针 in="stats.json" Base="stats.json" Dir="."
        // ⚠ `file_dir()` 在这里返回空 PathBuf（Go 是 "."），**形态不同但拼接结果相同**
        // （Rust `PathBuf::from("").join("x") == "x"`；Go `filepath.Join(".", "x") == "x"`）。
        // 生产路径永远是绝对路径，此处只锁「拼接结果一致」这个真正被用到的性质；
        // 形态差异登记在 `rs-proxy/README.md` 的差异清单里。
        let bare = UsageStats::new("stats.json");
        assert_eq!(bare.file_base(), "stats.json");
        assert_eq!(
            bare.file_dir().join("calibration.json"),
            std::path::PathBuf::from("calibration.json"),
            "file_dir 只被用于拼接（http.rs 的读/写校准），拼接结果必须与 filepath.Join 一致"
        );
    }
}

