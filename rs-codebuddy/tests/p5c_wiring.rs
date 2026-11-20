//! p5c_wiring.rs —— P5c 收口的两处**行为级**覆盖缺口（P5b 审查点名）：
//!
//! ① **chat 路径的 `Inc/DecLoad` 接线**：此前只有 `pool.rs` 的方法直调单测，接线回归
//!    （例如换号时忘了 `Dec` 旧号）发现不了。这里从 HTTP 路径发真请求，在**请求进行中**
//!    读 `/v1/accounts` 快照，断言 `inflight` 真的被 Inc、请求结束后真的回到 0；
//!    另覆盖换号路径（401 → 换活号）后**两个号的 inflight 都归零**（旧号的 Dec 不能漏）。
//! ② **499 的取消信号**（差异 13）：Go 的 `r.Context().Done()` 在 11133 退避等待期间可中断
//!    ⇒ 客户端断开后**不再打上游**。Go 侧事实由采集器采收在黄金 `meta.p5c_cancel`，
//!    本测试对同一场景比对「上游命中数」。
//! ③ **11128 按号封禁治理**（2026-10-17，对齐 Go `0415ec1`）：干净请求遇 11128 → 当次换号
//!    重试 + 肇事号 1 小时冷却；脏请求（内容指纹命中）→ 原样透传、不冷却任何号；全池被封
//!    → 有界透传（换号次数上限 = 池大小）；冷却到期由 `pick` 懒判定自动恢复。
//!
//! 测试都用**生产装配**（真凭据 + 真池 + 真 failover），域名/夹具与 P5b 黄金同款。

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

// ---------- ③ 11128 按号封禁治理（2026-10-17，对齐 Go `0415ec1`） ----------
//
// 场景与 Go `server_test.go::TestChatFlagged11128Governance` 一一对应：同一条干净请求在
// 「被封号」上必回 400/11128、换号即 200；脏请求（命中内容指纹）原样透传且不冷却任何号；
// 全池被封时有界透传（上游命中数 = 池大小，不死循环）。上游是进程内假上游按 `X-User-Id`
// 分派，登录态是合成 `.info`（临时目录），**不碰 8787/8789 与真实账号**。

/// 11128 上游错体（与 Go 单测同款：`code` 是字段级数字）。
const FLAGGED_BODY_11128: &str = r#"{"error":{"data":{"code":11128,"msg":"Illegal API invocation from an unapproved channel","requestId":"x"}}}"#;

/// 一路正常的 SSE（`stream:false` 时由 `collectStream` 聚合成 `chat.completion`）。
const OK_SSE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2,\"total_tokens\":9}}\n\n\
data: [DONE]\n\n";

/// 干净请求：无任何指纹词（品牌/合规词表与黑名单句都不命中）⇒ 11128 判账号级封禁。
const CLEAN_PAYLOAD: &[u8] = br#"{"model":"auto","messages":[{"role":"system","content":"You are a helpful assistant."},{"role":"user","content":"Summarize this function."}],"stream":false}"#;

/// 脏请求：品牌词 + 指纹黑名单句（`desensitize=false` 的部署形态；脱敏开着时同样内容会
/// 被插零宽/删句，那条路径由 `desensitize.rs` 的脱敏后判定单测覆盖）。
const DIRTY_PAYLOAD: &[u8] = br#"{"model":"auto","messages":[{"role":"system","content":"You are Claude Code, a coding agent."},{"role":"user","content":"Main branch (you will usually use this for PRs): main"}],"stream":false}"#;

/// 假上游 spec：`flagged` 里的号恒回 400/11128，其余号回 200 SSE（按 uid 分派）。
fn flagged_spec(flagged: &[&str]) -> Value {
    let bad = json!([{
        "status": 400,
        "headers": {"Content-Type": "application/json"},
        "body": FLAGGED_BODY_11128,
    }]);
    let ok = json!([{
        "status": 200,
        "headers": {"Content-Type": "text/event-stream"},
        "body": OK_SSE,
    }]);
    let mut table = serde_json::Map::new();
    for uid in ["u-wire", "u-wire2"] {
        table.insert(
            uid.to_string(),
            if flagged.contains(&uid) {
                bad.clone()
            } else {
                ok.clone()
            },
        );
    }
    json!({"per_user": Value::Object(table)})
}

/// 11140 账号级限制的官方报文（2026-10-24 实测同款）。
const RESTRICTED_BODY_11140: &str = r#"{"code":11140,"msg":"request illegal","displayMsg":{"zh":"账号存在异常行为，已被限制使用"},"displayTips":{"zh":"系统检测到该账号存在异常使用行为，如需申诉请提交反馈"},"requestId":"rid-11140"}"#;

/// 假上游 spec：`restricted` 里的号恒回 403/11140（账号限制文案），其余号回 200 SSE。
fn restricted_spec(restricted: &[&str]) -> Value {
    let bad = json!([{
        "status": 403,
        "headers": {"Content-Type": "application/json"},
        "body": RESTRICTED_BODY_11140,
    }]);
    let ok = json!([{
        "status": 200,
        "headers": {"Content-Type": "text/event-stream"},
        "body": OK_SSE,
    }]);
    let mut table = serde_json::Map::new();
    for uid in ["u-wire", "u-wire2"] {
        table.insert(
            uid.to_string(),
            if restricted.contains(&uid) {
                bad.clone()
            } else {
                ok.clone()
            },
        );
    }
    json!({"per_user": Value::Object(table)})
}

/// 上游命中的 uid 序列（按发生顺序；Go 侧打点计数同义）。
fn hit_uids(up: &FakeUpstream) -> Vec<String> {
    up.hits()
        .iter()
        .map(|h| h.header("x-user-id").unwrap_or("").to_string())
        .collect()
}

/// 快照里某号三个状态位都必须是 false（脏请求/正常号不该被标记）。
fn assert_untouched(snapshot: &str, uid: &str) {
    for key in ["cooling", "dead", "exhausted"] {
        assert_eq!(
            account_field(snapshot, uid, key),
            json!(false),
            "{uid}.{key} 不应被标记——实得快照：{snapshot}"
        );
    }
}

/// 场景一：干净请求遇 11128 → 当次换号重试成功（客户端 200），肇事号进 1 小时冷却；
/// 后续请求不再轮到它。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flagged11128_clean_request_swaps_then_cools_for_an_hour() {
    let env = EnvGuard::set(&[]);
    let up = FakeUpstream::start(&flagged_spec(&["u-wire"])).await;
    let sc = scenario();
    let rp = start_real_replay(
        "p5c-11128-clean",
        Region::Cn,
        &sc,
        Some(up.base.clone()),
        &env,
    )
    .await;

    let r = send(
        &rp.app.base,
        "POST",
        "/v1/chat/completions",
        Some(CLEAN_PAYLOAD),
    )
    .await;
    assert_eq!(
        r.status,
        200,
        "干净请求遇 11128 应换号拿到 200（实得 {}）",
        r.text()
    );
    assert_eq!(
        hit_uids(&up),
        vec!["u-wire", "u-wire2"],
        "上游应 A(封)/B(好) 各打一次"
    );

    let snap = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    let text = snap.text();
    assert_eq!(
        account_field(&text, "u-wire", "cooling"),
        json!(true),
        "肇事号应进冷却——实得快照：{text}"
    );
    let until = account_field(&text, "u-wire", "limited_until")
        .as_str()
        .unwrap_or("")
        .to_string();
    let until_ms = rs_codebuddy::gotime::parse_rfc3339(&until)
        .unwrap_or_else(|| panic!("limited_until 应是 RFC3339（实得 {until:?}）"));
    let remaining_min = (until_ms - rs_codebuddy::gotime::now_millis()) / 60_000;
    assert!(
        (55..=65).contains(&remaining_min),
        "冷却时长应约 1 小时（Go `flaggedCooldown = time.Hour`），实得 {remaining_min} 分钟"
    );
    assert_untouched(&text, "u-wire2");

    // 冷却生效：第二发同一请求直接由 B 承接（A 不再被 Pick）。
    let r2 = send(
        &rp.app.base,
        "POST",
        "/v1/chat/completions",
        Some(CLEAN_PAYLOAD),
    )
    .await;
    assert_eq!(r2.status, 200, "第二发应仍 200（实得 {}）", r2.text());
    assert_eq!(
        hit_uids(&up),
        vec!["u-wire", "u-wire2", "u-wire2"],
        "冷却中的号不应再被选中"
    );
    rp.finish();
}

/// 场景二：脏请求（品牌词/system 模板句）11128 → 原样透传，不冷却任何号。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flagged11128_dirty_request_passes_through_without_cooling() {
    let env = EnvGuard::set(&[]);
    let up = FakeUpstream::start(&flagged_spec(&["u-wire"])).await;
    let sc = scenario();
    let rp = start_real_replay(
        "p5c-11128-dirty",
        Region::Cn,
        &sc,
        Some(up.base.clone()),
        &env,
    )
    .await;

    let r = send(
        &rp.app.base,
        "POST",
        "/v1/chat/completions",
        Some(DIRTY_PAYLOAD),
    )
    .await;
    assert_eq!(
        r.status,
        400,
        "脏请求应原样透传 400/11128（实得 {}：{}）",
        r.status,
        r.text()
    );
    assert!(
        r.text().contains("11128"),
        "透传体应含 11128（实得 {}）",
        r.text()
    );
    assert_eq!(
        hit_uids(&up),
        vec!["u-wire"],
        "脏请求只打肇事号一次、不换号"
    );

    let snap = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    let text = snap.text();
    for uid in ["u-wire", "u-wire2"] {
        assert_untouched(&text, uid);
    }
    rp.finish();
}

/// 场景三：全池被封 → 有界透传（不死循环），全部号被冷却。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flagged11128_all_pool_flagged_passes_through_bounded() {
    let env = EnvGuard::set(&[]);
    let up = FakeUpstream::start(&flagged_spec(&["u-wire", "u-wire2"])).await;
    let sc = scenario();
    let rp = start_real_replay(
        "p5c-11128-all",
        Region::Cn,
        &sc,
        Some(up.base.clone()),
        &env,
    )
    .await;

    let r = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        send(
            &rp.app.base,
            "POST",
            "/v1/chat/completions",
            Some(CLEAN_PAYLOAD),
        ),
    )
    .await
    .expect("全池被封必须返回（疑似 11128 换号死循环）");
    assert_eq!(
        r.status,
        400,
        "全池被封应如实透传 400（实得 {}：{}）",
        r.status,
        r.text()
    );
    assert_eq!(
        up.hits().len(),
        2,
        "全池被封时上游命中数应为池大小 2 发（有界）"
    );

    let snap = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    let text = snap.text();
    for uid in ["u-wire", "u-wire2"] {
        assert_eq!(
            account_field(&text, uid, "cooling"),
            json!(true),
            "全池被封时 {uid} 应被冷却——实得快照：{text}"
        );
    }
    rp.finish();
}

/// 场景五（2026-11-21 重写）：**判据陷阱回归**。上游所有 11128 的 `displayTips` 都写
/// "This call failed channel verification…"——旧实现拿这句当子类型判据，结果 11128 一律
/// 「原样透传」，改写与按号冷却都成了死代码（v4.9.9 修）。现在这句只是报文文案，不参与
/// 分流：仍按 改写 → 内容指纹 → 账号级 的阶梯走。本例两个号都恒回这条 11128，且 system
/// 是短样板句（够不上搬家门槛），故预期账号级治理：各打一次、肇事号冷却、如实透传 400。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn flagged11128_channel_verification_text_is_not_a_subtype() {
    const CHANNEL_BODY_11128: &str = r#"{"error":{"data":{"code":11128,"msg":"request illegal","requestId":"x","displayMsg":{"en":"Request blocked. Please send it again"},"displayTips":{"en":"This call failed channel verification, possibly because it was not sent from an official client."}}}}"#;
    let bad = json!([{
        "status": 400,
        "headers": {"Content-Type": "application/json"},
        "body": CHANNEL_BODY_11128,
    }]);
    let mut table = serde_json::Map::new();
    for uid in ["u-wire", "u-wire2"] {
        table.insert(uid.to_string(), bad.clone());
    }
    let spec = json!({"per_user": Value::Object(table)});

    let env = EnvGuard::set(&[]);
    let up = FakeUpstream::start(&spec).await;
    let sc = scenario();
    let rp = start_real_replay(
        "p5c-11128-channel",
        Region::Cn,
        &sc,
        Some(up.base.clone()),
        &env,
    )
    .await;

    let r = send(
        &rp.app.base,
        "POST",
        "/v1/chat/completions",
        Some(CLEAN_PAYLOAD),
    )
    .await;
    assert_eq!(
        r.status,
        400,
        "全池 11128 应如实透传 400（实得 {}：{}）",
        r.status,
        r.text()
    );
    assert!(
        r.text().contains("channel verification"),
        "透传体应保留上游文案（实得 {}）",
        r.text()
    );
    assert_eq!(
        hit_uids(&up),
        vec!["u-wire", "u-wire2"],
        "文案不参与分流：按账号级治理，两个号各打一次（有界）"
    );

    let snap = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    let text = snap.text();
    assert_eq!(
        account_field(&text, "u-wire", "cooling"),
        json!(true),
        "肇事号应进冷却——文案不再让它逃过治理：{text}"
    );
    rp.finish();
}

// ---------- ⑥ 流内错误嗅探（2026-10-20） ----------
//
// 上游在 **200** 里塞错误帧/空流时，下游会拿到「200 但没内容」。流式路径在发出 SSE
// 响应头之前 peek 8KB（`STREAM_PEEK_WINDOW`），命中 `ErrCode` / `EmptyStream` 就换号重发；
// **判错只耗 attempt、绝不冷却账号**（不动 mark_limited / mark_exhausted）——
// 与 11128 账号级封禁治理的关键区别，防慢账号误判后级联耗竭整池。

/// 流内嗅探场景的上游应答表：`bad` 里的号回 200 + 错误帧，其余号回正常 SSE。
fn stream_head_spec(bad: &[&str]) -> Value {
    let err = json!([{
        "status": 200,
        "headers": {"Content-Type": "text/event-stream"},
        "body": r#"{"code":11128,"msg":"Illegal API invocation from an unapproved channel"}"#,
    }]);
    let ok = json!([{
        "status": 200,
        "headers": {"Content-Type": "text/event-stream"},
        "body": OK_SSE,
    }]);
    let mut table = serde_json::Map::new();
    for uid in ["u-wire", "u-wire2"] {
        table.insert(
            uid.to_string(),
            if bad.contains(&uid) { err.clone() } else { ok.clone() },
        );
    }
    json!({"per_user": Value::Object(table)})
}

/// 场景六：首号 200 + 错误帧，次号正常 SSE ⇒ 客户端拿 200 正常内容、两号各命中一次、
/// **两个号的 cooling/dead/exhausted 全为 false**（判错只耗 attempt，不动账号状态）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_error_head_swaps_without_cooling() {
    let env = EnvGuard::set(&[]);
    let up = FakeUpstream::start(&stream_head_spec(&["u-wire"])).await;
    let sc = scenario();
    let rp = start_real_replay(
        "p5c-stream-head-swap",
        Region::Cn,
        &sc,
        Some(up.base.clone()),
        &env,
    )
    .await;

    let r = send(
        &rp.app.base,
        "POST",
        "/v1/chat/completions",
        Some(br#"{"model":"auto","messages":[{"role":"user","content":"hi"}],"stream":true}"#),
    )
    .await;
    assert_eq!(
        r.status,
        200,
        "流内错误帧应换号拿到正常 200（实得 {}：{}）",
        r.status,
        r.text()
    );
    assert!(
        r.text().contains("hi"),
        "客户端应拿到次号的正常 SSE 内容（实得 {}）",
        r.text()
    );
    assert!(
        !r.text().contains("11128"),
        "错误帧不该透传到下游（实得 {}）",
        r.text()
    );
    assert_eq!(
        hit_uids(&up),
        vec!["u-wire", "u-wire2"],
        "上游应 A(错误帧)/B(正常) 各打一次"
    );

    let snap = send(&rp.app.base, "GET", "/v1/accounts", None).await;
    let text = snap.text();
    for uid in ["u-wire", "u-wire2"] {
        assert_untouched(&text, uid);
    }
    rp.finish();
}