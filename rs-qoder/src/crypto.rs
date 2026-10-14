// crypto.rs —— DPAPI 解密 + Go `base64.StdEncoding` 兼容的 base64 解码。
//
// 来源：`rs-bai/src/crypto.rs`（P0 试点）的**裁剪版**——那份是「读 ProxyDeck 自己的
// `PDAPI1` 密文文件」，qoder 用不上那层封装；这里只保留它真正需要的两块：
//   1. `unprotect`：直接调 Windows DPAPI `CryptUnprotectData`（当前用户作用域），
//      对应 Go `internal/qoder/auth.go` 的 `dpapiUnprotect`；
//   2. `base64_decode`：与 Go `base64.StdEncoding.DecodeString` **同表同严格度**——
//      `Local State` 里的 `os_crypt.encrypted_key` 就是标准表 base64，
//      而 Go 对「未填充 / 填充符位置非法 / 数据出现在填充符之后」一律报错，
//      这里必须一致（否则坏文件会被解成垃圾字节、再往下走出一串误判）。
//      被忽略的字符也同口径：**只有 `\r` 与 `\n`**，空格/制表符在 Go 侧算非法字符。
//
// FFI 写法与 rs-bai 逐字相同（入参整块在系统调用期间保持存活；输出**当次拷走再释放**——
// 把系统内存当切片用，一有分配就可能踩到已释放内存）。**不引入新依赖**。

/// 标准 base64 解码：与 Go `base64.StdEncoding` 同表**同严格度**——要求 4 字节对齐、
/// 填充符只能出现在末尾（`aGk` 这种未填充的输入 Go 也报错，这里同样报错）。
fn base64_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut clean: Vec<u8> = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    let mut seen_data_after_pad = false;
    for &b in input {
        // Go 的 `base64.StdEncoding` **只忽略 `\r` 和 `\n`**（文档原文：「New line characters
        // (\r and \n) are ignored」）；空格/制表符在 Go 侧是非法字符。这里必须同样口径，
        // 否则坏文件会被解成垃圾字节、再往下走出一串误判（原先放宽成「所有 ASCII 空白」）。
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
    // 填充符个数必须与数据长度匹配（Go 同样拒绝 `===`/`QUJD===`）
    match pad {
        1 if clean.len() % 4 == 3 => {}
        2 if clean.len() % 4 == 2 => {}
        0 if clean.len() % 4 == 0 => {}
        _ => return Err("填充符个数与长度不匹配".into()),
    }

    // 先解所有完整的 4 字符组
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
    // 收尾的非整组（0/2/3 个字符）：填充位不进输出，语义与 Go 一致
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

/// Go `base64.StdEncoding.DecodeString`（字符串入口）。
pub fn base64_std_decode(s: &str) -> Result<Vec<u8>, String> {
    base64_decode(s.as_bytes())
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

    /// 解密 DPAPI 密文（当前用户作用域）。对应 Go `auth.go:dpapiUnprotect`。
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
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("CryptUnprotectData: {err}"));
        }
        if out_blob.pb_data.is_null() || out_blob.cb_data == 0 {
            if !out_blob.pb_data.is_null() {
                unsafe { LocalFree(out_blob.pb_data) };
            }
            return Err("CryptUnprotectData: 系统返回空数据".into());
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
    /// 非 Windows 没有 DPAPI：如实报错（Qoder CN 只有 Windows 版，这条只为可编译）。
    pub fn unprotect(_cipher: &[u8]) -> Result<Vec<u8>, String> {
        Err("本平台无 DPAPI（CryptUnprotectData）".into())
    }
}

pub use imp::unprotect;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decoder_matches_std_encoding() {
        // 表与严格度对齐 Go `base64.StdEncoding`：带填充、4 字节对齐
        assert_eq!(base64_decode(b"aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode(b"QUJD").unwrap(), b"ABC");
        assert_eq!(base64_decode(b"QUJD\n").unwrap(), b"ABC", "容忍换行（Go 忽略 \\n）");
        assert_eq!(base64_decode(b"QU\r\nJD").unwrap(), b"ABC", "容忍 CRLF（Go 忽略 \\r\\n）");
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert!(
            base64_decode(b"aGk").is_err(),
            "未填充（长度非 4 的倍数）Go 也报错，这里必须一致"
        );
        assert!(base64_decode(b"****").is_err(), "非法字符要报错");
        // Go 只忽略 \r 与 \n：空格/制表符在 Go 侧是非法字符（旧实现放宽成「所有空白」，已改）
        assert!(base64_decode(b"aG k=").is_err(), "空格在 Go 侧不是被忽略的字符");
        assert!(base64_decode(b"QUJD\t").is_err(), "制表符在 Go 侧不是被忽略的字符");
        // 收尾非整组（标准表）：`QUJDREU=` = "ABCDE"
        assert_eq!(base64_decode(b"QUJDREU=").unwrap(), b"ABCDE");
        assert!(base64_decode(b"QUJD===").is_err(), "填充个数与长度不匹配要报错");
        assert!(base64_decode(b"QUJDREU").is_err(), "不足 4 的倍数要报错");
        assert!(base64_decode(b"Q=UJD").is_err(), "数据出现在填充符之后要报错");
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_call_is_really_made() {
        // 系统调用本身能跑通、但对无关字节必然失败——证明走的是真 DPAPI 而不是空转
        let err = unprotect(&[0x00, 0x01, 0x02, 0x03]).unwrap_err();
        assert!(err.contains("CryptUnprotectData"), "应如实报系统错误: {err}");
    }
}
