//! collect.rs —— 非流式聚合（`internal/codebuddy/server.go` 的 `collectStream` 逐字移植）。
//!
//! 上游只会回 SSE（Go 强制 `stream=true` 出站），客户端要非流式时把整条流聚合成一个
//! `chat.completion`：content 分片拼接、tool_calls 按 `index` 合并（`arguments` 累加）、
//! `finish_reason` 取最后一次非空值、缺省按「有 tool_calls 则 tool_calls，否则 stop」。
//!
//! **逐字节对齐点**：结果对象是 `map[string]any`，Go 用 `json.Encoder` 写（键排序 + 末尾
//! 换行）；`id` 是 `chatcmpl-` + 6 字节随机 hex（`crypto/rand`）；`created` 是秒级墙钟。
//!
//! **行边界**：Go 用 `bufio.Scanner`（`ScanLines`）取行——末条**没有换行**的残行在流结束时
//! 同样会被交给聚合（`if atEOF { return len(data), dropCR(data) }`，读错误也走这个分支），
//! 只有 `[DONE]` 触发的提前 `break` 才让它作废。本实现照做（见 `collect_stream` 末尾）；
//! 该合同由黄金 `pure.collect_eof`（真 Go `collectStream` 采收）逐字节锁住，另有
//! `eof_residual_line_is_consumed_across_chunks` 守「残行跨 chunk 拼接」这一形态。

use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};

use crate::gjson;

/// Go `bufio.Scanner` 的上限（`sc.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)`）。
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// 一个 `tool_calls` 槽（Go `collectStream` 内的 `toolSlot`）。
struct ToolSlot {
    id: String,
    name: String,
    args: String,
}

/// 聚合中间态（对应 Go `collectStream` 的局部变量，行处理抽成方法是为了能在
/// 「循环内」与「EOF 残行」两处复用同一段逻辑）。
#[derive(Default)]
struct Agg {
    content_parts: Vec<String>,
    tool_calls: std::collections::HashMap<i64, ToolSlot>,
    order: Vec<i64>,
    model: String,
    finish: String,
    usage: Option<Value>,
}

impl Agg {
    /// 处理**一行**（不含换行符；行尾 `\r` 按 `bufio.ScanLines` 的 `dropCR` 剥掉）。
    /// 返回 `true` 表示这行是 `[DONE]`（Go 的 `break`：后续行与残行都不再消费）。
    fn feed_line(&mut self, raw: &[u8]) -> bool {
        let mut line = raw;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let line = trim_ascii(line).to_vec();
        if !line.starts_with(b"data:") {
            return false;
        }
        let data = trim_ascii(&line[5..]);
        if data == b"[DONE]" {
            return true;
        }
        let Ok(chunk) = crate::godecode::unmarshal_map(data) else {
            return false;
        };
        if let Some(Value::String(m)) = chunk.get("model") {
            if !m.is_empty() {
                self.model = m.clone();
            }
        }
        if let Some(u) = chunk.get("usage") {
            if u.is_object() {
                self.usage = Some(u.clone());
            }
        }
        let Some(Value::Array(choices)) = chunk.get("choices") else {
            return false;
        };
        for c in choices {
            let Value::Object(cm) = c else { continue };
            if let Some(Value::String(fr)) = cm.get("finish_reason") {
                if !fr.is_empty() {
                    self.finish = fr.clone();
                }
            }
            let Some(Value::Object(delta)) = cm.get("delta") else {
                continue;
            };
            if let Some(Value::String(ctext)) = delta.get("content") {
                if !ctext.is_empty() {
                    self.content_parts.push(ctext.clone());
                }
            }
            if let Some(Value::Array(tcs)) = delta.get("tool_calls") {
                for t in tcs {
                    let Value::Object(tm) = t else { continue };
                    let idx = match tm.get("index").and_then(|v| v.as_f64()) {
                        Some(f) => f as i64,
                        None => 0,
                    };
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        self.tool_calls.entry(idx)
                    {
                        e.insert(ToolSlot {
                            id: String::new(),
                            name: String::new(),
                            args: String::new(),
                        });
                        self.order.push(idx);
                    }
                    let slot = self.tool_calls.get_mut(&idx).expect("刚插入");
                    if let Some(Value::String(id)) = tm.get("id") {
                        if !id.is_empty() {
                            slot.id = id.clone();
                        }
                    }
                    if let Some(Value::Object(fnv)) = tm.get("function") {
                        if let Some(Value::String(n)) = fnv.get("name") {
                            if !n.is_empty() {
                                slot.name = n.clone();
                            }
                        }
                        if let Some(Value::String(a)) = fnv.get("arguments") {
                            slot.args.push_str(a);
                        }
                    }
                }
            }
        }
        false
    }
}

/// `collectStream`：消费 OpenAI SSE 流，聚合为单个非流式 `chat.completion` 对象。
pub async fn collect_stream<S, E>(mut body: S, model_name: &str) -> Value
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    let mut agg = Agg::default();

    let mut rest: Vec<u8> = Vec::new();
    let mut eof = false;
    let mut done = false;
    while !eof {
        match body.next().await {
            Some(Ok(chunk)) => rest.extend_from_slice(&chunk),
            Some(Err(_)) | None => eof = true,
        }
        // 逐行消费缓冲（`bufio.Scanner` 不会把「不完整的尾行」交给调用方）。
        if let Some(cut) = rest.iter().rposition(|&b| b == b'\n').map(|i| i + 1) {
            let complete: Vec<u8> = rest.drain(..cut).collect();
            // `complete` 以 `\n` 结尾：`split` 末尾那个空片段不是「一行」，按 Go 的扫描语义丢掉。
            let mut parts: Vec<&[u8]> = complete.split(|&b| b == b'\n').collect();
            parts.pop();
            for raw in parts {
                if agg.feed_line(raw) {
                    done = true;
                    eof = true;
                    break;
                }
            }
        }
        // 单行超过 Go 的 16MB 上限时 `bufio.Scanner` 直接停止（ErrTooLong）：本实现把缓冲
        // 截到上限继续按行处理（差异登记在 README 差异表）。
        if rest.len() > MAX_LINE_BYTES {
            rest.truncate(MAX_LINE_BYTES);
        }
    }

    // **EOF 残行**：`bufio.Scanner` 在流结束时会把**末条没有换行的残行**也交给调用方
    // （`ScanLines` 的 `if atEOF { return len(data), dropCR(data) }` 分支，`bufio/scan.go`；
    // 读错误同样以 `atEOF=true` 进这个分支），Go 的 `collectStream` 因此会聚合它。
    // 只有 `[DONE]` 触发的提前 `break` 才让它作废（`done`）。
    if !done && !rest.is_empty() {
        let _ = agg.feed_line(&rest);
    }

    let Agg {
        content_parts,
        tool_calls,
        order,
        finish,
        model,
        usage,
    } = agg;

    let mut message = Map::new();
    message.insert("role".to_string(), gjson::s("assistant"));
    let content = content_parts.join("");
    if content.is_empty() {
        message.insert("content".to_string(), Value::Null);
    } else {
        message.insert("content".to_string(), Value::String(content));
    }
    let mut fin_reason = finish.clone();
    if !tool_calls.is_empty() {
        let tcs: Vec<Value> = order
            .iter()
            .map(|idx| {
                let slot = &tool_calls[idx];
                gjson::obj(vec![
                    ("id", gjson::s(&slot.id)),
                    ("type", gjson::s("function")),
                    (
                        "function",
                        gjson::obj(vec![
                            ("name", gjson::s(&slot.name)),
                            ("arguments", gjson::s(&slot.args)),
                        ]),
                    ),
                ])
            })
            .collect();
        message.insert("tool_calls".to_string(), Value::Array(tcs));
        if fin_reason.is_empty() {
            fin_reason = "tool_calls".to_string();
        }
    }
    if fin_reason.is_empty() {
        fin_reason = "stop".to_string();
    }
    let mut rid = [0u8; 6];
    let _ = crate::atrest::getrandom_fill(&mut rid);
    let usage = usage.unwrap_or_else(|| {
        gjson::obj(vec![
            ("prompt_tokens", Value::Number(0.into())),
            ("completion_tokens", Value::Number(0.into())),
            ("total_tokens", Value::Number(0.into())),
        ])
    });
    let choice = gjson::obj(vec![
        ("index", Value::Number(0.into())),
        ("message", Value::Object(message)),
        ("finish_reason", gjson::s(&fin_reason)),
    ]);
    gjson::obj(vec![
        ("id", gjson::s(&format!("chatcmpl-{}", hex_lower(&rid)))),
        ("object", gjson::s("chat.completion")),
        (
            "created",
            Value::Number(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
                    .into(),
            ),
        ),
        ("model", gjson::s(&or_default(&model, model_name))),
        ("choices", Value::Array(vec![choice])),
        ("usage", usage),
        ("finish_reason_debug", gjson::s(&fin_reason)),
    ])
}

fn hex_lower(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    s
}

fn or_default(s: &str, def: &str) -> String {
    if s.is_empty() {
        def.to_string()
    } else {
        s.to_string()
    }
}

/// Go `bytes.TrimSpace` / `strings.TrimSpace` 的 ASCII 子集（SSE 行内只会出现 ASCII 空白）。
pub(crate) fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &b[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn sse_stream(body: &str) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Unpin {
        let b = body.as_bytes().to_vec();
        stream::iter(vec![Ok(Bytes::from(b))])
    }

    #[tokio::test]
    async fn aggregates_content_tools_and_usage() {
        let sse = "data: {\"model\":\"m-up\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\n\
                   junk\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"f\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}},{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"g\",\"arguments\":\"[1]\"}}]}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n\
                   data: [DONE]\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"AFTER\"}}]}\n\n";
        let v = collect_stream(sse_stream(sse), "fallback-model").await;
        assert_eq!(v["model"], serde_json::json!("m-up"));
        assert_eq!(v["choices"][0]["message"]["content"], serde_json::json!("hello"));
        assert_eq!(v["choices"][0]["message"]["tool_calls"][0]["id"], serde_json::json!("c1"));
        assert_eq!(
            v["choices"][0]["message"]["tool_calls"][1]["function"]["arguments"],
            serde_json::json!("[1]")
        );
        assert_eq!(v["choices"][0]["finish_reason"], serde_json::json!("tool_calls"));
        assert_eq!(v["usage"]["total_tokens"].as_f64(), Some(3.0));
        assert!(v["id"].as_str().unwrap().starts_with("chatcmpl-"));
        assert_eq!(v["id"].as_str().unwrap().len(), "chatcmpl-".len() + 12);
    }

    /// 末帧**没有换行**、且跨了 chunk 边界：`bufio.Scanner` 在流结束（EOF 或读错误）时
    /// 仍会把它作为一条 token 交给聚合，这里锁住同一行为（Go 采收版本见黄金 `pure.collect_eof`）。
    #[tokio::test]
    async fn eof_residual_line_is_consumed_across_chunks() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"choices\":[{\"index\":0,\"delta\":{\"cont",
            )),
            Ok(Bytes::from_static(
                b"ent\":\"llo\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}",
            )),
        ];
        let v = collect_stream(stream::iter(chunks), "auto").await;
        assert_eq!(v["choices"][0]["message"]["content"], serde_json::json!("hello"));
        // 残行里的 usage 必须入账（改动前整条残行被丢 ⇒ 这里会是全 0 的缺省 usage）。
        // 帧里的数字由 `godecode` 落成 f64（Go `map[string]any` 语义），故按浮点比。
        assert_eq!(
            v["usage"],
            serde_json::json!({"prompt_tokens":1.0,"completion_tokens":2.0,"total_tokens":3.0})
        );
    }

    #[tokio::test]
    async fn defaults_when_stream_is_empty() {
        let v = collect_stream(sse_stream("garbage\n"), "auto").await;
        assert_eq!(v["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(v["choices"][0]["finish_reason"], serde_json::json!("stop"));
        assert_eq!(
            v["usage"],
            serde_json::json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0})
        );
        assert_eq!(v["model"], serde_json::json!("auto"));
    }
}
