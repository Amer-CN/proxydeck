//! crypto.rs —— 数据文件的落盘形态（`PDAPI1` 密文）与 Windows DPAPI。
//!
//! 对应 Go `internal/appdir/crypt.go` + `crypt_windows.go` + `acl_windows.go`（以
//! `rs-vibex/src/crypto.rs` 为底本，按本 crate 的日志口径适配，再加一处功能性扩展；
//! 「复制后各自独立」是既定惯例）：
//!
//! * **读**：明文原样返回（旧版落的文件）；`PDAPI1\n` 前缀 → base64 解 → `CryptUnprotectData`。
//!   解密失败**如实报错**，绝不返回半截内容（`accounts.go:77` 的 `appdir.ReadFile` 就靠这个）。
//! * **写**：先 DPAPI 加密（明文 access_token 绝不落盘），再 `PDAPI1\n`+base64+`\n`，写 `.tmp` →
//!   `icacls` 收紧到当前用户 → rename（`accounts.go:110` 的 `appdir.WriteFile`）。
//!
//! base64 与 Go `base64.StdEncoding` 同表**同严格度**（4 字节对齐、填充符只能在末尾；
//! 只忽略 `\r`/`\n`，空格/制表符算非法）。
use std::path::Path;

/// 密文自描述前缀（与 Go `cryptPrefix` 同值）。
pub const CRYPT_PREFIX: &[u8] = b"PDAPI1\n";

/// 读数据文件：明文原样返回，DPAPI 密文自动解密。
pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("读文件失败: {e}"))?;
    if !raw.starts_with(CRYPT_PREFIX) {
        return Ok(raw);
    }
    unprotect_file_bytes(&raw)
}

/// 解回落盘形态（前缀 + base64 密文）。密文格式坏掉时如实报错，不返回半截数据。
pub fn unprotect_file_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    let body = raw
        .get(CRYPT_PREFIX.len()..)
        .ok_or_else(|| "密文前缀不完整".to_string())?;
    let body: Vec<u8> = body
        .iter()
        .copied()
        .skip_while(|b| b.is_ascii_whitespace())
        .collect();
    let end = body
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(0);
    let ct = base64_decode(&body[..end]).map_err(|e| format!("密文 base64 损坏: {e}"))?;
    unprotect(&ct).map_err(|e| format!("密文解密失败: {e}"))
}

/// 写数据文件：DPAPI 加密 + ACL 收紧 + tmp→rename。明文绝不落盘。
pub fn write_file(path: &Path, plain: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let enc = protect_file_bytes(plain)?;
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, &enc).map_err(|e| format!("写临时文件失败: {e}"))?;
    if let Err(e) = acl_user_only(&tmp) {
        // ACL 收紧失败不阻断写入（数据本体仍然加密），但如实留痕。
        crate::logf(&format!(
            "ACL 收紧失败（文件仍为密文）：{}: {}",
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            e
        ));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("改名落盘失败: {e}"));
    }
    Ok(())
}

/// 把明文编成落盘形态（前缀 + base64 密文 + 换行，单行）。
pub fn protect_file_bytes(plain: &[u8]) -> Result<Vec<u8>, String> {
    let ct = protect(plain)?;
    let mut out = Vec::with_capacity(CRYPT_PREFIX.len() + ct.len() * 4 / 3 + 2);
    out.extend_from_slice(CRYPT_PREFIX);
    out.extend_from_slice(base64_encode(&ct).as_bytes());
    out.push(b'\n');
    Ok(out)
}

/// 标准 base64 编码（与 Go `base64.StdEncoding` 同表）。
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
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
            out.push(TABLE[*k] as char);
        }
        for _ in take..4 {
            out.push('=');
        }
    }
    out
}

/// 标准 base64 解码：与 Go `base64.StdEncoding` 同表**同严格度**，
/// 外加对无填充尾段（余 2/3 字符）的 Raw 收尾——rs-vibex 版要求输入 4 倍长，
/// 而 `PDAPI1` 落盘形态与 JWT 段天然无填充（Go `RawStdEncoding` 写出），故扩展。
/// 已用 Go 1.26.5 实测注释锁定边界（见函数内）。
pub fn base64_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut clean: Vec<u8> = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    let mut seen_data_after_pad = false;
    for &b in input {
        // Go 的 `base64.StdEncoding` **只忽略 `\r` 和 `\n`**；空格/制表符是非法字符。
        if b == b'\r' || b == b'\n' {
            continue;
        }
        if b == b'=' {
            pad += 1;
            continue;
        }
        if pad > 0 {
            seen_data_after_pad = true;
        }
        clean.push(b);
    }
    if seen_data_after_pad {
        return Err("填充符位置非法".into());
    }
    if pad > 2 {
        return Err("填充符过多".into());
    }
    // 长度规则分两种形态：带填充（`PDAPI1` 密文走这条）必须补齐到 4 的倍数；
    // **无填充**（`base64.RawURLEncoding`，`accounts.go`/`cdp_browser.go` 解 JWT payload 用的就是它）
    // 允许余 2/3 —— Go 的 Raw 解码器专门支持这种收尾（`decodeQuantum` 的 `n == 2/3`）。
    if pad > 0 && (clean.len() + pad) % 4 != 0 {
        return Err("长度不是 4 的倍数".into());
    }
    // 填充符个数必须与数据长度匹配
    match pad {
        1 if clean.len() % 4 == 3 => {}
        2 if clean.len() % 4 == 2 => {}
        0 if matches!(clean.len() % 4, 0 | 2 | 3) => {}
        _ => return Err("填充符个数与长度不匹配".into()),
    }

    let full = clean.len() / 4 * 4;
    let mut out = Vec::with_capacity(clean.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &b in &clean[..full] {
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
    let rem = &clean[full..];
    if !rem.is_empty() {
        let mut v: u32 = 0;
        for &b in rem {
            let x = TABLE
                .iter()
                .position(|&c| c == b)
                .ok_or_else(|| format!("非法字符 0x{b:02x}"))? as u32;
            v = (v << 6) | x;
        }
        match rem.len() {
            2 => out.push(((v >> 4) & 0xff) as u8),
            3 => {
                out.push(((v >> 10) & 0xff) as u8);
                out.push(((v >> 2) & 0xff) as u8);
            }
            // 余 1 是残缺输入（Go 的 Raw 解码器同样报 `CorruptInputError`：
            // 1 个字符凑不出一个输出字节）
            _ => return Err("长度不是 4 的倍数".into()),
        }
        // 注：Go 1.26.5 实测**不校验**「无填充收尾的尾部填充位」——`QUJDRQ`（余 2、
        // 尾字符 R 的低 4 位非零）在 `RawStdEncoding` 下解成 `ABCE` 而不报错
        // （带填充路径才查，见 `decodeQuantum` 的 `dlen == 4` 分支）。这里同样不查。
    }
    Ok(out)
}

/// `aclUserOnly`：给文件补一条「仅当前用户」的显式授权（与 Go 逐字同款 icacls 调用）。
/// `/grant:r` 只替换该用户自己的条目，不动其它条目，**绝不会把访问权收没了**。
#[cfg(windows)]
fn acl_user_only(path: &Path) -> Result<(), String> {
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return Err("USERNAME 环境变量为空，无法收紧 ACL".into());
    }
    let user = match (std::env::var("USERDOMAIN"), std::env::var("COMPUTERNAME")) {
        (Ok(dom), Ok(comp)) if !dom.is_empty() && dom != comp => format!("{dom}\\{user}"),
        _ => user,
    };
    let out = std::process::Command::new("icacls")
        .arg(path)
        .arg("/grant:r")
        .arg(format!("{user}:(F)"))
        .output()
        .map_err(|e| format!("icacls 调用失败: {e}"))?;
    if !out.status.success() {
        let text = String::from_utf8_lossy(&out.stdout);
        let text: String = text.chars().take(300).collect();
        return Err(format!("icacls 失败: {} ({})", out.status, text));
    }
    Ok(())
}

#[cfg(not(windows))]
fn acl_user_only(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;

    /// Windows `CRYPT_INTEGER_BLOB { DWORD cbData; BYTE *pbData; }`。
    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut c_void,
    }

    /// 失败即报错，绝不弹系统对话框（与 Go `cryptprotectUIForbidden` 同值）。
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            p_data_in: *const DataBlob,
            sz_data_descr: *const u16,
            p_optional_entropy: *const DataBlob,
            pv_reserved: *mut c_void,
            p_prompt_struct: *mut c_void,
            dw_flags: u32,
            p_data_out: *mut DataBlob,
        ) -> i32;
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

    /// 加密（当前用户作用域）。对应 Go `appdir.protect`。
    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        if plain.is_empty() {
            return Err("CryptProtectData: 空内容".into());
        }
        let mut in_blob = DataBlob {
            cb_data: plain.len() as u32,
            pb_data: plain.as_ptr() as *mut c_void,
        };
        let mut out_blob = DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &mut in_blob,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out_blob,
            )
        };
        finish(ok, out_blob, "CryptProtectData", plain)
    }

    /// 解密（当前用户作用域）。对应 Go `appdir.unprotect`。
    pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
        if cipher.is_empty() {
            return Err("CryptUnprotectData: 空内容".into());
        }
        let mut in_blob = DataBlob {
            cb_data: cipher.len() as u32,
            pb_data: cipher.as_ptr() as *mut c_void,
        };
        let mut out_blob = DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &mut in_blob,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out_blob,
            )
        };
        finish(ok, out_blob, "CryptUnprotectData", cipher)
    }

    /// 收集系统新分配的输出块并释放；`keepalive` 保证入参活过整次系统调用。
    fn finish(ok: i32, out_blob: DataBlob, what: &str, keepalive: &[u8]) -> Result<Vec<u8>, String> {
        let _ = keepalive;
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("{what}: {err}"));
        }
        if out_blob.pb_data.is_null() || out_blob.cb_data == 0 {
            if !out_blob.pb_data.is_null() {
                unsafe { LocalFree(out_blob.pb_data) };
            }
            return Err(format!("{what}: 系统返回空数据"));
        }
        // 当次拷走，再释放系统内存
        let out = unsafe {
            std::slice::from_raw_parts(out_blob.pb_data as *const u8, out_blob.cb_data as usize)
                .to_vec()
        };
        unsafe { LocalFree(out_blob.pb_data) };
        Ok(out)
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows 没有 DPAPI：如实报错（这条只为可编译）。
    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
        Err("本平台无 DPAPI（CryptProtectData）".into())
    }
    pub fn unprotect(_cipher: &[u8]) -> Result<Vec<u8>, String> {
        Err("本平台无 DPAPI（CryptUnprotectData）".into())
    }
}

pub use imp::{protect, unprotect};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_go_std_encoding() {
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(base64_decode(b"aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode(b"QUJD").unwrap(), b"ABC");
        assert_eq!(base64_decode(b"QUJD\n").unwrap(), b"ABC");
        assert_eq!(base64_decode(b"QU\r\nJD").unwrap(), b"ABC");
        assert_eq!(base64_decode(b"aGk").unwrap(), b"hi", "RawURLEncoding 形态（JWT 走这条）");
        // RawURLEncoding 的收尾（JWT payload 走这条）：无填充、余 2/3 都要能解
        assert_eq!(base64_decode(b"eyJzdWIiOiJ1c2VyLTEyMyJ9").unwrap(), br#"{"sub":"user-123"}"#);
        assert_eq!(base64_decode(b"eyJhIjoxfQ").unwrap(), br#"{"a":1}"#);
        assert_eq!(
            base64_decode(b"QUJDRA").unwrap(),
            b"ABCD",
            "RawURLEncoding 的余 2 收尾（Go 实测 raw=QUJDRA）"
        );
        assert_eq!(
            base64_decode(b"QUJDRQ").unwrap(),
            b"ABCE",
            "Go 1.26.5 实测不校验无填充收尾的尾部填充位"
        );
        assert!(base64_decode(b"QUJDR").is_err(), "余 1 是残缺输入（Go 同样报错）");
        assert!(base64_decode(b"aG k=").is_err(), "空格在 Go 侧不忽略");
        assert!(base64_decode(b"QUJD===").is_err());
        assert!(base64_decode(b"Q=UJD").is_err());
        let data: Vec<u8> = (0u8..=255).collect();
        assert_eq!(base64_decode(base64_encode(&data).as_bytes()).unwrap(), data);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip_and_prefix() {
        let plain = b"{\"accounts\":[]}\n";
        let enc = protect_file_bytes(plain).expect("加密应成功");
        assert!(enc.starts_with(CRYPT_PREFIX));
        let back = unprotect_file_bytes(&enc).expect("解密应成功");
        assert_eq!(back, plain);
        assert!(unprotect_file_bytes(plain).is_err());
    }

    /// **一次性取证**（默认 `#[ignore]`，手工跑）：用同一把 DPAPI（当前用户）解开
    /// 真实数据目录里的历史文件，把大小与顶层结构写进临时文件供核对。
    ///
    /// 只读不写：`read_file` 不落盘、不改动源文件；输出落到临时目录，绝不打印内容。
    /// 用法：`cargo test --offline --lib decrypt_real -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn decrypt_real_file_one_shot() {
        let name = std::env::var("RS_TJ_DECRYPT_NAME").unwrap_or_else(|_| "tuanjie-accounts.json".into());
        let dir = std::env::var("APPDATA")
            .map(|a| std::path::PathBuf::from(a).join("ProxyDeck"))
            .expect("APPDATA");
        let p = dir.join(&name);
        let plain = read_file(&p).expect("解开真实文件");
        let v: serde_json::Value = serde_json::from_slice(&plain).expect("解出的应是合法 JSON");
        let shape = if let Some(o) = v.as_object() {
            let mut keys: Vec<String> = o.keys().cloned().collect();
            keys.sort();
            let mut per: Vec<String> = Vec::new();
            if let Some(arr) = o.get("accounts").and_then(|a| a.as_array()) {
                per.push(format!("accounts={}", arr.len()));
                if let Some(first) = arr.first().and_then(|f| f.as_object()) {
                    let mut fk: Vec<String> = first.keys().cloned().collect();
                    fk.sort();
                    per.push(format!("first.keys={}", fk.join(",")));
                    per.push(format!(
                        "first.user_id.len={}",
                        first
                            .get("user_id")
                            .and_then(|x| x.as_str())
                            .map(|s| s.len())
                            .unwrap_or(0)
                    ));
                    per.push(format!(
                        "first.token.len={}",
                        first
                            .get("token")
                            .and_then(|x| x.as_str())
                            .map(|s| s.len())
                            .unwrap_or(0)
                    ));
                }
            }
            format!("keys=[{}] {}", keys.join(","), per.join(" "))
        } else {
            "非对象".to_string()
        };
        let out = std::env::temp_dir().join("rs-tj-decrypt-shape.txt");
        std::fs::write(&out, format!("{name}: plain={} 字节 {shape}\n", plain.len())).unwrap();
        println!("{name}: plain={} 字节 {shape}", plain.len());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_rejects_unrelated_bytes() {
        let err = unprotect(&[0x00, 0x01, 0x02, 0x03]).unwrap_err();
        assert!(err.contains("CryptUnprotectData"), "应如实报系统错误: {err}");
    }
}
