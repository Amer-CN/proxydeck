//! 签名层——**红线 1/2/3** 的落点（`client.go:304-324`）。
//!
//! - 红线 1：种子 hex 常量逐字（[`CODEY_SIGNING_SEED_HEX`] ← `client.go:44`）
//! - 红线 2：两层 HMAC 派生 `HMAC(seed,"codely-signing-v1") → HMAC(k1, cli_api_key)`
//! - 红线 3：签名体 `["v1", path, timestamp].join("\n")` → `v1.<ts>.<base64url>`
//!
//! 时间戳只取**秒**（Go `time.Time.Unix()`）；base64 是**无填充** URL 变体
//! （Go `base64.RawURLEncoding`）。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 官方 CLI 内置签名种子（逆向自 1.0.0-release.52）——`client.go:44` 逐字。
pub const CODEY_SIGNING_SEED_HEX: &str =
    "406f00f74768ba0cb0cd30f097ec6c2bdacb89c61a38b7dd140838bbd0e98018";

/// 第一层 HMAC 的固定消息——`client.go:313`。
pub const CODEY_SIGNING_KDF_LABEL: &str = "codely-signing-v1";

/// 签名版本前缀——`client.go:323`。
pub const SIGNATURE_VERSION: &str = "v1";

/// HMAC-SHA256（对应 Go `hmacSHA256`，`client.go:304-308`）。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// 从 cli_api_key 派生签名密钥（对应 Go `codelySigningKey`，`client.go:311-315`）。
///
/// ```text
/// k1  = HMAC-SHA256(seed, "codely-signing-v1")
/// key = HMAC-SHA256(k1, cli_api_key)
/// ```
pub fn codely_signing_key(cli_api_key: &str) -> [u8; 32] {
    let seed = hex_decode(CODEY_SIGNING_SEED_HEX).expect("种子常量是合法 hex");
    let k1 = hmac_sha256(&seed, CODEY_SIGNING_KDF_LABEL.as_bytes());
    hmac_sha256(&k1, cli_api_key.as_bytes())
}

/// 生成 `X-Codely-Signature` 头值（对应 Go `SignLitellm`，`client.go:319-324`）。
///
/// 消息体 `"v1\n<path>\n<timestamp>"`（只签 path，不含查询串处理——调用方给什么签什么）。
/// 时间戳从 `now_unix` 显式传入（Go 侧取 `time.Now()`，这里由调用方给，便于已知答案测试）。
pub fn sign_litellm(path: &str, cli_api_key: &str, now_unix: i64) -> String {
    let ts = now_unix.to_string();
    let msg = format!("{SIGNATURE_VERSION}\n{path}\n{ts}");
    let sig = hmac_sha256(&codely_signing_key(cli_api_key), msg.as_bytes());
    format!("{SIGNATURE_VERSION}.{ts}.{}", URL_SAFE_NO_PAD.encode(sig))
}

/// 十六进制解码（对应 Go `hex.DecodeString` 的成功路径）。
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = hex_val(b[i])?;
        let lo = hex_val(b[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_is_32_bytes() {
        assert_eq!(hex_decode(CODEY_SIGNING_SEED_HEX).unwrap().len(), 32);
    }

    /// 既有 Go `sign_test.go` 的参考值（由 Node crypto 独立实现算出、2026-08-21 上游 200 通过）。
    #[test]
    fn known_answer_from_go_sign_test() {
        assert_eq!(
            sign_litellm("/v1/chat/completions", "sk-test-key-1234567890", 1750000000),
            "v1.1750000000.SvPELRsS7YUqeErP10Z_rK3_dExT699l9GYyFalB3uU"
        );
    }

    // 注：秒级截断由参数类型静态保证——`sign_litellm` 只接受整秒 `i64` 时间戳，
    // 亚秒信息在类型层面就进不来，故此处只留文档说明，不设断言。
}
