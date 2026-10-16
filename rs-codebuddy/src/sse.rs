//! sse.rs —— 流式 SSE 的下游处理（`internal/codebuddy/server.go` 的 `sseFieldCleaner`
//! 与 `usageScanner` 的逐字移植）。
//!
//! 两件事各管一段，**都在同一份上游原始字节上跑**（Go 的转发循环就是先 feed 扫描器、
//! 再 feed 清洗器，见 `server.go:1463-1475`）：
//!
//! 1. `SseFieldCleaner`：逐行剔掉 delta 里**值为空字符串**的 `content` /
//!    `reasoning_content`，以及**空数组**的 `tool_calls`。CodeBuddy 后端每帧把这三个键全带
//!    一遍（未活动侧为空），ZCode 的流解析按字段出现切分 part，会把一次思考撕成几十个
//!    「思考」条目。非 data 行、`[DONE]`、解析失败的行**一律原样透传**；不完整行跨 feed
//!    缓冲，`flush` 冲尾。
//! 2. `UsageScanner`：扫描 `data:` 行里的 `usage`，命中即按模型入账（消耗 TOP 统计）。
//!
//! 逐行重写的帧按 Go `json.Marshal` 规则重序列化（键排序 + HTML 转义 + float64 数字），
//! 所以「改过的帧」与上游原帧在字节上不同——这正是要逐字节对齐的地方。

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::gjson;
use crate::stats::Stats;

/// `sseFieldCleaner`。
#[derive(Default)]
pub struct SseFieldCleaner {
    buf: Vec<u8>,
}

impl SseFieldCleaner {
    pub fn new() -> SseFieldCleaner {
        SseFieldCleaner { buf: Vec::new() }
    }

    /// 吃进一段上游字节，返回可写出的下游字节（可能是空）。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(i) = self.buf.iter().position(|&b| b == b'\n') else {
                break;
            };
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            append_cleaned_sse_line(&mut out, &line);
        }
        out
    }

    /// 冲掉尾部残行（Go `flush`）。
    pub fn flush(&mut self) -> Vec<u8> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let rest = std::mem::take(&mut self.buf);
        let mut out: Vec<u8> = Vec::new();
        append_cleaned_sse_line(&mut out, &rest);
        out
    }
}

/// Go `bytes.TrimSpace` 的 ASCII 子集（SSE 行里只会出现 ASCII 空白）。
fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map(|i| i + 1).unwrap_or(start);
    &b[start..end]
}

/// `appendCleanedSSELine`：仅当是可解析的 data 行且 delta 里有空字符串的
/// content / reasoning_content、或空数组的 tool_calls 时才重写，否则原样追加。
pub fn append_cleaned_sse_line(out: &mut Vec<u8>, line: &[u8]) {
    let trimmed = trim_ascii(line);
    if !trimmed.starts_with(b"data:") || trimmed.windows(6).any(|w| w == b"[DONE]") {
        out.extend_from_slice(line);
        return;
    }
    let Ok(mut chunk) = crate::godecode::unmarshal_map(trim_ascii(&trimmed[5..])) else {
        out.extend_from_slice(line);
        return;
    };
    let choices = match chunk.get_mut("choices") {
        Some(Value::Array(a)) => a,
        _ => {
            out.extend_from_slice(line);
            return;
        }
    };
    let mut changed = false;
    for c in choices.iter_mut() {
        let Value::Object(cm) = c else { continue };
        let Some(Value::Object(delta)) = cm.get_mut("delta") else {
            continue;
        };
        for k in ["content", "reasoning_content"] {
            if let Some(Value::String(s)) = delta.get(k) {
                if s.is_empty() {
                    delta.remove(k);
                    changed = true;
                }
            }
        }
        // 空数组的 tool_calls 也必须剔：ZCode 的 openai 兼容解析用 `ce.tool_calls != null`
        // 判定工具调用，空数组同样满足该条件（详见 Go 注释）。
        if let Some(Value::Array(arr)) = delta.get("tool_calls") {
            if arr.is_empty() {
                delta.remove("tool_calls");
                changed = true;
            }
        }
    }
    if !changed {
        out.extend_from_slice(line);
        return;
    }
    let nb = gjson::to_go(&Value::Object(chunk));
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(nb.as_bytes());
    out.push(b'\n');
}

/// `usageScanner`：拼行、解析 `data:` 行里的 usage，命中即入账。
pub struct UsageScanner {
    model: String,
    buf: Vec<u8>,
    stats: Arc<Stats>,
}

impl UsageScanner {
    pub fn new(model: &str, stats: Arc<Stats>) -> UsageScanner {
        UsageScanner {
            model: model.to_string(),
            buf: Vec::new(),
            stats,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        loop {
            let Some(i) = self.buf.iter().position(|&b| b == b'\n') else {
                break;
            };
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = trim_ascii(&line);
            if !line.starts_with(b"data:") || line.windows(6).any(|w| w == b"[DONE]") {
                continue;
            }
            if let Some((p, c, t)) = parse_usage(trim_ascii(&line[5..])) {
                self.stats.add_stat(&self.model, p, c, t);
            }
        }
    }
}

/// 解析一帧里的 `usage`（Go 侧 `json.Unmarshal` 进匿名结构体：`usage` 必须是对象，
/// 三个字段必须是**整数值**，否则整次解析失败、不入账；字段名匹配大小写不敏感）。
///
/// **已登记的差异（README 差异 41，本轮只登记不给对齐）**：本实现这条路径上的帧解析走
/// `godecode`，它把**所有**数字折成 f64（对齐 Go 的 `map[string]any` 语义），到这一步
/// **原始字面量已不存在**，因此只能按「f64 且无小数部分」判断——Go 用
/// `strconv.ParseInt(字面量, 10, 64)`，会拒绝的 `1e2` / `1.0` 这里会被接受、超过 2^53
/// 会丢精度、溢出会饱和（Go 一律报错 ⇒ 该帧不入账）。真实上游只发普通整数，两侧取值相同。
/// 逐字对齐要把「字面量」带出解析层（改 `godecode` 的数字模型会牵动 chat 请求体那条已被
/// 黄金逐字节锁住的路径；或另写一个 int64 专用扫描器）——两者都不是本轮该动的。
/// 对照：`stats.rs::load_from_disk` 那条路径 **已对齐**（serde_json 保留了整数形/浮点形之分）。
fn parse_usage(data: &[u8]) -> Option<(i64, i64, i64)> {
    let m = crate::godecode::unmarshal_map(data).ok()?;
    let usage = m.get("usage")?;
    let Value::Object(u) = usage else { return None };
    let get_int = |name: &str| -> Option<i64> {
        let mut found = None;
        for (k, v) in u {
            if k.eq_ignore_ascii_case(name) {
                let f = v.as_f64()?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return None; // Go 解 int64 遇非整数报错 → 整次失败
                }
                found = Some(f as i64);
            }
        }
        found
    };
    // 「缺字段 = 0」，但字段存在而类型不对 → 失败。
    let p = match u.keys().find(|k| k.eq_ignore_ascii_case("prompt_tokens")) {
        Some(_) => get_int("prompt_tokens")?,
        None => 0,
    };
    let c = match u.keys().find(|k| k.eq_ignore_ascii_case("completion_tokens")) {
        Some(_) => get_int("completion_tokens")?,
        None => 0,
    };
    let t = match u.keys().find(|k| k.eq_ignore_ascii_case("total_tokens")) {
        Some(_) => get_int("total_tokens")?,
        None => 0,
    };
    Some((p, c, t))
}

/// 供 `collect_stream` 复用的「取 usage map」：Go 的 `collectStream` 直接把
/// `chunk["usage"]` 原样放进结果（不校验字段类型）。
pub fn usage_value(chunk: &Map<String, Value>) -> Option<Value> {
    chunk.get("usage").cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaner_rewrites_only_when_a_key_is_dropped() {
        let mut c = SseFieldCleaner::new();
        // 未变化：原样（连帧里的原始空格都不动）
        let same = "data: {\"id\":\"a\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n";
        assert_eq!(String::from_utf8(c.feed(same.as_bytes())).unwrap(), same);
        // 变化：重写为 Go json.Marshal 形态（键排序）
        let dirty = "data: {\"id\":\"b\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"tool_calls\":[]}}]}\n";
        assert_eq!(
            String::from_utf8(c.feed(dirty.as_bytes())).unwrap(),
            "data: {\"choices\":[{\"delta\":{},\"index\":0}],\"id\":\"b\"}\n"
        );
        // 非 data 行 / [DONE] / 坏 JSON 原样
        let other = ": x\ndata: [DONE]\ndata: {broken\n";
        assert_eq!(String::from_utf8(c.feed(other.as_bytes())).unwrap(), other);
        // 跨 feed 的残行 + flush
        assert!(c
            .feed(b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"\"}}]}")
            .is_empty());
        assert_eq!(
            String::from_utf8(c.flush()).unwrap(),
            "data: {\"choices\":[{\"delta\":{}}],\"id\":\"c\"}\n"
        );
    }

    #[test]
    fn cleaner_keeps_non_ascii_and_crlf_semantics() {
        let mut c = SseFieldCleaner::new();
        let with_crlf = "data: {\"id\":\"d\",\"choices\":[{\"delta\":{\"reasoning_content\":\"\",\"content\":\"\"}}]}\r\n";
        assert_eq!(
            String::from_utf8(c.feed(with_crlf.as_bytes())).unwrap(),
            "data: {\"choices\":[{\"delta\":{}}],\"id\":\"d\"}\n"
        );
    }
}
