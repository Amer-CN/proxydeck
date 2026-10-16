//! P6 HTTP 合同与边界：黄金回放覆盖不到的地方（体量、方法、兜底、缓存时序、逐字节换行）。
//!
//! 与 `golden_proxy.rs` 的分工：
//! * 黄金门禁比的是**真 Go 采收的测量值**（出站体 / 下游体 / 路由表 / 统计）；
//! * 这里比的是**不依赖测量值的硬合同**：大体积请求体不得在本地被挡、
//!   Go `http.NotFound` 的三件套（状态 + `text/plain; charset=utf-8` + `nosniff`）、
//!   `json.Encoder` 的末尾换行、`/v1/usage` 的缓存命中形态、以及基准存在性守门。
//!
//! 所有用例都不出网：上游是进程内假上游，被测服务是 crate 自己的生产路由表。

mod common;

use std::collections::HashMap;

use common::*;

/// 起「假上游 + 被测服务」这一对，返回（上游记录句柄、被测服务地址）。
async fn boot() -> (FakeUpstream, String) {
    let up = FakeUpstream::start(0, "text/event-stream", text_stream()).await;
    let mut proxy = rs_proxy::Proxy::new("server-default-key");
    proxy.base_url = up.base.clone();
    rs_proxy::version::set_version_for_test("9.9.9-gate");
    let target = Target::start(proxy).await;
    (up, target.base)
}

/// 采集器里那段 `textStream` 语料（与黄金同源）。
fn text_stream() -> String {
    concat!(
        r#"{"type":"text-delta","text":"你好"}"#,
        "\n",
        r#"{"type":"text-delta","text":"，世界"}"#,
        "\n",
        r#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":12,"outputTokens":6,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}}"#,
        "\n"
    )
    .to_string()
}

// ===========================================================================
// 1) 大体积请求体不得在本地被挡
// ===========================================================================

/// 3.6MB 的请求体必须**逐字节到达上游**。
///
/// 防的是「悄悄退回 axum 的 2MB 默认上限」——P1/P3/P5 都在同一个坑上栽过
/// （`rs-bai` 的 `large_body_reaches_upstream`、`rs-codebuddy` 同款用例）。
/// 3.6MB > 2MB，一旦上限回来，本用例立刻红。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_body_reaches_upstream() {
    let (up, base) = boot().await;
    let filler = "x".repeat(3_600_000);
    let body = format!(
        r#"{{"model":"deepseek-v4-flash","messages":[{{"role":"user","content":"{filler}"}}],"max_tokens":16}}"#
    );
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert("Authorization".to_string(), "Bearer k".to_string());
    let ds = send_raw(
        &base,
        "POST",
        "/v1/chat/completions",
        &headers,
        Some(body.clone().into_bytes()),
    )
    .await;
    assert_eq!(ds.status, 200, "大体积请求不应在本地被挡");

    let hits = up.hits();
    assert_eq!(hits.len(), 1, "应当正好打了一次上游");
    // 上游看到的体里必须含那一整段 filler（不被截断）
    let got = String::from_utf8_lossy(&hits[0].body).to_string();
    assert!(
        got.contains(&filler),
        "上游体里 {} 个字符的 filler 不完整（体长 {}）",
        filler.len(),
        got.len()
    );
}

// ===========================================================================
// 2) Go `http.NotFound` 三件套
// ===========================================================================

/// 兜底 404：状态 + `text/plain; charset=utf-8` + `nosniff` + 体 `404 page not found\n`。
///
/// 这三点是 Go `net/http` 的实现细节（`http.Error` 写死 Content-Type 并补 nosniff），
/// 黄金 `route-boundaries` 的 7 条 404 步锁的就是它；这里给出**不依赖基准**的正面证据。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_404_matches_go_http_not_found() {
    let (_up, base) = boot().await;
    for path in ["/nope", "/health/", "/v1/models/", "/v1/models//x", "/v1", "/"] {
        let ds = send_raw(&base, "GET", path, &HashMap::new(), None).await;
        assert_eq!(ds.status, 404, "{path} 应是 404");
        assert_eq!(
            ds.header("content-type").unwrap_or(""),
            "text/plain; charset=utf-8",
            "{path} 的 Content-Type 应与 Go 的 http.NotFound 一致"
        );
        assert_eq!(
            ds.header("x-content-type-options").unwrap_or(""),
            "nosniff",
            "{path} 应带 nosniff（Go 的 http.Error 自带）"
        );
        assert_eq!(
            String::from_utf8_lossy(&ds.body),
            "404 page not found\n",
            "{path} 的兜底体应与 http.NotFound 逐字节一致"
        );
    }
}

// ===========================================================================
// 3) json.Encoder 的末尾换行
// ===========================================================================

/// `/v1/models` 与 `/v1/usage` 的成功响应末尾都必须有 `\n`（`json.NewEncoder().Encode`）。
///
/// 这是「逐字节合同」，少一个字节在黄金里就是红的（实测踩过两次：
/// `/v1/models`、`/v1/stats`）。这里用不依赖基准的断言再钉一遍。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_encoder_trailing_newline() {
    let (_up, base) = boot().await;
    let ds = send_raw(&base, "GET", "/v1/models", &HashMap::new(), None).await;
    assert_eq!(ds.status, 200);
    assert!(
        ds.body.ends_with(b"\n"),
        "/v1/models 的体必须以换行结尾（json.Encoder）"
    );
    assert_eq!(
        ds.header("content-type").unwrap_or(""),
        "application/json",
        "/v1/models 的 Content-Type 不应带 charset"
    );

    // `/v1/stats` 同理
    let ds = send_raw(&base, "GET", "/v1/stats", &HashMap::new(), None).await;
    assert_eq!(ds.status, 200);
    assert!(
        ds.body.ends_with(b"\n"),
        "/v1/stats 的体必须以换行结尾（json.Encoder）"
    );
}

// ===========================================================================
// 4) 方法/路由边界（不依赖基准）
// ===========================================================================

/// 405 的体与 Content-Type。
///
/// ⚠ **两套形状**（不是笔误，是 Go 的现状）：
/// * `/v1/messages` 走 `writeAnthropicError` ⇒ `{"type":"error","error":{...}}`；
/// * 其余端点走 `writeOpenAIError` ⇒ `{"error":{...,"param":null,"code":null}}`。
/// 两者都由黄金锁死（`edge-msg-get` vs `edge-chat-get`/`edge-resp-get`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_not_allowed_bodies() {
    let (_up, base) = boot().await;
    let openai_want = "{\"error\":{\"message\":\"Method not allowed\",\"type\":\"invalid_request_error\",\"param\":null,\"code\":null}}\n";
    let anthropic_want =
        "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Method not allowed\"}}\n";
    let cases = [
        ("GET", "/v1/chat/completions", openai_want),
        ("GET", "/v1/responses", openai_want),
        ("GET", "/v1/calibration", openai_want),
        ("POST", "/v1/stats", openai_want),
        ("GET", "/v1/messages", anthropic_want),
        ("GET", "/messages", anthropic_want),
    ];
    for (m, p, want) in cases {
        let ds = send_raw(&base, m, p, &HashMap::new(), None).await;
        assert_eq!(ds.status, 405, "{m} {p} 应 405");
        assert_eq!(ds.header("content-type").unwrap_or(""), "application/json");
        assert_eq!(
            String::from_utf8_lossy(&ds.body),
            want,
            "{m} {p} 的 405 体不一致"
        );
    }
}

/// `/health` 不看方法（Go 的 handler 是常量体，POST/HEAD 也回 200）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_answers_every_method() {
    let (_up, base) = boot().await;
    for m in ["GET", "POST", "HEAD", "OPTIONS"] {
        let ds = send_raw(&base, m, "/health", &HashMap::new(), None).await;
        assert_eq!(ds.status, 200, "{m} /health 应 200");
        assert_eq!(
            ds.header("content-type").unwrap_or(""),
            "text/plain; charset=utf-8",
            "{m} /health 的 Content-Type"
        );
        if m != "HEAD" {
            assert_eq!(String::from_utf8_lossy(&ds.body), "{\"status\":\"ok\"}");
        }
    }
}

/// `/v1/models?plan=go` 是子集、且**不带任何 key 也能拿**（本地目录，不打上游）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_plan_filter_is_local_and_smaller() {
    let (up, base) = boot().await;
    let all = send_raw(&base, "GET", "/v1/models", &HashMap::new(), None).await;
    let go = send_raw(&base, "GET", "/v1/models?plan=go", &HashMap::new(), None).await;
    assert_eq!(all.status, 200);
    assert_eq!(go.status, 200);
    assert!(
        go.body.len() < all.body.len(),
        "plan=go 应是子集（{} 应小于 {}）",
        go.body.len(),
        all.body.len()
    );
    assert_eq!(up.hits().len(), 0, "/v1/models 是本地目录，不该打上游");
    // `plan=GO`（大写）不是 go：Go 用 `== "go"` 精确比较
    let upper = send_raw(&base, "GET", "/v1/models?plan=GO", &HashMap::new(), None).await;
    assert_eq!(
        upper.body.len(),
        all.body.len(),
        "plan=GO 应等同不带 plan（大小写敏感）"
    );
}

// ===========================================================================
// 5) 无 key → 401（三种入站协议各自的错误体）
// ===========================================================================

/// 客户端没带 Authorization 且服务端没有默认 key ⇒ 三协议各自 401，且**不打上游**。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_key_is_401_and_never_hits_upstream() {
    let up = FakeUpstream::start(0, "text/event-stream", text_stream()).await;
    let mut proxy = rs_proxy::Proxy::new(""); // 无默认 key
    proxy.base_url = up.base.clone();
    rs_proxy::version::set_version_for_test("9.9.9-gate");
    let target = Target::start(proxy).await;

    let mut h = HashMap::new();
    h.insert("Content-Type".to_string(), "application/json".to_string());

    let ds = send_raw(
        &target.base,
        "POST",
        "/v1/chat/completions",
        &h,
        Some(br#"{"model":"m","messages":[{"role":"user","content":"x"}]}"#.to_vec()),
    )
    .await;
    assert_eq!(ds.status, 401);
    assert!(
        String::from_utf8_lossy(&ds.body).contains("API key required. Set Authorization header."),
        "chat 的 401 文案"
    );

    let ds = send_raw(
        &target.base,
        "POST",
        "/v1/messages",
        &h,
        Some(br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"x"}]}"#.to_vec()),
    )
    .await;
    assert_eq!(ds.status, 401);
    assert_eq!(
        String::from_utf8_lossy(&ds.body),
        "{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"API key required. Set Authorization header.\"}}\n",
        "anthropic 的 401 体是另一套形状"
    );

    let ds = send_raw(
        &target.base,
        "POST",
        "/v1/responses",
        &h,
        Some(br#"{"model":"m","input":"x"}"#.to_vec()),
    )
    .await;
    assert_eq!(ds.status, 401);

    // `/v1/usage` 的 401 走的是 openai 形状
    let ds = send_raw(&target.base, "GET", "/v1/usage", &HashMap::new(), None).await;
    assert_eq!(ds.status, 401);

    assert_eq!(up.hits().len(), 0, "无 key 的请求不该打上游");
}

/// 带客户端 key 时上游收到的是**客户端那把**（不是服务端默认 key）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_key_wins_over_server_default() {
    let up = FakeUpstream::start(0, "text/event-stream", text_stream()).await;
    let mut proxy = rs_proxy::Proxy::new("server-default-key");
    proxy.base_url = up.base.clone();
    rs_proxy::version::set_version_for_test("9.9.9-gate");
    let target = Target::start(proxy).await;

    let mut h = HashMap::new();
    h.insert("Content-Type".to_string(), "application/json".to_string());
    h.insert("Authorization".to_string(), "Bearer client-key".to_string());
    let ds = send_raw(
        &target.base,
        "POST",
        "/v1/chat/completions",
        &h,
        Some(br#"{"model":"m","messages":[{"role":"user","content":"x"}]}"#.to_vec()),
    )
    .await;
    assert_eq!(ds.status, 200);
    let hits = up.hits();
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].header("authorization").unwrap_or(""),
        "Bearer client-key",
        "客户端 key 应当优先"
    );
}

// ===========================================================================
// 6) `/v1/usage` 的缓存形态（对齐 Go 的「缓存里不带换行」）
// ===========================================================================

/// 第二次请求命中 20s 缓存 ⇒ 不再打上游，且响应体是**缓存里的形态**（无末尾换行）。
///
/// ⚠ 这条不是「照抄口味」：Go 的 `p.writeJSON`（带换行）之后才把 `json.Marshal(sum)`
/// （不带换行）存进 `usageData`，命中缓存时直接 `w.Write` ⇒ 缓存响应少一个字节。
/// 黄金 `usage-endpoints` 两步长度 1298 / 1297 就是这个。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_cache_hit_has_no_trailing_newline() {
    let usage = FakeUsage::start().await;
    let mut proxy = rs_proxy::Proxy::new("k");
    proxy.base_url = usage.base.clone();
    rs_proxy::version::set_version_for_test("9.9.9-gate");
    let target = Target::start(proxy).await;

    let mut h = HashMap::new();
    h.insert("Authorization".to_string(), "Bearer k".to_string());
    let first = send_raw(&target.base, "GET", "/v1/usage", &h, None).await;
    assert_eq!(first.status, 200);
    assert!(
        first.body.ends_with(b"\n"),
        "首次（真打上游）的 /v1/usage 体应带 json.Encoder 的末尾换行"
    );
    let second = send_raw(&target.base, "GET", "/v1/usage", &h, None).await;
    assert_eq!(second.status, 200);
    assert_eq!(
        second.body.len() + 1,
        first.body.len(),
        "缓存命中的体应当比首次少一个字节（末尾换行）"
    );
    assert!(
        !second.body.ends_with(b"\n"),
        "缓存响应不带末尾换行（Go 的缓存存的是 json.Marshal 的形态）"
    );
    assert_eq!(
        second.body, first.body[..first.body.len() - 1],
        "除末尾换行外，缓存体与首次一致"
    );
}

// ===========================================================================
// 7) 基准存在性守门（`.work/` 被清后不至于整批转红）
// ===========================================================================

/// 入库黄金必须在位，且形态完整（场景数 / 别名表 / 目录条数 / 三协议场景齐备）。
#[test]
fn golden_baseline_is_vendored_and_complete() {
    let p = golden_path(GOLDEN_FILE);
    assert!(
        p.exists(),
        "黄金基准不在位：{}（再生成链见 rs-proxy/tests/golden/README.md）",
        p.display()
    );
    let g = load_golden();
    let labels: Vec<String> = g["scenarios"]
        .as_array()
        .expect("scenarios")
        .iter()
        .map(|s| s["label"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(labels.len(), 52, "场景数变了：{labels:?}");
    assert_eq!(g["pure"]["alias_table"].as_object().unwrap().len(), 92);
    assert_eq!(g["pure"]["catalog"].as_array().unwrap().len(), 61);
    // 三协议 + 路由 + 统计 + 用量 + 管线都要在
    for need in [
        "chat-nonstream",
        "chat-stream",
        "resp-nonstream",
        "resp-stream",
        "msg-nonstream",
        "msg-stream",
        "route-boundaries",
        "stats-and-calibration",
        "usage-endpoints",
        "cc-pipeline",
        "anthropic-convert",
        "responses-convert",
    ] {
        assert!(labels.iter().any(|l| l == need), "缺场景 {need}");
    }
    // 场景必须自描述（proxy_key / stats_file 由基准给，不许门禁自己猜）
    for s in g["scenarios"].as_array().unwrap() {
        let label = s["label"].as_str().unwrap_or("");
        assert!(
            s.get("proxy_key").is_some() || matches!(label, "route-boundaries"),
            "{label} 缺 proxy_key（门禁不该猜服务端默认 key）"
        );
    }
    // 出站体不得再含未展开的 `[BIG:…]` 规格串（采集侧已展开，见 golden/README.md 的坑 3）
    let raw = std::fs::read_to_string(&p).expect("读基准");
    assert!(
        !raw.contains("[BIG:"),
        "基准里仍有未展开的 [BIG:…] 规格串（会导致 max_tokens 等测量值失真）"
    );
}

/// **时钟纪律**（P6 修正轮 F1）：黄金里不得有「随墙钟漂移但不归一」的字段。
///
/// `daysLeft` 由「距周期结束的天数」现算（`usage.rs::days_left_from`，每天递减），
/// 采集日定值不能直接比——门禁必须在**两侧**都把它折成 `<DAYSLEFT>`。
/// 这条自检做两件事：① 断言黄金里确实还有 `daysLeft`（否则规则失去作用对象，
/// 将来字段改名会让门禁**静默**失去覆盖）；② 直接断言 `norm_volatile` 的折算行为
/// （含 `0` 与正数两种形态）。
#[test]
fn golden_clock_discipline_days_left_is_normalized() {
    // ① 作用对象存在：黄金原文里必须有 daysLeft（至少两处：endpoints + degraded）
    let p = golden_path("golden-proxy-go.json");
    let raw = std::fs::read_to_string(&p).expect("读基准");
    let hits = raw.matches("\"daysLeft\"").count()
        + raw.matches("daysLeft\\\":").count();
    assert!(
        hits >= 2,
        "黄金里找不到 daysLeft（命中 {hits} 处）：时钟纪律规则失去作用对象，\
         要么字段被改名（请同步 norm_volatile），要么基准被换过"
    );

    // ② 折算行为：正数与 0 都必须被折成同一占位符，且不得残留原值
    for v in ["26679", "0", "99999"] {
        let s = format!("{{\"daysLeft\":{v},\"other\":1}}");
        let n = norm_volatile(&s);
        assert!(
            n.contains("\"daysLeft\":<DAYSLEFT>"),
            "norm_volatile 未折算 daysLeft={v}：{n}"
        );
        assert!(
            !n.contains(&format!("\"daysLeft\":{v}")),
            "norm_volatile 折算后仍残留 daysLeft={v}：{n}"
        );
        // 邻居字段不许被误伤
        assert!(n.contains("\"other\":1"), "折算误伤邻居字段：{n}");
    }

    // ③ null 形态不折算（Go 的 omitempty 会让它根本不出现；出现了就是别的形态）
    let n = norm_volatile("{\"daysLeft\":null}");
    assert!(
        n.contains("\"daysLeft\":null"),
        "daysLeft:null 不该被折算（应原样保留以便暴露形态分歧）：{n}"
    );
}
