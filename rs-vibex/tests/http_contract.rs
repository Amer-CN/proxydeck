// tests/http_contract.rs —— 路由/边界合同 + 两条「黄金之外必须单独钉住」的契约：
//
// 1. **流式中断哨兵不泄漏**（C53）：Go 侧 v3.19.0 审查驳回的那处缺陷，Rust 侧不得复现。
//    可观测判据：客户端中途断流后，那一轮必须真的结束——若哨兵把生产方挂住，
//    下一轮会卡在 `turn_mu` 上，本用例即超时判红。
// 2. **32 MiB 请求体不被本地默认上限截断**（C68）：请求体要逐字节进到 WS 的 `prompt` 帧里
//    （不是本地 413，也不是被截半）。
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;

struct Ctx {
    up: FakeUpstream,
    srv: Arc<rs_vibex::Server>,
    app: AppServer,
    _dir: TestDir,
}

async fn boot(tag: &str) -> Ctx {
    let dir = TestDir::new(tag);
    let up = FakeUpstream::start().await;
    let mut cfg = test_cfg(&up.base, dir.join("vibex-config.json"));
    cfg.tokens = vec![fake_jwt_at("u-1", "小明", 1_800_000_000, "")];
    let (srv, app) = start_app(cfg, rs_vibex::Seams::default(), &dir).await;
    Ctx {
        up,
        srv,
        app,
        _dir: dir,
    }
}

// ---------- 路由边界 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn route_boundaries_match_go() {
    let ctx = boot("routes").await;
    let b = &ctx.app.base;

    // 未知路径 → Go `http.NotFound` 的逐字文案
    let (status, hdrs, body) = send(b, "GET", "/nope", &[], "").await;
    assert_eq!(status, 404);
    assert_eq!(body, "404 page not found\n");
    assert_eq!(hdrs["content-type"], "text/plain; charset=utf-8");
    assert_eq!(hdrs["x-content-type-options"], "nosniff");
    assert_eq!(hdrs["access-control-allow-origin"], "*");

    // 尾斜杠不算命中（Go 的精确模式匹配）
    let (status, _, body) = send(b, "GET", "/health/", &[], "").await;
    assert_eq!(status, 404, "{body}");

    // OPTIONS → 204 空体 + CORS 三头
    let (status, hdrs, body) = send(b, "OPTIONS", "/v1/models", &[], "").await;
    assert_eq!(status, 204);
    assert_eq!(body, "");
    assert_eq!(hdrs["access-control-allow-methods"], "GET, POST, OPTIONS");
    assert_eq!(
        hdrs["access-control-allow-headers"],
        "Content-Type, Authorization, X-Vibex-Session"
    );

    // /health 不看方法（Go 的 `HandleFunc` 不判方法）
    let (status, _, body) = send(b, "POST", "/health", &[], "{}").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"service\":\"vibex-go\""), "{body}");

    // /accounts 非 GET/POST → 405 `只接受 GET/POST`
    let (status, _, body) = send(b, "PUT", "/accounts", &[], "").await;
    assert_eq!(status, 405);
    assert!(body.contains("只接受 GET/POST"), "{body}");

    // 请求体校验：空 / 非对象 / messages 缺失
    for body_in in ["", "   ", "[1,2]", "null", r#"{"model":"lite-1"}"#, r#"{"messages":[]}"#] {
        let (status, _, body) = send(b, "POST", "/v1/chat/completions", &[], body_in).await;
        assert_eq!(status, 400, "输入 {body_in:?} → {body}");
        assert!(body.contains("\"type\":\"vibex_error\""), "{body}");
    }
}

// ---------- C53：哨兵不泄漏 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sentinel_does_not_leak_when_client_disconnects() {
    let ctx = boot("sentinel").await;
    // 把流拉长：300 片 × 20ms ≈ 6s，客户端读到第一帧就断
    // （上游回放比客户端读取慢得多，断流事件才有机会真的落到服务端）
    ctx.up.set_replay(vec!["增量"; 300]).await;
    ctx.up.set_replay_delay(20).await;

    {
        let cl = client();
        let resp = cl
            .post(format!("{}/v1/chat/completions", ctx.app.base))
            .header("content-type", "application/json")
            .body(r#"{"messages":[{"role":"user","content":"x"}],"stream":true}"#)
            .send()
            .await
            .expect("发流式请求");
        assert_eq!(resp.status().as_u16(), 200);
        // 读一小段就丢——模拟 GUI/客户端中途关页面
        drop(resp);
    }

    // 第二轮必须能跑完：哨兵若把生产方挂住，这一轮会卡在 turn_mu 上、这里超时判红。
    let done = tokio::time::timeout(Duration::from_secs(15), async {
        let cl = client();
        let resp = cl
            .post(format!("{}/v1/chat/completions", ctx.app.base))
            .header("content-type", "application/json")
            .body(r#"{"messages":[{"role":"user","content":"第二轮"}]}"#)
            .send()
            .await
            .expect("第二轮流式请求");
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        (status, body)
    })
    .await;

    let (status, body) = done.expect(
        "第二轮被挂住了 —— 说明上一轮的哨兵泄漏（生产方永久阻塞在向已退出的消费方投递哨兵上）",
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("chat.completion"), "{body}");
    let _ = ctx.srv.pool.len();
}

// ---------- C68：32 MiB 请求体不被截 ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_body_reaches_the_ws_prompt() {
    let ctx = boot("largebody").await;
    // 2.5 MiB：超过 axum 的 2 MiB 默认上限，但远小于 Go 的 32 MiB 口径
    let filler = "A".repeat(2_500_000);
    let body = format!(
        r#"{{"messages":[{{"role":"user","content":"{filler}"}}]}}"#
    );
    let cl = client();
    let resp = cl
        .post(format!("{}/v1/chat/completions", ctx.app.base))
        .header("content-type", "application/json")
        .body(body.clone())
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .expect("大体积请求应被受理（本地不该 413）");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    assert_eq!(status, 200, "本地不该因体积拒绝：{text}");

    // 上游 WS 收到的 prompt 帧应含完整原文（不是被截半）
    let rec = ctx.up.take().await;
    let mut prompts: Vec<String> = Vec::new();
    for conn in &rec.ws_conns {
        for frame in conn {
            if frame.contains("\"type\":\"prompt\"") {
                prompts.push(frame.clone());
            }
        }
    }
    assert_eq!(prompts.len(), 1, "应恰好有一条 prompt 帧");
    let frame = &prompts[0];
    assert!(
        frame.contains(&filler),
        "prompt 帧里没有完整原文（长度 {}，请求体长度 {}）——请求体被截断了",
        frame.len(),
        body.len()
    );
}
