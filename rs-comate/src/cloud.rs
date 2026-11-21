//! Comate 云直连协议（2026-11-15 实测，照官方 CLI 的 HTTP 层复刻）。
//!
//! 为什么要这条路线：zulu serve 的会话接口字段全集里**没有 `tools`**，客户端工具根本
//! 注入不进去；而官方 CLI（`zulu`）自己就是这套 HTTPS 调用——
//!
//! ```text
//! GET  /api/v2/api/models/available?key=<license>&type=ZULU&...
//! POST /api/aidevops/autocomate/rest/autowork/v2/conversation  → data.id
//! POST .../rest/autowork/v2/task                              → data.taskId
//! POST .../rest/autowork/v2/execute                           → SSE
//! ```
//!
//! 认证只要两个头：`login-name` / `Uuap-login-name` = license（**无签名、无设备绑定**）。
//! 客户端工具通过 `agentInfo.mcpInfos` 声明成一个名叫 [`MCP_SERVER`] 的「假 MCP 服务器」，
//! 模型调它时回 `FUNCTION_CALL_*` 帧，我们再翻成 OpenAI 的 `tool_calls`。
//!
//! 本模块只管「协议」：HTTP 调用 + SSE 帧解析 + 工具参数归并。
//! OpenAI 那一侧的翻译在 `cloud_chat.rs`。

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};

/// 云直连的入口（SaaS）。
pub const CLOUD_BASE: &str = "https://comate.baidu.com";

/// 客户端工具的「假 MCP 服务器」名（官方客户端自己也有同名机制，这里只是借用字段）。
pub const MCP_SERVER: &str = "openai_client";

/// `agentVersion` 常量（Comate2API 逆向自官方客户端；2026-11-15 实测 200）。
pub const AGENT_VERSION: i64 = 12;

/// 额度用尽后仍可用的免费轻量模型。
///
/// 它**不在**账号模型目录里（目录只列付费模型），但上游认这个 modelKey：
/// 2026-11-21 实测 `execute` 返回 200、正文与 TOKEN_USAGE 都正常。
pub const AUTO_FREE_MODEL: &str = "auto-free-individual";

/// 探测不到本机 IDE 扩展版本时的兜底值。
///
/// ⚠ 这里填的是**IDE 扩展版本**（`zulucli-<扩展版本>`），**不是** `@comate/zulu` 包的版本。
/// 2026-11-21 对照 [magpie-comate PROTOCOL.md](https://github.com/httpdone/magpie-comate/blob/main/PROTOCOL.md)
/// 才发现：官方 `plugin-version` 形如 `zulucli-4.13.0`，对应 `BaiduComate.comate` 扩展的
/// `package.json` 版本；而本机 zulu 包是 1.8.1 —— 旧值 `zulucli-1.8.1` 一直填错了来源。
pub const DEFAULT_PLUGIN_VERSION: &str = "zulucli-4.13.4";

/// 解析要上报的 `plugin-version`。
///
/// 优先级：`COMATE_PLUGIN_VERSION` → 本机已装 IDE 扩展的版本（`zulucli-<ver>`）→ 兜底常量。
/// 跟着本机安装走，Comate 升级后不用改代码。
pub fn resolve_plugin_version() -> String {
    if let Ok(v) = std::env::var("COMATE_PLUGIN_VERSION") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    match crate::zulu::detect_extension_version() {
        Some(v) => format!("zulucli-{v}"),
        None => DEFAULT_PLUGIN_VERSION.to_string(),
    }
}

/// 一个 uuid-v4 形态的 id（官方的 `X-Trace-Id` 是 uuid）。
///
/// 不引 uuid crate：只要形态一致就行，取值本身没有语义。
pub fn trace_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut x = nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x2545_F491_4F6C_DD1D;
    let mut hex = String::with_capacity(32);
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        hex.push_str(&format!("{x:016x}"));
    }
    // 32 个 ASCII hex 字符，按字节切片安全。
    format!(
        "{}-{}-4{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

// ---------- 模型目录 ----------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CloudModel {
    #[serde(rename = "modelId", default)]
    pub model_id: String,
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    #[serde(rename = "modelType", default)]
    pub model_type: String,
    #[serde(rename = "provider", default)]
    pub provider: String,
    #[serde(rename = "supportImage", default)]
    pub support_image: bool,
    #[serde(rename = "supportThinking", default)]
    pub support_thinking: bool,
    #[serde(rename = "supportEffort", default)]
    pub support_effort: Option<String>,
    #[serde(rename = "maxContextWindow", default)]
    pub max_context_window: Option<i64>,
    #[serde(rename = "ratio", default)]
    pub ratio: Option<String>,
    #[serde(rename = "modelDescription", default)]
    pub model_description: Option<String>,
    #[serde(rename = "isDefault", default)]
    pub is_default: bool,
}

/// 一帧上游 SSE（`data: {...}` 里的那个对象）。
#[derive(Debug, Default, Clone)]
pub struct CloudFrame {
    /// `content.type`：`ANSWER` / `FUNCTION_CALL_START` / `FUNCTION_CALL_PARAMS_APPEND` /
    /// `FUNCTION_CALL_END` / `TOKEN_USAGE` / `EXCEPTION` …
    pub kind: String,
    /// `detail.delta`（正文增量）。
    pub delta: String,
    /// `detail.reasoningDelta`（思考增量）。
    pub reasoning: String,
    /// 结束标记（`content.end` 或顶层 `end`）。
    pub end: bool,
    /// 失败文案（`detail.exceptionMsg`）。
    pub exception: String,
    /// 非内容帧的附带文案（`NOTIFICATION` / `DOWNGRADE` 等的 message 类字段）。
    pub note: String,
    /// `QUOTA_EXCEED` 场景：服务端给的按钮链接（购买/了解额度）。
    pub hint_url: String,
    /// `QUOTA_EXCEED` 场景：服务端建议的降级模型（如 `auto-free-individual`）。
    pub auto_model: String,
    pub usage: Option<CloudUsage>,
    pub tool_use: Vec<CloudToolUse>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CloudUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
}

impl CloudUsage {
    /// Comate 的 `inputTokens` **不含**缓存命中，两者相加才是 OpenAI 的 prompt_tokens。
    pub fn prompt_tokens(&self) -> i64 {
        self.input_tokens + self.cached_input_tokens
    }
    pub fn completion_tokens(&self) -> i64 {
        self.output_tokens
    }
    pub fn total_tokens(&self) -> i64 {
        self.prompt_tokens() + self.completion_tokens()
    }
}

/// `detail.toolUse[]` 的一路参数片段（`input` 里同名字段是**追加**语义）。
#[derive(Debug, Default, Clone)]
pub struct CloudToolUse {
    pub id: String,
    pub name: String,
    pub input: Map<String, Value>,
}

// ---------- SSE 帧解析 ----------

fn s(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(x)) => x.clone(),
        _ => String::new(),
    }
}

fn b(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true)))
}

/// 解析一条 `data:` 行里的 JSON。不认识的形状返回 `None`（调用方跳过）。
pub fn parse_frame(json: &str) -> Option<CloudFrame> {
    let v: Value = serde_json::from_str(json).ok()?;
    let top = v.as_object()?;
    let content = top.get("content")?.as_object()?;
    let kind = match content.get("type") {
        Some(Value::String(t)) => t.clone(),
        _ => return None,
    };
    let detail = content.get("detail").and_then(|d| d.as_object());
    let mut f = CloudFrame {
        kind: kind.clone(),
        end: b(content.get("end")) || b(top.get("end")),
        ..Default::default()
    };
    let Some(detail) = detail else {
        return Some(f);
    };
    f.delta = s(detail.get("delta"));
    f.reasoning = s(detail.get("reasoningDelta"));
    f.exception = s(detail.get("exceptionMsg"));
    // 非内容帧（NOTIFICATION / DOWNGRADE / QUOTA_EXCEED）的文案字段各异，挨个认。
    f.note = ["message", "notificationMsg", "notification", "msg", "desc", "reason"]
        .iter()
        .find_map(|k| {
            let v = s(detail.get(*k));
            (!v.trim().is_empty()).then_some(v)
        })
        .unwrap_or_default();
    // 额度用尽时真正的文案在 `detail.userActions.cards[].message`，按钮链接在
    // `cards[].buttons[].url`（2026-11-21 真机抓帧）。detail.message 往往是空的。
    f.auto_model = s(detail.get("autoModel"));
    if let Some(cards) = detail
        .get("userActions")
        .and_then(|a| a.get("cards"))
        .and_then(|c| c.as_array())
    {
        for card in cards {
            if f.note.trim().is_empty() {
                let m = s(card.get("message"));
                if !m.trim().is_empty() {
                    f.note = m;
                }
            }
            if f.hint_url.is_empty() {
                if let Some(btns) = card.get("buttons").and_then(|b| b.as_array()) {
                    for btn in btns {
                        let u = s(btn.get("url"));
                        if !u.trim().is_empty() {
                            f.hint_url = u;
                            let label = s(btn.get("text"));
                            if f.note.is_empty() && !label.is_empty() {
                                f.note = label;
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
    if let Some(u) = detail.get("usage").and_then(|u| u.as_object()) {
        let pick = |k: &str| {
            u.get(k)
                .and_then(|x| x.as_i64())
                .or_else(|| u.get(k).and_then(|x| x.as_str()).and_then(|x| x.parse().ok()))
                .unwrap_or(0)
        };
        f.usage = Some(CloudUsage {
            input_tokens: pick("inputTokens"),
            cached_input_tokens: pick("cachedInputTokens"),
            output_tokens: pick("outputTokens"),
        });
    }
    if let Some(list) = detail.get("toolUse").and_then(|x| x.as_array()) {
        for it in list {
            let Some(m) = it.as_object() else { continue };
            let input = m
                .get("input")
                .and_then(|x| x.as_object())
                .cloned()
                .unwrap_or_default();
            f.tool_use.push(CloudToolUse {
                id: s(m.get("id")),
                name: s(m.get("name")),
                input,
            });
        }
    }
    Some(f)
}

// ---------- 工具参数归并 ----------

#[derive(Debug, Clone)]
pub struct PendingToolCall {
    pub id: String,
    pub name: String,
    pub input: Map<String, Value>,
    pub ended: bool,
}

/// 把 `FUNCTION_CALL_*` 的碎片拼成完整调用。
///
/// 官方语义（2026-11-15 抓包实证）：
/// * `_START` 给 `id`/`name`，`input` 是空对象；
/// * `_PARAMS_APPEND` 的 `input` 是**同名 key 的字符串追加**（`{"arguments":"{\""}`、
///   `{"arguments":"city"}` …拼成 `{"city":`）——不是整段替换；
/// * `_END` 只置结束位，`input` 为空。
#[derive(Debug, Default)]
pub struct ToolCallAcc {
    order: Vec<String>,
    map: HashMap<String, PendingToolCall>,
}

impl ToolCallAcc {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn consume(&mut self, kind: &str, uses: &[CloudToolUse]) {
        let append = kind.contains("PARAMS_APPEND") || kind.contains("MERGE");
        let ended = kind.ends_with("_END");
        for u in uses {
            if u.id.is_empty() {
                continue;
            }
            if !self.map.contains_key(&u.id) {
                self.order.push(u.id.clone());
                self.map.insert(
                    u.id.clone(),
                    PendingToolCall {
                        id: u.id.clone(),
                        name: u.name.clone(),
                        input: Map::new(),
                        ended: false,
                    },
                );
            }
            let Some(call) = self.map.get_mut(&u.id) else {
                continue;
            };
            if !u.name.is_empty() {
                call.name = u.name.clone();
            }
            for (k, v) in &u.input {
                match (append, call.input.get(k).cloned(), v) {
                    (true, Some(Value::String(prev)), Value::String(next)) => {
                        call.input
                            .insert(k.clone(), Value::String(format!("{prev}{next}")));
                    }
                    (true, Some(Value::Null), _) | (true, None, _) => {
                        call.input.insert(k.clone(), v.clone());
                    }
                    _ => {
                        call.input.insert(k.clone(), v.clone());
                    }
                }
            }
            if ended {
                call.ended = true;
            }
        }
    }

    /// 取出已结束的调用（按首次出现顺序）。未结束的留在累加器里等后续帧。
    pub fn take_finished(&mut self, flush: bool) -> Vec<PendingToolCall> {
        let mut out = Vec::new();
        let ids = self.order.clone();
        for id in ids {
            let done = self.map.get(&id).map(|c| c.ended).unwrap_or(false);
            if done || flush {
                if let Some(c) = self.map.remove(&id) {
                    self.order.retain(|x| x != &id);
                    if !c.input.is_empty() || !c.name.is_empty() {
                        out.push(c);
                    }
                }
            }
        }
        out
    }
}

// ---------- HTTP 客户端 ----------

#[derive(Debug)]
pub struct CloudError {
    pub status: u16,
    pub message: String,
}

impl CloudError {
    fn net(e: impl std::fmt::Display) -> Self {
        Self {
            status: 502,
            message: format!("云直连失败: {e}"),
        }
    }
    fn http(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub struct CloudClient {
    http: reqwest::Client,
    pub base: String,
    pub license: String,
    pub plugin_version: String,
    pub ide_version: String,
    pub device: String,
}

impl CloudClient {
    pub fn new(base: impl Into<String>, license: impl Into<String>) -> Self {
        let license = license.into();
        let plugin_version = resolve_plugin_version();
        let ide_version = plugin_version
            .strip_prefix("zulucli-")
            .unwrap_or(&plugin_version)
            .to_string();
        let device = device_for(&license);
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            base: base.into(),
            license,
            plugin_version,
            ide_version,
            device,
        }
    }

    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        let lic = HeaderValue::from_str(&self.license).unwrap_or(HeaderValue::from_static(""));
        h.insert("login-name", lic.clone());
        h.insert("Uuap-login-name", lic);
        h.insert("X-Source", HeaderValue::from_static("COMATE"));
        insert_str(&mut h, "plugin-version", &self.plugin_version);
        h.insert("x-skip-fcnap", HeaderValue::from_static("yes"));
        // 官方每条请求都带（2026-11-21 对照 magpie-comate PROTOCOL.md 补齐）。
        insert_str(&mut h, "X-Trace-Id", &trace_id());
        h.insert("Accept", HeaderValue::from_static("application/json"));
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        h.insert("User-Agent", HeaderValue::from_static("axios/1.7.9"));
        h
    }

    fn upload_base_info(&self) -> Value {
        serde_json::json!({
            "os": "windows",
            "osVersion": "Windows 11",
            "extName": "zulucli",
            "extVersion": self.plugin_version,
            "ideType": "zulucli",
            "ideName": "zulucli",
            "ideVersion": self.ide_version,
            "username": self.license,
            "license": self.license,
            "pluginVersion": self.plugin_version,
            "device": self.device,
            "triggerSource": "",
        })
    }

    /// `GET /api/v2/api/models/available`（实时目录，带能力字段）。
    pub async fn list_models(&self) -> Result<Vec<CloudModel>, CloudError> {
        let url = format!(
            "{}/api/v2/api/models/available?key={}&type=ZULU&repoName=api&loopVersion=v2&zuluVersion={}&source=plugin&ideType=zulucli",
            self.base,
            urlencode(&self.license),
            urlencode(&self.plugin_version)
        );
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers())
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(CloudError::net)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(CloudError::net)?;
        if status != 200 {
            return Err(CloudError::http(status, format!("模型目录 HTTP {status}: {text}")));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| CloudError::http(502, format!("模型目录不是 JSON: {e}")))?;
        let status_str = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
        if !status_str.eq_ignore_ascii_case("OK") {
            let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
            return Err(CloudError::http(502, format!("模型目录 {status_str}: {msg}")));
        }
        let list = v
            .get("data")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list
            .into_iter()
            .filter_map(|x| serde_json::from_value::<CloudModel>(x).ok())
            .filter(|m| !m.model_id.is_empty())
            .collect())
    }

    /// 第一步：让服务端分配 conversationId。
    pub async fn create_conversation(
        &self,
        query: &str,
        agent_info: &Value,
    ) -> Result<i64, CloudError> {
        let body = serde_json::json!({
            "username": self.license,
            "ide": "zulucli",
            "ideVersion": self.ide_version,
            "pluginVersion": self.plugin_version,
            "query": query,
            "uploadBaseInfo": self.upload_base_info(),
            "agentInfo": agent_info,
        });
        let v = self
            .post_json("/api/aidevops/autocomate/rest/autowork/v2/conversation", &body)
            .await?;
        parse_id(&v, "id").ok_or_else(|| CloudError::http(502, "会话响应缺少 data.id"))
    }

    /// 第二步：让服务端分配 taskId。
    pub async fn create_task(
        &self,
        conversation_id: i64,
        agent_info: &Value,
    ) -> Result<i64, CloudError> {
        let mut agent = agent_info.clone();
        if let Some(m) = agent.as_object_mut() {
            m.insert("isProjectAgent".into(), Value::Bool(false));
            m.remove("mcpInfos");
            m.remove("dynamicToolDefinitions");
            m.remove("agentPrompt");
            m.insert("canInvokeAgents".into(), Value::Bool(false));
        }
        let body = serde_json::json!({
            "agentInfo": agent,
            "conversationId": conversation_id,
            "username": self.license,
            "ide": "zulucli",
            "ideVersion": self.ide_version,
            "pluginVersion": self.plugin_version,
        });
        let v = self
            .post_json("/api/aidevops/autocomate/rest/autowork/v2/task", &body)
            .await?;
        parse_id(&v, "taskId").ok_or_else(|| CloudError::http(502, "任务响应缺少 data.taskId"))
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<Value, CloudError> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .post(&url)
            .headers(self.auth_headers())
            .body(serde_json::to_string(body).unwrap_or_default())
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(CloudError::net)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(CloudError::net)?;
        if status != 200 {
            return Err(CloudError::http(status, format!("上游 HTTP {status}: {text}")));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| CloudError::http(502, format!("上游不是 JSON: {e}")))?;
        let code = v.get("code").and_then(|c| {
            c.as_i64()
                .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
        });
        match code {
            Some(c) if c == 0 || c == 200 => Ok(v),
            Some(c) => {
                let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
                Err(CloudError::http(502, format!("上游业务码 {c}: {msg}")))
            }
            None => Err(CloudError::http(502, format!("上游响应缺 code: {text}"))),
        }
    }

    /// 第三步：发起推理，返回已打开的 SSE 连接。
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        conversation_id: i64,
        task_id: i64,
        model_id: &str,
        model_provider: &str,
        query: &str,
        agent_info: &Value,
        tool_results: Option<Vec<Value>>,
        first: bool,
        has_mcp: bool,
    ) -> Result<reqwest::Response, CloudError> {
        let results = match tool_results {
            Some(r) => Value::Array(r),
            // 首轮也必须带非空 toolUseResults（官方客户端的约定，空数组上游会 400）
            None => serde_json::json!([{
                "id": "init",
                "name": "noop",
                "success": true,
                "params": {},
                "message": "ok",
            }]),
        };
        let body = serde_json::json!({
            "username": self.license,
            "ide": "zulucli",
            "ideVersion": self.ide_version,
            "pluginVersion": self.plugin_version,
            "query": query,
            "conversationId": conversation_id,
            "taskId": task_id,
            "modelKey": model_id,
            "modelProvider": model_provider,
            "agentInfo": agent_info,
            "uploadBaseInfo": self.upload_base_info(),
            "contexts": [],
            "subAgents": [],
            "isFirstQuery": first,
            "isUserQuery": first,
            "needCompression": false,
            "toolUseResults": results,
            "hasMcp": has_mcp,
            "agentVersion": AGENT_VERSION,
        });
        let mut body = body;
        // auto-free 没有 modelProvider（真机探针里该字段整个不发送）；空串会被上游当成
        // 「未知 provider」，所以空值直接删掉这个键。
        if model_provider.is_empty() {
            if let Some(m) = body.as_object_mut() {
                m.remove("modelProvider");
            }
        }
        let url = format!("{}/api/aidevops/autocomate/rest/autowork/v2/execute", self.base);
        let mut headers = self.auth_headers();
        headers.insert("Accept", reqwest::header::HeaderValue::from_static("text/event-stream"));
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .body(serde_json::to_string(&body).unwrap_or_default())
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(CloudError::net)?;
        let status = resp.status().as_u16();
        if status != 200 {
            let text = resp.text().await.unwrap_or_default();
            return Err(CloudError::http(status, format!("推理接口 HTTP {status}: {text}")));
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if ctype.contains("application/json") {
            let text = resp.text().await.unwrap_or_default();
            return Err(CloudError::http(
                502,
                format!("推理接口未返回 SSE（{ctype}）: {text}"),
            ));
        }
        Ok(resp)
    }
}

fn insert_str(h: &mut reqwest::header::HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = reqwest::header::HeaderValue::from_str(value) {
        h.insert(name, v);
    }
}

/// 极简 percent-encoding（query 参数里只会出现 license/版本号，仍是安全兜底）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `data` 里的 id 可能是数字也可能是字符串。
fn parse_id(v: &Value, key: &str) -> Option<i64> {
    let d = v.get("data")?.as_object()?;
    let x = d.get(key)?;
    x.as_i64()
        .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
}

/// 设备号：按 license 稳定派生（16 字节 hex，形如官方客户端的 device）。
fn device_for(license: &str) -> String {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x8422_2325_cbf2_9ce4;
    for b in license.bytes() {
        h1 ^= b as u64;
        h1 = h1.wrapping_mul(0x1000_0000_01b3);
        h2 ^= (b as u64).rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15;
        h2 = h2.wrapping_mul(0x1000_0000_01b3).rotate_left(7);
    }
    format!("{h1:016x}{h2:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_answer_and_usage_frames() {
        let f = parse_frame(
            r#"{"sessionId":1,"content":{"type":"ANSWER","detail":{"delta":"云","reasoningDelta":"想","end":true},"end":true},"end":true}"#,
        )
        .unwrap();
        assert_eq!(f.kind, "ANSWER");
        assert_eq!(f.delta, "云");
        assert_eq!(f.reasoning, "想");
        assert!(f.end);

        let f = parse_frame(
            r#"{"content":{"type":"TOKEN_USAGE","detail":{"usage":{"inputTokens":55,"cachedInputTokens":13312,"outputTokens":3}},"end":false},"end":true}"#,
        )
        .unwrap();
        assert_eq!(f.kind, "TOKEN_USAGE");
        let u = f.usage.unwrap();
        assert_eq!(u.prompt_tokens(), 55 + 13312);
        assert_eq!(u.completion_tokens(), 3);
        assert!(f.end);

        // 心跳/未知形状 → None
        assert!(parse_frame(":heartbeat").is_none());
        assert!(parse_frame(r#"{"sessionId":1}"#).is_none());
    }

    #[test]
    fn accumulates_tool_params_by_append() {
        let mut acc = ToolCallAcc::default();
        let mk = |kind: &str, id: &str, input: Value| CloudFrame {
            kind: kind.to_string(),
            tool_use: vec![CloudToolUse {
                id: id.to_string(),
                name: "UseMcpTool".into(),
                input: input.as_object().cloned().unwrap_or_default(),
            }],
            ..Default::default()
        };
        acc.consume("FUNCTION_CALL_START", &mk("", "c1", serde_json::json!({})).tool_use);
        for frag in ["{\"", "city", "\":", " \"", "上海", "\"}"] {
            let f = mk("FUNCTION_CALL_PARAMS_APPEND", "c1", serde_json::json!({"arguments": frag}));
            acc.consume("FUNCTION_CALL_PARAMS_APPEND", &f.tool_use);
        }
        for frag in ["open", "ai", "_client"] {
            let f = mk("FUNCTION_CALL_PARAMS_APPEND", "c1", serde_json::json!({"server_name": frag}));
            acc.consume("FUNCTION_CALL_PARAMS_APPEND", &f.tool_use);
        }
        let f = mk("FUNCTION_CALL_PARAMS_APPEND", "c1", serde_json::json!({"tool_name": "get_weather"}));
        acc.consume("FUNCTION_CALL_PARAMS_APPEND", &f.tool_use);
        assert!(acc.take_finished(false).is_empty(), "没 END 不该出队");
        acc.consume("FUNCTION_CALL_END", &mk("", "c1", serde_json::json!({})).tool_use);
        let done = acc.take_finished(false);
        assert_eq!(done.len(), 1);
        let c = &done[0];
        assert_eq!(c.name, "UseMcpTool");
        assert_eq!(
            c.input.get("server_name").and_then(|v| v.as_str()),
            Some("openai_client")
        );
        assert_eq!(
            c.input.get("arguments").and_then(|v| v.as_str()),
            Some("{\"city\": \"上海\"}")
        );
        assert_eq!(
            c.input.get("tool_name").and_then(|v| v.as_str()),
            Some("get_weather")
        );
    }

    #[test]
    fn device_is_stable_and_long() {
        let a = device_for("lic-1");
        assert_eq!(a, device_for("lic-1"));
        assert_ne!(a, device_for("lic-2"));
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 端到端复刻 2026-11-15 真机那串工具帧（含 reasoning 与空 input 的 START/END）。
    #[test]
    fn real_tool_frame_sequence_yields_complete_call() {
        let frames = [
            r#"{"content":{"type":"ANSWER","detail":{"delta":"","reasoningDelta":"我得查一下","end":false},"end":false},"end":false}"#,
            r#"{"content":{"type":"FUNCTION_CALL_START","detail":{"toolUse":[{"input":{},"name":"UseMcpTool","id":"call_x","type":"tool_use","version":12}]},"end":false},"end":false}"#,
            r#"{"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"arguments":"{\"ci"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#,
            r#"{"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"arguments":"ty\":\"上海\"}"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#,
            r#"{"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"server_name":"openai_client"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#,
            r#"{"content":{"type":"FUNCTION_CALL_PARAMS_APPEND","detail":{"toolUse":[{"input":{"tool_name":"get_weather"},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#,
            r#"{"content":{"type":"FUNCTION_CALL_END","detail":{"toolUse":[{"input":{},"name":"UseMcpTool","id":"call_x","type":"tool_use"}]},"end":false},"end":false}"#,
        ];
        let mut acc = ToolCallAcc::default();
        for f in frames {
            let parsed = parse_frame(f).expect(f);
            acc.consume(&parsed.kind, &parsed.tool_use);
        }
        let done = acc.take_finished(true);
        assert_eq!(done.len(), 1, "calls={done:?}");
        let c = &done[0];
        assert_eq!(c.id, "call_x");
        assert_eq!(c.input.get("server_name").and_then(|v| v.as_str()), Some("openai_client"));
        assert_eq!(c.input.get("tool_name").and_then(|v| v.as_str()), Some("get_weather"));
        assert_eq!(
            c.input.get("arguments").and_then(|v| v.as_str()),
            Some("{\"city\":\"上海\"}")
        );
    }

    #[test]
    fn trace_id_is_uuid_shaped_and_unique() {
        let a = trace_id();
        let b = trace_id();
        assert_eq!(a.len(), 36, "{a}");
        assert_ne!(a, b, "两次取值不能相同");
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{a}"
        );
        assert!(a.chars().all(|c| c == '-' || c.is_ascii_hexdigit()), "{a}");
    }

    /// 2026-11-21：`plugin-version` 是 `zulucli-<IDE 扩展版本>`，不是 `@comate/zulu` 包版本。
    #[test]
    fn plugin_version_has_zulucli_prefix() {
        let v = resolve_plugin_version();
        assert!(v.starts_with("zulucli-"), "{v}");
        let ver = v.strip_prefix("zulucli-").unwrap_or("");
        assert!(
            !ver.is_empty() && ver.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false),
            "{v}"
        );
    }

}