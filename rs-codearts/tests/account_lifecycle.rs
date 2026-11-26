//! A03：码道稳定账号 ID、旧身份别名与配置往返合同。

use std::path::PathBuf;
use std::sync::Arc;

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
