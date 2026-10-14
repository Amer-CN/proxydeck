// tests/golden_qoder.rs —— P3 黄金门禁：Rust 侧对着**真 Go 采收**的基准逐字节比对。
//
// 基准：`tests/golden/golden-qoder-go.json`（`internal/qoder` 采，见 tests/golden/README.md）。
// 门禁口径与采集器**同一套归一化规则**（`tests/common/mod.rs`）：
// 忽略时钟与进程内现生成的值（`created` / `id` / `uptimeSec` / 账本 `updatedAt` / 响应头 `date`）。
//
// 三条**显式跳过**（不静默）：
// 1. 合成登录态夹具解不开（DPAPI 是当前用户作用域，换机即失效）⇒ 打 SKIP 并计入汇总；
// 2. `oversize-line`（单行 >8MB）压根没入基准（Go 侧采一次要等 600s 上下文超时）；
// 3. 采集机路径本机不存在（见 tests/golden/README.md「采集机字面量」）⇒ 该组 SKIP 并计数。
mod common;

use common::*;
use serde_json::Value;
use std::sync::Arc;

// ---------- 纯函数 ----------

#[test]
fn pure_functions_match_go() {
    let g = load_golden();
    let mut checks = 0usize;

    for case in g["flatten"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let msgs: Vec<Value> = case["messages"].as_array().unwrap().clone();
        assert_eq!(
            rs_qoder::flatten_messages(&msgs),
            case["out"].as_str().unwrap(),
            "[flatten {id}]"
        );
        checks += 1;
    }

    for case in g["resolve"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        assert_eq!(
            rs_qoder::resolve_model(input),
            case["out"].as_str().unwrap(),
            "[resolve {input:?}]"
        );
        checks += 1;
    }

    for case in g["chunks"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let kind = case["kind"].as_str().unwrap();
        let delta = case["delta"].as_str().unwrap();
        let finish = case["finish"].as_str().unwrap();
        let model = case["model"].as_str().unwrap();
        let got = if kind == "text" {
            rs_qoder::chunk_json(id, model, delta, finish, 1234567890)
        } else {
            rs_qoder::chunk_reasoning_json(id, model, delta, finish, 1234567890)
        };
        assert_eq!(
            String::from_utf8(got).unwrap(),
            case["out"].as_str().unwrap(),
            "[chunk {id}]"
        );
        checks += 1;
    }

    for case in g["complete"].as_array().unwrap() {
        let got = rs_qoder::complete_json(
            case["id"].as_str().unwrap(),
            case["model"].as_str().unwrap(),
            case["content"].as_str().unwrap(),
            1234567890,
        );
        assert_eq!(
            String::from_utf8(got).unwrap(),
            case["out"].as_str().unwrap(),
            "[complete {}]",
            case["id"]
        );
        checks += 1;
    }

    for case in g["quota_error"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        assert_eq!(
            rs_qoder::is_quota_error(input),
            case["out"].as_bool().unwrap(),
            "[is_quota_error {input:?}]"
        );
        checks += 1;
    }

    for case in g["rfc3339"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        let ok = case["ok"].as_bool().unwrap();
        let is_zero = case["is_zero"].as_bool().unwrap();
        let got = rs_qoder::auth::parse_rfc3339_millis(input);
        if ok && !is_zero {
            assert_eq!(
                got,
                Some(case["unix_ms"].as_i64().unwrap()),
                "[rfc3339 {input:?}]"
            );
        } else {
            // Go 的零值时间（IsZero）与解析失败都要落成 None（见 auth.rs 的注释）
            assert_eq!(got, None, "[rfc3339 {input:?}] ok={ok} is_zero={is_zero}");
        }
        checks += 1;
    }

    for case in g["floats"].as_array().unwrap() {
        let v = case["in"].as_f64().unwrap();
        assert_eq!(
            rs_qoder::gjson::f(v).to_go(),
            case["out"].as_str().unwrap(),
            "[float {v:?}]"
        );
        checks += 1;
    }

    for case in g["usage_view"].as_array().unwrap() {
        let raw = case["in"].as_str().unwrap();
        let parsed: Result<rs_qoder::usage::UsageQuota, _> = serde_json::from_str(raw);
        match (parsed, case["out"].as_str()) {
            (Ok(u), Some(want)) => assert_eq!(
                rs_qoder::usage::usage_view(&u).map(|v| v.to_go()),
                Some(want.to_string()),
                "[usage_view {raw}]"
            ),
            (Ok(u), None) => assert!(
                rs_qoder::usage::usage_view(&u).is_none(),
                "[usage_view {raw}] Go 回 nil，Rust 也得回 None"
            ),
            (Err(_), _) => {
                // Go 侧 json.Unmarshal 失败 ⇒ fetchUsage 报错（调用方回落旧缓存）；
                // 这里只要求 Rust 也拒绝同一份输入。
                assert!(case["err"].as_str().is_some(), "[usage_view {raw}] 黄金没记错误");
            }
        }
        checks += 1;
    }

    // 账本：读（含缺失/损坏/类型错/字段大小写与重复键）
    let dir = TestDir::new("golden-ledger");
    for (i, case) in g["credits_ledger"]["parse"].as_array().unwrap().iter().enumerate() {
        let raw = case["in"].as_str().unwrap();
        let want = case["out"].as_f64().unwrap();
        let p = dir.join(&format!("ledger-{i}.json"));
        if raw == "<absent>" {
            let _ = std::fs::remove_file(&p);
        } else {
            std::fs::write(&p, raw).unwrap();
        }
        assert_eq!(
            rs_qoder::credits::read_spent(Some(&p)),
            want,
            "[credits parse {raw:?}]"
        );
        checks += 1;
    }
    // 账本：写形态（键序、浮点形状；updatedAt 是时钟，归一化后比）
    let wp = dir.join("ledger-write.json");
    rs_qoder::credits::write_spent(&wp, 1.5, 1792001113643).unwrap();
    assert_eq!(
        normalize_json_text(&std::fs::read_to_string(&wp).unwrap()),
        normalize_json_text(g["credits_ledger"]["write_first"].as_str().unwrap())
    );
    rs_qoder::credits::write_spent(&wp, 0.0, 1792001113643).unwrap();
    assert_eq!(
        normalize_json_text(&std::fs::read_to_string(&wp).unwrap()),
        normalize_json_text(g["credits_ledger"]["write_zero"].as_str().unwrap())
    );
    checks += 2;

    println!("纯函数黄金对比：{checks} 项与 Go 逐字节/逐值一致");
}

// ---------- 夹具可用性 ----------

/// 需要登录态的场景统一用这个构造（`credits` 传账本路径）。
fn state_with_fixture(
    auth_dir: std::path::PathBuf,
    credits: Option<std::path::PathBuf>,
    cwd_env: Option<String>,
) -> Arc<rs_qoder::AppState> {
    Arc::new(rs_qoder::AppState::new(auth_dir, None, credits, cwd_env))
}

// ---------- /health ----------

#[tokio::test]
async fn health_cases_match_go() {
    let g = load_golden();

    // 无登录态：auth_dir 指到空目录（不需要夹具，任何机器都能跑）
    let dir = TestDir::new("golden-health");
    let empty = TestDir::new("golden-health-empty");
    let srv = AppServer::start(state_with_fixture(
        empty.path.clone(),
        Some(dir.join(".qoder-cli-credits.json")),
        None,
    ))
    .await;
    let h = hop(&srv.base, "GET", "/health", None).await;
    assert_eq!(h.status, g["health_no_license"]["status"].as_u64().unwrap() as u16);
    assert_eq!(h.body, g["health_no_license"]["body"].as_str().unwrap());
    assert_headers_like_go(
        "health-no-license",
        &g["health_no_license"]["resp_headers"],
        &h.headers,
        h.body.len(),
        false,
    );
    // 坏 Local State / 坏 encrypted_key 同样是 no_license（两条都在黄金里）
    for name in ["health_bad_local_state", "health_bad_encrypted_key"] {
        let bad = TestDir::new(&format!("golden-{name}"));
        std::fs::write(bad.join("Local State"), "{not json").unwrap();
        let srv = AppServer::start(state_with_fixture(bad.path.clone(), None, None)).await;
        let h = hop(&srv.base, "GET", "/health", None).await;
        assert_eq!(h.status, 200, "[{name}]");
        assert_eq!(h.body, g[name]["body"].as_str().unwrap(), "[{name}]");
    }
    let mut checked = 3usize;

    if !auth_fixture_usable() {
        // 计数直接从黄金读，避免写死后随基准条数漂移
        let set_n = g["credits_set"].as_array().map(|a| a.len()).unwrap_or(0);
        let chat_n = g["chat"].as_array().map(|a| a.len()).unwrap_or(0);
        println!(
            "SKIP(dpapi-fixture): 合成登录态夹具解不开（DPAPI 当前用户作用域，换机即失效）—— \
             /health(ok/refresh×4)、/credits/set {set_n} 组、chat {chat_n} 组未跑；重采见 tests/golden/README.md"
        );
        println!("黄金对照汇总：/health 跑了 {checked} 组（no_license 三形态）；显式跳过 1 类（DPAPI 夹具）");
        return;
    }

    // 有登录态 + 账本 1.25；**不带 refresh 时绝不访问官方额度接口**
    let home = TestDir::new("golden-health-ok");
    let ledger = home.join(".qoder-cli-credits.json");
    std::fs::write(&ledger, r#"{"spent":1.25,"updatedAt":1700000000000}"#).unwrap();
    let srv = AppServer::start(state_with_fixture(
        auth_fixture_dir(),
        Some(ledger.clone()),
        None,
    ))
    .await;
    let h = hop(&srv.base, "GET", "/health", None).await;
    assert_eq!(h.status, 200);
    assert_eq!(
        normalize_json_text(&h.body),
        normalize_json_text(g["health_ok"]["body"].as_str().unwrap()),
        "[/health ok]"
    );
    assert_headers_like_go(
        "health-ok",
        &g["health_ok"]["resp_headers"],
        &h.headers,
        h.body.len(),
        false,
    );
    let h2 = hop(&srv.base, "POST", "/health", None).await;
    assert_eq!(h2.status, 200, "POST /health 也回 200（Go 不看方法）");
    assert_eq!(
        normalize_json_text(&h2.body),
        normalize_json_text(g["health_ok"]["post_body"].as_str().unwrap())
    );
    checked += 1;

    // ---- refresh=1 系列：现拉额度（假上游，按黄金记的形态应答） ----
    // 这一组另起实例（与采集器同构：采集时 refresh 系列也是同一实例内连跑四步）
    let case = &g["health_refresh"];
    let usage = FakeUsage::start(
        case["usage_status"].as_u64().unwrap() as u16,
        case["usage_body"].as_str().unwrap(),
    )
    .await;
    let st = Arc::new(
        rs_qoder::AppState::new(
            auth_fixture_dir(),
            None,
            Some(ledger.clone()),
            None,
        )
        .with_usage_url(usage.url.clone()),
    );
    let srv = AppServer::start(st).await;

    let h = hop(&srv.base, "GET", "/health?refresh=1", None).await;
    assert_eq!(
        normalize_json_text(&h.body),
        normalize_json_text(case["body"].as_str().unwrap()),
        "[/health?refresh=1]"
    );
    let hits = usage.take_hits();
    assert_eq!(hits.len(), 1, "refresh=1 必须**恰好**去拉一次额度");
    assert_eq!(
        hits[0].2, "Bearer dt-fake-token-for-golden",
        "额度接口的 Authorization 注入"
    );
    assert_eq!(hits[0].3, "Qoder", "额度接口的 User-Agent");
    assert_eq!(hits[0].4, "1", "额度接口的 Cosy-ClientType");
    assert!(hits[0].1.contains("/api/v2/quota/usage"), "额度接口路径");

    // 不带 refresh → 命中缓存、**一次都不打接口**
    let h = hop(&srv.base, "GET", "/health", None).await;
    assert_eq!(
        normalize_json_text(&h.body),
        normalize_json_text(case["cache_body"].as_str().unwrap()),
        "[/health 命中缓存]"
    );
    assert_eq!(usage.hits(), 0, "不带 refresh 时不该访问额度接口");
    checked += 1;

    // isQuotaExceeded=true → credits.quotaExceeded 被顶成 true
    let case = &g["health_quota_exceeded"];
    usage.set(
        case["usage_status"].as_u64().unwrap() as u16,
        case["usage_body"].as_str().unwrap(),
    );
    let h = hop(&srv.base, "GET", "/health?refresh=1", None).await;
    let _ = usage.take_hits();
    assert_eq!(
        normalize_json_text(&h.body),
        normalize_json_text(case["body"].as_str().unwrap()),
        "[/health?refresh=1 配额超限]"
    );
    checked += 1;

    // 接口挂 → 保留旧缓存（旧缓存还是「超限」那一份）
    let case = &g["health_refresh_fail"];
    usage.set(
        case["usage_status"].as_u64().unwrap() as u16,
        case["usage_body"].as_str().unwrap(),
    );
    let h = hop(&srv.base, "GET", "/health?refresh=1", None).await;
    let _ = usage.take_hits();
    assert_eq!(
        normalize_json_text(&h.body),
        normalize_json_text(case["body"].as_str().unwrap()),
        "[/health?refresh=1 接口挂 → 保旧缓存]"
    );
    checked += 1;

    // 坏 JSON（200）→ 同样保旧缓存
    let case = &g["health_refresh_bad_json"];
    usage.set(
        case["usage_status"].as_u64().unwrap() as u16,
        case["usage_body"].as_str().unwrap(),
    );
    let h = hop(&srv.base, "GET", "/health?refresh=1", None).await;
    let _ = usage.take_hits();
    assert_eq!(
        normalize_json_text(&h.body),
        normalize_json_text(case["body"].as_str().unwrap()),
        "[/health?refresh=1 坏 JSON → 保旧缓存]"
    );
    checked += 1;

    // 全新实例 + 接口挂 → 没有 realtime 键
    let case = &g["health_refresh_cold_fail"];
    usage.set(
        case["usage_status"].as_u64().unwrap() as u16,
        case["usage_body"].as_str().unwrap(),
    );
    let st = Arc::new(
        rs_qoder::AppState::new(auth_fixture_dir(), None, Some(ledger.clone()), None)
            .with_usage_url(usage.url.clone()),
    );
    let srv = AppServer::start(st).await;
    let h = hop(&srv.base, "GET", "/health?refresh=1", None).await;
    let _ = usage.take_hits();
    assert_eq!(
        normalize_json_text(&h.body),
        normalize_json_text(case["body"].as_str().unwrap()),
        "[/health?refresh=1 冷启动 + 接口挂]"
    );
    checked += 1;

    println!("黄金对照汇总：/health 跑了 {checked} 组（含 refresh 四步与假额度上游）；显式跳过 0 类");
}

// ---------- 路由边界 ----------

#[tokio::test]
async fn routes_match_go() {
    let g = load_golden();
    let dir = TestDir::new("golden-routes");
    let fallback = TestDir::new("golden-routes-noauth");
    let auth = if auth_fixture_usable() {
        auth_fixture_dir()
    } else {
        fallback.path.clone()
    };
    let srv = AppServer::start(state_with_fixture(
        auth,
        Some(dir.join(".qoder-cli-credits.json")),
        None,
    ))
    .await;
    let mut n = 0usize;
    for case in g["routes"].as_array().unwrap() {
        let method = case["method"].as_str().unwrap();
        let path = case["path"].as_str().unwrap();
        let want_status = case["status"].as_u64().unwrap() as u16;
        let (status, raw_headers, body) = raw_request(&srv.addr, method, path, &[], "").await;
        assert_eq!(want_status, status, "[route {method} {path}] 状态码");
        let want_body = case["body"].as_str().unwrap();
        if path.contains("/health") && want_status == 200 {
            // /health 的体含时钟字段，归一化
            assert_eq!(
                normalize_json_text(&String::from_utf8_lossy(&body)),
                normalize_json_text(want_body),
                "[route {method} {path}] body"
            );
        } else {
            assert_eq!(
                String::from_utf8_lossy(&body),
                want_body,
                "[route {method} {path}] body"
            );
        }
        assert_headers_like_go(
            &format!("route {method} {path}"),
            &case["resp_headers"],
            &headers_from_raw(&raw_headers),
            body.len(),
            method == "HEAD",
        );
        n += 1;
    }
    println!("路由/边界黄金对比：{n} 组逐字节一致");
}

// ---------- /v1/models ----------

#[tokio::test]
async fn models_match_go() {
    let g = load_golden();
    let dir = TestDir::new("golden-models");
    let fallback = TestDir::new("golden-models-noauth");
    let auth = if auth_fixture_usable() {
        auth_fixture_dir()
    } else {
        fallback.path.clone()
    };
    let srv = AppServer::start(state_with_fixture(
        auth,
        Some(dir.join(".qoder-cli-credits.json")),
        None,
    ))
    .await;
    let mut n = 0usize;
    for m in ["get", "post", "put", "delete", "head", "options"] {
        let case = &g[format!("models_{m}")];
        let (status, raw_headers, body) =
            raw_request(&srv.addr, &m.to_uppercase(), "/v1/models", &[], "").await;
        let want = case["status"].as_u64().unwrap() as u16;
        assert_eq!(want, status, "[/v1/models {m}]");
        assert_eq!(
            String::from_utf8_lossy(&body),
            case["body"].as_str().unwrap(),
            "[/v1/models {m}] body"
        );
        assert_headers_like_go(
            &format!("models {m}"),
            &case["resp_headers"],
            &headers_from_raw(&raw_headers),
            body.len(),
            m == "head",
        );
        n += 1;
    }
    println!("模型目录黄金对比：{n} 组（6 种方法）逐字节一致");
}

// ---------- /credits/set ----------

#[tokio::test]
async fn credits_set_matches_go() {
    let g = load_golden();
    if !auth_fixture_usable() {
        println!(
            "SKIP(dpapi-fixture): /credits/set {} 组需要合成登录态夹具，本机解不开——未跑",
            g["credits_set"].as_array().map(|a| a.len()).unwrap_or(0)
        );
        return;
    }
    let dir = TestDir::new("golden-set");
    let ledger = dir.join(".qoder-cli-credits.json");
    let srv = AppServer::start(state_with_fixture(
        auth_fixture_dir(),
        Some(ledger.clone()),
        None,
    ))
    .await;
    let mut n = 0usize;
    for case in g["credits_set"].as_array().unwrap() {
        // 每步先把账本复位（与采集器一致），保证读数可复现
        std::fs::write(&ledger, r#"{"spent":9.5,"updatedAt":1}"#).unwrap();
        let method = case["method"].as_str().unwrap();
        let body_in = case["body_in"].as_str().unwrap();
        let (status, raw_headers, body) = raw_request(
            &srv.addr,
            method,
            "/credits/set",
            &[("Content-Type", "application/json")],
            body_in,
        )
        .await;
        assert_eq!(
            case["status"].as_u64().unwrap() as u16,
            status,
            "[/credits/set {}] 状态码",
            case["id"]
        );
        assert_eq!(
            String::from_utf8_lossy(&body),
            case["body_out"].as_str().unwrap(),
            "[/credits/set {}] body",
            case["id"]
        );
        assert_headers_like_go(
            &format!("credits/set {}", case["id"]),
            &case["resp_headers"],
            &headers_from_raw(&raw_headers),
            body.len(),
            false,
        );
        let ledger_now = std::fs::read_to_string(&ledger).unwrap_or_default();
        assert_eq!(
            normalize_json_text(&ledger_now),
            normalize_json_text(case["ledger_after"].as_str().unwrap()),
            "[/credits/set {}] 账本",
            case["id"]
        );
        let h = hop(&srv.base, "GET", "/health", None).await;
        assert_eq!(
            normalize_json_text(&h.body),
            normalize_json_text(case["health_after"].as_str().unwrap()),
            "[/credits/set {}] 事后的 /health",
            case["id"]
        );
        n += 1;
    }
    println!("积分账本接口黄金对比：{n} 组（含账本落盘形态）逐字节一致");
}

// ---------- /v1/chat/completions ----------

fn worker_logs_equal(case: &str, want: &[Value], got: &[Value]) {
    let kinds = |v: &[Value]| -> Vec<String> {
        v.iter()
            .map(|x| x["kind"].as_str().unwrap_or("?").to_string())
            .collect()
    };
    assert_eq!(
        kinds(want),
        kinds(got),
        "[{case}] 假 worker 记录条数/种类不一致（时序或退出方式不同）"
    );
    for (i, (w, gt)) in want.iter().zip(got.iter()).enumerate() {
        assert_eq!(
            w, gt,
            "[{case}] 假 worker 第 {i} 条记录不一致（argv/cwd/env/payload/stdin）\n  期望: {w}\n  实得: {gt}"
        );
    }
}

#[tokio::test]
async fn chat_scenarios_match_go() {
    let g = load_golden();
    let cases = g["chat"].as_array().unwrap();

    if !auth_fixture_usable() {
        println!(
            "SKIP(dpapi-fixture): 合成登录态夹具解不开（DPAPI 当前用户作用域）—— chat 的 {} 组未跑",
            cases.len()
        );
        println!("黄金对照汇总：chat 0 组；显式跳过 1 类（DPAPI 夹具）");
        return;
    }

    let worker = FakeWorker::new("golden-chat");
    let home = TestDir::new("golden-chat-home");
    let ledger = home.join(".qoder-cli-credits.json"); // 起始不存在，与采集器一致

    // 采集时是一条流水跑完的：`health_after` 记的是**同一个 Server 实例**里的累计。
    // 所以这里按 `cwd_env` 的取值分段复用实例（Rust 的 cwd_env 是构造时定死的，见 README 差异 1）；
    // 分段依据直接取自黄金每条场景的 `cwd_env`，不再写死下标。
    // 采集机路径（临时目录 / 仓库根）在这里换成**本机**等价物：换机不重采也不假红。
    let mp = MachinePaths::from_golden(&g);

    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut server: Option<AppServer> = None;
    let mut server_env: Option<String> = None;

    for (i, case) in cases.iter().enumerate() {
        let want_env = mp.local(case["cwd_env"].as_str().unwrap_or(""));
        // 本机没有这个目录时，Go/Rust 都会回落到临时目录，而黄金记的是采集机那条路径 ——
        // 这一条在本机不可判定，**显式跳过并计数**，不当成差异（见 README「采集机字面量」）。
        if !want_env.is_empty() && !std::path::Path::new(&want_env).is_dir() {
            println!(
                "SKIP(machine-path): chat #{i} 的 QODER_CWD={want_env} 在本机不是目录\
                 （黄金绑定采集机路径，重采见 tests/golden/README.md）——该组未跑"
            );
            skipped += 1;
            continue;
        }
        if server.is_none() || server_env.as_deref() != Some(want_env.as_str()) {
            server_env = Some(want_env.clone());
            let cwd = if want_env.is_empty() { None } else { Some(want_env) };
            let st = Arc::new(rs_qoder::AppState::new(
                auth_fixture_dir(),
                Some(worker.path()),
                Some(ledger.clone()),
                cwd,
            ));
            server = Some(AppServer::start(st).await);
        }
        let srv = server.as_ref().unwrap();

        worker.set_scenario(case["mode"].as_str().unwrap());
        let method = case["method"].as_str().unwrap();
        let body_in = case["body_in"].as_str().unwrap();
        let h = if method == "GET" {
            hop(&srv.base, "GET", "/v1/chat/completions", None).await
        } else {
            hop(&srv.base, "POST", "/v1/chat/completions", Some(body_in)).await
        };
        let log = worker.log();
        let label = format!("chat #{i} {} {}", case["mode"], case["body_in"]);

        assert_eq!(
            case["status"].as_u64().unwrap() as u16,
            h.status,
            "[{label}] 状态码；body={}",
            h.body
        );
        assert_eq!(
            normalize_volatile(&h.body),
            normalize_volatile(case["body_out"].as_str().unwrap()),
            "[{label}] body"
        );
        assert_headers_like_go(
            &label,
            &case["resp_headers"],
            &h.headers,
            h.body.len(),
            false,
        );
        // 黄金里「一条记录都没有」会是 JSON null（Go 的 nil 切片），按空数组处理。
        // `cwd` 记的是采集机的路径字面量（默认那条 = 采集机临时目录），换成本机等价物再比。
        let mut want_log: Vec<Value> = case
            .get("worker_log")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for entry in want_log.iter_mut().filter(|e| e.get("cwd").is_some()) {
            let local = mp.local(entry["cwd"].as_str().unwrap_or(""));
            entry["cwd"] = Value::String(local);
        }
        worker_logs_equal(&label, &want_log, &log);
        if let Some(hb) = case.get("health_after").and_then(|v| v.as_str()) {
            let hh = hop(&srv.base, "GET", "/health", None).await;
            assert_eq!(
                normalize_json_text(&hh.body),
                normalize_json_text(hb),
                "[{label}] 事后的 /health（进程内累计）"
            );
        }
        if let Some(lb) = case.get("ledger_after").and_then(|v| v.as_str()) {
            let now = std::fs::read_to_string(&ledger).unwrap_or_default();
            assert_eq!(
                normalize_json_text(&now),
                normalize_json_text(lb),
                "[{label}] 事后的账本"
            );
        }
        checked += 1;
    }

    // 无登录态：独立实例 + 空 auth_dir，且**一个 worker 都不该被 spawn**
    {
        let empty = TestDir::new("golden-chat-noauth");
        let case = &g["chat_no_license"];
        worker.set_scenario("normal");
        let st = Arc::new(rs_qoder::AppState::new(
            empty.path.clone(),
            Some(worker.path()),
            Some(ledger.clone()),
            None,
        ));
        let srv = AppServer::start(st).await;
        let h = hop(
            &srv.base,
            "POST",
            "/v1/chat/completions",
            Some(r#"{"model":"Auto","messages":[]}"#),
        )
        .await;
        assert_eq!(case["status"].as_u64().unwrap() as u16, h.status);
        // 采集器这一条用的键名是 `body`（其它 chat 场景是 `body_out`），两种都认
        let want_body = case["body_out"]
            .as_str()
            .or_else(|| case["body"].as_str())
            .expect("黄金缺 chat_no_license.body");
        assert_eq!(h.body, want_body);
        assert_headers_like_go(
            "chat-no-license",
            &case["resp_headers"],
            &h.headers,
            h.body.len(),
            false,
        );
        assert!(worker.log().is_empty(), "no_license 分支不该拉起 worker");
        checked += 1;
    }

    println!(
        "chat 黄金对比：{checked} 组（状态码/响应体/响应头/假 worker 观测）逐字节一致；\
         显式跳过 {skipped} 组（采集机路径，本机没有该目录）"
    );
}
