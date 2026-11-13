//! 码道 OAuth2 登录（PKCE + DPoP），不依赖码道桌面端。
//!
//! 逐字对齐社区参考实现（HITZY2002/codearts2api `internal/upstream/client.go`
//! 的 `BuildAuthorizeURL` / `ExchangeCode`，以及官方 `huaweicloud.authentication`
//! 扩展的登录链路）：
//!
//! ```text
//! 1. 本地 127.0.0.1:<port> 起回调服务（/oauth/callback）
//! 2. GET https://codearts.huaweicloud.com/portal/authorize
//!      ?theme=2&locale=zh-cn&uri_scheme=codearts-agent&client_id=codearts-agent
//!      &port=<port>&code_challenge=<PKCE>&code_challenge_method=SHA-256
//!      &ticket_id=<hex>&plugin-name=snap_AIIDE&plugin-version=5.2.0
//! 3. portal 回调两次：先 secret+redirect（307 跳回），再 code
//! 4. POST https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens
//!      client_id / code / code_verifier / grant_type=authorization_code / redirect_uri
//!      + DPoP: <ES256 JWS>（私钥必须与 refresh_token 一起保存）
//! 5. 响应 {credentials:{access_key_id, secret_access_key, security_token, expiration},
//!          refresh_token, user_id, user_name, domain_id} → 存账号
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::any;
use axum::Router;
use base64::Engine;
use serde_json::{json, Value};

use crate::config::{now_unix, Account};
use crate::refresh;

pub const CLIENT_ID: &str = "codearts-agent";
pub const PORTAL_HOST: &str = "https://codearts.huaweicloud.com/portal";
pub const STS_URL: &str = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens";
pub const SNAP_MANAGER: &str = "https://snap-access.cn-north-4.myhuaweicloud.com/snap-manager";
pub const CALLBACK_PATH: &str = "/oauth/callback";
pub const PLUGIN_NAME: &str = "snap_AIIDE";
pub const PLUGIN_VERSION: &str = "5.2.0";
pub const LOGIN_TTL: Duration = Duration::from_secs(600);

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn form_enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// PKCE：verifier = base64url(64 随机字节)，challenge = base64url(sha256(verifier))。
pub fn pkce() -> (String, String) {
    use sha2::{Digest, Sha256};
    let verifier = b64url(&random_bytes(64));
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// 32/64 字节随机（`RandomState` 由 OS 熵播种，再用 HMAC-SHA256 展开）。
fn random_bytes(n: usize) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use std::hash::{BuildHasher, Hasher};
    let mut seed = [0u8; 32];
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0));
    hasher.write_u32(std::process::id());
    seed[..8].copy_from_slice(&hasher.finish().to_le_bytes());
    let mut h2 = std::collections::hash_map::RandomState::new().build_hasher();
    h2.write_u64(hasher.finish());
    h2.write_u64(n as u64);
    seed[8..16].copy_from_slice(&h2.finish().to_le_bytes());
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&seed).expect("hmac key");
    mac.update(b"codearts-login-jwk");
    let mut out = mac.finalize().into_bytes().to_vec();
    let mut counter = 1u64;
    while out.len() < n {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&seed).expect("hmac key");
        mac.update(&counter.to_le_bytes());
        mac.update(b"codearts-login-jwk");
        out.extend_from_slice(&mac.finalize().into_bytes());
        counter += 1;
    }
    out.truncate(n);
    out
}

/// 生成 P-256 DPoP 私钥 JWK（含 d；x/y 由私钥导出）。
pub fn new_dpop_jwk() -> Result<Value, String> {
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::SecretKey;
    let mut raw = random_bytes(32);
    // 极低概率落在非法区间：重试到合法为止（32 字节随机 ≈ 必然一次成功）。
    for _ in 0..16 {
        if let Ok(sk) = SecretKey::from_slice(&raw) {
            let pk = sk.public_key().to_encoded_point(false);
            let x = pk.x().ok_or("公钥 x 缺失")?;
            let y = pk.y().ok_or("公钥 y 缺失")?;
            let d = sk.to_bytes();
            return Ok(json!({
                "kty": "EC",
                "crv": "P-256",
                "x": b64url(x),
                "y": b64url(y),
                "d": b64url(&d),
            }));
        }
        raw = random_bytes(32);
    }
    Err("生成 P-256 私钥失败".to_string())
}

/// portal 授权链接。
pub fn authorize_url(ticket_id: &str, challenge: &str, port: u16) -> String {
    format!(
        "{PORTAL_HOST}/authorize?theme=2&locale=zh-cn&uri_scheme={CLIENT_ID}&client_id={CLIENT_ID}\
         &port={port}&code_challenge={}&code_challenge_method=SHA-256&ticket_id={}\
         &plugin-name={PLUGIN_NAME}&plugin-version={PLUGIN_VERSION}",
        form_enc(challenge),
        form_enc(ticket_id)
    )
}

/// 登录成功后拿到的全部凭据。
#[derive(Clone, Debug, Default)]
pub struct Token {
    pub user_id: String,
    pub user_name: String,
    pub domain_id: String,
    pub access_key: String,
    pub secret_key: String,
    pub security_token: String,
    pub expires_at: String,
    pub refresh_token: String,
}

impl Token {
    pub fn into_account(self, verifier: &str, jwk: &Value) -> Account {
        let name = if self.user_name.trim().is_empty() {
            self.user_id.clone()
        } else {
            format!("{} ({})", self.user_name.trim(), self.user_id.trim())
        };
        Account {
            name,
            enabled: true,
            access_key: self.access_key,
            secret_key: self.secret_key,
            security_token: self.security_token,
            expires_at: if self.expires_at.is_empty() {
                None
            } else {
                Some(self.expires_at)
            },
            refresh_token: Some(self.refresh_token),
            code_verifier: Some(verifier.to_string()),
            dpop_private_jwk: Some(jwk.clone()),
            domain_id: if self.domain_id.is_empty() {
                None
            } else {
                Some(self.domain_id)
            },
            client_id: Some(CLIENT_ID.to_string()),
            ..Default::default()
        }
    }
}

/// authorization_code 换 token。
pub async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
    port: u16,
    jwk: &Value,
) -> Result<Token, String> {
    let signer = Account {
        dpop_private_jwk: Some(jwk.clone()),
        ..Default::default()
    };
    let proof = refresh::build_dpop_jwt(&signer, "POST", STS_URL)?;
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let form = format!(
        "client_id={}&code={}&code_verifier={}&grant_type=authorization_code&redirect_uri={}",
        CLIENT_ID,
        form_enc(code),
        form_enc(verifier),
        form_enc(&redirect)
    );
    let resp = client
        .post(STS_URL)
        .header("dpop", proof)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await
        .map_err(|e| format!("换 token 请求失败: {e}"))?;
    let status = resp.status().as_u16();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读换 token 响应失败: {e}"))?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("换 token 响应不是 JSON: {e}; body={}", trunc(&bytes, 300)))?;
    if status >= 400 {
        let code = v.get("error").and_then(|x| x.as_str()).unwrap_or("http_error");
        let msg = v
            .get("error_description")
            .or_else(|| v.get("error_msg"))
            .and_then(|x| x.as_str())
            .unwrap_or("换 token 失败");
        return Err(format!("换 token 失败 {status} {code}: {msg}"));
    }
    parse_token(&v)
}

fn parse_token(v: &Value) -> Result<Token, String> {
    let cred = v
        .get("credentials")
        .ok_or("换 token 响应缺少 credentials")?;
    let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut t = Token {
        user_id: s(v, "user_id"),
        user_name: s(v, "user_name"),
        domain_id: s(v, "domain_id"),
        access_key: s(cred, "access_key_id"),
        secret_key: s(cred, "secret_access_key"),
        security_token: s(cred, "security_token"),
        expires_at: s(cred, "expiration"),
        refresh_token: s(v, "refresh_token"),
    };
    if t.user_id.is_empty() {
        // 部分部署把用户信息放在 user 子对象里。
        if let Some(u) = v.get("user") {
            t.user_id = s(u, "id");
            t.user_name = s(u, "name");
            t.domain_id = s(u, "domain_id");
        }
    }
    if t.access_key.is_empty() || t.security_token.is_empty() {
        return Err("换 token 响应缺少临时 AK/SK/SecurityToken".to_string());
    }
    if t.refresh_token.is_empty() {
        return Err("换 token 响应缺少 refresh_token（无法自动续期）".to_string());
    }
    Ok(t)
}

fn trunc(bytes: &[u8], n: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.chars().take(n).collect()
}

/// 一次未完成的登录。
#[derive(Clone, Debug)]
pub struct Pending {
    pub url: String,
    pub port: u16,
    pub verifier: String,
    pub ticket_id: String,
    pub jwk: Value,
    pub created: Instant,
    pub code: Option<String>,
    pub secret: Option<String>,
    pub done: bool,
    pub error: Option<String>,
    pub account: Option<String>,
}

impl Pending {
    pub fn status_json(&self, id: &str) -> Value {
        json!({
            "id": id,
            "url": self.url,
            "port": self.port,
            "done": self.done,
            "error": self.error,
            "account": self.account,
            "age_secs": self.created.elapsed().as_secs(),
        })
    }
}

/// 回调服务（一次登录一个监听；收到 code 后由轮询任务完成兑换）。
pub async fn serve_callback(listener: tokio::net::TcpListener, st: Arc<crate::Server>, id: String) {
    #[derive(Clone)]
    struct CbState {
        st: Arc<crate::Server>,
        id: String,
    }
    #[derive(serde::Deserialize)]
    struct Q {
        code: Option<String>,
        secret: Option<String>,
        redirect: Option<String>,
    }
    async fn cb(State(s): State<CbState>, Query(q): Query<Q>) -> Response {
        if let Some(secret) = q.secret {
            s.st.set_login_secret(&s.id, &secret);
        }
        if q.code.is_none() {
            if let Some(r) = q.redirect {
                if !r.is_empty() {
                    return Redirect::temporary(&r).into_response();
                }
            }
        }
        match q.code {
            Some(code) if !code.is_empty() => {
                s.st.set_login_code(&s.id, &code);
                Html("<h3>登录成功，可关闭此页面。</h3>").into_response()
            }
            _ => (
                axum::http::StatusCode::BAD_REQUEST,
                Html("<h3>登录失败：缺少 code</h3>"),
            )
                .into_response(),
        }
    }
    let app = Router::new()
        .route(CALLBACK_PATH, any(cb))
        .with_state(CbState { st, id });
    // 回调监听最多活到登录 TTL 之后一分钟，别让废弃的登录长期占端口。
    let _ = tokio::time::timeout(LOGIN_TTL + Duration::from_secs(60), axum::serve(listener, app)).await;
}

/// 轮询登录结果（回调 or 手动提交 code）；成功后换 token 并落账号。
pub async fn finish_worker(st: Arc<crate::Server>, id: String) {
    let start = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if start.elapsed() > LOGIN_TTL {
            st.set_login_error(&id, "登录超时（10 分钟）");
            return;
        }
        let (code, verifier, port, jwk) = {
            match st.login_view(&id) {
                Some((code, verifier, port, jwk, done)) => {
                    if done || code.is_none() {
                        if done {
                            return;
                        }
                        continue;
                    }
                    (code.unwrap(), verifier, port, jwk)
                }
                None => return,
            }
        };
        match exchange_code(st.up.client(), &code, &verifier, port, &jwk).await {
            Ok(tok) => {
                let account = tok.into_account(&verifier, &jwk);
                let name = account.display_name();
                match st.save_imported(vec![account]) {
                    Ok(_) => st.set_login_success(&id, &name),
                    Err(e) => st.set_login_error(&id, &format!("保存账号失败: {e}")),
                }
                return;
            }
            Err(e) => {
                st.set_login_error(&id, &e);
                return;
            }
        }
    }
}

/// 便于测试：状态结构里的时间基准（epoch 秒）。
pub fn now_secs() -> i64 {
    now_unix()
}

/// 生成登录时会话的公开视图（供 `/login/start` 返回）。
pub fn start_json(id: &str, url: &str, port: u16, challenge: &str, ticket: &str) -> Value {
    json!({
        "id": id,
        "url": url,
        "port": port,
        "callback": format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
        "code_challenge": challenge,
        "ticket_id": ticket,
    })
}

/// 兼容旧接口：把 `HashMap` 里的待登录项转成公开 JSON。
pub fn status_map(pending: HashMap<String, Pending>) -> Value {
    json!({
        "pending": pending.iter().map(|(k, v)| (k.clone(), v.status_json(k))).collect::<serde_json::Map<_, _>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_sha256() {
        use sha2::{Digest, Sha256};
        let (v, c) = pkce();
        assert!(v.len() >= 43 && v.len() <= 128);
        let expect = b64url(&Sha256::digest(v.as_bytes()));
        assert_eq!(c, expect);
        assert!(v.chars().all(|x| x.is_ascii_alphanumeric() || x == '-' || x == '_'));
    }

    #[test]
    fn dpop_jwk_has_full_key_material() {
        let j = new_dpop_jwk().unwrap();
        assert_eq!(j["kty"], "EC");
        assert_eq!(j["crv"], "P-256");
        for k in ["x", "y", "d"] {
            assert!(j[k].as_str().map(|s| !s.is_empty()).unwrap_or(false), "{k} 缺失");
        }
        // 生成的私钥能签出可验证的 DPoP JWT
        let acct = Account {
            dpop_private_jwk: Some(j),
            ..Default::default()
        };
        let jwt = refresh::build_dpop_jwt(&acct, "POST", STS_URL).unwrap();
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[test]
    fn authorize_url_has_required_params() {
        let u = authorize_url("abc123", "chal-xyz", 51234);
        assert!(u.starts_with("https://codearts.huaweicloud.com/portal/authorize?"));
        for p in [
            "theme=2",
            "locale=zh-cn",
            "client_id=codearts-agent",
            "uri_scheme=codearts-agent",
            "port=51234",
            "code_challenge=chal-xyz",
            "code_challenge_method=SHA-256",
            "ticket_id=abc123",
            "plugin-name=snap_AIIDE",
            "plugin-version=5.2.0",
        ] {
            assert!(u.contains(p), "缺少 {p}: {u}");
        }
    }

    #[test]
    fn parses_token_response_and_builds_account() {
        let v = json!({
            "user_id": "u1", "user_name": "张三", "domain_id": "d1",
            "credentials": {
                "access_key_id": "AK", "secret_access_key": "SK",
                "security_token": "TOK", "expiration": "2026-11-14T00:00:00Z"
            },
            "refresh_token": "RT"
        });
        let t = parse_token(&v).unwrap();
        assert_eq!(t.access_key, "AK");
        let acct = t.into_account("ver", &json!({"kty":"EC"}));
        assert_eq!(acct.access_key, "AK");
        assert_eq!(acct.client_id.as_deref(), Some("codearts-agent"));
        assert_eq!(acct.code_verifier.as_deref(), Some("ver"));
        assert!(acct.name.contains("张三"));
    }

    #[test]
    fn rejects_token_without_credentials() {
        assert!(parse_token(&json!({"refresh_token":"x"})).is_err());
        assert!(parse_token(&json!({"credentials":{"access_key_id":"a"}})).is_err());
    }
}