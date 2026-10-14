//! `internal/comate/translate.go` 与 `handleChatStream` / `readFullAnswer` 的 Rust 等价物：
//! 消息扁平化、OpenAI chunk 拼装、上游 SSE 事件解析、以及 Go `bufio.Scanner` 的等价扫描器。
//!
//! 逐字节对齐的三个点：
//! * chunk / complete 的字段集与键序（Go 用 `map[string]any`，序列化时键排序——见 `gjson`）；
//! * `flattenMessages` 的角色前缀、content 数组取 `text` 子串、末尾 `TrimRight("\n")`；
//! * 扫描器的 1MB 行长上限与错误文案（`bufio.Scanner: token too long`，黄金基准锁住）。

use crate::gjson::{arr, i, obj, s, J};
use serde_json::{Map, Value};

/// `bufio.Scanner` 的 max token（`scanner.Buffer(make([]byte, 64*1024), 1024*1024)`）。
pub const MAX_TOKEN: usize = 1024 * 1024;

/// Go `strings.TrimSpace`（Unicode White_Space，与 Rust 的 `char::is_whitespace` 同义）。
pub fn trim_space(s: &str) -> &str {
    s.trim_matches(char::is_whitespace)
}

// ---------- flattenMessages ----------

/// 把 OpenAI `messages` 扁平化为 zulu 会话 query 文本（逐字复刻 `flattenMessages`）。
pub fn flatten_messages(messages: &[Value]) -> String {
    let mut sb = String::new();
    for raw in messages {
        let msg = match raw {
            Value::Object(m) => m,
            _ => continue, // Go: raw.(map[string]any) 失败即跳过（含 null 元素）
        };
        let role = match msg.get("role") {
            Some(Value::String(r)) => r.as_str(),
            _ => "",
        };
        match role {
            "system" => sb.push_str("[System]\n"),
            "user" => sb.push_str("[User]\n"),
            "assistant" => sb.push_str("[Assistant]\n"),
            other => {
                sb.push('[');
                sb.push_str(other);
                sb.push_str("]\n");
            }
        }
        match msg.get("content") {
            Some(Value::String(c)) => sb.push_str(c),
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Value::Object(p) = part {
                        if let Some(Value::String(t)) = p.get("text") {
                            if !t.is_empty() {
                                sb.push_str(t);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        sb.push('\n');
    }
    // Go: strings.TrimRight(sb.String(), "\n")
    while sb.ends_with('\n') {
        sb.pop();
    }
    sb
}

// ---------- OpenAI 响应拼装 ----------

/// `chunkJSON`：delta.content 增量块（finish 为空时 finish_reason=null）。
pub fn chunk_json(id: &str, model: &str, delta: &str, finish: &str, created: i64) -> String {
    let mut d: Vec<(String, J)> = Vec::new();
    if !delta.is_empty() {
        d.push(("content".to_string(), s(delta)));
    }
    obj_entry(id, model, created, d, finish)
}

/// `chunkReasoningJSON`：delta.reasoning_content 增量块（DeepSeek 方言）。
pub fn chunk_reasoning_json(id: &str, model: &str, thinking: &str, finish: &str, created: i64) -> String {
    let mut d: Vec<(String, J)> = Vec::new();
    if !thinking.is_empty() {
        d.push(("reasoning_content".to_string(), s(thinking)));
    }
    obj_entry(id, model, created, d, finish)
}

fn obj_entry(id: &str, model: &str, created: i64, delta: Vec<(String, J)>, finish: &str) -> String {
    let fr = if finish.is_empty() { J::Null } else { s(finish) };
    J::Obj(vec![
        (
            "choices".to_string(),
            arr(vec![J::Obj(vec![
                ("index".to_string(), i(0)),
                ("delta".to_string(), J::Obj(delta)),
                ("finish_reason".to_string(), fr),
            ])]),
        ),
        ("created".to_string(), i(created)),
        ("id".to_string(), s(id)),
        ("model".to_string(), s(model)),
        ("object".to_string(), s("chat.completion.chunk")),
    ])
    .to_go()
}

/// `completeJSON`：非流式完整响应（上游没有 usage 数据，三项填 0）。
pub fn complete_json(id: &str, model: &str, content: &str, created: i64) -> String {
    J::Obj(vec![
        (
            "choices".to_string(),
            arr(vec![J::Obj(vec![
                ("index".to_string(), i(0)),
                (
                    "message".to_string(),
                    obj(vec![("role", s("assistant")), ("content", s(content))]),
                ),
                ("finish_reason".to_string(), s("stop")),
            ])]),
        ),
        ("created".to_string(), i(created)),
        ("id".to_string(), s(id)),
        ("model".to_string(), s(model)),
        ("object".to_string(), s("chat.completion")),
        (
            "usage".to_string(),
            obj(vec![
                ("prompt_tokens", i(0)),
                ("completion_tokens", i(0)),
                ("total_tokens", i(0)),
            ]),
        ),
    ])
    .to_go()
}

// ---------- 上游 SSE 事件 ----------

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SseEvent {
    pub kind: String,
    pub status: String,
    pub content: String,
    pub chunks: Vec<SseChunk>,
    pub element: Option<SseElement>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SseChunk {
    pub kind: String,
    pub delta: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SseElement {
    pub typ: String,
    pub content: String,
}

/// Go 结构体字段匹配（先精确、再 ASCII 忽略大小写）。
fn pick<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    if let Some(v) = m.get(key) {
        return Some(v);
    }
    m.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// 取字符串字段：缺失/null → 零值；类型不符 → Err（Go 会整份 Unmarshal 失败）。
fn str_field(m: &Map<String, Value>, key: &str) -> Result<String, ()> {
    match pick(m, key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(v)) => Ok(v.clone()),
        Some(_) => Err(()),
    }
}

fn chunks_field(m: &Map<String, Value>) -> Result<Vec<SseChunk>, ()> {
    match pick(m, "chunks") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Object(cm) => out.push(SseChunk {
                        kind: str_field(cm, "kind")?,
                        delta: str_field(cm, "delta")?,
                    }),
                    Value::Null => out.push(SseChunk::default()),
                    _ => return Err(()),
                }
            }
            Ok(out)
        }
        Some(_) => Err(()),
    }
}

fn element_field(m: &Map<String, Value>) -> Result<Option<SseElement>, ()> {
    match pick(m, "element") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(em)) => Ok(Some(SseElement {
            typ: str_field(em, "type")?,
            content: str_field(em, "content")?,
        })),
        Some(_) => Err(()),
    }
}

/// 解析一条上游 SSE 事件 JSON；返回 `None` 表示 Go 的 `json.Unmarshal` 会报错（调用方 continue）。
pub fn parse_sse_event(json: &str) -> Option<SseEvent> {
    let v: Value = serde_json::from_str(json).ok()?;
    match v {
        Value::Null => Some(SseEvent::default()),
        Value::Object(m) => Some(SseEvent {
            kind: str_field(&m, "kind").ok()?,
            status: str_field(&m, "status").ok()?,
            content: str_field(&m, "content").ok()?,
            chunks: chunks_field(&m).ok()?,
            element: element_field(&m).ok()?,
        }),
        _ => None,
    }
}

/// `readFullAnswer` / `handleChatStream` 共用的「data: 行 → 事件」步骤。
pub fn event_from_line(line: &[u8]) -> Option<SseEvent> {
    // Go 里 line 是 string（可含非法 UTF-8）：json 解码时字符串内部非法字节会变 U+FFFD，
    // 结构外非法字节直接报错。`from_utf8_lossy` 两头都对得上。
    let s = String::from_utf8_lossy(line);
    let t = trim_space(&s);
    let rest = t.strip_prefix("data:")?;
    parse_sse_event(trim_space(rest))
}

// ---------- Go bufio.Scanner 等价扫描器 ----------

/// Go `bufio.Scanner` + `ScanLines` 的等价物（1MB 行长上限、错误文案逐字相同）。
///
/// 用法：`push(chunk)` 取出完整行 → 数据流结束时 `finish()` 取最后一行 → `err()` 看是否
/// 超长（Go 的 `ErrTooLong` = "bufio.Scanner: token too long"）。
pub struct LineScanner {
    pending: Vec<u8>,
    max: usize,
    err: Option<String>,
}

impl LineScanner {
    pub fn new(max: usize) -> Self {
        Self {
            pending: Vec::new(),
            max,
            err: None,
        }
    }

    /// 追加数据并切出完整行；超长时停止（后续数据不再切，与 Go 的 Scan 返回 false 一致）。
    ///
    /// 上限口径与 Go 相同：缓冲区涨到 `maxTokenSize` 还没见到换行 → `ErrTooLong`，
    /// 因此**行字节数 >= max 即超长**（含换行的那一字节不在判定内）。
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if self.err.is_some() {
            return out;
        }
        self.pending.extend_from_slice(data);
        loop {
            match self.pending.iter().position(|&b| b == b'\n') {
                Some(nl) => {
                    if nl >= self.max {
                        // 换行在 max 之外：这一行太长，Go 早已判 ErrTooLong
                        self.err = Some("bufio.Scanner: token too long".to_string());
                        return out;
                    }
                    let mut line: Vec<u8> = self.pending.drain(..=nl).collect();
                    line.pop(); // '\n'
                    if line.last() == Some(&b'\r') {
                        line.pop(); // ScanLines 的 dropCR
                    }
                    out.push(line);
                }
                None => break,
            }
        }
        if self.pending.len() >= self.max {
            self.err = Some("bufio.Scanner: token too long".to_string());
        }
        out
    }

    /// 数据流结束：返回最后一段（无换行结尾）——Go 的 ScanLines 在 EOF 上也会吐出残留 token。
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        if self.err.is_some() || self.pending.is_empty() {
            return None;
        }
        let mut line: Vec<u8> = std::mem::take(&mut self.pending);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(line)
    }

    pub fn err(&self) -> Option<&str> {
        self.err.as_deref()
    }
}

/// `readFullAnswer`（非流式路径）：读完整条上游 SSE，取 completed(status=ok) 的 content。
/// 返回 `(content, errMsg)`：errMsg 非空表示失败（文案与 Go 逐字一致）。
pub fn read_full_answer(body: &[u8]) -> (String, String) {
    let mut scanner = LineScanner::new(MAX_TOKEN);
    let mut lines = scanner.push(body);
    if let Some(tail) = scanner.finish() {
        lines.push(tail);
    }
    for line in &lines {
        let ev = match event_from_line(line) {
            Some(ev) => ev,
            None => continue,
        };
        if ev.kind != "completed" {
            continue;
        }
        if ev.status != "ok" {
            let msg = if !ev.content.is_empty() {
                ev.content.clone()
            } else {
                ev.status.clone()
            };
            return (String::new(), msg);
        }
        return (ev.content, String::new());
    }
    if let Some(e) = scanner.err() {
        return (String::new(), format!("上游 SSE 中断: {e}"));
    }
    (String::new(), "上游会话结束但未收到 completed（SSE 中断）".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_splits_like_go() {
        let mut sc = LineScanner::new(MAX_TOKEN);
        let lines = sc.push(b"a\r\nb\n\nc");
        assert_eq!(lines, vec![b"a".to_vec(), b"b".to_vec(), Vec::<u8>::new()]);
        assert_eq!(sc.finish(), Some(b"c".to_vec()));
        assert!(sc.err().is_none());

        // 末尾换行后没有残留 token
        let mut sc = LineScanner::new(MAX_TOKEN);
        assert_eq!(sc.push(b"x\n"), vec![b"x".to_vec()]);
        assert_eq!(sc.finish(), None);
    }

    #[test]
    fn scanner_flags_too_long_line() {
        // 1MB 无换行 → Go 的 ErrTooLong
        let mut sc = LineScanner::new(1024);
        let _ = sc.push(&vec![b'a'; 1024]);
        assert_eq!(sc.err(), Some("bufio.Scanner: token too long"));
        // 刚好差一个字节且有换行 → 不算超长
        let mut sc = LineScanner::new(1024);
        let mut data = vec![b'a'; 1023];
        data.push(b'\n');
        assert_eq!(sc.push(&data).len(), 1);
        assert!(sc.err().is_none());
        // 「行内就有换行但行本身超长」也必须判超长（`data: <1.8MB>\n\n` 就是这条）
        let mut sc = LineScanner::new(1024);
        let mut data = vec![b'a'; 2048];
        data.push(b'\n');
        let lines = sc.push(&data);
        assert!(lines.is_empty(), "超长行不该被当成正常行吐出来");
        assert_eq!(sc.err(), Some("bufio.Scanner: token too long"));
        // 超长行之前的正常行仍然要拿到（Go 的 Scan 在报错前已经吐过它们）
        let mut sc = LineScanner::new(8);
        let mut data = b"ok\n".to_vec();
        data.extend(vec![b'a'; 9]);
        data.push(b'\n');
        assert_eq!(sc.push(&data), vec![b"ok".to_vec()]);
        assert_eq!(sc.err(), Some("bufio.Scanner: token too long"));
    }

    #[test]
    fn flatten_matches_golden_examples() {
        let cases: Vec<(&str, &str)> = vec![
            ("[]", ""),
            ("[{\"role\":\"tool\",\"content\":\"T\"}]", "[tool]\nT"),
            ("[{\"role\":\"user\",\"content\":\"no newline at end\"}]", "[User]\nno newline at end"),
            ("[{\"role\":\"user\",\"content\":\"a\\n\\nb\\n\"}]", "[User]\na\n\nb"),
            ("[{\"role\":\"user\",\"content\":null}]", "[User]"),
            ("[{\"role\":\"user\",\"content\":[{\"text\":5},{\"type\":\"text\"}]}]", "[User]"),
            ("[{}]", "[]"),
        ];
        for (input, want) in cases {
            let msgs: Vec<Value> = serde_json::from_str(input).unwrap();
            assert_eq!(flatten_messages(&msgs), want, "input={input}");
        }
    }
}
