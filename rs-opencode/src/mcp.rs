//! mcp.rs —— 每轮临时的本地 MCP 桥（客户端 `tools[]` → 真 `tool_calls`）。
//!
//! 链路（见 `.work/current-task-mcp-bridge.md`）：
//!
//! ```text
//! 客户端带 tools 的请求
//!   → 临时目录写 tools.json（sanitized 名 ↔ 客户端原名）
//!   → PUT /api/experimental/mcp/pd-<id>   {"config":{type:"local",command:[...],codemode:false}}
//!   → opencode 拉起 `rs-opencode --plugin-opencode --mcp-bridge <dir>`
//!   → 发 prompt（文本仍按老逻辑压平）
//!   → 事件流里 session.tool.called + session.tool.progress(metadata.toolCalls[].tool = "pd-<id>.<tool>")
//!   → 收 1500ms 窗口 → interrupt / delete session / delete MCP → 清目录
//!   → OpenAI `finish_reason:"tool_calls"`
//! ```
//!
//! 设计边界：
//! * **无状态多轮**：不跨下游 HTTP 挂起/恢复。客户端下一轮把 `role:"tool"` 结果带回来时
//!   继续走老的压平路径（`[tool result for <id>]`）。
//! * **安全**：MCP server 只暴露这一轮客户端给的 tools，`tools/call` 永不执行、只落盘挂起；
//!   `permission.asked` 仍由 lib.rs 固定 reject。
//! * **失败闭合**：注册/连接/识别任一步失败都退回纯文本，不把请求挂死。
//! * **不写日志内容**：工具名/参数/描述一律不进日志（只记数量与状态）。
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::Method;
use serde_json::{json, Value};

use crate::translate;
use crate::upstream::UpError;

/// 首次识别到外部工具调用后，等并行调用的收集窗口。
pub const TOOL_COLLECT_WINDOW: Duration = Duration::from_millis(1500);
/// PUT 注册后等 `GET /api/mcp` 变 connected 的上限。
pub const MCP_SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// 单次 MCP 管理请求（PUT/GET/DELETE/interrupt）的超时。
pub const MCP_TIMEOUT: Duration = Duration::from_secs(10);
/// 连接状态轮询间隔。
pub const MCP_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 子进程绝对超时默认值（简报：120s 或随上游 deadline）。
pub const DEFAULT_CHILD_TIMEOUT_MS: u64 = 120_000;
/// 单轮最多桥接多少个工具（防御异常客户端一次塞几万个 schema）。
pub const MAX_TOOLS: usize = 128;
/// OpenAI / MCP 工具名长度上限（OpenAI 是 64）。
pub const MAX_TOOL_NAME: usize = 64;
/// 描述截断长度（只影响 tools/list 的体积，不影响参数 schema）。
pub const MAX_DESCRIPTION_CHARS: usize = 4096;

/// 临时目录根：`%TEMP%\ProxyDeck\opencode-mcp\<bridge_id>\`。
pub fn bridge_root() -> PathBuf {
    std::env::temp_dir().join("ProxyDeck").join("opencode-mcp")
}

// ------------------------------ 最小 HTTP 客户端（MCP 管理端点） ------------------------------
//
// 刻意不扩 `upstream.rs`：交付清单只允许 mcp*.rs / lib.rs / translate.rs。
// 这里只打本机回环，复用 `Upstream` 的 base/password 公共字段，错误仍统一成 `UpError`。

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn opencode_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn with_auth(req: reqwest::RequestBuilder, password: &str) -> reqwest::RequestBuilder {
    if password.is_empty() {
        req
    } else {
        req.basic_auth("opencode", Some(password))
    }
}

/// 发一个 JSON 请求；非 2xx 转 `UpError`（只有 MCP 端点用，不需要 429 细分）。
async fn mcp_http(
    method: Method,
    base: &str,
    password: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value, UpError> {
    let mut req = http_client()
        .request(method, opencode_url(base, path))
        .timeout(timeout)
        .header(reqwest::header::ACCEPT, "application/json");
    req = with_auth(req, password);
    if let Some(body) = body {
        let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
        req = req
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(bytes);
    }
    let resp = req.send().await.map_err(UpError::network)?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(UpError::network)?;
    let value: Value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}))
    };
    if !(200..300).contains(&status) {
        let message = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str))
            .unwrap_or("opencode MCP request failed")
            .to_string();
        return Err(UpError::http(status, message));
    }
    Ok(value)
}

/// `DELETE /api/experimental/mcp/{name}`。
async fn delete_mcp(base: &str, password: &str, name: &str) -> Result<(), UpError> {
    mcp_http(
        Method::DELETE,
        base,
        password,
        &format!("/api/experimental/mcp/{name}"),
        None,
        MCP_TIMEOUT,
    )
    .await?;
    Ok(())
}

/// `POST /api/experimental/mcp/{name}/disconnect`（DELETE 失败时的退路）。
async fn disconnect_mcp(base: &str, password: &str, name: &str) -> Result<(), UpError> {
    mcp_http(
        Method::POST,
        base,
        password,
        &format!("/api/experimental/mcp/{name}/disconnect"),
        None,
        MCP_TIMEOUT,
    )
    .await?;
    Ok(())
}

/// `POST /api/session/{id}/interrupt`：打断当前执行。
pub async fn interrupt_session(
    base: &str,
    password: &str,
    session_id: &str,
) -> Result<Value, UpError> {
    mcp_http(
        Method::POST,
        base,
        password,
        &format!("/api/session/{session_id}/interrupt"),
        None,
        MCP_TIMEOUT,
    )
    .await
}

/// 轮询 `GET /api/mcp` 直到指定 server `connected`（或 failed/超时）。
async fn wait_mcp_connected(
    base: &str,
    password: &str,
    name: &str,
    timeout: Duration,
) -> Result<(), UpError> {
    let started = std::time::Instant::now();
    let mut last_error: Option<UpError> = None;
    loop {
        match mcp_http(Method::GET, base, password, "/api/mcp", None, MCP_TIMEOUT).await {
            Ok(body) => match mcp_status(&body, name) {
                Some((kind, _)) if kind == "connected" => return Ok(()),
                Some((kind, err))
                    if kind == "failed" || kind == "needs_auth" || kind == "disabled" =>
                {
                    let suffix = err.map(|e| format!(": {e}")).unwrap_or_default();
                    return Err(UpError::http(
                        502,
                        format!("MCP server {name} status {kind}{suffix}"),
                    ));
                }
                _ => {}
            },
            Err(e) => {
                if e.status == Some(404) {
                    return Err(e);
                }
                last_error = Some(e);
            }
        }
        if started.elapsed() >= timeout {
            let message = last_error
                .map(|e| e.message)
                .unwrap_or_else(|| format!("MCP server {name} did not become connected"));
            return Err(UpError::http(502, message));
        }
        tokio::time::sleep(MCP_POLL_INTERVAL).await;
    }
}

/// 从 `GET /api/mcp` 的响应里取指定 server 的 `(status, error)`。
///
/// 兼容三种形状：`{"data":[...]}`、裸数组、`{"servers":[...]}`。
fn mcp_status(body: &Value, name: &str) -> Option<(String, Option<String>)> {
    let arr = body
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| body.get("servers").and_then(Value::as_array))
        .or_else(|| body.pointer("/data/servers").and_then(Value::as_array))
        .or_else(|| body.as_array())?;
    for server in arr {
        if server.get("name").and_then(Value::as_str) != Some(name) {
            continue;
        }
        let status = server.get("status")?;
        let kind = status
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                status
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        let err = status
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                server
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        return Some((kind, err));
    }
    None
}

/// 一个客户端工具（已经过清洗）。
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeTool {
    /// MCP 工具名（`[A-Za-z0-9_-]+`，最多 64 字符，桥内唯一）。
    pub name: String,
    /// 客户端原始名（回填到 OpenAI `function.name`）。
    pub original: String,
    pub description: String,
    /// MCP `inputSchema`（原样转发客户端 `function.parameters`）。
    pub parameters: Value,
}

/// 解析入站 `tools[]`；只收 `{"type":"function","function":{name,...}}`。
///
/// 非法项直接跳过（不报 400）：工具是增强能力，不应让整个对话失败。
pub fn parse_tools(value: Option<&Value>) -> Vec<BridgeTool> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if out.len() >= MAX_TOOLS {
            break;
        }
        let ty = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function");
        if ty != "function" {
            continue;
        }
        let Some(func) = item.get("function") else {
            continue;
        };
        let Some(raw_name) = func
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let description: String = func
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .chars()
            .take(MAX_DESCRIPTION_CHARS)
            .collect();
        let parameters = func
            .get("parameters")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        let sanitized = unique_name(&sanitize_name(raw_name), &mut used);
        out.push(BridgeTool {
            name: sanitized,
            original: raw_name.to_string(),
            description,
            parameters,
        });
    }
    out
}

/// OpenAI 工具名 → MCP 工具名：非 `[A-Za-z0-9_-]` 一律换成 `_`，空名兜底 `tool`。
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if out.len() >= MAX_TOOL_NAME {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("tool");
    }
    out
}

/// 桥内去重：冲突时追加 `_2`、`_3`…并保持 ≤64 字符。
fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    for i in 2..10_000u32 {
        let suffix = format!("_{i}");
        let keep = MAX_TOOL_NAME.saturating_sub(suffix.len());
        let mut candidate: String = base.chars().take(keep).collect();
        candidate.push_str(&suffix);
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    // 极端兜底：64 位以内几乎不可能走到这里。
    let fallback = format!("tool_{}", used.len());
    used.insert(fallback.clone());
    fallback
}

/// 一轮的桥：目录、工具表、注册名。
pub struct Bridge {
    id: String,
    name: String,
    dir: PathBuf,
    tools: Vec<BridgeTool>,
    mapping: HashMap<String, String>,
    timeout_ms: u64,
}

impl Bridge {
    /// 建临时目录 + 写 `tools.json`。`turn_timeout` 用于把子进程绝对超时
    /// 夹在单轮预算内（默认 120s，且不小于 1s）。
    pub fn create(tools: Vec<BridgeTool>, turn_timeout: Duration) -> Result<Bridge, String> {
        if tools.is_empty() {
            return Err("没有可桥接的工具".to_string());
        }
        let id = crate::random_hex(8);
        let name = format!("pd-{id}");
        let dir = bridge_root().join(&id);
        std::fs::create_dir_all(&dir).map_err(|e| format!("建桥目录失败: {e}"))?;

        let timeout_ms = DEFAULT_CHILD_TIMEOUT_MS
            .min(turn_timeout.as_millis().max(1) as u64)
            .max(1_000);
        let payload = json!({
            "version": 1,
            "timeout_ms": timeout_ms,
            // 客户端原始 tools 内容只落在这里；不写日志、不进错误消息。
            "tools": tools.iter().map(|t| json!({
                "name": t.name,
                "original": t.original,
                "description": t.description,
                "inputSchema": t.parameters,
            })).collect::<Vec<_>>(),
        });
        let text =
            serde_json::to_string(&payload).map_err(|e| format!("序列化 tools.json 失败: {e}"))?;
        if let Err(e) = std::fs::write(dir.join("tools.json"), text) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("写 tools.json 失败: {e}"));
        }
        let mapping = tools
            .iter()
            .map(|t| (t.name.clone(), t.original.clone()))
            .collect();
        Ok(Bridge {
            id,
            name,
            dir,
            tools,
            mapping,
            timeout_ms,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// opencode `Mcp.LocalConfig`。`codemode:false` 是硬要求：默认 true 时调用会被
    /// 包成 `execute` 的 JS 字符串，参数不再干净。
    ///
    /// 用 `--plugin-opencode` 一起传只是为了复用现有 `main.rs` 的参数门（不额外改
    /// `main.rs`）；`run()` 看到 `--mcp-bridge` 会在加载配置之前就切到 stdio 模式。
    pub fn config(&self, exe: &Path) -> Value {
        json!({
            "type": "local",
            "command": [
                exe.to_string_lossy(),
                "--plugin-opencode",
                "--mcp-bridge",
                self.dir.to_string_lossy(),
            ],
            "codemode": false,
        })
    }

    /// 事件关联状态（需要 prefix + 回填映射）。
    pub fn runtime(&self) -> BridgeRuntime {
        BridgeRuntime::new(
            format!("{}.", self.name),
            // permission.asked 的 action 用 `<server>_<tool>`（下划线），不是点。
            format!("{}_", self.name),
            self.mapping.clone(),
            TOOL_COLLECT_WINDOW,
        )
    }

    /// 注册 + 等 connected；等待失败会顺手注销，避免留下半连接。
    pub async fn register(&self, base: &str, password: &str, exe: &Path) -> Result<(), UpError> {
        mcp_http(
            Method::PUT,
            base,
            password,
            &format!("/api/experimental/mcp/{}", self.name),
            Some(&json!({"config": self.config(exe)})),
            MCP_TIMEOUT,
        )
        .await?;
        match wait_mcp_connected(base, password, &self.name, MCP_SETUP_TIMEOUT).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = delete_mcp(base, password, &self.name).await;
                Err(e)
            }
        }
    }

    /// 注销（DELETE；404 当已删除）。其它失败再试 disconnect，最后只记状态。
    pub async fn unregister(&self, base: &str, password: &str) {
        match delete_mcp(base, password, &self.name).await {
            Ok(()) => {}
            Err(e) if e.status == Some(404) => {}
            Err(e) => {
                crate::logf(&format!(
                    "工具桥接：注销 {} 失败（{}），尝试 disconnect",
                    self.name, e.message
                ));
                if let Err(e2) = disconnect_mcp(base, password, &self.name).await {
                    crate::logf(&format!(
                        "工具桥接：disconnect {} 也失败（{}）",
                        self.name, e2.message
                    ));
                }
            }
        }
    }

    /// 清目录。先落 `cancel` 给挂起的 tools/call 一个尽快退出的信号，再删整目录。
    /// 幂等；失败只可能是子进程仍持有文件句柄，`Drop` 不会再重试（避免无限递归）。
    pub fn remove_dir(&self) {
        let _ = std::fs::write(self.dir.join("cancel"), b"1");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = std::fs::write(self.dir.join("cancel"), b"1");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 一条已识别为“外部桥接”的工具调用（回填名 + 参数对象）。
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeCall {
    pub opencode_id: String,
    /// 客户端原始工具名。
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Default)]
struct CallState {
    /// `session.tool.called.data.input`。
    input: Value,
    /// `session.tool.called.data.executed`（None = 还没见到 called）。
    executed: Option<bool>,
    /// `session.tool.input.started.data.name`（可能是裸名或 `pd-<id>.<tool>`）。
    name_hint: Option<String>,
    /// `session.tool.progress` / `permission.asked` 里去掉 server 前缀后的工具名。
    progress_tool: Option<String>,
    /// 是否在 progress metadata / permission action 里见过本桥 server 前缀。
    server_seen: bool,
}

/// 事件流里的工具调用关联器（每轮一个，不跨请求）。
#[derive(Debug, Clone)]
pub struct BridgeRuntime {
    /// progress metadata 里的 `<server>.` 前缀。
    prefix: String,
    /// permission action 里的 `<server>_` 前缀。
    action_prefix: String,
    mapping: HashMap<String, String>,
    calls: HashMap<String, CallState>,
    order: Vec<String>,
    window: Duration,
}

impl BridgeRuntime {
    pub fn new(
        prefix: String,
        action_prefix: String,
        mapping: HashMap<String, String>,
        window: Duration,
    ) -> Self {
        Self {
            prefix,
            action_prefix,
            mapping,
            calls: HashMap::new(),
            order: Vec::new(),
            window,
        }
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// 吃一条事件；返回 true 表示“本轮第一次出现可返回的外部调用”。
    ///
    /// 只做状态记账，不决定何时打断（打断时机由 `drive_turn` 的收集窗控制）。
    pub fn observe(&mut self, ev: &translate::Event) -> bool {
        let was_ready = self.has_ready_calls();
        match ev.ty.as_str() {
            "session.tool.input.started" => {
                if let Some((id, name)) = translate::tool_input_name(ev) {
                    // 只记名字候选；是否属于桥接 server 必须由 progress metadata
                    // 或 permission action 证明（简报要求 called + 第二信号齐了才算）。
                    let st = self.entry(&id);
                    st.name_hint = Some(name);
                }
            }
            "session.tool.called" => {
                if let Some(called) = translate::tool_called(ev) {
                    let st = self.entry(&called.id);
                    // called 通常带 input；缺失时不要覆盖 permission/progress 先拿到的参数。
                    if !called.input.is_null() || st.input.is_null() {
                        st.input = called.input;
                    }
                    st.executed = Some(called.executed);
                }
            }
            "session.tool.progress" => {
                for p in translate::tool_progress_entries(ev, &self.prefix) {
                    let st = self.entry(&p.call_id);
                    st.server_seen = true;
                    st.progress_tool = Some(p.tool);
                    if st.input.is_null() {
                        if let Some(input) = p.input {
                            st.input = input;
                        }
                    }
                }
            }
            // 第二信号（兼容不产生 tool.progress 的 opencode 版本）：
            // permission.asked 的 `source.id` 就是 tool call id，`action` 是
            // `<server>_<sanitized_tool>`。只有前缀/映射都对上才算外部调用。
            "permission.asked" => {
                let call_id = ev
                    .data
                    .pointer("/source/id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let action = ev.data.get("action").and_then(Value::as_str);
                if let (Some(call_id), Some(action)) = (call_id, action) {
                    let matched = action
                        .strip_prefix(&self.action_prefix)
                        .filter(|tool| self.mapping.contains_key(*tool))
                        .map(str::to_string);
                    if let Some(tool) = matched {
                        let input = ev
                            .data
                            .pointer("/metadata/input")
                            .cloned()
                            .filter(|v| !v.is_null());
                        let st = self.entry(call_id);
                        st.server_seen = true;
                        st.progress_tool = Some(tool);
                        if st.input.is_null() {
                            if let Some(input) = input {
                                st.input = input;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        !was_ready && self.has_ready_calls()
    }

    fn entry(&mut self, id: &str) -> &mut CallState {
        if !self.calls.contains_key(id) {
            self.calls.insert(id.to_string(), CallState::default());
            self.order.push(id.to_string());
        }
        self.calls.get_mut(id).expect("刚插入的 call 状态")
    }

    /// 是否已经有一条“外部桥接 + executed:false + 名字能回填”的调用。
    pub fn has_ready_calls(&self) -> bool {
        self.order.iter().any(|id| {
            self.calls
                .get(id)
                .map(|st| self.is_ready(st))
                .unwrap_or(false)
        })
    }

    fn is_ready(&self, st: &CallState) -> bool {
        // executed:true 说明上游已经执行过（或不在等待我们回填），不能当外部调用。
        if st.executed != Some(false) {
            return false;
        }
        if !st.server_seen {
            return false;
        }
        self.resolve_name(st).is_some()
    }

    fn resolve_name(&self, st: &CallState) -> Option<String> {
        if let Some(tool) = &st.progress_tool {
            if let Some(original) = self.mapping.get(tool) {
                return Some(original.clone());
            }
        }
        if let Some(hint) = &st.name_hint {
            let suffix = hint.strip_prefix(&self.prefix).unwrap_or(hint);
            if let Some(original) = self.mapping.get(suffix) {
                return Some(original.clone());
            }
        }
        None
    }

    /// 按模型发出顺序返回所有可回填的调用。
    pub fn ready_calls(&self) -> Vec<BridgeCall> {
        let mut out = Vec::new();
        for id in &self.order {
            let Some(st) = self.calls.get(id) else {
                continue;
            };
            if !self.is_ready(st) {
                continue;
            }
            let Some(name) = self.resolve_name(st) else {
                continue;
            };
            let arguments = if st.input.is_null() {
                json!({})
            } else {
                st.input.clone()
            };
            out.push(BridgeCall {
                opencode_id: id.clone(),
                name,
                arguments,
            });
        }
        out
    }

    /// 测试/诊断：当前已知调用数。
    pub fn observed_calls(&self) -> usize {
        self.order.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::Event;

    fn ev(ty: &str, data: Value) -> Event {
        Event {
            ty: ty.to_string(),
            data,
        }
    }

    fn runtime() -> BridgeRuntime {
        let mut mapping = HashMap::new();
        mapping.insert("get_weather".to_string(), "get weather!".to_string());
        mapping.insert("second_tool".to_string(), "second.tool".to_string());
        BridgeRuntime::new(
            "pd-abc.".to_string(),
            "pd-abc_".to_string(),
            mapping,
            Duration::from_millis(1500),
        )
    }

    #[test]
    fn sanitize_turns_illegal_chars_into_underscores() {
        assert_eq!(sanitize_name("get weather!"), "get_weather_");
        assert_eq!(sanitize_name("a.b/c:d"), "a_b_c_d");
        assert_eq!(sanitize_name("   "), "___");
        assert_eq!(sanitize_name(""), "tool");
        assert_eq!(sanitize_name("天气"), "__");
        let long = "x".repeat(200);
        assert_eq!(sanitize_name(&long).len(), MAX_TOOL_NAME);
    }

    #[test]
    fn parse_tools_filters_bad_entries_and_dedupes_names() {
        let tools = json!([
            {"type":"function","function":{"name":"get weather!","description":"d","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}},
            {"type":"function","function":{"name":"get_weather_","description":"dup"}},
            {"type":"retrieval","function":{"name":"nope"}},
            {"type":"function"},
            {"type":"function","function":{"name":"   "}},
            {"type":"function","function":{"name":"a.b","parameters":"not-an-object"}},
            "junk"
        ]);
        let parsed = parse_tools(Some(&tools));
        let names: Vec<&str> = parsed.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["get_weather_", "get_weather__2", "a_b"]);
        assert_eq!(parsed[0].original, "get weather!");
        assert_eq!(parsed[0].parameters["properties"]["city"]["type"], "string");
        assert_eq!(
            parsed[2].parameters,
            json!({"type":"object","properties":{}}),
            "非法 parameters 要退成空 object schema"
        );
        assert!(parse_tools(None).is_empty());
        assert!(parse_tools(Some(&json!({}))).is_empty());
    }

    #[test]
    fn called_plus_progress_is_ready_and_maps_back_original_name() {
        let mut r = runtime();
        assert!(!r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc1","input":{"city":"Beijing"},"executed":false})
        )));
        assert!(r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc1","metadata":{"toolCalls":[
                {"tool":"pd-abc.get_weather","status":"running","input":{"city":"Beijing"}}
            ]}})
        )));
        let calls = r.ready_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].opencode_id, "tc1");
        assert_eq!(calls[0].name, "get weather!");
        assert_eq!(calls[0].arguments["city"], "Beijing");
    }

    #[test]
    fn progress_before_called_also_works() {
        let mut r = runtime();
        r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc1","metadata":{"toolCalls":[{"tool":"pd-abc.second_tool"}]}}),
        ));
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc1","input":{"x":1},"executed":false}),
        ));
        let calls = r.ready_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "second.tool");
    }

    #[test]
    fn input_started_name_alone_is_not_enough() {
        let mut r = runtime();
        r.observe(&ev(
            "session.tool.input.started",
            json!({"id":"tc1","name":"pd-abc.get_weather"}),
        ));
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc1","input":{},"executed":false}),
        ));
        assert!(
            r.ready_calls().is_empty(),
            "没有 progress 的 server 前缀时必须 fail-closed"
        );
        r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc1","metadata":{"toolCalls":[{"tool":"pd-abc.get_weather"}]}}),
        ));
        assert_eq!(r.ready_calls().len(), 1);
    }

    #[test]
    fn executed_true_or_wrong_prefix_or_unknown_tool_is_not_ready() {
        let mut r = runtime();
        // executed:true
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc1","input":{},"executed":true}),
        ));
        r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc1","metadata":{"toolCalls":[{"tool":"pd-abc.get_weather"}]}}),
        ));
        // 别人的 server 前缀
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc2","input":{},"executed":false}),
        ));
        r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc2","metadata":{"toolCalls":[{"tool":"pd-other.get_weather"}]}}),
        ));
        // 前缀对但工具不在映射里
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc3","input":{},"executed":false}),
        ));
        r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc3","metadata":{"toolCalls":[{"tool":"pd-abc.unknown_tool"}]}}),
        ));
        assert!(r.ready_calls().is_empty());
    }

    #[test]
    fn permission_action_can_identify_external_call_without_progress() {
        let mut r = runtime();
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc1","executed":false}),
        ));
        assert!(r.observe(&ev(
            "permission.asked",
            json!({
                "sessionID":"s",
                "id":"perm_1",
                "action":"pd-abc_get_weather",
                "resources":["*"],
                "source":{"type":"tool","messageID":"msg_1","id":"tc1"},
                "metadata":{"input":{"city":"Beijing"}}
            }),
        )));
        let calls = r.ready_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get weather!");
        assert_eq!(calls[0].arguments["city"], "Beijing");
    }

    #[test]
    fn permission_for_other_server_or_wrong_action_is_not_ready() {
        let mut r = runtime();
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc1","input":{},"executed":false}),
        ));
        r.observe(&ev(
            "permission.asked",
            json!({
                "id":"perm_1",
                "action":"other_get_weather",
                "source":{"type":"tool","id":"tc1"}
            }),
        ));
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc2","input":{},"executed":false}),
        ));
        r.observe(&ev(
            "permission.asked",
            json!({
                "id":"perm_2",
                "action":"pd-abc_unknown_tool",
                "source":{"type":"tool","id":"tc2"}
            }),
        ));
        assert!(r.ready_calls().is_empty());
    }

    #[test]
    fn parallel_calls_keep_emission_order_and_unique_ids_are_made_later() {
        let mut r = runtime();
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc1","input":{"a":1},"executed":false}),
        ));
        r.observe(&ev(
            "session.tool.called",
            json!({"id":"tc2","input":{"b":2},"executed":false}),
        ));
        r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc2","metadata":{"toolCalls":[{"tool":"pd-abc.second_tool"}]}}),
        ));
        r.observe(&ev(
            "session.tool.progress",
            json!({"id":"tc1","metadata":{"toolCalls":[{"tool":"pd-abc.get_weather"}]}}),
        ));
        let calls = r.ready_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].opencode_id, "tc1");
        assert_eq!(calls[1].opencode_id, "tc2");
    }

    #[test]
    fn bridge_create_writes_tools_json_and_drop_removes_directory() {
        let tools = parse_tools(Some(&json!([
            {"type":"function","function":{"name":"get weather!","description":"d","parameters":{"type":"object"}}}
        ])));
        let b = Bridge::create(tools, Duration::from_secs(300)).unwrap();
        let dir = b.dir().to_path_buf();
        let text = std::fs::read_to_string(dir.join("tools.json")).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["tools"][0]["name"], "get_weather_");
        assert_eq!(v["tools"][0]["original"], "get weather!");
        assert_eq!(v["timeout_ms"], 120_000, "单轮 300s 时仍按 120s 封顶");
        assert!(b.name().starts_with("pd-"));
        assert!(b.config(Path::new("C:\\fake\\rs-opencode.exe"))["codemode"] == false);
        drop(b);
        assert!(!dir.exists(), "Drop 必须清掉桥目录");
    }

    #[test]
    fn mcp_status_parses_data_array_and_failed_error() {
        let body = json!({"data":[
            {"name":"pd-a","status":{"status":"pending"}},
            {"name":"pd-b","status":{"status":"connected"}},
            {"name":"pd-c","status":{"status":"failed","error":"spawn failed"}}
        ]});
        assert_eq!(
            mcp_status(&body, "pd-a"),
            Some(("pending".to_string(), None))
        );
        assert_eq!(
            mcp_status(&body, "pd-b"),
            Some(("connected".to_string(), None))
        );
        assert_eq!(
            mcp_status(&body, "pd-c"),
            Some(("failed".to_string(), Some("spawn failed".to_string())))
        );
        assert_eq!(mcp_status(&body, "nope"), None);
        assert_eq!(
            mcp_status(&json!([{"name":"x","status":{"status":"connected"}}]), "x"),
            Some(("connected".to_string(), None))
        );
        assert_eq!(
            mcp_status(
                &json!({"servers":[{"name":"y","status":{"status":"pending"}}]}),
                "y"
            ),
            Some(("pending".to_string(), None))
        );
        // 某些版本把 status 直接写成字符串
        assert_eq!(
            mcp_status(&json!([{"name":"z","status":"connected"}]), "z"),
            Some(("connected".to_string(), None))
        );
    }
}
