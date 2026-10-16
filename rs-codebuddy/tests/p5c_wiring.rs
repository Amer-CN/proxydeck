//! p5c_wiring.rs —— P5c 收口的两处**行为级**覆盖缺口（P5b 审查点名）：
//!
//! ① **chat 路径的 `Inc/DecLoad` 接线**：此前只有 `pool.rs` 的方法直调单测，接线回归
//!    （例如换号时忘了 `Dec` 旧号）发现不了。这里从 HTTP 路径发真请求，在**请求进行中**
//!    读 `/v1/accounts` 快照，断言 `inflight` 真的被 Inc、请求结束后真的回到 0；
//!    另覆盖换号路径（401 → 换活号）后**两个号的 inflight 都归零**（旧号的 Dec 不能漏）。
//! ② **499 的取消信号**（差异 13）：Go 的 `r.Context().Done()` 在 11133 退避等待期间可中断
//!    ⇒ 客户端断开后**不再打上游**。Go 侧事实由采集器采收在黄金 `meta.p5c_cancel`，
//!    本测试对同一场景比对「上游命中数」。
//!
//! 两个测试都用**生产装配**（真凭据 + 真池 + 真 failover），域名/夹具与 P5b 黄金同款。

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use serde_json::{json, Value};

use rs_codebuddy::region::Region;

fn field<'a>(v: &'a Value, k: &str) -> &'a Value {
    v.get(k).unwrap_or(&Value::Null)
}

/// 合成 CN 登录态（与 P5b/P5c 黄金采集器同款；mtime 决定池内顺序：新的在前）。
fn cn_auth_files() -> Value {
    let file = |uid: &str, counter: u64| {
        json!({
            "name": format!("workbuddy-desktop-{uid}.info"),
            "body": format!(
                "{{\"auth\":{{\"accessToken\":\"tok-{uid}\",\"domain\":\"codebuddy.cn\",\"expiresAt\":4102416000000}},\"account\":{{\"uid\":\"{uid}\",\"nickname\":\"nick-{uid}\",\"enterpriseId\":\"ent\"}}}}"
            ),
            "mtime_unix_ms": 1_792_000_000_000u64 + counter,
        })
    };
    json!([file("u-wire", 2), file("u-wire2", 1)])
}

/// 一个可控的慢上游：按 `X-User-Id` 决定应答，且在应答前先睡 `delay_ms`
/// （用来在「请求进行中」的窗口里读 `/v1/accounts` 快照）。
struct SlowUpstream {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl SlowUpstream {
    async fn start(delay_ms: u64, deny_uids: &[&str]) -> SlowUpstream {
        let hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = hits.clone();
        let deny: Vec<String> = deny_uids.iter().map(|s| s.to_string()).collect();
        let app = axum::Router::new().fallback(axum::routing::any(
            move |req: axum::extract::Request| {
                let sink = sink.clone();
                let deny = deny.clone();
                async move {
                    let uid = req
                        .headers()
                        .get("x-user-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    sink.lock().unwrap().push(uid.clone());
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    if deny.iter().any(|d| d == &uid) {
                        return axum::response::Response::builder()
                            .status(401)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(
                                r#"{"code":11101,"msg":"token expired"}"#.to_string(),
                            ))
                            .unwrap();
                    }
                    let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
                               data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n\
                               data: [DONE]\n\n";
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(sse.to_string()))
                        .unwrap()
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        SlowUpstream {
            base: format!("http://{addr}"),
            hits,
            handle,
        }
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

impl Drop for SlowUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// 场景骨架：生产装配 + 两个合成登录态；上游地址由调用方覆盖。
fn scenario() -> Value {
    json!({
        "name": "wiring",
        "region": "CN",
        "desensitize": false,
        "cred_mode": "real",
        "auth_files": cn_auth_files(),
        "upstream_spec": {"kind": "fixed"},
    })
}

/// 从 `/v1/accounts` 快照里取某号字段。
fn account_field(snapshot: &str, uid: &str, key: &str) -> Value {
    let v: Value = serde_json::from_str(snapshot).expect("快照是合法 JSON");
    for a in field(&v, "accounts").as_array().expect("accounts 数组") {
        if field(a, "uid").as_str() == Some(uid) {
            return field(a, key).clone();
        }
    }
    panic!("快照里没有 {uid}");
}

/// ① 接线：请求进行中 `inflight=1`、结束后 `=0`；`use_count` 累加、`last_used` 非空。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_path_increments_and_decrements_inflight() {
    let env = EnvGuard::set(&[]);
    let up = SlowUpstream::start(700, &[]).await;
    let sc = scenario();
    let rp = start_real_replay("p5c-wiring-incdec", Region::Cn, &sc, Some(up.base.clone()), &env).await;

    // 池内 mtime 新的在前 → 第一个号就是 u-wire。
    let body = br#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":false}"#.to_vec();
    let url = format!("{}/v1/chat/completions", rp.app.base);
    let handle = tokio::spawn(async move {
        common::client()
            .post(url)
            .body(body)
            .send()
            .await
            .expect("请求发不出去")
            .status()
            .as_u16()
    });

    // 上游还在 sleep（700ms）里 ⇒ 此刻请求进行中。
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let mid = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    assert_eq!(mid.status, 200);
    assert_eq!(
        account_field(&mid.text(), "u-wire", "inflight"),
        json!(1),
        "请求进行中 inflight 应为 1（chat 路径的 IncLoad 接线）——实得快照：{}",
        mid.text()
    );

    let status = handle.await.expect("join");
    assert_eq!(status, 200);

    let after = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    assert_eq!(
        account_field(&after.text(), "u-wire", "inflight"),
        json!(0),
        "请求结束后 inflight 应回到 0（DecLoad 接线）——实得快照：{}",
        after.text()
    );
    assert_eq!(account_field(&after.text(), "u-wire", "use_count"), json!(1));
    assert!(
        account_field(&after.text(), "u-wire", "last_used").as_str().unwrap_or("").len() > 0,
        "last_used 应被 Touch 写上"
    );
    assert_eq!(up.hits(), vec!["u-wire".to_string()], "上游应只被该号打过一次");
    rp.finish();
}

/// ① 的换号分支：401 → 换活号重发；**两个号的 inflight 都必须归零**（旧号的 Dec 不能漏）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_path_swap_releases_inflight_on_both_accounts() {
    let env = EnvGuard::set(&[]);
    // 第一个号（mtime 最新 = 池内首选）回 401，第二个号回 200。
    let up = SlowUpstream::start(0, &["u-wire"]).await;
    let sc = scenario();
    let rp = start_real_replay("p5c-wiring-swap", Region::Cn, &sc, Some(up.base.clone()), &env).await;

    let r = send(
        &rp.app.base,
        "POST",
        "/v1/chat/completions",
        Some(br#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":false}"#),
    )
    .await;
    assert_eq!(r.status, 200, "换号后应成功（实得 {}）", r.text());

    let snap = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    assert_eq!(
        account_field(&snap.text(), "u-wire", "inflight"),
        json!(0),
        "401 号换出后 inflight 必须归零（换号分支的 DecLoad）——实得快照：{}",
        snap.text()
    );
    assert_eq!(
        account_field(&snap.text(), "u-wire2", "inflight"),
        json!(0),
        "换入号在请求结束后 inflight 必须归零"
    );
    assert_eq!(account_field(&snap.text(), "u-wire", "dead"), json!(true), "401 号应被标死");
    assert_eq!(account_field(&snap.text(), "u-wire2", "use_count"), json!(1));
    assert_eq!(
        up.hits(),
        vec!["u-wire".to_string(), "u-wire2".to_string()],
        "应先打 401 号再换打活号各一次"
    );
    rp.finish();
}

/// ② 499 取消信号（差异 13）：退避期间客户端断开 ⇒ 不再打上游；不断开 ⇒ 打满 3 发。
///
/// 期望值来自真 Go 的采收（黄金 `meta.p5c_cancel`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_during_backoff_cancels_like_go() {
    let env = EnvGuard::set(&[]);
    let g = load_golden();
    let cancel = field(field(&g, "meta"), "p5c_cancel");
    assert!(
        !cancel.is_null(),
        "黄金缺 meta.p5c_cancel（取消实验基准）——再生成链见 tests/golden/README.md"
    );
    let want_with = field(cancel, "hits_with_cancel").as_i64().unwrap();
    let want_without = field(cancel, "hits_without_cancel").as_i64().unwrap();
    let cancel_after_ms = field(cancel, "cancel_after_ms").as_u64().unwrap_or(250);

    // ---------- 不带取消：打满（首轮 + 2 次退避重试） ----------
    {
        let up = FakeUpstream::start(&json!({
            "kind": "fixed",
            "status": 400,
            "headers": {"Content-Type": "application/json"},
            "body": "{\"code\":11133,\"msg\":\"content rejected\"}"
        }))
        .await;
        let sc = scenario();
        let rp = start_real_replay("p5c-cancel-none", Region::Cn, &sc, Some(up.base.clone()), &env).await;
        let r = send(
            &rp.app.base,
            "POST",
            "/v1/chat/completions",
            Some(br#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":true}"#),
        )
        .await;
        assert_eq!(r.status, 400, "不断开时应把上游 400 如实透传");
        assert_eq!(
            up.hits().len() as i64,
            want_without,
            "不断开时的上游命中数与 Go 不一致"
        );
        rp.finish();
    }

    // ---------- 退避期间断开：服务端应停止重试 ----------
    {
        let up = FakeUpstream::start(&json!({
            "kind": "fixed",
            "status": 400,
            "headers": {"Content-Type": "application/json"},
            "body": "{\"code\":11133,\"msg\":\"content rejected\"}"
        }))
        .await;
        let sc = scenario();
        let rp = start_real_replay("p5c-cancel-mid", Region::Cn, &sc, Some(up.base.clone()), &env).await;
        let url = format!("{}/v1/chat/completions", rp.app.base);
        let handle = tokio::spawn(async move {
            let _ = common::client()
                .post(url)
                .body(
                    br#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":true}"#
                        .to_vec(),
                )
                .send()
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(cancel_after_ms)).await;
        handle.abort();
        // 退避总时长 800ms + 1600ms：断开后给服务端足够时间证明「没有后续上游请求」。
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        assert_eq!(
            up.hits().len() as i64,
            want_with,
            "客户端断开后仍打了上游（Go 的 499 取消语义没生效）"
        );
        rp.finish();
    }
}
