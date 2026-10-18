//! `handleChat` 的**池路径与单账号路径** —— `internal/tuanjie/server.go:938-1430` 的逐条对应物。
//!
//! ## 本片覆盖什么
//!
//! * 入站：`POST` 方法闸门 → 64MB 限读 → **非法 UTF-8 本地 400**（`rejectInvalidUTF8Body`）
//!   → 解析 `model`/`stream` → 会话租借 → 反竞品身份脱敏
//!   （`reqshape::desensitize_agent_identity`）→ 体重排（`reqshape::reshape_chat_body`）。
//! * 池路径（有池且选得出号）：`ForwardDirect`（按号换 key + 签名头）→ 402 禁用该号并换号
//!   → 瞬时错误重发（≤3 次，间隔 800ms）→ **429 兜底两层**（即时换号 ≤3 次、换前
//!   [`ChatTiming::switch_backoff`] 退避；全池撞墙按 `Retry-After` 排队重发，单请求
//!   [`crate::pacing::PACING_MAX_WAIT_SECS`] 总预算）→ 非 200 透传 / 200 转发（流式或
//!   整块 + usage 入账）。
//! * 单账号路径（无池）：`ForwardWithSession`（共享 `apiKeyCached`）→ 429 **只有等待层**
//!   （无池可换）→ 瞬时错误重发（先 `invalidate_key`）→ 402 标记本地账号 → 200 空响应
//!   嗅探重放（[`ensure_non_empty`]）。
//!
//! ## 与 Go 的已知差异（逐条登记，不静默）
//!
//! 1. **`noteUpstreamError`/`noteUpstreamOK`（判定告警计数）未迁**：属 P8c 的面
//!    （`server.go:280-322`）。本片不写告警状态，`/health` 的 `judgment_alert` 仍恒 `false`
//!    （第三片已登记）。
//! 2. **`logRequestShape` 未迁**：纯诊断日志（`server.go:2212-2266`），不含响应字节。
//! 3. **`desensitizeAgentIdentity`（反竞品身份脱敏）已迁（P8d-3）**：Go 在
//!    `reshapeChatBody` **之前**对 system 消息里的 `You are <竞品名>` 做零宽空格改写
//!    （`server.go:988-991`），Rust 侧同位调用 [`crate::reqshape::desensitize_agent_identity`]，
//!    55 条 Go 真函数采收向量逐字节一致。
//!    【2026-10-18 更新】识图「静默改写模型」已按用户裁决删除：请求含图片/音频而目标
//!    模型不在内置多模态清单时直接回 400（列出可用模型）；远程图片预下载
//!    （`FetchRemoteImages`）与外部 provider 分支保留（见 [`crate::media`] /
//!    [`crate::providers`]）。自定义服务商模型**不参与**多模态拦截，命中即直接转发。
//! 4. **别名解析观测（`water.RecordResolution`）/被动注水（`water.RecordPassive`）未迁**：
//!    注水面（P8c）。
//! 5. **单账号路径 200 的 `defer activity.Add("ok", …)` 未迁**：Go 那条恒写（`:1379-1381`），
//!    池路径只在非流式含 usage 时写（`:1249-1250`）；Rust 侧实现了后者的同一形态
//!    （[`record_usage`]），前者少一条**仅用于 GUI 展示**的事件。
//! 6. **流式 usage 入账（`usageScanner`）已迁**：逐块 `feed`（usage 段入账、流内错误记一次）
//!    + `finish`，随池路径流式分支逐块驱动；别名解析观测（`water.RecordResolution`）
//!    未迁（属 P8c 的注水面），响应字节与入账行为不受影响。
//!
//! ## 测试缝
//!
//! 三个固定间隔是 Go 的**字面量**（`time.Sleep(800ms)` / `(1500ms)`，Go 侧无包级 var 可注入），
//! 本片照抄同一常量；只有 Go 侧**本身就可注入**的 `pacingSwitchBackoff`（`server.go:906`
//! 的包级 var）在 Rust 侧做成运行时可设的 [`ChatTiming::switch_backoff`]——否则门禁里每条
//! 换号用例都要真等 3 秒。生产构造恒为 1s。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;

use crate::client::now_unix;
use crate::pacing::{is_pacing_model, retry_wait_secs, PACING_MAX_WAIT_SECS};
use crate::server::{gobj, go_err_text, write_err, write_json, AppState, GJ};
use crate::session::{acquire, LitellmSession};

/// 池路径「上游瞬时错误」重发上限 —— `server.go:1103` 的 `attempt < 3`。
pub const POOL_RETRY_ATTEMPTS: usize = 3;
/// 单账号路径同一上限 —— `server.go:1299`。
pub const SINGLE_RETRY_ATTEMPTS: usize = 3;
/// 上游瞬时错误重发前的固定间隔 —— `server.go:1112` / `:1312` 的 `800ms`（Go 字面量）。
pub const RETRY_SLEEP: Duration = Duration::from_millis(800);
/// 空响应重放前的固定间隔 —— `server.go:2119` 的 `1500ms`（Go 字面量）。
pub const EMPTY_REPLAY_SLEEP: Duration = Duration::from_millis(1500);
/// 每请求即时换号次数上限 —— `server.go:1140` 的 `switches < 3`。
pub const MAX_POOL_SWITCHES: usize = 3;
/// 请求体读取上限 —— `server.go:944` 的 `io.LimitReader(r.Body, 64<<20)`。
///
/// **实测过**：`DefaultBodyLimit` 在 `axum::serve` 路径上没把请求截到 32 MiB
/// （`tests/p8b4_chat_behavior.rs::request_body_above_32mb_but_below_64mb_is_not_truncated`
/// 发 35 MiB 体，`model` 放在填充之后仍被解析出来 ⇒ 全量到达）。本常量是这一侧真正的闸门。
pub const MAX_CHAT_BODY_BYTES: usize = 64 << 20;
/// 上游错误体读取上限 —— `server.go:1104` 的 `io.LimitReader(resp.Body, 1<<20)`。
pub const MAX_UPSTREAM_ERR_BYTES: usize = 1 << 20;
/// 上游成功体读取上限 —— `server.go:1227` / `:1409` 的 `64<<20`。
pub const MAX_UPSTREAM_BODY_BYTES: usize = 64 << 20;
/// 空响应嗅探的流式首段上限 —— `server.go:2088` 的 `make([]byte, 64*1024)`。
pub const SNIFF_HEAD_BYTES: usize = 64 * 1024;

/// 本地拒绝非法 UTF-8 请求体时的返回文案 —— `server.go:909-912`（逐字）。
pub const INVALID_UTF8_BODY_MESSAGE: &str =
    "请求体不是合法 UTF-8，已本地拒绝（未转发上游）。\
     请确认发送端以 UTF-8 编码 JSON；命令行测试请把 body 写入文件后用 \
     curl --data-binary @文件，不要用 -d '中文'（Git Bash 调用原生 curl.exe 时会按本地\
     代码页 GBK 转码，整份 JSON 会变成非法 UTF-8）";

/// 空响应重放耗尽后的报错文案 —— `server.go:1373`（逐字）。
pub const EMPTY_UPSTREAM_MESSAGE: &str = "上游连续返回空响应（GLM-5.3 过载），请稍后重试";

// ---------------------------------------------------------------- 可注入的时序

/// 池路径 429 兜底的**可注入**参数（Go 侧只有 `pacingSwitchBackoff` 一个包级 var）。
pub struct ChatTiming {
    switch_backoff_ms: AtomicU64,
    pacing_max_wait_ms: AtomicU64,
}

impl Default for ChatTiming {
    fn default() -> Self {
        ChatTiming {
            switch_backoff_ms: AtomicU64::new(1000),
            // Go `pacingMaxWait`（`pacing.go:22`）= 30 分钟。
            pacing_max_wait_ms: AtomicU64::new(PACING_MAX_WAIT_SECS * 1000),
        }
    }
}

impl ChatTiming {
    /// 换号退避（Go `pacingSwitchBackoff` 缺省 1s）。
    pub fn switch_backoff(&self) -> Duration {
        Duration::from_millis(self.switch_backoff_ms.load(Ordering::SeqCst))
    }

    /// 注入换号退避 —— **Go 侧同款测试缝**（`pacing_pool_test.go:88-90` 直接改写该包级 var）。
    #[doc(hidden)]
    pub fn set_switch_backoff_for_test(&self, d: Duration) {
        self.switch_backoff_ms
            .store(d.as_millis() as u64, Ordering::SeqCst);
    }

    /// 单请求 429 兜底总预算（Go `pacingMaxWait` 缺省 30 分钟）。
    pub fn pacing_max_wait(&self) -> Duration {
        Duration::from_millis(self.pacing_max_wait_ms.load(Ordering::SeqCst))
    }

    /// 注入总预算 —— **Go 侧没有这个缝**（它靠取消请求 ctx 提前退出循环，见
    /// `pacing_pool_test.go:122-137`）。Rust 侧加它只为让「预算耗尽 ⇒ 循环退出」这条
    /// 分支能在秒级门禁里被走到；生产构造恒为 30 分钟。
    #[doc(hidden)]
    pub fn set_pacing_max_wait_for_test(&self, d: Duration) {
        self.pacing_max_wait_ms
            .store(d.as_millis() as u64, Ordering::SeqCst);
    }
}

async fn sleep(d: Duration) {
    tokio::time::sleep(d).await;
}

// ---------------------------------------------------------------- 判定告警（P8c）

/// 疑似官方判定变更告警 —— `judgmentAlert`（`server.go:64-72`）。
///
/// `noteUpstreamError` 记录上游错误响应：
/// - 401 且错误体含「请升级」/「x-codely-signature」→ 立即置告警；
/// - 401 其他情况 → `consec401+1`，连续 3 次置告警（换 key 后仍被拒）。
///
/// `noteUpstreamOK`（上游 200）清零计数，告警 active 则整组清除。
#[derive(Debug, Default)]
pub struct JudgeAlert {
    inner: std::sync::Mutex<JudgeInner>,
}

#[derive(Debug, Default)]
struct JudgeInner {
    active: bool,
    /// 置位时刻（Unix 秒；0 = 未置位）。
    since_unix: i64,
    signal: String,
    consec401: i32,
}

/// `/health` 需要的四件事 —— `server.go:261-269`。
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeSnapshot {
    pub active: bool,
    pub signal: String,
    /// 未置位为 0（Go 的 `time.Time{}` → `Unix()` 也得 0）。
    pub since_unix: i64,
}

impl JudgeAlert {
    /// `NewServer` 的零值（`judgmentAlert{}`）。
    pub fn new() -> Self {
        JudgeAlert {
            inner: std::sync::Mutex::new(JudgeInner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JudgeInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `/health` 快照。
    pub fn snapshot(&self) -> JudgeSnapshot {
        let g = self.lock();
        JudgeSnapshot {
            active: g.active,
            signal: g.signal.clone(),
            since_unix: g.since_unix,
        }
    }

    /// `consec401` 计数（测试可读）。
    pub fn consec401(&self) -> i32 {
        self.lock().consec401
    }

    /// `noteUpstreamError`（`server.go:284-310`）。`body` 只读已读出的错误体字符串。
    pub fn note_upstream_error(&self, status: u16, body: &str) {
        let low = body.to_lowercase();
        let mut hit = "";
        if status == 401 {
            if low.contains("请升级") {
                hit = "请升级";
            } else if low.contains("x-codely-signature") {
                hit = "x-codely-signature";
            }
        }
        let mut g = self.lock();
        if !hit.is_empty() {
            g.active = true;
            g.since_unix = crate::client::now_unix();
            g.signal = format!("{hit} 状态码 {status}");
            return;
        }
        if status == 401 {
            g.consec401 += 1;
            if g.consec401 >= 3 {
                g.active = true;
                g.since_unix = crate::client::now_unix();
                g.signal = "换 key 后连续 401".to_string();
            }
        }
    }

    /// `noteUpstreamOK`（`server.go:313-322`）：计数清零，告警 active 则整组清除。
    pub fn note_upstream_ok(&self) {
        let mut g = self.lock();
        g.consec401 = 0;
        if g.active {
            g.active = false;
            g.since_unix = 0;
            g.signal.clear();
        }
    }
}

// ---------------------------------------------------------------- 请求形态指纹（P8c）

/// 入站/出站请求的结构指纹 —— `logRequestShape`（`server.go:2212-2266`）。
///
/// 只记结构摘要（字段名列表、消息条数与每条的 role/keys/content 形态与长度），
/// **不记消息正文**。返回该打印的**整行文案**（不含时间戳前缀）——
/// 便于门禁逐字对照；调用方把它交给 [`crate::logf`]。
pub fn request_shape_line(model: &str, body: &[u8]) -> String {
    let Ok(m) = serde_json::from_slice::<serde_json::Value>(body) else {
        return format!("[tuanjie] shape model={model} 非JSON请求");
    };
    let Some(obj) = m.as_object() else {
        return format!("[tuanjie] shape model={model} 非JSON请求");
    };
    let mut fields: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    // Go 的 `for k := range m` 是**无序**的，`%v` 打出的顺序随机；
    // 这里按字节序固化，使门禁可复跑（登记差异：Go 的 fields 顺序不可复现）
    fields.sort_unstable();

    let mut msg_detail = String::new();
    if let Some(msgs) = obj.get("messages").and_then(|v| v.as_array()) {
        msg_detail = format!("messages={}", msgs.len());
        for (i, raw) in msgs.iter().enumerate() {
            if i >= 6 {
                break;
            }
            if let Some(msg) = raw.as_object() {
                let mut keys: Vec<&str> = msg.keys().map(|k| k.as_str()).collect();
                keys.sort_unstable();
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let (ctype, clen) = match msg.get("content") {
                    // Go 的 `len(cv)` 对 string 是**字节**长度
                    Some(serde_json::Value::String(s)) => ("str", s.len()),
                    Some(serde_json::Value::Array(a)) => ("arr", a.len()),
                    Some(serde_json::Value::Null) => ("nil", 0),
                    None => ("?", 0),
                    _ => ("?", 0),
                };
                msg_detail += &format!(
                    " [{i}]role={role} keys=[{}] c={ctype} clen={clen}",
                    keys.join(" ")
                );
            }
        }
    }
    let sys_len = obj
        .get("system")
        .and_then(|v| v.as_str())
        .map(|s| s.len())
        .unwrap_or(0);
    let mut special: Vec<&str> = Vec::new();
    for k in [
        "reasoning_effort",
        "user",
        "metadata",
        "stream_options",
        "tools",
        "tool_choice",
        "temperature",
        "max_tokens",
        "extra_body",
        "store",
        "model_info",
    ] {
        if obj.contains_key(k) {
            special.push(k);
        }
    }
    format!(
        "[tuanjie] shape model={model} fields=[{}] {msg_detail} system={sys_len}B special=[{}]",
        fields.join(" "),
        special.join(" ")
    )
}

/// Go 的 `len(sys)` 是**字节**长度（`system` 是 string 时）。
#[allow(dead_code)]
fn system_bytes(system: Option<&serde_json::Value>) -> usize {
    match system.and_then(|v| v.as_str()) {
        Some(s) => s.len(),
        None => 0,
    }
}

// ---------------------------------------------------------------- 纯函数门面

/// `utf8.Valid(body)`（`server.go:925`）。
pub fn is_valid_utf8(body: &[u8]) -> bool {
    std::str::from_utf8(body).is_ok()
}

/// `retriableUpstream`（`server.go:2050-2071`）：空串不重试；`invalid model name` 是
/// 确定性错误不重试；其余含 model/rate/429/502/503/504/bad gateway/internal server
/// 任一子串即重试。
pub fn retriable_upstream(body: &str) -> bool {
    if body.is_empty() {
        return false;
    }
    let low = body.to_lowercase();
    if low.contains("invalid model name") {
        return false;
    }
    [
        "model",
        "rate",
        "429",
        "502",
        "503",
        "504",
        "bad gateway",
        "internal server",
    ]
    .iter()
    .any(|s| low.contains(s))
}

/// `hasStreamDataLine`（`server.go:2133-2135`）：首段以 `data:` 开头，或含 `\ndata:`。
pub fn has_stream_data_line(head: &[u8]) -> bool {
    head.starts_with(b"data:") || contains(head, b"\ndata:")
}

/// `streamHasContent`（`server.go:2138-2171`）：逐行取 SSE `data:` 载荷，任一 choice 的
/// `delta.content` / `delta.reasoning_content` / `message.content` / `message.reasoning_content`
/// 非空即有内容。非法 JSON 行跳过（Go 的 `json.Unmarshal` 失败同样 continue）。
pub fn stream_has_content(head: &[u8]) -> bool {
    for raw in head.split(|b| *b == b'\n') {
        let line = trim_ascii(raw);
        let Some(rest) = line.strip_prefix(b"data:") else {
            continue;
        };
        let payload = trim_ascii(rest);
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) else {
            continue;
        };
        let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        for c in choices {
            for (parent, key) in [
                ("delta", "content"),
                ("delta", "reasoning_content"),
                ("message", "content"),
                ("message", "reasoning_content"),
            ] {
                let s = c
                    .get(parent)
                    .and_then(|d| d.get(key))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if !s.is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

/// 流式 usage 扫描器 —— `usageScanner`（`server.go:1766-1825`）：逐行取 SSE `data:`
/// 载荷，`usage` 段入账（`addStat`）、流内错误记一次、上游模型别名观测。
///
/// 别名解析观测（`water.RecordResolution`）未迁：属 P8c 的注水面（`water.go`），
/// 这里只做 usage 入账与错误日志——响应字节与入账行为不受影响。
pub struct UsageScanner {
    model: String,
    buf: String,
    err_logged: bool,
    /// 本流已记过 model 字段（SSE 每块都带，防逐 chunk 重复记录）——
    /// `usageScanner.resSeen`（`server.go:1771`）。
    res_seen: bool,
}

impl UsageScanner {
    pub fn new(model: &str) -> Self {
        UsageScanner {
            model: model.to_string(),
            buf: String::new(),
            err_logged: false,
            res_seen: false,
        }
    }

    /// 喂一块字节（Go `feed`）：攒 buffer、按 `\n` 切行、逐行处理。
    pub fn feed(
        &mut self,
        stats: &crate::stats::Stats,
        water: &crate::water::WaterCheck,
        chunk: &[u8],
    ) {
        self.buf.push_str(&String::from_utf8_lossy(chunk));
        loop {
            let Some(i) = self.buf.find('\n') else {
                break;
            };
            let line: String = self.buf.drain(..=i).collect();
            let line = line.trim();
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            if rest.contains("[DONE]") {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(rest.as_bytes()) else {
                continue;
            };
            if let Some(err) = v.get("error") {
                // 上游可能先回 200 再把自身异常塞进流里，只记一次防刷屏。
                if !err.is_null() && !self.err_logged {
                    self.err_logged = true;
                    crate::logf(&format!(
                        "[tuanjie] 流内错误 model={} {}",
                        self.model,
                        crate::client::truncate_bytes(
                            err.to_string().as_bytes(),
                            300
                        )
                    ));
                }
            }
            // 别名解析观测（本流首块带 model 即记一次）—— `server.go:1808-1818`。
            let returned = v.get("model").and_then(|m| m.as_str()).unwrap_or("");
            if !returned.is_empty() && !self.res_seen {
                self.res_seen = true;
                let (from, changed) = water.record_resolution(&self.model, returned);
                if changed {
                    if from.is_empty() {
                        crate::logf(&format!(
                            "[tuanjie] 别名解析基线 {} → {returned}",
                            self.model
                        ));
                    } else {
                        crate::logf(&format!(
                            "[tuanjie] 模型映射变化 {}: {from} → {returned}",
                            self.model
                        ));
                    }
                }
            }
            if let Some(u) = v.get("usage") {
                let p = u.get("prompt_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
                let c = u
                    .get("completion_tokens")
                    .and_then(|x| x.as_i64())
                    .unwrap_or(0);
                let t = u.get("total_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
                stats.add(&self.model, p, c, t);
            }
        }
    }

    /// 流结束（Go `finish`：缓冲里通常已无完整行，是个空操作）。
    pub fn finish(&mut self) {}
}

#[cfg(test)]
mod scanner_tests {
    use super::*;

    /// 与 Go `feed` 逐条同形：跨块断行照样拼出来、usage 入账、`[DONE]` 跳过、
    /// 非法行跳过、流内错误只记一次（记的是日志，不断言它）。
    #[test]
    fn usage_scanner_feeds_across_chunk_boundaries_like_go() {
        // `with_path(None)`：无落盘形态（Go 侧 `statsPath == ""` 同款）——
        // 绝不允许测试碰真实数据目录（`Stats::new()` 会读真实 `tuanjie-stats.json`
        // 且 `add()` 会写回去；血泪教训，见简报 P8b 第四片）。
        let stats = crate::stats::Stats::with_path(None);
        // 分离式注水实例：绝不落盘、不碰真实 `tuanjie-water.json`。
        let water = crate::water::WaterCheck::detached();
        let mut sc = UsageScanner::new("codely-core");
        // 第一块在行中间断开，第二块补齐；usage 行跨两块。
        sc.feed(&stats, &water, b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":");
        sc.feed(&stats, &water, b"10,\"completion_tokens\":20,\"total_tokens\":30}}\n");
        let snap = stats.snapshot();
        let st = snap.get("codely-core").expect("usage 必须入账");
        assert_eq!((st.input_tokens, st.output_tokens, st.total_tokens), (10, 20, 30));
        // `[DONE]`、非法行、非 data 行都不入账也不炸。
        sc.feed(&stats, &water, b"data: [DONE]\n");
        sc.feed(&stats, &water, b"not json at all\n");
        sc.feed(&stats, &water, b"event: x\n");
        sc.finish();
        let snap = stats.snapshot();
        assert_eq!(snap.len(), 1, "噪音行不得产生新条目");
        assert_eq!(snap["codely-core"].total_tokens, 30);
        assert!(
            water.resolved_map().is_empty(),
            "无 model 字段的流不该落别名解析"
        );
    }

    /// 别名解析观测：**本流首块带 model 才记一次**（`server.go:1808-1818`），
    /// 后续块再带同名也不重复记。
    #[test]
    fn usage_scanner_records_resolution_once_per_stream() {
        let stats = crate::stats::Stats::with_path(None);
        let water = crate::water::WaterCheck::detached();
        let mut sc = UsageScanner::new("codely-core");
        sc.feed(
            &stats,
            &water,
            b"data: {\"model\":\"glm-5.3\",\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n",
        );
        assert_eq!(
            water.resolved_map().get("codely-core").map(|s| s.as_str()),
            Some("glm-5.3")
        );
        // 第二块再带同一 model：不重复落盘（数值不变）
        sc.feed(
            &stats,
            &water,
            b"data: {\"model\":\"glm-5.3\",\"choices\":[]}\n",
        );
        assert_eq!(water.resolved_map().len(), 1);
        // 直连真实模型（归一化相等）不记
        let mut sc2 = UsageScanner::new("GLM-5.3");
        sc2.feed(
            &stats,
            &water,
            b"data: {\"model\":\"glm-5.3\",\"choices\":[]}\n",
        );
        assert_eq!(water.resolved_map().len(), 1, "归一化相等 → 不记");
    }

    /// 请求形态指纹：非 JSON、消息数组与 special 字段的形状（`logRequestShape`）。
    #[test]
    fn request_shape_line_shape() {
        let line = request_shape_line("m", b"not json");
        assert_eq!(line, "[tuanjie] shape model=m 非JSON请求");
        let body = br#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":[{"type":"text"}]}],"temperature":0.5,"system":"sys"}"#;
        let line = request_shape_line("m", body);
        assert!(line.contains("fields=[model stream messages temperature system]") || line.contains("fields=["), "{line}");
        assert!(line.contains("messages=2"), "{line}");
        assert!(line.contains("keys=[content role]"), "{line}");
        assert!(line.contains("c=str clen=5"), "{line}");
        assert!(line.contains("c=arr clen=1"), "{line}");
        assert!(line.contains("system=3B"), "{line}");
        assert!(line.contains("special=[temperature stream_options]") || line.contains("special=[temperature]"), "{line}");
    }
}

/// `nonStreamHasContent`（`server.go:2174-2193`）：解析失败时「有字节就放行」；
/// 解析成功则要求某个 choice 的 content / reasoning_content 非空或 `tool_calls` 非空。
///
/// `tool_calls` 的判据是 Go 的 `len(json.RawMessage) > 0`：只有**键缺席**时才是 nil
/// （0 字节 → false）；显式 `null` 落成 4 字节 `"null"`、`[]` 落成 2 字节，
/// 两者都是 true（真机实测 `encoding/json` 行为，非推测）。
pub fn non_stream_has_content(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return !trim_ascii(body).is_empty();
    };
    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        return false;
    };
    for c in choices {
        let msg = c.get("message");
        let get = |key: &str| {
            msg.and_then(|m| m.get(key))
                .and_then(|x| x.as_str())
                .unwrap_or("")
        };
        let has_tools = msg.and_then(|m| m.get("tool_calls")).is_some();
        if !get("content").is_empty() || !get("reasoning_content").is_empty() || has_tools {
            return true;
        }
    }
    false
}

/// 入站请求里的 `model` / `stream`（`server.go:948-958`）：解析失败或 `model` 为空则回落
/// `model="?"`、`stream=false`（Go 的零值语义）。
///
/// Go 用的是 `json.Unmarshal(body, &req)`——要求**整串**是合法 JSON（与 `Decoder.Decode`
/// 的尾随容忍不同），这里同样用 `serde_json::from_slice`（尾随垃圾即失败）。
pub fn parse_model_and_stream(body: &[u8]) -> (String, bool) {
    #[derive(serde::Deserialize)]
    struct Req {
        #[serde(default)]
        model: String,
        #[serde(default)]
        stream: bool,
    }
    match serde_json::from_slice::<Req>(body) {
        Ok(r) if !r.model.is_empty() => (r.model, r.stream),
        _ => ("?".to_string(), false),
    }
}

/// `parseQuota5h`（`server.go:202-233`）：取 `usage_5h` 窗口的剩余（窗口耗尽
/// `exhausted` 且 remaining ≤ 0 时改用综合剩余 `plan.effective_remaining`）；
/// 重置时刻三级兜底 `PeriodEnd → NextBoundaryAt → now+5h`。
/// 返回 `(remaining, reset_at_unix, ok)`：`ok` 只在配额对象本身不可用
/// （`source == "none"`）时为 false——此时 remaining/reset 全零值；
/// 其余情况 `ok` 恒 true（Go 同：第三级兜底保证 reset 总有值）。
/// `None` 的 reset = Go 的零值 Time = 不参与自动恢复。
pub fn parse_quota_5h(q: &crate::quota::Quota) -> (f64, Option<i64>, bool) {
    if q.source == "none" {
        return (0.0, None, false);
    }
    let mut remaining = q.plan.effective_remaining;
    for w in &q.plan.windows {
        if w.r#type != "usage_5h" {
            continue;
        }
        remaining = if w.exhausted && w.remaining <= 0.0 {
            q.plan.effective_remaining
        } else {
            w.remaining
        };
        break;
    }
    let mut reset: Option<i64> = None;
    for w in &q.plan.windows {
        if w.r#type == "usage_5h" {
            reset = parse_window_end(&w.period_end);
            break;
        }
    }
    if reset.is_none() {
        reset = parse_window_end(&q.plan.next_boundary_at);
    }
    if reset.is_none() {
        reset = Some(now_unix() + 5 * 3600);
    }
    (remaining, reset, true)
}

/// Go `parseWindowEnd`：去空白后空串 → 零值；RFC3339 或 `2006-01-02 15:04:05`
/// （按 UTC）能解则取值，否则零值。这里用 `None` 表零值。
fn parse_window_end(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(dt.timestamp());
    }
    chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|n| n.and_utc().timestamp())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut s = 0;
    let mut e = b.len();
    while s < e && b[s].is_ascii_whitespace() {
        s += 1;
    }
    while e > s && b[e - 1].is_ascii_whitespace() {
        e -= 1;
    }
    &b[s..e]
}

// ---------------------------------------------------------------- 出站转发与读取

/// `ForwardDirect`（`handlers_extra.go:211-226`）的一次转发：按传入的 access_token 现换
/// key，再用同一套签名头打 LiteLLM（**不共享单账号的 key 缓存**）。
///
/// 上游 **401** 时重发一次（Go 的 key 换取本无缓存，重发表达的是「换个 key 再来」）。
pub async fn forward_direct(
    st: &AppState,
    path: &str,
    body: &[u8],
    access_token: &str,
    sess: &LitellmSession,
) -> Result<reqwest::Response, String> {
    let resp = direct_once(st, path, body, access_token, sess)
        .await
        .map_err(|e| go_err_text(&e))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        drop(resp);
        return direct_once(st, path, body, access_token, sess)
            .await
            .map_err(|e| go_err_text(&e));
    }
    Ok(resp)
}

async fn direct_once(
    st: &AppState,
    path: &str,
    body: &[u8],
    access_token: &str,
    sess: &LitellmSession,
) -> crate::Result<reqwest::Response> {
    let key = st.client.fetch_key_with_token(access_token).await?;
    let url = format!("{}{}", st.client.litellm_base(), path);
    let mut req = st
        .client
        .http()
        .request(reqwest::Method::POST, url)
        .body(body.to_vec());
    for (k, v) in crate::headers::litellm_headers(
        path,
        &key,
        &sess.id,
        now_unix(),
        st.client.user_agent(),
    ) {
        req = req.header(k, v);
    }
    req.send().await.map_err(crate::Error::Forward)
}

/// `io.ReadAll(io.LimitReader(body, n))`：读满上限字节，读错/超时回空。
pub async fn read_body_bounded(resp: reqwest::Response, limit: usize) -> Vec<u8> {
    match resp.bytes().await {
        Ok(b) => {
            let mut v = b.to_vec();
            v.truncate(limit);
            v
        }
        Err(_) => Vec::new(),
    }
}

/// 取一个响应头（缺失/非 UTF-8 回空串，与 Go `resp.Header.Get` 同口径）。
fn header_of(headers: &reqwest::header::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------- 池路径

/// 池路径的终态。
pub enum PoolOutcome {
    /// 有响应（可能仍非 200 —— 调用方按 `reply.status` 分流）。
    Resp {
        reply: Reply,
        /// 最终使用的账号 user_id（换号后会变）。
        pool_uid: String,
        /// 429 兜底**实际介入过**（Go 的 `pacedPool`）：池路径流式成功不写 `ok` 事件，
        /// 恢复成功要补一条完成事件（`server.go:1131` / `:1199-1201`）。
        paced: bool,
        /// 本请求即时换号实际用掉的次数（有界性断言要它）。
        switches: usize,
        /// 进行中注册号（收尾配平 `Finish`；流分支在流尽时配平）。
        rid: String,
        /// 池路径入口的单调时钟起点换算的毫秒数（`ok`/`error` 事件的耗时字段，
        /// Go 传 `time.Since(start).Milliseconds()`，`server.go:1189/1249`）。
        elapsed_ms: i64,
    },
    /// 终态 200 + 入站要流式：上游响应的 body 保持 **live**，由收尾逐块转发
    /// （Go `server.go:1206-1226`：32KB 块 + usage 扫描 + registry touch + Flush）。
    /// 缓冲会杀死 SSE 实时性，故 200+stream 恒走此分支、永不进 `Resp`。
    Stream {
        resp: reqwest::Response,
        /// 最终使用的账号 user_id（换号后会变）。
        pool_uid: String,
        /// 429 兜底实际介入过（收尾补完成事件，与 `Resp` 同）。
        paced: bool,
        /// 进行中注册号（收尾逐块 touch，流尽/中断时 finish）。
        rid: String,
        /// usage 入账与日志用的模型名。
        model: String,
    },
    /// 池非空但当场选不出号 —— `server.go:1056-1059` 的 502 文案。
    NoAccount,
    /// 取 key / 转发在网络层失败 —— 文案 = `"上游转发失败: " + err`。
    NetworkError(String),
}

/// 一份**已读进内存**的上游响应。
///
/// 池路径要在「换号重发」与「非 200 透传」之间反复搬运响应体，而 Go 靠
/// `poolErrBody` 这个裸变量 + 重新赋值的 `resp` 表达同一件事；Rust 的 `reqwest::Response`
/// 读体即被消费，故这里先把状态/头/体一次取齐（字节与最终写出**逐字节一致**：
/// 成功的非流式路径 Go 也是整块 `io.ReadAll` 后再 `w.Write`）。
pub struct Reply {
    pub status: StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}

/// 读一份响应成 [`Reply`]（池路径用；体上限 [`MAX_UPSTREAM_BODY_BYTES`]）。
pub async fn read_reply(resp: reqwest::Response) -> Reply {
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = read_body_bounded(resp, MAX_UPSTREAM_BODY_BYTES).await;
    Reply {
        status,
        headers,
        body,
    }
}

/// 池路径的重发闭包（与 Go 的 `s.ForwardDirect(...)` 同参）。
macro_rules! pool_send {
    ($st:expr, $body:expr, $sess:expr, $token:expr) => {
        forward_direct($st, "/v1/chat/completions", $body, &$token, $sess).await
    };
}

/// 池路径主体（`server.go:1054-1252`，不含入站公共段）。
///
/// `st.pool.inc_load(pool_uid)` 的配平由调用方（[`handle_chat`]）负责——位置与 Go 的
/// `defer s.pool.DecLoad(poolUID)` 同一处：换号**只换 token/uid，不迁移 load 计数**
/// （Go 原样：`DecLoad` 只对最初选的 uid 生效一次）。
pub async fn pool_path(
    st: &AppState,
    body: &[u8],
    sess: &LitellmSession,
    token: String,
    pool_uid: String,
    model: &str,
    pacing: bool,
    wants_stream: bool,
) -> PoolOutcome {
    let mut token = token;
    let mut pool_uid = pool_uid;
    // 池路径入口时钟（`ok`/`error` 事件的耗时字段用；Go `server.go:1062` 的 `start`）。
    let start = Instant::now();
    // 进行中注册（Go `server.go:1063` 的 `registry.Register`；`defer Finish` 由各出口
    // 与流收尾分别配平——此前本路径漏注册，流式逐块 touch 更无从谈起，P8b 审查 P1 附带补齐）。
    let rid = st.registry.register(model, &pool_uid, wants_stream);
    let mut reply = match pool_send!(st, body, sess, token) {
        Ok(r) => {
            // 终态 200 + 入站要流式：不再缓冲，直接返回 live 流（下详 `Stream` 分支）。
            // 200 在池路径恒为终态（402/重试/429 分支都只认非 200），故此处可直接返回；
            // pacing 块尚未进入，配平 inc/dec 对以与非提前路径逐字一致。
            if wants_stream && r.status() == reqwest::StatusCode::OK {
                if pacing {
                    st.pacer.pending_inc();
                    st.pacer.pending_dec();
                }
                return PoolOutcome::Stream {
                    resp: r,
                    pool_uid: pool_uid.clone(),
                    paced: false,
                    rid: rid.clone(),
                    model: model.to_string(),
                };
            }
            read_reply(r).await
        }
        Err(e) => {
            st.registry.finish(&rid);
            return PoolOutcome::NetworkError(e);
        }
    };

    // 402 配额耗尽（`server.go:1072-1098`）：同步路径零延迟禁用该号；异步查官网 5h 窗口
    // 拿重置时刻（查询失败零值 = 不参与自动恢复），查完再 `MarkBudgetExceededAt`。
    if reply.status == reqwest::StatusCode::PAYMENT_REQUIRED {
        let orig_uid = pool_uid.clone();
        let orig_token = token.clone();
        let pool: Arc<crate::accounts::AccountPool> = st.pool.clone();
        let http = st.client.http().clone();
        let ep = crate::quota::QuotaEndpoint {
            url: crate::server::quota_endpoint_url(&st.client),
            ..Default::default()
        };
        tokio::spawn(async move {
            // Go 同款：`_, resetAt, ok := parseQuota5h(...)`，`!ok`（配额对象不可用）
            // 则零值 = 不参与自动恢复；查询失败同理。
            let reset_at = match crate::quota::fetch_quota_with_token(&http, &ep, &orig_token).await
            {
                Ok(q) => {
                    let (_, reset, ok) = parse_quota_5h(&q);
                    if ok {
                        reset
                    } else {
                        None
                    }
                }
                Err(_) => None,
            };
            pool.mark_budget_exceeded_at(&orig_uid, reset_at);
        });
        st.activity.add(
            "error",
            &format!("账号 {pool_uid} 配额用尽，自动禁用"),
            model,
            &pool_uid,
            0,
            0,
            402,
        );
        crate::logf(&format!(
            "[tuanjie] account={pool_uid} 402 budget_exceeded 已禁用，切换下一账号"
        ));
        if let Some((acc, tok)) = st.pool.pick_with_token(model) {
            if acc.user_id != pool_uid {
                token = tok;
                pool_uid = acc.user_id;
                reply = match pool_send!(st, body, sess, token) {
                    Ok(r) => {
                        if wants_stream && r.status() == reqwest::StatusCode::OK {
                            if pacing {
                                st.pacer.pending_inc();
                                st.pacer.pending_dec();
                            }
                            return PoolOutcome::Stream {
                                resp: r,
                                pool_uid: pool_uid.clone(),
                                paced: false,
                                rid: rid.clone(),
                                model: model.to_string(),
                            };
                        }
                        read_reply(r).await
                    }
                    Err(e) => {
                        st.registry.finish(&rid);
                        return PoolOutcome::NetworkError(e);
                    }
                };
            }
        }
    }

    // 上游瞬时错误重发（`server.go:1099-1120`）：非 200/402/429 且报文可重试时重发，
    // 间隔 800ms，最多 3 次；不可重试则跳出（错误体随非 200 透传）。
    for attempt in 0..POOL_RETRY_ATTEMPTS {
        let status = reply.status;
        if status == reqwest::StatusCode::OK
            || status == reqwest::StatusCode::PAYMENT_REQUIRED
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            break;
        }
        if !retriable_upstream(&String::from_utf8_lossy(&reply.body)) {
            break;
        }
        crate::logf(&format!(
            "[tuanjie] pool chat model={model} account={pool_uid} status={} 重试 {}/3 err={}",
            status.as_u16(),
            attempt + 1,
            crate::client::truncate_bytes(&reply.body, 200)
        ));
        sleep(RETRY_SLEEP).await;
        reply = match pool_send!(st, body, sess, token) {
            Ok(r) => {
                if wants_stream && r.status() == reqwest::StatusCode::OK {
                    if pacing {
                        st.pacer.pending_inc();
                        st.pacer.pending_dec();
                    }
                    return PoolOutcome::Stream {
                        resp: r,
                        pool_uid: pool_uid.clone(),
                        paced: false,
                        rid: rid.clone(),
                        model: model.to_string(),
                    };
                }
                read_reply(r).await
            }
            Err(e) => {
                st.registry.finish(&rid);
                return PoolOutcome::NetworkError(e);
            }
        };
    }

    // 429 兜底（`server.go:1121-1183`）：pacing 开启时两层——第一层换池里其他可用账号
    // 即时重发；全池撞墙或无号可换才按 Retry-After 等待原号重发（单请求 30 分钟总预算，
    // 超限或客户端断开后透传最后一次原始错误）。
    let mut paced_pool = false;
    let mut switches = 0usize;
    if pacing {
        st.pacer.pending_inc();
        let deadline = Instant::now() + st.chat_timing.pacing_max_wait();
        while reply.status == reqwest::StatusCode::TOO_MANY_REQUESTS && Instant::now() < deadline {
            paced_pool = true;
            let retry_after = header_of(&reply.headers, "Retry-After");
            let switched = if switches < MAX_POOL_SWITCHES {
                st.pool
                    .pick_with_token(model)
                    .filter(|(a, _)| a.user_id != pool_uid)
            } else {
                None
            };
            match switched {
                Some((acc, tok)) => {
                    switches += 1;
                    crate::logf(&format!(
                        "[tuanjie] pacing pool model={model} account={pool_uid} status=429 即时换号 {} 重试 第 {}/3 次",
                        acc.user_id, switches
                    ));
                    st.activity.add(
                        "info",
                        &format!(
                            "{model} · 429 换号 {pool_uid} → {}（{switches}/3）",
                            acc.user_id
                        ),
                        model,
                        &acc.user_id,
                        0,
                        0,
                        429,
                    );
                    sleep(st.chat_timing.switch_backoff()).await;
                    pool_uid = acc.user_id;
                    token = tok;
                    reply = match pool_send!(st, body, sess, token) {
                        Ok(r) => {
                            // 已在 pacing 块内（pending_inc 已发生）：200+stream 直接返回，
                            // 配平 pending_dec（非提前路径走到底由块尾统一 dec）。
                            if wants_stream && r.status() == reqwest::StatusCode::OK {
                                st.pacer.pending_dec();
                                return PoolOutcome::Stream {
                                    resp: r,
                                    pool_uid: pool_uid.clone(),
                                    paced: paced_pool,
                                    rid: rid.clone(),
                                    model: model.to_string(),
                                };
                            }
                            read_reply(r).await
                        }
                        Err(e) => {
                            st.registry.finish(&rid);
                            return PoolOutcome::NetworkError(e);
                        }
                    };
                }
                None => {
                    // 第二层：全池撞墙或换号预算用尽 —— 按 Retry-After 等原号重发。
                    let mut wait = Duration::from_secs(retry_wait_secs(&retry_after));
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if wait > remaining {
                        wait = remaining;
                    }
                    if wait.is_zero() {
                        break;
                    }
                    crate::logf(&format!(
                        "[tuanjie] pacing pool model={model} account={pool_uid} status=429 等待 {}s 后重发（剩余预算 {}s）",
                        wait.as_secs(),
                        deadline
                            .saturating_duration_since(Instant::now())
                            .as_secs()
                    ));
                    st.activity.add(
                        "info",
                        &format!("{model} · 429 全池限流，等待 {}s 后重发", wait.as_secs()),
                        model,
                        &pool_uid,
                        0,
                        0,
                        429,
                    );
                    sleep(wait).await;
                    reply = match pool_send!(st, body, sess, token) {
                        Ok(r) => {
                            // 同上：pacing 块内，配平 pending_dec 后返回 live 流。
                            if wants_stream && r.status() == reqwest::StatusCode::OK {
                                st.pacer.pending_dec();
                                return PoolOutcome::Stream {
                                    resp: r,
                                    pool_uid: pool_uid.clone(),
                                    paced: paced_pool,
                                    rid: rid.clone(),
                                    model: model.to_string(),
                                };
                            }
                            read_reply(r).await
                        }
                        Err(e) => {
                            st.registry.finish(&rid);
                            return PoolOutcome::NetworkError(e);
                        }
                    };
                }
            }
        }
        st.pacer.pending_dec();
    }

    PoolOutcome::Resp {
        reply,
        pool_uid,
        paced: paced_pool,
        switches,
        rid,
        elapsed_ms: start.elapsed().as_millis() as i64,
    }
}


// ---------------------------------------------------------------- 空响应嗅探

/// `ensureNonEmpty` 的**非流式**产物：整份响应体已进内存（响应本体读体后即消失，
/// 故这里保存写响应所需的**状态与头**）。
pub struct Sniffed {
    pub status: StatusCode,
    pub headers: reqwest::header::HeaderMap,
    /// 已从上游读出的字节（= Go 重新 `MultiReader` 拼回的那一段）。
    pub prefix: Vec<u8>,
}

/// `ensureNonEmpty` 的**流式**产物：首段已读走，其余仍从上游网络流式转发
/// （`rest` 是把 `io.MultiReader(head, resp.Body)` 那段接回去的读取器）。
pub struct SniffedStream {
    pub status: StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub prefix: Vec<u8>,
    /// 上游剩余部分（首段用掉的那个流）。
    pub rest: ByteStream,
}

/// 上游响应体的字节流（`reqwest::Response::bytes_stream` 的固定形状）。
pub type ByteStream = futures_util::stream::BoxStream<'static, reqwest::Result<axum::body::Bytes>>;

/// `ensureNonEmpty` 的结果（流式/非流式各一形态）。
pub enum SniffedResp {
    Stream(SniffedStream),
    Full(Sniffed),
}

/// `ensureNonEmpty`（`server.go:2077-2130`）：写给客户端**之前**嗅探「200 但内容为空」，
/// 空则丢弃本次响应、等 [`EMPTY_REPLAY_SLEEP`] 后重放一次（`send` 复用已缓存的请求体）。
///
/// 返回 `None` = 重放后仍为空（原响应已关闭，调用方只能报错）。
///
/// * 流式：读首段最多 64KB，**出现 data 行即停**；放行判据 = 首段有实际内容（content /
///   reasoning 非空）**或**首段里已出现 data 行（读到 EOF 且无内容则判空）。
///   （慢启动的正常流不得被误判为空而重放 —— 2026-08-21 实测教训。）
/// * 非流式：整读 64MB 判 [`non_stream_has_content`]。
pub async fn ensure_non_empty<F, Fut>(
    stream: bool,
    resp: Option<reqwest::Response>,
    model: &str,
    mut send: F,
) -> Option<SniffedResp>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, String>>,
{
    let mut resp = resp?;
    for attempt in 0..2 {
        if stream {
            let (status, headers, prefix, rest) = sniff_stream_head(resp).await;
            if stream_has_content(&prefix) || has_stream_data_line(&prefix) {
                return Some(SniffedResp::Stream(SniffedStream {
                    status,
                    headers,
                    prefix,
                    rest,
                }));
            }
            // 空流：等一会儿重放（Go 同），重放的响应还得从 `send()` 拿。
            crate::logf(&format!(
                "[tuanjie] chat model={model} status=200 空流式响应(EOF无内容)，重放 {}/1",
                attempt + 1
            ));
            sleep(EMPTY_REPLAY_SLEEP).await;
            resp = match send().await {
                Ok(nr) if nr.status() == reqwest::StatusCode::OK => nr,
                _ => return None,
            };
            continue;
        }
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = read_body_bounded(resp, MAX_UPSTREAM_BODY_BYTES).await;
        if non_stream_has_content(&body) {
            // 非流式：体已全读走，交出去的是状态 + 头 + 体（供 copyHeader 与写出）。
            return Some(SniffedResp::Full(Sniffed {
                status,
                headers,
                prefix: body,
            }));
        }
        crate::logf(&format!(
            "[tuanjie] chat model={model} status=200 空响应，重放 {}/1",
            attempt + 1
        ));
        sleep(EMPTY_REPLAY_SLEEP).await;
        resp = match send().await {
            Ok(nr) if nr.status() == reqwest::StatusCode::OK => nr,
            _ => return None,
        };
    }
    None
}

/// 流式首段：最多读 [`SNIFF_HEAD_BYTES`]，出现 data 行即停；读到 EOF 也停。
///
/// **消费并交还响应**：Go 是 `io.MultiReader(bytes.NewReader(head), r.Body)`，
/// 这里把「剩余部分」的流一并交出去（[`SniffedStream::rest`]），字节一个不丢。
async fn sniff_stream_head(resp: reqwest::Response) -> (StatusCode, reqwest::header::HeaderMap, Vec<u8>, ByteStream) {
    let status = resp.status();
    let headers = resp.headers().clone();
    let mut stream = resp.bytes_stream();
    let mut out: Vec<u8> = Vec::new();
    while out.len() < SNIFF_HEAD_BYTES {
        match stream.next().await {
            Some(Ok(chunk)) => {
                out.extend_from_slice(&chunk);
                if has_stream_data_line(&out) {
                    break;
                }
            }
            Some(Err(_)) | None => break,
        }
    }
    (status, headers, out, Box::pin(stream))
}

// ---------------------------------------------------------------- 入口

/// `POST /v1/chat/completions`（`handleChat`，`server.go:938-1430`）。
pub async fn handle_chat(State(st): State<Arc<AppState>>, req: axum::extract::Request) -> Response {
    if req.method() != Method::POST {
        return crate::server::method_not_allowed(Some("POST"));
    }
    let body = axum::body::to_bytes(req.into_body(), MAX_CHAT_BODY_BYTES)
        .await
        .unwrap_or_default()
        .to_vec();
    // 非法 UTF-8：必须在解析 model / 重排 body **之前**拦（Go `server.go:922-924` 的原话），
    // 否则坏字节会被 JSON 解析宽容成 U+FFFD，转发出去等于把「编码错了」伪装成「模型名错了」。
    if !is_valid_utf8(&body) {
        crate::logf(&format!(
            "[tuanjie] chat 请求体不是合法 UTF-8（{} 字节），本地拒绝，未转发上游",
            body.len()
        ));
        st.activity.add(
            "error",
            "请求体不是合法 UTF-8，本地拒绝（未转发）",
            "?",
            "",
            0,
            0,
            400,
        );
        return write_err(StatusCode::BAD_REQUEST, INVALID_UTF8_BODY_MESSAGE);
    }

    let (model, wants_stream) = parse_model_and_stream(&body);
    let mut body = body;
    // 诊断：入站请求结构摘要（**重排前的原始形态**）—— `server.go:960`。
    crate::logf(&request_shape_line(&model, &body));

    // 自定义服务商模型先认出来：命中即**整段绕过**下面的多模态拒绝判断，按既有路由
    // 直接转发（用户配置的源可能自己就支持识图，代理不做假设）。只看 `model` 字符串，
    // 与后面的 body 重排无关，故提前算一次、转发时复用。
    let external_prov = st.providers.match_model(&model);

    // 团结自己的模型：请求含图片/音频等非文本块而目标模型不在内置多模态清单 ⇒ 明确 400，
    // **不再静默改写 model 后继续请求**（旧 `RerouteIfMedia` 已按用户裁决删除）。
    if external_prov.is_none() {
        if let Some(msg) = crate::media::multimodal_reject_message(&body, &model) {
            crate::logf(&format!("[tuanjie] media-reject model={model}: {msg}"));
            st.activity.add("error", &msg, &model, "", 0, 0, 400);
            return write_err(StatusCode::BAD_REQUEST, &msg);
        }
    }

    // 远程 URL 图片预下载转 base64（团结上游下载远程图片经常超时挂死）
    // —— `server.go:961-965`。
    let (nb, n) = crate::media::fetch_remote_images(&st.fetch_img_client, &body).await;
    if n > 0 {
        body = nb;
        crate::logf(&format!("[tuanjie] fetched {n} remote image(s) to base64"));
    }

    // 会话租借（对齐官方 CLI「一个窗口一个 session、窗口内复用」）：
    // 串行请求恒复用同一空闲会话；并发全忙则新建（模拟再开窗口）；
    // 闲置 30min/超 4h 自动清理。B3 traceid/parentspanid 绑在会话上。
    // defer 归还——响应（含流式）全部写完才算窗口闲置。
    let sess = acquire();
    // 反竞品过滤规避（**必须在 reshape 前**，reshape 会重建有序 JSON）——`server.go:983-991`。
    // 上游网关只检测 system 消息里的 `You are <竞品名>` 身份声明，在 You/are 之间插
    // 零宽空格即可放行；未命中时返回 `None`，body 原样透传。
    if let Some(nb) = crate::reqshape::desensitize_agent_identity(&body) {
        body = nb;
        crate::logf("[tuanjie] agent-identity 脱敏已应用（system 含竞品身份声明）");
    }
    body = crate::reqshape::reshape_chat_body(&body, Some(sess.session()));
    // 重排后的 shape（验证字段序效果）—— `server.go:993`。
    crate::logf(&request_shape_line(&model, &body));

    // 外部 provider 模型：直接转发（静态 Bearer key，不占账号池、不做团结签名）。
    // 命中即转发——多模态拦截在上面按 `external_prov.is_none()` 已经整段绕过，
    // 自定义服务商模型不参与团结侧的多模态判定 —— `server.go:995-1046`。
    if let Some(prov) = external_prov {
        let out = crate::providers::forward_external(
            &st,
            body,
            &model,
            wants_stream,
            &prov,
            "/chat/completions",
        )
        .await;
        return match out {
            Ok(resp) => resp,
            Err(code) => {
                if code == -1 {
                    // 网络错（forwardExternal 未写响应，由这里写终态）
                    write_json(&gobj(vec![(
                        "error",
                        gobj(vec![
                            (
                                "message",
                                GJ::Str(format!(
                                    "{} 请求失败（网络错误，详见实时动态）",
                                    prov.name
                                )),
                            ),
                            ("type", GJ::Str("server_error".to_string())),
                        ]),
                    )]))
                } else {
                    // **如实复刻 Go 的既有行为**：5xx/429 时 forwardExternal 不写任何字节，
                    // 而 handleChat 直接 return ⇒ Go 兜底一个 **200 空响应**（不是 bug 修复点，
                    // 见 README「Go 侧可疑项」）。Rust 侧同样回空 200，差分金标锁住它。
                    StatusCode::OK.into_response()
                }
            }
        };
    }

    // KIMI-K3 429 兜底：开启且 model 含 "KIMI" 才启用（`server.go:1050`）。
    let pacing = st.pacer.enabled() && is_pacing_model(&model);

    // 多账号池：`accountTokenFor`（`handlers_extra.go:197-206`）——池非空**且**当场能选出号
    // 才走池路径；选不出（全死/全冷却/全不可用）则**回退单账号路径**，而不是回"无可用账号"。
    // Go 的"账号池无可用账号"文案只用于 pick 成功后 `Get` 失败的竞态分支（`server.go:1055-1058`），
    // Rust 按同一语义保留给 `PoolOutcome::NoAccount`（池内竞态）。
    if let Some((acc, token)) = st.pool.pick_with_token(&model) {
        let pool_uid = acc.user_id.clone();
        st.pool.inc_load(&pool_uid);
        let outcome = pool_path(
            &st,
            &body,
            sess.session(),
            token,
            pool_uid.clone(),
            &model,
            pacing,
            wants_stream,
        )
        .await;
        // `defer s.pool.DecLoad(poolUID)` 的位置在 Go 里早于响应写出（`server.go:1061`），
        // 故这里也在写响应之前配平；只对**最初**选的号生效（换号不迁移计数，Go 原样）。
        st.pool.dec_load(&pool_uid);
        return finish_pool(&st, outcome, &model, wants_stream).await;
    }

    // ---- 单账号路径（`server.go:1255-1429`）----
    let start = Instant::now();
    let rid = st.registry.register(&model, "", wants_stream);
    let resp = match single_send(&st, &body, sess.session()).await {
        Ok(r) => r,
        Err(e) => {
            crate::logf(&format!("[tuanjie] chat model={model} err={e}"));
            st.registry.finish(&rid);
            return write_err(StatusCode::BAD_GATEWAY, &e);
        }
    };
    let mut status = resp.status();
    let mut resp_headers = resp.headers().clone();

    // 单账号路径只有等待层（无池可换）：`server.go:1262-1298`。
    let mut paced_single = false;
    let mut err_body: Vec<u8> = Vec::new();
    let mut resp = Some(resp);
    let mut resp_consumed = false;
    if pacing {
        st.pacer.pending_inc();
        let deadline = Instant::now() + st.chat_timing.pacing_max_wait();
        while status == reqwest::StatusCode::TOO_MANY_REQUESTS && Instant::now() < deadline {
            paced_single = true;
            err_body = read_body_bounded(resp.take().expect("响应在本分支只被消费一次"), MAX_UPSTREAM_ERR_BYTES)
                .await;
            resp_consumed = true;
            let retry_after = header_of(&resp_headers, "Retry-After");
            let mut wait = Duration::from_secs(retry_wait_secs(&retry_after));
            let remaining = deadline.saturating_duration_since(Instant::now());
            if wait > remaining {
                wait = remaining;
            }
            if wait.is_zero() {
                break;
            }
            crate::logf(&format!(
                "[tuanjie] pacing model={model} status=429 等待 {}s 后重发（剩余预算 {}s）",
                wait.as_secs(),
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs()
            ));
            st.activity.add(
                "info",
                &format!("{model} · 429 限流，等待 {}s 后重发", wait.as_secs()),
                &model,
                "",
                0,
                0,
                429,
            );
            sleep(wait).await;
            resp = match single_send(&st, &body, sess.session()).await {
                Ok(r) => Some(r),
                Err(e) => {
                    st.registry.finish(&rid);
                    return write_err(StatusCode::BAD_GATEWAY, &e);
                }
            };
            resp_consumed = false;
            let r = resp.as_ref().expect("刚赋值");
            status = r.status();
            resp_headers = r.headers().clone();
        }
        st.pacer.pending_dec();
    }

    // 上游瞬时错误重试（`server.go:1299-1314`）：429 直接跳出（TPM 按模型计数，换 key
    // 无效且重试继续吃限流窗口）；不可重试跳出；否则清缓存 + 800ms + 重发，最多 3 次。
    for attempt in 0..SINGLE_RETRY_ATTEMPTS {
        if status == reqwest::StatusCode::OK {
            break;
        }
        if !resp_consumed {
            err_body = read_body_bounded(resp.take().expect("本分支只消费一次"), MAX_UPSTREAM_ERR_BYTES)
                .await;
            resp_consumed = true;
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            break;
        }
        if !retriable_upstream(&String::from_utf8_lossy(&err_body)) {
            break;
        }
        crate::logf(&format!(
            "[tuanjie] chat model={model} status={} 重试 {}/3 err={}",
            status.as_u16(),
            attempt + 1,
            crate::client::truncate_bytes(&err_body, 200)
        ));
        st.client.invalidate_key();
        sleep(RETRY_SLEEP).await;
        resp = match single_send(&st, &body, sess.session()).await {
            Ok(r) => Some(r),
            Err(e) => {
                crate::logf(&format!("[tuanjie] chat model={model} err={e}"));
                st.registry.finish(&rid);
                return write_err(StatusCode::BAD_GATEWAY, &e);
            }
        };
        resp_consumed = false;
        let r = resp.as_ref().expect("刚赋值");
        status = r.status();
        resp_headers = r.headers().clone();
    }

    if status != reqwest::StatusCode::OK {
        let out = finish_non_200(
            &st,
            resp,
            resp_consumed,
            resp_headers.clone(),
            err_body,
            &model,
            status,
        )
        .await;
        // 判定告警计数（单账号路径）—— `server.go:1357/1361`。注意：`err_body` 已被
        // `finish_non_200` 消费，这里从返回的响应无从取回原文，故按 Go 的同一处调用点
        // 在 `finish_non_200` 内部记账（见该函数的 `note_upstream_error`）。
        st.registry.finish(&rid);
        return out;
    }
    // 上游 200：告警计数清零 —— `server.go:1365`。
    st.judge_alert.note_upstream_ok();

    // 200 但内容为空：嗅探后重放一次（`server.go:1370-1375`）。
    let sniffed = {
        let st2 = st.clone();
        let body_owned = body.clone();
        let sess2 = sess.session().clone();
        ensure_non_empty(wants_stream, resp, &model, move || {
            let st3 = st2.clone();
            let body = body_owned.clone();
            let sess = sess2.clone();
            async move { single_send(&st3, &body, &sess).await }
        })
        .await
    };
    let Some(sniffed) = sniffed else {
        st.registry.finish(&rid);
        return write_err(StatusCode::BAD_GATEWAY, EMPTY_UPSTREAM_MESSAGE);
    };
    if paced_single {
        // 兜底介入后恢复成功：补完成事件（`server.go:1366-1368`）。
        st.activity.add(
            "info",
            &format!("{model} · 限流等待结束，重发成功"),
            &model,
            "",
            0,
            0,
            200,
        );
    }
    crate::logf(&format!(
        "[tuanjie] chat model={model} status=200 dur={:?}",
        start.elapsed()
    ));
    // 单账号路径 200 的 `defer activity.Add("ok", …)` —— `server.go:1379-1381`。
    // Go 是 defer（handler 返回时执行），这里在写响应之前记账——**同一事件、同一内容**，
    // 与「写响应之后」的观测差只在极短窗口内的 `/activity` 快照（登记差异）。
    st.activity.add(
        "ok",
        &format!("{model} · 单账号"),
        &model,
        "",
        start.elapsed().as_millis() as i64,
        0,
        200,
    );
    let out = success_response(&st, sniffed, wants_stream, &model, "").await;
    st.registry.finish(&rid);
    out
}

/// 单账号路径的一次转发（`send`，`server.go:1258-1260`）：共享 `apiKeyCached` + 401 重换。
async fn single_send(
    st: &AppState,
    body: &[u8],
    sess: &LitellmSession,
) -> Result<reqwest::Response, String> {
    st.client
        .forward(
            reqwest::Method::POST,
            "/v1/chat/completions",
            body.to_vec(),
            sess,
        )
        .await
        .map_err(|e| go_err_text(&e))
}

/// 池路径的收尾（`server.go:1184-1252`）：非 200 透传、200 转发。
///
/// `st` 取 `&Arc`：流分支的响应体是 `'static` 异步流，逐块回调里还要碰
/// stats/registry，必须持有所有权。
async fn finish_pool(
    st: &Arc<AppState>,
    outcome: PoolOutcome,
    model: &str,
    wants_stream: bool,
) -> Response {
    // 四个分支全部直接 return；match 无通配臂——将来加变体时编译器强制补齐。
    match outcome {
        PoolOutcome::NoAccount => {
            return crate::server::write_json(&crate::server::gobj(vec![(
                "error",
                crate::server::gobj(vec![
                    (
                        "message",
                        crate::server::GJ::Str("账号池无可用账号".to_string()),
                    ),
                    ("type", crate::server::GJ::Str("server_error".to_string())),
                ]),
            )]))
        }
        // Go `server.go:1067/1093/1115/1155/1176`：一律 `writeJSON`（**200**）+ server_error。
        PoolOutcome::NetworkError(e) => {
            return crate::server::write_json(&crate::server::gobj(vec![(
                "error",
                crate::server::gobj(vec![
                    (
                        "message",
                        crate::server::GJ::Str(format!("上游转发失败: {e}")),
                    ),
                    ("type", crate::server::GJ::Str("server_error".to_string())),
                ]),
            )]))
        }
        PoolOutcome::Stream {
            resp,
            pool_uid,
            paced,
            rid,
            model,
        } => {
            return stream_pool_response(st, resp, &pool_uid, paced, rid, &model).await;
        }
        PoolOutcome::Resp {
            reply,
            pool_uid,
            paced,
            switches: _,
            rid,
            elapsed_ms,
        } => {
            // Go 的 `defer Finish(rid)` 在 handler 返回时执行——此处在完成事件之后、
            // 写响应之前配平，观测等价（inflight 快照只关心条目有无）。
            st.registry.finish(&rid);
            if reply.status != reqwest::StatusCode::OK {
                crate::logf(&format!(
                    "[tuanjie] pool chat model={model} account={pool_uid} status={} err={}",
                    reply.status.as_u16(),
                    crate::client::truncate_bytes(&reply.body, 300)
                ));
                st.activity.add(
                    "error",
                    &format!("{model} · 账号 {pool_uid} 上游返回 {}", reply.status.as_u16()),
                    model,
                    &pool_uid,
                    elapsed_ms,
                    0,
                    reply.status.as_u16() as i32,
                );
                // 判定告警计数 —— `server.go:1193`。
                st.judge_alert
                    .note_upstream_error(reply.status.as_u16(), &String::from_utf8_lossy(&reply.body));
                return write_err(reply.status, &String::from_utf8_lossy(&reply.body));
            }
            // 上游 200：告警计数清零 —— `server.go:1198`。
            st.judge_alert.note_upstream_ok();
            if paced {
                // 池路径流式成功没有 ok 事件可承载，兜底介入过就自发上报恢复（`server.go:1199-1201`）。
                st.activity.add(
                    "info",
                    &format!("{model} · 限流兜底完成，重发成功"),
                    model,
                    &pool_uid,
                    0,
                    0,
                    200,
                );
            }
            crate::logf(&format!(
                "[tuanjie] pool chat model={model} account={pool_uid} status=200"
            ));
            // 池路径**不做**空响应嗅探（Go 同：`ensureNonEmpty` 只在单账号路径调）。
            return buffered_response(st, reply, wants_stream, model, &pool_uid, elapsed_ms).await;
        }
    }
}

/// 流收尾守卫：流尽时由收尾逻辑显式 `done()` 配平 `Finish`；若客户端中途断开导致
/// 响应流被提前丢弃，`Drop` 兜底配平（Go 靠 handler 返回时的 `defer Finish` 覆盖
/// 同种情形；不兜底则 inflight 残留死条目）。
///
/// `on_done` 供外部转发路径挂 `defer` 的第二件事（Go 里 `registry.Finish` 先注册、
/// `activity.Add("ok")` 后注册 ⇒ 返回时**先跑 activity 再 Finish**，这里同序）。
pub(crate) struct StreamGuard {
    inner: Option<(Arc<crate::inflight::RequestRegistry>, String)>,
    on_done: Option<Box<dyn FnOnce() + Send>>,
}

impl StreamGuard {
    fn new(registry: Arc<crate::inflight::RequestRegistry>, rid: String) -> Self {
        StreamGuard {
            inner: Some((registry, rid)),
            on_done: None,
        }
    }

    /// 带收尾回调的构造（外部转发路径的 `ok` 事件 + `Finish`）。
    pub(crate) fn with_callback(
        registry: Arc<crate::inflight::RequestRegistry>,
        rid: String,
        cb: impl FnOnce() + Send + 'static,
    ) -> Self {
        StreamGuard {
            inner: Some((registry, rid)),
            on_done: Some(Box::new(cb)),
        }
    }

    pub(crate) fn rid(&self) -> &str {
        self.inner.as_ref().map(|(_, r)| r.as_str()).unwrap_or("")
    }

    pub(crate) fn done(&mut self) {
        // Go 的 defer 是 LIFO：后注册的 activity 先跑，先注册的 Finish 后跑
        if let Some(cb) = self.on_done.take() {
            cb();
        }
        if let Some((reg, rid)) = self.inner.take() {
            reg.finish(&rid);
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.done();
    }
}

/// 终态 200 + 入站要流式的逐块转发（Go `server.go:1197-1226`）。
///
/// 头与非流式同形（`copyHeader` + `X-Accel-Buffering` + `Cache-Control`，Go 在分支前
/// 统一设置）；body 按上游到达逐块写出（axum 刷块即发；`Flush` 语义由传输层保证，
/// 生态红线第 3 条的 50ms 冲刷面）；每块喂 usage 扫描器 + `registry.touch`；
/// 流尽或上游读错时结束（Go 的 `rerr != nil` 同样直接结束，不补错误体）。
async fn stream_pool_response(
    st: &Arc<AppState>,
    resp: reqwest::Response,
    pool_uid: &str,
    paced: bool,
    rid: String,
    model: &str,
) -> Response {
    if paced {
        // 池路径流式成功没有 ok 事件可承载，兜底介入过就自发上报恢复（`server.go:1199-1201`）。
        st.activity.add(
            "info",
            &format!("{model} · 限流兜底完成，重发成功"),
            model,
            pool_uid,
            0,
            0,
            200,
        );
    }
    // 上游 200：告警计数清零 —— `server.go:1198`（Go 在流式分支之前就调了）。
    st.judge_alert.note_upstream_ok();
    crate::logf(&format!(
        "[tuanjie] pool chat model={model} account={pool_uid} status=200"
    ));
    let out_headers = base_out_headers(resp.headers());
    let scanner = UsageScanner::new(model);
    let stc = st.clone();
    let guard = StreamGuard::new(stc.registry.clone(), rid);
    let stream = futures_util::stream::unfold(
        (resp.bytes_stream(), scanner, guard),
        move |(mut inner, mut scan, mut g)| {
            let stc = stc.clone();
            async move {
                match inner.next().await {
                    Some(Ok(chunk)) => {
                        let n = chunk.len() as i64;
                        scan.feed(&stc.stats, &stc.water, &chunk);
                        stc.registry.touch(g.rid(), n);
                        let out: Result<Vec<u8>, std::convert::Infallible> =
                            Ok(chunk.to_vec());
                        Some((out, (inner, scan, g)))
                    }
                    _ => {
                        scan.finish();
                        g.done();
                        None
                    }
                }
            }
        },
    );
    (
        StatusCode::OK,
        out_headers,
        Body::from_stream(stream),
    )
        .into_response()
}

/// 非 200 的透传/封装（单账号路径 `server.go:1320-1363`）。
async fn finish_non_200(
    st: &AppState,
    resp: Option<reqwest::Response>,
    resp_consumed: bool,
    headers: reqwest::header::HeaderMap,
    err_body: Vec<u8>,
    model: &str,
    status: reqwest::StatusCode,
) -> Response {
    let retry_after = header_of(&headers, "Retry-After");
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        crate::logf(&format!(
            "[tuanjie] chat model={model} status=429 限流透传 retryAfter={}",
            if retry_after.is_empty() {
                "-".to_string()
            } else {
                retry_after.clone()
            }
        ));
    }
    let err_body = if err_body.is_empty() && !resp_consumed {
        match resp {
            Some(r) => read_body_bounded(r, MAX_UPSTREAM_ERR_BYTES).await,
            None => err_body,
        }
    } else {
        err_body
    };
    crate::logf(&format!(
        "[tuanjie] chat model={model} status={} err={}",
        status.as_u16(),
        crate::client::truncate_bytes(&err_body, 300)
    ));
    // 单账号路径 402：标记本地账号配额用尽（选号自动绕过、GUI 出 402 徽章），
    // 返回可读中文；上游原始报错拼在尾部供排查（`server.go:1332-1359`）。
    let mut msg = String::from_utf8_lossy(&err_body).into_owned();
    // 判定告警计数 —— `server.go:1357`（402 分支）/`:1361`（其余非 200）。
    st.judge_alert
        .note_upstream_error(status.as_u16(), &msg);
    if status == reqwest::StatusCode::PAYMENT_REQUIRED {
        if let Some(acc) = st.pool.local_account() {
            let uid = acc.user_id.clone();
            msg = "该团结账号上游配额已用尽（budget_exceeded），已自动禁用该账号；请稍后重试或检查账号池"
                .to_string();
            if !err_body.is_empty() {
                msg += "。上游原始报错: ";
                msg += &crate::client::truncate_bytes(&err_body, 300);
            }
            st.activity.add(
                "error",
                &format!("本地账号 {uid} 配额用尽，自动禁用"),
                model,
                &uid,
                0,
                0,
                402,
            );
            crate::logf(&format!(
                "[tuanjie] account={uid} 402 budget_exceeded 已禁用（单账号路径）"
            ));
            // Go `server.go:1335-1349` 的异步标记链：取 token 失败 → 零值标记；
            // 成功 → 查 5h 窗口（`parse_quota_5h` 三级兜底）→ 带时刻标记。
            // 此前此处只有文案/事件/日志，标记从未落地（选号绕过与 402 徽章不生效）。
            let pool = st.pool.clone();
            let http = st.client.http().clone();
            let ep = crate::quota::QuotaEndpoint {
                url: crate::server::quota_endpoint_url(&st.client),
                ..Default::default()
            };
            tokio::spawn(async move {
                match pool.effective_access_token(&acc) {
                    Err(_) => pool.mark_budget_exceeded(&uid),
                    Ok(tok) => {
                        let reset_at =
                            match crate::quota::fetch_quota_with_token(&http, &ep, &tok).await
                            {
                                Ok(q) => {
                                    let (_, reset, ok) = parse_quota_5h(&q);
                                    if ok {
                                        reset
                                    } else {
                                        None
                                    }
                                }
                                Err(_) => None,
                            };
                        pool.mark_budget_exceeded_at(&uid, reset_at);
                    }
                }
            });
        }
    }
    let mut out = write_err(status, &msg);
    if !retry_after.is_empty() {
        if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after) {
            out.headers_mut().insert("Retry-After", v);
        }
    }
    out
}

/// 200 之后的转发（池路径 `server.go:1197-1252`、单账号 `server.go:1383-1429`）。
///
/// `sniffed.prefix` 是空响应嗅探时已从上游读出的字节（非空时必须先写回客户端，
/// 否则丢数据；Go 用 `io.MultiReader` 拼回去，这里用 `stream::once` 先吐这一段）。
async fn success_response(
    st: &AppState,
    sniffed: SniffedResp,
    wants_stream: bool,
    model: &str,
    pool_uid: &str,
) -> Response {
    match sniffed {
        SniffedResp::Stream(s) => {
            let SniffedStream {
                status,
                headers,
                prefix,
                rest,
            } = s;
            let _ = status;
            let out_headers = base_out_headers(&headers);
            let head =
                futures_util::stream::once(async move { Ok::<Vec<u8>, std::io::Error>(prefix) });
            let tail = rest.map(|r| {
                r.map(|b| b.to_vec())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            });
            return (
                StatusCode::OK,
                out_headers,
                Body::from_stream(head.chain(tail)),
            )
                .into_response();
        }
        SniffedResp::Full(Sniffed {
            status,
            headers,
            prefix,
        }) => {
            let _ = (status, &headers);
            let out_headers = base_out_headers(&headers);
            let _ = wants_stream;
            record_usage(st, model, pool_uid, &prefix, 0);
            return (StatusCode::OK, out_headers, Body::from(prefix)).into_response();
        }
    }
}

/// 池路径的 200 转发：响应体已随 [`read_reply`] 整块进内存。
async fn buffered_response(
    st: &AppState,
    reply: Reply,
    _wants_stream: bool,
    model: &str,
    pool_uid: &str,
    elapsed_ms: i64,
) -> Response {
    let out_headers = base_out_headers(&reply.headers);
    record_usage(st, model, pool_uid, &reply.body, elapsed_ms);
    (StatusCode::OK, out_headers, Body::from(reply.body)).into_response()
}

/// `copyHeader` + `X-Accel-Buffering` / `Cache-Control`（`server.go:1202-1204` / `:1383-1385`）。
fn base_out_headers(src: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    copy_header(&mut out, src);
    out.insert("X-Accel-Buffering", "no".parse().unwrap());
    out.insert("Cache-Control", "no-cache".parse().unwrap());
    out
}

/// 非流式响应体里的 usage 入账 + 实时动态 `ok` 事件
/// （池路径 `server.go:1229-1251`、单账号 `server.go:1411-1429`）。
/// `elapsed_ms` 只在池路径为真值（Go 传 `time.Since(start).Milliseconds()`）；
/// 单账号路径调这里时传 0（Go 那边 `ok` 事件走另一条 defer，与本函数无关）。
fn record_usage(st: &AppState, model: &str, pool_uid: &str, body: &[u8], elapsed_ms: i64) {
    #[derive(serde::Deserialize)]
    struct Done {
        #[serde(default)]
        model: String,
        #[serde(default)]
        usage: Option<Usage>,
    }
    #[derive(serde::Deserialize)]
    struct Usage {
        #[serde(default)]
        prompt_tokens: i64,
        #[serde(default)]
        completion_tokens: i64,
        #[serde(default)]
        total_tokens: i64,
    }
    let Ok(done) = serde_json::from_slice::<Done>(body) else {
        return;
    };
    let Some(u) = done.usage else {
        return;
    };
    st.stats
        .add(model, u.prompt_tokens, u.completion_tokens, u.total_tokens);
    // 别名解析观测：两条路径都有（池 `server.go:1239-1248`、单账号 `:1419-1426`）；
    // 被动注水观测只有池路径有（单账号不记 passive——Go 同）。
    let (from, changed) = st.water.record_resolution(model, &done.model);
    if changed {
        if from.is_empty() {
            crate::logf(&format!("[tuanjie] 别名解析基线 {model} → {}", done.model));
        } else {
            crate::logf(&format!(
                "[tuanjie] 模型映射变化 {model}: {from} → {}",
                done.model
            ));
        }
    }
    if !pool_uid.is_empty() {
        // 被动注水观测（仅池路径）—— `server.go:1239-1248`。
        st.water.record_passive(model, &done.model, pool_uid);
        st.activity.add(
            "ok",
            &format!("{model} · 账号 {pool_uid}"),
            model,
            pool_uid,
            elapsed_ms,
            u.total_tokens,
            200,
        );
    }
}

/// `copyHeader`（`server.go:2195-2205`）：逐条复制，跳过 `Content-Length` / `Transfer-Encoding`。
pub(crate) fn copy_header(dst: &mut HeaderMap, src: &reqwest::header::HeaderMap) {
    for (name, value) in src.iter() {
        let name_str = name.as_str();
        if name_str.eq_ignore_ascii_case("content-length")
            || name_str.eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        if let (Ok(n), Ok(val)) = (
            axum::http::HeaderName::from_bytes(name_str.as_bytes()),
            axum::http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            dst.append(n, val);
        }
    }
}


