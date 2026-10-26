//! 华为云 SDK-HMAC-SHA256 请求签名。
//!
//! 逐字对齐 `@huaweicloud/huaweicloud-sdk-core` 的 JS signer（码道桌面端内打包的那份）。
//! **两处与官方文档不同，必须同时复刻**，否则上游必回 `APIG.0301`：
//!
//! 1. CanonicalURI 每段严格 percent-encode 后**强制补尾斜杠**：
//!    `/api/v2/chat/completions` → `/api/v2/chat/completions/`；
//! 2. CanonicalHeaders 每行自带尾 `\n`，拼装 CanonicalRequest 时又补一个 `\n`
//!    —— 于是 `SignedHeaders` 之前有一个空行。
//!
//! 另外三条容易写错的细节（来自 2951461586/CodeArts2API 的 `signer_test.go` 实测）：
//! * URL 编码只保留 `A-Za-z0-9-._~`，其余一律 `%XX` **大写**；
//! * 只有 `PUT` / `PATCH` / `POST` 的 body 参与 payload hash，GET 按空 body；
//! * header 名小写后参与排序与签名，只对**值**做 `trim`。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 签名算法名。
pub const ALGORITHM: &str = "SDK-HMAC-SHA256";

/// 一组 STS 临时凭证。
#[derive(Clone, Debug, Default)]
pub struct Credential {
    pub access_key: String,
    pub secret_key: String,
    pub security_token: String,
}

/// 把请求拆成 (authority, path, query)。只支持绝对 http(s) URL。
fn split_url(raw: &str) -> Result<(String, String, String), String> {
    let after_scheme = raw
        .split_once("://")
        .map(|(_, rest)| rest)
        .ok_or_else(|| format!("URL 缺少 scheme: {raw}"))?;
    let (authority, rest) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => (after_scheme, "/"),
    };
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (rest, String::new()),
    };
    Ok((authority.to_string(), path.to_string(), query))
}

/// 严格 percent-encode：只放过 `A-Za-z0-9-._~`，其余 `%XX` 大写。
pub fn percent_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.as_bytes() {
        let c = *b as char;
        let unreserved = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~');
        if unreserved || (keep_slash && *b == b'/') {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// CanonicalURI：逐段编码 + 强制尾斜杠。
pub fn canonical_uri(path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    let mut out = String::with_capacity(path.len() + 1);
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&percent_encode(seg, false));
    }
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// 百分号解码（`+` 按字面加号，不做 query 的空格转换——只用于把已编码的键值还原后重编码）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &input[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// CanonicalQuery：键、值各自排序后再编码。
pub fn canonical_query(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in raw.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(part), String::new()),
        };
        pairs.push((k, v));
    }
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{}={}", percent_encode(&k, false), percent_encode(&v, false)))
        .collect::<Vec<_>>()
        .join("&")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

fn hmac_hex(key: &str, msg: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC 接受任意长度密钥");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 给请求头集合签名。
///
/// `headers` 的键必须**全部小写**；本函数会补 `host` / `x-sdk-date` /
/// `x-security-token`（若凭证带），再把 `authorization` 写回这张表。
/// 返回写回的 `authorization` 值，便于调用方断言。
pub fn sign_request(
    method: &str,
    raw_url: &str,
    body: &[u8],
    headers: &mut std::collections::BTreeMap<String, String>,
    cred: &Credential,
) -> Result<String, String> {
    let (authority, path, query) = split_url(raw_url)?;
    headers.insert("host".to_string(), authority);
    headers
        .entry("x-sdk-date".to_string())
        .or_insert_with(now_x_sdk_date);
    if !cred.security_token.is_empty() {
        headers.insert("x-security-token".to_string(), cred.security_token.clone());
    }

    // 只有 PUT/PATCH/POST 的 body 参与签名；GET/HEAD/DELETE 按空 body。
    let payload: &[u8] = match method.to_ascii_uppercase().as_str() {
        "PUT" | "PATCH" | "POST" => body,
        _ => &[],
    };
    let payload_hash = sha256_hex(payload);

    // 键已按 BTreeMap 字典序排列（与 JS 的 .sort() 同序）。
    let names: Vec<String> = headers.keys().cloned().collect();
    let mut canonical_headers = String::new();
    for name in &names {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(headers[name].trim());
        // ★ 每行自带尾 \n；下面拼 CanonicalRequest 时还会再加一个 \n。
        canonical_headers.push('\n');
    }
    let signed_headers = names.join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        canonical_uri(&path),
        canonical_query(&query),
        canonical_headers,
        signed_headers,
        payload_hash
    );
    let string_to_sign = format!(
        "{}\n{}\n{}",
        ALGORITHM,
        headers["x-sdk-date"],
        sha256_hex(canonical_request.as_bytes())
    );
    let signature = hmac_hex(&cred.secret_key, &string_to_sign);
    let authorization = format!(
        "{} Access={}, SignedHeaders={}, Signature={}",
        ALGORITHM, cred.access_key, signed_headers, signature
    );
    headers.insert("authorization".to_string(), authorization.clone());
    Ok(authorization)
}

/// `X-Sdk-Date: 20261026T120000Z`（UTC）。
pub fn now_x_sdk_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_x_sdk_date(now.as_secs())
}

/// 把 Unix 秒格式化成 `%Y%m%dT%H%M%SZ`（UTC）。
pub fn format_x_sdk_date(unix_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil_from_unix(unix_secs as i64);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// Unix 秒 → (年, 月, 日, 时, 分, 秒)，UTC（Howard Hinnant 算法）。
pub fn civil_from_unix(t: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (h, mi, s) = (
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    );
    // days from 1970-01-01 → civil date
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn canonical_uri_adds_trailing_slash() {
        assert_eq!(canonical_uri("/api/v2/chat/completions"), "/api/v2/chat/completions/");
        assert_eq!(canonical_uri("/"), "/");
        assert_eq!(canonical_uri("/v1/model/builtin"), "/v1/model/builtin/");
        // 每段严格编码：冒号/空格都不得放过
        assert_eq!(canonical_uri("/a b:c"), "/a%20b%3Ac/");
    }

    #[test]
    fn date_format_is_utc() {
        assert_eq!(format_x_sdk_date(0), "19700101T000000Z");
        assert_eq!(format_x_sdk_date(1_767_225_600), "20260101T000000Z");
    }

    #[test]
    fn signature_is_stable_and_matches_reference_vector() {
        // 输入固定（固定日期、固定 body、固定头），期望值由独立 Python 复刻算出。
        let cred = Credential {
            access_key: "HSTAAK".to_string(),
            secret_key: "SK".to_string(),
            security_token: "TOKEN".to_string(),
        };
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("x-sdk-date".to_string(), "20261026T120000Z".to_string());
        let body = b"{\"model\":\"glm-5.3\",\"stream\":true}";
        let auth = sign_request(
            "POST",
            "https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/chat/completions",
            body,
            &mut headers,
            &cred,
        )
        .expect("签名不应失败");
        assert_eq!(auth, EXPECTED_AUTH);
        // signed headers 必须含全部头（含 host / x-security-token）
        assert!(auth.contains("SignedHeaders=content-type;host;x-sdk-date;x-security-token"));
    }

    /// 由独立 Python 复刻生成的期望值（见 `.work/task-codearts.md` §4）。
    const EXPECTED_AUTH: &str = "SDK-HMAC-SHA256 Access=HSTAAK, SignedHeaders=content-type;host;x-sdk-date;x-security-token, Signature=48b3dc86971aeee702043e6e3822fcdfe05d0bd25f83f21f5a59722d5bc5cee0";
}
