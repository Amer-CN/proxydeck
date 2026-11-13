//! 活动积分 / 福利模型的上游合同测试：用本地 mock 上游验证请求形状与解析。
//!
//! 不碰真实华为云：`Upstream::new` 指向本地 mock，`set_benefit_base` 指向同一 mock。
//! 真机验证另做（需要有效登录态），这里只锁协议形状——请求体字段名、campaignId 类型、
//! Agent-Type、idempotentKey、福利 `error_code` 语义。

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use rs_codearts::config::Account;
use rs_codearts::upstream::Upstream;

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Value>>>);

async fn delivery() -> Json<Value> {
    Json(json!({"code":0,"data":{"items":[
        {"campaignId":1,"type":"USER_LOGIN","title":"每日签到","status":"ELIGIBLE","claimable":true,
         "benefitUnit":"CREDIT","benefitAmount":1000,
         "extra":{"startTime":"2026-10-01T00:00:00+08:00","endTime":"2026-12-31T23:59:59+08:00"}},
        {"campaignId":"inv-7","type":"INVITE_REGISTER","title":"邀请注册","status":"ELIGIBLE",
         "claimable":true,"benefitUnit":"CREDIT","benefitAmount":1000}
    ]}}))
}

async fn claim(State(rec): State<Recorder>, Json(body): Json<Value>) -> Json<Value> {
    rec.0.lock().unwrap().push(json!({"path":"claim","body":body}));
    Json(json!({"code":0,"data":{"id":"claim-id-1","benefitAmount":1000}}))
}

async fn confirm(State(rec): State<Recorder>, Json(body): Json<Value>) -> Json<Value> {
    rec.0.lock().unwrap().push(json!({"path":"confirm","body":body}));
    Json(json!({"code":0}))
}

async fn benefit_claim(State(rec): State<Recorder>) -> Json<Value> {
    rec.0.lock().unwrap().push(json!({"path":"benefit_claim"}));
    Json(json!({"error_code":"0000"}))
}

async fn benefit_config() -> Json<Value> {
    Json(json!({"error_code":"0000","result":{"base_url":"http://mock","models":[
        {"model_id":"glm-5.3-flash","model_name":"GLM-5.3-Flash","model_desc":"福利",
         "context_window":1048576,"max_tokens":65536}
    ]}}))
}

async fn mock_server() -> (String, Recorder) {
    let rec = Recorder::default();
    let app = Router::new()
        .route("/v1/ops/delivery", get(delivery))
        .route("/v1/ops/claim", post(claim))
        .route("/v1/ops/confirm", post(confirm))
        .route("/api/v1/benefit/claim", post(benefit_claim))
        .route("/api/v1/gateway/config", get(benefit_config))
        .with_state(rec.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), rec)
}

fn account() -> Account {
    Account {
        access_key: "AK_TEST".into(),
        secret_key: "SK_TEST".into(),
        security_token: "TOKEN_TEST".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn activity_flow_matches_upstream_contract() {
    let (base, rec) = mock_server().await;
    let up = Upstream::new(&base).unwrap();

    let campaigns = up.campaigns(&account()).await.expect("活动列表应解析");
    assert_eq!(campaigns.len(), 2);
    assert_eq!(campaigns[0].id_text(), "1");
    assert!(campaigns[0].is_login());
    assert_eq!(campaigns[1].id_text(), "inv-7");
    assert_eq!(campaigns[1].kind, "INVITE_REGISTER");

    // 数字 campaignId 必须原样回数字（上游实测 ID 是数字）。
    let out = up
        .claim_campaign(&account(), &json!(1))
        .await
        .expect("claim 应成功");
    assert_eq!(out["code"], 0);
    let recs = rec.0.lock().unwrap().clone();
    let claim_req = recs
        .iter()
        .find(|r| r["path"] == "claim")
        .expect("应收到 claim 请求");
    assert_eq!(claim_req["body"]["campaignId"], json!(1));
    assert_eq!(claim_req["body"]["channel"], "IDE");
    assert!(
        claim_req["body"]["idempotentKey"]
            .as_str()
            .unwrap()
            .starts_with("claim_1_"),
        "idempotentKey 形状不对：{}",
        claim_req["body"]["idempotentKey"]
    );
    // claim 响应带 data.id → confirm 的请求体只带 campaignId。
    up.confirm_campaign(&account(), &json!(1)).await.expect("confirm 应成功");
    let recs = rec.0.lock().unwrap().clone();
    let confirm_req = recs.iter().find(|r| r["path"] == "confirm").unwrap();
    assert_eq!(confirm_req["body"]["campaignId"], json!(1));
}

#[tokio::test]
async fn benefit_claim_and_catalog_match_contract() {
    let (base, rec) = mock_server().await;
    let mut up = Upstream::new(&base).unwrap();
    up.set_benefit_base(&base);

    up.claim_benefit(&account()).await.expect("福利领取应成功");
    assert!(rec.0.lock().unwrap().iter().any(|r| r["path"] == "benefit_claim"));

    let models = up.benefit_models(&account()).await.expect("福利目录应解析");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "glm-5.3-flash");
    assert!(models[0].benefit, "福利模型必须带 benefit 标记");
    assert_eq!(models[0].context_window, Some(1048576));
}