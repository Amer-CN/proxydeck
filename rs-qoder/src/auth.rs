//! `internal/qoder/auth.go` 的 Rust 等价物：读并解密 Qoder CN App 的登录态。
//!
//! 解密链（与 Go 逐条同序，任一步失败都回错误 ⇒ 调用方回 `no_license`）：
//!
//! ```text
//! %APPDATA%\com.qodercn.app.stable\
//!   Local State    → json["os_crypt"]["encrypted_key"]（base64，std 表）
//!                  → 去掉开头 5 字节 "DPAPI" → CryptUnprotectData(CurrentUser) = 32 字节 AES key
//!   auth.v1.dat    → 前 3 字节必须是 "v10"、总长 ≥ 19
//!                  → nonce = dat[3:15]，密文+tag = dat[15:]
//!                  → AES-GCM 解出 JSON {token, expiresAt, user}
//! ```
//!
//! **凭据纪律**：本模块只把 token 交给调用方（worker 的 `fetch_job_token` 应答），
//! 任何错误信息都**不含** token 或密钥内容；日志里只允许出现「存在性/长度」。

use std::path::{Path, PathBuf};

/// 与 Go `authDir()` 同值：`%APPDATA%\com.qodercn.app.stable`。
pub const AUTH_DIR_NAME: &str = "com.qodercn.app.stable";

/// 与 Go `authDir()` 同判定：`filepath.Join(os.Getenv("APPDATA"), "com.qodercn.app.stable")`。
/// APPDATA 缺失时 Go 得到的是相对路径 `com.qodercn.app.stable`（Join("") 的结果），这里同样返回它。
pub fn auth_dir() -> PathBuf {
    match std::env::var("APPDATA") {
        Ok(v) if !v.is_empty() => Path::new(&v).join(AUTH_DIR_NAME),
        _ => PathBuf::from(AUTH_DIR_NAME),
    }
}

/// 解密后的登录态（对应 Go `qoderAuth`）。
#[derive(Debug, Clone, PartialEq)]
pub struct QoderAuth {
    /// `dt-` OAuth token（`fetch_job_token` 回调回给 worker）。**绝不打印、绝不落盘**。
    pub token: String,
    /// `expiresAt` 的 Unix 毫秒；`None` = 解不出（Go 的零值 `time.Time`）。
    pub expires_at_ms: Option<i64>,
    /// `user` 字段原样保留（Go 侧也只存取不判）。
    pub user: Option<serde_json::Value>,
}

/// 读 `Local State` + `auth.v1.dat` 并解密。错误串与 Go 同义（前缀逐字对齐 Go 的 `%w` 包装）。
pub fn load_auth(dir: &Path) -> Result<QoderAuth, String> {
    let ls = std::fs::read(dir.join("Local State"))
        .map_err(|e| format!("读取 Local State 失败: {}", io_err_text(&e)))?;
    let ls_json: serde_json::Value = serde_json::from_slice(&ls)
        .map_err(|e| format!("Local State 解析失败: {}", json_err_text(&e)))?;
    let enc = ls_json
        .get("os_crypt")
        .and_then(|v| v.get("encrypted_key"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Go：base64 解码失败、或去前缀前长度<6、或前 5 字节不是 "DPAPI" → 「encrypted_key 格式异常」
    let raw = match crate::crypto::base64_std_decode(enc) {
        Ok(v) if v.len() >= 6 && &v[..5] == b"DPAPI" => v,
        _ => return Err("encrypted_key 格式异常".to_string()),
    };
    let key = crate::crypto::unprotect(&raw[5..]).map_err(|e| format!("DPAPI 解密失败: {e}"))?;

    let dat = std::fs::read(dir.join("auth.v1.dat"))
        .map_err(|e| format!("读取 auth.v1.dat 失败: {}", io_err_text(&e)))?;
    if dat.len() < 19 || &dat[..3] != b"v10" {
        return Err("auth.v1.dat 格式异常".to_string());
    }
    let pt = aes_gcm_open(&key, &dat[3..15], &dat[15..])
        .map_err(|e| format!("auth.v1.dat AES-GCM 解密失败: {e}"))?;

    let parsed: serde_json::Value = serde_json::from_slice(&pt)
        .map_err(|_| "登录态 JSON 解析失败（token 为空）".to_string())?;
    let token = parsed
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        return Err("登录态 JSON 解析失败（token 为空）".to_string());
    }
    let expires_at_ms = parsed
        .get("expiresAt")
        .and_then(|v| v.as_str())
        .and_then(parse_rfc3339_millis);
    Ok(QoderAuth {
        token,
        expires_at_ms,
        user: parsed.get("user").cloned(),
    })
}

/// Go 侧 `os.ReadFile` 失败的文案近似（`%v` 给的是 `open <path>: ...`；
/// 这里只保留后半截，错误串不进 HTTP 合同，见 README 差异清单）。
fn io_err_text(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "系统找不到指定的文件。".to_string(),
        _ => e.to_string(),
    }
}

/// Rust 侧 JSON 解析错误 → Go 文案的近似（同上，只进日志/`no_license` 判定）。
fn json_err_text(e: &serde_json::Error) -> String {
    format!("invalid character / {e}")
}

/// AES-GCM 解密（组合模式：`ct||tag`）。Go `aes.NewCipher(key)` 接受 16/24/32 字节，
/// 这里逐档对应选实现；其它长度回 Go 的原文案。
fn aes_gcm_open(key: &[u8], nonce: &[u8], ct_and_tag: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::consts::U12;
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes128Gcm, Aes256Gcm, AesGcm, Nonce};

    // aes-gcm 0.11 只导出 128/256 的别名；192 用泛型别名补上（Go 同样接受这个档）
    type Aes192Gcm = AesGcm<aes_gcm::aes::Aes192, U12>;

    let n: Nonce<U12> = nonce
        .try_into()
        .map_err(|_| "crypto/cipher: incorrect nonce length given to GCM".to_string())?;
    match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(|e| e.to_string())?
            .decrypt(&n, ct_and_tag)
            .map_err(|e| e.to_string()),
        24 => Aes192Gcm::new_from_slice(key)
            .map_err(|e| e.to_string())?
            .decrypt(&n, ct_and_tag)
            .map_err(|e| e.to_string()),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(|e| e.to_string())?
            .decrypt(&n, ct_and_tag)
            .map_err(|e| e.to_string()),
        n => Err(format!("crypto/aes: invalid key size {n}")),
    }
}

/// Go `time.Parse(time.RFC3339, s)` 的等价物：成功回 Unix 毫秒，失败回 `None`。
///
/// 布局 `2006-01-02T15:04:05Z07:00`，Go 的额外宽容度照搬：
/// * 秒后**可选**的小数部分（`.` 或 `,` 皆可，见 Go `Parse` 里 `commaOrPeriod` 分支）；
/// * 时区要么 `Z`，要么 `±hh:mm`（RFC3339 布局里写的是 `Z07:00`）；
/// * 多出来的尾随内容一律算失败（Go 的 `extra text` 错误）。
///
/// 期望值（含各种边界与失败样本）由真 Go 采收，锁在 `tests/golden/golden-qoder-go.json`
/// 的 `rfc3339` 段。
pub fn parse_rfc3339_millis(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0usize;
    let digits = |b: &[u8], i: &mut usize, n: usize| -> Option<i64> {
        if *i + n > b.len() {
            return None;
        }
        let mut v: i64 = 0;
        for k in 0..n {
            let c = b[*i + k];
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        *i += n;
        Some(v)
    };
    let year = digits(b, &mut i, 4)?;
    if b.get(i) != Some(&b'-') {
        return None;
    }
    i += 1;
    let month = digits(b, &mut i, 2)?;
    if b.get(i) != Some(&b'-') {
        return None;
    }
    i += 1;
    let day = digits(b, &mut i, 2)?;
    if b.get(i) != Some(&b'T') {
        return None;
    }
    i += 1;
    let hour = digits(b, &mut i, 2)?;
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    let min = digits(b, &mut i, 2)?;
    if b.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    let sec = digits(b, &mut i, 2)?;
    // 可选小数秒
    let mut nanos: i64 = 0;
    if matches!(b.get(i), Some(&b'.') | Some(&b',')) && b.get(i + 1).map_or(false, u8::is_ascii_digit) {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let take = frac.len().min(9);
        let mut v: i64 = frac[..take].parse().ok()?;
        for _ in take..9 {
            v *= 10;
        }
        nanos = v;
    }
    // 时区
    let off_secs: i64 = match b.get(i) {
        Some(&b'Z') => {
            i += 1;
            0
        }
        Some(&sign @ (b'+' | b'-')) => {
            i += 1;
            let oh = digits(b, &mut i, 2)?;
            if b.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
            let om = digits(b, &mut i, 2)?;
            let mag = oh * 3600 + om * 60;
            if sign == b'+' {
                mag
            } else {
                -mag
            }
        }
        _ => return None,
    };
    if i != b.len() {
        return None; // Go: extra text
    }
    // 范围校验（Go 的 Parse 同样拒绝 13 月 / 32 日 / 25 时）
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || min > 59 || sec > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86400 + hour * 3600 + min * 60 + sec - off_secs;
    // Go 的零值 `time.Time` 就是「公元 1 年 1 月 1 日 00:00:00 **UTC**」——`IsZero()` 为真，
    // 调用方（`currentAuth` 的缓存门槛、`fetch_job_token` 的 `expires_at`）都会走「没有过期时间」
    // 那一支。所以这个**恰好落在零值上**的解析结果要映射成 `None`，否则与 Go 分歧。
    if year == 1 && month == 1 && day == 1 && hour == 0 && min == 0 && sec == 0 && nanos == 0 && off_secs == 0
    {
        return None;
    }
    Some(secs * 1000 + nanos / 1_000_000)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant 的 days_from_civil（与 Go 的 `time.Date` 内部算法同语义）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    // 期望值由**真 Go** `time.Parse(time.RFC3339, s)` 采收（golden-qoder-go.json 的 rfc3339 段）。
    #[test]
    fn rfc3339_parsing_matches_go_samples() {
        let ok: Vec<(&str, i64)> = vec![
            ("2026-10-15T08:30:00Z", 1792053000000),
            ("2026-10-15T08:30:00.5Z", 1792053000500),
            ("2026-10-15T08:30:00.123Z", 1792053000123),
            ("2026-10-15T08:30:00.123456789Z", 1792053000123),
            ("2026-10-15T08:30:00+08:00", 1792024200000),
            ("2026-10-15T08:30:00-05:30", 1792072800000),
            ("2026-10-15T08:30:00+00:00", 1792053000000),
            ("2026-10-15T08:30:00,25Z", 1792053000250),
            ("2026-10-15T23:59:59Z", 1792108799000),
            ("2026-10-15T00:00:00Z", 1792022400000),
            ("2024-02-29T00:00:00Z", 1709164800000),
        ];
        for (s, want) in ok {
            assert_eq!(parse_rfc3339_millis(s), Some(want), "input={s}");
        }
        let bad = [
            "",
            "2026-10-15",
            "2026-10-15T08:30:00",
            "2026-10-15T08:30:00Z ",
            "2026-10-15T08:30:00ZZ",
            "2026-13-15T08:30:00Z",
            "2026-10-32T08:30:00Z",
            "2026-10-15T25:30:00Z",
            "2026-10-15T08:30:00+0800",
            "2026-10-15t08:30:00Z",
            "not-a-time",
            "2026-02-29T00:00:00Z",
        ];
        for s in bad {
            assert_eq!(parse_rfc3339_millis(s), None, "input={s}");
        }
        // Go 的零值 `time.Time`（`IsZero()==true`）要映射成 None，
        // 否则 worker 的 `expires_at` 会被多塞一个字段（黄金 rfc3339 段 `0001-01-01T00:00:00Z`）
        assert_eq!(parse_rfc3339_millis("0001-01-01T00:00:00Z"), None);
    }
}
