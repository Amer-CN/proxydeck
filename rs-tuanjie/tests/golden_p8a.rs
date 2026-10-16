//! P8a 黄金门禁 —— **八项红线的已知答案对照**（Go 侧独立复算，逐字节）。
//!
//! 基准 `tests/golden/golden-p8a-go.json` 由 Go 侧一次性采集器
//! `internal/tuanjie/zz_p8a_golden_test.go` 现采（跑完即删，采集器口径见简报 P8a 节）。
//! 加载顺序：先入库副本 `tests/golden/`，缺则回落 `.work/p8a/`（与 rs-bai 同规）。
//!
//! 覆盖：
//! 1. 签名向量 253 条（种子 hex / 两层 HMAC / 消息体 / base64url 逐字节）
//! 2. 请求体重排 21 条（字段序 + 默认字段 + 非流式删 stream + 转义口径）
//! 3. UA 模板 / 兜底常量 / 版本正则行为表
//! 4. 头集合（与 Go 实抓头名逐字对照）
//! 5. 换 key 路径与两个域常量
//! 6. 线上字节：Rust 出站 vs Go 出站实抓（**大小写/头序差异单独登记断言**）

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rs_tuanjie::client::Client;
use rs_tuanjie::headers::{litellm_headers, sorted_header_names};
use rs_tuanjie::reqshape::reshape_chat_body_with_cwd;
use rs_tuanjie::session::LitellmSession;
use rs_tuanjie::sign::{codely_signing_key, sign_litellm};
use rs_tuanjie::{Error, CLI_API_KEY_URL};

// ---------------------------------------------------------------- 基准加载

fn golden_dir() -> PathBuf {
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    if vendored.join("golden-p8a-go.json").exists() {
        return vendored;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".work/p8a")
}

fn load_golden() -> serde_json::Value {
    let p = golden_dir().join("golden-p8a-go.json");
    let bytes = std::fs::read(&p)
        .unwrap_or_else(|e| panic!("读不到黄金基准 {}: {e}（再生成见 tests/golden/README.md）", p.display()));
    eprintln!("黄金基准: {}", p.display());
    serde_json::from_slice(&bytes).expect("黄金基准是合法 JSON")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- 1. 签名

#[test]
fn sign_vectors_match_go_byte_for_byte() {
    let g = load_golden();
    let vecs = g["sign_vectors"].as_array().expect("sign_vectors 是数组");
    assert!(vecs.len() >= 200, "向量数偏少: {}", vecs.len());
    let mut checked = 0usize;
    let mut with_note = 0usize;
    for v in vecs {
        let path = v["path"].as_str().unwrap();
        let key = v["key"].as_str().unwrap();
        let unix = v["unix"].as_i64().unwrap();
        let want_sig = v["sig"].as_str().unwrap();
        let want_key = v["signing_key_hex"].as_str().unwrap();
        if v.get("note").is_some() {
            with_note += 1;
        }
        assert_eq!(
            sign_litellm(path, key, unix),
            want_sig,
            "签名不一致 path={path:?} key={key:?} unix={unix}"
        );
        assert_eq!(
            hex(&codely_signing_key(key)),
            want_key,
            "派生密钥不一致 key={key:?}"
        );
        checked += 1;
    }
    eprintln!(
        "签名对照：{checked} 条向量逐字节一致（其中 {} 条带 Go 侧既有 sign_test.go 参考值标注）",
        with_note
    );
}

#[test]
fn seed_and_kdf_intermediate_match_go() {
    let g = load_golden();
    assert_eq!(
        rs_tuanjie::sign::CODEY_SIGNING_SEED_HEX,
        g["ua"]["seed_hex"].as_str().unwrap(),
        "红线 1：种子 hex 逐字"
    );
    // k1 = HMAC(seed, "codely-signing-v1") 的中间值也对照（红线 2 的第一层）
    let seed = rs_tuanjie::sign::hex_decode(rs_tuanjie::sign::CODEY_SIGNING_SEED_HEX).unwrap();
    let k1 = rs_tuanjie::sign::hmac_sha256(&seed, b"codely-signing-v1");
    assert_eq!(hex(&k1), g["ua"]["k1_hex"].as_str().unwrap(), "k1 不一致");
}

// ---------------------------------------------------------------- 2. 重排

#[test]
fn reshape_cases_match_go_byte_for_byte() {
    let g = load_golden();
    let cases = g["reshape_cases"].as_array().expect("reshape_cases 是数组");
    assert!(cases.len() >= 20, "用例数偏少: {}", cases.len());
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let input = c["in"].as_str().unwrap().as_bytes();
        let cwd = c["cwd"].as_str().unwrap();
        let want = c["out"].as_str().unwrap().as_bytes();
        let sess = LitellmSession {
            id: c["sess"]["id"].as_str().unwrap().to_string(),
            conversation_id: c["sess"]["conversation_id"].as_str().unwrap().to_string(),
            prompt_seq: c["sess"]["prompt_seq"].as_i64().unwrap(),
        };
        let got = reshape_chat_body_with_cwd(input, Some(&sess), cwd);
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(want),
            "[{name}] 重排结果与 Go 不一致"
        );
    }
    eprintln!("重排对照：{} 条用例逐字节一致", cases.len());
}

// ---------------------------------------------------------------- 3. UA

#[test]
fn ua_constants_and_version_regex_match_go() {
    let g = load_golden();
    assert_eq!(
        rs_tuanjie::ua::CLI_USER_AGENT_FALLBACK,
        g["ua"]["fallback"].as_str().unwrap(),
        "红线 6：兜底 UA 逐字"
    );
    for c in g["version_regex"].as_array().unwrap() {
        let v = c["v"].as_str().unwrap();
        let want = c["ok"].as_bool().unwrap();
        assert_eq!(
            rs_tuanjie::ua::is_valid_cli_version(v),
            want,
            "版本正则行为不一致: {v:?}"
        );
    }
    // 本机实采对照：探测结果与 Go 同机采的样子一致（若 npm 包已升级，这里给出可见提示）
    let detected = rs_tuanjie::ua::detect_local_cli_version();
    let golden_detected = g["ua"]["detected_version"].as_str().unwrap();
    match &detected {
        Some(v) if v == golden_detected => {
            eprintln!("UA 探测：与 Go 同机采值一致（{v}）");
        }
        other => {
            eprintln!(
                "注意：本机 CLI 版本已变（Go 采集时 {golden_detected}，现在 {other:?}）——\
                 非失败项：UA 版本号随本机 npm 包走的语义不变（红线 6 的模板用下面两条断言锁死）"
            );
        }
    }
    if let Some(v) = detected {
        assert_eq!(
            rs_tuanjie::ua::resolve_cli_user_agent(),
            format!("codely-cli/{v} (win32; x64)"),
            "红线 6：UA 模板"
        );
        assert!(
            g["ua"]["resolved"].as_str().unwrap().starts_with("codely-cli/"),
            "Go 侧 resolved 形态"
        );
    }
}

// ---------------------------------------------------------------- 4. 头集合

#[test]
fn header_set_matches_go_capture() {
    let g = load_golden();
    let want: Vec<(String, String)> = g["header_set"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["name"].as_str().unwrap().to_string(),
                h["value"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    let sess_id = "11111111-2222-4333-8444-555555555555";
    let key = "sk-test-key-1234567890";
    // 签名值含时间戳，用 golden 里那条签名的时间戳回放
    let want_sig = want
        .iter()
        .find(|(n, _)| n == "X-Codely-Signature")
        .map(|(_, v)| v.clone())
        .unwrap();
    let ts: i64 = want_sig.split('.').nth(1).unwrap().parse().unwrap();
    let got = litellm_headers(
        "/v1/chat/completions",
        key,
        sess_id,
        ts,
        &rs_tuanjie::ua::cli_user_agent_from(Some("1.0.0-rc.60".into())),
    );

    let want_names: Vec<String> = want.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(
        sorted_header_names(&got),
        want_names,
        "头名集合与 Go 实抓不一致"
    );
    for (name, value) in &want {
        let got_value = got.iter().find(|(n, _)| n == name).map(|(_, v)| v);
        assert_eq!(got_value, Some(value), "头 {name} 取值不一致");
    }
    for (name, _) in &got {
        assert!(
            !name.starts_with("X-DashScope-") && !name.starts_with("x-b3-"),
            "红线 7：不应出现 {name}"
        );
    }
    eprintln!("头集合对照：{} 个头名与取值逐字一致", want.len());
}

// ---------------------------------------------------------------- 5. 端点常量

#[test]
fn endpoints_and_timeouts_match_go() {
    let g = load_golden();
    assert_eq!(
        CLI_API_KEY_URL,
        g["ua"]["cli_api_key_url"].as_str().unwrap(),
        "红线 5：换 key 路径"
    );
    assert!(!CLI_API_KEY_URL.contains('?'), "红线 5：不带 ?teamId");
    // 生产客户端默认就打到这条路径（不是只在常量里对）
    assert_eq!(Client::new().unwrap().cli_api_key_url(), CLI_API_KEY_URL);
    assert_eq!(rs_tuanjie::CODEY_API_BASE, g["ua"]["codely_base"].as_str().unwrap());
    assert_eq!(
        rs_tuanjie::LITELLM_API_BASE,
        g["ua"]["litellm_base"].as_str().unwrap()
    );
    assert_eq!(
        rs_tuanjie::KEY_CACHE_TTL.as_secs() as i64,
        g["ua"]["key_cache_ttl_s"].as_i64().unwrap()
    );
    assert_eq!(
        rs_tuanjie::KEY_FETCH_TIMEOUT.as_secs() as i64,
        g["ua"]["key_fetch_tmo_s"].as_i64().unwrap()
    );
    assert_eq!(
        rs_tuanjie::DEFAULT_TIMEOUT.as_secs() as i64,
        g["ua"]["default_tmo_s"].as_i64().unwrap()
    );
}

// ---------------------------------------------------------------- 6. 线上字节

/// 极简假上游：按路径出应答，并把**收到的原始请求字节**记下来。
struct FakeUpstream {
    addr: String,
    captured: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FakeUpstream {
    async fn start(routes: BTreeMap<String, (u16, &'static str)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let sink = sink.clone();
                let routes = routes.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let mut head_end = None;
                    while head_end.is_none() {
                        let n = match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            head_end = Some(i + 4);
                        }
                    }
                    let Some(head_end) = head_end else { return };
                    let mut clen = 0usize;
                    for line in String::from_utf8_lossy(&buf[..head_end]).lines() {
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            clen = v.trim().parse().unwrap_or(0);
                        }
                    }
                    while buf.len() < head_end + clen {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    sink.lock().unwrap().push(buf.clone());
                    let line = String::from_utf8_lossy(&buf[..head_end]);
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    let (status, body) = routes
                        .get(&path)
                        .copied()
                        .unwrap_or((404, "{\"error\":\"no route\"}"));
                    let reason = if status == 200 { "OK" } else { "Unauthorized" };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        FakeUpstream { addr, captured }
    }

    fn http_base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Vec<u8>> {
        self.captured.lock().unwrap().clone()
    }
}

fn test_session() -> LitellmSession {
    LitellmSession {
        id: "11111111-2222-4333-8444-555555555555".into(),
        conversation_id: "litellm_conversation_1700000000000_abc123".into(),
        prompt_seq: 3,
    }
}

fn synthetic_client(upstream: &FakeUpstream) -> Client {
    let loader: rs_tuanjie::client::TokenLoader =
        Arc::new(move || Ok("AT-SYNTHETIC-NOT-REAL".to_string()));
    Client::new()
        .unwrap()
        .with_http(reqwest::Client::new())
        .with_bases(&upstream.http_base(), &upstream.http_base())
        .with_token_loader(loader)
        .with_user_agent_for_test("codely-cli/1.0.0-rc.58 (win32; x64)")
}

fn head_text(raw: &[u8]) -> String {
    let i = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..i]).into_owned()
}

fn body_bytes(raw: &[u8]) -> &[u8] {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => &raw[i + 4..],
        None => &[],
    }
}

/// 从请求头里取某头（大小写不敏感）。
fn header_value(head: &str, name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim().to_ascii_lowercase() == lower {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

#[tokio::test]
async fn key_fetch_hits_red_line_five_path_with_go_header_set() {
    let mut routes = BTreeMap::new();
    routes.insert(
        "/api/api-token/cli-api-key".to_string(),
        (200u16, "{\"cli_api_key\":\"sk-synthetic-key\"}"),
    );
    let up = FakeUpstream::start(routes).await;
    let client = synthetic_client(&up);

    let key = client.fetch_key().await.expect("换 key 应成功");
    assert_eq!(key, "sk-synthetic-key");

    let reqs = up.requests();
    assert_eq!(reqs.len(), 1, "换 key 只打一站");
    let head = head_text(&reqs[0]);
    // 请求行：GET + 红线 5 的路径（无 ?teamId、不换源）
    assert!(
        head.starts_with("GET /api/api-token/cli-api-key HTTP/1.1\r\n"),
        "换 key 请求行不符: {head}"
    );
    assert!(!head.contains("teamId"), "红线 5：不得带 ?teamId");
    assert_eq!(
        header_value(&head, "authorization").as_deref(),
        Some("Bearer AT-SYNTHETIC-NOT-REAL")
    );
    assert_eq!(
        header_value(&head, "accept").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        header_value(&head, "user-agent").as_deref(),
        Some("codely-cli/1.0.0-rc.58 (win32; x64)")
    );
    eprintln!("换 key 线上实抓:\n{head}");
}

#[tokio::test]
async fn forward_wire_matches_go_body_and_header_set() {
    let mut routes = BTreeMap::new();
    routes.insert(
        "/api/api-token/cli-api-key".to_string(),
        (200u16, "{\"cli_api_key\":\"sk-synthetic-key\"}"),
    );
    routes.insert("/v1/chat/completions".to_string(), (200u16, "{}"));
    let up = FakeUpstream::start(routes).await;
    let client = synthetic_client(&up);

    let sess = test_session();
    let body = reshape_chat_body_with_cwd(
        br#"{"model":"codely-flash","messages":[{"role":"user","content":"OK"}],"stream":false}"#,
        Some(&sess),
        r"C:\pd\exe",
    );
    let resp = client
        .forward(
            reqwest::Method::POST,
            "/v1/chat/completions",
            body.clone(),
            &sess,
        )
        .await
        .expect("转发应成功");
    assert_eq!(resp.status(), 200);

    let reqs = up.requests();
    let fwd = reqs.last().expect("有转发请求");
    let head = head_text(fwd);
    assert!(
        head.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
        "请求行不符: {head}"
    );
    // body 逐字节 = 重排结果
    assert_eq!(body_bytes(fwd), body.as_slice(), "出站体与重排结果不一致");

    // 头集合（大小写不敏感）与 Go 实抓的头名集合逐个对齐
    let go_wire = std::fs::read_to_string(golden_dir().join("go-outbound-wire.txt"))
        .expect("Go 出站实抓缺失（tests/golden/go-outbound-wire.txt）");
    let go_head = head_text(go_wire.as_bytes());
    let mut go_names: Vec<String> = go_head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':').map(|(k, _)| k.trim().to_ascii_lowercase()))
        .collect();
    let mut rust_names: Vec<String> = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':').map(|(k, _)| k.trim().to_ascii_lowercase()))
        .collect();
    go_names.sort();
    rust_names.sort();
    assert_eq!(
        rust_names, go_names,
        "头名集合（大小写不敏感）应与 Go 实抓一致"
    );

    // 签名：线上值必须能由 path/key/ts 复算出来
    let sig = header_value(&head, "x-codely-signature").expect("有签名头");
    let parts: Vec<&str> = sig.split('.').collect();
    assert_eq!(parts.len(), 3, "签名形态 v1.<ts>.<b64url>: {sig}");
    assert_eq!(parts[0], "v1");
    let ts: i64 = parts[1].parse().expect("时间戳是整数");
    let key_used = header_value(&head, "authorization")
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap()
        .to_string();
    assert_eq!(
        sig,
        sign_litellm("/v1/chat/completions", &key_used, ts),
        "线上签名与 path/key/ts 复算不一致"
    );
    assert_eq!(key_used, "sk-synthetic-key");
    // 会话三元组：头 = 请求体 litellm_session_id = prompt_cache_key
    assert_eq!(
        header_value(&head, "x-litellm-session-id").as_deref(),
        Some(sess.id.as_str())
    );
    let body_json: serde_json::Value = serde_json::from_slice(body_bytes(fwd)).unwrap();
    assert_eq!(body_json["litellm_session_id"], sess.id);
    assert_eq!(body_json["prompt_cache_key"], sess.id);

    // —— 已登记的平台级差异：Go 写头名用 Go 的规范化大小写，hyper 一律小写 ——
    let title_case = ["X-Litellm-Session-Id", "X-Codely-Signature", "Authorization"];
    for name in title_case {
        assert!(
            head.contains(&format!("{}:", name.to_ascii_lowercase()))
                || header_value(&head, name).is_some(),
            "hyper 应写出小写头名: {name}"
        );
        assert!(
            !head.contains(&format!("\r\n{name}:")),
            "hyper 不该写出 Go 式 Title-Case 头名: {name}"
        );
    }
    eprintln!("转发线上实抓:\n{head}");
    eprintln!("Go 侧实抓头段:\n{go_head}");
}

#[tokio::test]
async fn forward_retries_once_after_401_and_invalidates_key() {
    // 第一次转发 401 → 清缓存重换 key → 第二次 200
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let counter = Arc::new(Mutex::new(0usize));
    let c2 = counter.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let c2 = c2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let mut head_end = None;
                while head_end.is_none() {
                    let n = match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        head_end = Some(i + 4);
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let (status, body) = if head.contains("/api/api-token/cli-api-key") {
                    (200u16, "{\"cli_api_key\":\"sk-synthetic-key\"}")
                } else {
                    let mut n = c2.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        (401u16, "{\"error\":\"unauthorized\"}")
                    } else {
                        (200u16, "{}")
                    }
                };
                let reason = if status == 200 { "OK" } else { "Unauthorized" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });

    let base = format!("http://{addr}");
    let loader: rs_tuanjie::client::TokenLoader = Arc::new(|| Ok("AT-SYNTHETIC".into()));
    let client = Client::new()
        .unwrap()
        .with_http(reqwest::Client::new())
        .with_bases(&base, &base)
        .with_token_loader(loader)
        .with_user_agent_for_test("codely-cli/1.0.0-rc.58 (win32; x64)");
    let resp = client
        .forward(
            reqwest::Method::POST,
            "/v1/chat/completions",
            b"{}".to_vec(),
            &test_session(),
        )
        .await
        .expect("401 后重换 key 应拿到 200");
    assert_eq!(resp.status(), 200);
    assert_eq!(*counter.lock().unwrap(), 2, "应恰好重试一次");
}

#[tokio::test]
async fn key_fetch_errors_are_reported_with_status_and_body() {
    let mut routes = BTreeMap::new();
    routes.insert(
        "/api/api-token/cli-api-key".to_string(),
        (403u16, "{\"error\":\"forbidden\"}"),
    );
    let up = FakeUpstream::start(routes).await;
    let client = synthetic_client(&up);
    match client.fetch_key().await {
        Err(Error::FetchKeyStatus { status, body }) => {
            assert_eq!(status, 403);
            assert!(body.contains("forbidden"), "{body}");
        }
        other => panic!("应报 FetchKeyStatus，得到 {other:?}"),
    }
}

#[tokio::test]
async fn key_fetch_rejects_empty_key_like_go() {
    let mut routes = BTreeMap::new();
    routes.insert(
        "/api/api-token/cli-api-key".to_string(),
        (200u16, "{\"cli_api_key\":\"\"}"),
    );
    let up = FakeUpstream::start(routes).await;
    let client = synthetic_client(&up);
    match client.fetch_key().await {
        Err(Error::FetchKeyBody(body)) => assert!(body.contains("cli_api_key"), "{body}"),
        other => panic!("空 key 应报 FetchKeyBody，得到 {other:?}"),
    }
}

#[tokio::test]
async fn api_key_is_cached_between_calls() {
    let mut routes = BTreeMap::new();
    routes.insert(
        "/api/api-token/cli-api-key".to_string(),
        (200u16, "{\"cli_api_key\":\"sk-synthetic-key\"}"),
    );
    let up = FakeUpstream::start(routes).await;
    let client = synthetic_client(&up);
    let a = client.api_key_cached().await.unwrap();
    let b = client.api_key_cached().await.unwrap();
    assert_eq!(a, b);
    assert_eq!(up.requests().len(), 1, "第二次应命中缓存（24h TTL）");
    client.invalidate_key();
    let _ = client.api_key_cached().await.unwrap();
    assert_eq!(up.requests().len(), 2, "invalidate 后应重新换取");
}
