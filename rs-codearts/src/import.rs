//! 本机码道（CodeArts Doer）登录态自动导入。
//!
//! 2026-10 新版码道是 **Electron 应用**（不是参考仓库里的 VS Code fork），凭据落在：
//!
//! ```text
//! %APPDATA%\codearts\Local State
//!   os_crypt.encrypted_key = base64("DPAPI" + CryptProtectData(AES-256 key))
//!
//! %APPDATA%\codearts\secretStorage\<sha256>.enc
//!   base64( "v10" + nonce(12) + AES-256-GCM(ct||tag) )   ← 注意外层还有一层 base64
//! ```
//!
//! 解出来的 JSON：
//!
//! ```jsonc
//! { "session": { "userId","userName","domainId","accessKey","secretKey",
//!                "securityToken","refreshToken","expiresAt"(毫秒),"clientId" },
//!   "loginContext": { "pkcePair": {...}, "dpopKeyPair": { "privateKey","publicKey" } } }
//! ```
//!
//! **只读**：不写回、不续期（续期会让桌面端与插件互相作废 refresh_token）。
//! 桌面端每小时自己刷新一次，本模块按文件 mtime 跟随即可。

use std::path::{Path, PathBuf};

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use serde_json::Value;

use crate::config::{iso8601_utc, Account, SessionAccount};

/// 本机码道数据目录（`%APPDATA%\codearts`）。
pub fn local_app_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    if appdata.trim().is_empty() {
        return None;
    }
    Some(Path::new(&appdata).join("codearts"))
}

/// 导入结果。
#[derive(Clone, Debug)]
pub struct Imported {
    pub account: Account,
    /// 说明来源，便于 `/health` 与日志（不打印任何凭据）。
    pub source: String,
    /// `secretStorage/*.enc` 的修改时间（毫秒），用于热跟随。
    pub mtime_ms: u64,
}

/// 读取本机登录态；失败返回可读原因（调用方决定是否降级到账号文件）。
pub fn read_local_session() -> Result<Imported, String> {
    let dir = local_app_dir().ok_or("APPDATA 未设置")?;
    if !dir.is_dir() {
        return Err(format!("未找到码道数据目录 {}", dir.display()));
    }
    let key = read_master_key(&dir.join("Local State"))?;
    let (enc_path, mtime_ms) = newest_secret(&dir.join("secretStorage"))?;
    let raw = std::fs::read(&enc_path)
        .map_err(|e| format!("读 {} 失败: {e}", enc_path.display()))?;
    let plain = decrypt_secret(&key, &raw)?;
    let v: Value = serde_json::from_slice(&plain)
        .map_err(|e| format!("登录态 JSON 解析失败: {e}"))?;
    let mut account = account_from_value(&v)?;
    let display = account.display_name();
    account.name = display;
    Ok(Imported {
        account,
        source: format!("本机码道 ({})", enc_path.file_name().unwrap_or_default().to_string_lossy()),
        mtime_ms,
    })
}

fn read_master_key(local_state: &Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read_to_string(local_state)
        .map_err(|e| format!("读 {} 失败: {e}", local_state.display()))?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("解析 Local State 失败: {e}"))?;
    let b64 = v
        .get("os_crypt")
        .and_then(|c| c.get("encrypted_key"))
        .and_then(|k| k.as_str())
        .ok_or("Local State 里没有 os_crypt.encrypted_key（这版码道可能尚未登录）")?;
    let decoded = base64_decode(b64)?;
    let blob = decoded
        .strip_prefix(b"DPAPI")
        .ok_or("encrypted_key 缺少 DPAPI 前缀")?;
    let key = dpapi_unprotect(blob)?;
    if key.len() != 32 {
        return Err(format!("主密钥长度异常: {}", key.len()));
    }
    Ok(key)
}

fn newest_secret(dir: &Path) -> Result<(PathBuf, u64), String> {
    let mut best: Option<(PathBuf, u64)> = None;
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读 {} 失败: {e}", dir.display()))?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("enc") {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if best.as_ref().map(|(_, m)| mtime > *m).unwrap_or(true) {
            best = Some((path, mtime));
        }
    }
    best.ok_or_else(|| format!("{} 里没有 .enc 凭据文件（尚未登录）", dir.display()))
}

fn decrypt_secret(master: &[u8], raw: &[u8]) -> Result<Vec<u8>, String> {
    // 外层是 base64 文本（文件名里那串 sha256 只是键名，不影响解密）
    let decoded = if raw.first() == Some(&b'v') && raw.get(1) == Some(&b'1') {
        raw.to_vec()
    } else {
        base64_decode(std::str::from_utf8(raw).map_err(|_| "凭据文件不是 base64 文本")?)?
    };
    if decoded.len() < 15 || &decoded[..3] != b"v10" {
        return Err("凭据文件不是 v10 封装".to_string());
    }
    let cipher = Aes256Gcm::new_from_slice(master).map_err(|e| format!("AES key 非法: {e}"))?;
    let nonce = Nonce::try_from(&decoded[3..15])
        .map_err(|_| "nonce 长度必须是 12 字节".to_string())?;
    cipher
        .decrypt(&nonce, &decoded[15..])
        .map_err(|_| "AES-256-GCM 解密失败（DPAPI 主密钥不匹配？）".to_string())
}

fn account_from_value(v: &Value) -> Result<Account, String> {
    let s = v.get("session").ok_or("登录态里没有 session")?;
    let get = |k: &str| s.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let expires_ms = s.get("expiresAt").and_then(|x| x.as_i64()).unwrap_or(0);
    let expires_at = if expires_ms > 0 {
        Some(iso8601_utc(expires_ms / 1000))
    } else {
        None
    };
    let user_name = get("userName");
    let login_ctx = v.get("loginContext").cloned();
    Ok(Account {
        name: user_name.clone(),
        enabled: true,
        access_key: get("accessKey"),
        secret_key: get("secretKey"),
        security_token: get("securityToken"),
        expires_at,
        refresh_token: s
            .get("refreshToken")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
        code_verifier: login_ctx
            .as_ref()
            .and_then(|c| c.get("pkcePair"))
            .and_then(|p| p.get("codeVerifier"))
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
        dpop_private_jwk: login_ctx
            .as_ref()
            .and_then(|c| c.get("dpopKeyPair"))
            .cloned(),
        domain_id: s
            .get("domainId")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
        client_id: s
            .get("clientId")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
        plugin_name: Some("snap_AIIDE".to_string()),
        plugin_version: Some("5.1.0".to_string()),
        ide_name: Some("CodeArts".to_string()),
        ide_version: Some("5.1.0".to_string()),
        account: Some(SessionAccount {
            id: get("userId"),
            label: user_name,
        }),
    })
}

// ---------------------------------------------------------------------------
// 基础件：base64 / DPAPI
// ---------------------------------------------------------------------------

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))
}

#[cfg(windows)]
fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    use std::ffi::c_void;
    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut c_void,
    }
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;
    #[link(name = "crypt32")]
    extern "system" {
        fn CryptUnprotectData(
            p_data_in: *const DataBlob,
            ppsz_data_descr: *mut *mut u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *mut c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(h_mem: *mut c_void) -> *mut c_void;
    }

    let in_buf = data.to_vec();
    let in_blob = DataBlob {
        cb_data: in_buf.len() as u32,
        pb_data: in_buf.as_ptr() as *mut c_void,
    };
    let mut out = DataBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 {
        return Err("CryptUnprotectData 失败（非当前用户或不是本机加密）".to_string());
    }
    let bytes = unsafe {
        let slice = std::slice::from_raw_parts(out.pb_data as *const u8, out.cb_data as usize);
        let v = slice.to_vec();
        LocalFree(out.pb_data);
        v
    };
    Ok(bytes)
}

#[cfg(not(windows))]
fn dpapi_unprotect(_data: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI 只在 Windows 可用；请用账号文件导入 session".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        assert_eq!(base64_decode("djEw").unwrap(), b"v10");
    }
}
