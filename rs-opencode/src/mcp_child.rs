//! mcp_child.rs —— `rs-opencode --mcp-bridge <dir>` 的 stdio MCP 子进程。
//!
//! 由 opencode 通过 `PUT /api/experimental/mcp/pd-<id>` 的 `command` 拉起，只做三件事：
//! 1. `initialize` / `notifications/initialized`；
//! 2. `tools/list` —— 返回 `tools.json` 里本轮客户端给的工具（名字已清洗）；
//! 3. `tools/call` —— **落盘 `calls.jsonl` 后挂起**，等 `results/<id>.json` / `cancel`
//!    文件 / 绝对超时；绝不执行任何工具。
//!
//! 硬约束：
//! * stdout 只能出现 JSON-RPC 行（`logf` 会 println，所以 `run()` 在桥接模式**不打印**）；
//! * 绝对超时（默认 120s / 随上游 deadline）到点必退，不常驻；
//! * `tools.json` 缺失或非法直接退出非 0，不静默空转；
//! * 工具名/参数不写日志（只落 `calls.jsonl`，目录整轮结束就删）。
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// 默认绝对超时（与 `mcp::DEFAULT_CHILD_TIMEOUT_MS` 同值；这里独立一份避免循环依赖）。
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MIN_TIMEOUT_MS: u64 = 50;
const MAX_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// 等结果文件/取消文件的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// 支持的 MCP 协议版本（opencode legacy 会发 2025-06-18 或 2025-11-25）。
const PROTOCOL_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

#[derive(Debug, Clone)]
struct ToolDef {
    name: String,
    description: String,
    input_schema: Value,
}

struct BridgeConfig {
    dir: PathBuf,
    tools: Vec<ToolDef>,
    timeout: Duration,
}

/// 进程入口：读桥目录 → 起 stdin 读线程 → 跑协议循环。返回进程退出码。
pub fn serve(dir: &Path) -> i32 {
    let cfg = match load_config(dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("opencode-mcp-bridge: {e}");
            return 2;
        }
    };
    let deadline = Instant::now() + cfg.timeout;
    let (tx, rx) = mpsc::channel::<String>();
    // stdin 用独立线程读：主循环才能用 recv_timeout 同时盯绝对超时。
    // 线程随进程退出被系统回收；正常 EOF 时主循环返回 0。
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match lock.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf)
                        .trim_end_matches(['\r', '\n'])
                        .to_string();
                    if !line.is_empty() && tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serve_loop(&cfg, &rx, &mut out, deadline)
}

/// 协议主循环（抽出来便于单测用内存 reader/writer 覆盖）。
fn serve_loop<W: Write>(
    cfg: &BridgeConfig,
    rx: &Receiver<String>,
    out: &mut W,
    deadline: Instant,
) -> i32 {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return 2;
        }
        let wait = (deadline - now).min(Duration::from_secs(1));
        match rx.recv_timeout(wait) {
            Ok(line) => match handle_line(cfg, rx, out, deadline, &line) {
                Flow::Continue => {}
                Flow::Exit(code) => return code,
            },
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return 0,
        }
    }
}

enum Flow {
    Continue,
    Exit(i32),
}

fn handle_line<W: Write>(
    cfg: &BridgeConfig,
    rx: &Receiver<String>,
    out: &mut W,
    deadline: Instant,
    line: &str,
) -> Flow {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            let _ = write_message(
                out,
                &json!({"jsonrpc":"2.0","id":Value::Null,"error":{"code":-32700,"message":"parse error"}}),
            );
            return Flow::Continue;
        }
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let has_id = msg.get("id").is_some();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            let version = if PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                "2025-06-18"
            };
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "proxydeck-opencode-bridge", "version": "0.1.0"},
                }
            });
            if !write_message(out, &resp) {
                return Flow::Exit(0);
            }
            Flow::Continue
        }
        // 通知没有 id，不回应。
        "notifications/initialized" | "notifications/cancelled" | "notifications/progress" => {
            Flow::Continue
        }
        "tools/list" => {
            let tools: Vec<Value> = cfg
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })
                })
                .collect();
            let resp = json!({"jsonrpc":"2.0","id":id,"result":{"tools":tools}});
            if !write_message(out, &resp) {
                return Flow::Exit(0);
            }
            Flow::Continue
        }
        "tools/call" => match handle_call(cfg, rx, out, deadline, &id, &params) {
            Flow::Continue => Flow::Continue,
            Flow::Exit(code) => Flow::Exit(code),
        },
        "ping" if has_id => {
            let resp = json!({"jsonrpc":"2.0","id":id,"result":{}});
            if !write_message(out, &resp) {
                return Flow::Exit(0);
            }
            Flow::Continue
        }
        _ if has_id => {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "method not found"},
            });
            if !write_message(out, &resp) {
                return Flow::Exit(0);
            }
            Flow::Continue
        }
        // 未知通知：忽略。
        _ => Flow::Continue,
    }
}

fn handle_call<W: Write>(
    cfg: &BridgeConfig,
    rx: &Receiver<String>,
    out: &mut W,
    deadline: Instant,
    request_id: &Value,
    params: &Value,
) -> Flow {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(tool) = cfg.tools.iter().find(|t| t.name == name) else {
        let resp = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": -32602, "message": "unknown tool"},
        });
        return if write_message(out, &resp) {
            Flow::Continue
        } else {
            Flow::Exit(0)
        };
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let call_id = id_key(request_id);
    append_call(&cfg.dir, &call_id, &tool.name, &arguments);

    loop {
        if Instant::now() >= deadline {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": "bridge timeout"},
            });
            let _ = write_message(out, &resp);
            return Flow::Exit(2);
        }
        if let Some(result) = read_result(&cfg.dir, &call_id) {
            let resp = json!({"jsonrpc":"2.0","id":request_id,"result":result});
            return if write_message(out, &resp) {
                Flow::Continue
            } else {
                Flow::Exit(0)
            };
        }
        if cfg.dir.join("cancel").exists() {
            let resp = json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": "cancelled"},
            });
            let _ = write_message(out, &resp);
            return Flow::Exit(0);
        }
        // 挂起期间只看取消类消息；其它请求（并发 tools/call）不排队，直接忽略。
        loop {
            match rx.try_recv() {
                Ok(line) => {
                    if is_cancelled_request(&line, request_id) {
                        let resp = json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {"code": -32000, "message": "cancelled"},
                        });
                        let _ = write_message(out, &resp);
                        return Flow::Exit(0);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Flow::Exit(0),
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// `notifications/cancelled` 的 `params.requestId` 是否就是当前挂起的请求。
fn is_cancelled_request(line: &str, request_id: &Value) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if v.get("method").and_then(Value::as_str) != Some("notifications/cancelled") {
        return false;
    }
    v.pointer("/params/requestId") == Some(request_id)
}

fn write_message<W: Write>(out: &mut W, value: &Value) -> bool {
    let Ok(text) = serde_json::to_string(value) else {
        return false;
    };
    writeln!(out, "{text}").and_then(|_| out.flush()).is_ok()
}

/// JSON-RPC id → 文件名安全 key（只留 `[A-Za-z0-9_-]`，最长 64）。
pub fn result_file_name(id: &str) -> String {
    let mut out = String::new();
    for ch in id.chars() {
        if out.len() >= 64 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("call");
    }
    out
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) if !s.is_empty() => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => format!("call_{}", now_ms()),
    }
}

fn read_result(dir: &Path, call_id: &str) -> Option<Value> {
    let path = dir
        .join("results")
        .join(format!("{}.json", result_file_name(call_id)));
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(normalize_result(value))
}

/// `results/<id>.json` 既可以是完整 MCP result（带 content/isError），
/// 也可以是任意 JSON —— 后者包成一条 text content。
fn normalize_result(value: Value) -> Value {
    match &value {
        Value::Object(map) if matches!(map.get("content"), Some(Value::Array(_))) => {
            let mut out = value;
            if out.get("isError").is_none() {
                out["isError"] = Value::Bool(false);
            }
            out
        }
        Value::String(s) => json!({
            "content": [{"type": "text", "text": s}],
            "isError": false,
        }),
        _ => {
            let text = serde_json::to_string(&value).unwrap_or_default();
            json!({
                "content": [{"type": "text", "text": text}],
                "isError": false,
            })
        }
    }
}

fn append_call(dir: &Path, id: &str, name: &str, arguments: &Value) {
    let line = json!({
        "id": id,
        "name": name,
        "arguments": arguments,
        "ts_ms": now_ms(),
    });
    let Ok(text) = serde_json::to_string(&line) else {
        return;
    };
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("calls.jsonl"))
    {
        let _ = writeln!(f, "{text}");
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_config(dir: &Path) -> Result<BridgeConfig, String> {
    let path = dir.join("tools.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("读取 tools.json 失败: {e}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("tools.json 不是合法 JSON: {e}"))?;
    let Some(items) = value.get("tools").and_then(Value::as_array) else {
        return Err("tools.json 缺少 tools 数组".to_string());
    };
    if items.is_empty() {
        return Err("tools.json 的 tools 为空".to_string());
    }
    let mut tools = Vec::new();
    for item in items {
        let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return Err("tools.json 里有工具缺少 name".to_string());
        };
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("tools.json 里有工具名含非法字符".to_string());
        }
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input_schema = item
            .get("inputSchema")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        tools.push(ToolDef {
            name: name.to_string(),
            description,
            input_schema,
        });
    }
    let timeout_ms = value
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    fs::create_dir_all(dir.join("results")).map_err(|e| format!("建 results 目录失败: {e}"))?;
    Ok(BridgeConfig {
        dir: dir.to_path_buf(),
        tools,
        timeout: Duration::from_millis(timeout_ms),
    })
}

/// 供单测/诊断：把一行 JSON 喂给协议循环（不碰真实 stdio）。
///
/// 这个入口只用于测试；正常运行走 `serve()`。
#[cfg(test)]
fn serve_io_for_test<W: Write>(dir: &Path, lines: &[&str], out: &mut W, timeout: Duration) -> i32 {
    let Ok(cfg) = load_config(dir) else {
        return 2;
    };
    let (tx, rx) = mpsc::channel();
    for line in lines {
        let _ = tx.send((*line).to_string());
    }
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut code = 0;
    for line in lines {
        match handle_line(&cfg, &rx, out, deadline, line) {
            Flow::Continue => {}
            Flow::Exit(c) => {
                code = c;
                break;
            }
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ProxyDeck-mcp-child-test-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_tools(dir: &Path, timeout_ms: u64) -> Vec<String> {
        let tools = json!([
            {"name":"get_weather","original":"get weather!","description":"d","inputSchema":{"type":"object","properties":{"city":{"type":"string"}}}},
            {"name":"second_tool","original":"second.tool","description":"","inputSchema":{"type":"object","properties":{}}}
        ]);
        fs::write(
            dir.join("tools.json"),
            json!({"version":1,"timeout_ms":timeout_ms,"tools":tools}).to_string(),
        )
        .unwrap();
        vec!["get_weather".to_string(), "second_tool".to_string()]
    }

    #[test]
    fn missing_tools_json_fails() {
        let dir = temp_dir("missing");
        assert_eq!(
            serve_io_for_test(&dir, &[], &mut Vec::new(), Duration::from_secs(1)),
            2
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_tools_json_fails() {
        let dir = temp_dir("invalid");
        fs::write(dir.join("tools.json"), "{not json").unwrap();
        assert_eq!(
            serve_io_for_test(&dir, &[], &mut Vec::new(), Duration::from_secs(1)),
            2
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn initialize_and_tools_list_return_sanitized_tools() {
        let dir = temp_dir("list");
        write_tools(&dir, 10_000);
        let mut out = Vec::new();
        let lines = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        ];
        assert_eq!(
            serve_io_for_test(&dir, &lines, &mut out, Duration::from_secs(2)),
            0
        );
        let text = String::from_utf8(out).unwrap();
        let msgs: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(msgs.len(), 2, "通知不应有响应：{text}");
        assert_eq!(msgs[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(msgs[0]["result"]["capabilities"]["tools"], json!({}));
        let tools = msgs[1]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["city"]["type"],
            "string"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tools_call_writes_calls_jsonl_then_waits_for_result_file() {
        let dir = temp_dir("call-result");
        write_tools(&dir, 5_000);
        // 先放结果文件，验证 read_result + normalize_result 路径。
        fs::create_dir_all(dir.join("results")).unwrap();
        fs::write(
            dir.join("results").join("r1.json"),
            r#"{"ok":true,"value":42}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        let lines = [
            r#"{"jsonrpc":"2.0","id":"r1","method":"tools/call","params":{"name":"get_weather","arguments":{"city":"Beijing"}}}"#,
        ];
        assert_eq!(
            serve_io_for_test(&dir, &lines, &mut out, Duration::from_secs(2)),
            0
        );
        let text = String::from_utf8(out).unwrap();
        let msg: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(msg["id"], "r1");
        assert_eq!(msg["result"]["isError"], false);
        assert!(msg["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"value\":42"));
        let calls = fs::read_to_string(dir.join("calls.jsonl")).unwrap();
        assert!(calls.contains("\"name\":\"get_weather\""));
        assert!(calls.contains("\"city\":\"Beijing\""));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tools_call_cancel_file_exits_zero() {
        let dir = temp_dir("call-cancel");
        write_tools(&dir, 5_000);
        fs::create_dir_all(dir.join("results")).unwrap();
        fs::write(dir.join("cancel"), b"1").unwrap();
        let mut out = Vec::new();
        let lines = [
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"second_tool","arguments":{}}}"#,
        ];
        assert_eq!(
            serve_io_for_test(&dir, &lines, &mut out, Duration::from_secs(2)),
            0
        );
        let text = String::from_utf8(out).unwrap();
        let msg: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(msg["error"]["code"], -32000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_tool_is_rejected_without_writing_calls() {
        let dir = temp_dir("unknown");
        write_tools(&dir, 5_000);
        fs::create_dir_all(dir.join("results")).unwrap();
        let mut out = Vec::new();
        let lines = [
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        ];
        assert_eq!(
            serve_io_for_test(&dir, &lines, &mut out, Duration::from_secs(1)),
            0
        );
        let text = String::from_utf8(out).unwrap();
        let msg: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(msg["error"]["code"], -32602);
        assert!(!dir.join("calls.jsonl").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn result_file_name_is_path_safe() {
        assert_eq!(result_file_name("r1"), "r1");
        assert_eq!(result_file_name("../x y"), "___x_y");
        assert_eq!(result_file_name(""), "call");
    }
}
