//! 模型三端点的移植门禁：`/account-models`、`/v1/models?full=true`、`/model/info`。
//!
//! 对应 Go `internal/tuanjie/server.go`（提交 `13ecd2e`）：
//! * `handleAccountModels`（597-676）——按账号矩阵，条目字段与声明序逐字一致；
//! * `handleModelsFull`（523-593）——逐账号拉 `/v1/models`，取模型数最多的那份；
//! * `handleModelInfo`（709-870）——别名 × 部署元数据聚合，上游失败时**有旧缓存用旧缓存、
//!   否则 502**（绝不编造费率）。
//!
//! **纪律**：假上游 + 临时 HOME/数据目录 + 合成账号；不碰真实凭据、不碰 8788。
//! 同一测试二进制里的用例共享进程级环境变量与测试缝 base，故用 [`serial`] 串起来。

#[path = "p8b3_scenario/mod.rs"]
mod scenario;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use rs_tuanjie::server::{router, AppState};

/// 进程级共享资源（`USERPROFILE` / `PROXYDECK_DATA_DIR` / 测试缝 base）必须串行。
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// 用例里的一个合成账号。
struct AccSpec {
    uid: &'static str,
    source: &'static str,
    enabled: bool,
    budget: bool,
}

fn acc(uid: &'static str, source: &'static str) -> AccSpec {
    AccSpec {
        uid,
        source,
        enabled: true,
        budget: false,
    }
}

impl AccSpec {
    fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
    fn budget_exceeded(mut self) -> Self {
        self.budget = true;
        self
    }
}

/// 隔离环境：临时 HOME/数据目录 + 合成账号池 + 假上游 + AppState。
struct Run {
    root: PathBuf,
    st: Arc<AppState>,
    up: scenario::FakeUpstream,
}

impl Run {
    fn new(tag: &str, specs: &[AccSpec]) -> Run {
        assert!(!specs.is_empty(), "至少一个账号");
        let root = std::env::temp_dir().join(format!(
            "rs-tuanjie-models-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时根目录");

        // 登录态：本地号的 sub 必须与 accounts.json 里那条 local 对齐。
        let env = scenario::Env::create_with_salt(&root, specs[0].uid, tag);
        env.apply();

        let up = scenario::FakeUpstream::start(vec![]);
        let mut entries: Vec<String> = Vec::new();
        for s in specs {
            let token = scenario::synthetic_jwt_for(s.uid, tag);
            let cli = format!("sk-key-{}", s.uid);
            up.set_token_key(&token, &cli);
            let at = if s.source == "local" { "" } else { token.as_str() };
            entries.push(format!(
                "{{\"user_id\":\"{}\",\"access_token\":\"{}\",\"enabled\":{},\
                  \"budget_exceeded\":{},\"has_glm53\":true,\"source\":\"{}\"}}",
                s.uid, at, s.enabled, s.budget, s.source
            ));
        }
        std::fs::create_dir_all(env.data.join("")).ok();
        std::fs::write(
            env.data.join("tuanjie-accounts.json"),
            format!("{{\"accounts\":[{}]}}", entries.join(",")),
        )
        .expect("写合成账号池");

        rs_tuanjie::client::set_test_bases(up.base(), up.base());
        let st = Arc::new(AppState::new().expect("构造 AppState"));
        Run { root, st, up }
    }

    fn cli_key(uid: &str) -> String {
        format!("sk-key-{uid}")
    }

    fn models_doc(ids: &[&str]) -> String {
        let data: Vec<String> = ids
            .iter()
            .map(|id| format!("{{\"id\":\"{id}\",\"object\":\"model\"}}"))
            .collect();
        format!("{{\"object\":\"list\",\"data\":[{}]}}", data.join(","))
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn get(st: &Arc<AppState>, path: &str) -> (StatusCode, String) {
    let (status, _h, body) = get_with_headers(st, path).await;
    (status, body)
}

async fn get_with_headers(
    st: &Arc<AppState>,
    path: &str,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("构造请求");
    let resp = router(st.clone()).oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), 1 << 24)
        .await
        .expect("读响应体");
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("响应应为合法 JSON：{e}; {body:?}"))
}

fn entry<'a>(v: &'a Value, uid: &str) -> &'a Value {
    v["accounts"]
        .as_array()
        .expect("accounts 是数组")
        .iter()
        .find(|a| a["user_id"] == Value::String(uid.to_string()))
        .unwrap_or_else(|| panic!("accounts 里缺 {uid}: {v}"))
}

// ---------------------------------------------------------------- /account-models

#[tokio::test]
async fn account_models_entry_shape_and_failure_texts() {
    let _g = serial();
    let run = Run::new(
        "acc-shape",
        &[
            acc("uid-a", "local"),
            acc("uid-b", "pool"),
            acc("uid-c", "pool").disabled(),
            acc("uid-d", "pool").budget_exceeded(),
        ],
    );
    run.up.set_models_script(
        &Run::cli_key("uid-a"),
        vec![scenario::Reply::json(
            200,
            &Run::models_doc(&["codely-core", "KIMI-K3"]),
        )],
    );
    run.up.set_models_script(
        &Run::cli_key("uid-b"),
        vec![scenario::Reply::json(500, "{\"error\":\"boom\"}")],
    );

    let (status, headers, body) = get_with_headers(&run.st, "/account-models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let v = json_of(&body);
    assert_eq!(v["ok"], Value::Bool(true));
    assert!(v["fetched_at"].is_i64(), "fetched_at 应为整数: {v}");
    let arr = v["accounts"].as_array().expect("accounts 数组");
    assert_eq!(arr.len(), 4);

    // 账号按 SortedUserIDs（字典序）排列。
    let uids: Vec<&str> = arr.iter().map(|a| a["user_id"].as_str().unwrap()).collect();
    assert_eq!(uids, ["uid-a", "uid-b", "uid-c", "uid-d"]);

    // 成功项：count 必须存在且等于 models.len()（GUI 矩阵直接读 count）。
    let a = entry(&v, "uid-a");
    assert_eq!(a["enabled"], Value::Bool(true));
    assert_eq!(a["budget_exceeded"], Value::Bool(false));
    assert_eq!(a["count"], Value::from(2), "count 必须等于模型数：{a}");
    assert_eq!(
        a["models"],
        serde_json::json!(["codely-core", "KIMI-K3"]),
        "models 应为官方 id 数组：{a}"
    );
    assert!(a.get("err").is_none(), "成功项不应带 err：{a}");
    assert_eq!(
        a["count"].as_i64().unwrap(),
        a["models"].as_array().unwrap().len() as i64
    );

    // 上游非 200 → 逐字 Go 文案。
    let b = entry(&v, "uid-b");
    assert_eq!(b["count"], Value::from(0));
    assert_eq!(b["models"], serde_json::json!([]), "空模型必须是 [] 不是 null");
    assert_eq!(b["err"], Value::String("上游返回 500".into()));

    // 停用 / 配额用尽 → 逐字 Go 文案，count 0、models 空数组。
    for uid in ["uid-c", "uid-d"] {
        let e = entry(&v, uid);
        assert_eq!(
            e["err"],
            Value::String("未参与调度（停用或配额用尽）".into()),
            "{uid}"
        );
        assert_eq!(e["count"], Value::from(0), "{uid}");
        assert_eq!(e["models"], serde_json::json!([]), "{uid}");
    }
    assert_eq!(entry(&v, "uid-c")["enabled"], Value::Bool(false));
    assert_eq!(
        entry(&v, "uid-d")["budget_exceeded"],
        Value::Bool(true)
    );

    // 首项是 local：Go `source` 有值就写（"local"），username 为空则省略。
    assert_eq!(entry(&v, "uid-a")["source"], Value::String("local".into()));
    assert!(entry(&v, "uid-a").get("username").is_none());
}

#[tokio::test]
async fn account_models_parse_failure_cache_and_refresh() {
    let _g = serial();
    let run = Run::new("acc-cache", &[acc("uid-a", "local")]);
    // 首次坏 JSON（→ 该账号 err="上游响应解析失败"），之后正常。
    run.up.set_models_script(
        &Run::cli_key("uid-a"),
        vec![
            scenario::Reply::json(200, "not-json"),
            scenario::Reply::json(200, &Run::models_doc(&["codely-core", "codely-vl"])),
        ],
    );

    let (status, body) = get(&run.st, "/account-models?refresh=1").await;
    assert_eq!(status, StatusCode::OK);
    let v = json_of(&body);
    assert_eq!(
        entry(&v, "uid-a")["err"],
        Value::String("上游响应解析失败".into())
    );

    let (status, body) = get(&run.st, "/account-models?refresh=1").await;
    assert_eq!(status, StatusCode::OK);
    let v = json_of(&body);
    assert_eq!(entry(&v, "uid-a")["count"], Value::from(2));
    assert!(entry(&v, "uid-a").get("err").is_none());

    // 5 分钟缓存：不带 refresh 直接命中，不再打上游。
    let hits = run.up.models_hits();
    let (_, cached_body) = get(&run.st, "/account-models").await;
    assert_eq!(run.up.models_hits(), hits, "第二次应命中缓存（不重复打上游）");
    assert_eq!(cached_body, body, "缓存应逐字节复用上次响应");

    // ?refresh=1 旁路缓存 → 再打一次。
    let (_, _) = get(&run.st, "/account-models?refresh=1").await;
    assert_eq!(
        run.up.models_hits(),
        hits + 1,
        "refresh=1 必须旁路 5 分钟缓存"
    );
}

#[tokio::test]
async fn account_models_token_failure_is_reported() {
    let _g = serial();
    let run = Run::new("acc-token", &[acc("uid-a", "local"), acc("uid-b", "pool")]);
    run.up.set_models_script(
        &Run::cli_key("uid-b"),
        vec![scenario::Reply::json(200, &Run::models_doc(&["codely-core"]))],
    );
    // 构造完 AppState 后抽掉登录态：本地号取 token 失败（Go 文案「取 token 失败」）。
    std::fs::remove_file(
        run.root
            .join("home")
            .join(".codely-cli")
            .join("oauth_creds.json"),
    )
    .expect("删除合成登录态");

    let (status, body) = get(&run.st, "/account-models?refresh=1").await;
    assert_eq!(status, StatusCode::OK);
    let v = json_of(&body);
    assert_eq!(
        entry(&v, "uid-a")["err"],
        Value::String("取 token 失败".into())
    );
    assert_eq!(entry(&v, "uid-b")["count"], Value::from(1));
}

// ---------------------------------------------------------------- ?full=true

#[tokio::test]
async fn models_full_picks_largest_and_falls_back_to_plain_models() {
    let _g = serial();
    // uid-a=本地号但停用（逐账号路径会跳过它）；uid-b 可用；uid-c 停用。
    let run = Run::new(
        "full",
        &[
            acc("uid-a", "local").disabled(),
            acc("uid-b", "pool"),
            acc("uid-c", "pool").disabled(),
        ],
    );

    // 1) 所有可用账号都失败 → 回退普通 /v1/models（假上游默认 1 个模型）。
    run.up.set_models_script(
        &Run::cli_key("uid-b"),
        vec![scenario::Reply::json(500, "")],
    );
    let (status, plain) = get(&run.st, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    let plain = json_of(&plain);
    let (status, body) = get(&run.st, "/v1/models?full=true&refresh=1").await;
    assert_eq!(status, StatusCode::OK, "full=true 不得再回 501/502：{body:?}");
    let fallback = json_of(&body);
    assert!(fallback["data"].is_array(), "{fallback}");

    // 2) uid-b 能给 4 个模型 → 胜出（uid-c 即使脚本更多也不该被请求）。
    run.up.set_models_script(
        &Run::cli_key("uid-b"),
        vec![scenario::Reply::json(
            200,
            &Run::models_doc(&["codely-core", "codely-vl", "KIMI-K3", "GLM-5.3-FLASH"]),
        )],
    );
    run.up.set_models_script(
        &Run::cli_key("uid-c"),
        vec![scenario::Reply::json(
            200,
            &Run::models_doc(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]),
        )],
    );
    let (status, body) = get(&run.st, "/v1/models?full=true&refresh=1").await;
    assert_eq!(status, StatusCode::OK);
    let v = json_of(&body);
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["codely-core", "codely-vl", "KIMI-K3", "GLM-5.3-FLASH"],
        "应返回模型数最多的账号那份原始列表：{v}"
    );
    // 验收标准 2：data 长度 ≥ 现有 /v1/models。
    assert!(
        v["data"].as_array().unwrap().len() >= plain["data"].as_array().unwrap().len(),
        "full=true 的模型数应不少于普通 /v1/models"
    );

    // 3) 5 分钟缓存：不带 refresh 命中，不再打上游；refresh=1 旁路。
    let hits = run.up.models_hits();
    let (_, cached) = get(&run.st, "/v1/models?full=true").await;
    assert_eq!(cached, body, "缓存应逐字节复用");
    assert_eq!(run.up.models_hits(), hits, "第二次应命中缓存");
    let (_, _) = get(&run.st, "/v1/models?full=true&refresh=1").await;
    assert_eq!(run.up.models_hits(), hits + 1, "refresh=1 必须旁路缓存");
}

// ---------------------------------------------------------------- /model/info

#[tokio::test]
async fn model_info_aggregates_alias_metadata() {
    let _g = serial();
    let run = Run::new("info", &[acc("uid-a", "local")]);

    // 别名清单（默认客户端与本地号共用同一把 cli key）。
    run.up.set_models_script(
        &Run::cli_key("uid-a"),
        vec![scenario::Reply::json(
            200,
            r#"{"object":"list","data":[
                {"id":"codely-core","max_model_len":128000},
                {"id":"KIMI-K3"}
            ]}"#,
        )],
    );
    // 部署元数据：-internal/-public 两个变体按去后缀合并，字段取先出现者。
    run.up.set_path_script(
        "/model/info",
        vec![scenario::Reply::json(
            200,
            r#"{"data":[
                {"model_name":"codely-core-internal",
                 "litellm_params":{"model":"glm-5-fp8-128k"},
                 "model_info":{"max_tokens":8192,"max_input_tokens":200000,
                               "input_cost_per_token":0.0000016,
                               "output_cost_per_token":0.0000032,
                               "cache_read_input_token_cost":0.0000001}},
                {"model_name":"codely-core-public",
                 "litellm_params":{"model":"glm-5-2-260617"},
                 "model_info":{"max_tokens":999999,"max_input_tokens":1,
                               "input_cost_per_token":99.0,
                               "output_cost_per_token":99.0,
                               "cache_read_input_token_cost":99.0}}
            ]}"#,
        )],
    );

    let (status, headers, body) = get_with_headers(&run.st, "/model/info").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    assert!(body.ends_with('\n'), "writeJSON 末尾应带换行：{body:?}");
    let v = json_of(&body);
    let arr = v.as_array().expect("/model/info 是数组");
    assert_eq!(arr.len(), 2);

    let core = &arr[0];
    assert_eq!(core["name"], Value::String("codely-core".into()));
    // /v1/models 的 max_model_len 优先于部署元数据。
    assert_eq!(core["maxInput"], Value::from(128000));
    assert_eq!(core["maxOutput"], Value::from(8192));
    assert!((core["inputCost"].as_f64().unwrap() - 1.6).abs() < 1e-9);
    assert!((core["outputCost"].as_f64().unwrap() - 3.2).abs() < 1e-9);
    assert!((core["cacheReadCost"].as_f64().unwrap() - 0.1).abs() < 1e-9);
    assert_eq!(
        core["backends"],
        serde_json::json!(["glm-5-fp8-128k", "glm-5-2-260617"]),
        "后端去重且保序"
    );
    assert_eq!(core["reasoning"], Value::String("max/high/low".into()));
    assert!(core["note"].as_str().unwrap().contains("glm-5-2-260617"));

    // 没有部署元数据的别名：只有名字 + 推理档位，**没有任何费率/上限**。
    let kimi = &arr[1];
    assert_eq!(kimi["name"], Value::String("KIMI-K3".into()));
    assert_eq!(kimi["reasoning"], Value::String("max/high/low".into()));
    for k in [
        "maxInput",
        "maxOutput",
        "inputCost",
        "outputCost",
        "cacheReadCost",
        "backends",
    ] {
        assert!(kimi.get(k).is_none(), "KIMI-K3 不应有 {k}: {kimi}");
    }

    // 1 小时缓存：第二次不再打上游，响应逐字节相同。
    let hits = run.up.models_hits();
    let (_, cached) = get(&run.st, "/model/info").await;
    assert_eq!(run.up.models_hits(), hits, "/model/info 第二次应命中缓存");
    assert_eq!(cached, body);
}

#[tokio::test]
async fn model_info_upstream_403_yields_502_without_inventing_rates() {
    let _g = serial();
    let run = Run::new("info-403", &[acc("uid-a", "local")]);
    run.up.set_models_script(
        &Run::cli_key("uid-a"),
        vec![scenario::Reply::json(
            200,
            &Run::models_doc(&["codely-core"]),
        )],
    );
    // 上游 /model/info 早已 403（nginx HTML）——与 Go 同口径：解析失败且无旧缓存 → 502。
    run.up.set_path_script(
        "/model/info",
        vec![scenario::Reply {
            code: 403,
            headers: vec![("Content-Type".to_string(), "text/html".to_string())],
            body: b"<html><body>403 Forbidden</body></html>".to_vec(),
        }],
    );

    let (status, body) = get(&run.st, "/model/info").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body:?}");
    let v = json_of(&body);
    let msg = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("解析 /model/info 失败"),
        "错误文案应说明解析失败：{body:?}"
    );
    assert_eq!(v["error"]["type"], Value::String("api_error".into()));
    // 绝不编造：失败响应里不得出现任何费率字段。
    for k in ["inputCost", "outputCost", "cacheReadCost", "maxInput"] {
        assert!(!body.contains(k), "403 时不得返回 {k}: {body:?}");
    }

    // 失败不写缓存：第二次仍然 502（而不是返回一份凭空缓存）。
    run.up.set_path_script(
        "/model/info",
        vec![scenario::Reply {
            code: 403,
            headers: vec![("Content-Type".to_string(), "text/html".to_string())],
            body: b"<html><body>403 Forbidden</body></html>".to_vec(),
        }],
    );
    let (status2, _) = get(&run.st, "/model/info").await;
    assert_eq!(status2, StatusCode::BAD_GATEWAY);
}
