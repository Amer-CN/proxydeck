//! `internal/qoder/worker.go` 的 Rust 等价物：每请求 spawn 一个**官方 agent SDK worker**
//! （`qoder-worker-runtime.obf.mjs`，node 单文件），走 stdin/stdout 各一行一个 JSON 的
//! stream-json 协议。**全部上游流量出自官方 worker 进程**——代理只做协议翻译，
//! 不自己构造任何对上游的请求（Go `server.go:3-6` 的「零自造请求」原则）。
//!
//! 时序（与 Go 逐条同序）：
//!
//! 1. 宿主 → `control_request` `initialize`（`request_id` 固定 `pd-init`）；
//! 2. worker → `control_response`（`request_id == "pd-init"`）⇒ 宿主接着喂用户消息；
//! 3. worker → `control_request` `fetch_job_token`（`payload hostTokenCallback=true` 必发），
//!    宿主回 `{token, expires_at?}`——**accessToken 直注会被上游拒收，jobToken 回调是唯一
//!    验证成功的方式**；
//! 4. worker 输出 `system`/`assistant` 流，最后 `result` 收尾退出。
//!
//! 思考块：`{"type":"thinking","thinking":"...","signature":"..."}`（Claude stream-json 约定，
//! 2026-09-11 探针实测）——只透传，不计入 `text`。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::gjson::{b, i, obj, s, J};

/// 单次请求总超时（含 worker 起动 + 完整 agent 任务；写码类任务可长达数分钟）。
pub const WORKER_TIMEOUT: Duration = Duration::from_secs(600);

/// worker 相对 `<ProgramFiles>` 的路径（对应 Go `findWorker()` 里的 `rel`）。
pub const WORKER_REL: &str = r"Qoder\Qoder CN\resources\app.asar.unpacked\node_modules\@qoder-ai\qoder-cn-agent-sdk\dist\_worker";
pub const WORKER_FILE: &str = "qoder-worker-runtime.obf.mjs";

/// auth payload 文件内容（Go 里的字面量，70 字节，**无末尾换行**）。
pub const AUTH_PAYLOAD_BODY: &str =
    r#"{"hostTokenCallback":true,"type":"jobToken","jobTokenProvider":"host"}"#;

/// `bufio.Scanner` 的单 token 上限（Go `sc.Buffer(make([]byte, 64*1024), 8*1024*1024)`）。
pub const SCAN_MAX_LINE: usize = 8 * 1024 * 1024;

/// 一次 worker 会话的收成（对应 Go `workerResult`）。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WorkerResult {
    /// 全部 assistant text 块拼接。
    pub text: String,
    /// `result.result`（官方最终全文）。
    pub final_text: String,
    /// `result.is_error`。
    pub is_err: bool,
    /// `result.errors` 失败原因数组。
    pub errs: Vec<String>,
    /// `result.total_credits`：本次会话累计消耗（每请求单个 worker ⇒ 即本次请求消耗）。
    pub total_credits: f64,
}

/// worker 过程中的增量事件（流式翻译用）。文本与思考**共用一条通道**，
/// 保证与 Go 的 `onText`/`onThinking` 回调同一个到达顺序。
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerEvent {
    Text(String),
    Thinking(String),
}

/// Go `findWorker()`：env `QODER_WORKER` → `D:\Program Files\...` → `%ProgramFiles%\...`。
/// 找不到回 `None`（调用方回 503 提示安装 Qoder）。
pub fn find_worker(env_override: Option<&str>, program_files: Option<&str>) -> Option<PathBuf> {
    let rel = Path::new(WORKER_REL).join(WORKER_FILE);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = env_override {
        let t = p.trim();
        if !t.is_empty() {
            candidates.push(PathBuf::from(t));
        }
    }
    candidates.push(Path::new(r"D:\Program Files").join(&rel));
    if let Some(pf) = program_files {
        if !pf.is_empty() {
            candidates.push(Path::new(pf).join(&rel));
        }
    }
    candidates.into_iter().find(|c| c.is_file())
}

/// payload 临时文件（Go `os.CreateTemp("", "qoder-auth-*.json")`）：写好后**用完即删**。
struct TempPayload {
    path: PathBuf,
}

impl TempPayload {
    fn create() -> std::io::Result<Self> {
        let dir = std::env::temp_dir();
        for attempt in 0..64u32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = dir.join(format!(
                "qoder-auth-{}-{nanos}-{attempt}.json",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut fh) => {
                    fh.write_all(AUTH_PAYLOAD_BODY.as_bytes())?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "无法创建 auth payload 临时文件",
        ))
    }
}

impl Drop for TempPayload {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 子进程环境（Go 的构造逐条照搬）：父环境去掉 `NODE_OPTIONS`（官方 SDK spawn 时会清理），
/// 末尾追加三个 `QODER_*`。
pub fn child_env(
    parent: &[(std::ffi::OsString, std::ffi::OsString)],
    payload_path: &Path,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        Vec::with_capacity(parent.len() + 3);
    for (k, v) in parent {
        // Go 用的是 `strings.HasPrefix(e, "NODE_OPTIONS=")`——**逐字**前缀，不做大小写折叠
        if k == "NODE_OPTIONS" {
            continue;
        }
        env.push((k.clone(), v.clone()));
    }
    env.push(("QODER_AGENT_SDK_ENTRYPOINT".into(), "sdk-ts".into()));
    env.push(("QODER_AGENT_SDK_VERSION".into(), "1.0.27".into()));
    env.push((
        "QODER_SDK_AUTH_PAYLOAD_FILE".into(),
        payload_path.as_os_str().to_owned(),
    ));
    env
}

/// worker 命令行参数（官方 App 的精简集；`--bare` **绝不能带**——实测会拒绝普通用户消息，
/// 报 `bare_session_control_only`）。
pub fn worker_args(worker_path: &Path, model: &str) -> Vec<String> {
    vec![
        worker_path.to_string_lossy().to_string(),
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--model".into(),
        model.to_string(),
        "--disable-builtin-skills".into(),
        "--setting-sources".into(),
        String::new(),
        "--strict-mcp-config".into(),
    ]
}

/// 跑一次 worker 会话（每请求 spawn）。
///
/// * `cwd_env` 是 Go 每请求现读的 `QODER_CWD`（本 crate 在构造状态时读一次，见 README 差异）；
/// * `events` 非 `None` 时逐块投递文本/思考增量（流式翻译用）。
///
/// 返回值与 Go `runWorker` 同语义：拿到 `result` ⇒ `Ok`；否则 `Err(原因文案)`，
/// 原因文案的优先级也与 Go 一致（超时 > stdout 中断 > 退出异常 > 未握手），末尾附 stderr 尾巴。
pub async fn run_worker(
    auth_token: String,
    auth_expires_at_ms: Option<i64>,
    worker_path: PathBuf,
    model: String,
    prompt: String,
    cwd_env: Option<String>,
    events: Option<UnboundedSender<WorkerEvent>>,
) -> Result<WorkerResult, String> {
    let payload = TempPayload::create().map_err(|e| format!("写 auth payload 失败: {e}"))?;

    let work_dir = crate::cwd::dir(&prompt, cwd_env.as_deref());
    println!("qoder-plugin: worker cwd={work_dir}");

    let parent_env: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let env = child_env(&parent_env, &payload.path);

    let mut cmd = tokio::process::Command::new("node");
    cmd.args(worker_args(&worker_path, &model));
    cmd.current_dir(&work_dir);
    cmd.env_clear();
    cmd.envs(env);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // Go 的 hideConsole：CREATE_NO_WINDOW，不弹控制台窗口
        cmd.creation_flags(0x08000000);
    }

    let mut child = cmd.spawn().map_err(|e| format!("启动 worker 失败: {e}"))?;

    let mut stdin = child.stdin.take().ok_or("worker stdin 不可用")?;
    let stdout = child.stdout.take().ok_or("worker stdout 不可用")?;
    let mut stderr_pipe = child.stderr.take().ok_or("worker stderr 不可用")?;

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    let deadline = tokio::time::Instant::now() + WORKER_TIMEOUT;

    // Go 的 send()：json.Marshal 后追加 '\n' 写 stdin。
    macro_rules! send {
        ($stdin:expr, $v:expr) => {{
            let mut line = $v.to_go();
            line.push('\n');
            $stdin.write_all(line.as_bytes()).await
        }};
    }

    let init = obj(vec![
        ("type", s("control_request")),
        ("request_id", s("pd-init")),
        (
            "request",
            obj(vec![
                ("type", s("initialize")),
                ("subtype", s("initialize")),
                ("modelPolicyProvider", b(false)),
                ("supportsCatalogReadyInitialize", b(true)),
                ("initializeTimeoutMs", i(120000)),
            ]),
        ),
    ]);
    if let Err(e) = send!(&mut stdin, &init) {
        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = stderr_task.await;
        return Err(format!("写 initialize 失败: {e}"));
    }

    let reply_ok = |req: &CtrlReq, resp: J| -> J {
        obj(vec![
            ("type", s("control_response")),
            (
                "response",
                obj(vec![
                    ("subtype", s("success")),
                    ("request_id", s(&req.request_id)),
                    ("response", resp),
                ]),
            ),
        ])
    };

    let mut res = WorkerResult::default();
    let mut inited = false;
    let mut done = false;
    let mut timed_out = false;
    let mut scan_err: Option<String> = None;

    let mut reader = BufReader::new(stdout);
    let mut line: Vec<u8> = Vec::new();
    while !done {
        line.clear();
        let read =
            tokio::time::timeout_at(deadline, reader.read_until(b'\n', &mut line)).await;
        let n = match read {
            Err(_) => {
                timed_out = true;
                break;
            }
            Ok(Err(e)) => {
                // Go：`sc.Err()` 分支统一带 "worker stdout 中断: " 前缀
                scan_err = Some(format!("worker stdout 中断: {e}"));
                break;
            }
            Ok(Ok(0)) => break, // EOF
            Ok(Ok(n)) => n,
        };
        if n > SCAN_MAX_LINE + 1 {
            // bufio.Scanner 的 token too long：超过 8MB 的行不再解析。
            // ⚠ 内存形状与 Go 不同（如实登记，见 README 差异 5）：Go 的 Scanner 封顶在
            // 8MB —— 超过即弃、不再读；这里的 `read_until` 是**先把整行读进内存**，
            // 读完才判这一行，所以超长行的峰值 = 该行实际长度，不是 8MB。
            scan_err = Some("worker stdout 中断: bufio.Scanner: token too long".to_string());
            break;
        }
        let text = String::from_utf8_lossy(&line).to_string();
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // Go：Unmarshal 失败就 continue
        };
        match msg.get("type").and_then(|v| v.as_str()) {
            Some("control_response") => {
                let resp = msg.get("response");
                let rid = resp
                    .and_then(|r| r.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !inited && resp.is_some() && rid == "pd-init" {
                    inited = true;
                    let user_msg = obj(vec![
                        ("type", s("user")),
                        (
                            "message",
                            obj(vec![
                                ("role", s("user")),
                                (
                                    "content",
                                    J::Arr(vec![obj(vec![
                                        ("type", s("text")),
                                        ("text", s(&prompt)),
                                    ])]),
                                ),
                            ]),
                        ),
                    ]);
                    if let Err(e) = send!(&mut stdin, &user_msg) {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        let _ = stderr_task.await;
                        return Err(format!("写用户消息失败: {e}"));
                    }
                }
            }
            Some("control_request") => {
                let req = CtrlReq::parse(&msg);
                match req.request.subtype.as_str() {
                    "fetch_job_token" => {
                        let mut resp = vec![("token", s(&auth_token))];
                        if let Some(ms) = auth_expires_at_ms {
                            resp.push(("expires_at", i(ms)));
                        }
                        let _ = send!(&mut stdin, &reply_ok(&req, obj(resp)));
                    }
                    "can_use_tool" => {
                        // 全放行：与官方 App 的 bypassPermissions 同语义
                        let _ = send!(
                            &mut stdin,
                            &reply_ok(
                                &req,
                                obj(vec![
                                    ("behavior", s("allow")),
                                    ("toolUseID", s(&req.request.tool_use_id)),
                                ]),
                            )
                        );
                    }
                    other => {
                        let _ = send!(
                            &mut stdin,
                            &obj(vec![
                                ("type", s("control_response")),
                                (
                                    "response",
                                    obj(vec![
                                        ("subtype", s("error")),
                                        ("request_id", s(&req.request_id)),
                                        (
                                            "error",
                                            s(format!("unhandled control request: {other}")),
                                        ),
                                    ]),
                                ),
                            ])
                        );
                    }
                }
            }
            Some("assistant") => {
                let content = msg
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array());
                if let Some(items) = content {
                    for item in items {
                        match item.get("type").and_then(|v| v.as_str()) {
                            Some("text") => {
                                let t = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                if !t.is_empty() {
                                    res.text.push_str(t);
                                    if let Some(tx) = &events {
                                        let _ = tx.send(WorkerEvent::Text(t.to_string()));
                                    }
                                }
                            }
                            Some("thinking") => {
                                let t = item
                                    .get("thinking")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if !t.is_empty() {
                                    if let Some(tx) = &events {
                                        let _ = tx.send(WorkerEvent::Thinking(t.to_string()));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("result") => {
                res.final_text = msg
                    .get("result")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                res.is_err = msg
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if let Some(tc) = msg.get("total_credits").and_then(|v| v.as_f64()) {
                    res.total_credits = tc;
                }
                if let Some(errs) = msg.get("errors").and_then(|v| v.as_array()) {
                    for e in errs {
                        if let Some(sv) = e.as_str() {
                            res.errs.push(sv.to_string());
                        }
                    }
                }
                done = true;
            }
            _ => {}
        }
    }

    let _ = stdin.shutdown().await;
    drop(stdin);

    if timed_out {
        let _ = child.kill().await;
    }
    // Rust 的 `wait()` 对**非零退出码**回的是 `Ok(ExitStatus)`，Go 的 `cmd.Wait()` 回的是错误
    // （`exit status 3`）——这里按 Go 的形状折算，否则 `exit-nonzero` 场景会静默走错分支。
    //
    // 等待同样套在 600s 预算里：Go 的 `exec.CommandContext` 到点会**杀掉**子进程，
    // 而「子进程卡住不再写 stdout」（例如它被写满的管道堵住、或干脆挂着不动）正是
    // CommandContext 存在的意义。不套预算就会永久挂住，与 Go 分歧。
    let wait_status = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(st) => st,
        Err(_) => {
            timed_out = true;
            let _ = child.kill().await;
            child.wait().await
        }
    };
    let wait_err = match wait_status {
        Err(e) => Some(e.to_string()),
        Ok(st) if !st.success() => Some(match st.code() {
            Some(c) => format!("exit status {c}"),
            None => "signal: killed".to_string(),
        }),
        Ok(_) => None,
    };
    let stderr_buf = stderr_task.await.unwrap_or_default();

    if done {
        return Ok(res);
    }

    let mut cause = "worker 结束但未收到 result".to_string();
    if timed_out {
        // Go：fmt.Sprintf("worker 超时（%v）", 600*time.Second) ⇒ "worker 超时（10m0s）"
        cause = format!("worker 超时（{}）", go_duration(WORKER_TIMEOUT));
    } else if let Some(e) = scan_err {
        cause = e;
    } else if let Some(e) = wait_err {
        cause = format!("worker 退出异常: {e}");
    } else if !inited {
        cause = "worker 握手失败（未收到 initialize 应答）".to_string();
    }
    let tail = String::from_utf8_lossy(&stderr_buf).trim().to_string();
    if !tail.is_empty() {
        let mut tail = tail;
        if tail.len() > 500 {
            // Go 是按**字节**切尾部（可能落在 UTF-8 中间），这里保持同语义
            let bytes = tail.as_bytes();
            tail = String::from_utf8_lossy(&bytes[bytes.len() - 500..]).to_string();
        }
        cause.push_str("; stderr: ");
        cause.push_str(&tail);
    }
    Err(cause)
}

/// Go `time.Duration.String()` 的形状（本 crate 只用得到整秒级的时长：
/// 600s ⇒ `10m0s`、60s ⇒ `1m0s`、5s ⇒ `5s`；秒数在分钟/小时存在时**一定要写**）。
fn go_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// worker stdout 里 `control_request` 的最小字段（对应 Go `ctrlReq`）。
#[derive(Debug, Default, Clone)]
struct CtrlReq {
    request_id: String,
    request: CtrlReqInner,
}

#[derive(Debug, Default, Clone)]
struct CtrlReqInner {
    subtype: String,
    tool_use_id: String,
}

impl CtrlReq {
    fn parse(msg: &serde_json::Value) -> Self {
        let request_id = msg
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let req = msg.get("request");
        let subtype = req
            .and_then(|r| r.get("subtype"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tool_use_id = req
            .and_then(|r| r.get("tool_use_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Self {
            request_id,
            request: CtrlReqInner {
                subtype,
                tool_use_id,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_worker_prefers_env_override() {
        // 本机**真的装了 Qoder CN**（`D:\Program Files\Qoder\Qoder CN\...` 实测存在），
        // 所以这里不能断言「找不到」；只锁两条可复现的判定：
        //   1. 空白 env 不算覆盖（Go 的 `strings.TrimSpace(p) != ""`）；
        //   2. 指到真文件就以它为准（env 是第一优先）。
        let dir = std::env::temp_dir().join("rs-qoder-findworker");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(WORKER_FILE);
        std::fs::write(&f, "// fake").unwrap();
        assert_eq!(find_worker(Some(&f.to_string_lossy()), None), Some(f.clone()));
        assert_ne!(
            find_worker(Some("   "), None),
            Some(f.clone()),
            "空白 env 不该被当成路径"
        );
        // 目录而不是文件 → 不算命中（Go 是 `!fi.IsDir()`），继续往后找
        assert_ne!(
            find_worker(Some(&dir.to_string_lossy()), None),
            Some(f.clone())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn child_env_drops_node_options_and_appends_three() {
        let parent: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
            ("PATH".into(), "C:\\x".into()),
            ("NODE_OPTIONS".into(), "--max-old-space-size=64".into()),
            ("QODER_AGENT_SDK_VERSION".into(), "old".into()),
        ];
        let env = child_env(&parent, Path::new("C:\\tmp\\qoder-auth-1.json"));
        let keys: Vec<String> = env
            .iter()
            .map(|(k, _)| k.to_string_lossy().to_string())
            .collect();
        assert!(
            !keys.contains(&"NODE_OPTIONS".to_string()),
            "NODE_OPTIONS 必须被剔除"
        );
        assert_eq!(
            &keys[keys.len() - 3..],
            &[
                "QODER_AGENT_SDK_ENTRYPOINT".to_string(),
                "QODER_AGENT_SDK_VERSION".to_string(),
                "QODER_SDK_AUTH_PAYLOAD_FILE".to_string()
            ],
            "三个 QODER_* 追加在末尾"
        );
        assert_eq!(
            env.last().unwrap().1.to_string_lossy(),
            "C:\\tmp\\qoder-auth-1.json"
        );
        // 同名变量的旧值仍在（Go 也是原样保留，Windows 环境块里后者覆盖前者）
        assert_eq!(
            keys.iter()
                .filter(|k| *k == "QODER_AGENT_SDK_VERSION")
                .count(),
            2
        );
    }

    #[test]
    fn worker_args_match_go() {
        let args = worker_args(Path::new("C:\\w\\worker.mjs"), "Auto");
        assert_eq!(
            args,
            vec![
                "C:\\w\\worker.mjs",
                "--print",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--model",
                "Auto",
                "--disable-builtin-skills",
                "--setting-sources",
                "",
                "--strict-mcp-config",
            ]
        );
        assert!(!args.iter().any(|a| a == "--bare"), "--bare 绝不能带");
    }

    #[test]
    fn go_duration_shape() {
        assert_eq!(go_duration(Duration::from_secs(600)), "10m0s");
        assert_eq!(go_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(go_duration(Duration::from_secs(5)), "5s");
    }
}
