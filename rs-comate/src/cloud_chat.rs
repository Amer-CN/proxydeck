//! OpenAI ⇄ Comate 云直连的翻译层（2026-11-15 新增）。
//!
//! 分工：`cloud.rs` 只管协议（HTTP + SSE 帧 + 工具参数归并），这里管「OpenAI 客户端要什么」：
//! 请求体解析 → 会话/任务准备 → 流式读循环 → chunk/响应拼装 → 工具调用绑定与续接。
//!
//! 客户端工具的注入方式（照 Comate2API / magpie-comate-auth 的公开实现）：
//! `agentInfo.mcpInfos` 里声明一个名叫 [`MCP_SERVER`] 的「假 MCP 服务器」，把每个 OpenAI
//! function 塞成它的 tool；模型调用时回 `UseMcpTool`，我们再翻回 `tool_calls`。
//! 模型看不到「MCP」这个细节——它只看到一组带 inputSchema 的函数。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{Map, Value};

use crate::cloud::{
    CloudClient, CloudError, CloudUsage, PendingToolCall, ToolCallAcc, MCP_SERVER,
};
use crate::gjson::{arr, i, obj, s, J};
use crate::translate::{chunk_json, chunk_reasoning_json, event_from_line_like, LineScanner, MAX_TOKEN};

/// 会话绑定存活时长（工具调用回来时用它续接同一 conversation/task）。
const BINDING_TTL: Duration = Duration::from_secs(30 * 60);
/// 绑定条目上限（防长跑进程无限涨）。
const BINDING_MAX: usize = 256;

// ---------- OpenAI 请求 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    None,
    Auto,
    Required,
    Named(String),
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct ToolResultMsg {
    pub tool_call_id: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub stream: bool,
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    pub parallel: bool,
    /// 末尾连续的 `role:"tool"` 消息（工具结果续接用）。
    pub tool_results: Vec<ToolResultMsg>,
    /// 去掉末尾 tool 消息后的历史（用来拼 query）。
    pub history: Vec<Value>,
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(x)) => x.clone(),
        _ => String::new(),
    }
}

/// 把 `message.content`（字符串或内容块数组）压成纯文本。
fn content_text(v: Option<&Value>) -> Result<String, String> {
    match v {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(x)) => Ok(x.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for p in parts {
                if let Some(o) = p.as_object() {
                    if let Some(Value::String(t)) = o.get("text") {
                        out.push_str(t);
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err("message.content 必须是字符串或内容块数组".to_string()),
    }
}

/// 解析 OpenAI chat.completions 请求体（只取云直连要用的字段）。
pub fn parse_chat_request(body: &[u8]) -> Result<ChatRequest, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("请求体解析失败: {e}"))?;
    let m = v.as_object().ok_or("请求体必须是 JSON 对象")?;

    let messages = m
        .get("messages")
        .and_then(|x| x.as_array())
        .ok_or("messages 必须是数组")?;
    if messages.is_empty() {
        return Err("messages 不能为空".to_string());
    }

    let tools = parse_tools(m.get("tools"))?;
    let tool_choice = parse_tool_choice(m.get("tool_choice"), &tools)?;
    let parallel = match m.get("parallel_tool_calls") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        _ => return Err("parallel_tool_calls 必须是布尔值".to_string()),
    };

    // 末尾连续的 tool 消息 = 工具结果
    let mut idx = messages.len();
    while idx > 0 {
        let role = messages[idx - 1]
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        if role != "tool" {
            break;
        }
        idx -= 1;
    }
    let mut tool_results = Vec::new();
    for t in &messages[idx..] {
        let id = str_of(t.get("tool_call_id"));
        if id.is_empty() {
            return Err("tool 消息缺少 tool_call_id".to_string());
        }
        tool_results.push(ToolResultMsg {
            tool_call_id: id,
            content: content_text(t.get("content"))?,
        });
    }

    Ok(ChatRequest {
        model: str_of(m.get("model")),
        stream: matches!(m.get("stream"), Some(Value::Bool(true))),
        history: messages[..idx].to_vec(),
        tools,
        tool_choice,
        parallel,
        tool_results,
    })
}

fn parse_tools(raw: Option<&Value>) -> Result<Vec<ToolDef>, String> {
    let Some(v) = raw else {
        return Ok(Vec::new());
    };
    if v.is_null() {
        return Ok(Vec::new());
    }
    let list = v.as_array().ok_or("tools 必须是数组")?;
    let mut out: Vec<ToolDef> = Vec::new();
    for t in list {
        let o = t.as_object().ok_or("tools 元素必须是对象")?;
        if o.get("type").and_then(|x| x.as_str()) != Some("function") {
            return Err("目前只支持 function 类型的 tools".to_string());
        }
        let f = o
            .get("function")
            .and_then(|x| x.as_object())
            .ok_or("tool.function 必须是对象")?;
        let name = str_of(f.get("name"));
        if name.trim().is_empty() {
            return Err("工具名称必须是非空字符串".to_string());
        }
        if out.iter().any(|x| x.name == name) {
            return Err(format!("工具名称重复: {name}"));
        }
        let parameters = f
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type":"object","properties":{}}));
        if !parameters.is_object() {
            return Err(format!("工具 {name} 的 parameters 必须是 JSON Schema 对象"));
        }
        out.push(ToolDef {
            description: str_of(f.get("description")),
            name,
            parameters,
        });
    }
    Ok(out)
}

fn parse_tool_choice(raw: Option<&Value>, tools: &[ToolDef]) -> Result<ToolChoice, String> {
    match raw {
        None | Some(Value::Null) => Ok(if tools.is_empty() {
            ToolChoice::None
        } else {
            ToolChoice::Auto
        }),
        Some(Value::String(x)) => match x.as_str() {
            "auto" => Ok(ToolChoice::Auto),
            "none" => Ok(ToolChoice::None),
            "required" if tools.is_empty() => Err("tool_choice=required 需要至少一个工具".into()),
            "required" => Ok(ToolChoice::Required),
            other => Err(format!("未知 tool_choice: {other}")),
        },
        Some(Value::Object(o)) => {
            let name = o
                .get("function")
                .and_then(|f| f.as_object())
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if name.is_empty() {
                return Err("tool_choice 对象缺少 function.name".to_string());
            }
            if !tools.iter().any(|t| t.name == name) {
                return Err(format!("tool_choice 指定的工具不存在: {name}"));
            }
            Ok(ToolChoice::Named(name.to_string()))
        }
        Some(_) => Err("tool_choice 形态不支持".to_string()),
    }
}

/// 本回合真正启用的工具（`none` 全禁；指定函数只放那一个）。
pub fn active_tools(req: &ChatRequest) -> Vec<ToolDef> {
    match &req.tool_choice {
        ToolChoice::None => Vec::new(),
        ToolChoice::Named(n) => req
            .tools
            .iter()
            .filter(|t| &t.name == n)
            .cloned()
            .collect(),
        _ => req.tools.clone(),
    }
}

// ---------- agentInfo（把 tools 变成假 MCP 服务器） ----------

fn prompt_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn schema_json(v: &Value) -> String {
    serde_json::to_string(v)
        .unwrap_or_else(|_| "{}".into())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

/// 构造 `agentInfo`。没有工具时就是一个「只会说话」的 API agent。
pub fn build_agent_info(active: &[ToolDef], choice: &ToolChoice, parallel: bool) -> Value {
    let mut prompt = String::from(
        "You are a conversational API assistant. The query contains the full message history with role labels. \
         Follow system and developer messages, then answer the latest user request. \
         Only use the tools explicitly enabled below. Tool execution is performed by the caller; never invent tool results.",
    );
    let mut names: Vec<String> = Vec::new();
    let mut mcp_infos: Vec<Value> = Vec::new();
    if !active.is_empty() {
        names.push("use_mcp_tool".to_string());
        let mut server = String::new();
        let mut tools_json = Vec::new();
        for t in active {
            server.push_str(&format!(
                "<server_tool name=\"{}\"><tool_description>{}</tool_description><input_schema>{}</input_schema></server_tool>",
                prompt_escape(&t.name),
                prompt_escape(&t.description),
                schema_json(&t.parameters)
            ));
            tools_json.push(serde_json::json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.parameters,
            }));
        }
        prompt.push_str(&format!(
            "\n\nCall the following functions using UseMcpTool / use_mcp_tool with server_name=\"{MCP_SERVER}\", \
             tool_name set to the function name, and arguments containing a JSON object.\n\
             <server name=\"{MCP_SERVER}\">{server}</server>"
        ));
        match choice {
            ToolChoice::Required => prompt.push_str(
                "\nYou MUST call at least one of these functions in this response before providing a final answer.",
            ),
            _ => prompt.push_str(
                "\nCall a function when needed; otherwise answer directly. Existing tool results are already in the history.",
            ),
        }
        if !parallel {
            prompt.push_str("\nCall at most one function in this response.");
        }
        mcp_infos.push(serde_json::json!({
            "serverName": MCP_SERVER,
            "tools": tools_json,
        }));
    } else {
        prompt.push_str("\nRespond only with text. No tools are available.");
    }

    let mut info = Map::new();
    info.insert("agentName".into(), Value::String("API".into()));
    info.insert("isProjectAgent".into(), Value::Bool(true));
    info.insert("isCustomAgent".into(), Value::Bool(true));
    info.insert("canInvokeAgents".into(), Value::Bool(false));
    info.insert(
        "toolNames".into(),
        Value::Array(names.into_iter().map(Value::String).collect()),
    );
    info.insert("agentPrompt".into(), Value::String(prompt));
    if !mcp_infos.is_empty() {
        info.insert("mcpInfos".into(), Value::Array(mcp_infos));
    }
    Value::Object(info)
}

// ---------- 工具调用映射 ----------

#[derive(Debug, Clone)]
pub struct OpenAiToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 原生 `UseMcpTool` 调用 → 客户端的 function 调用。
pub fn map_native_calls(
    native: Vec<PendingToolCall>,
    active: &[ToolDef],
) -> Result<Vec<OpenAiToolCall>, String> {
    let mut out = Vec::new();
    for c in native {
        let is_mcp = c.name == "UseMcpTool" || c.name == "use_mcp_tool";
        let (name, args) = if is_mcp {
            let server = c
                .input
                .get("server_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !server.is_empty() && server != MCP_SERVER {
                return Err(format!("上游调用了未知 MCP 服务器: {server}"));
            }
            let name = c
                .input
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                return Err("上游工具调用缺少 tool_name".to_string());
            }
            if !active.iter().any(|t| t.name == name) {
                return Err(format!("上游调用了未声明的工具: {name}"));
            }
            let raw = c.input.get("arguments").cloned().unwrap_or(Value::Null);
            let args = match raw {
                Value::String(x) => x,
                Value::Null => "{}".to_string(),
                other => serde_json::to_string(&other).unwrap_or_else(|_| "{}".into()),
            };
            let parsed: Value = serde_json::from_str(&args)
                .map_err(|e| format!("工具 {name} 的 arguments 不是合法 JSON: {e}"))?;
            if !parsed.is_object() {
                return Err(format!("工具 {name} 的 arguments 必须是 JSON 对象"));
            }
            (name, serde_json::to_string(&parsed).unwrap_or_else(|_| "{}".into()))
        } else {
            (
                c.name.clone(),
                serde_json::to_string(&Value::Object(c.input.clone()))
                    .unwrap_or_else(|_| "{}".into()),
            )
        };
        out.push(OpenAiToolCall {
            id: if c.id.is_empty() {
                format!("call_{}", crate::new_id_suffix())
            } else {
                c.id
            },
            name,
            arguments: args,
        });
    }
    Ok(out)
}

// ---------- 会话绑定（工具结果续接） ----------

#[derive(Debug, Clone)]
pub struct Binding {
    pub tool_call_id: String,
    pub conversation_id: i64,
    pub task_id: i64,
    pub model_id: String,
    pub native_id: String,
    pub native_name: String,
    pub params: Value,
    pub at: Instant,
}

#[derive(Default)]
pub struct SessionStore {
    map: std::sync::Mutex<HashMap<String, Binding>>,
}

impl SessionStore {
    pub fn record(&self, b: Binding) {
        let Ok(mut m) = self.map.lock() else { return };
        m.insert(b.tool_call_id.clone(), b);
        // 过期 + 上限（超了先扔最老的）
        let now = Instant::now();
        m.retain(|_, v| now.duration_since(v.at) < BINDING_TTL);
        if m.len() > BINDING_MAX {
            let mut by_age: Vec<(Instant, String)> =
                m.iter().map(|(k, v)| (v.at, k.clone())).collect();
            by_age.sort_by_key(|(t, _)| *t);
            for (_, k) in by_age.into_iter().take(m.len() - BINDING_MAX) {
                m.remove(&k);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Binding> {
        let m = self.map.lock().ok()?;
        m.get(id)
            .filter(|b| b.at.elapsed() < BINDING_TTL)
            .cloned()
    }

    /// 这一轮的工具结果是否都能续接到同一个会话上。
    pub fn resume_for(&self, results: &[ToolResultMsg]) -> Option<(Binding, Vec<Value>)> {
        if results.is_empty() {
            return None;
        }
        let first = self.get(&results[0].tool_call_id)?;
        let mut out = Vec::new();
        for r in results {
            let b = self.get(&r.tool_call_id)?;
            if b.conversation_id != first.conversation_id || b.task_id != first.task_id {
                return None;
            }
            out.push(serde_json::json!({
                "id": b.native_id,
                "name": b.native_name,
                "success": true,
                "params": b.params,
                "message": r.content,
            }));
        }
        Some((first, out))
    }
}

// ---------- 一轮云对话 ----------

pub struct PreparedTurn {
    pub resp: reqwest::Response,
    pub conversation_id: i64,
    pub task_id: i64,
    pub model_id: String,
    pub resuming: bool,
}

/// 准备一轮对话：解析模型 → 建会话/任务（或续接）→ 发起 execute。
pub async fn prepare_turn(
    client: &CloudClient,
    req: &ChatRequest,
    model_id: &str,
    model_provider: &str,
    active: &[ToolDef],
    sessions: &SessionStore,
) -> Result<PreparedTurn, CloudError> {
    let agent_info = build_agent_info(active, &req.tool_choice, req.parallel);
    let resume = sessions.resume_for(&req.tool_results);
    let (conversation_id, task_id, query, tool_results, first) = match resume {
        Some((b, results)) => {
            let q = String::new();
            (b.conversation_id, b.task_id, q, Some(results), false)
        }
        None => {
            let query = crate::translate::flatten_messages(&req.history);
            if query.trim().is_empty() {
                return Err(CloudError {
                    status: 400,
                    message: "messages 里没有可用内容".to_string(),
                });
            }
            let conv = client.create_conversation(&query, &agent_info).await?;
            let task = client.create_task(conv, &agent_info).await?;
            (conv, task, query, None, true)
        }
    };
    let resp = client
        .execute(
            conversation_id,
            task_id,
            model_id,
            model_provider,
            &query,
            &agent_info,
            tool_results,
            first,
            !active.is_empty(),
        )
        .await?;
    Ok(PreparedTurn {
        resp,
        conversation_id,
        task_id,
        model_id: model_id.to_string(),
        resuming: !first,
    })
}

/// 流式读循环里的状态（与 zulu 侧的 `StreamRun` 同构）。
pub struct CloudReader {
    pub id: String,
    pub model: String,
    pub created: i64,
    pub out: Vec<u8>,
    pub committed: bool,
    pub content: String,
    pub reasoning: String,
    pub usage: Option<CloudUsage>,
    pub error: String,
    pub ended: bool,
    tools: ToolCallAcc,
    /// 已映射成 OpenAI 形态的调用（结束时填）。
    pub calls: Vec<OpenAiToolCall>,
    /// 上游原始调用（保留 native id/参数，供工具结果续接时回填 `toolUseResults`）。
    pub native: Vec<PendingToolCall>,
}

impl CloudReader {
    pub fn new(id: String, model: String, created: i64) -> Self {
        Self {
            id,
            model,
            created,
            out: Vec::new(),
            committed: false,
            content: String::new(),
            reasoning: String::new(),
            usage: None,
            error: String::new(),
            ended: false,
            tools: ToolCallAcc::default(),
            calls: Vec::new(),
            native: Vec::new(),
        }
    }

    fn push_frame(&mut self, chunk: String) {
        self.out.extend_from_slice(b"data: ");
        self.out.extend_from_slice(chunk.as_bytes());
        self.out.extend_from_slice(b"\n\n");
        self.committed = true;
    }

    /// 吃一行 SSE。
    pub fn feed(&mut self, line: &[u8]) {
        let Some(f) = event_from_line_like(line) else {
            return;
        };
        match f.kind.as_str() {
            "ANSWER" => {
                if !f.reasoning.is_empty() {
                    self.reasoning.push_str(&f.reasoning);
                    let c = chunk_reasoning_json(&self.id, &self.model, &f.reasoning, "", self.created);
                    self.push_frame(c);
                }
                if !f.delta.is_empty() {
                    self.content.push_str(&f.delta);
                    let c = chunk_json(&self.id, &self.model, &f.delta, "", self.created);
                    self.push_frame(c);
                }
            }
            "TOKEN_USAGE" => {
                if let Some(u) = f.usage {
                    let acc = self.usage.get_or_insert(CloudUsage::default());
                    acc.input_tokens += u.input_tokens;
                    acc.cached_input_tokens += u.cached_input_tokens;
                    acc.output_tokens += u.output_tokens;
                }
            }
            "EXCEPTION" => {
                if self.error.is_empty() {
                    self.error = if f.exception.is_empty() {
                        "上游返回异常（无文案）".to_string()
                    } else {
                        f.exception
                    };
                }
            }
            k if k.starts_with("FUNCTION_CALL_") || k.starts_with("TOOL_CALL_") => {
                self.tools.consume(k, &f.tool_use);
            }
            _ => {}
        }
        if f.end {
            self.ended = true;
        }
    }

    /// 结束块：tool_calls（若有）→ finish_reason → usage → `[DONE]`。
    pub fn finish_frames(&mut self, active: &[ToolDef]) -> Result<(), String> {
        let native = self.tools.take_finished(true);
        if !native.is_empty() {
            self.native = native.clone();
            match map_native_calls(native, active) {
                Ok(calls) => self.calls = calls,
                Err(e) => {
                    if self.error.is_empty() {
                        self.error = e;
                    }
                }
            }
        }
        if !self.calls.is_empty() {
            let chunk = chunk_tool_calls(&self.id, &self.model, self.created, &self.calls);
            self.push_frame(chunk);
            let stop = chunk_json(&self.id, &self.model, "", "tool_calls", self.created);
            self.push_frame(stop);
        } else {
            let stop = chunk_json(&self.id, &self.model, "", "stop", self.created);
            self.push_frame(stop);
        }
        if let Some(u) = self.usage {
            let chunk = chunk_usage(&self.id, &self.model, self.created, u);
            self.push_frame(chunk);
        }
        self.out.extend_from_slice(b"data: [DONE]\n\n");
        Ok(())
    }
}

/// OpenAI `delta.tool_calls` 块（一次给全所有调用）。
pub fn chunk_tool_calls(id: &str, model: &str, created: i64, calls: &[OpenAiToolCall]) -> String {
    let list: Vec<J> = calls
        .iter()
        .enumerate()
        .map(|(n, c)| {
            obj(vec![
                ("index", i(n as i64)),
                ("id", s(&c.id)),
                ("type", s("function")),
                (
                    "function",
                    obj(vec![("name", s(&c.name)), ("arguments", s(&c.arguments))]),
                ),
            ])
        })
        .collect();
    let delta = obj(vec![("tool_calls", J::Arr(list))]);
    chunk_entry(id, model, created, delta)
}

/// 流式最后一块的 usage（OpenAI 规范：`choices: []` + `usage`）。
pub fn chunk_usage(id: &str, model: &str, created: i64, u: CloudUsage) -> String {
    let usage = obj(vec![
        ("prompt_tokens", i(u.prompt_tokens())),
        ("completion_tokens", i(u.completion_tokens())),
        ("total_tokens", i(u.total_tokens())),
        (
            "prompt_tokens_details",
            obj(vec![("cached_tokens", i(u.cached_input_tokens))]),
        ),
    ]);
    J::Obj(vec![
        ("choices".to_string(), arr(vec![])),
        ("created".to_string(), i(created)),
        ("id".to_string(), s(id)),
        ("model".to_string(), s(model)),
        ("object".to_string(), s("chat.completion.chunk")),
        ("usage".to_string(), usage),
    ])
    .to_go()
}

fn chunk_entry(id: &str, model: &str, created: i64, delta: J) -> String {
    J::Obj(vec![
        (
            "choices".to_string(),
            arr(vec![J::Obj(vec![
                ("delta".to_string(), delta),
                ("finish_reason".to_string(), J::Null),
                ("index".to_string(), i(0)),
            ])]),
        ),
        ("created".to_string(), i(created)),
        ("id".to_string(), s(id)),
        ("model".to_string(), s(model)),
        ("object".to_string(), s("chat.completion.chunk")),
    ])
    .to_go()
}

/// 非流式完整响应（带 reasoning / tool_calls / usage）。
pub fn complete_response(
    id: &str,
    model: &str,
    created: i64,
    content: &str,
    reasoning: &str,
    calls: &[OpenAiToolCall],
    usage: Option<CloudUsage>,
) -> String {
    let mut message: Vec<(String, J)> = vec![("role".to_string(), s("assistant"))];
    message.push((
        "content".to_string(),
        if calls.is_empty() && content.is_empty() {
            J::Null
        } else {
            s(content)
        },
    ));
    if !reasoning.is_empty() {
        message.push(("reasoning_content".to_string(), s(reasoning)));
    }
    if !calls.is_empty() {
        let list: Vec<J> = calls
            .iter()
            .map(|c| {
                obj(vec![
                    ("id", s(&c.id)),
                    ("type", s("function")),
                    (
                        "function",
                        obj(vec![("name", s(&c.name)), ("arguments", s(&c.arguments))]),
                    ),
                ])
            })
            .collect();
        message.push(("tool_calls".to_string(), J::Arr(list)));
    }
    let finish = if calls.is_empty() { "stop" } else { "tool_calls" };
    let usage_j = match usage {
        Some(u) => obj(vec![
            ("prompt_tokens", i(u.prompt_tokens())),
            ("completion_tokens", i(u.completion_tokens())),
            ("total_tokens", i(u.total_tokens())),
            (
                "prompt_tokens_details",
                obj(vec![("cached_tokens", i(u.cached_input_tokens))]),
            ),
        ]),
        None => obj(vec![
            ("prompt_tokens", i(0)),
            ("completion_tokens", i(0)),
            ("total_tokens", i(0)),
        ]),
    };
    J::Obj(vec![
        (
            "choices".to_string(),
            arr(vec![J::Obj(vec![
                ("finish_reason".to_string(), s(finish)),
                ("index".to_string(), i(0)),
                ("message".to_string(), J::Obj(message)),
            ])]),
        ),
        ("created".to_string(), i(created)),
        ("id".to_string(), s(id)),
        ("model".to_string(), s(model)),
        ("object".to_string(), s("chat.completion")),
        ("usage".to_string(), usage_j),
    ])
    .to_go()
}

/// 非流式：把整条 SSE 读完（不算超时，交给 reqwest 的 600s）。
pub async fn read_all(
    resp: reqwest::Response,
    reader: &mut CloudReader,
    active: &[ToolDef],
) -> Result<(), String> {
    let mut scanner = LineScanner::new(MAX_TOKEN);
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                for line in scanner.push(&chunk) {
                    reader.feed(&line);
                }
            }
            Err(e) => {
                if reader.error.is_empty() {
                    reader.error = format!("上游 SSE 中断: {e}");
                }
                break;
            }
        }
        if reader.ended {
            break;
        }
    }
    if let Some(tail) = scanner.finish() {
        reader.feed(&tail);
    }
    if let Err(e) = reader.finish_frames(active) {
        if reader.error.is_empty() {
            reader.error = e;
        }
    }
    if !reader.error.is_empty() {
        return Err(reader.error.clone());
    }
    Ok(())
}

/// 流式：把已积累的输出取走（供 lib.rs 的读循环用）。
pub fn take_out(reader: &mut CloudReader) -> Vec<u8> {
    std::mem::take(&mut reader.out)
}

/// 供流式响应体使用的小工具：把 Vec<u8> 转成 `Bytes`。
pub fn bytes_of(v: Vec<u8>) -> Bytes {
    Bytes::from(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "d".into(),
            parameters: serde_json::json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
        }
    }

    #[test]
    fn parses_tools_and_choice() {
        let body = r#"{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"get_weather","description":"w","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}],"tool_choice":{"type":"function","function":{"name":"get_weather"}}}"#;
        let req = parse_chat_request(body.as_bytes()).unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tool_choice, ToolChoice::Named("get_weather".into()));
        assert!(req.tool_results.is_empty());

        let bad = r#"{"model":"x","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"a","parameters":[]}}]}"#;
        assert!(parse_chat_request(bad.as_bytes()).is_err());
    }

    #[test]
    fn collects_trailing_tool_results() {
        let body = r#"{"model":"x","messages":[
            {"role":"user","content":"q"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"w","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"晴"}
        ]}"#;
        let req = parse_chat_request(body.as_bytes()).unwrap();
        assert_eq!(req.tool_results.len(), 1);
        assert_eq!(req.tool_results[0].tool_call_id, "call_1");
        assert_eq!(req.history.len(), 2);
    }

    #[test]
    fn agent_info_declares_mcp_server() {
        let info = build_agent_info(&[tool("get_weather")], &ToolChoice::Auto, true);
        let names = info.get("toolNames").unwrap().as_array().unwrap();
        assert_eq!(names[0], "use_mcp_tool");
        let mcp = info.get("mcpInfos").unwrap().as_array().unwrap();
        assert_eq!(mcp[0]["serverName"], MCP_SERVER);
        assert_eq!(mcp[0]["tools"][0]["name"], "get_weather");
        let prompt = info.get("agentPrompt").unwrap().as_str().unwrap();
        assert!(prompt.contains("get_weather"));
        assert!(prompt.contains(MCP_SERVER));
    }

    #[test]
    fn maps_native_mcp_call_to_function() {
        let mut input = Map::new();
        input.insert("server_name".into(), Value::String(MCP_SERVER.into()));
        input.insert("tool_name".into(), Value::String("get_weather".into()));
        input.insert("arguments".into(), Value::String("{\"city\":\"上海\"}".into()));
        let native = vec![PendingToolCall {
            id: "call_1".into(),
            name: "UseMcpTool".into(),
            input,
            ended: true,
        }];
        let calls = map_native_calls(native, &[tool("get_weather")]).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, "{\"city\":\"上海\"}");

        // 未声明的工具 → 报错
        let mut bad = Map::new();
        bad.insert("tool_name".into(), Value::String("nope".into()));
        let native = vec![PendingToolCall {
            id: "c".into(),
            name: "UseMcpTool".into(),
            input: bad,
            ended: true,
        }];
        assert!(map_native_calls(native, &[tool("get_weather")]).is_err());
    }

    #[test]
    fn reader_streams_text_and_finishes_with_usage() {
        let mut r = CloudReader::new("id".into(), "m".into(), 1);
        r.feed(r#"data: {"content":{"type":"ANSWER","detail":{"delta":"你","reasoningDelta":"想","end":false},"end":false},"end":false}"#.as_bytes());
        r.feed(r#"data: {"content":{"type":"ANSWER","detail":{"delta":"好","reasoningDelta":"","end":true},"end":true},"end":true}"#.as_bytes());
        assert_eq!(r.content, "你好");
        assert_eq!(r.reasoning, "想");
        assert!(r.ended);
        r.usage = Some(CloudUsage {
            input_tokens: 10,
            cached_input_tokens: 5,
            output_tokens: 2,
        });
        r.finish_frames(&[]).unwrap();
        let out = String::from_utf8(r.out.clone()).unwrap();
        assert!(out.contains("\"content\":\"你\""), "{out}");
        assert!(out.contains("\"reasoning_content\":\"想\""), "{out}");
        assert!(out.contains("\"finish_reason\":\"stop\""), "{out}");
        assert!(out.contains("\"prompt_tokens\":15"), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
    }

    #[test]
    fn session_resume_matches_same_conversation() {
        let store = SessionStore::default();
        store.record(Binding {
            tool_call_id: "call_1".into(),
            conversation_id: 7,
            task_id: 9,
            model_id: "m".into(),
            native_id: "call_1".into(),
            native_name: "UseMcpTool".into(),
            params: serde_json::json!({"tool_name":"get_weather"}),
            at: Instant::now(),
        });
        let results = vec![ToolResultMsg {
            tool_call_id: "call_1".into(),
            content: "晴".into(),
        }];
        let (b, payload) = store.resume_for(&results).unwrap();
        assert_eq!(b.conversation_id, 7);
        assert_eq!(payload[0]["id"], "call_1");
        assert_eq!(payload[0]["message"], "晴");
        assert_eq!(payload[0]["success"], true);

        let unknown = vec![ToolResultMsg {
            tool_call_id: "call_x".into(),
            content: "".into(),
        }];
        assert!(store.resume_for(&unknown).is_none());
    }
}