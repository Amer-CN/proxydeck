//! jwt.rs —— Rh-Accesstoken 的 JWT payload 解码（对应 Go `internal/vibex/jwt.go`）。
//!
//! 与 Go 逐条对齐（C10/C11）：
//! * `strings.Split(token, ".")`，`len(parts) < 2` → `nil`；
//! * 取 `parts[1]` 并 `TrimRight(seg, "=")`，用 **RawURL**（无填充、URL 安全表）解码；
//! * 解码失败或 payload 不是 JSON 对象 → `nil`；
//! * `sub`（`str`）、`nickName`/`user_name`/`username`/`mobile` 首个非空，全空回退 `sub`；
//! * `exp` 只认 JSON 数值（Go 解成 `float64` 后 `int64` 截断）；非数值 → 零值（无 exp）。
use serde_json::Value;

/// 解码后我们用得到的字段（对应 Go `jwtInfo`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtInfo {
    pub sub: String,
    pub username: String,
    /// Unix 秒；`None` = 无 exp 或解析不出（对应 Go 的零值 `time.Time`）。
    pub exp: Option<i64>,
}

impl JwtInfo {
    /// 对应 Go `hasExp`。
    pub fn has_exp(&self) -> bool {
        self.exp.is_some()
    }
    /// 对应 Go `expired`（无 exp 视为不过期）。
    pub fn expired(&self, now_unix: i64) -> bool {
        self.has_exp() && now_unix > self.exp.unwrap()
    }
}

/// URL 安全 base64（无填充）解码——与 Go `base64.RawURLEncoding` 同表同严格度。
///
/// Go 只忽略 `\r`/`\n`；其余非法字符报错；无填充时长度 % 4 == 1 也报错。
fn raw_url_base64_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut clean: Vec<u8> = Vec::with_capacity(input.len());
    for &b in input {
        if b == b'\r' || b == b'\n' {
            continue;
        }
        if b == b'=' {
            return Err("RawURLEncoding 不接受填充符".into());
        }
        clean.push(b);
    }
    if clean.len() % 4 == 1 {
        return Err("长度非法".into());
    }
    let mut out = Vec::with_capacity(clean.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in &clean {
        let v = TABLE
            .iter()
            .position(|&c| c == b)
            .ok_or_else(|| format!("非法字符 0x{b:02x}"))? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// 解出 JWT 的 payload 段；不是合法 JWT 返回 `None`（对应 Go `decodeJWT`）。
pub fn decode_jwt(token: &str) -> Option<JwtInfo> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let seg = parts[1].trim_end_matches('=');
    let raw = raw_url_base64_decode(seg.as_bytes()).ok()?;
    let payload: Value = serde_json::from_slice(&raw).ok()?;
    let map = match payload {
        Value::Object(m) => m,
        _ => return None,
    };
    let mut info = JwtInfo {
        sub: crate::util::str_of(map.get("sub")),
        username: String::new(),
        exp: None,
    };
    for k in ["nickName", "user_name", "username", "mobile"] {
        let v = crate::util::str_of(map.get(k));
        if !v.is_empty() {
            info.username = v;
            break;
        }
    }
    if info.username.is_empty() {
        info.username = info.sub.clone();
    }
    info.exp = unix_time(map.get("exp"));
    Some(info)
}

/// 把 JWT 里的 `exp` 转成 Unix 秒；非法值返回 `None`（对应 Go `unixTime`）。
fn unix_time(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().map(|f| f as i64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jwt(payload: &str) -> String {
        format!("eyJhbGciOiJIUzI1NiJ9.{}.SIG", {
            // 与采集器同一套合成编码（RawURL，无填充）
            let bytes = payload.as_bytes();
            let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
                let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
                let idx = [
                    ((n >> 18) & 63) as usize,
                    ((n >> 12) & 63) as usize,
                    ((n >> 6) & 63) as usize,
                    (n & 63) as usize,
                ];
                let take = match chunk.len() {
                    1 => 2,
                    2 => 3,
                    _ => 4,
                };
                for k in idx.iter().take(take) {
                    out.push(table[*k] as char);
                }
            }
            out
        })
    }

    #[test]
    fn decodes_like_go() {
        let tok = fake_jwt(r#"{"sub":"u-1","nickName":"小明","exp":1800000000}"#);
        let info = decode_jwt(&tok).expect("应解出");
        assert_eq!(info.sub, "u-1");
        assert_eq!(info.username, "小明");
        assert_eq!(info.exp, Some(1800000000));
        assert!(info.has_exp());
        assert!(!info.expired(1800000000));
        assert!(info.expired(1800000001));
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_jwt("").is_none());
        assert!(decode_jwt("hello").is_none());
        assert!(decode_jwt("a.b").is_none(), "b 不是合法 base64/JSON");
        assert!(decode_jwt("a.@@@@.c").is_none());
    }

    #[test]
    fn non_numeric_exp_counts_as_none() {
        let tok = fake_jwt(r#"{"sub":"u-x","exp":"nope"}"#);
        let info = decode_jwt(&tok).expect("应解出");
        assert!(!info.has_exp());
        assert_eq!(info.username, "u-x", "全空回退 sub");
    }
}
