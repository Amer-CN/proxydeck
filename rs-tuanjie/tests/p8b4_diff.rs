//! P8b 第四片的**差分对照**：把 Rust 侧 `handleChat` 打在与 Go 采集器
//! （`.work/p8b4/zz_p8b4_collect_test.go`，临时文件跑完即删；留档同目录）**逐条相同**的
//! 场景上，与真 Go 采收的 `tests/golden/golden-p8b4-go.json` 比对：
//! `{status, content_type, body}` + `Retry-After`/`Cache-Control` + 假上游命中序与逐号命中数。
//!
//! 场景（与采集器 `p8b4Scenarios()` 一一对应）：
//!
//! 1. `chat-200` —— 池路径 200 原样透传 + usage 入账
//! 2. `chat-429-switch-then-200` —— 429 即时换号（A→B→C 健康号）
//! 3. `chat-402-swap` —— 402 禁用该号并换号重发
//! 4. `chat-retry-5xx` —— 502 可重试报文：首发 + 3 次重发（4 次上游）
//! 5. `chat-noretry-400` —— `invalid model name` 确定性 400：**不重发**
//! 6. `chat-empty-then-200` —— 池路径**不做**空响应嗅探：首份 200 空体原样透传
//!
//! 纪律同 `p8b4_chat_behavior.rs`：合成账号 + 临时 HOME/数据目录 + 假上游，
//! 真实凭据一个字节不读、不碰 8788。

#[path = "p8b3_scenario/mod.rs"]
mod scenario;

use std::sync::{Arc, Mutex, OnceLock};

use axum::http::header;

use rs_tuanjie::server::{router, AppState};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn run_locked<F, Fut, T>(f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 current-thread 运行时");
    let out = rt.block_on(f());
    drop(guard);
    out
}

fn shared_upstream() -> &'static scenario::FakeUpstream {
    static UP: OnceLock<scenario::FakeUpstream> = OnceLock::new();
    UP.get_or_init(|| scenario::FakeUpstream::start(vec![]))
}

/// 金标基准：仓库内优先，缺则回落 `.work/rust-rewrite`。
fn golden_path() -> std::path::PathBuf {
    let in_repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/golden-p8b4-go.json");
    if in_repo.exists() {
        return in_repo;
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../.work/p8b4/golden-p8b4-go.json")
}

#[derive(Debug, Clone)]
struct Golden {
    name: String,
    request: String,
    status: u16,
    content_type: String,
    body: String,
    retry_after: String,
    cache_control: String,
    chat_order: Vec<String>,
    chat_calls: std::collections::BTreeMap<String, i64>,
}

fn load_golden() -> Vec<Golden> {
    let p = golden_path();
    let text = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("读不到金标基准 {}：{e}", p.display()));
    let v: serde_json::Value = serde_json::from_str(&text).expect("金标基准是合法 JSON");
    v["scenarios"]
        .as_array()
        .expect("scenarios 是数组")
        .iter()
        .map(|s| Golden {
            name: s["name"].as_str().unwrap_or("").to_string(),
            // 金标自带请求体（缺则回落默认非流式体——老 6 场景无此键，行为不变）。
            request: s["request"].as_str().unwrap_or(
                r#"{"model":"KIMI-K3","messages":[{"role":"user","content":"hi"}]}"#,
            ).to_string(),
            status: s["status"].as_u64().unwrap_or(0) as u16,
            content_type: s["content_type"].as_str().unwrap_or("").to_string(),
            body: s["body"].as_str().unwrap_or("").to_string(),
            retry_after: s["retry_after"].as_str().unwrap_or("").to_string(),
            cache_control: s["cache_control"].as_str().unwrap_or("").to_string(),
            chat_order: s["chat_order"]
                .as_array()
                .map(|a| a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
                .unwrap_or_default(),
            chat_calls: s["chat_calls"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0)))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// 一次场景运行的结果（与 Go 采集器记录的形状对齐）。
struct Outcome {
    status: u16,
    content_type: String,
    body: String,
    retry_after: String,
    cache_control: String,
    all_headers: Vec<String>,
    chat_order: Vec<String>,
    chat_calls: std::collections::BTreeMap<String, i64>,
}

/// 场景描述（与 Go 采集器的 `p8b4Scenarios()` 一一对应）。
struct Case {
    name: &'static str,
    accounts: &'static [&'static str],
    pacing: bool,
    /// `key → 逐次应答`（越界重复最后一条）；key 命名与 Go 侧同源：`sk-key-<uid>`。
    script: Vec<(&'static str, Vec<(u16, &'static str, &'static str)>)>,
    /// 入站请求体；默认 `DEFAULT_REQ`（非流式），流式场景单独给（含 `"stream":true`）。
    request: &'static str,
}

const DEFAULT_REQ: &str = r#"{"model":"KIMI-K3","messages":[{"role":"user","content":"hi"}]}"#;

fn cases() -> Vec<Case> {
    const KIMI_OK: &str = concat!(
        r#"{"id":"chatcmpl-1","object":"chat.completion","model":"KIMI-K3","#,
        r#""choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"#,
        r#""usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#
    );
    const RATE: (u16, &str, &str) = (429, "1", r#"{"error":{"message":"rate limited"}}"#);
    const EMPTY_200: &str = concat!(
        r#"{"id":"x","object":"chat.completion","model":"KIMI-K3","#,
        r#""choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}"#
    );
    // 流式 SSE 体（与 Go 采集器逐字节同源）：两块 delta + 末块带 usage + [DONE]。
    const SSE_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"，世界\"}}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":6,\"total_tokens\":18}}\n\ndata: [DONE]\n\n";
    const STREAM_REQ: &str =
        r#"{"model":"KIMI-K3","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    vec![
        Case {
            name: "chat-200",
            accounts: &["uid-a"],
            pacing: false,
            request: DEFAULT_REQ,
            script: vec![("sk-key-uid-a", vec![(200, "", KIMI_OK)])],
        },
        Case {
            name: "chat-429-switch-then-200",
            accounts: &["uid-a", "uid-b", "uid-c"],
            pacing: true,
            request: DEFAULT_REQ,
            script: vec![
                ("sk-key-uid-a", vec![RATE]),
                ("sk-key-uid-b", vec![RATE]),
                ("sk-key-uid-c", vec![(200, "", KIMI_OK)]),
            ],
        },
        Case {
            name: "chat-402-swap",
            accounts: &["uid-a", "uid-b"],
            pacing: false,
            request: DEFAULT_REQ,
            script: vec![
                (
                    "sk-key-uid-a",
                    vec![(402, "", r#"{"error":{"message":"budget_exceeded"}}"#)],
                ),
                ("sk-key-uid-b", vec![(200, "", KIMI_OK)]),
            ],
        },
        Case {
            name: "chat-retry-5xx",
            accounts: &["uid-a"],
            pacing: false,
            request: DEFAULT_REQ,
            script: vec![(
                "sk-key-uid-a",
                vec![(502, "", r#"{"error":{"message":"bad gateway"}}"#)],
            )],
        },
        Case {
            name: "chat-noretry-400",
            accounts: &["uid-a"],
            pacing: false,
            request: DEFAULT_REQ,
            script: vec![(
                "sk-key-uid-a",
                vec![(
                    400,
                    "",
                    r#"{"error":{"message":"Invalid model name passed in model=None"}}"#,
                )],
            )],
        },
        Case {
            name: "chat-empty-then-200",
            accounts: &["uid-a"],
            pacing: false,
            request: DEFAULT_REQ,
            script: vec![(
                "sk-key-uid-a",
                vec![(200, "", EMPTY_200), (200, "", KIMI_OK)],
            )],
        },
        Case {
            // 池路径流式分支（P8b 审查 P1 的门禁锁）：与 Go 采集器同场景同输入，
            // 下游重组字节逐字节比对（块边界不比——TCP 分段两侧天然不同）。
            name: "chat-stream-1",
            accounts: &["uid-a"],
            pacing: false,
            request: STREAM_REQ,
            script: vec![("sk-key-uid-a", vec![(200, "", SSE_BODY)])],
        },
    ]
}

async fn run_case(case: &Case, tag: &str) -> Outcome {
    let root = std::env::temp_dir().join(format!(
        "rs-tuanjie-p8b4diff-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建临时根目录");
    let env = scenario::Env::create_with_salt(&root, case.accounts[0], tag);
    env.apply();

    let mut entries: Vec<String> = Vec::new();
    let mut tokens: Vec<(String, String)> = Vec::new();
    for (i, id) in case.accounts.iter().enumerate() {
        let token = scenario::synthetic_jwt_for(id, tag);
        let source = if i == 0 { "local" } else { "pool" };
        let at = if i == 0 { "" } else { token.as_str() };
        entries.push(format!(
            "{{\"user_id\":\"{id}\",\"access_token\":\"{at}\",\"enabled\":true,\
              \"has_glm53\":true,\"source\":\"{source}\"}}"
        ));
        tokens.push((id.to_string(), token));
    }
    std::fs::write(
        root.join("data").join("tuanjie-accounts.json"),
        format!("{{\"accounts\":[{}]}}", entries.join(",")),
    )
    .expect("写合成账号池");

    let up = shared_upstream();
    for (id, token) in &tokens {
        up.set_token_key(token, &format!("sk-key-{id}"));
    }
    // 收集本次场景的观测量：先清脚本，再登记本场景的 key（其余 key 未登记 ⇒ 404）
    let mut wanted: Vec<String> = Vec::new();
    for (key, script) in &case.script {
        up.set_chat_script(key, script.clone());
        wanted.push((*key).to_string());
    }
    let before_order: Vec<String> = up.chat_order();    let before_calls: std::collections::BTreeMap<String, i64> = wanted
        .iter()
        .map(|k| (k.clone(), up.chat_calls(k) as i64))
        .collect();

    rs_tuanjie::client::set_test_bases(up.base(), up.base());
    let st = Arc::new(AppState::new().expect("构造 AppState"));
    if case.pacing {
        st.pacer.set_enabled(true);
        st.chat_timing
            .set_switch_backoff_for_test(std::time::Duration::from_millis(120));
    }

    // 起一个**真 HTTP 服务**（回环临时端口）再用真 HTTP 客户端打它：`oneshot` 会在
    // 这一层把 `Cache-Control` 吃掉（实测：`cors` 中间件出来时头还在，`oneshot` 的返回值里
    // 就没了），于是「响应头有没有对齐」就测不到了。Go 采集器打的是真 handler + Recorder，
    // 这一侧也必须过真网络才可比。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑临时端口");
    let addr = listener.local_addr().expect("local_addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router(st)).await;
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("建 HTTP 客户端");
    let resp = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(case.request)
        .send()
        .await
        .expect("打本地服务");
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let retry_after = resp
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let cache_control = resp
        .headers()
        .get("Cache-Control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let all_headers: Vec<String> = resp
        .headers()
        .iter()
        .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("?")))
        .collect();
    let body = resp.bytes().await.expect("读响应体").to_vec();
    let body = String::from_utf8_lossy(&body).into_owned();
    server.abort();

    let full_order: Vec<String> = up.chat_order();
    let mine: Vec<String> = full_order[before_order.len()..]
        .iter()
        .filter(|k| wanted.contains(k))
        .cloned()
        .collect();
    let calls: std::collections::BTreeMap<String, i64> = wanted
        .iter()
        .map(|k| {
            let now = up.chat_calls(k) as i64;
            (k.clone(), now - before_calls.get(k).copied().unwrap_or(0))
        })
        .collect();

    let _ = std::fs::remove_dir_all(&root);
    Outcome {
        status,
        content_type,
        body,
        retry_after,
        cache_control,
        all_headers,
        chat_order: mine,
        chat_calls: calls,
    }
}

#[test]
fn chat_pool_path_matches_the_go_golden() {
    run_locked(|| async {
        let golden = load_golden();
        let cases = cases();
        assert_eq!(
            golden.len(),
            cases.len(),
            "金标基准场景数应与本文件一致（基准过期？重跑 Go 采集器）"
        );
        let mut checked = 0usize;
        for (g, case) in golden.iter().zip(cases.iter()) {
            assert_eq!(g.name, case.name, "场景顺序必须与金标一致");
            // 输入同源守卫：本侧实际发出的请求体必须与金标记录的 Go 侧输入逐字节一致，
            // 否则"输出一致"没有意义（各自打了不同的输入）。
            assert_eq!(
                case.request, g.request,
                "[{}] 输入请求体与金标记录不一致（两侧输入分叉）",
                g.name
            );
            let got = run_case(case, &case.name.replace('-', "_")).await;
            assert_eq!(
                got.status, g.status,
                "[{}] status 分歧：rust={} go={}",
                g.name, got.status, g.status
            );
            assert_eq!(
                got.content_type, g.content_type,
                "[{}] content-type 分歧",
                g.name
            );
            // 2026-10-18 收尾任务①：上游非 200 的错误体现在会抽出 `error.message`
            // （人话），不再整串转义进我们的 message。金标是 Go 旧行为的逐字节记录，
            // **故意保留原件**；这两条显式断言新的期望体，其余场景仍逐字节对照。
            let humanized = match g.name.as_str() {
                "chat-retry-5xx" => Some(
                    "{\"error\":{\"message\":\"bad gateway\",\"type\":\"api_error\"}}\n",
                ),
                "chat-noretry-400" => Some(
                    "{\"error\":{\"message\":\"Invalid model name passed in model=None\",\"type\":\"api_error\"}}\n",
                ),
                _ => None,
            };
            match humanized {
                Some(want) => assert_eq!(got.body, want, "[{}] 人性化后的响应体", g.name),
                None => assert_eq!(got.body, g.body, "[{}] 响应体逐字节分歧", g.name),
            }
            assert_eq!(
                got.retry_after, g.retry_after,
                "[{}] Retry-After 分歧",
                g.name
            );
            assert_eq!(
                got.cache_control, g.cache_control,
                "[{}] Cache-Control 分歧（Go 在 `copyHeader` 后显式设 no-cache）；rust_headers={:?}",
                g.name, got.all_headers
            );
            assert_eq!(
                got.chat_order, g.chat_order,
                "[{}] 上游命中序分歧（换号/重发次数与顺序）",
                g.name
            );
            assert_eq!(
                got.chat_calls, g.chat_calls,
                "[{}] 逐号命中数分歧",
                g.name
            );
            checked += 1;
        }
        eprintln!(
            "P8b 第四片差分对照：{} 场景逐条一致（status / content-type / body 逐字节 / \
             Retry-After / 上游命中序与逐号命中数）",
            checked
        );
    })
}

/// 反向验证（非空转）：把 `MAX_POOL_SWITCHES` 从 3 改成 1 → `chat-429-switch-then-200`
/// 的上游命中序会少一跳，本差分立即变红。此处只用**常量形状**断言锁住「门禁确实在读它」。
#[test]
fn golden_files_exist() {
    let p = golden_path();
    assert!(p.exists(), "金标基准必须存在：{}", p.display());
    let g = load_golden();
    assert_eq!(g.len(), 7, "7 个场景");
    assert!(
        g.iter()
            .any(|x| x.name == "chat-429-switch-then-200" && x.chat_order.len() == 3),
        "429 换号场景必须记到三跳"
    );
    assert!(
        g.iter().any(|x| x.name == "chat-stream-1"
            && x.request.contains("\"stream\":true")
            && x.body.contains("data: [DONE]")),
        "流式场景必须带着 stream 请求与 SSE 体"
    );
    // 金标是 Go 旧行为：两条上游错误场景仍记录着「整串转义」的 message。
    // 收尾任务① 之后 Rust 侧故意人性化，差异由 `chat_pool_path_matches_the_go_golden`
    // 显式断言（金标原件不改）。
    for name in ["chat-retry-5xx", "chat-noretry-400"] {
        let g = g.iter().find(|x| x.name == name).expect("金标场景存在");
        assert!(
            g.body.contains("\\\"error\\\":"),
            "[{name}] 金标应仍是 Go 旧行为（整串转义）"
        );
    }
    // 基准里的 200 场景必须带 `Cache-Control: no-cache`（否则那条断言形同虚设——
    // 采集器第一版用 `rec.Result().Header` 取到的是空串，正是这么漏过去的）。
    assert!(
        g.iter()
            .filter(|x| x.status == 200)
            .all(|x| x.cache_control == "no-cache"),
        "200 场景的头被采空了？重跑 Go 采集器"
    );
}
