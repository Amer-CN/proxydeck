//! `internal/api` 的 Rust 对应物：三条协议的线上类型，以及**逐字节对齐 Go `encoding/json`**
//! 的写出方法。
//!
//! 字段序 = Go 结构体的声明序（Go 对 struct 不排序）；`omitempty` 语义逐条对应
//! （`*T == nil` / `s == ""` / `len(slice) == 0`）。所有 JSON 都经 [`crate::gjson`] 写出，
//! 不走 serde derive——浮点必须走 Go 的规则。

use serde_json::Value;

use crate::gjson::{push_go_string, to_go, GoObj};

// ===========================================================================
// OpenAI chat（入站请求）
// ===========================================================================

/// `api.OpenAIMessage`。`content` 是 `any`（字符串 / 数组 / null），本 crate 保留原值。
#[derive(Debug, Clone, Default)]
pub struct OpenAIMessage {
    pub role: String,
    /// `json:"content,omitempty"`：Go 的 interface 判空是「interface 本身为 nil」。
    /// `None` = 未设置（不写出）；`Some(Value::Null)` = 显式 null（写出 `null`）。
    pub content: Option<Value>,
    pub name: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub reasoning_content: String,
}

/// `api.ToolCall`（无 omitempty，字段全写出）。
#[derive(Debug, Clone, Default)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function_name: String,
    pub function_arguments: String,
}

impl ToolCall {
    fn to_go(&self) -> String {
        let mut fo = GoObj::new();
        fo.field_str("name", &self.function_name)
            .field_str("arguments", &self.function_arguments);
        let mut o = GoObj::new();
        o.field_str("id", &self.id)
            .field_str("type", &self.r#type)
            .raw("function", &fo.finish());
        o.finish()
    }
}

/// `api.OpenAIChatRequest`：入站 chat 请求（只留 BuildRequest 真正读的字段）。
///
/// 其余字段 Go 也解进结构体却不参与 `BuildRequest`（`stream_options` / `response_format` /
/// `presence_penalty`…），本 crate 用「取值树 + 类型化读取」等价实现
/// （见 [`crate::chat::parse_chat_request`]）。
#[derive(Debug, Clone, Default)]
pub struct OpenAIChatRequest {
    pub model: String,
    pub messages: Vec<OpenAIMessage>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub max_completion_tokens: Option<i64>,
    pub stream: bool,
    pub tools: Vec<Value>,
}

// ===========================================================================
// CommandCode（出站请求体）
// ===========================================================================

/// `api.CCContentPart`。指针字段用 `Option`，写出口径与 Go 的 `omitempty` 一致
/// （nil 指针不写；指向空串的指针**写出**空串）。
#[derive(Debug, Clone, Default)]
pub struct CCContentPart {
    pub r#type: String,
    pub text: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub input: Option<Value>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub output: Option<CCToolOutput>,
    pub tool_use_id: Option<String>,
    pub content: Option<Value>,
}

impl CCContentPart {
    pub fn text(t: impl Into<String>) -> CCContentPart {
        CCContentPart {
            r#type: "text".to_string(),
            text: Some(t.into()),
            ..Default::default()
        }
    }

    pub fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("type", &self.r#type);
        o.opt_str("text", self.text.as_deref().unwrap_or(""));
        o.opt_str("id", self.id.as_deref().unwrap_or(""));
        o.opt_str("name", self.name.as_deref().unwrap_or(""));
        if let Some(v) = &self.input {
            o.field("input", v);
        }
        o.opt_str("toolCallId", self.tool_call_id.as_deref().unwrap_or(""));
        o.opt_str("toolName", self.tool_name.as_deref().unwrap_or(""));
        if let Some(out) = &self.output {
            o.raw("output", &out.to_go());
        }
        o.opt_str("tool_use_id", self.tool_use_id.as_deref().unwrap_or(""));
        if let Some(v) = &self.content {
            o.field("content", v);
        }
        o.finish()
    }
}

/// `api.CCToolOutput`。
#[derive(Debug, Clone, Default)]
pub struct CCToolOutput {
    pub r#type: String,
    pub value: String,
}

impl CCToolOutput {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("type", &self.r#type)
            .field_str("value", &self.value);
        o.finish()
    }
}

/// `api.CCMessage`（`content` 无 omitempty，空切片也写出 `[]`）。
#[derive(Debug, Clone, Default)]
pub struct CCMessage {
    pub role: String,
    pub content: Vec<CCContentPart>,
}

fn parts_to_go(parts: &[CCContentPart]) -> String {
    let mut s = String::from("[");
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&p.to_go());
    }
    s.push(']');
    s
}

impl CCMessage {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("role", &self.role)
            .raw("content", &parts_to_go(&self.content));
        o.finish()
    }
}

/// `api.CCChatParams`。
#[derive(Debug, Clone, Default)]
pub struct CCChatParams {
    pub model: String,
    /// `messages []api.CCMessage` —— **nil 与空切片在 Go 里编出不同字节**：
    /// `ConvertMessages` 全程 append，返回 nil 切片时 `json.Marshal` 给 `null`（不是 `[]`）。
    /// 黄金实测 `responses-convert #6`（`input:[1,2]` → 每个非对象 item 都被跳过 ⇒ 零条消息）
    /// 出站就是 `"messages":null`；`None` 表示「nil」，`Some(空)` 表示「非 nil 空切片」。
    pub messages: Option<Vec<CCMessage>>,
    pub tools: Vec<Value>,
    pub system: String,
    pub max_tokens: i64,
    pub temperature: f64,
    pub stream: bool,
}

fn values_to_go(items: &[Value]) -> String {
    let mut s = String::from("[");
    for (i, t) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&to_go(t));
    }
    s.push(']');
    s
}

fn strings_to_go(items: &[String]) -> String {
    let mut s = String::from("[");
    for (i, x) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        push_go_string(&mut s, x);
    }
    s.push(']');
    s
}

impl CCChatParams {
    fn to_go(&self) -> String {
        let msgs = match &self.messages {
            // nil 切片 ⇒ `null`（Go 的 `json.Marshal` 对 nil slice 就是 null）
            None => "null".to_string(),
            Some(list) => {
                let mut s = String::from("[");
                for (i, m) in list.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    s.push_str(&m.to_go());
                }
                s.push(']');
                s
            }
        };
        let mut o = GoObj::new();
        o.field_str("model", &self.model)
            .raw("messages", &msgs)
            // `tools []any` 无 omitempty：BuildRequest 里它始终来自 ConvertTools
            // （非 nil 空切片），故空 → `[]`（Go 的 nil 切片才编成 `null`）
            .raw("tools", &values_to_go(&self.tools))
            .field_str("system", &self.system)
            .field_i64("max_tokens", self.max_tokens)
            .field_f64("temperature", self.temperature)
            .field("stream", &Value::Bool(self.stream));
        o.finish()
    }
}

/// `api.CCConfig`。
#[derive(Debug, Clone, Default)]
pub struct CCConfig {
    pub working_dir: String,
    pub date: String,
    pub environment: String,
    pub structure: Vec<String>,
    pub is_git_repo: bool,
    pub current_branch: String,
    pub main_branch: String,
    pub git_status: String,
    pub recent_commits: Vec<String>,
}

impl CCConfig {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("workingDir", &self.working_dir)
            .field_str("date", &self.date)
            .field_str("environment", &self.environment)
            .raw("structure", &strings_to_go(&self.structure))
            .field("isGitRepo", &Value::Bool(self.is_git_repo))
            .field_str("currentBranch", &self.current_branch)
            .field_str("mainBranch", &self.main_branch)
            .field_str("gitStatus", &self.git_status)
            .raw("recentCommits", &strings_to_go(&self.recent_commits));
        o.finish()
    }
}

/// `api.CCRequestBody`。
#[derive(Debug, Clone, Default)]
pub struct CCRequestBody {
    pub config: CCConfig,
    pub memory: String,
    pub taste: String,
    pub skills: String,
    pub params: CCChatParams,
    pub thread_id: String,
}

impl CCRequestBody {
    pub fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.raw("config", &self.config.to_go())
            .field_str("memory", &self.memory)
            .field_str("taste", &self.taste)
            .field_str("skills", &self.skills)
            .raw("params", &self.params.to_go())
            .field_str("threadId", &self.thread_id);
        o.finish()
    }

    pub fn to_go_bytes(&self) -> Vec<u8> {
        self.to_go().into_bytes()
    }
}

/// `api.CCStreamEvent.TotalUsage`。
#[derive(Debug, Clone, Default)]
pub struct CCTotalUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
}

/// `api.CCStreamEvent`（解析用；字段全可缺省）。
#[derive(Debug, Clone, Default)]
pub struct CCStreamEvent {
    pub r#type: String,
    pub text: String,
    pub id: String,
    pub delta: String,
    pub input: Option<Value>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub finish_reason: String,
    pub error_message: Option<String>,
    pub total_usage: Option<CCTotalUsage>,
}

impl CCStreamEvent {
    /// 按 Go 的结构体解码语义取字段（`json.Unmarshal(line, &api.CCStreamEvent)`）。
    ///
    /// 返回 `None` 表示**这一行解码失败**——Go 的 `process`（流式）与 `readCCResult`（非流式）
    /// 都把解码错误静默吞掉（`return nil` / 跳过该行），所以整行事件被丢弃。
    ///
    /// 关键差异逐条对齐：
    /// * `input map[string]any`：非对象（如 `"input": 5`）在 Go 里报错 → 丢行；
    /// * `totalUsage` 缺失 → nil；字段缺失 → 0（Go 零值）；
    /// * 顶层必须是对象。
    pub fn from_line(line: &str) -> Option<CCStreamEvent> {
        let v: Value = crate::godecode::parse_value(line.as_bytes()).ok()?;
        let m = v.as_object()?;
        let get_str = |k: &str| -> String {
            match m.get(k) {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            }
        };
        let input = match m.get("input") {
            None | Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(Value::Object(o.clone())),
            Some(_) => return None,
        };
        let error_message = match m.get("error") {
            None | Some(Value::Null) => None,
            Some(Value::Object(e)) => Some(match e.get("message") {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            }),
            Some(_) => return None,
        };
        let total_usage = match m.get("totalUsage") {
            None | Some(Value::Null) => None,
            Some(Value::Object(u)) => {
                // ⚠ 数字经本 crate 的解码器一律落成 f64（与 Go 的 `map[string]any` 同），
                // 不能用 `as_i64()`（那对 f64 形态一律 None）——要用整数值判定。
                let num = |k: &str| -> i64 { u.get(k).and_then(crate::godecode::go_int).unwrap_or(0) };
                Some(CCTotalUsage {
                    input_tokens: num("inputTokens"),
                    output_tokens: num("outputTokens"),
                    cache_read_tokens: num("cacheReadTokens"),
                    cache_write_tokens: num("cacheWriteTokens"),
                    reasoning_tokens: num("reasoningTokens"),
                })
            }
            Some(_) => return None,
        };
        Some(CCStreamEvent {
            r#type: get_str("type"),
            text: get_str("text"),
            id: get_str("id"),
            delta: get_str("delta"),
            input,
            tool_call_id: get_str("toolCallId"),
            tool_name: get_str("toolName"),
            finish_reason: get_str("finishReason"),
            error_message,
            total_usage,
        })
    }
}

// ===========================================================================
// OpenAI chat（出站响应）
// ===========================================================================

/// `api.OpenAIDelta`。
#[derive(Debug, Clone, Default)]
pub struct OpenAIDelta {
    pub role: String,
    pub content: String,
    pub tool_calls: Vec<OpenAIDeltaToolCall>,
    pub refusal: String,
    pub reasoning_content: String,
}

/// `api.OpenAIDeltaToolCall`。
#[derive(Debug, Clone, Default)]
pub struct OpenAIDeltaToolCall {
    pub index: i64,
    pub id: String,
    pub r#type: String,
    pub function_name: String,
    pub function_arguments: String,
    /// `function` 指针是否为 nil（nil 时 `function` 键整个不写）
    pub function_present: bool,
}

impl OpenAIDeltaToolCall {
    pub fn with_function(index: i64, name: &str, arguments: &str) -> OpenAIDeltaToolCall {
        OpenAIDeltaToolCall {
            index,
            function_name: name.to_string(),
            function_arguments: arguments.to_string(),
            function_present: true,
            ..Default::default()
        }
    }

    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_i64("index", self.index)
            .opt_str("id", &self.id)
            .opt_str("type", &self.r#type);
        if self.function_present {
            let mut fo = GoObj::new();
            fo.opt_str("name", &self.function_name)
                .opt_str("arguments", &self.function_arguments);
            o.raw("function", &fo.finish());
        }
        o.finish()
    }
}

impl OpenAIDelta {
    pub fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.opt_str("role", &self.role)
            .opt_str("content", &self.content);
        if !self.tool_calls.is_empty() {
            let mut s = String::from("[");
            for (i, t) in self.tool_calls.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&t.to_go());
            }
            s.push(']');
            o.raw("tool_calls", &s);
        }
        o.opt_str("refusal", &self.refusal)
            .opt_str("reasoning_content", &self.reasoning_content);
        o.finish()
    }
}

/// `api.OpenAIUsage`。`completion_tokens_details` 为 nil 时不写。
#[derive(Debug, Clone, Default)]
pub struct OpenAIUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub reasoning_tokens: Option<i64>,
}

impl OpenAIUsage {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_i64("prompt_tokens", self.prompt_tokens)
            .field_i64("completion_tokens", self.completion_tokens)
            .field_i64("total_tokens", self.total_tokens);
        if let Some(rt) = self.reasoning_tokens {
            let mut d = GoObj::new();
            d.field_i64("reasoning_tokens", rt);
            o.raw("completion_tokens_details", &d.finish());
        }
        o.finish()
    }
}

/// `api.OpenAIMessage` 的**出站**写出（非流式响应里的 message）。
pub struct OutMessage {
    pub role: String,
    /// `None` → 不写 `content`（Go 的 nil interface + omitempty）；`Some(s)` → 写字符串
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub reasoning_content: String,
}

impl OutMessage {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("role", &self.role);
        if let Some(c) = &self.content {
            o.field_str("content", c);
        }
        if !self.tool_calls.is_empty() {
            let mut s = String::from("[");
            for (i, t) in self.tool_calls.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&t.to_go());
            }
            s.push(']');
            o.raw("tool_calls", &s);
        }
        o.opt_str("reasoning_content", &self.reasoning_content);
        o.finish()
    }
}

/// `api.OpenAIChoice`。`message` 与 `delta` 恰有其一。
pub struct OpenAIChoice {
    pub index: i64,
    pub message: Option<OutMessage>,
    pub delta: Option<OpenAIDelta>,
    pub finish_reason: Option<String>,
}

impl OpenAIChoice {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_i64("index", self.index);
        if let Some(m) = &self.message {
            o.raw("message", &m.to_go());
        }
        if let Some(d) = &self.delta {
            o.raw("delta", &d.to_go());
        }
        if let Some(fr) = &self.finish_reason {
            o.field_str("finish_reason", fr);
        }
        o.finish()
    }
}

/// `api.OpenAIChatResponse`。
pub struct OpenAIChatResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<OpenAIChoice>,
    pub usage: Option<OpenAIUsage>,
}

impl OpenAIChatResponse {
    pub fn to_go(&self) -> String {
        let mut s = String::from("[");
        for (i, c) in self.choices.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&c.to_go());
        }
        s.push(']');
        let mut o = GoObj::new();
        o.field_str("id", &self.id)
            .field_str("object", &self.object)
            .field_i64("created", self.created)
            .field_str("model", &self.model)
            .raw("choices", &s);
        if let Some(u) = &self.usage {
            o.raw("usage", &u.to_go());
        }
        o.finish()
    }
}

/// `api.OpenAIErrorResponse` / `api.OpenAIError`：`param` / `code` **无** omitempty，
/// 值恒为 nil → 写出 `null`。
pub struct OpenAIErrorResponse {
    pub message: String,
    pub r#type: String,
}

impl OpenAIErrorResponse {
    pub fn to_go(&self) -> String {
        let mut e = GoObj::new();
        e.field_str("message", &self.message)
            .field_str("type", &self.r#type)
            .field("param", &Value::Null)
            .field("code", &Value::Null);
        let mut o = GoObj::new();
        o.raw("error", &e.finish());
        o.finish()
    }
}

/// `api.OpenAIModel`。
pub struct OpenAIModel {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
}

impl OpenAIModel {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("id", &self.id)
            .field_str("object", &self.object)
            .field_i64("created", self.created)
            .field_str("owned_by", &self.owned_by);
        o.finish()
    }
}

/// `api.OpenAIModelList`。
pub struct OpenAIModelList {
    pub object: String,
    pub data: Vec<OpenAIModel>,
}

impl OpenAIModelList {
    pub fn to_go(&self) -> String {
        let mut s = String::from("[");
        for (i, m) in self.data.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&m.to_go());
        }
        s.push(']');
        let mut o = GoObj::new();
        o.field_str("object", &self.object).raw("data", &s);
        o.finish()
    }
}

// ===========================================================================
// Anthropic
// ===========================================================================

/// `api.AnthropicContentBlock`（`input` 为 any + omitempty：nil 不写）。
#[derive(Debug, Clone, Default)]
pub struct AnthropicContentBlock {
    pub r#type: String,
    pub text: String,
    pub id: String,
    pub name: String,
    pub input: Option<Value>,
}

impl AnthropicContentBlock {
    pub fn text(t: &str) -> AnthropicContentBlock {
        AnthropicContentBlock {
            r#type: "text".to_string(),
            text: t.to_string(),
            ..Default::default()
        }
    }

    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("type", &self.r#type)
            .opt_str("text", &self.text)
            .opt_str("id", &self.id)
            .opt_str("name", &self.name);
        if let Some(v) = &self.input {
            o.field("input", v);
        }
        o.finish()
    }
}

/// `api.AnthropicUsage`。
pub struct AnthropicUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

impl AnthropicUsage {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_i64("input_tokens", self.input_tokens)
            .field_i64("output_tokens", self.output_tokens);
        o.finish()
    }
}

/// `api.AnthropicMessageResponse`（`stop_reason` / `stop_sequence` 是 `*string` 且**无**
/// omitempty：nil 写出 `null`）。
pub struct AnthropicMessageResponse {
    pub id: String,
    pub r#type: String,
    pub role: String,
    pub model: String,
    pub content: Vec<AnthropicContentBlock>,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: AnthropicUsage,
}

fn opt_string_value(v: &Option<String>) -> Value {
    match v {
        Some(s) => Value::String(s.clone()),
        None => Value::Null,
    }
}

impl AnthropicMessageResponse {
    pub fn to_go(&self) -> String {
        let mut c = String::from("[");
        for (i, b) in self.content.iter().enumerate() {
            if i > 0 {
                c.push(',');
            }
            c.push_str(&b.to_go());
        }
        c.push(']');
        let mut o = GoObj::new();
        o.field_str("id", &self.id)
            .field_str("type", &self.r#type)
            .field_str("role", &self.role)
            .field_str("model", &self.model)
            .raw("content", &c)
            .field("stop_reason", &opt_string_value(&self.stop_reason))
            .field("stop_sequence", &opt_string_value(&self.stop_sequence))
            .raw("usage", &self.usage.to_go());
        o.finish()
    }
}

/// `api.AnthropicErrorResponse`。
pub struct AnthropicErrorResponse {
    pub r#type: String,
    pub error_type: String,
    pub message: String,
}

impl AnthropicErrorResponse {
    pub fn to_go(&self) -> String {
        let mut e = GoObj::new();
        e.field_str("type", &self.error_type)
            .field_str("message", &self.message);
        let mut o = GoObj::new();
        o.field_str("type", &self.r#type).raw("error", &e.finish());
        o.finish()
    }
}

/// `anthropicStreamEvent`：`delta` 是 `any`（omitempty），`usage` 是指针（omitempty）。
pub struct AnthropicStreamEvent<'a> {
    pub r#type: &'a str,
    pub message: Option<&'a AnthropicMessageResponse>,
    pub index: Option<i64>,
    pub content_block: Option<&'a AnthropicContentBlock>,
    pub delta: Option<Value>,
    pub usage: Option<&'a AnthropicUsage>,
}

impl AnthropicStreamEvent<'_> {
    pub fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("type", self.r#type);
        if let Some(m) = self.message {
            o.raw("message", &m.to_go());
        }
        if let Some(i) = self.index {
            o.field_i64("index", i);
        }
        if let Some(b) = self.content_block {
            o.raw("content_block", &b.to_go());
        }
        if let Some(d) = &self.delta {
            o.field("delta", d);
        }
        if let Some(u) = self.usage {
            o.raw("usage", &u.to_go());
        }
        o.finish()
    }
}

// ===========================================================================
// OpenAI Responses
// ===========================================================================

/// `api.ResponseContentPart`（`text` / `annotations` 都无 omitempty）。
#[derive(Debug, Clone)]
pub struct ResponseContentPart {
    pub r#type: String,
    pub text: String,
}

impl ResponseContentPart {
    pub fn output_text(t: &str) -> ResponseContentPart {
        ResponseContentPart {
            r#type: "output_text".to_string(),
            text: t.to_string(),
        }
    }

    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("type", &self.r#type)
            .field_str("text", &self.text)
            .raw("annotations", "[]");
        o.finish()
    }
}

/// `api.OpenAIResponseItem`。
#[derive(Debug, Clone, Default)]
pub struct OpenAIResponseItem {
    pub id: String,
    pub r#type: String,
    pub status: String,
    pub role: String,
    /// Go 是 `Content []ResponseContentPart` + `json:"content,omitempty"`：
    /// **nil 与空切片都不写**（omitempty 对 slice 判 `len == 0`）——所以「有内容」用
    /// `Some(非空)`，「空/无」一律 `None`，不要用 `Some(Vec::new())`（那会多写一个 `[]`）。
    pub content: Option<Vec<ResponseContentPart>>,
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

impl OpenAIResponseItem {
    pub fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("id", &self.id)
            .field_str("type", &self.r#type)
            .field_str("status", &self.status)
            .opt_str("role", &self.role);
        if let Some(parts) = &self.content {
            let mut s = String::from("[");
            for (i, p) in parts.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&p.to_go());
            }
            s.push(']');
            o.raw("content", &s);
        }
        o.opt_str("call_id", &self.call_id)
            .opt_str("name", &self.name)
            .opt_str("arguments", &self.arguments);
        o.finish()
    }
}

/// `api.ResponseUsage`。
pub struct ResponseUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

impl ResponseUsage {
    fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_i64("input_tokens", self.input_tokens)
            .field_i64("output_tokens", self.output_tokens)
            .field_i64("total_tokens", self.total_tokens);
        o.finish()
    }
}

/// `api.OpenAIResponse`。
pub struct OpenAIResponse {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub status: String,
    pub model: String,
    pub output: Vec<OpenAIResponseItem>,
    pub usage: Option<ResponseUsage>,
}

fn items_to_go(items: &[OpenAIResponseItem]) -> String {
    let mut s = String::from("[");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&it.to_go());
    }
    s.push(']');
    s
}

impl OpenAIResponse {
    pub fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("id", &self.id)
            .field_str("object", &self.object)
            .field_i64("created_at", self.created_at)
            .field_str("status", &self.status)
            .field_str("model", &self.model)
            .raw("output", &items_to_go(&self.output));
        if let Some(u) = &self.usage {
            o.raw("usage", &u.to_go());
        }
        o.finish()
    }
}

/// `responsesStreamEvent`：`delta` / `text` 是 string + omitempty（空串不写）。
pub struct ResponsesStreamEvent<'a> {
    pub r#type: &'a str,
    pub response: Option<&'a OpenAIResponse>,
    pub output_index: Option<i64>,
    pub item: Option<&'a OpenAIResponseItem>,
    pub item_id: String,
    pub content_index: Option<i64>,
    pub part: Option<&'a ResponseContentPart>,
    pub delta: String,
    pub text: String,
}

impl ResponsesStreamEvent<'_> {
    pub fn to_go(&self) -> String {
        let mut o = GoObj::new();
        o.field_str("type", self.r#type);
        if let Some(r) = self.response {
            o.raw("response", &r.to_go());
        }
        if let Some(i) = self.output_index {
            o.field_i64("output_index", i);
        }
        if let Some(it) = self.item {
            o.raw("item", &it.to_go());
        }
        o.opt_str("item_id", &self.item_id);
        if let Some(ci) = self.content_index {
            o.field_i64("content_index", ci);
        }
        if let Some(p) = self.part {
            o.raw("part", &p.to_go());
        }
        o.opt_str("delta", &self.delta);
        o.opt_str("text", &self.text);
        o.finish()
    }
}

