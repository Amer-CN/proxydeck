//! P8b 第三片差分对照的**共用场景**（Rust 侧集成测试与对照 runner 都用这一份，
//! 避免"两边各写一套用例"导致的漂移）。
//!
//! 场景内容：假上游（换 key / 计费 / 模型清单三条路径）+ 合成登录态 + 一串固定的
//! HTTP 请求，然后把每次响应的
//! `{status, content_type, body}` 记下来，供两侧逐字节/逐形状比较。
//!
//! **纪律**：假上游只监听 `127.0.0.1` 的临时端口；不碰 8788（生产端口）、不出网、
//! 不读真实凭据（`USERPROFILE` 指向临时目录，登录态是合成本地 JWT）。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

// ---------------------------------------------------------------- 合成数据

/// 假上游下发的 cli_api_key（≥10 字符，`/health` 的 `key[:10]+"..."` 才有形态）。
pub const FAKE_CLI_KEY: &str = "sk-rs-tuanjie-pilot-cli-key-0001";

/// 合成账号的 user_id（JWT 的 `sub`）。
pub const FAKE_UID: &str = "uid-p8b3-synthetic";

/// 合成 JWT（三段、base64url 无填充、`exp` 在远期）——
/// **不是**任何真实凭据：只用来让 `jwtSub`/`jwtExpiry` 走出可比的确定值。
///
/// 注意：默认形状**无 salt**（`{sub,user_id,exp}`）——Go 侧的合成 JWT 同形，
/// 差分门禁按 `Authorization` 全值比对计费命中，形状一变就红。
/// 要按用例区分 token 请用 [`synthetic_jwt_for`]（显式盐），别改这里。
pub fn synthetic_jwt() -> String {
    let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = b64url(
        format!(
            r#"{{"sub":"{FAKE_UID}","user_id":999001,"exp":4102444800}}"#
        )
        .as_bytes(),
    );
    let sig = b64url(b"p8b3-synthetic-signature");
    format!("{header}.{payload}.{sig}")
}

/// 同上，`sub` 由调用方给（P8b 第四片的合成账号池要多个不同身份的号）。
pub fn synthetic_jwt_with_sub(sub: &str) -> String {
    synthetic_jwt_for(sub, sub)
}

/// `sub` 与「盐」分开给：同一个 `sub`（= 池里的 `user_id`）在不同用例里必须得到
/// **不同的 token 字符串** —— 否则池号的 access_token 会跨用例撞成同一个值，
/// 而假上游是按 token 分辨账号的（P8b 第四片踩过：两个用例的 `uid-a` 拿到同一把 token，
/// 于是互相打对方的脚本）。
pub fn synthetic_jwt_for(sub: &str, salt: &str) -> String {
    let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = b64url(
        format!(r#"{{"sub":"{sub}","user_id":999001,"salt":"{salt}","exp":4102444800}}"#)
            .as_bytes(),
    );
    let sig = b64url(b"p8b3-synthetic-signature");
    format!("{header}.{payload}.{sig}")
}

fn b64url(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// 计费摘要（`/api/user/billing/usage/summary`）的成功应答 —— 覆盖 Go `summaryResp`
/// 的每个字段（points 类是**字符串**、token 类是数字）。
pub const BILLING_OK_JSON: &str = r#"{
  "billing": {"effective_available_points": 12.5},
  "daily_allowance": {"quota_points": "1000.000000000", "used_points": "250.500000000",
    "remaining_points": "749.500000000", "period_end_at": "2026-01-01T00:00:00Z"},
  "gift_credits": {"remaining_points": "30.25"},
  "coding_plan": {"effective_remaining_points": "500.5", "next_boundary_at": "2026-01-02T00:00:00Z",
    "windows": [
      {"window_type": "usage_5h", "quota_points": "100", "used_points": "20",
       "remaining_points": "80", "exhausted": false, "period": {"end_at": "2026-01-01T05:00:00Z"}},
      {"window_type": "subscription_week", "quota_points": "200", "used_points": "200",
       "remaining_points": "0", "exhausted": true, "period": {"end_at": "2026-01-05T00:00:00Z"}}
    ]},
  "totals": {"recorded_points": 321.75, "total_tokens": 1234567, "prompt_tokens": 1000000,
    "completion_tokens": 234567, "settlement_count": 42,
    "by_source": [
      {"label": "codely-core", "recorded_points": 100.5, "total_tokens": 500000, "settlement_count": 20},
      {"label": "codely-core", "recorded_points": 50.25, "total_tokens": 250000, "settlement_count": 10},
      {"label": "codely-vl", "recorded_points": 171.0, "total_tokens": 484567, "settlement_count": 12}
    ]},
  "period": {"start_date": "2026-01-01", "end_date": "2026-01-31"}
}"#;

/// `/v1/models` 的上游应答（透传路径用）。
pub const MODELS_JSON: &str =
    r#"{"object":"list","data":[{"id":"codely-core","object":"model","owned_by":"tuanjie"}]}"#;

// ---------------------------------------------------------------- 假上游

/// 假上游记录的一次请求（只记形态，不记真实密钥）。
///
/// P8b 第五片补了 `api_key`/`content_type`/`opencode_session`/`user_agent`/`body`——
/// 外部 provider 的三条转发路径要比**出站体**（协议转换的合同面），只记四件套不够。
#[derive(Debug, Clone, PartialEq)]
pub struct SeenReq {
    pub method: String,
    pub path: String,
    pub authorization: String,
    pub accept: String,
    pub api_key: String,
    pub content_type: String,
    pub opencode_session: bool,
    pub user_agent: String,
    pub body: String,
}

/// 一条脚本化应答（P8b 第五片：外部 provider 路径与团结路径共用同一种形状）。
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub code: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// JSON 应答（`Content-Type: application/json`）。
    pub fn json(code: u16, body: &str) -> Reply {
        Reply {
            code,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.as_bytes().to_vec(),
        }
    }

    /// SSE 应答。
    pub fn sse(body: &str) -> Reply {
        Reply {
            code: 200,
            headers: vec![(
                "Content-Type".to_string(),
                "text/event-stream".to_string(),
            )],
            body: body.as_bytes().to_vec(),
        }
    }
}

struct FakeState {
    /// `authorization` 全值 → 计费接口命中次数。
    billing_hits: BTreeMap<String, usize>,    /// `authorization` 全值 → 计费接口应答（状态码, 体）。
    billing_reply: BTreeMap<String, (u16, String)>,
    seen: Vec<SeenReq>,
    cli_key_hits: usize,
    models_hits: usize,
    /// P8b 第四片：**chat 路径的确定性应答**
    /// * `token_keys`：换 key 请求里 `Authorization: Bearer <access_token>` → 下发的 cli_api_key
    ///   （每个合成账号一把，便于按号区分 chat 命中）；
    /// * `chat_script`：`cli_api_key → 逐次的（状态码, Retry-After, 体）`，越界后重复最后一条；
    ///   未登记的 key 回 404。
    token_keys: BTreeMap<String, String>,
    chat_script: BTreeMap<String, Vec<(u16, String, String)>>,
    chat_calls: BTreeMap<String, usize>,
    chat_order: Vec<String>,
    chat_hits: usize,
    last_chat_body: Vec<u8>,
    /// P8b 第五片：**按完整 path 脚本的应答**（外部 provider 用；越界重复最后一条）。
    path_script: BTreeMap<String, Vec<Reply>>,
    path_calls: BTreeMap<String, usize>,
    /// P8b 第五片：**按 cli_api_key 脚本的应答**（团结路径的通用形态，带自定义头）。
    unity_script: BTreeMap<String, Vec<Reply>>,
    /// `/v1/models` 的应答体（默认 [`MODELS_JSON`]）。
    models_body: String,
    /// `/img/fetch.png` 的应答字节（远程图片预下载用）。
    image_body: Vec<u8>,
    /// P8b 第五片的探针整轮模式（按请求体算应答，见 [`pipeline_reply`]）。
    pipeline: bool,
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            billing_hits: BTreeMap::new(),
            billing_reply: BTreeMap::new(),
            seen: Vec::new(),
            cli_key_hits: 0,
            models_hits: 0,
            token_keys: BTreeMap::new(),
            chat_script: BTreeMap::new(),
            chat_calls: BTreeMap::new(),
            chat_order: Vec::new(),
            chat_hits: 0,
            last_chat_body: Vec::new(),
            path_script: BTreeMap::new(),
            path_calls: BTreeMap::new(),
            unity_script: BTreeMap::new(),
            models_body: MODELS_JSON.to_string(),
            image_body: Vec::new(),
            pipeline: false,
        }
    }
}

/// 极简假上游：一个连接一个线程，逐请求应答（`Connection: close`，避免保活复杂度）。
pub struct FakeUpstream {
    pub addr: String,
    state: Arc<Mutex<FakeState>>,
    stop: Arc<AtomicBool>,
}

impl FakeUpstream {
    /// 启动假上游；`billing_reply` 决定每个 token 打计费口时的应答。
    pub fn start(billing_reply: Vec<(String, u16, String)>) -> FakeUpstream {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
        let addr = listener.local_addr().expect("local_addr").to_string();
        // 阻塞 accept + 每连接一线程：**不要**用 nonblocking 轮询——连接风暴时
        // accept 的瞬时错误会打断轮询循环、假上游就此死掉（踩过一次：
        // `models-get-final` 报"转发请求网络失败"，两侧命中数凭空差一条）。
        listener
            .set_nonblocking(false)
            .expect("阻塞模式便于逐连接服务");
        let state = Arc::new(Mutex::new(FakeState::default()));
        {
            let mut st = state.lock().unwrap();
            for (auth, code, body) in billing_reply {
                st.billing_reply.insert(auth, (code, body));
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let st2 = state.clone();
        std::thread::spawn(move || loop {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let st = st2.clone();
                    std::thread::spawn(move || serve_conn(stream, st));
                }
                Err(_) => {
                    // 停机由 stop 标志决定；accept 的瞬时错误不终止服务
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
            }
        });
        FakeUpstream { addr, state, stop }
    }

    /// 假上游的 base（`http://127.0.0.1:<port>`）。
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 某个 authorization 在计费口上的命中次数。
    pub fn billing_hits(&self, auth: &str) -> usize {
        *self.state.lock().unwrap().billing_hits.get(auth).unwrap_or(&0)
    }

    /// cli_api_key 端点命中次数。
    pub fn cli_key_hits(&self) -> usize {
        self.state.lock().unwrap().cli_key_hits
    }

    /// `/v1/models` 命中次数。
    pub fn models_hits(&self) -> usize {
        self.state.lock().unwrap().models_hits
    }

    /// 记录到的请求形态。
    pub fn seen(&self) -> Vec<SeenReq> {
        self.state.lock().unwrap().seen.clone()
    }

    // ------------------------------------------------ P8b 第四片：chat 确定性应答

    /// 登记「account access_token → 下发的 cli_api_key」（换 key 端点按 Bearer 区分）。
    pub fn set_token_key(&self, access_token: &str, cli_api_key: &str) {
        self.state.lock().unwrap().token_keys.insert(
            format!("Bearer {access_token}"),
            cli_api_key.to_string(),
        );
    }

    /// 登记某个 cli_api_key 在 chat 路径上的**逐次应答**（越界后重复最后一条）。
    /// `Retry-After` 为空串表示不下发该头。
    ///
    /// **顺带把该 key 的命中计数与命中序清零**：多个用例共用同一个假上游时，
    /// 只换脚本不重置计数会让新用例从脚本中间取应答（P8b 第四片差分对照踩过一次：
    /// `chat-empty-then-200` 取到了第二条，与服务端「没嗅探」的行为混在一起误导排查）。
    /// 语义上等价于「每个场景起一个全新的假上游」。
    pub fn set_chat_script(&self, cli_api_key: &str, script: Vec<(u16, &str, &str)>) {
        let mut st = self.state.lock().unwrap();
        st.chat_calls.remove(cli_api_key);
        st.chat_order.retain(|k| k != cli_api_key);
        st.chat_script.insert(
            cli_api_key.to_string(),
            script
                .into_iter()
                .map(|(c, ra, b)| (c, ra.to_string(), b.to_string()))
                .collect(),
        );
    }

    /// chat 路径被访问的 key 序（按到达顺序）。
    pub fn chat_order(&self) -> Vec<String> {
        self.state.lock().unwrap().chat_order.clone()
    }

    /// 某个 key 在 chat 路径上的命中次数。
    pub fn chat_calls(&self, cli_api_key: &str) -> usize {
        *self
            .state
            .lock()
            .unwrap()
            .chat_calls
            .get(cli_api_key)
            .unwrap_or(&0)
    }

    /// chat 路径总命中次数。
    pub fn chat_hits(&self) -> usize {
        self.state.lock().unwrap().chat_hits
    }

    /// chat 路径收到的最后一个请求体（原始字节）。
    pub fn last_chat_body(&self) -> Vec<u8> {
        self.state.lock().unwrap().last_chat_body.clone()
    }

    // ------------------------------------------------ P8b 第五片：外部 provider / 团结通用脚本

    /// 登记某个**完整 path** 的逐次应答（外部 provider 路径用；越界后重复最后一条）。
    /// 顺带把该 path 的命中计数清零（语义同 [`Self::set_chat_script`]）。
    pub fn set_path_script(&self, path: &str, script: Vec<Reply>) {
        let mut st = self.state.lock().unwrap();
        st.path_calls.remove(path);
        st.path_script.insert(path.to_string(), script);
    }

    /// 登记某个 cli_api_key 在团结路径上的逐次应答（带自定义头）。
    pub fn set_unity_script(&self, cli_api_key: &str, script: Vec<Reply>) {
        let mut st = self.state.lock().unwrap();
        st.unity_script.insert(cli_api_key.to_string(), script);
    }

    /// 覆盖 `/v1/models` 的应答体。
    pub fn set_models_body(&self, body: &str) {
        self.state.lock().unwrap().models_body = body.to_string();
    }

    /// 覆盖 `/img/fetch.png` 的字节。
    pub fn set_image_body(&self, body: Vec<u8>) {
        self.state.lock().unwrap().image_body = body;
    }

    /// 清空脚本与命中计数（每个场景开头调一次）。
    pub fn reset_scripts(&self) {
        let mut st = self.state.lock().unwrap();
        st.path_script.clear();
        st.path_calls.clear();
        st.unity_script.clear();
        st.chat_script.clear();
        st.chat_calls.clear();
        st.chat_order.clear();
        st.chat_hits = 0;
        st.models_body = MODELS_JSON.to_string();
        st.image_body = vec![];
        st.seen.clear();
        st.pipeline = false;
    }

    /// 探针整轮采集模式：`/v1/chat/completions` 按**请求体**算确定性应答
    /// （prompt_tokens = 文本 rune 数 + 3），与 Go 采集器同一套算法。
    pub fn set_pipeline_mode(&self, on: bool) {
        self.state.lock().unwrap().pipeline = on;
    }
}

/// 探针整轮模式的确定性应答 —— 与 Go 采集器的 `p8b5PipelineReply` 逐条同算法。
fn pipeline_reply(body: &[u8]) -> Reply {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
    let text = v
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|a| a.first())
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let max_tokens = v.get("max_tokens").and_then(|m| m.as_i64()).unwrap_or(0);
    let prompt = text.chars().count() + 3;
    if max_tokens == 1000000000 {
        return Reply::json(400, r#"{"error":"max_tokens too large: 1000000000"}"#);
    }
    if max_tokens == 6 {
        return Reply::json(
            200,
            &format!(
                r#"{{"model":"codely-core","choices":[{{"finish_reason":"length","message":{{"content":"秋天"}}}}],"usage":{{"prompt_tokens":{prompt},"completion_tokens":6}}}}"#
            ),
        );
    }
    Reply::json(
        200,
        &format!(
            r#"{{"model":"codely-core","choices":[{{"finish_reason":"stop","message":{{"content":"好"}}}}],"usage":{{"prompt_tokens":{prompt},"completion_tokens":2}}}}"#
        ),
    )
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<FakeState>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    // 读头（本场景全是 GET/POST 小请求）
    let mut head = Vec::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut auth = String::new();
    let mut accept = String::new();
    let mut api_key = String::new();
    let mut content_type = String::new();
    let mut user_agent = String::new();
    let mut opencode_session = false;
    let mut content_length = 0usize;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "authorization" => auth = v.trim().to_string(),
            "accept" => accept = v.trim().to_string(),
            "x-api-key" => api_key = v.trim().to_string(),
            "content-type" => content_type = v.trim().to_string(),
            "user-agent" => user_agent = v.trim().to_string(),
            "x-opencode-session" => opencode_session = true,
            "content-length" => content_length = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    // 请求体：`\r\n\r\n` 之后的字节（可能已在同一批 read 里到齐，也可能还要再读）。
    let body_at = head
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(head.len());
    let mut req_body: Vec<u8> = head[body_at..].to_vec();
    while req_body.len() < content_length {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => req_body.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let (code, headers, body): (u16, Vec<(String, String)>, Vec<u8>) = {
        let mut st = state.lock().unwrap();
        st.seen.push(SeenReq {
            method,
            path: path.clone(),
            authorization: auth.clone(),
            accept,
            api_key,
            content_type,
            opencode_session,
            user_agent,
            body: String::from_utf8_lossy(&req_body).into_owned(),
        });
        let bare = path.split('?').next().unwrap_or("").to_string();
        let billing_path = "/api/user/billing/usage/summary";
        if bare == "/api/api-token/cli-api-key" {
            st.cli_key_hits += 1;
            // P8b 第四片：按 Bearer 下发**每个合成账号自己的** key；未登记时回默认常量
            // （P8b 第三片的既有用例仍走这条）。
            let key = st
                .token_keys
                .get(&auth)
                .cloned()
                .unwrap_or_else(|| FAKE_CLI_KEY.to_string());
            let body = format!("{{\"cli_api_key\":\"{key}\"}}");
            (200u16, vec![], body.into_bytes())
        } else if bare == "/img/fetch.png" {
            (
                200u16,
                vec![("Content-Type".to_string(), "image/png".to_string())],
                st.image_body.clone(),
            )
        } else if bare == "/img/jpeg.bin" {
            // 无 `image/*` 的 Content-Type ⇒ 必须走**魔术字节嗅探**（覆盖 `sniffImageMIME`）
            (
                200u16,
                vec![(
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                )],
                vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4],
            )
        } else if let Some(script) = st.path_script.get(&bare).cloned() {
            // P8b 第五片：外部 provider 路径（完整 path 脚本，越界重复最后一条）
            let n = st.path_calls.entry(bare.clone()).or_insert(0);
            let idx = *n;
            *n += 1;
            let rep = script[idx.min(script.len() - 1)].clone();
            (rep.code, rep.headers, rep.body)
        } else if bare == billing_path {
            let n = st.billing_hits.entry(auth.clone()).or_insert(0);
            *n += 1;
            match st.billing_reply.get(&auth) {
                Some((c, b)) => (*c, vec![], b.clone().into_bytes()),
                None => (404u16, vec![], Vec::new()),
            }
        } else if bare == "/v1/models" {
            st.models_hits += 1;
            (200u16, vec![], st.models_body.clone().into_bytes())
        } else if bare == "/v1/chat/completions" {
            st.chat_hits += 1;
            if st.pipeline {
                // P8b 第五片：探针整轮模式（按请求体算应答）
                let rep = pipeline_reply(&req_body);
                (rep.code, rep.headers, rep.body)
            } else {
            st.last_chat_body = req_body.clone();
            let key = auth.strip_prefix("Bearer ").unwrap_or(&auth).to_string();
            st.chat_order.push(key.clone());
            let n = st.chat_calls.entry(key.clone()).or_insert(0);
            let idx = *n;
            *n += 1;
            // 先看通用 unity 脚本（带自定义头），再回落第四片的 (code, retry_after, body) 形态
            if let Some(script) = st.unity_script.get(&key).cloned() {
                if !script.is_empty() {
                    let rep = script[idx.min(script.len() - 1)].clone();
                    (rep.code, rep.headers, rep.body)
                } else {
                    (404u16, vec![], Vec::new())
                }
            } else {
                match st
                    .chat_script
                    .get(&key)
                    .map(|s| s.as_slice())
                    .and_then(|s| {
                        if s.is_empty() {
                            None
                        } else {
                            Some(s[idx.min(s.len() - 1)].clone())
                        }
                    }) {
                    Some((c, ra, b)) => {
                        let mut hs: Vec<(String, String)> = Vec::new();
                        if !ra.is_empty() {
                            hs.push(("Retry-After".to_string(), ra));
                        }
                        (c, hs, b.into_bytes())
                    }
                    None => (404u16, vec![], Vec::new()),
                }
            }
            }
        } else {
            (404u16, vec![], Vec::new())
        }
    };
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Status",
    };
    let has_ct = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    let mut resp = format!(
        "HTTP/1.1 {code} {reason}\r\n{}\
         Content-Length: {}\r\nConnection: close\r\n",
        if has_ct {
            String::new()
        } else {
            "Content-Type: application/json\r\n".to_string()
        },
        body.len()
    );
    for (k, v) in &headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str("\r\n");
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

// ---------------------------------------------------------------- 场景用例

/// 一条请求。
#[derive(Debug, Clone)]
pub struct Case {
    pub name: &'static str,
    pub method: &'static str,
    pub path: &'static str,
    /// POST 体（None = 不带体）。
    pub body: Option<&'static str>,
    /// 跑这一条之前要做的预置动作（两侧 runner 都按同一份执行）。
    pub setup: Setup,
}

/// 用例前置动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    None,
    /// 预置一条合成进行中请求：rid 固定 `000000000001`（`rid_from_n(1)`，两侧同值）、
    /// `model=codely-core`、`stream=true`、`bytes=7`。
    InsertInflight,
}

/// 固定请求序列 —— **两侧共用这一份**。
pub fn cases() -> Vec<Case> {
    vec![
        Case { name: "health-get", method: "GET", path: "/health", body: None, setup: Setup::None },
        Case { name: "health-post", method: "POST", path: "/health", body: None, setup: Setup::None },
        Case { name: "models-get", method: "GET", path: "/v1/models", body: None, setup: Setup::None },
        Case { name: "models-put-405", method: "PUT", path: "/v1/models", body: None, setup: Setup::None },
        Case { name: "model-info-501", method: "GET", path: "/model/info", body: None, setup: Setup::None },
        Case { name: "stats-get", method: "GET", path: "/v1/stats", body: None, setup: Setup::None },
        Case { name: "stats-post", method: "POST", path: "/v1/stats", body: None, setup: Setup::None },
        Case { name: "accounts-get", method: "GET", path: "/accounts", body: None, setup: Setup::None },
        Case { name: "accounts-put-405", method: "PUT", path: "/accounts", body: None, setup: Setup::None },
        Case { name: "health-options", method: "OPTIONS", path: "/health", body: None, setup: Setup::None },
        Case { name: "unknown-path-404", method: "GET", path: "/nope", body: None, setup: Setup::None },
        Case { name: "pacing-get", method: "GET", path: "/kimi-pacing", body: None, setup: Setup::None },
        Case {
            name: "pacing-post-true",
            method: "POST",
            path: "/kimi-pacing",
            body: Some(r#"{"enabled":true}"#),
            setup: Setup::None,
        },
        Case { name: "pacing-get-after-true", method: "GET", path: "/kimi-pacing", body: None, setup: Setup::None },
        Case {
            name: "pacing-post-bad-body",
            method: "POST",
            path: "/kimi-pacing",
            body: Some(r#"{"enabled":"yes"}"#),
            setup: Setup::None,
        },
        Case {
            name: "pacing-post-notjson",
            method: "POST",
            path: "/kimi-pacing",
            body: Some("{not json"),
            setup: Setup::None,
        },
        Case {
            // Go `Decoder` 读完第一个值就停：尾随垃圾不算错（P8b 审查 R3）。
            name: "pacing-post-trailing",
            method: "POST",
            path: "/kimi-pacing",
            body: Some(r#"{"enabled":true}x"#),
            setup: Setup::None,
        },
        Case {
            name: "pacing-post-false",
            method: "POST",
            path: "/kimi-pacing",
            body: Some(r#"{"enabled":false}"#),
            setup: Setup::None,
        },
        Case { name: "pacing-get-after-false", method: "GET", path: "/kimi-pacing", body: None, setup: Setup::None },
        Case { name: "activity-get", method: "GET", path: "/activity", body: None, setup: Setup::None },
        Case { name: "activity-limit-20", method: "GET", path: "/activity?limit=20", body: None, setup: Setup::None },
        Case { name: "activity-limit-bad", method: "GET", path: "/activity?limit=abc", body: None, setup: Setup::None },
        Case { name: "activity-limit-over", method: "GET", path: "/activity?limit=999", body: None, setup: Setup::None },
        Case { name: "inflight-get", method: "GET", path: "/inflight", body: None, setup: Setup::None },
        // 下面是**预置一条合成进行中请求**之后的快照：elapsed/idle 是 float64，
        // 两侧都在「注册后近乎立刻」取样 ⇒ gojson 都写 `0.0000xx` 一类的短形态。
        // 该用例已登记进 compare.mjs 的「数值尾数归一化」名单（只归一化数值尾数，
        // 键序/字段存在性/类型形态一律照比）。
        Case {
            name: "inflight-with-one",
            method: "GET",
            path: "/inflight",
            body: None,
            setup: Setup::InsertInflight,
        },
        // /quota：三条分支 + 缓存口径（uid / user_id / refresh / 负缓存）
        Case { name: "quota-no-uid", method: "GET", path: "/quota", body: None, setup: Setup::None },
        Case { name: "quota-no-uid-cache", method: "GET", path: "/quota", body: None, setup: Setup::None },
        Case {
            name: "quota-uid-param-name-probe",
            method: "GET",
            path: "/quota?uid=uid-not-in-pool",
            body: None,
            setup: Setup::None,
        },
        Case {
            name: "quota-uid-param-in-pool",
            method: "GET",
            path: "/quota?uid=uid-p8b3-synthetic",
            body: None,
            setup: Setup::None,
        },
        Case {
            name: "quota-both-params",
            method: "GET",
            path: "/quota?uid=uid-not-in-pool&user_id=uid-not-in-pool",
            body: None,
            setup: Setup::None,
        },
        Case {
            name: "quota-userid-not-in-pool",
            method: "GET",
            path: "/quota?user_id=uid-not-in-pool",
            body: None,
            setup: Setup::None,
        },
        Case {
            name: "quota-userid-in-pool",
            method: "GET",
            path: "/quota?user_id=uid-p8b3-synthetic",
            body: None,
            setup: Setup::None,
        },
        Case {
            name: "quota-userid-in-pool-cache",
            method: "GET",
            path: "/quota?user_id=uid-p8b3-synthetic",
            body: None,
            setup: Setup::None,
        },
        Case {
            name: "quota-userid-refresh",
            method: "GET",
            path: "/quota?user_id=uid-p8b3-synthetic&refresh=1",
            body: None,
            setup: Setup::None,
        },
        Case {
            name: "quota-empty-userid",
            method: "GET",
            path: "/quota?user_id=",
            body: None,
            setup: Setup::None,
        },
        // /accounts POST 五个分支（同一序列、同一初始池 ⇒ 结果可比）
        Case {
            name: "accounts-post-unknown-action",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"nope"}"#),
            setup: Setup::None,
        },
        Case {
            name: "accounts-post-notjson",
            method: "POST",
            path: "/accounts",
            body: Some("{not json"),
            setup: Setup::None,
        },
        Case {
            name: "accounts-post-setmodels-empty-uid",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"setmodels"}"#),
            setup: Setup::None,
        },
        Case {
            name: "accounts-post-setmodels",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"setmodels","user_id":"uid-p8b3-synthetic","models":["codely-core","KIMI-K3"]}"#),
            setup: Setup::None,
        },
        Case { name: "accounts-get-after-setmodels", method: "GET", path: "/accounts", body: None, setup: Setup::None },
        Case {
            name: "accounts-post-setglm",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"setglm","user_id":"uid-p8b3-synthetic","has_glm53":false}"#),
            setup: Setup::None,
        },
        Case {
            name: "accounts-post-toggle-off",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"toggle","user_id":"uid-p8b3-synthetic","enabled":false}"#),
            setup: Setup::None,
        },
        Case { name: "accounts-get-after-toggle", method: "GET", path: "/accounts", body: None, setup: Setup::None },
        Case {
            name: "accounts-post-toggle-on",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"toggle","user_id":"uid-p8b3-synthetic","enabled":true}"#),
            setup: Setup::None,
        },
        Case {
            name: "accounts-post-remove-unknown",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"remove","user_id":"uid-not-there"}"#),
            setup: Setup::None,
        },
        Case {
            // 本地账号删除守卫（Go `handlers_extra.go:64-68`）：两侧隔离 HOME 里都有
            // 同一份合成登录态，本地 sub 都是 `uid-p8b3-synthetic`，用它触发守卫；
            // 守卫拦住后池子不动，后续 GET 自愈回池，不污染。
            name: "accounts-post-remove-local",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"remove","user_id":"uid-p8b3-synthetic"}"#),
            setup: Setup::None,
        },
        Case {
            // Go 按 struct 解码：`user_id` 非 string 即整体失败（P8b 审查 R3）。
            name: "accounts-post-remove-nonstrid",
            method: "POST",
            path: "/accounts",
            body: Some(r#"{"action":"remove","user_id":123}"#),
            setup: Setup::None,
        },
        Case { name: "accounts-get-final", method: "GET", path: "/accounts", body: None, setup: Setup::None },
        Case { name: "stats-get-final", method: "GET", path: "/v1/stats", body: None, setup: Setup::None },
        Case { name: "models-get-final", method: "GET", path: "/v1/models", body: None, setup: Setup::None },
    ]
}

// ---------------------------------------------------------------- 响应记录

/// 一次响应的原始记录。
#[derive(Debug, Clone, PartialEq)]
pub struct Resp {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

/// 一次场景运行的完整结果。
#[derive(Debug, Clone)]
pub struct Report {
    /// 用例名 → 响应（**保持插入序**，便于逐条 diff）。
    pub cases: Vec<(String, Resp)>,
    /// 假上游观测量（计费命中次数等），供两侧对照。
    pub billing_hits: BTreeMap<String, usize>,
    pub cli_key_hits: usize,
    pub models_hits: usize,
    /// 假上游记录到的出站请求形态。
    pub seen: Vec<SeenReq>,
}

impl Report {
    /// 归一化后的 JSON（供 `.work/p8b3/` 落档与脚本 diff）。
    pub fn to_json(&self) -> Value {
        let mut cases = serde_json::Map::new();
        for (name, r) in &self.cases {
            cases.insert(
                name.clone(),
                serde_json::json!({
                    "status": r.status,
                    "content_type": r.content_type,
                    "body": r.body,
                }),
            );
        }
        serde_json::json!({
            "cases": Value::Object(cases),
            "probe": {
                "billing_hits": self.billing_hits,
                "cli_key_hits": self.cli_key_hits,
                "models_hits": self.models_hits,
                "seen": self.seen.iter().map(|s| serde_json::json!({
                    "method": s.method, "path": s.path,
                    "authorization": s.authorization, "accept": s.accept,
                })).collect::<Vec<_>>(),
            },
        })
    }
}

// ---------------------------------------------------------------- 环境隔离

/// 造一个隔离环境：临时 HOME（合成 `oauth_creds.json`）+ 临时数据目录。
/// **真实 `%USERPROFILE%` / 真实账号池一个字节都不碰**。
pub struct Env {
    pub home: PathBuf,
    pub data: PathBuf,
}

impl Env {
    /// 默认隔离环境：登录态 JWT 用无 salt 的 [`synthetic_jwt`]（与 Go 侧同形，
    /// 差分门禁按头全值比对——理由同上，别在这里加盐）。
    pub fn create(root: &Path) -> Env {
        let home = root.join("home");
        let data = root.join("data");
        let cli_dir = home.join(".codely-cli");
        std::fs::create_dir_all(&cli_dir).expect("建合成 HOME");
        std::fs::create_dir_all(&data).expect("建临时数据目录");
        let token = synthetic_jwt();
        std::fs::write(
            cli_dir.join("oauth_creds.json"),
            format!("{{\"access_token\":\"{token}\",\"user_id\":999001}}"),
        )
        .expect("写合成登录态");
        Env { home, data }
    }

    /// 同上，但合成 JWT 的 `sub` 由调用方给 —— P8b 第四片的账号池用例要它
    /// （本地号在池里的 `user_id` 就是登录态 JWT 的 `sub`，必须与 `tuanjie-accounts.json`
    /// 里那一条对得上，池里才不会多出一个身份不明的号）。`salt` 见 [`synthetic_jwt_for`]。
    pub fn create_with_sub(root: &Path, sub: &str) -> Env {
        Env::create_with_salt(root, sub, sub)
    }

    /// 带盐版本：同一 `sub` 在不同用例里拿到不同 token（假上游按 token 分辨账号）。
    pub fn create_with_salt(root: &Path, sub: &str, salt: &str) -> Env {
        let home = root.join("home");
        let data = root.join("data");
        let cli_dir = home.join(".codely-cli");
        std::fs::create_dir_all(&cli_dir).expect("建合成 HOME");
        std::fs::create_dir_all(&data).expect("建临时数据目录");
        let token = synthetic_jwt_for(sub, salt);
        std::fs::write(
            cli_dir.join("oauth_creds.json"),
            format!("{{\"access_token\":\"{token}\",\"user_id\":999001}}"),
        )
        .expect("写合成登录态");
        Env { home, data }
    }

    /// 把隔离环境装进子进程/本进程的环境变量（`USERPROFILE` + `PROXYDECK_DATA_DIR`）。
    pub fn apply(&self) {
        std::env::set_var("USERPROFILE", &self.home);
        std::env::set_var("PROXYDECK_DATA_DIR", &self.data);
    }

    /// 本环境里的合成 access_token（登录态那份）——P8b 第四片的取 key 登记要用。
    pub fn access_token(&self) -> String {
        let p = self.home.join(".codely-cli").join("oauth_creds.json");
        let text = std::fs::read_to_string(p).expect("读合成登录态");
        let v: Value = serde_json::from_str(&text).expect("合成登录态是合法 JSON");
        v["access_token"].as_str().expect("有 access_token").to_string()
    }
}

// ---------------------------------------------------------------- 体内值归一

/// 把易变字段替换成定点哨兵，其余**逐字节保留**：
///
/// * `/health`：`uptimeSec` 与 `cli_api_key` 的随机尾巴（本场景下 key 是常量，只会命中
///   `uptimeSec` 那一处）；
/// * 计费/会话时间戳等由用例本身的固定常量消除，不在这里兜。
///
/// 归一化只做**点替换**，不做重排：键序、数字形态、转义形态全部保留 —— 这样
/// 「Go 写 `2` 而 Rust 写 `2.0`」一类差异不会被抹掉。
pub fn normalize_body(body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let mut s = body.to_string();
    // /health 的 uptimeSec（进程启动到请求之间可能跨秒）
    if let Some(obj) = v.as_object() {
        if let Some(val) = obj.get("uptimeSec") {
            let needle = format!("\"uptimeSec\":{}", val);
            s = s.replace(&needle, "\"uptimeSec\":<UPTIME>");
        }
    }
    // /inflight 的 elapsed/idle：归一化成一个**定值数字**——它们取决于"注册到取样"的
    // 真实间隔，两侧不可能逐位相同。仍写成数字字面量（不是字符串）⇒ 类型形态照比，
    // 「把 elapsed 改成字符串」那类突变照样会被门禁抓到。
    if let Some(reqs) = v.get("requests").and_then(|r| r.as_array()) {
        if !reqs.is_empty() {
            for key in ["elapsed", "idle"] {
                let is_number = reqs
                    .iter()
                    .any(|r| r.get(key).map(|x| x.is_number()).unwrap_or(false));
                if is_number {
                    s = mask_number(&s, key);
                }
            }
        }
    }
    s
}

/// 把 `"key":<数字>` 的尾数抹成定值 `0.5`（保留"是数字"这一形态）。
fn mask_number(body: &str, key: &str) -> String {
    let prefix = format!("\"{key}\":");
    let Some(idx) = body.find(&prefix) else {
        return body.to_string();
    };
    let start = idx + prefix.len();
    let bytes = body.as_bytes();
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
        end += 1;
    }
    if end == start {
        return body.to_string();
    }
    format!("{}0.5{}", &body[..start], &body[end..])
}

