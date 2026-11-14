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
    /// 旧协议的事件名（`kind`）：`delta-batch` / `element-add` / `completed` …
    pub kind: String,
    /// 新协议（zulu 1.8.1）的事件名（`type`）：`task_done` / `task_failed` …
    pub typ: String,
    pub status: String,
    pub content: String,
    pub chunks: Vec<SseChunk>,
    pub element: Option<SseElement>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SseChunk {
    pub kind: String,
    pub delta: String,
    /// `element-add` 里的元素快照。
    pub element: Option<SseElement>,
    /// `element-patch` 的目标元素 id。
    pub eid: String,
    /// `element-patch` 的元素快照（`content` 是**累计全文**，不是增量）。
    pub patch: Option<SseElement>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SseElement {
    pub typ: String,
    /// 元素 id：同一元素的多次 patch 靠它归并。
    pub id: String,
    pub content: String,
    /// `done:true` 表示该元素已写完（目前只用于诊断，不参与拼接）。
    pub done: bool,
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

/// 取布尔字段：缺失/null → false；类型不符 → Err。
fn bool_field(m: &Map<String, Value>, key: &str) -> Result<bool, ()> {
    match pick(m, key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(v)) => Ok(*v),
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
                        element: object_field(cm, "element")?,
                        eid: str_field(cm, "eid")?,
                        patch: object_field(cm, "patch")?,
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

/// 取 `element` / `patch` 这类元素对象：缺失/null → None；对象 → Some；其它 → Err。
fn object_field(m: &Map<String, Value>, key: &str) -> Result<Option<SseElement>, ()> {
    match pick(m, key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(em)) => Ok(Some(SseElement {
            typ: str_field(em, "type")?,
            id: str_field(em, "id")?,
            content: str_field(em, "content")?,
            done: bool_field(em, "done")?,
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
            typ: str_field(&m, "type").ok()?,
            status: str_field(&m, "status").ok()?,
            content: str_field(&m, "content").ok()?,
            chunks: chunks_field(&m).ok()?,
            element: object_field(&m, "element").ok()?,
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

/// 云直连（`cloud.rs`）的「data: 行 → 帧」步骤：同一套行清洗，换成云帧解析。
///
/// SSE 注释行（`:heartbeat`）与 `data:` 之外的行自然返回 `None`。
pub fn event_from_line_like(line: &[u8]) -> Option<crate::cloud::CloudFrame> {
    let s = String::from_utf8_lossy(line);
    let t = trim_space(&s);
    let rest = t.strip_prefix("data:")?;
    crate::cloud::parse_frame(trim_space(rest))
}

// ---------- 事件状态机（新旧两套协议共用） ----------

/// 一次增量下发：`reasoning=false` 进 `delta.content`，`true` 进 `delta.reasoning_content`。
#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub reasoning: bool,
    pub text: String,
}

/// 上游事件累积器。
///
/// 两套协议同时吃：
/// * **旧**（Go 时代 zulu）：`delta-batch` 里的 `text-delta` / `thinking-delta` 是**增量**；
///   结束帧是 `{"kind":"completed","status":"ok","content":<全文>}`（全文以它为准）。
/// * **新**（zulu 1.8.1，2026-11-15 实测）：正文/思考都走**元素**——`element-add` 给首段，
///   之后 `element-patch` 给的是**累计全文**（`content` 不是增量！）；结束帧是
///   `{"type":"task_done","status":"completed"}`。失败时正文位置会先出现一个 `EXCEPTION`
///   元素，`content` 是人话错误（例如「您当前会话执行遇到了异常，请检测网络后重试！」）。
#[derive(Debug, Default)]
pub struct SseAcc {
    /// 正文全文（非流式路径取它）。
    pub text: String,
    /// 思考全文。
    pub reasoning: String,
    /// 错误文案（`EXCEPTION` 元素 / 旧 completed 的 content / 失败状态）。
    pub error: String,
    /// 收到成功结束帧。
    pub finished: bool,
    /// 收到失败结束帧。
    pub failed: bool,
    /// 每个元素 id 已下发的完整内容（元素帧给累计全文，靠它算增量）。
    seen: std::collections::HashMap<String, String>,
}

impl SseAcc {
    /// 吃一条事件，返回需要下发给客户端的增量（正文/思考）。
    pub fn ingest(&mut self, ev: &SseEvent) -> Vec<Delta> {
        let mut out = Vec::new();

        // --- 结束帧：新协议 type=task_done ---
        if ev.typ == "task_done" {
            if ev.status == "completed" {
                self.finished = true;
            } else {
                self.failed = true;
                if self.error.is_empty() {
                    self.error = if ev.content.is_empty() {
                        ev.status.clone()
                    } else {
                        ev.content.clone()
                    };
                }
            }
            return out;
        }

        // --- 结束帧：旧协议 kind=completed ---
        if ev.kind == "completed" {
            if ev.status == "ok" {
                self.finished = true;
                // 旧协议以 completed.content 为权威全文（黄金基准 read-ok / read-first-wins 锁死）。
                if !ev.content.is_empty() {
                    self.text = ev.content.clone();
                }
            } else {
                self.failed = true;
                self.error = if ev.content.is_empty() {
                    ev.status.clone()
                } else {
                    ev.content.clone()
                };
            }
            return out;
        }

        // --- 顶层元素帧（旧协议形态，兜底兼容） ---
        // 旧协议里顶层 `element-add` 的 REASON 会被 Go 丢掉（黄金基准 `stream-element-add`
        // 锁着这条），所以这里只接 TEXT；新协议的 REASON 走 delta-batch 里的元素帧。
        if ev.kind == "element-add" || ev.kind == "element-patch" {
            if let Some(el) = &ev.element {
                if let Some(d) = self.take_element(el, false) {
                    out.push(d);
                }
            }
        }

        // --- delta-batch ---
        for c in &ev.chunks {
            match c.kind.as_str() {
                "text-delta" => {
                    if !c.delta.is_empty() {
                        self.text.push_str(&c.delta);
                        out.push(Delta {
                            reasoning: false,
                            text: c.delta.clone(),
                        });
                    }
                }
                "thinking-delta" => {
                    if !c.delta.is_empty() {
                        self.reasoning.push_str(&c.delta);
                        out.push(Delta {
                            reasoning: true,
                            text: c.delta.clone(),
                        });
                    }
                }
                "element-add" => {
                    if let Some(el) = &c.element {
                        if let Some(d) = self.take_element(el, true) {
                            out.push(d);
                        }
                    }
                }
                "element-patch" => {
                    if let Some(p) = &c.patch {
                        if let Some(d) = self.take_element(p, true) {
                            out.push(d);
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// 元素帧 → 增量。`element-add` 与 `element-patch` 同一条路径：
    /// 内容按「累计全文」处理，只下发比上次多出来的尾巴。
    ///
    /// `allow_reasoning`：新协议（delta-batch 内的元素帧）才把 REASON 透成
    /// `reasoning_content`；旧协议顶层元素帧保持 Go 的「丢 REASON」行为。
    fn take_element(&mut self, el: &SseElement, allow_reasoning: bool) -> Option<Delta> {
        let reasoning = match el.typ.as_str() {
            "EXCEPTION" => {
                if !el.content.is_empty() {
                    self.error = el.content.clone();
                }
                return None;
            }
            "TEXT" => false,
            "REASON" => {
                if !allow_reasoning {
                    return None;
                }
                true
            }
            _ => return None,
        };
        let key = if el.id.is_empty() {
            format!("~{}", el.typ)
        } else {
            el.id.clone()
        };
        let prev = self.seen.get(&key).cloned().unwrap_or_default();
        let next = if prev.is_empty() {
            el.content.clone()
        } else if let Some(rest) = el.content.strip_prefix(prev.as_str()) {
            rest.to_string()
        } else if el.content == prev {
            String::new()
        } else {
            // 非前缀（上游改写了整段）：重发会重复，宁可不发——已下发的正文保持原样。
            String::new()
        };
        if el.content.len() > prev.len() {
            self.seen.insert(key, el.content.clone());
        }
        if next.is_empty() {
            return None;
        }
        if reasoning {
            self.reasoning.push_str(&next);
        } else {
            self.text.push_str(&next);
        }
        Some(Delta {
            reasoning,
            text: next,
        })
    }
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

/// `readFullAnswer`（非流式路径）：读完整条上游 SSE，取最终的正文/错误。
///
/// 旧协议取 `completed(status=ok).content`；新协议（zulu 1.8.1）取元素累积正文 +
/// `task_done(status=completed)`。返回 `(content, errMsg)`：errMsg 非空表示失败
/// （文案与 Go 侧逐字一致；新协议多一种来源：`EXCEPTION` 元素的人话错误）。
pub fn read_full_answer(body: &[u8]) -> (String, String) {
    let mut scanner = LineScanner::new(MAX_TOKEN);
    let mut lines = scanner.push(body);
    if let Some(tail) = scanner.finish() {
        lines.push(tail);
    }
    let mut acc = SseAcc::default();
    for line in &lines {
        let ev = match event_from_line(line) {
            Some(ev) => ev,
            None => continue,
        };
        let _ = acc.ingest(&ev);
        if acc.finished || acc.failed {
            break;
        }
    }
    if acc.failed {
        return (String::new(), acc.error);
    }
    if acc.finished {
        return (acc.text, String::new());
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

    // ---------- zulu 1.8.1 新协议（2026-11-15 真机抓的帧形态） ----------

    /// 新协议：元素帧（`element-add` + `element-patch` 累计全文）+ `type=task_done`。
    #[test]
    fn new_protocol_element_frames_accumulate() {
        let body = concat!(
            "data: {\"kind\":\"delta-batch\",\"cid\":\"remote-1\",\"chunks\":[{\"kind\":\"element-add\",\"mid\":\"m\",\"element\":{\"type\":\"REASON\",\"id\":\"r1\",\"content\":\"思考一\",\"done\":false}}]}\n\n",
            "data: {\"kind\":\"delta-batch\",\"cid\":\"remote-1\",\"chunks\":[{\"kind\":\"element-patch\",\"mid\":\"m\",\"eid\":\"r1\",\"patch\":{\"type\":\"REASON\",\"id\":\"r1\",\"content\":\"思考一。继续\",\"done\":true}}]}\n\n",
            "data: {\"kind\":\"delta-batch\",\"cid\":\"remote-1\",\"chunks\":[{\"kind\":\"element-add\",\"mid\":\"m\",\"element\":{\"type\":\"TEXT\",\"id\":\"t1\",\"content\":\"你好\",\"done\":false}}]}\n\n",
            "data: {\"kind\":\"delta-batch\",\"cid\":\"remote-1\",\"chunks\":[{\"kind\":\"element-patch\",\"mid\":\"m\",\"eid\":\"t1\",\"patch\":{\"type\":\"TEXT\",\"id\":\"t1\",\"content\":\"你好，世界。\",\"done\":true}}]}\n\n",
            "data: {\"type\":\"task_done\",\"status\":\"completed\",\"conversationId\":\"remote-1\"}\n\n",
        );
        let mut acc = SseAcc::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        for line in body.lines() {
            let Some(ev) = event_from_line(line.as_bytes()) else {
                continue;
            };
            for d in acc.ingest(&ev) {
                if d.reasoning {
                    reasoning.push_str(&d.text);
                } else {
                    text.push_str(&d.text);
                }
            }
        }
        assert_eq!(text, "你好，世界。");
        assert_eq!(reasoning, "思考一。继续");
        assert!(acc.finished && !acc.failed);
        // 非流式同一份字节也要拿到同样的正文
        let (content, err) = read_full_answer(body.as_bytes());
        assert_eq!((content.as_str(), err.as_str()), ("你好，世界。", ""));
    }

    /// 新协议失败：先来 `EXCEPTION` 元素，再来 `task_done(failed)`——错误要取人话文案。
    #[test]
    fn new_protocol_failure_surfaces_exception() {
        let body = concat!(
            "data: {\"kind\":\"delta-batch\",\"cid\":\"remote-1\",\"chunks\":[{\"kind\":\"element-add\",\"mid\":\"m\",\"element\":{\"type\":\"EXCEPTION\",\"id\":\"e1\",\"content\":\"您当前会话执行遇到了异常，请检测网络后重试！\"}}]}\n\n",
            "data: {\"type\":\"task_done\",\"status\":\"failed\",\"conversationId\":\"remote-1\"}\n\n",
        );
        let (content, err) = read_full_answer(body.as_bytes());
        assert_eq!(content, "");
        assert_eq!(err, "您当前会话执行遇到了异常，请检测网络后重试！");
    }

    /// 同一元素重复 patch（内容没变）不能重复下发。
    #[test]
    fn repeated_patch_emits_nothing() {
        let body = concat!(
            "data: {\"kind\":\"delta-batch\",\"chunks\":[{\"kind\":\"element-add\",\"element\":{\"type\":\"TEXT\",\"id\":\"t1\",\"content\":\"abc\",\"done\":false}}]}\n\n",
            "data: {\"kind\":\"delta-batch\",\"chunks\":[{\"kind\":\"element-patch\",\"eid\":\"t1\",\"patch\":{\"type\":\"TEXT\",\"id\":\"t1\",\"content\":\"abc\",\"done\":false}}]}\n\n",
            "data: {\"kind\":\"delta-batch\",\"chunks\":[{\"kind\":\"element-patch\",\"eid\":\"t1\",\"patch\":{\"type\":\"TEXT\",\"id\":\"t1\",\"content\":\"abcd\",\"done\":true}}]}\n\n",
            "data: {\"type\":\"task_done\",\"status\":\"completed\"}\n\n",
        );
        let mut acc = SseAcc::default();
        let mut emitted = String::new();
        for line in body.lines() {
            if let Some(ev) = event_from_line(line.as_bytes()) {
                for d in acc.ingest(&ev) {
                    emitted.push_str(&d.text);
                }
            }
        }
        assert_eq!(emitted, "abcd");
    }

    /// 新协议 `task_done` 之前没有 completed 帧，也要算「正常收尾」——
    /// 这正是 2026-11-15 之前代理报「上游会话结束但未收到 completed」的原因。
    #[test]
    fn task_done_completes_without_completed_frame() {
        let body = concat!(
            "data: {\"kind\":\"delta-batch\",\"chunks\":[{\"kind\":\"element-add\",\"element\":{\"type\":\"TEXT\",\"id\":\"t1\",\"content\":\"ok\",\"done\":true}}]}\n\n",
            "data: {\"type\":\"task_done\",\"status\":\"completed\"}\n\n",
        );
        let (content, err) = read_full_answer(body.as_bytes());
        assert_eq!((content.as_str(), err.as_str()), ("ok", ""));
    }
}
