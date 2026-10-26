//! DPoP 自动续期（ES256 JWS + refresh_token）。
//!
//! 逐字对齐码道 Electron 版逆向结果（`resources/app.asar` 2026-10 版本）：
//!
//! ```text
//! POST https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens
//! Content-Type: application/x-www-form-urlencoded
//! DPoP: <ES256 JWS>
//!
//! client_id=<session.clientId>      // 这版是 codearts
//! code_verifier=<loginContext.pkcePair.codeVerifier>
//! grant_type=refresh_token
//! refresh_token=<session.refreshToken>
//! ```
//!
//! DPoP JWT：
//! * header  = {"alg":"ES256","typ":"dpop+jwt","jwk": <publicKey JWK>}
//! * payload = {"htm":"POST","htu":<上面的 URL 全文>,"iat":<秒>,"jti":<32 hex>}
//! * 签名    = P-256 ECDSA / SHA-256，base64url(r||s)
//!
//! 响应：
//! `{"credentials":{"access_key_id","secret_access_key","security_token","expiration"},
//!   "refresh_token"}`。

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{json, Value};

use crate::config::Account;
use crate::upstream::random_hex;

/// 默认 STS 令牌端点。
pub const DEFAULT_STS_URL: &str = "https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens";

/// 续期后的凭据。
#[derive(Clone, Debug)]
pub struct Refreshed {
    pub access_key: String,
    pub secret_key: String,
    pub security_token: String,
    pub expires_at: String,
    pub refresh_token: String,
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| format!("base64url 解码失败: {e}"))
}

/// P-256 私钥（JWK 的 d）与公钥坐标（x/y）。
struct EcKey {
    signing: SigningKey,
    x: String,
    y: String,
}

/// 从账号里的 `dpop_private_jwk` 解析密钥对。
///
/// 兼容两种写法：
/// 1. `loginContext.dpopKeyPair` 原样（`{"privateKey":"<JWK 字符串>","publicKey":"<JWK 字符串>"}`）；
/// 2. 直接给私钥 JWK（含 `kty/crv/x/y/d`）。
fn parse_key(account: &Account) -> Result<EcKey, String> {
    let v = account
        .dpop_private_jwk
        .as_ref()
        .ok_or("账号没有 DPoP 私钥（loginContext.dpopKeyPair），无法续期")?;
    let (private_jwk, public_jwk) = if let Some(p) = v.get("privateKey").and_then(|x| x.as_str()) {
        let priv_v: Value = serde_json::from_str(p)
            .map_err(|e| format!("DPoP privateKey 不是 JSON: {e}"))?;
        let pub_v = v
            .get("publicKey")
            .and_then(|x| x.as_str())
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        (priv_v, pub_v)
    } else {
        (v.clone(), None)
    };
    let crv = private_jwk.get("crv").and_then(|x| x.as_str()).unwrap_or("P-256");
    if crv != "P-256" {
        return Err(format!("只支持 P-256，账号是 {crv}"));
    }
    let d = private_jwk
        .get("d")
        .and_then(|x| x.as_str())
        .ok_or("DPoP 私钥缺少 d")?;
    let d_bytes = b64url_decode(d)?;
    let signing = SigningKey::from_slice(&d_bytes)
        .map_err(|e| format!("P-256 私钥非法: {e}"))?;
    // 公钥坐标：优先用存储的 jwk；缺失就从私钥推。
    let (x, y) = if let Some(pubv) = public_jwk {
        (
            pubv.get("x").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
            pubv.get("y").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        )
    } else if let (Some(x), Some(y)) = (
        private_jwk.get("x").and_then(|x| x.as_str()),
        private_jwk.get("y").and_then(|x| x.as_str()),
    ) {
        (x.to_string(), y.to_string())
    } else {
        let point = signing.verifying_key().to_encoded_point(false);
        (
            b64url(point.x().ok_or("公钥缺少 x")?),
            b64url(point.y().ok_or("公钥缺少 y")?),
        )
    };
    if x.is_empty() || y.is_empty() {
        return Err("DPoP 公钥坐标缺失".to_string());
    }
    Ok(EcKey { signing, x, y })
}

/// 生成 DPoP JWS。`htu` 必须与实际请求 URL 逐字一致。
pub fn build_dpop_jwt(account: &Account, method: &str, htu: &str) -> Result<String, String> {
    let key = parse_key(account)?;
    let header = json!({
        "alg": "ES256",
        "typ": "dpop+jwt",
        "jwk": { "kty": "EC", "crv": "P-256", "x": key.x, "y": key.y }
    });
    let payload = json!({
        "htm": method,
        "htu": htu,
        "iat": now_secs(),
        "jti": random_hex(16)
    });
    let h = b64url(serde_json::to_vec(&header).map_err(|e| e.to_string())?.as_slice());
    let p = b64url(serde_json::to_vec(&payload).map_err(|e| e.to_string())?.as_slice());
    let signing_input = format!("{h}.{p}");
    let sig: Signature = key.signing.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", b64url(&sig.to_bytes())))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// form-urlencoded 编码（值都是 base64url/hex，实际不会出现特殊字符；仍按规范转义）。
fn form_enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 用 refresh_token 换一组新凭据。
pub async fn refresh_token(
    client: &reqwest::Client,
    sts_url: &str,
    account: &Account,
) -> Result<Refreshed, String> {
    let refresh = account
        .refresh_token
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or("账号没有 refresh_token，无法续期")?;
    let verifier = account
        .code_verifier
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or("账号没有 PKCE code_verifier，无法续期")?;
    let client_id = account
        .client_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("codearts");
    let dpop = build_dpop_jwt(account, "POST", sts_url)?;
    let form = [
        ("client_id", client_id),
        ("code_verifier", verifier),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
    ];
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", form_enc(k), form_enc(v)))
        .collect::<Vec<_>>()
        .join("&");
    let resp = client
        .post(sts_url)
        .header("DPoP", dpop)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("续期请求失败: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("读续期响应失败: {e}"))?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("续期响应不是 JSON (HTTP {status}): {e}; body={}", trunc(&text, 300)))?;
    if !status.is_success() {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message").and_then(|m| m.as_str()).or_else(|| e.as_str()))
            .or_else(|| v.get("error_description").and_then(|x| x.as_str()))
            .or_else(|| v.get("error_msg").and_then(|x| x.as_str()))
            .unwrap_or("续期被拒");
        return Err(format!("续期失败 HTTP {status}: {}", trunc(msg, 300)));
    }
    parse_token_response(&v, refresh)
}

/// 解析 STS 响应（兼容 `credentials` / 旧 `credential` 两种字段名）。
pub fn parse_token_response(v: &Value, old_refresh: &str) -> Result<Refreshed, String> {
    if let Some(err) = v.get("error") {
        if !err.is_null() && err.as_str() != Some("") {
            return Err(format!("续期返回错误: {err}"));
        }
    }
    let creds = v
        .get("credentials")
        .or_else(|| v.get("credential"))
        .ok_or("续期响应缺少 credentials")?;
    let pick = |k_snake: &str, k_camel: &str| -> String {
        creds
            .get(k_snake)
            .or_else(|| creds.get(k_camel))
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let access_key = pick("access_key_id", "accessKeyId");
    let secret_key = pick("secret_access_key", "secretAccessKey");
    let security_token = pick("security_token", "securityToken");
    if access_key.is_empty() || secret_key.is_empty() || security_token.is_empty() {
        return Err(format!(
            "续期响应凭据不完整: {}",
            trunc(&v.to_string(), 300)
        ));
    }
    let expiration = creds
        .get("expiration")
        .or_else(|| creds.get("expires_at"))
        .or_else(|| creds.get("expiresAt"));
    let expires_at = match expiration {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => {
            // 毫秒或秒都兼容
            let raw = n.as_i64().unwrap_or(0);
            let secs = if raw > 10_000_000_000 { raw / 1000 } else { raw };
            crate::config::iso8601_utc(secs)
        }
        _ => String::new(),
    };
    let refresh_token = v
        .get("refresh_token")
        .or_else(|| v.get("refreshToken"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(old_refresh)
        .to_string();
    Ok(Refreshed {
        access_key,
        secret_key,
        security_token,
        expires_at,
        refresh_token,
    })
}

fn trunc(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;

    fn test_account() -> Account {
        // 固定标量 1；对应公钥可复现。
        let signing = SigningKey::from_slice(&[1u8; 32]).unwrap();
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let point = signing.verifying_key().to_encoded_point(false);
        let x = b64url(point.x().unwrap());
        let y = b64url(point.y().unwrap());
        let d = b64url(&[1u8; 32]);
        let private = json!({"kty":"EC","crv":"P-256","x":x,"y":y,"d":d}).to_string();
        let public = json!({"kty":"EC","crv":"P-256","x":x,"y":y}).to_string();
        Account {
            refresh_token: Some("r".into()),
            code_verifier: Some("v".into()),
            client_id: Some("codearts".into()),
            dpop_private_jwk: Some(json!({"privateKey": private, "publicKey": public})),
            ..Default::default()
        }
    }

    #[test]
    fn dpop_jwt_has_expected_claims_and_verifies() {
        let acct = test_account();
        let jwt = build_dpop_jwt(&acct, "POST", DEFAULT_STS_URL).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: Value = serde_json::from_slice(&b64url_decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "dpop+jwt");
        assert!(header["jwk"]["x"].is_string());
        let payload: Value = serde_json::from_slice(&b64url_decode(parts[1]).unwrap()).unwrap();
        assert_eq!(payload["htm"], "POST");
        assert_eq!(payload["htu"], DEFAULT_STS_URL);
        assert_eq!(payload["jti"].as_str().unwrap().len(), 32);
        assert!(payload["iat"].as_u64().unwrap() > 1_700_000_000);

        // 签名可用 header 里的公钥验证（证明 r||s 64 字节格式正确）
        let x = b64url_decode(header["jwk"]["x"].as_str().unwrap()).unwrap();
        let y = b64url_decode(header["jwk"]["y"].as_str().unwrap()).unwrap();
        let mut sec1 = vec![0x04u8];
        sec1.extend_from_slice(&x);
        sec1.extend_from_slice(&y);
        let vk = VerifyingKey::from_sec1_bytes(&sec1).unwrap();
        let sig_bytes = b64url_decode(parts[2]).unwrap();
        assert_eq!(sig_bytes.len(), 64);
        let sig = Signature::from_slice(&sig_bytes).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .expect("DPoP 签名必须可验证");
    }

    #[test]
    fn parses_token_response() {
        let v = json!({
            "credentials": {
                "access_key_id": "AK", "secret_access_key": "SK",
                "security_token": "TOK", "expiration": "2026-10-27T00:00:00Z"
            },
            "refresh_token": "RT2"
        });
        let r = parse_token_response(&v, "RT1").unwrap();
        assert_eq!(r.access_key, "AK");
        assert_eq!(r.refresh_token, "RT2");
        assert_eq!(r.expires_at, "2026-10-27T00:00:00Z");
    }
}
