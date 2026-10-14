// crypto.rs —— 读数据文件的「明文 / DPAPI 密文」双形态（对应 Go 侧
// `internal/appdir/crypt.go` + `crypt_windows.go`）。
//
// Go 侧口径（逐条复刻）：
//   * 落盘形态：`PDAPI1\n` + base64(标准表，带填充) + `\n`，单行
//   * 没有这个前缀 → 当明文原样返回（读兼容：旧版落的文件仍能读）
//   * 有前缀 → base64 解码后调 DPAPI `CryptUnprotectData`（**当前用户作用域**）
//   * 解密失败如实返回错误，绝不返回半截内容（调用方据此报错）
//
// 为什么必须自己调 DPAPI：渠道配置 `tuanjie-water-channels.json` 里是 B.AI 的 Bearer，
// 磁盘上是 DPAPI 密文；不解开就拿不到 key，`/model/matrix` 永远 need_key。
// Rust 侧用 std 的 FFI 直接装 crypt32.dll（与 Go 的 `syscall.NewLazyDLL("crypt32.dll")`
// 同一套系统 API），**不引入新依赖**。本模块只读不写——试点期不碰任何配置文件。
//
// 写侧（protect）暂时没有调用方，故不实现——需要时再补，免得多一份没人用的加密代码。

/// 密文自描述前缀（与 Go `cryptPrefix` 同值）。
const CRYPT_PREFIX: &[u8] = b"PDAPI1\n";

/// 读数据文件：明文原样返回，DPAPI 密文自动解密。
/// 错误信息只说明「读失败 / 解密失败」，**绝不夹带文件内容**（调用方会把错误回给 GUI）。
pub fn read_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
    if !raw.starts_with(CRYPT_PREFIX) {
        return Ok(raw);
    }
    unprotect_file_bytes(&raw).map_err(|e| format!("DPAPI 解密失败: {e}"))
}

/// 解回落盘形态（前缀 + base64 密文）。密文格式坏掉时如实报错，不返回半截数据。
pub fn unprotect_file_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    let body = &raw[CRYPT_PREFIX.len()..];
    // Go 用 bytes.TrimSpace：两端空白都切掉（base64 解码器本身也容忍换行，这里保持同一步骤便于对照）
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

/// 标准 base64 解码：与 Go `base64.StdEncoding` 同表**同严格度**——要求 4 字节对齐、
/// 填充符只能出现在末尾（`aGk` 这种未填充的输入 Go 也报错，这里同样报错）。
/// 容忍空白：密文是「前缀 + base64 + 换行」的单行文本，读取时两端的换行必须吃掉。
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

    /// 解密 DPAPI 密文（当前用户作用域）。
    ///
    /// 两处照抄 Go 的实测教训（`internal/appdir/crypt_windows.go` 注释）：
    /// 入参整块交给系统调用期间必须保持存活；输出**必须当次拷走再释放**——
    /// 把系统内存当切片用，一有分配就可能踩到已释放内存。
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
    /// 非 Windows 没有 DPAPI：如实报错（对应 Go 侧 `crypt_other.go`）。
    pub fn unprotect(_cipher: &[u8]) -> Result<Vec<u8>, String> {
        Err("本平台无 DPAPI（CryptUnprotectData）".into())
    }
}

pub use imp::unprotect;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_passes_through_unchanged() {
        let dir = std::env::temp_dir().join("rs-bai-crypto-plain");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plain.json");
        let body = b"[{\"id\":\"bai\",\"key\":\"sk-plain\"}]".to_vec();
        std::fs::write(&p, &body).unwrap();
        assert_eq!(read_file(&p).unwrap(), body, "无前缀即明文，一个字节都不许动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_an_error_not_empty_bytes() {
        let p = std::env::temp_dir().join("rs-bai-crypto-missing-xyz.json");
        let _ = std::fs::remove_file(&p);
        assert!(read_file(&p).is_err(), "文件缺失必须报错，不能当空内容");
    }

    #[test]
    fn corrupt_base64_fails_without_leaking_content() {
        let dir = std::env::temp_dir().join("rs-bai-crypto-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("corrupt.json");
        std::fs::write(&p, b"PDAPI1\nnot!base64!!\n").unwrap();
        let err = read_file(&p).unwrap_err();
        assert!(err.contains("DPAPI 解密失败"), "错误种类要可辨认: {err}");
        assert!(
            !err.contains("not!base64"),
            "错误信息不得夹带文件内容: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base64_decoder_matches_std_encoding() {
        // 表与严格度对齐 Go `base64.StdEncoding`：带填充、4 字节对齐
        assert_eq!(base64_decode(b"aGk=").unwrap(), b"hi");
        assert_eq!(base64_decode(b"QUJD").unwrap(), b"ABC");
        assert_eq!(base64_decode(b"QUJD\n").unwrap(), b"ABC", "容忍尾随换行");
        assert_eq!(base64_decode(b"").unwrap(), b"");
        assert!(
            base64_decode(b"aGk").is_err(),
            "未填充（长度非 4 的倍数）Go 也报错，这里必须一致"
        );
        assert!(base64_decode(b"****").is_err(), "非法字符要报错");
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
