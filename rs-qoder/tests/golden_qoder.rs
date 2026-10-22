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

/// 2026-10-20：目录扩充（`cmodel`/`kmodel_latest` 等）后，这几个输入在 Go 基准里是
/// 「不认识 ⇒ 空」，现在必须解析成 worker key。**只列这四个**——旧代号
/// （`q37fmodel`/`gm51model`）与全部拒绝样本仍按黄金逐字比，不在这里开口子。
const RESOLVE_EXTENDED: [(&str, &str); 4] = [
    ("Cantus", "cmodel"),
    ("cmodel", "cmodel"),
    ("Kimi-K3", "kmodel_latest"),
    ("kmodel_latest", "kmodel_latest"),
];

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
        // 2026-10-20：模型目录扩到 14 条（`catalog` 动态目录 + 同内容静态兜底）后，
        // **只有这四个输入**的解析结果有意偏离 Go 基准（基准记的是旧 13 条目录）。
        // 其余输入（含旧代号 q37fmodel/gm51model 与全部拒绝样本）仍逐字节比 Go。
        let want = RESOLVE_EXTENDED
            .iter()
            .find(|(extended_input, _)| *extended_input == input)
            .map(|(_, mapped)| *mapped)
            .unwrap_or_else(|| case["out"].as_str().unwrap());
        assert_eq!(rs_qoder::resolve_model(input), want, "[resolve {input:?}]");
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
            // 与 `models_match_go` 同一条已知偏离（目录改用动态目录（13 条））：目录体走
            // 「信封比黄金 + 目录比手写期望」两条断言；404/307 等非目录体仍逐字节比。
            assert_models_body(
                &format!("route {method} {path}"),
                want_body,
                &String::from_utf8_lossy(&body),
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

/// 2026-10-20：`/v1/models` 的模型目录从写死的 13 条改成 `catalog`
/// （Qoder 客户端 `resources\dynamic-text` 的 14 条）+ 同内容的静态兜底。
/// 显式构造的 `AppState`（黄金回放用的测试缝）固定走静态表，所以这里可复现。
///
/// 这是**有意偏离 Go 黄金**的一处（黄金录的是旧目录）。旧做法「`id` 换回代号后逐字节比」
/// 在条目数 13→14 之后已不可能成立，于是拆成两条**更窄**的断言，避免靠放宽断言变绿：
/// 1. **信封逐字节**：把 `data` 数组挖空后与黄金完全相同（键序/分隔/换行仍锁死）；
/// 2. **目录逐字节**：与下面手写的新期望体完全相同（14 条、worker key 字典序、
///    `id == display_name == 可读名`）。期望体**不从被测代码推导**。
fn models_envelope(body: &str) -> String {
    const OPEN: &str = "{\"data\":[";
    const CLOSE: &str = "],\"object\":\"list\"}";
    let start = body
        .find(OPEN)
        .unwrap_or_else(|| panic!("models 响应体缺 data 前缀: {body:?}"))
        + OPEN.len();
    let end = body
        .find(CLOSE)
        .unwrap_or_else(|| panic!("models 响应体缺 object 后缀: {body:?}"));
    format!("{}{}", &body[..start], &body[end..])
}

/// 新目录的期望响应体（手写；`\n` 是 Go `json.Encoder` 的末尾换行）。
const EXPECTED_MODELS_BODY: &str = concat!(
    "{\"data\":[",
    "{\"display_name\":\"Auto\",\"id\":\"Auto\",\"object\":\"model\"},",
    "{\"display_name\":\"Cantus\",\"id\":\"Cantus\",\"object\":\"model\"},",
    "{\"display_name\":\"DeepSeek-V4-Flash\",\"id\":\"DeepSeek-V4-Flash\",\"object\":\"model\"},",
    "{\"display_name\":\"DeepSeek-V4-Pro\",\"id\":\"DeepSeek-V4-Pro\",\"object\":\"model\"},",
    "{\"display_name\":\"GLM-5.3-Flash\",\"id\":\"GLM-5.3-Flash\",\"object\":\"model\"},",
    "{\"display_name\":\"GLM-5.3\",\"id\":\"GLM-5.3\",\"object\":\"model\"},",
    "{\"display_name\":\"Kimi-K2.7-Code\",\"id\":\"Kimi-K2.7-Code\",\"object\":\"model\"},",
    "{\"display_name\":\"Kimi-K3\",\"id\":\"Kimi-K3\",\"object\":\"model\"},",
    "{\"display_name\":\"MiniMax-M3\",\"id\":\"MiniMax-M3\",\"object\":\"model\"},",
    "{\"display_name\":\"Qwen3.8-Flash\",\"id\":\"Qwen3.8-Flash\",\"object\":\"model\"},",
    "{\"display_name\":\"Qwen3.7-Plus\",\"id\":\"Qwen3.7-Plus\",\"object\":\"model\"},",
    "{\"display_name\":\"Qwen3.8-Max\",\"id\":\"Qwen3.8-Max\",\"object\":\"model\"},",
    "{\"display_name\":\"Qwen3.7-Max\",\"id\":\"Qwen3.7-Max\",\"object\":\"model\"}",
    "],\"object\":\"list\"}\n",
);

/// `/v1/models` 响应体的两条断言（`models_match_go` 与 `routes_match_go` 共用）：
/// 空体（HEAD/OPTIONS）逐字节比黄金；非目录体（404/307）逐字节比黄金；
/// 目录体 = 信封比黄金 + 目录比手写期望。
fn assert_models_body(case: &str, want_golden: &str, got: &str) {
    if !want_golden.contains("\"data\":[") {
        assert_eq!(got, want_golden, "[{case}] 非目录响应体");
        return;
    }
    assert_eq!(
        models_envelope(got),
        models_envelope(want_golden),
        "[{case}] 信封（data 数组以外的键序/分隔/换行）必须与 Go 逐字节一致"
    );
    assert_eq!(got, EXPECTED_MODELS_BODY, "[{case}] 13 条目录必须逐字节一致");
}

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
        // 已知偏离：目录改用动态目录（13 条）（见 `assert_models_body` 的说明）。
        assert_models_body(
            &format!("models {m}"),
            case["body"].as_str().unwrap(),
            &String::from_utf8_lossy(&body),
        );
        // 已知偏离：目录变长 ⇒ 响应体变长 ⇒ HEAD 的 content-length 必然与黄金不同。
        // 只跳过这一项，其余头仍逐项严格比。
        assert_headers_like_go_opt(
            &format!("models {m}"),
            &case["resp_headers"],
            &headers_from_raw(&raw_headers),
            body.len(),
            m == "head",
            true,
        );
        n += 1;
    }
    println!("模型目录黄金对比：{n} 组（6 种方法；信封逐字节 + 目录逐字节）");
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

/// 2026-10-22 有意偏离 Go 的两项 argv（黄金录的是 Go 的旧 argv）：
/// `--permission-mode bypassPermissions`。
///
/// 原因：`--print`（headless）模式默认「遇工具先问」，反代场景没人能回答确认提示，
/// 模型只能回「我没有权限」（DSH 实测：`Error: Allow reading F:\AIXM\...?`）。
///
/// 做法与 `/v1/models` 的可读 id 一致：**不改黄金文件**，比对前把这两项从实得 argv 里
/// 剥掉；同时单独断言它们**确实在**——偏离只允许这一处，其余参数/顺序仍严格一致。
const ARGV_DEVIATION: [&str; 2] = ["--permission-mode", "bypassPermissions"];

fn strip_argv_deviation(mut v: Value) -> Value {
    if let Some(argv) = v.get_mut("argv").and_then(|a| a.as_array_mut()) {
        let mut out: Vec<Value> = Vec::with_capacity(argv.len());
        let mut i = 0usize;
        while i < argv.len() {
            let is_perm = argv[i].as_str() == Some(ARGV_DEVIATION[0])
                && argv.get(i + 1).and_then(|x| x.as_str()) == Some(ARGV_DEVIATION[1]);
            if is_perm {
                // 断言确实带了这对参数（剥之前先验证，不能变成「有没有都通过」）
                i += 2;
                continue;
            }
            out.push(argv[i].clone());
            i += 1;
        }
        *argv = out;
    }
    v
}

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
        // `start` 记录带 argv：先确认权限旁路在，再剥掉后逐字段比
        if gt["kind"].as_str() == Some("start") {
            let argv = gt["argv"].as_array().cloned().unwrap_or_default();
            let p = argv.iter().position(|a| a.as_str() == Some(ARGV_DEVIATION[0]));
            assert!(
                p.is_some(),
                "[{case}] 第 {i} 条记录缺 {} —— headless 下不加会被权限提示卡死",
                ARGV_DEVIATION[0]
            );
            assert_eq!(
                argv[p.unwrap() + 1].as_str(),
                Some(ARGV_DEVIATION[1]),
                "[{case}] {} 的取值必须是 {}",
                ARGV_DEVIATION[0],
                ARGV_DEVIATION[1]
            );
        }
        assert_eq!(
            w,
            &strip_argv_deviation(gt.clone()),
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
