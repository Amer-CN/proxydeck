//! A03：码道稳定账号 ID、旧身份别名与配置往返合同。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use rs_codearts::accounts::{AccountManager, AccountSource};
use rs_codearts::config::{Account, Config};
use serde_json::{json, Value};

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rs-codearts-{tag}-{stamp}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn account(ak: &str, name: &str) -> Account {
    Account {
        name: name.into(),
        enabled: true,
        access_key: ak.into(),
        secret_key: format!("sk-{ak}"),
        security_token: format!("tok-{ak}"),
        expires_at: Some("2099-01-01T00:00:00Z".into()),
        refresh_token: Some(format!("rt-{ak}")),
        code_verifier: Some("verifier".into()),
        client_id: Some("codearts".into()),
        ..Default::default()
    }
}

#[test]
fn no_domain_same_name_accounts_keep_distinct_stable_ids() {
    let dir = temp_dir("same-name");
    let path = dir.join("codearts-accounts.json");
    let manager = AccountManager::new(path.clone());
    let a = manager.upsert(account("AK-A", "same")).unwrap();
    let b = manager.upsert(account("AK-B", "same")).unwrap();
    assert_ne!(a.id, b.id, "缺 domainId 时不得按显示名合并");
    assert_eq!(manager.load().unwrap().accounts.len(), 2);
    assert_eq!(a.source, AccountSource::File);
    assert_eq!(manager.snapshot(&a.id).unwrap().credentials.access_key, "AK-A");
    assert_eq!(manager.snapshot(&b.id).unwrap().credentials.access_key, "AK-B");
}

#[test]
fn same_domain_different_names_stay_distinct_and_keep_own_credentials() {
    let dir = temp_dir("same-domain");
    let path = dir.join("codearts-accounts.json");
    let manager = AccountManager::new(path);
    let mut alice = account("AK-A", "alice");
    alice.domain_id = Some("tenant-1".into());
    let mut bob = account("AK-B", "bob");
    bob.domain_id = Some("tenant-1".into());
    let a = manager.upsert(alice).unwrap();
    let b = manager.upsert(bob).unwrap();
    assert_ne!(a.id, b.id, "同一 tenant 的不同用户不得互相覆盖");
    assert_eq!(manager.snapshot(&a.id).unwrap().credentials.access_key, "AK-A");
    assert_eq!(manager.snapshot(&b.id).unwrap().credentials.access_key, "AK-B");
    assert_eq!(manager.load().unwrap().accounts.len(), 2);
}

#[test]
fn ak_rotation_and_reload_preserve_id_aliases_and_other_accounts() {
    let dir = temp_dir("rotation");
    let path = dir.join("codearts-accounts.json");
    let manager = AccountManager::new(path.clone());
    let a = manager.upsert(account("AK-OLD-A", "before")).unwrap();
    let b = manager.upsert(account("AK-OLD-B", "B")).unwrap();
    let old_identity = a.credentials.identity();

    let mut replacement = account("AK-NEW-A", "after");
    replacement.domain_id = None;
    let updated = manager.replace_credentials(&a.id, replacement).unwrap();
    assert_eq!(updated.id, a.id);
    assert_eq!(updated.revision, a.revision + 1);
    assert!(updated.credentials.identity_aliases.contains(&old_identity));

    let reloaded = AccountManager::new(path).load().unwrap();
    let by_alias = AccountManager::resolve_in(&reloaded, &old_identity).expect("旧 identity 应解析稳定 ID");
    assert_eq!(by_alias, a.id);
    let snap = AccountManager::snapshot_in(&reloaded, &a.id).unwrap();
    assert_eq!(snap.credentials.access_key, "AK-NEW-A");
    assert_eq!(snap.credentials.name, "after");
    assert_eq!(snap.revision, updated.revision);
    assert_eq!(
        AccountManager::snapshot_in(&reloaded, &b.id).unwrap().credentials.access_key,
        "AK-OLD-B"
    );
}

#[test]
fn legacy_removed_string_only_matches_exactly_and_stable_removal_keeps_aliases() {
    let dir = temp_dir("removed");
    let path = dir.join("codearts-accounts.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "channel": "credit",
            "accounts": [],
            "removed": ["ak|LEGACY12|Legacy"]
        }))
        .unwrap(),
    )
    .unwrap();
    let manager = AccountManager::new(path.clone());
    let cfg = manager.load().unwrap();
    assert!(AccountManager::legacy_removed_in(&cfg, "ak|LEGACY12|Legacy"));
    assert!(!AccountManager::legacy_removed_in(&cfg, "ak|ROTATED|Legacy"));
    assert!(AccountManager::resolve_in(&cfg, "ak|LEGACY12|Legacy").is_none());

    let a = manager.upsert(account("AK-STABLE", "Stable")).unwrap();
    let alias = a.credentials.identity();
    manager.remove(&a.id).unwrap();
    let cfg = manager.load().unwrap();
    assert!(cfg.accounts.is_empty());
    assert!(cfg.removed.iter().any(|x| x == &format!("id:{}", a.id.as_str())));
    assert!(cfg.removed.iter().any(|x| x == &alias));
    assert_eq!(AccountManager::resolve_in(&cfg, &alias), Some(a.id.clone()));
}

#[test]
fn concurrent_first_loads_allocate_one_id_per_account() {
    let dir = temp_dir("concurrent-ids");
    let path = dir.join("codearts-accounts.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "channel": "credit",
            "accounts": [account("AK-ONE", "One"), account("AK-TWO", "Two")],
            "removed": []
        }))
        .unwrap(),
    )
    .unwrap();
    let path = Arc::new(path);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            AccountManager::new((*path).clone()).load().unwrap()
        }));
    }
    let mut ids = Vec::new();
    for h in handles {
        let cfg = h.join().unwrap();
        assert_eq!(cfg.accounts.len(), 2);
        ids.extend(cfg.accounts.iter().map(|a| a.account_id.clone().unwrap()));
    }
    let unique: std::collections::BTreeSet<_> = ids.into_iter().collect();
    assert_eq!(unique.len(), 2, "并发首次分配不得给同一账号不同 ID");
}

#[tokio::test]
async fn http_account_management_resolves_old_alias_and_stable_id_after_rotation() {
    let dir = temp_dir("http-management");
    let path = dir.join("codearts-accounts.json");
    let manager = AccountManager::new(path.clone());
    let before = manager.upsert(account("AK-HTTP-OLD", "HTTP")).unwrap();
    let old_identity = before.credentials.identity();
    let server = rs_codearts::Server::new(
        rs_codearts::upstream::Upstream::new("http://127.0.0.1:1").unwrap(),
        path,
        false,
        "http://127.0.0.1:1".into(),
        false,
        3,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, rs_codearts::router(server)).await.unwrap();
    });

    manager
        .replace_credentials(&before.id, account("AK-HTTP-NEW", "HTTP renamed"))
        .unwrap();
    let client = reqwest::Client::new();
    let accounts: Value = client
        .get(format!("{base}/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let row = &accounts["accounts"][0];
    assert_eq!(row["accountId"], before.id.as_str());
    assert_eq!(row["identity"], "ak|HTTP-NEW|HTTP renamed");
    assert_ne!(row["identity"], old_identity);

    let enabled: Value = client
        .post(format!("{base}/accounts/enable"))
        .json(&json!({"identity": old_identity, "enabled": false}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(enabled["enabled"], false);
    assert_eq!(enabled["accountId"], before.id.as_str());

    let removed: Value = client
        .post(format!("{base}/accounts/remove"))
        .json(&json!({"identity": before.id.as_str()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(removed["removed"], true);
    let accounts: Value = client
        .get(format!("{base}/accounts"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accounts["accounts"], json!([]));
    task.abort();
}

#[test]
fn known_fields_and_new_metadata_round_trip_and_save_errors_are_visible() {
    let dir = temp_dir("roundtrip");
    let path = dir.join("codearts-accounts.json");
    let manager = AccountManager::new(path.clone());
    let mut a = account("AK-ROUND", "Round");
    a.domain_id = Some("domain-1".into());
    a.plugin_name = Some("plugin".into());
    a.plugin_version = Some("1.2.3".into());
    a.ide_name = Some("ide".into());
    a.ide_version = Some("4.5.6".into());
    a.dpop_private_jwk = Some(json!({"kty":"EC","crv":"P-256"}));
    let snap = manager.upsert(a).unwrap();
    let cfg = Config::load(&path).unwrap();
    let stored = cfg.accounts.iter().find(|a| a.account_id.as_deref() == Some(snap.id.as_str())).unwrap();
    assert_eq!(stored.domain_id.as_deref(), Some("domain-1"));
    assert_eq!(stored.plugin_name.as_deref(), Some("plugin"));
    assert_eq!(stored.dpop_private_jwk, Some(json!({"kty":"EC","crv":"P-256"})));
    assert!(!stored.identity_aliases.is_empty());
    assert_eq!(stored.revision, snap.revision);

    let blocker = dir.join("not-a-directory");
    std::fs::write(&blocker, b"x").unwrap();
    let bad = AccountManager::new(blocker.join("accounts.json"));
    assert!(bad.load().is_err(), "落盘路径失败必须返回错误，不能假成功");
}

fn refreshable_account(ak: &str) -> Account {
    use base64::Engine;
    let d = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8; 32]);
    Account {
        expires_at: Some("2000-01-01T00:00:00Z".into()),
        refresh_token: Some("refresh-old".into()),
        code_verifier: Some("verifier".into()),
        client_id: Some("codearts".into()),
        dpop_private_jwk: Some(json!({"kty":"EC","crv":"P-256","d":d})),
        ..account(ak, "Refresh")
    }
}

struct StubSts {
    base: String,
    hits: Arc<AtomicUsize>,
    mode: Arc<AtomicU8>,
    task: tokio::task::JoinHandle<()>,
}

impl StubSts {
    async fn start(mode: u8) -> Self {
        use axum::routing::post;
        use axum::Router;
        let hits = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(AtomicU8::new(mode));
        let app = Router::new()
            .route("/sts", post(sts_response))
            .with_state((hits.clone(), mode.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/sts", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, hits, mode, task }
    }
}

impl Drop for StubSts {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn sts_response(
    State((hits, mode)): State<(Arc<AtomicUsize>, Arc<AtomicU8>)>,
    body: String,
) -> (axum::http::StatusCode, axum::Json<Value>) {
    hits.fetch_add(1, Ordering::SeqCst);
    assert!(body.contains("grant_type=refresh_token"));
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    match mode.load(Ordering::SeqCst) {
        1 => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":{"message":"temporary"}})),
        ),
        2 => (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":"invalid_grant"})),
        ),
        _ => (
            axum::http::StatusCode::OK,
            axum::Json(json!({
                "credentials": {
                    "access_key_id":"AK-REFRESHED",
                    "secret_access_key":"SK-REFRESHED",
                    "security_token":"TOK-REFRESHED",
                    "expiration":"2099-01-01T00:00:00Z"
                },
                "refresh_token":"refresh-new"
            })),
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_entries_share_one_sts_rotation_and_latest_credentials() {
    use rs_codearts::accounts::RefreshMode;
    let dir = temp_dir("refresh-concurrent");
    let path = dir.join("codearts-accounts.json");
    let seed = AccountManager::new(path.clone()).upsert(refreshable_account("AK-OLD")).unwrap();
    let sts = StubSts::start(0).await;
    let manager = Arc::new(AccountManager::with_refresh(
        path,
        reqwest::Client::new(),
        sts.base.clone(),
    ));
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let manager = manager.clone();
        let id = seed.id.clone();
        tasks.push(tokio::spawn(async move {
            manager.ensure_fresh(&id, seed.revision, RefreshMode::IfExpired).await
        }));
    }
    let mut snapshots = Vec::new();
    for task in tasks {
        snapshots.push(task.await.unwrap().unwrap());
    }
    assert_eq!(sts.hits.load(Ordering::SeqCst), 1);
    for snapshot in snapshots {
        assert_eq!(snapshot.id, seed.id);
        assert_eq!(snapshot.credentials.access_key, "AK-REFRESHED");
        assert_eq!(snapshot.credentials.refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(snapshot.revision, seed.revision + 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_share_one_sts_rotation() {
    let dir = temp_dir("refresh-processes");
    let path = dir.join("codearts-accounts.json");
    let seed = AccountManager::new(path.clone()).upsert(refreshable_account("AK-OLD")).unwrap();
    let sts = StubSts::start(0).await;
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            std::process::Command::new(&exe)
                .args(["--exact", "cross_process_refresh_child", "--nocapture"])
                .env("CODEARTS_REFRESH_CHILD", "1")
                .env("CODEARTS_REFRESH_ACCOUNTS", &path)
                .env("CODEARTS_REFRESH_STS", &sts.base)
                .env("CODEARTS_REFRESH_ID", seed.id.as_str())
                .env("CODEARTS_REFRESH_REVISION", seed.revision.to_string())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "子进程失败: {}", String::from_utf8_lossy(&output.stderr));
    }
    assert_eq!(sts.hits.load(Ordering::SeqCst), 1);
    let snapshot = AccountManager::new(path).snapshot(&seed.id).unwrap();
    assert_eq!(snapshot.credentials.access_key, "AK-REFRESHED");
    assert_eq!(snapshot.revision, seed.revision + 1);
}

#[tokio::test]
async fn cross_process_refresh_child() {
    use rs_codearts::accounts::RefreshMode;
    if std::env::var_os("CODEARTS_REFRESH_CHILD").is_none() {
        return;
    }
    let path = std::env::var_os("CODEARTS_REFRESH_ACCOUNTS").unwrap().into();
    let sts = std::env::var("CODEARTS_REFRESH_STS").unwrap();
    let id = rs_codearts::accounts::AccountId(std::env::var("CODEARTS_REFRESH_ID").unwrap());
    let revision = std::env::var("CODEARTS_REFRESH_REVISION").unwrap().parse().unwrap();
    let manager = AccountManager::with_refresh(path, reqwest::Client::new(), sts);
    let snapshot = manager.ensure_fresh(&id, revision, RefreshMode::Force).await.unwrap();
    assert_eq!(snapshot.credentials.access_key, "AK-REFRESHED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_errors_distinguish_transient_definitive_and_success() {
    use rs_codearts::accounts::{RefreshError, RefreshMode};
    let dir = temp_dir("refresh-errors");
    let path = dir.join("codearts-accounts.json");
    let seed = AccountManager::new(path.clone()).upsert(refreshable_account("AK-OLD")).unwrap();
    let sts = StubSts::start(1).await;
    let manager = AccountManager::with_refresh(path.clone(), reqwest::Client::new(), sts.base.clone());
    let error = manager.ensure_fresh(&seed.id, seed.revision, RefreshMode::IfExpired).await.unwrap_err();
    assert!(matches!(error, RefreshError::Transient(_)));
    assert_eq!(AccountManager::new(path.clone()).snapshot(&seed.id).unwrap().revision, seed.revision);

    sts.mode.store(2, Ordering::SeqCst);
    let error = manager.ensure_fresh(&seed.id, seed.revision, RefreshMode::Force).await.unwrap_err();
    assert!(matches!(error, RefreshError::Definitive { revision, .. } if revision == seed.revision));

    sts.mode.store(0, Ordering::SeqCst);
    let snapshot = manager.ensure_fresh(&seed.id, seed.revision, RefreshMode::Force).await.unwrap();
    assert_eq!(snapshot.revision, seed.revision + 1);
    assert_eq!(snapshot.credentials.access_key, "AK-REFRESHED");
}

struct StubUpstream {
    base: String,
    task: tokio::task::JoinHandle<()>,
}

impl StubUpstream {
    async fn start() -> Self {
        use axum::routing::{get, post};
        use axum::{Json, Router};
        let app = Router::new()
            .route("/snap-manager/v1/statistics/plugin", get(|| async {
                Json(json!({"metrics": []}))
            }))
            .route("/api/v2/chat/completions", post(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                )
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, task }
    }
}

impl Drop for StubUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_server_chat_quota_and_background_share_a_single_refresh() {
    let dir = temp_dir("server-refresh");
    let path = dir.join("codearts-accounts.json");
    let seed = AccountManager::new(path.clone()).upsert(refreshable_account("AK-OLD")).unwrap();
    let sts = StubSts::start(0).await;
    let upstream = StubUpstream::start().await;
    let server = rs_codearts::Server::new(
            rs_codearts::upstream::Upstream::new(&upstream.base).unwrap(),
            path.clone(),
            false,
            sts.base.clone(),
            false,
            3,
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn({
        let server = server.clone();
        async move { axum::serve(listener, rs_codearts::router(server)).await.unwrap() }
    });
    let client = reqwest::Client::new();
    let quota = {
        let client = client.clone();
        let base = base.clone();
        tokio::spawn(async move { client.get(format!("{base}/v1/quota")).send().await.unwrap() })
    };
    let chat = {
        let client = client.clone();
        let base = base.clone();
        tokio::spawn(async move {
            client
                .post(format!("{base}/v1/chat/completions"))
                .json(&json!({"model":"glm-5.3","messages":[{"role":"user","content":"hi"}],"stream":false}))
                .send()
                .await
                .unwrap()
        })
    };
    let background = {
        let server = server.clone();
        tokio::spawn(async move { server.refresh_due_accounts().await })
    };
    let quota = quota.await.unwrap();
    assert_eq!(quota.status(), 200, "quota body: {}", quota.text().await.unwrap());
    let chat = chat.await.unwrap();
    assert_eq!(chat.status(), 200, "chat body: {}", chat.text().await.unwrap());
    background.await.unwrap();
    assert_eq!(sts.hits.load(Ordering::SeqCst), 1);
    assert_eq!(AccountManager::new(path).snapshot(&seed.id).unwrap().revision, seed.revision + 1);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_health_is_bound_to_credential_revision_and_recovers_on_new_success() {
    use rs_codearts::accounts::RefreshMode;
    let dir = temp_dir("server-health");
    let path = dir.join("codearts-accounts.json");
    let seed = AccountManager::new(path.clone()).upsert(refreshable_account("AK-OLD")).unwrap();
    let sts = StubSts::start(1).await;
    let upstream = StubUpstream::start().await;
    let server = rs_codearts::Server::new(
            rs_codearts::upstream::Upstream::new(&upstream.base).unwrap(),
            path.clone(),
            false,
            sts.base.clone(),
            false,
            3,
        )
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn({
        let server = server.clone();
        async move { axum::serve(listener, rs_codearts::router(server)).await.unwrap() }
    });
    let client = reqwest::Client::new();
    let identity = seed.credentials.identity();
    let transient = client
        .post(format!("{base}/accounts/refresh"))
        .json(&json!({"identity": identity}))
        .send()
        .await
        .unwrap();
    assert_eq!(transient.status(), 502);
    let accounts: Value = client.get(format!("{base}/accounts")).send().await.unwrap().json().await.unwrap();
    assert_eq!(accounts["accounts"][0]["dead"], false, "瞬时失败不得判死");

    sts.mode.store(2, Ordering::SeqCst);
    let definitive = client
        .post(format!("{base}/accounts/refresh"))
        .json(&json!({"identity": identity}))
        .send()
        .await
        .unwrap();
    assert_eq!(definitive.status(), 502);
    let accounts: Value = client.get(format!("{base}/accounts")).send().await.unwrap().json().await.unwrap();
    assert_eq!(accounts["accounts"][0]["dead"], true, "确定失效绑定当前凭据版本");

    sts.mode.store(0, Ordering::SeqCst);
    let manager = AccountManager::with_refresh(path, reqwest::Client::new(), sts.base.clone());
    manager
        .ensure_fresh(&seed.id, seed.revision, RefreshMode::Force)
        .await
        .unwrap();
    let accounts: Value = client.get(format!("{base}/accounts")).send().await.unwrap().json().await.unwrap();
    assert_eq!(accounts["accounts"][0]["dead"], false, "新凭据版本不得被旧失败标死");
    assert_eq!(client.get(format!("{base}/v1/quota")).send().await.unwrap().status(), 200);
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_persistence_failure_is_returned_not_reported_as_success() {
    use rs_codearts::accounts::{RefreshError, RefreshMode};
    let dir = temp_dir("refresh-storage-error");
    let path = dir.join("codearts-accounts.json");
    let seed = AccountManager::new(path.clone()).upsert(refreshable_account("AK-OLD")).unwrap();
    std::fs::create_dir(dir.join("codearts-refreshed.json")).unwrap();
    let sts = StubSts::start(0).await;
    let manager = AccountManager::with_refresh(path, reqwest::Client::new(), sts.base.clone());
    let error = manager.ensure_fresh(&seed.id, seed.revision, RefreshMode::Force).await.unwrap_err();
    assert!(matches!(error, RefreshError::Storage(_)));
}
