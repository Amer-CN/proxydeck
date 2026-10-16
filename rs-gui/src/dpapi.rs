//! dpapi.rs —— 落盘加密形态（对应 Go `internal/appdir/crypt.go` + `crypt_windows.go`）。
//!
//! 与 `rs-proxy/src/cred_crypt.rs`（P6 从 `rs-bai/src/crypto.rs` 整份搬过去的那份）同源：
//! 本文件把**读侧照搬**过来，并补上桥接层必需的**写侧**（`protect` / `protect_file_bytes`）——
//! `appdir.WriteFile` 落盘一律 DPAPI 加密（明文绝不落盘），`ccSaveKey` / `ccDismiss` 都走它。
//!
//! Go 侧口径（逐条复刻）：
//!   * 落盘形态：`PDAPI1\n` + base64(标准表，带填充) + `\n`，单行
//!   * 没有这个前缀 → 当明文原样返回（读兼容：旧版落的文件仍能读）
//!   * 有前缀 → base64 解码后调 DPAPI（**当前用户作用域**）
//!   * 解密失败如实返回错误，绝不返回半截内容
//!   * `CryptProtectData`/`CryptUnprotectData` 的空内容入参直接报错（Go 明文这么判）

/// 密文自描述前缀（与 Go `cryptPrefix` 同值）。
pub const CRYPT_PREFIX: &[u8] = b"PDAPI1\n";

/// 字节流是不是 DPAPI 密文形态（Go `isProtected`）。
pub fn is_protected(b: &[u8]) -> bool {
    b.starts_with(CRYPT_PREFIX)
}

/// 读数据文件：明文原样返回，DPAPI 密文自动解密。
/// 错误信息只说明「读失败 / 解密失败」，**绝不夹带文件内容**。
pub fn read_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{e}"))?;
    if !is_protected(&raw) {
        return Ok(raw);
    }
    unprotect_file_bytes(&raw)
}

/// 解回落盘形态（前缀 + base64 密文）。密文格式坏掉时如实报错，不返回半截数据。
pub fn unprotect_file_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    if !is_protected(raw) {
        return Err("不是 PDAPI1 密文形态".into());
    }
    let body = &raw[CRYPT_PREFIX.len()..];
    // Go 用 bytes.TrimSpace：两端空白都切掉
    let start = body
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(body.len());
    let end = body
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    let trimmed = &body[start..end];
    let ct = base64_decode(trimmed).map_err(|e| format!("密文 base64 损坏: {e}"))?;
    unprotect(&ct)
}

/// 把明文编成落盘形态（前缀 + base64 密文 + 换行）。
pub fn protect_file_bytes(plain: &[u8]) -> Result<Vec<u8>, String> {
    let ct = protect(plain)?;
    let mut out = Vec::with_capacity(CRYPT_PREFIX.len() + base64_encoded_len(ct.len()) + 1);
    out.extend_from_slice(CRYPT_PREFIX);
    out.extend_from_slice(&base64_encode(&ct));
    out.push(b'\n');
    Ok(out)
}

fn base64_encoded_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

/// 标准 base64 编码（Go `base64.StdEncoding`：同表、带 `=` 填充）。
pub fn base64_encode(data: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(base64_encoded_len(data.len()));
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize]);
        out.push(TABLE[((n >> 12) & 0x3f) as usize]);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3f) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3f) as usize]
        } else {
            b'='
        });
    }
    out
}

/// 标准 base64 解码：与 Go `base64.StdEncoding` 同表**同严格度**——要求 4 字节对齐、
/// 填充符只能出现在末尾。容忍空白（密文是「前缀 + base64 + 换行」的单行文本）。
fn base64_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut clean: Vec<u8> = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    let mut seen_data_after_pad = false;
    for &b in input {
        if b.is_ascii_whitespace() {
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

    /// 调一次 DPAPI，把系统新分配的输出块当次拷走、立刻 LocalFree 后返回
    /// （照抄 Go 的实测教训：把系统内存当切片用，一有分配就可能踩到已释放内存）。
    fn call_dpapi(
        protect: bool,
        input: &[u8],
        what: &str,
    ) -> Result<Vec<u8>, String> {
        if input.is_empty() {
            return Err(format!("{what}: 空内容"));
        }
        let mut in_blob = DataBlob {
            cb_data: input.len() as u32,
            pb_data: input.as_ptr() as *mut c_void,
        };
        let mut out_blob = DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &mut in_blob,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out_blob,
                )
            } else {
                CryptUnprotectData(
                    &mut in_blob,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out_blob,
                )
            }
        };
        std::hint::black_box(input);
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
        let out = unsafe {
            std::slice::from_raw_parts(out_blob.pb_data as *const u8, out_blob.cb_data as usize).to_vec()
        };
        unsafe { LocalFree(out_blob.pb_data) };
        Ok(out)
    }

    /// 加密（当前用户作用域）：其他 Windows 账户 / 拷到别的机器都解不开。
    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        call_dpapi(true, data, "CryptProtectData")
    }

    /// 解密（当前用户作用域）。
    pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
        call_dpapi(false, cipher, "CryptUnprotectData")
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows 没有 DPAPI：如实报错（对应 Go 侧 `crypt_other.go`）。
    pub fn protect(_data: &[u8]) -> Result<Vec<u8>, String> {
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
    fn plaintext_passes_through_unchanged() {
        let dir = std::env::temp_dir().join("rs-gui-dpapi-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plain.txt");
        let body = b"hello-plain".to_vec();
        std::fs::write(&p, &body).unwrap();
        assert_eq!(read_file(&p).unwrap(), body, "无前缀即明文，一个字节都不许动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_protect_then_read() {
        let plain = b"TESTKEY-ALIGN".to_vec();
        let enc = protect_file_bytes(&plain).unwrap();
        assert!(is_protected(&enc), "落盘形态必须带 PDAPI1 前缀");
        assert_eq!(unprotect_file_bytes(&enc).unwrap(), plain);
        // 与 Go 的落盘形态同形：前缀 + 单行 base64 + 换行
        let text = String::from_utf8(enc.clone()).unwrap();
        assert!(text.starts_with("PDAPI1\n"));
        assert!(text.ends_with('\n'));
        assert_eq!(text.matches('\n').count(), 2, "只有前缀后与结尾两个换行");
    }

    #[test]
    fn base64_encoder_matches_std_encoding() {
        assert_eq!(base64_encode(b""), b"");
        assert_eq!(base64_encode(b"f"), b"Zg==");
        assert_eq!(base64_encode(b"fo"), b"Zm8=");
        assert_eq!(base64_encode(b"foo"), b"Zm9v");
        assert_eq!(base64_encode(b"foob"), b"Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), b"Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), b"Zm9vYmFy");
    }

    #[test]
    fn base64_decoder_matches_std_encoding() {
        assert_eq!(base64_decode(b"aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode(b"QUJD").unwrap(), b"ABC");
        assert_eq!(base64_decode(b"QUJD\n").unwrap(), b"ABC", "容忍尾随换行");
        assert!(base64_decode(b"aGk").is_err(), "未填充必须报错");
        assert!(base64_decode(b"QUJD===").is_err(), "填充个数不匹配必须报错");
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_call_is_really_made() {
        let err = unprotect(&[0x00, 0x01, 0x02, 0x03]).unwrap_err();
        assert!(err.contains("CryptUnprotectData"), "应如实报系统错误: {err}");
    }

    #[cfg(windows)]
    #[test]
    fn decrypts_a_go_written_blob() {
        // 由 Go 宿主（appdir.WriteFile + ccSaveKey("TESTKEY-ALIGN")）产出的真实密文原件，
        // Rust 侧必须能解开。注意：它是**另一次采集运行的产出**，与现行黄金 S1_fresh 里
        // 那份不是同一批字节（DPAPI 每次换随机会话密钥；已核对同 316 字节、第 89 字节起分叉），
        // 但解出的明文同样是 TESTKEY-ALIGN——这正是跨实现兼容要证明的（不是各写各的往返测试）。
        let raw = include_bytes!("../tests/golden/pdapi1-go-apikey-sample.txt");
        assert!(is_protected(raw), "样本必须是 PDAPI1 形态");
        assert_eq!(
            unprotect_file_bytes(raw).unwrap(),
            b"TESTKEY-ALIGN".to_vec(),
            "Go 写的密文 Rust 必须能解回原文"
        );
    }
}
