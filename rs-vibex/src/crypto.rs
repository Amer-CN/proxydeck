//! crypto.rs —— 数据文件的落盘形态（`PDAPI1` 密文）与 Windows DPAPI。
//!
//! 对应 Go `internal/appdir/crypt.go` + `crypt_windows.go` + `acl_windows.go`：
//!
//! * **读**：明文原样返回（旧版落的文件）；`PDAPI1\n` 前缀 → base64 解 → `CryptUnprotectData`。
//!   解密失败**如实报错**，绝不返回半截内容。
//! * **写**：先 DPAPI 加密（明文绝不落盘），再 `PDAPI1\n`+base64+`\n`，写 `.tmp` → `icacls`
//!   收紧到当前用户 → rename。加密失败即返回错误且**不改动原文件**。
//!
//! base64 与 Go `base64.StdEncoding` 同表**同严格度**（要求 4 字节对齐、填充符只能在末尾；
//! 只忽略 `\r`/`\n`，空格/制表符算非法）。FFI 写法与 `rs-bai`/`rs-qoder` 一致：
//! 入参整块在系统调用期间保持存活；输出**当次拷走再释放**（把系统内存当切片用，
//! 一有分配就可能踩到已释放内存）。
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

/// 已经把 ACL 收紧过的目录（进程内缓存，见 [`write_file`] 的说明）。
fn acl_done_dirs() -> &'static std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static C: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// 写数据文件：DPAPI 加密 + ACL 收紧 + tmp→rename。明文绝不落盘。
///
/// **ACL 每个目录只收紧一次（进程内）**：原来每次写都对目标文件跑一遍 `icacls`，
/// 而 `accounts.rs::pick()`（**每个请求都调**）会走到这里——等于每请求起一个
/// `icacls.exe` 子进程：既会弹黑窗（v4.2.4 已用 CREATE_NO_WINDOW 藏掉），又是
/// 30~50ms 的纯浪费，用户侧看到的是"ProxyDeck 在不停地设置权限"。
/// 现在改为：首次写某目录时对该**目录**设一次 `(OI)(CI)` 继承授权，之后同目录的
/// 新文件自动继承，不再调子进程。DPAPI 密文仍是本体保护，ACL 只是纵深防御。
pub fn write_file(path: &Path, plain: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败: {e}"))?;
    }
    let enc = protect_file_bytes(plain)?;
    let tmp = std::path::PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, &enc).map_err(|e| format!("写临时文件失败: {e}"))?;
    // 该目录还没收紧过 → 对目录设一次带继承的授权（子文件自动继承）
    let dir_key = path.parent().map(|d| d.to_path_buf());
    let need_acl = match &dir_key {
        Some(d) => !acl_done_dirs().lock().unwrap().contains(d),
        None => true,
    };
    if need_acl {
        let target = dir_key.clone().unwrap_or_else(|| tmp.clone());
        match acl_dir_user_only(&target) {
            Ok(()) => {
                if let Some(d) = dir_key {
                    acl_done_dirs().lock().unwrap().insert(d);
                }
            }
            Err(e) => {
                // 目录级失败 → 退回单文件收紧（保持旧行为，只是这一次）
                crate::logf(&format!("目录 ACL 收紧失败，退回单文件：{e}"));
                if let Err(e2) = acl_user_only(&tmp) {
                    // 收紧失败不阻断写入（数据本体仍然加密），但如实留痕。
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    crate::logf(&format!("ACL 收紧失败（文件仍为密文）：{name}: {e2}"));
                }
            }
        }
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

/// 标准 base64 解码：与 Go `base64.StdEncoding` 同表**同严格度**。
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
    if (clean.len() + pad) % 4 != 0 {
        return Err("长度不是 4 的倍数".into());
    }
    match pad {
        1 if clean.len() % 4 == 3 => {}
        2 if clean.len() % 4 == 2 => {}
        0 if clean.len() % 4 == 0 => {}
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
            _ => return Err("长度不是 4 的倍数".into()),
        }
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
    let mut acl_cmd = std::process::Command::new("icacls");
    acl_cmd
        .arg(path)
        .arg("/grant:r")
        .arg(format!("{user}:(F)"));
    // CREATE_NO_WINDOW：写状态文件（stats/pool）会走这里，每写一次弹一个黑窗。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        acl_cmd.creation_flags(0x0800_0000);
    }
    let out = acl_cmd
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

/// 目录级授权（带 `(OI)(CI)` 继承）：**只需调一次**，之后该目录下新建的文件自动继承。
/// 用于替代"每次写文件都跑一遍 `icacls`"——`accounts.rs::pick()` 每个请求都会写盘，
/// 原来等于每请求起一个子进程（30~50ms + 黑窗），用户侧表现为"ProxyDeck 不停设置权限"。
#[cfg(windows)]
fn acl_dir_user_only(dir: &Path) -> Result<(), String> {
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return Err("USERNAME 环境变量为空，无法收紧 ACL".into());
    }
    let user = match (std::env::var("USERDOMAIN"), std::env::var("COMPUTERNAME")) {
        (Ok(dom), Ok(comp)) if !dom.is_empty() && dom != comp => format!("{dom}\\{user}"),
        _ => user,
    };
    let mut acl_cmd = std::process::Command::new("icacls");
    acl_cmd
        .arg(dir)
        .arg("/grant:r")
        .arg(format!("{user}:(OI)(CI)F"));
    // CREATE_NO_WINDOW：本进程是 GUI 子系统，spawn 控制台程序会新建黑窗。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        acl_cmd.creation_flags(0x0800_0000);
    }
    let out = acl_cmd
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
fn acl_dir_user_only(_dir: &Path) -> Result<(), String> {
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
        assert!(base64_decode(b"aGk").is_err(), "未填充 Go 也报错");
        assert!(base64_decode(b"aG k=").is_err(), "空格在 Go 侧不忽略");
        assert!(base64_decode(b"QUJD===").is_err());
        assert!(base64_decode(b"Q=UJD").is_err());
        // 往返
        let data: Vec<u8> = (0u8..=255).collect();
        assert_eq!(base64_decode(base64_encode(&data).as_bytes()).unwrap(), data);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip_and_prefix() {
        let plain = b"{\"tokens\":[\"FAKE-tok\"]}";
        let enc = protect_file_bytes(plain).expect("加密应成功");
        assert!(enc.starts_with(CRYPT_PREFIX));
        let back = unprotect_file_bytes(&enc).expect("解密应成功");
        assert_eq!(back, plain);
        // 无密文前缀的字节按密文解应报错（生产路径由 read_file 先判前缀，故明文不会被这样解）
        assert!(unprotect_file_bytes(plain).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_rejects_unrelated_bytes() {
        let err = unprotect(&[0x00, 0x01, 0x02, 0x03]).unwrap_err();
        assert!(err.contains("CryptUnprotectData"), "应如实报系统错误: {err}");
    }
}
