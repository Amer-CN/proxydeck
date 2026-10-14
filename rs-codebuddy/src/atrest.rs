//! atrest.rs —— WorkBuddy 桌面端 at-rest 加密登录态（`$wbEncrypted`）解密，对应 Go
//! `internal/codebuddy/atrest.go` + `atrest_win.go` / `atrest_other.go`。
//!
//! 密钥链（与 Go / 配方逐字节一致）：
//! 1. `protector = sha256(atRestSecretKey 字符串 utf8)`，32 字节。secret 由桌面端
//!    Electron 原生层提供（`ELECTRON_RUN_AS_NODE=1 <exe> -e loggerGet…`）。
//! 2. `keyId = hex(sha256(protector))[:16]`（envelope.keyId，全安装静态一致，须校验）。
//! 3. AAD（sym-v1 field framing）：`"WB-AAD\x00"` + `\x01` + u32be(5)+"WBEV1" +
//!    u32be(6)+"sym-v1" + u32be(1) + u32be(16)+keyId + `\x02` + `\x00` + `\x00`。
//! 4. AES-256-GCM 解密；刷新后加密回写（nonce 随机，保持桌面端可读）。
//!
//! 取 key 路径纪律（简报红线，逐条照抄 Go）：一律动态发现，**禁止写死默认安装位置**。
//! secret 来源只有三处：`WORKBUDDY_AT_REST_SECRET` 环境变量（逃生口，命中即唯一）、
//! `WORKBUDDY_ELECTRON_PATH`（显式 exe/目录）、注册表卸载表推导（`InstallLocation` 为空时
//! 从 `DisplayIcon` / `UninstallString` 反推）。`LOCALAPPDATA\Programs` 这类默认路径
//! 一律不拼。
//!
//! 凭据纪律：所有错误只给形态与来源（"信封损坏"/"keyId 不一致"/"取密钥失败"），
//! **绝不回显 secret / token / envelope 内容**；成功描述只打 JWT 头部前 20 字＋长度。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use aes_gcm::aead::{AeadInOut, KeyInit};
use aes_gcm::Aes256Gcm;

use crate::region::{Region, REGION_INTL};

/// 经桌面端自带 Electron（run-as-node）调用原生层取 key 的脚本（与 Go / 配方一致）。
pub const LOGGER_GET_JS: &str = "const b = process._linkedBinding('electron_browser_workbuddy_storage');\
process.stdout.write(b.loggerGet().toString('utf8'));";

/// 取 key 失败的负缓存 TTL（Go `atrestErrTTL`）。
pub const ATREST_ERR_TTL: Duration = Duration::from_secs(60);

/// 一把派生好的 at-rest 密钥。
#[derive(Clone)]
pub struct AtrestKey {
    /// `protector = sha256(secret utf8)`。
    pub key: [u8; 32],
    /// `hex(sha256(protector))[:16]`。
    pub key_id: String,
    /// 脱敏来源（`"env"` / exe 完整路径），日志用。
    pub src: String,
}

/// 一个候选桌面端可执行文件。
#[derive(Clone)]
pub struct ElectronCand {
    pub path: String,
    /// `"CN"` / `"INTL"`（DisplayName / 文件名判定，仅用于排序）。
    pub label: String,
}

/// Go `deriveAtrestKey`：由 secret 原串派生（纯函数）。
pub fn derive_atrest_key(secret: &str, src: &str) -> AtrestKey {
    let protector = sha256(secret.as_bytes());
    let id = sha256(&protector);
    AtrestKey {
        key: protector,
        key_id: hex_lower(&id)[..16].to_string(),
        src: src.to_string(),
    }
}

/// `envelope` 字段 base64 解出的内层 JSON（Go `wbEnvelope`）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WbEnvelope {
    pub suite: i64,
    #[serde(rename = "keyId")]
    pub key_id: String,
    pub nonce: String,
    #[serde(rename = "authTag")]
    pub auth_tag: String,
    pub ciphertext: String,
}

/// Go `isEncryptedField`：值是否为 `$wbEncrypted` 信封形态（不碰密钥，纯形态判定）。
pub fn is_encrypted_field(v: &serde_json::Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    match m.get("$wbEncrypted").and_then(|x| x.as_f64()) {
        Some(f) if f == 1.0 => {}
        _ => return false,
    }
    m.get("envelope").map(|x| x.is_string()).unwrap_or(false)
}

/// Go `aadForKeyID`：sym-v1 field framing AAD（逐字节一致）。
pub fn aad_for_key_id(key_id: &str) -> Vec<u8> {
    let mut aad: Vec<u8> = Vec::with_capacity(23 + key_id.len());
    aad.extend_from_slice(b"WB-AAD\x00");
    aad.push(0x01);
    aad.extend_from_slice(&lp("WBEV1"));
    aad.extend_from_slice(&lp("sym-v1"));
    aad.extend_from_slice(&1u32.to_be_bytes());
    aad.extend_from_slice(&lp(key_id));
    aad.extend_from_slice(&[0x02, 0x00, 0x00]);
    aad
}

/// `lp = u32be(len) + bytes`（Go `lp`）。
fn lp(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + s.len());
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    out
}

/// Go `parseWbEnvelope`：解信封结构（**不碰密钥**：形态坏的假信封在此失败，不触发 exe）。
pub fn parse_wb_envelope(raw: &[u8]) -> Result<WbEnvelope, String> {
    let outer: serde_json::Value =
        serde_json::from_slice(raw).map_err(|_| "非 $wbEncrypted 信封形态".to_string())?;
    if !is_encrypted_field(&outer) {
        return Err("非 $wbEncrypted 信封形态".to_string());
    }
    let enc = outer
        .get("envelope")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    let inner = b64_decode(enc).map_err(|_| "信封 base64 损坏".to_string())?;
    let env: WbEnvelope =
        serde_json::from_slice(&inner).map_err(|_| "信封内层 JSON 损坏".to_string())?;
    if env.suite != 1 {
        return Err(format!("不支持的加密 suite={}", env.suite));
    }
    if env.key_id.len() != 16 {
        return Err("信封 keyId 形态异常".to_string());
    }
    Ok(env)
}

// ---------- 取密钥（缓存 + 负缓存） ----------

struct KeyCache {
    keys: Vec<AtrestKey>,
    err: Option<(String, Instant)>,
}

fn cache() -> &'static Mutex<KeyCache> {
    static C: OnceLock<Mutex<KeyCache>> = OnceLock::new();
    C.get_or_init(|| {
        Mutex::new(KeyCache {
            keys: Vec::new(),
            err: None,
        })
    })
}

/// 测试/单测用：清空全局密钥缓存（Go 侧的 `atrestKeys` 包级变量同款，仅测试重置）。
pub fn reset_key_cache() {
    let mut c = lock_cache();
    c.keys.clear();
    c.err = None;
}

fn lock_cache() -> std::sync::MutexGuard<'static, KeyCache> {
    cache().lock().unwrap_or_else(|e| e.into_inner())
}

/// Go `ensureAtrestKeys`：取本机 at-rest 密钥（进程级缓存；region 只决定候选 exe
/// 的尝试顺序，解密时以 `envelope.keyId` 匹配为准，不跨区误用）。
///
/// 与 Go 的差异（登记在 README）：Go 的候选发现与取 key 走同步 `exec.Command`；
/// 本实现拆成「发现（同步）」+「取 secret（逐候选、20s 超时）」，语义一致。
pub fn ensure_atrest_keys(region: Region) -> Result<Vec<AtrestKey>, String> {
    {
        let c = lock_cache();
        if !c.keys.is_empty() {
            return Ok(c.keys.clone());
        }
        if let Some((msg, at)) = &c.err {
            if at.elapsed() < ATREST_ERR_TTL {
                return Err(msg.clone());
            }
        }
    }
    let fail = |msg: String| -> Result<Vec<AtrestKey>, String> {
        let mut c = lock_cache();
        c.err = Some((msg.clone(), Instant::now()));
        Err(msg)
    };
    // 1. 环境变量直接给 secret（逃生口；测试注入同口）。
    if let Ok(secret) = std::env::var("WORKBUDDY_AT_REST_SECRET") {
        if !secret.is_empty() {
            let keys = vec![derive_atrest_key(&secret, "env")];
            let mut c = lock_cache();
            c.keys = keys.clone();
            c.err = None;
            return Ok(keys);
        }
    }
    // 2. 候选 exe 逐个 run-as-node 取 secret。
    let cands = electron_candidates(region);
    if cands.is_empty() {
        return fail("找不到 WorkBuddy 桌面端可执行文件（注册表卸载表无条目），可用 WORKBUDDY_ELECTRON_PATH 指定".to_string());
    }
    let mut keys = Vec::new();
    let mut last_err: Option<String> = None;
    for c in &cands {
        match fetch_atrest_secret(&c.path) {
            Ok(secret) => keys.push(derive_atrest_key(&secret, &c.path)),
            Err(e) => last_err = Some(e),
        }
    }
    if keys.is_empty() {
        let mut msg = "取本机 at-rest 密钥失败".to_string();
        if let Some(e) = last_err {
            msg.push('：');
            msg.push_str(&crate::chat::truncate(&e, 200));
        }
        return fail(msg);
    }
    let mut c = lock_cache();
    c.keys = keys.clone();
    c.err = None;
    Ok(keys)
}

/// Go `fetchAtrestSecret`：对单个 exe 跑 loggerGet 取 `atRestSecretKey`（20s 超时）。
pub fn fetch_atrest_secret(exe: &str) -> Result<String, String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("-e")
        .arg(LOGGER_GET_JS)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let out = run_with_timeout(cmd, Duration::from_secs(20))
        .map_err(|e| format!("{}: {}", exe, e))?;
    let payload: serde_json::Value = serde_json::from_slice(&out)
        .map_err(|_| format!("{}: loggerGet 未返回 atRestSecretKey", exe))?;
    let secret = payload
        .get("atRestSecretKey")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if secret.is_empty() {
        return Err(format!("{}: loggerGet 未返回 atRestSecretKey", exe));
    }
    if b64_decode(secret).is_err() {
        return Err(format!("{}: atRestSecretKey 非法 base64", exe));
    }
    Ok(secret.to_string())
}

/// 带超时地跑一个子进程并收 stdout（Go `exec.CommandContext` + `cmd.Output()` 的对等物）。
///
/// 用 `try_wait` 轮询而不是等 `wait_with_output`：超时后要能杀掉子进程并如实报错
/// （Go 的 CommandContext 到点即 kill）。
fn run_with_timeout(mut cmd: std::process::Command, timeout: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                let mut buf = Vec::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = so.read_to_end(&mut buf);
                }
                return Ok(buf);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("取密钥超时（20s）".to_string());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

// ---------- 解密 / 加密 ----------

/// Go `decryptEnvelopeRaw`：解一个信封 raw（region 只定候选顺序；keyId 匹配才解）。
pub fn decrypt_envelope_raw(raw: &[u8], region: Region) -> Result<String, String> {
    let env = parse_wb_envelope(raw)?;
    let nonce = b64_decode(&env.nonce).map_err(|_| "信封 nonce 异常".to_string())?;
    if nonce.len() != 12 {
        return Err("信封 nonce 异常".to_string());
    }
    let tag = b64_decode(&env.auth_tag).map_err(|_| "信封 authTag 异常".to_string())?;
    if tag.len() != 16 {
        return Err("信封 authTag 异常".to_string());
    }
    let ct = b64_decode(&env.ciphertext).map_err(|_| "信封 ciphertext 异常".to_string())?;
    let keys = ensure_atrest_keys(region)?;
    for k in &keys {
        if k.key_id != env.key_id {
            continue;
        }
        let pt = gcm_open(&k.key, &nonce, &ct, &tag, &aad_for_key_id(&env.key_id))?;
        return String::from_utf8(pt).map_err(|_| "信封解密结果非 UTF-8".to_string());
    }
    Err("信封 keyId 与本机派生不一致（桌面端可能已更换密钥）".to_string())
}

/// Go `DecryptAuthField`：解 `.info` 里的字段值。明文原样回；信封解密；
/// 其他置空不报错（昵称等显示字段沿用旧 flex 语义）。
pub fn decrypt_auth_field(raw: &[u8], region: Region) -> Result<String, String> {
    if let Ok(s) = serde_json::from_slice::<String>(raw) {
        return Ok(s);
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return Ok(String::new());
    };
    if !is_encrypted_field(&v) {
        return Ok(String::new());
    }
    decrypt_envelope_raw(raw, region)
}

/// Go `EncryptLike`：把明文加密回 `$wbEncrypted` 信封：`original_raw` 若为信封则沿用
/// 其 keyId 对应的本机 key（桌面端可读）；明文原文件则用首选 key。返回外层对象。
pub fn encrypt_like(
    original_raw: &[u8],
    plaintext: &str,
    region: Region,
) -> Result<serde_json::Value, String> {
    let keys = ensure_atrest_keys(region)?;
    let mut key = keys[0].clone();
    if !original_raw.is_empty() {
        if let Ok(env) = parse_wb_envelope(original_raw) {
            for k in &keys {
                if k.key_id == env.key_id {
                    key = k.clone();
                    break;
                }
            }
        }
    }
    let mut nonce = [0u8; 12];
    getrandom_fill(&mut nonce)?;
    let sealed = gcm_seal(
        &key.key,
        &nonce,
        plaintext.as_bytes(),
        &aad_for_key_id(&key.key_id),
    )?;
    let (ct, tag) = sealed.split_at(sealed.len() - 16);
    let inner = serde_json::json!({
        "suite": 1,
        "keyId": key.key_id,
        "nonce": b64_encode(&nonce),
        "authTag": b64_encode(tag),
        "ciphertext": b64_encode(ct),
    });
    let ib = serde_json::to_vec(&inner).map_err(|_| "信封组装失败".to_string())?;
    Ok(serde_json::json!({
        "$wbEncrypted": 1,
        "envelope": b64_encode(&ib),
    }))
}

/// Go `desensToken`：脱敏描述（日志/汇报用）：JWT 头部前 20 字＋长度，零原文。
pub fn desens_token(pt: &str) -> String {
    let head: String = pt.chars().take(20).collect();
    format!("JWT头部={}... 长度={}", head, pt.len())
}

// ---------- 低层 ----------

fn cipher_for(key: &[u8; 32]) -> Result<Aes256Gcm, String> {
    Aes256Gcm::new_from_slice(key).map_err(|_| "本地 AES 初始化失败".to_string())
}

fn gcm_open(
    key: &[u8; 32],
    nonce: &[u8],
    ct: &[u8],
    tag: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, String> {
    let cipher = cipher_for(key)?;
    let mut buf = ct.to_vec();
    let nonce = aes_gcm::Nonce::try_from(nonce).map_err(|_| "信封 nonce 异常".to_string())?;
    let tag = aes_gcm::Tag::try_from(tag).map_err(|_| "信封 authTag 异常".to_string())?;
    cipher
        .decrypt_inout_detached(&nonce, aad, buf.as_mut_slice().into(), &tag)
        .map_err(|_| "信封解密失败（文件损坏或桌面端已换密钥）".to_string())?;
    Ok(buf)
}

fn gcm_seal(
    key: &[u8; 32],
    nonce: &[u8; 12],
    pt: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, String> {
    let cipher = cipher_for(key)?;
    let mut buf = pt.to_vec();
    let nonce =
        aes_gcm::Nonce::try_from(nonce.as_slice()).map_err(|_| "nonce 长度异常".to_string())?;
    let tag = cipher
        .encrypt_inout_detached(&nonce, aad, buf.as_mut_slice().into())
        .map_err(|_| "AES-GCM 加密失败".to_string())?;
    buf.extend_from_slice(tag.as_slice());
    Ok(buf)
}

fn sha256(b: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b);
    let out = h.finalize();
    let mut r = [0u8; 32];
    r.copy_from_slice(&out);
    r
}

/// 系统 CSPRNG（Go `crypto/rand.Read` 的对等物）。Windows 上取 `RtlGenRandom`。
fn getrandom_fill(buf: &mut [u8]) -> Result<(), String> {
    #[cfg(windows)]
    {
        // SystemFunction036 = RtlGenRandom（advapi32），Go 在 Windows 上走的就是它。
        #[link(name = "advapi32")]
        extern "system" {
            fn SystemFunction036(buf: *mut u8, len: u32) -> u8;
        }
        let ok = unsafe { SystemFunction036(buf.as_mut_ptr(), buf.len() as u32) };
        if ok == 0 {
            return Err("随机 nonce 失败".to_string());
        }
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        let mut f = std::fs::File::open("/dev/urandom").map_err(|e| format!("随机 nonce 失败: {e}"))?;
        f.read_exact(buf).map_err(|e| format!("随机 nonce 失败: {e}"))?;
        Ok(())
    }
}

fn hex_lower(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 0x0f) as usize] as char);
    }
    s
}

/// 标准 base64（与 Go `base64.StdEncoding` 同表同严格度）。
fn b64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn b64_decode(s: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| ())
}

// ---------- 桌面端 exe 发现 ----------

/// Go `electronCandidates`：返回候选桌面端 exe（region 命中的排前）。
#[cfg(windows)]
pub fn electron_candidates(region: Region) -> Vec<ElectronCand> {
    use windows_registry::{CURRENT_USER, LOCAL_MACHINE};

    if let Ok(p) = std::env::var("WORKBUDDY_ELECTRON_PATH") {
        if !p.is_empty() {
            if is_file(&p) {
                return vec![ElectronCand {
                    path: p,
                    label: region.as_str().to_string(),
                }];
            }
            // 指向目录时按两 exe 名试（CN/INTL 各一）。
            for name in ["WorkBuddy.exe", "WorkBuddyAI.exe"] {
                let fp = std::path::Path::new(&p).join(name);
                if is_file(&fp.to_string_lossy()) {
                    return vec![ElectronCand {
                        path: fp.to_string_lossy().to_string(),
                        label: region.as_str().to_string(),
                    }];
                }
            }
        }
    }
    let mut cands: Vec<ElectronCand> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let sub = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    let sub_wow = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    let mut add = |path: String, label: &str| {
        if path.is_empty() || seen.contains(&path) || !is_file(&path) {
            return;
        }
        seen.push(path.clone());
        cands.push(ElectronCand {
            path,
            label: label.to_string(),
        });
    };
    for root in [LOCAL_MACHINE, CURRENT_USER] {
        for sub in [sub, sub_wow] {
            let Ok(k) = root.open(sub) else { continue };
            let Ok(names) = k.keys() else { continue };
            for n in names {
                let Ok(sk) = root.open(format!("{sub}\\{n}")) else {
                    continue;
                };
                let disp = sk.get_string("DisplayName").unwrap_or_default();
                let icon = sk.get_string("DisplayIcon").unwrap_or_default();
                let uninst = sk.get_string("UninstallString").unwrap_or_default();
                if !disp.to_lowercase().contains("workbuddy") {
                    continue;
                }
                let label = if disp.contains(" AI") || icon.contains("WorkBuddyAI.exe") {
                    "INTL"
                } else {
                    "CN"
                };
                // 首选 DisplayIcon（直指真实 exe），回退 UninstallString 目录。
                let exe = exe_from_icon(&icon);
                if !exe.is_empty() {
                    add(exe, label);
                    continue;
                }
                let dir = dir_from_uninstall(&uninst);
                if !dir.is_empty() {
                    for name in ["WorkBuddyAI.exe", "WorkBuddy.exe"] {
                        add(
                            std::path::Path::new(&dir)
                                .join(name)
                                .to_string_lossy()
                                .to_string(),
                            label,
                        );
                    }
                }
            }
        }
    }
    // region 命中的排前（同区 exe 的 secret 优先试，省一次子进程）。
    let want = region.as_str();
    let mut hit: Vec<ElectronCand> = Vec::new();
    let mut rest: Vec<ElectronCand> = Vec::new();
    for c in cands {
        if c.label == want {
            hit.push(c);
        } else {
            rest.push(c);
        }
    }
    hit.extend(rest);
    hit
}

/// 非 Windows：没有注册表，只认环境变量（对应 Go 侧 `atrest_other.go` 的空实现）。
#[cfg(not(windows))]
pub fn electron_candidates(_region: Region) -> Vec<ElectronCand> {
    Vec::new()
}

/// Go `exeFromIcon`：从 `DisplayIcon`（`"D:\…\x.exe,0"`）提 exe 路径。
pub fn exe_from_icon(icon: &str) -> String {
    if icon.is_empty() {
        return String::new();
    }
    let first = icon.split(',').next().unwrap_or("");
    let p = first.trim().trim_matches('"');
    if p.to_lowercase().ends_with(".exe") && is_file(p) {
        return p.to_string();
    }
    String::new()
}

/// Go `dirFromUninstall`：从 `UninstallString` 提安装目录。
pub fn dir_from_uninstall(uninst: &str) -> String {
    if uninst.is_empty() {
        return String::new();
    }
    let s = uninst.trim();
    let rest: &str = if let Some(stripped) = s.strip_prefix('"') {
        match stripped.find('"') {
            Some(i) => &stripped[..i],
            None => s,
        }
    } else {
        match s.find([' ', '\t', '/']) {
            Some(i) => &s[..i],
            None => s,
        }
    };
    let t = rest.trim_matches('"');
    let d = std::path::Path::new(t)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if d.is_empty() || d == "." {
        return String::new();
    }
    d
}

fn is_file(p: &str) -> bool {
    std::fs::metadata(p).map(|m| !m.is_dir()).unwrap_or(false)
}

/// `regionOfAuth`（Go `credential.rs` 里同名函数的对等物）：只按 INTL 后缀判国际，
/// 其余（含 CN 的新老域名）一律归 CN。
pub fn region_of_auth(bytes: &[u8]) -> Region {
    let probe: serde_json::Value = serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null);
    let domain = probe
        .get("auth")
        .and_then(|a| a.get("domain"))
        .and_then(|d| d.as_str())
        .unwrap_or("");
    if region::contains_domain(domain, REGION_INTL.domain_suffixes) {
        Region::Intl
    } else {
        Region::Cn
    }
}

use crate::region;

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 Go 单测同族的 AAD 形状（Go `atrest_test.go` 的边界断言）。
    #[test]
    fn aad_framing_is_stable() {
        let aad = aad_for_key_id("0123456789abcdef");
        assert!(aad.starts_with(b"WB-AAD\x00\x01"));
        assert_eq!(aad.len(), 8 + 4 + 5 + 4 + 6 + 4 + 4 + 16 + 3);
        assert_eq!(&aad[aad.len() - 3..], &[0x02, 0x00, 0x00]);
        // u32be(5)+"WBEV1" 紧跟 0x01
        assert_eq!(&aad[8..12], &5u32.to_be_bytes());
        assert_eq!(&aad[12..17], b"WBEV1");
        assert_eq!(&aad[17..21], &6u32.to_be_bytes());
        assert_eq!(&aad[21..27], b"sym-v1");
        assert_eq!(&aad[27..31], &1u32.to_be_bytes());
        assert_eq!(&aad[31..35], &16u32.to_be_bytes());
        assert_eq!(&aad[35..51], b"0123456789abcdef");
    }

    #[test]
    fn derive_matches_go_golden_vector() {
        // 由真 Go（internal/codebuddy）采收的派生向量，见黄金的 atrest 段。
        let k = derive_atrest_key("c2VjcmV0LWdvbGRlbi1zZWNyZXQ=", "env");
        assert_eq!(k.key_id.len(), 16);
        assert_eq!(
            k.key_id, "d0944e8ffb9d20ed",
            "keyId 派生与 Go 不一致（sha256(sha256(secret))[:16]）"
        );
    }

    #[test]
    fn encrypt_decrypt_roundtrip_with_injected_secret() {
        std::env::set_var("WORKBUDDY_AT_REST_SECRET", "c2VjcmV0LWdvbGRlbi1zZWNyZXQ=");
        reset_key_cache();
        let env = encrypt_like(b"", "tok-roundtrip", Region::Cn).unwrap();
        let raw = serde_json::to_vec(&env).unwrap();
        let out = decrypt_envelope_raw(&raw, Region::Cn).unwrap();
        assert_eq!(out, "tok-roundtrip");
        std::env::remove_var("WORKBUDDY_AT_REST_SECRET");
        reset_key_cache();
    }

    #[test]
    fn plaintext_field_passes_through_and_junk_becomes_empty() {
        assert_eq!(
            decrypt_auth_field(b"\"tok-plain\"", Region::Cn).unwrap(),
            "tok-plain"
        );
        assert_eq!(
            decrypt_auth_field(b"{\"a\":1}", Region::Cn).unwrap(),
            "",
            "非字符串非信封 → 空串不报错（Go flex 语义）"
        );
        assert_eq!(decrypt_auth_field(b"null", Region::Cn).unwrap(), "");
        assert_eq!(decrypt_auth_field(b"42", Region::Cn).unwrap(), "");
    }

    #[test]
    fn broken_envelope_fails_without_leaking_content() {
        let err = parse_wb_envelope(br#"{"$wbEncrypted":1,"envelope":"not!base64!!"}"#).unwrap_err();
        assert_eq!(err, "信封 base64 损坏");
        assert!(!err.contains("not!base64"), "错误信息不得夹带信封内容");
        // suite 非 1
        let inner = b64_encode(br#"{"suite":2,"keyId":"0123456789abcdef","nonce":"","authTag":"","ciphertext":""}"#);
        let raw = format!(r#"{{"$wbEncrypted":1,"envelope":"{inner}"}}"#);
        assert!(parse_wb_envelope(raw.as_bytes())
            .unwrap_err()
            .contains("不支持的加密 suite=2"));
    }

    #[test]
    fn desens_token_never_returns_full_token() {
        let d = desens_token("abcdefghijklmnopqrstuvwxyz");
        assert_eq!(d, "JWT头部=abcdefghijklmnopqrst... 长度=26");
        assert!(!d.contains("uvwxyz"), "脱敏描述不得含尾部原文");
    }
}
