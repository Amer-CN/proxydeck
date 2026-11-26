//! A02：WorkBuddy 初选与换号都必须按最终上游模型过滤 11158 黑名单。

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::any;
use axum::Router;
use common::{send, write_auth_files, AppServer, EnvGuard, TestDir};
use rs_codebuddy::cred::StubCredential;
use rs_codebuddy::failover::HY4_PRIMARY;
use rs_codebuddy::pool::AccountPool;
use rs_codebuddy::region::Region;
use serde_json::{json, Value};

const MODEL_M: &str = "gpt-6";
const MODEL_N: &str = "glm-5.2";
const FALLBACK_M: &str = "deepseek-v4-pro";
const OK_SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

struct ModelUpstream {
    base: String,
    hits: Arc<Mutex<Vec<(String, String)>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl ModelUpstream {
    async fn start() -> Self {
        Self::start_with("u-a", false).await
    }

    async fn start_with(fallback_error_uid: &str, hy4_quota_429: bool) -> Self {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sink = hits.clone();
        let fallback_error_uid = fallback_error_uid.to_string();
        let app = Router::new().fallback(any(move |req: Request| {
            let sink = sink.clone();
            let fallback_error_uid = fallback_error_uid.clone();
            let hy4_quota_429 = hy4_quota_429;
            async move {
                let (parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let uid = parts
                    .headers
                    .get("x-user-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let model = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default();
                sink.lock().unwrap().push((uid.clone(), model.clone()));
                if hy4_quota_429 && model == HY4_PRIMARY {
                    return (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        Body::from(
                            r#"{"code":6004,"msg":"Quota exceeded, will reset at 2026-10-15 06:20:02 UTC+8"}"#,
                        ),
                    )
                        .into_response();
                }
                if uid == fallback_error_uid && (model == MODEL_M || model == FALLBACK_M) {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        Body::from(
                            r#"{"code":11158,"msg":"model is not allowed for user type"}"#,
                        ),
                    )
                        .into_response();
                }
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    Body::from(OK_SSE),
                )
                    .into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { base, hits, handle }
    }

    fn hits(&self) -> Vec<(String, String)> {
        self.hits.lock().unwrap().clone()
    }
}

impl Drop for ModelUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct Fixture {
    app: AppServer,
    _auth: TestDir,
    data: TestDir,
    _home: TestDir,
}

async fn fixture(
    env: &EnvGuard,
    region: Region,
    upstream: &ModelUpstream,
    tag: &str,
    configure_pool: impl FnOnce(&AccountPool),
    configure_state: impl FnOnce(&mut rs_codebuddy::AppState),
) -> Fixture {
    let auth = TestDir::new(&format!("{tag}-auth"));
    let data = TestDir::new(&format!("{tag}-data"));
    let home = TestDir::new(&format!("{tag}-home"));
    let domain = region.cfg().domain_suffixes[0];
    write_auth_files(
        auth.path(),
        &json!([
            {
                "name": "a.info",
                "body": format!("{{\"auth\":{{\"accessToken\":\"token-a\",\"domain\":\"{domain}\",\"expiresAt\":4102416000000}},\"account\":{{\"uid\":\"u-a\",\"nickname\":\"A\"}}}}"),
                "mtime_unix_ms": 1_792_000_000_001u64
            },
            {
                "name": "b.info",
                "body": format!("{{\"auth\":{{\"accessToken\":\"token-b\",\"domain\":\"{domain}\",\"expiresAt\":4102416000000}},\"account\":{{\"uid\":\"u-b\",\"nickname\":\"B\"}}}}"),
                "mtime_unix_ms": 1_792_000_000_000u64
            }
        ]),
    );
    let auth_s = auth.path().to_string_lossy();
    let data_s = data.path().to_string_lossy();
    let home_s = home.path().to_string_lossy();
    env.set_locked(&[
        ("CODEBUDDY_AUTH_DIR", Some(auth_s.as_ref())),
        ("PROXYDECK_DATA_DIR", Some(data_s.as_ref())),
        ("CODEBUDDY_POOL_DIR", Some(data_s.as_ref())),
        ("USERPROFILE", Some(home_s.as_ref())),
        ("LOCALAPPDATA", Some("")),
        ("XDG_DATA_HOME", Some("")),
    ]);
    let pool = AccountPool::new(region).expect("两个合成账号应入池");
    assert_eq!(pool.size(), 2);
    configure_pool(&pool);
    let a_path = auth.path().join("a.info").to_string_lossy().to_string();
    let b_path = auth.path().join("b.info").to_string_lossy().to_string();
    let cred = Arc::new(
        StubCredential::from_json(&json!({
            "headers": {"Content-Type":"application/json","Authorization":"Bearer synthetic"},
            "headers_by_path": {
                a_path.clone(): {"Content-Type":"application/json","Authorization":"Bearer a","X-User-Id":"u-a"},
                b_path.clone(): {"Content-Type":"application/json","Authorization":"Bearer b","X-User-Id":"u-b"}
            },
            "headers_ok": true
        }))
        .unwrap(),
    );
    let mut state = rs_codebuddy::AppState::new(
        region,
        cred,
        upstream.base.clone(),
        data.path().to_path_buf(),
    );
    state.pool = Some(pool);
    configure_state(&mut state);
    Fixture {
        app: AppServer::start(Arc::new(state)).await,
        _auth: auth,
        data,
        _home: home,
    }
}

async fn chat(app: &AppServer, model: &str) -> common::Resp {
    let body = serde_json::to_vec(&json!({
        "model": model,
        "messages": [{"role":"user","content":"hello"}],
        "stream": false
    }))
    .unwrap();
    send(&app.base, "POST", "/v1/chat/completions", Some(&body)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_block_is_remembered_on_next_http_request_without_poisoning_other_models() {
    let env = EnvGuard::set(&[]);
    let upstream = ModelUpstream::start().await;
    let fx = fixture(&env, Region::Cn, &upstream, "a02-memory", |_| {}, |_| {}).await;

    let first = chat(&fx.app, MODEL_M).await;
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(
        upstream.hits(),
        vec![("u-a".into(), MODEL_M.into()), ("u-b".into(), MODEL_M.into())]
    );

    let second = chat(&fx.app, MODEL_M).await;
    assert_eq!(second.status, 200, "{}", second.text());
    assert_eq!(
        upstream.hits(),
        vec![
            ("u-a".into(), MODEL_M.into()),
            ("u-b".into(), MODEL_M.into()),
            ("u-b".into(), MODEL_M.into()),
        ],
        "第二次 M 不得再先打 A"
    );

    let other = chat(&fx.app, MODEL_N).await;
    assert_eq!(other.status, 200, "{}", other.text());
    assert_eq!(upstream.hits().last(), Some(&("u-a".into(), MODEL_N.into())));
    env.point_to_empty(fx.data.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_rewrite_selects_account_for_the_sent_model() {
    let env = EnvGuard::set(&[]);
    let upstream = ModelUpstream::start().await;
    let fx = fixture(
        &env,
        Region::Cn,
        &upstream,
        "a02-fallback",
        |pool| pool.mark_model_blocked("u-a", FALLBACK_M),
        |state| {
            let until = rs_codebuddy::gotime::now_millis() + 60_000;
            state.failover.hy4_trip(until);
        },
    )
    .await;

    let response = chat(&fx.app, HY4_PRIMARY).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        upstream.hits(),
        vec![("u-b".into(), FALLBACK_M.into())],
        "请求名是 hy4-preview，资格必须按最终 sent_model=fallback-model"
    );
    env.point_to_empty(fx.data.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_accounts_blocked_for_model_returns_model_unavailable_without_upstream_hit() {
    let env = EnvGuard::set(&[]);
    let upstream = ModelUpstream::start().await;
    let fx = fixture(
        &env,
        Region::Cn,
        &upstream,
        "a02-all-blocked",
        |pool| {
            pool.mark_model_blocked("u-a", MODEL_M);
            pool.mark_model_blocked("u-b", MODEL_M);
        },
        |_| {},
    )
    .await;

    let response = chat(&fx.app, MODEL_M).await;
    assert_eq!(response.status, 400, "{}", response.text());
    assert!(response.text().contains("model_unavailable"), "{}", response.text());
    assert!(upstream.hits().is_empty(), "全池无模型资格时不得再打上游");
    env.point_to_empty(fx.data.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cn_model_block_does_not_leak_into_intl_pool() {
    let env = EnvGuard::set(&[]);
    let upstream = ModelUpstream::start().await;
    let cn = fixture(
        &env,
        Region::Cn,
        &upstream,
        "a02-cn-isolation",
        |pool| pool.mark_model_blocked("u-a", MODEL_M),
        |_| {},
    )
    .await;
    let intl = fixture(&env, Region::Intl, &upstream, "a02-intl-isolation", |_| {}, |_| {}).await;

    let cn_response = chat(&cn.app, MODEL_M).await;
    assert_eq!(cn_response.status, 200, "{}", cn_response.text());
    assert_eq!(upstream.hits(), vec![("u-b".into(), MODEL_M.into())]);

    let intl_response = chat(&intl.app, MODEL_M).await;
    assert_eq!(intl_response.status, 200, "{}", intl_response.text());
    assert_eq!(
        upstream.hits(),
        vec![
            ("u-b".into(), MODEL_M.into()),
            ("u-a".into(), MODEL_M.into()),
            ("u-b".into(), MODEL_M.into()),
        ],
        "INTL 是同 uid 的独立池，不得继承 CN 的内存黑名单"
    );
    env.point_to_empty(intl.data.path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_trip_requalifies_current_account_before_fallback_replay() {
    let env = EnvGuard::set(&[]);
    let upstream = ModelUpstream::start_with("u-b", true).await;
    let fx = fixture(
        &env,
        Region::Cn,
        &upstream,
        "a02-trip-requalify",
        |pool| pool.mark_model_blocked("u-b", FALLBACK_M),
        |_| {},
    )
    .await;

    let response = chat(&fx.app, HY4_PRIMARY).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(
        upstream.hits(),
        vec![
            ("u-a".into(), HY4_PRIMARY.into()),
            ("u-b".into(), HY4_PRIMARY.into()),
            ("u-a".into(), FALLBACK_M.into()),
        ],
        "429 trip 改写 fallback 后，不得先用已知不支持 fallback 的当前号重放"
    );
    env.point_to_empty(fx.data.path());
}
