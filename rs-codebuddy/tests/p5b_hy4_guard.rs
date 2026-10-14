//! `chat.rs` 的 hy4 trip 守卫：**`attempt` 必须每轮自增**（对齐 Go `server.go:1247`）。
//!
//! 驳回项（P4）：Go 是 `for attempt := 0; ; attempt++`——**每一轮**（含 11133 退避重试、
//! 14018/401 换号重发）都自增，于是守卫 `(attempt == 0 || accountResent)` 会拒绝
//! 「已经重试过之后的 hy4 trip」；Rust 原先只在 hy4-trip 分支自增，等于多放行了一次
//! 模型级兜底。本文件把这条差异钉死：
//!
//! * 第一发命中 **11133**（上游瞬时拒绝）→ 退避后原地重试（此时 `attempt` 已非 0）；
//! * 第二发命中 **429 + 配额报文** → 守卫必须拦住 trip：**不下发 fallback 请求**，
//!   把 429 如实透传给客户端；
//! * 对照组（同款上游、但第一发就 429）→ trip 必须**照常生效**（证明守卫不是把功能关掉）。
//!
//! **为什么这两条用「零值 Server（无池）+ `StubCredential`」而不是生产装配**：429 一旦
//! 命中，`chat.rs` 的**账号级换号分支**先于 hy4 守卫跑，而它只要 `Pick()` 拿到号就会
//! `account_resent = true` 并重发。单号池下 `Pick()` 会把刚标了冷却的同一个号再选出来
//! （Go 的 `Pick` 在「全池都在冷却」时同样会兜底返回轮转到的那个号，见 `pool.go`），
//! 于是被测量的是账号分支而不是 hy4 守卫——测出来的东西不对。无池（`pool == None`）
//! 时账号分支一律不参与，hy4 守卫就是唯一在场的那条逻辑，与本驳回项一一对应。

mod common;

use std::sync::Arc;

use common::{AppServer, FakeUpstream, Resp, TestDir};
use serde_json::{json, Value};

use rs_codebuddy::cred::StubCredential;
use rs_codebuddy::region::Region;

/// 上游第一发时间片：11133（内容级拒绝，会触发退避重试）。
const TRANSIENT: &str = r#"{"code":11133,"msg":"content rejected, please retry"}"#;
/// 配额 429 的报文骨架：重置时点**运行时生成**（写在未来的 UTC+8 墙上时间）。
///
/// 为什么不能写死一个日期：`hy4_limit` 只在窗口**未到期**时才是对象（到期即懒清除），
/// 写死的日期一旦成为过去，断言就会因为「窗口已过期」而失败（本轮实测踩过：
/// 黄金里的 `2026-10-15 06:20:02` 在 07:02 跑就已经过期了）。上游报文里的时点永远是
/// 未来，测试要复现的是**形态**而不是某个具体日期。
fn quota_429_body() -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 未来 1 小时，按 UTC+8 的墙上时间写（`parseQuotaReset` 固定按东八区解释）
    let wall = unix + 3600 + 8 * 3600;
    let lit = rs_codebuddy::gotime::format_rfc3339_seconds(wall * 1000, 0).replace('T', " ");
    format!(
        r#"{{"code":6004,"msg":"Quota exceeded, will reset at {} UTC+8"}}"#,
        &lit[..19]
    )
}
const SSE_OK: &str =
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
const CHAT_BODY: &[u8] = br#"{"model":"hy4-preview","messages":[{"role":"user","content":"hi"}]}"#;

fn upstream_spec(items: Value) -> Value {
    json!({"kind": "sequence", "items": items})
}

/// 起一套**无池**的被测量例（stub 凭据 + 指定上游），返回 (app, upstream, dir)。
async fn app_with_stub(dir: &TestDir, upstream: &str) -> AppServer {
    let cred = Arc::new(
        StubCredential::from_json(&json!({
            "auth_dirs": ["__AUTHDIR__"],
            "summary": {"kind":"ok","pairs":{"uid":"u-stub"}},
            "headers": {"Content-Type":"application/json","Authorization":"Bearer tok-stub","X-User-Id":"u-stub"},
            "headers_ok": true,
        }))
        .unwrap(),
    );
    let state =
        rs_codebuddy::AppState::new(Region::Cn, cred, upstream.to_string(), dir.path().to_path_buf());
    // 生产缺省：enabled=true / deepseek-v4-pro / max（`Failover::load("")` 即生产缺省）。
    state.failover.update(Some(true), Some("deepseek-v4-pro".to_string()), &["deepseek-v4-pro"]).unwrap();
    AppServer::start(Arc::new(state)).await
}

async fn post_chat(app: &AppServer) -> Resp {
    common::send(&app.base, "POST", "/v1/chat/completions", Some(CHAT_BODY)).await
}

async fn hy4_limit(app: &AppServer) -> Value {
    let r = common::send(&app.base, "GET", "/v1/failover", None).await;
    serde_json::from_slice::<Value>(&r.body).expect("/v1/failover 回 JSON")
}

/// 11133 之后那一发 429 **不许**触发 hy4 兜底（`attempt` 已非 0）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hy4_trip_is_refused_after_a_transient_retry_like_go() {
    let dir = TestDir::new("hy4-guard-after-11133");
    let up = FakeUpstream::start(&upstream_spec(json!([
        {"status": 400, "body": TRANSIENT, "headers": {"Content-Type": "application/json"}},
        {"status": 429, "body": quota_429_body(), "headers": {"Content-Type": "application/json"}},
        {"status": 200, "body": SSE_OK, "headers": {"Content-Type": "text/event-stream"}},
    ])))
    .await;
    let app = app_with_stub(&dir, &up.base).await;
    let r = post_chat(&app).await;

    let hits = up.hits();
    let models: Vec<String> = hits.iter().map(|h| h.model()).collect();
    assert_eq!(
        r.status, 429,
        "11133 重试之后的 429 应如实透传（实得 {} / {}）",
        r.status,
        r.text()
    );
    assert_eq!(
        hits.len(),
        2,
        "应为「11133 一发 + 重试一发 = 2 发」，多出来的那一发就是多放行的 hy4 兜底：{models:?}"
    );
    assert!(
        models.iter().all(|m| m == "hy4-preview"),
        "任何一发都不该被改写成 fallback（模型级兜底被守卫拒绝了）：{models:?}"
    );
    assert!(
        r.text().contains("6004") || r.text().contains("Quota exceeded"),
        "429 的原始报文应透传：{}",
        r.text()
    );
    // 限流窗口**不该**被记录（trip 被拒 ⇒ 没有窗口）
    assert_eq!(
        hy4_limit(&app).await["hy4_limit"],
        Value::Null,
        "trip 被守卫拒绝时不该记下限流窗口"
    );
    println!(
        "守卫生效：上游 {} 发（{}），第 2 发 429 未被 hy4 兜底改写，客户端收到 429 原文",
        hits.len(),
        models.join(" → ")
    );
}

/// 对照组：第一发就 429 → trip 照常生效（**不是**把兜底关掉）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hy4_trip_still_fires_on_first_round_429() {
    let dir = TestDir::new("hy4-trip-control");
    let up = FakeUpstream::start(&upstream_spec(json!([
        {"status": 429, "body": quota_429_body(), "headers": {"Content-Type": "application/json"}},
        {"status": 200, "body": SSE_OK, "headers": {"Content-Type": "text/event-stream"}},
    ])))
    .await;
    let app = app_with_stub(&dir, &up.base).await;
    let r = post_chat(&app).await;

    let hits = up.hits();
    let models: Vec<String> = hits.iter().map(|h| h.model()).collect();
    assert_eq!(
        hits.len(),
        2,
        "首轮 429 → trip → 用 fallback 重放一次：{models:?}"
    );
    assert_eq!(models[0], "hy4-preview");
    assert_eq!(models[1], "deepseek-v4-pro", "第二发应是 fallback 模型");
    assert_eq!(r.status, 200, "重放的 fallback 请求应成功：{}", r.text());
    // 窗口记下来了（`hy4_limit` 非 null）
    let lim = hy4_limit(&app).await;
    assert!(
        lim["hy4_limit"].is_object(),
        "trip 生效时应有限流窗口：{lim}"
    );
    println!(
        "对照组生效：首轮 429 立刻 trip，{} 发（{} → {}）",
        hits.len(),
        models[0],
        models[1]
    );
}
