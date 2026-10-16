//! `internal/proxy/proxy.go` 的流式骨架：`forEachLine` / `readFirstLine` / `streamCCEvents`
//! / `readCCResult`。
//!
//! Go 用阻塞 `io.Reader`（`bufio.NewReader.ReadBytes('\n')`），Rust 侧 reqwest 给的是
//! async 字节流——这里把上游体**边收边切行**，语义逐条对齐：
//!
//! * **无行路上限**（Go 用 `ReadBytes` 而不是 `bufio.Scanner`，所以 5MB 的单行事件不会被截断）；
//! * 只有**非空 trimmed 行**交给回调；
//! * 读错误时：**先把已读到的行处理完**，再报错（Go 的 `ReadBytes` 返回 `data + err`，
//!   先处理 data 再判 err）；
//! * `[DONE]` 之类没有特殊处理——CommandCode 的事件是逐行 JSON。

use futures_util::StreamExt;
use serde_json::Value;

use crate::api::{CCTotalUsage, ToolCall};
use crate::gjson::to_go;

/// 处理一行（返回 `Err` 表示要中断整条流，与 Go 的 `fn(string) error` 同义）。
pub type LineFn<'a> = dyn FnMut(&str) -> Result<(), ()> + 'a;

/// 上游体的**一次读取**缓冲：把 `reqwest::Response` 取成字节流并缓存已读字节。
///
/// ⚠ 为什么不用 `Response::chunk()`：它对**已整体到达**的响应（axum 假上游就是这样）
/// 会一次性把整段体交出来并**吞掉剩余部分**——实测 `chunk()` 一次拿到全文，
/// 再 `chunk()` 得 `None`、之后 `bytes_stream()` 也是空。Go 那边是
/// `bufio.NewReader(body).ReadBytes('\n')`，读一行后剩下仍在 reader 里等着被读。
/// 这里用同样的语义：**只取一次流**，自己按行切。
pub struct LineReader {
    stream: futures_util::stream::BoxStream<'static, Result<Vec<u8>, reqwest::Error>>,
    buf: Vec<u8>,
    done: bool,
}

impl LineReader {
    pub fn new(resp: reqwest::Response) -> LineReader {
        use futures_util::StreamExt as _;
        LineReader {
            stream: Box::pin(resp.bytes_stream().map(|r| r.map(|b| b.to_vec()))),
            buf: Vec::new(),
            done: false,
        }
    }

    /// 取下一行（含换行，trim 后的内容）；流结束返回 `None`。
    pub async fn next_line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = trim_space(&String::from_utf8_lossy(&line_bytes));
                if line.is_empty() {
                    continue; // Go 的 `forEachLine` 也丢空行
                }
                return Some(line);
            }
            if self.done {
                if self.buf.is_empty() {
                    return None;
                }
                let line = trim_space(&String::from_utf8_lossy(&self.buf));
                self.buf.clear();
                if line.is_empty() {
                    return None;
                }
                return Some(line);
            }
            match self.stream.next().await {
                Some(Ok(c)) => self.buf.extend_from_slice(&c),
                Some(Err(_)) => {
                    // 读错误：Go 的 `forEachLine` 会把**已读到的**行先处理掉，再返回错误。
                    // 本 crate 记一行日志后按流结束处理（剩余缓冲照常交出去）。
                    self.done = true;
                }
                None => self.done = true,
            }
        }
    }

    /// `readFirstLine`：读第一行（不阻塞等更多数据）。
    pub async fn first_line(&mut self) -> String {
        self.next_line().await.unwrap_or_default()
    }

    /// 把剩下的全部行喂给回调（`forEachLine` 的语义）。
    pub async fn for_each_remaining(&mut self, mut f: impl FnMut(&str) -> Result<(), ()>) {
        while let Some(line) = self.next_line().await {
            if f(&line).is_err() {
                return;
            }
        }
    }

    /// 把剩下的体读完（`readCCResult` 用）。
    pub async fn read_all_lines(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(line) = self.next_line().await {
            out.push(line);
        }
        out
    }
}

/// `strings.TrimSpace` 的等价物（Go 的 TrimSpace 认 Unicode 的 White_Space + `\u0085`）。
fn trim_space(s: &str) -> String {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{85}' || c == '\u{a0}')
        .to_string()
}

/// `readFirstLine`：读第一行（不阻塞等更多数据）。返回 `(行, 是否有错)`。
///
/// 用 `Response::chunk()`（对 `&mut Response` 可用）而不是 `bytes_stream()`——
/// 后者**按值**取走响应，`readFirstLine` 之后还要继续读剩下的体，必须能借用。
pub async fn read_first_line(resp: &mut reqwest::Response) -> (String, bool) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(c)) => {
                buf.extend_from_slice(&c);
                if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line = String::from_utf8_lossy(&buf[..=pos]);
                    return (trim_space(&line), false);
                }
            }
            Ok(None) => break,
            Err(_) => return (trim_space(&String::from_utf8_lossy(&buf)), true),
        }
    }
    (trim_space(&String::from_utf8_lossy(&buf)), false)
}

/// `streamCCEvents` 的返回：`Some(tokens)` 表示上游以 context 超限拒绝（重试信号）。
pub struct StreamHead {
    pub retry_tokens: i64,
}

/// `readCCResult` 的累积结果（`ccResult`）。
#[derive(Default)]
pub struct CCResult {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_by_id: Vec<(String, usize)>,
    pub tool_input_buffers: Vec<(String, String)>,
    pub has_tool_calls: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub finish_reason: String,
}

impl CCResult {
    fn idx_of(&self, id: &str) -> Option<usize> {
        self.tool_call_by_id
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| *v)
    }

    fn set_idx(&mut self, id: &str, i: usize) {
        if let Some(slot) = self.tool_call_by_id.iter_mut().find(|(k, _)| k == id) {
            slot.1 = i;
        } else {
            self.tool_call_by_id.push((id.to_string(), i));
        }
    }

    fn buf_push(&mut self, id: &str, s: &str) {
        if let Some(slot) = self.tool_input_buffers.iter_mut().find(|(k, _)| k == id) {
            slot.1.push_str(s);
        }
    }

    /// 处理一行事件（`readCCResult` 的 switch）。
    pub fn feed_line(&mut self, line: &str) {
        let ev = match crate::api::CCStreamEvent::from_line(line) {
            Some(e) => e,
            None => {
                return;
            }
        };
        match ev.r#type.as_str() {
            "text-delta" => self.content.push_str(&ev.text),
            "tool-use" => {
                self.has_tool_calls = true;
                self.set_idx(&ev.tool_call_id, self.tool_calls.len());
                self.tool_calls.push(ToolCall {
                    id: ev.tool_call_id.clone(),
                    r#type: "function".to_string(),
                    function_name: ev.tool_name.clone(),
                    function_arguments: String::new(),
                });
            }
            "tool-delta" => {
                if let Some(last) = self.tool_calls.last_mut() {
                    last.function_arguments.push_str(&ev.text);
                }
            }
            "tool-input-start" => {
                self.has_tool_calls = true;
                self.set_idx(&ev.id, self.tool_calls.len());
                if !self.tool_input_buffers.iter().any(|(k, _)| k == &ev.id) {
                    self.tool_input_buffers.push((ev.id.clone(), String::new()));
                }
                self.tool_calls.push(ToolCall {
                    id: ev.id.clone(),
                    r#type: "function".to_string(),
                    function_name: ev.tool_name.clone(),
                    function_arguments: String::new(),
                });
            }
            "tool-input-delta" => {
                self.buf_push(&ev.id, &ev.delta);
                if let Some(i) = self.idx_of(&ev.id) {
                    self.tool_calls[i].function_arguments.push_str(&ev.delta);
                }
            }
            "tool-call" => {
                self.has_tool_calls = true;
                let args = match &ev.input {
                    Some(v) => to_go(v),
                    None => String::new(),
                };
                match self.idx_of(&ev.tool_call_id) {
                    Some(i) => {
                        self.tool_calls[i].function_name = ev.tool_name.clone();
                        if !args.is_empty() {
                            self.tool_calls[i].function_arguments = args;
                        }
                    }
                    None => {
                        self.set_idx(&ev.tool_call_id, self.tool_calls.len());
                        self.tool_calls.push(ToolCall {
                            id: ev.tool_call_id.clone(),
                            r#type: "function".to_string(),
                            function_name: ev.tool_name.clone(),
                            function_arguments: args,
                        });
                    }
                }
            }
            "reasoning-delta" => self.reasoning.push_str(&ev.text),
            "finish" => {
                self.finish_reason = ev.finish_reason.clone();
                if let Some(u) = &ev.total_usage {
                    self.input_tokens = u.input_tokens;
                    self.output_tokens = u.output_tokens;
                    self.cache_read_tokens = u.cache_read_tokens;
                    self.cache_write_tokens = u.cache_write_tokens;
                    self.reasoning_tokens = u.reasoning_tokens;
                }
            }
            "error" => {
                if let Some(m) = &ev.error_message {
                    println!("[ERROR] Stream error: {m}");
                }
            }
            _ => {}
        }
    }
}

/// `readCCResult`：把整段上游响应读完并累积（非流式路径）。
///
/// ⚠ Go 的 `forEachLine(ccResp.Body, nil, fn)` 会把**整段体**逐行喂进来；
/// 这里也必须读完（只读首行会让 `finish` 事件的 usage 永远拿不到）。
/// Go 的 `forEachLine` 传 nil context ⇒ 不做取消检查，且读错误只记日志。
pub async fn read_cc_result(resp: reqwest::Response) -> CCResult {
    let mut res = CCResult::default();
    let body = resp.bytes().await.unwrap_or_default();
    for line in iterate_lines(&body) {
        res.feed_line(&line);
    }
    res
}

/// 按 `forEachLine` 的语义切开一段字节（`\n` 分行、trim、丢空行）。
pub fn iterate_lines(body: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    for part in text.split_inclusive('\n') {
        let line = trim_space(part);
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// `NormalizeFinishReason`。
pub fn normalize_finish_reason(reason: &str) -> &'static str {
    match reason {
        "tool_calls" | "tool-calls" => "tool_calls",
        "length" | "max_tokens" => "length",
        "content_filter" | "content-filter" => "content_filter",
        _ => "stop",
    }
}

/// 便利：把 CC 用量块转成 i64 元组。
pub fn usage_tuple(u: &Option<CCTotalUsage>) -> (i64, i64, i64, i64, i64) {
    match u {
        Some(u) => (
            u.input_tokens,
            u.output_tokens,
            u.cache_read_tokens,
            u.cache_write_tokens,
            u.reasoning_tokens,
        ),
        None => (0, 0, 0, 0, 0),
    }
}

/// 供测试：把一段文本按 `forEachLine` 的语义切成行（不涉及网络）。
pub fn split_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line_bytes in text.split_inclusive('\n') {
        let line = trim_space(line_bytes);
        if !line.is_empty() {
            out.push(line);
        }
    }
    // 末尾无换行的残行
    if !text.ends_with('\n') && !text.is_empty() {
        // split_inclusive 已把残行包含在内，无需重复处理
    }
    out
}

/// 把上游响应体读成一个 `Value`（供 `/v1/usage` 的 RawMessage 保真使用）。
pub fn raw_or_null(bytes: &[u8]) -> Value {
    match crate::godecode::parse_value(bytes) {
        Ok(v) => v,
        Err(_) => Value::Null,
    }
}
