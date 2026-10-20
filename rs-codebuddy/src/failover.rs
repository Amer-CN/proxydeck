//! failover.rs —— hy4-preview 限流兜底配置与判定，对应 Go `internal/codebuddy/server.go`
//! 的 `failoverCfg` / `loadFailover` / `saveFailover` / `hy4LimitState` / `handleFailover`
//! / `parseQuotaReset` / `reasoningDefaultFor` / `failoverDecide` / `hy4Limited` / `hy4Trip`。
//!
//! 与 Go 的结构差异（登记在 README）：Go 把这些状态散在 `Server` 的 `failMu`/`limitMu`
//! 两把锁后面；Rust 侧收进一个 `Mutex<State>`（配置 + 限流窗口），语义同（各自互斥、
//! 懒解除）。
//!
//! 缺省值（Go `loadFailover` 的初始赋值）：`enabled=true` / `fallback=deepseek-v4-pro` /
//! `default_reasoning=max`。文件缺失或损坏（解析失败或 fallback 为空）保持缺省；
//! `default_reasoning` 缺字段或不在合法档位集合时落缺省 `max`。**生产缺省 ≠ 零值**，
//! 这一点是 P5b 明确点名的坑（P5a 的 deferred 黄金记的是零值 Server 的 `enabled:false`）。

use std::sync::Mutex;
use std::time::Instant;

use serde_json::Value;

use crate::gjson;
use crate::gotime;
use crate::models;

/// Go `hy4Primary`。
pub const HY4_PRIMARY: &str = "hy4-preview";
/// Go `hy4Fallback`（只作缺省值）。
pub const HY4_FALLBACK: &str = "deepseek-v4-pro";
/// Go `defaultReasoningTier`。
pub const DEFAULT_REASONING_TIER: &str = "max";

/// Go `reasoningTiers`：上游 `reasoning_effort` 合法档位。
pub fn is_reasoning_tier(t: &str) -> bool {
    matches!(t, "off" | "low" | "medium" | "high" | "max")
}

/// Go `failoverConfig`（内存配置快照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverConfig {
    pub enabled: bool,
    pub fallback: String,
    pub default_reasoning: String,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        FailoverConfig {
            enabled: true,
            fallback: HY4_FALLBACK.to_string(),
            default_reasoning: DEFAULT_REASONING_TIER.to_string(),
        }
    }
}

/// Go `failoverDecision`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverDecision {
    /// 请求前改写模型。
    pub rewrite: bool,
    /// 记限流窗口并重放。
    pub trip: bool,
    /// 窗口重置时点（epoch 毫秒；`i64::MIN` = Go 零值；`trip=true` 时有效）。
    pub until_ms: i64,
}

impl Default for FailoverDecision {
    fn default() -> Self {
        FailoverDecision {
            rewrite: false,
            trip: false,
            until_ms: i64::MIN,
        }
    }
}

/// 兜底配置 + hy4 限流窗口（Go 的 `failMu`/`limitMu` 两段状态）。
pub struct Failover {
    inner: Mutex<Inner>,
    path: String,
}

struct Inner {
    cfg: FailoverConfig,
    limit_until_ms: i64,
}

impl Failover {
    /// Go `loadFailover`：启动读回；文件缺失/损坏用缺省值。
    pub fn load(path: &str) -> Failover {
        let mut cfg = FailoverConfig::default();
        if !path.is_empty() {
            if let Ok(b) = std::fs::read(path) {
                if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                    let fallback = v
                        .get("fallback")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !fallback.is_empty() {
                        cfg.enabled = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
                        cfg.fallback = fallback;
                        if let Some(t) = v.get("default_reasoning").and_then(|x| x.as_str()) {
                            if is_reasoning_tier(t) {
                                cfg.default_reasoning = t.to_string();
                            }
                        }
                    }
                }
            }
        }
        Failover {
            inner: Mutex::new(Inner {
                cfg,
                limit_until_ms: i64::MIN,
            }),
            path: path.to_string(),
        }
    }

    pub fn config(&self) -> FailoverConfig {
        lock(&self.inner).cfg.clone()
    }

    /// Go `saveFailover`：原子落盘（`Marshal` + tmp/rename）。
    pub fn save(&self) {
        if self.path.is_empty() {
            return;
        }
        let cfg = self.config();
        let v = gjson::obj(vec![
            ("default_reasoning", gjson::s(&cfg.default_reasoning)),
            ("enabled", Value::Bool(cfg.enabled)),
            ("fallback", gjson::s(&cfg.fallback)),
        ]);
        let body = gjson::to_go_bytes(&v);
        let tmp = format!("{}.tmp", self.path);
        if std::fs::write(&tmp, &body).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// Go `hy4LimitState`：限流窗口快照（无窗口/已到期返回 `None`；到期顺带懒清除）。
    pub fn limit_state(&self) -> Option<Value> {
        let mut g = lock(&self.inner);
        if g.limit_until_ms == i64::MIN {
            return None;
        }
        if gotime::now_millis() > g.limit_until_ms {
            g.limit_until_ms = i64::MIN;
            return None;
        }
        let fallback = g.cfg.fallback.clone();
        let until = g.limit_until_ms;
        Some(gjson::obj(vec![
            ("active", Value::Bool(true)),
            ("until", gjson::s(format_full_display(until))),
            ("fallback", gjson::s(fallback)),
        ]))
    }

    /// Go `hy4Limited`：报告 hy4-preview 是否处于限流窗口；到期即自动解除（懒检查）。
    pub fn hy4_limited(&self) -> bool {
        let mut g = lock(&self.inner);
        if g.limit_until_ms == i64::MIN {
            return false;
        }
        if gotime::now_millis() > g.limit_until_ms {
            println!("[codebuddy] hy4-preview 限流窗口到期，自动恢复");
            g.limit_until_ms = i64::MIN;
            return false;
        }
        true
    }

    /// Go `hy4Trip`：记录一次限流窗口（重置时点来自上游报文）。
    pub fn hy4_trip(&self, until_ms: i64) {
        lock(&self.inner).limit_until_ms = until_ms;
    }

    /// `POST /v1/failover` 的写侧：`fallback` 非 `None` 时先校验是否命中候选池，
    /// 命中才写入；返回 `Err(msg)` 表示应回 400。
    pub fn update(
        &self,
        enabled: Option<bool>,
        fallback: Option<String>,
        candidates: &[&str],
    ) -> Result<FailoverConfig, String> {
        if let Some(f) = &fallback {
            if !candidates.iter().any(|m| m == f) {
                return Err(format!("unknown fallback model: {f}"));
            }
        }
        let mut g = lock(&self.inner);
        if let Some(f) = fallback {
            g.cfg.fallback = f;
        }
        if let Some(e) = enabled {
            g.cfg.enabled = e;
        }
        Ok(g.cfg.clone())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Go `hy4LimitState` 的 `until` 形态：`2006-01-02 15:04:05`（本地时区）。
pub fn format_full_display(ms: i64) -> String {
    if ms == i64::MIN {
        return String::new();
    }
    let off = gotime::local_offset_secs();
    let rfc = gotime::format_rfc3339_seconds(ms, off);
    // "2026-10-15T06:20:02+08:00" → "2026-10-15 06:20:02"
    rfc.replace('T', " ").chars().take(19).collect()
}

/// Go `MarkLimited` / 换号日志里的 `until.Format("01-02 15:04:05")`（秒精度）。
pub fn format_reset_display(ms: i64) -> String {
    let full = format_full_display(ms);
    if full.len() == 19 {
        full[5..].to_string()
    } else {
        full
    }
}

/// Go `hy4 trip` 日志里的 `d.until.Format("01-02 15:04")`（**秒精度不带秒**，
/// `server.go:1441`）——与上面那条差一个精度，别混用。
pub fn format_reset_display_minute(ms: i64) -> String {
    let full = format_full_display(ms);
    if full.len() == 19 {
        full[5..16].to_string()
    } else {
        full
    }
}

/// Go `resetTimeRe`：上游 429 报文里的重置时点。
fn find_reset_time(body: &str) -> Option<(usize, usize)> {
    let b = body.as_bytes();
    let digits = |c: u8| c.is_ascii_digit();
    let mut i = 0usize;
    while i + 19 <= b.len() {
        let ok = b[i..i + 4].iter().all(|&c| digits(c))
            && b[i + 4] == b'-'
            && b[i + 5..i + 7].iter().all(|&c| digits(c))
            && b[i + 7] == b'-'
            && b[i + 8..i + 10].iter().all(|&c| digits(c))
            && b[i + 10] == b' '
            && b[i + 11..i + 13].iter().all(|&c| digits(c))
            && b[i + 13] == b':'
            && b[i + 14..i + 16].iter().all(|&c| digits(c))
            && b[i + 16] == b':'
            && b[i + 17..i + 19].iter().all(|&c| digits(c));
        if ok {
            return Some((i, i + 19));
        }
        i += 1;
    }
    None
}

/// Go `parseQuotaReset`：从上游 429 报文提取限流重置时间（上游报 UTC+8，按固定东八区
/// 解析）。非配额类 429 或解析不出时点返回 `None`（调用方如实透传）。
///
/// 关键字三族：CN 配额 `Quota exceeded`、CN 6004 中文 `频率限制`、INTL 6004 英文
/// `frequency limit`（大小写不敏感——国际版报文是 `usage exceeds frequency limit`）。
pub fn parse_quota_reset(body: &str) -> Option<i64> {
    if !body.contains("Quota exceeded")
        && !body.contains("频率限制")
        && !body.to_lowercase().contains("frequency limit")
    {
        return None;
    }
    let (s, e) = find_reset_time(body)?;
    // Go：`time.ParseInLocation("2006-01-02 15:04:05", m[1], time.FixedZone("UTC+8", 8*3600))`
    // ——字面量**就是**东八区墙上时间，解析结果即该时刻的绝对时间（不加减偏移）。
    // 这里等价实现：把字面量当作带 `+08:00` 偏移的 RFC3339 解析。
    gotime::parse_rfc3339(&format!(
        "{}T{}+08:00",
        &body[s..s + 10],
        &body[s + 11..e]
    ))
}

/// Go `isExhaustedBody`：判上游返回体是否为额度耗尽（14018）：只认业务码。
pub fn is_exhausted_body(body: &str) -> bool {
    !body.is_empty() && body.contains("14018")
}

/// 流内嗅探（200 响应头之后、发出 SSE 之前）的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamVerdict {
    /// 看起来是正常流（有 `data:` 行，且不是「只有 `[DONE]`」）。
    Ok,
    /// 上游在 200 里塞了已知业务错误码（封禁/结构/抖动/额度/频控）。
    ErrCode(u32),
    /// 空流：非空但一行 `data:` 都没有，或只有 `[DONE]` 而没有任何 content/tool_calls。
    EmptyStream,
}

/// 流式响应体里出现即判「假成功」的已知业务码。
///
/// 与非流式路径的 14018/11128/11133 治理同源，但**判错只换号、不进冷却**：
/// 200 里的错误帧往往是上游抖动或单号问题，把它当账号级封禁会级联耗竭整池。
pub const STREAM_ERR_CODES: &[u32] = &[11128, 11129, 11133, 11102, 11115, 6004];

/// `head` 里是否出现**独立**的业务码 token。
///
/// 与 `chat::is_flagged_body` 的注释同口径：裸子串会把 requestId/正文里的同数字串误判
///（`16004` 里也含 `6004`）。这里只加一条「前后都不是 ASCII 数字」的边界，仍是子串检测
///（业务码本身就是独立数字 token），不改变判定语义。
fn contains_code_token(haystack: &str, code: u32) -> bool {
    let needle = code.to_string();
    let n = needle.as_bytes();
    let h = haystack.as_bytes();
    let mut i = 0usize;
    while i + n.len() <= h.len() {
        if &h[i..i + n.len()] == n {
            let before = if i == 0 { None } else { Some(h[i - 1]) };
            let after = h.get(i + n.len()).copied();
            let is_digit = |c: Option<u8>| c.map(|b| b.is_ascii_digit()).unwrap_or(false);
            if !is_digit(before) && !is_digit(after) {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// 判定上游 200 响应体的开头（peek 窗口）是不是「假成功」——按序：
///
/// 1. 含已知业务码 ⇒ `ErrCode(码)`（错误帧可能带 `data:` 行，也可能不带，两种形态都认）；
/// 2. 非空但一行 `data:` 都没有 ⇒ `EmptyStream`（HTML 错误页 / JSON 错体 / 空体占位）；
/// 3. 含 `data: [DONE]` 但整个 head 里既无 `content` 也无 `tool_calls` ⇒ `EmptyStream`
///    （流「正常结束」了却一个回答都没有）；
/// 4. 其余 ⇒ `Ok`。
///
/// 空 `head` 按字面落 `Ok`（规则 2 明写「非空但…」）——调用方把它当正常流透传。
///
/// `truncated`：head 是否**没读完**就被截断（时间预算到点 / 窗口未满）。为 `true` 时
/// 规则 2 不生效——半截的 `"dat"` 是 `data:` 的前缀，不是「上游没发 SSE」。
pub fn classify_stream_head(head: &str, truncated: bool) -> StreamVerdict {
    // ① 业务码优先：这是唯一能判「上游明确报错」的信号，比形态判定可靠。
    for code in STREAM_ERR_CODES {
        if contains_code_token(head, *code) {
            return StreamVerdict::ErrCode(*code);
        }
    }
    // ② 一行 data: 都没有 ⇒ 上游根本没在发 SSE。
    //
    // ⚠ `truncated == true` 时**不能**据此判空流：peek 被时间预算截断时，head 可能
    // 恰好停在 `data:` 的半截前缀（如 `"dat"`），那是「还没读全」而不是「上游没发」。
    // 这种情形只认规则 ① 的明确业务码，其余一律按 Ok 透传（宁可漏判，不可误换号）。
    let has_data_line = head
        .lines()
        .any(|l| l.trim_start().starts_with("data:"));
    if !head.is_empty() && !has_data_line && !truncated {
        return StreamVerdict::EmptyStream;
    }
    // ③ 只有结束标记：`[DONE]` 前后都没有任何实际载荷。
    let has_done = head.contains("data: [DONE]") || head.contains("data:[DONE]");
    if has_done && !head.contains("content") && !head.contains("tool_calls") {
        return StreamVerdict::EmptyStream;
    }
    StreamVerdict::Ok
}

/// Go `reasoningDefaultFor`：缺省 `reasoning_effort` 注入判定（纯逻辑）。
/// 返回 `None` 表示不注入：模型无元数据 / reasoning 矩阵缺失，或配置档位不在该模型矩阵。
pub fn reasoning_default_for(model: &str, cfg: &str) -> Option<String> {
    if cfg.is_empty() {
        return None;
    }
    let matrix = models::reasoning_matrix(model)?;
    if matrix.split('/').any(|t| t == cfg) {
        Some(cfg.to_string())
    } else {
        None
    }
}

/// Go `failoverDecide`：兜底守卫纯逻辑。`cfg.enabled=false` 时改写与 trip 一律不触发。
pub fn failover_decide(
    cfg: &FailoverConfig,
    sent_model: &str,
    limited: bool,
    status: u16,
    body: &str,
) -> FailoverDecision {
    let mut d = FailoverDecision {
        until_ms: i64::MIN,
        ..Default::default()
    };
    if !cfg.enabled || sent_model != HY4_PRIMARY {
        return d;
    }
    if limited {
        d.rewrite = true;
    }
    if status == 429 {
        if let Some(until) = parse_quota_reset(body) {
            d.trip = true;
            d.until_ms = until;
        }
    }
    d
}

/// `/v1/failover` 的 `hy4_limit` 键值（无窗口时 Go 写 `null`）。
pub fn limit_state_or_null(f: &Failover) -> Value {
    f.limit_state().unwrap_or(Value::Null)
}

/// 供 `Instant` 版本的时间点做「已到期」判定的辅助（单测用）。
pub fn elapsed_since(start: Instant) -> i64 {
    start.elapsed().as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_production_not_zero() {
        let f = Failover::load("");
        let c = f.config();
        assert!(c.enabled, "生产缺省是 enabled=true（零值口径是 P5a 的坑）");
        assert_eq!(c.fallback, "deepseek-v4-pro");
        assert_eq!(c.default_reasoning, "max");
        assert!(f.limit_state().is_none());
        assert!(!f.hy4_limited());
    }

    #[test]
    fn broken_file_keeps_defaults_and_bad_tier_falls_back_to_max() {
        let dir = std::env::temp_dir().join("rs-codebuddy-failover-cases");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 坏 JSON → 缺省
        let p1 = dir.join("broken.json");
        std::fs::write(&p1, "{not json").unwrap();
        assert_eq!(
            Failover::load(&p1.to_string_lossy()).config(),
            FailoverConfig::default()
        );
        // fallback 空 → 缺省
        let p2 = dir.join("nofallback.json");
        std::fs::write(&p2, r#"{"enabled":true,"fallback":"","default_reasoning":"low"}"#).unwrap();
        assert_eq!(
            Failover::load(&p2.to_string_lossy()).config(),
            FailoverConfig::default()
        );
        // 合法档位生效；非法档位落回 max
        let p3 = dir.join("ok.json");
        std::fs::write(
            &p3,
            r#"{"enabled":false,"fallback":"glm-5.2","default_reasoning":"low"}"#,
        )
        .unwrap();
        let c = Failover::load(&p3.to_string_lossy()).config();
        assert!(!c.enabled);
        assert_eq!(c.fallback, "glm-5.2");
        assert_eq!(c.default_reasoning, "low");
        let p4 = dir.join("badtier.json");
        std::fs::write(
            &p4,
            r#"{"enabled":true,"fallback":"glm-5.2","default_reasoning":"turbo"}"#,
        )
        .unwrap();
        assert_eq!(
            Failover::load(&p4.to_string_lossy()).config().default_reasoning,
            "max"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_a_round_trip() {
        let dir = std::env::temp_dir().join("rs-codebuddy-failover-save");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("codebuddy-failover.json");
        let f = Failover::load(&p.to_string_lossy());
        f.update(Some(false), Some("glm-5.2".to_string()), &["glm-5.2", "hy3"])
            .unwrap();
        f.save();
        let raw = std::fs::read_to_string(&p).unwrap();
        // 落盘形态：Go `saveFailover` 是 `json.Marshal(failoverCfg{…})` ⇒ **字段声明序**
        // （`enabled,fallback,default_reasoning`）；Rust 走 `gjson::to_go`（等价
        // `map[string]any` 的语义）⇒ **键字母序**。两者是同一份内容的两种键序，
        // **不是逐字节一致**（登记在 README 差异 22）。这里按「解析后等价」判：
        // 先把两侧都解析成 `Value` 再比字段集合与取值（顺带把「Go 的 map 字段按键排序」
        // 那层语义也一并覆盖——本结构体没有 map 字段，纯声明序差异）。
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap(),
            serde_json::from_str::<Value>(
                r#"{"enabled":false,"fallback":"glm-5.2","default_reasoning":"max"}"#
            )
            .unwrap(),
            "落盘内容（键序无关）须与 Go json.Marshal(failoverCfg) 等价"
        );
        assert_eq!(
            raw,
            r#"{"default_reasoning":"max","enabled":false,"fallback":"glm-5.2"}"#,
            "Rust 侧的键序是字母序（与 Go 的字段声明序不同，见 README 差异 22）"
        );
        let back = Failover::load(&p.to_string_lossy()).config();
        assert!(!back.enabled);
        assert_eq!(back.fallback, "glm-5.2");
        // 非候选池模型 → 400
        assert!(f
            .update(None, Some("nope".to_string()), &["glm-5.2"])
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quota_reset_keyword_families() {
        // CN 配额（上游报 UTC+8，固定东八区解释）
        let cn = r#"{"code":6004,"msg":"Quota exceeded, will reset at 2026-10-15 06:20:02 UTC+8"}"#;
        let t = parse_quota_reset(cn).expect("CN 配额族应命中");
        assert_eq!(format_full_display(t), "2026-10-15 06:20:02");
        // 绝对值：`2026-10-15 06:20:02 +08:00` == `2026-10-14 22:20:02 UTC`
        let want = gotime::parse_rfc3339("2026-10-14T22:20:02Z").unwrap();
        assert_eq!(t, want, "UTC+8 解释错了（不许再减 8h）");
        // CN 中文
        let cn2 = "请求频率限制，将在 2026-10-15 06:20:02 UTC+8 重置";
        assert!(parse_quota_reset(cn2).is_some());
        // INTL 英文（大小写不敏感）
        let intl = r#"{"code":6004,"msg":"usage exceeds frequency limit, reset at 2026-10-15 06:20:02 UTC+8"}"#;
        assert!(parse_quota_reset(intl).is_some());
        assert!(parse_quota_reset("FREQUENCY LIMIT at 2026-10-15 06:20:02").is_some());
        // 无关键字 / 无时点
        assert!(parse_quota_reset(r#"{"code":6004,"msg":"rate limited"}"#).is_none());
        assert!(parse_quota_reset("Quota exceeded").is_none());
        assert!(parse_quota_reset("").is_none());
    }

    #[test]
    fn decide_matches_go_guard() {
        let cfg = FailoverConfig::default();
        // 非 hy4 → 永不动
        assert_eq!(
            failover_decide(&cfg, "glm-5.2", true, 429, "Quota exceeded 2026-10-15 06:20:02"),
            FailoverDecision::default()
        );
        // hy4 + 已在窗口 → rewrite
        let d = failover_decide(&cfg, HY4_PRIMARY, true, 0, "");
        assert!(d.rewrite && !d.trip);
        // hy4 + 429 配额报文 → trip
        let d = failover_decide(
            &cfg,
            HY4_PRIMARY,
            false,
            429,
            "Quota exceeded, will reset at 2026-10-15 06:20:02 UTC+8",
        );
        assert!(d.trip && !d.rewrite);
        assert_eq!(format_full_display(d.until_ms), "2026-10-15 06:20:02");
        // 关闭 → 一律不触发
        let off = FailoverConfig {
            enabled: false,
            ..cfg.clone()
        };
        assert_eq!(
            failover_decide(
                &off,
                HY4_PRIMARY,
                true,
                429,
                "Quota exceeded 2026-10-15 06:20:02"
            ),
            FailoverDecision::default()
        );
    }

    #[test]
    fn reasoning_default_only_when_tier_is_in_matrix() {
        // auto 的矩阵是 low/medium/high/max（拒 off）
        assert_eq!(
            reasoning_default_for("auto", "max"),
            Some("max".to_string())
        );
        assert_eq!(reasoning_default_for("auto", "off"), None);
        // glm-5.2 五档全接
        assert_eq!(
            reasoning_default_for("glm-5.2", "off"),
            Some("off".to_string())
        );
        // 无元数据模型 / 空配置
        assert_eq!(reasoning_default_for("not-a-model", "max"), None);
        assert_eq!(reasoning_default_for("auto", ""), None);
    }

    #[test]
    fn is_exhausted_only_matches_business_code() {
        assert!(is_exhausted_body(r#"{"code":14018,"msg":"Credits exhausted"}"#));
        assert!(is_exhausted_body(r#"{"code":"14018"}"#));
        assert!(is_exhausted_body("14018"));
        assert!(!is_exhausted_body("Credits exhausted"));
        assert!(!is_exhausted_body(""));
    }

    /// 验收标准 4：6 类输入（正常流 / 11128 / 11129 / 11133 / 空流 / 只有 DONE）。
    #[test]
    fn classify_stream_head_cases() {
        // 正常流
        assert_eq!(
            classify_stream_head(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                false
            ),
            StreamVerdict::Ok
        );
        // 只有 tool_calls 的正常流同样算 Ok（规则 3 的 `tool_calls` 分支）
        assert_eq!(
            classify_stream_head(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n",
                false
            ),
            StreamVerdict::Ok
        );
        // 六个已知业务码各命中一次（数字形态与字符串形态都认）
        for (body, code) in [
            (r#"{"code":11128,"msg":"illegal"}"#, 11128u32),
            (r#"{"code":11129,"msg":"invalid_function_parameters"}"#, 11129),
            (r#"{"code":11133,"msg":"content rejected"}"#, 11133),
            (r#"{"code":11102,"msg":"bad"}"#, 11102),
            (r#"{"code":11115,"msg":"bad"}"#, 11115),
            (r#"{"code":6004,"msg":"frequency limit"}"#, 6004),
        ] {
            assert_eq!(
                classify_stream_head(body, false),
                StreamVerdict::ErrCode(code),
                "业务码 {code} 应判 ErrCode"
            );
            let quoted = format!("{{\"code\":\"{code}\"}}");
            assert_eq!(
                classify_stream_head(&quoted, false),
                StreamVerdict::ErrCode(code),
                "字符串形态的业务码 {code} 应同样判 ErrCode"
            );
        }
        // 业务码即使出现在 data: 帧里也照样判错（形态与码序：码优先）
        assert_eq!(
            classify_stream_head("data: {\"code\":11128,\"msg\":\"blocked\"}\n\n", false),
            StreamVerdict::ErrCode(11128)
        );
        // 空流：非空但没有任何 data: 行（HTML 错误页 / JSON 错体）
        assert_eq!(classify_stream_head("<html>gateway</html>", false), StreamVerdict::EmptyStream);
        assert_eq!(
            classify_stream_head(r#"{"error":"boom"}"#, false),
            StreamVerdict::EmptyStream
        );
        // 只有 DONE：没有任何 content / tool_calls
        assert_eq!(
            classify_stream_head("data: [DONE]\n\n", false),
            StreamVerdict::EmptyStream
        );
        assert_eq!(
            classify_stream_head("data: {\"choices\":[]}\n\ndata: [DONE]\n\n", false),
            StreamVerdict::EmptyStream,
            "有 data 行但无 content/tool_calls、且带 [DONE] ⇒ 空流"
        );
        // 空 head：按字面落 Ok（规则 2 是「非空但…」）
        assert_eq!(classify_stream_head("", false), StreamVerdict::Ok);
        // 子串边界：16004 / 111280 不是业务码（前后都是数字 ⇒ 不算独立 token）
        assert_eq!(classify_stream_head(r#"{"code":16004}"#, false), StreamVerdict::EmptyStream);
        assert_eq!(
            classify_stream_head(r#"{"code":111280}"#, false),
            StreamVerdict::EmptyStream
        );
        // 已知局限（有意）：判定是**子串**级的（简报口径「head 含已知业务码」），
        // requestId 里恰好带 11128 也会命中。代价只是多耗一次 attempt、绝不冷却账号，
        // 故不为它加字段级解析。
        assert_eq!(
            classify_stream_head("data: {\"requestId\":\"11128\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n", false),
            StreamVerdict::ErrCode(11128)
        );
    }
}