//! credential.rs —— WorkBuddy / CodeBuddy 桌面端登录态（真实凭据层），对应 Go
//! `internal/codebuddy/credential.go`。
//!
//! 流程（与 Go 逐条对齐）：
//! 1. 按区域过滤扫描 auth 目录，**可用性优先**（明文非空 / `$wbEncrypted` 信封可解密），
//!    命中区域且 token 可用的文件里取 mtime 最新一个；
//! 2. `loadIfStale`：文件 mtime 变了才重读 + 解密（信封解密不在每请求重复做）；
//! 3. `headers_for(path)`：切换账号（池选号）与读取在同一把锁内完成，必要时先刷新 token；
//! 4. `summary()`：`/health` 的诊断块（uid/昵称/企业/token 过期状态/文件路径）。
//!
//! **凭据纪律（简报红线）**：`.info` 文件只读、不落盘、不回写（除 token 刷新这条与 Go
//! 同源的路径）、不打印；所有错误只给文件名与形态，绝不夹带 token / secret / envelope。
//! 测试一律用 `CODEBUDDY_AUTH_DIR` 指向的临时目录里的**合成假文件**。
//!
//! 与 Go 的已知差异（登记在 README）：Go 的 `Credential` 是单实例多方法调用的可变结构；
//! Rust 侧把「当前文件 + 缓存」装进 `Mutex`。**`headers_for` 是一把锁罩全程**——切文件 →
//! 读 → 必要时刷新（含网络）→ 建头全在同一把锁内，逐条对齐 Go `credential.go:552-586`
//! （Go 的注释明言：拆锁会让「并发请求各持不同账号时互相串号」）。行尾四条并发行为
//! 证据见 `tests/p5b_refresh.rs`。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};

use crate::atrest;
use crate::cred::{CredSource, CredSummary};
use crate::region::{contains_domain, Region};

/// Go `defaultDomain`。
pub const DEFAULT_DOMAIN: &str = "www.codebuddy.cn";

/// Go `findAuthDirs`：候选 auth 目录。`CODEBUDDY_AUTH_DIR` 命中即唯一（逃生口）。
///
/// 与 Go 的差异（登记）：Go 的 `os.UserHomeDir()` 在 Windows 上依次看 `USERPROFILE` /
/// `HOMEDRIVE`+`HOMEPATH`；Rust 侧照抄这一顺序（不引入 dirs crate）。
pub fn find_auth_dirs() -> Vec<String> {
    if let Ok(d) = std::env::var("CODEBUDDY_AUTH_DIR") {
        if !d.is_empty() {
            return vec![d];
        }
    }
    let home = user_home_dir();
    let local_app_data = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let xdg = std::env::var("XDG_DATA_HOME").unwrap_or_default();
    candidate_auth_dirs(&local_app_data, &home, &xdg)
}

fn user_home_dir() -> String {
    if let Ok(p) = std::env::var("USERPROFILE") {
        if !p.is_empty() {
            return p;
        }
    }
    let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
    let path = std::env::var("HOMEPATH").unwrap_or_default();
    if !drive.is_empty() || !path.is_empty() {
        return format!("{drive}{path}");
    }
    std::env::var("HOME").unwrap_or_default()
}

/// Go `candidateAuthDirs`（纯函数：按传入的根拼多候选目录，去重、去空）。
pub fn candidate_auth_dirs(local_app_data: &str, home: &str, xdg: &str) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    let mut add = |d: String| {
        if d.is_empty() || dirs.contains(&d) {
            return;
        }
        dirs.push(d);
    };
    let mut lad = local_app_data.to_string();
    if lad.is_empty() && !home.is_empty() {
        lad = join(home, &["AppData", "Local"]);
    }
    if !lad.is_empty() {
        add(join(
            &lad,
            &["CodeBuddyExtension", "Data", "Public", "auth"],
        ));
    }
    if !home.is_empty() {
        add(join(
            home,
            &[
                "Library",
                "Application Support",
                "CodeBuddyExtension",
                "Data",
                "Public",
                "auth",
            ],
        ));
    }
    let mut x = xdg.to_string();
    if x.is_empty() && !home.is_empty() {
        x = join(home, &[".local", "share"]);
    }
    if !x.is_empty() {
        add(join(&x, &["CodeBuddyExtension", "Data", "Public", "auth"]));
    }
    dirs
}

/// 按目录分隔符拼路径（Go `filepath.Join` 的语义：不做盘符判断，只拼）。
fn join(base: &str, parts: &[&str]) -> String {
    let mut out = base.trim_end_matches(['/', '\\']).to_string();
    for p in parts {
        if out.is_empty() {
            out = (*p).to_string();
        } else {
            out.push(std::path::MAIN_SEPARATOR);
            out.push_str(p);
        }
    }
    out
}

// ---------- auth 文件读取 ----------

/// Go `sessionInfo`（宽松解析：只读本层需要的字段）。
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub access_raw: Vec<u8>,
    pub refresh_raw: Vec<u8>,
    pub access_encrypted: bool,
    pub refresh_encrypted: bool,
    pub domain: String,
    pub expires_at: i64,
    pub refresh_expires_at: i64,
    pub uid: String,
    pub nickname: String,
    pub enterprise_id: String,
    pub enterprise_name: String,
}

/// `flexString` 语义：字符串原样收；对象/数组/数字/null 一律收成空串（不报错）。
fn flex_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// `wbToken` 语义：字符串原样收；信封记形态；其余（数字/null/数组）置空不报错。
fn wb_token_field(v: Option<&Value>) -> (Vec<u8>, bool) {
    match v {
        Some(Value::String(s)) => (serde_json::to_vec(s).unwrap_or_default(), false),
        Some(other) if atrest::is_encrypted_field(other) => (
            serde_json::to_vec(other).unwrap_or_default(),
            true,
        ),
        _ => (Vec::new(), false),
    }
}

/// 解析 `.info` 的顶层结构（Go `sessionInfo` + `wbToken.UnmarshalJSON` 的合成语义）。
pub fn parse_session_info(bytes: &[u8]) -> Result<SessionInfo, String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("auth 文件格式错误: {e}"))?;
    let auth = v.get("auth");
    let acct = v.get("account");
    let (access_raw, access_encrypted) = wb_token_field(auth.and_then(|a| a.get("accessToken")));
    let (refresh_raw, refresh_encrypted) =
        wb_token_field(auth.and_then(|a| a.get("refreshToken")));
    Ok(SessionInfo {
        access_raw,
        refresh_raw,
        access_encrypted,
        refresh_encrypted,
        domain: auth
            .and_then(|a| a.get("domain"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
        expires_at: auth
            .and_then(|a| a.get("expiresAt"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0),
        refresh_expires_at: auth
            .and_then(|a| a.get("refreshExpiresAt"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0),
        uid: acct
            .and_then(|a| a.get("uid"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
        nickname: flex_string(acct.and_then(|a| a.get("nickname"))),
        enterprise_id: acct
            .and_then(|a| a.get("enterpriseId"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
        enterprise_name: acct
            .and_then(|a| a.get("enterpriseName"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Go `authFileDomainMatches`：读 auth 文件的 `auth.domain` 判断是否命中区域后缀。
pub fn auth_file_domain_matches(path: &Path, suffixes: &[&str]) -> bool {
    let Ok(b) = std::fs::read(path) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&b) else {
        return false;
    };
    let domain = v
        .get("auth")
        .and_then(|a| a.get("domain"))
        .and_then(|d| d.as_str())
        .unwrap_or("");
    contains_domain(domain, suffixes)
}

/// Go `authTokenUnusableReason`：不可用时返回短原因（"" = 可用；扫盘日志用，不带原文）。
pub fn auth_token_unusable_reason(path: &Path) -> String {
    let Ok(b) = std::fs::read(path) else {
        return "读文件失败".to_string();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&b) else {
        return "auth.accessToken 缺失".to_string();
    };
    let Some(raw) = v.get("auth").and_then(|a| a.get("accessToken")) else {
        return "auth.accessToken 缺失".to_string();
    };
    if raw.is_null() {
        return "auth.accessToken 缺失".to_string();
    }
    if let Some(s) = raw.as_str() {
        return if s.is_empty() {
            "明文 token 为空".to_string()
        } else {
            String::new()
        };
    }
    if !atrest::is_encrypted_field(raw) {
        return "token 形态未知".to_string();
    }
    let raw_bytes = serde_json::to_vec(raw).unwrap_or_default();
    if let Err(e) = atrest::parse_wb_envelope(&raw_bytes) {
        return format!("加密信封损坏({e})");
    }
    match atrest::decrypt_envelope_raw(&raw_bytes, atrest::region_of_auth(&b)) {
        Err(e) => format!("加密 token 解密失败({e})"),
        Ok(pt) if pt.is_empty() => "加密 token 解密后为空".to_string(),
        Ok(_) => String::new(),
    }
}

/// Go `authTokenUsable`。
pub fn auth_token_usable(path: &Path) -> bool {
    auth_token_unusable_reason(path).is_empty()
}

/// Go `countEncryptedFiles`：该区域下「可解析、区域对、但 token 不可用」的文件数。
pub fn count_encrypted_files(region: Region) -> usize {
    let suffixes = region.cfg().domain_suffixes;
    let mut n = 0;
    for d in find_auth_dirs() {
        let Ok(ents) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in ents.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() || !name.ends_with(".info") {
                continue;
            }
            let p = e.path();
            if auth_file_domain_matches(&p, suffixes) && !auth_token_usable(&p) {
                n += 1;
            }
        }
    }
    n
}

/// Go `FindAuthFileForRegion`：按区域过滤 + 可用性优先 + 同目录内 mtime 最新。
///
/// 零可用时如实区分「一个文件都没有」与「有文件但全解不出来」，并把搜索过的目录
/// 写进日志（`log::warn` 语义由 `println!` 承担，与 P5a 起全 crate 的口径一致）。
pub fn find_auth_file_for_region(region: Region) -> String {
    let suffixes = region.cfg().domain_suffixes;
    let dirs = find_auth_dirs();
    let mut encrypted = 0usize;
    for d in &dirs {
        let Ok(ents) = std::fs::read_dir(d) else {
            continue;
        };
        let mut files: Vec<(String, std::time::SystemTime)> = Vec::new();
        for e in ents.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() || !name.ends_with(".info") {
                continue;
            }
            let Ok(info) = e.metadata() else { continue };
            let p = e.path();
            if !auth_file_domain_matches(&p, suffixes) {
                continue;
            }
            if !auth_token_usable(&p) {
                encrypted += 1;
                println!(
                    "[codebuddy] 跳过不可用登录文件 {}（{}，用最后可用文件）",
                    name,
                    auth_token_unusable_reason(&p)
                );
                continue;
            }
            let mtime = info.modified().unwrap_or(std::time::UNIX_EPOCH);
            files.push((p.to_string_lossy().to_string(), mtime));
        }
        if !files.is_empty() {
            files.sort_by(|a, b| b.1.cmp(&a.1));
            return files[0].0.clone();
        }
    }
    if encrypted > 0 {
        println!(
            "[codebuddy] {} 有 {} 个登录文件但 token 均不可用（加密解不出来或已损坏），已搜索目录：{}",
            region.as_str(),
            encrypted,
            dirs.join("; ")
        );
    } else {
        println!(
            "[codebuddy] 未找到 {} 登录文件，已搜索目录：{}",
            region.as_str(),
            dirs.join("; ")
        );
    }
    String::new()
}

// ---------- Credential ----------

/// 凭据缓存的内部状态（`Mutex` 保护，等价于 Go 的 `mu` + 字段）。
#[derive(Default)]
struct Inner {
    path: String,
    cached: Option<Map<String, Value>>,
    mtime: Option<std::time::SystemTime>,
    dec_access: String,
    dec_refresh: String,
}

/// Go `Credential`。
pub struct RealCredential {
    region: Region,
    /// 刷新端点所在的 base URL（Go 用区域常量 `regionConfigs[c.region].BaseURL`）。
    /// 留一个构造参数给对照/回放：刷新路径要打本地假上游，黄金回放靠它。
    base_url: String,
    inner: Mutex<Inner>,
}

impl RealCredential {
    /// Go `NewCredential`：`path` 为空则按 region 自动查找。
    pub fn new(path: &str, region: Region) -> RealCredential {
        RealCredential::with_base_url(path, region, region.cfg().base_url)
    }

    /// 同 `new`，但显式指定刷新端点 base URL（黄金回放 / 测试用）。
    pub fn with_base_url(path: &str, region: Region, base_url: &str) -> RealCredential {
        let p = if path.is_empty() {
            find_auth_file_for_region(region)
        } else {
            path.to_string()
        };
        RealCredential {
            region,
            base_url: base_url.to_string(),
            inner: Mutex::new(Inner {
                path: p,
                ..Default::default()
            }),
        }
    }

    /// 当前使用的 auth 文件路径（Go `Path()`）。
    pub fn path(&self) -> String {
        lock(&self.inner).path.clone()
    }

    /// Go `loadIfStale`：文件 mtime 变了才重读 + 解密。**须已持锁**（与 Go 同款契约）。
    fn load_if_stale(&self, g: &mut Inner) -> Result<(), String> {
        let path = g.path.clone();
        let st = std::fs::metadata(&path).map_err(|e| format!("无法读取 auth 文件 {path}: {e}"))?;
        let mtime = st.modified().unwrap_or(std::time::UNIX_EPOCH);
        if g.cached.is_some() && g.mtime == Some(mtime) {
            return Ok(());
        }
        let b = std::fs::read(&path).map_err(|e| e.to_string())?;
        let m: Map<String, Value> =
            serde_json::from_slice(&b).map_err(|e| format!("auth 文件格式错误: {e}"))?;
        g.cached = Some(m);
        g.mtime = Some(mtime);
        g.dec_access = String::new();
        g.dec_refresh = String::new();
        let s = session_of(g)?;
        let access = resolve_token(&s.access_raw, s.access_encrypted, self.region).map_err(|e| {
            format!(
                "登录文件 {} 的加密 token 无法解密（{}）",
                base_name(&path),
                e
            )
        })?;
        let refresh =
            resolve_token(&s.refresh_raw, s.refresh_encrypted, self.region).map_err(|e| {
                format!(
                    "登录文件 {} 的加密 token 无法解密（{}）",
                    base_name(&path),
                    e
                )
            })?;
        g.dec_access = access;
        g.dec_refresh = refresh;
        if s.access_encrypted || s.refresh_encrypted {
            println!(
                "[codebuddy] at-rest 加密登录态解密成功：文件={} {}",
                base_name(&path),
                atrest::desens_token(&g.dec_access)
            );
        }
        Ok(())
    }

    /// Go `session()`：从缓存的 map 中解析结构化信息（**须已持锁**版本）。
    fn session_locked(g: &Inner) -> Result<SessionInfo, String> {
        session_of(g)
    }

    /// Go `headersFor`：**单锁罩全程**——切文件 → 读 → 必要时刷新（含网络）→ 建头，
    /// 全在同一把锁内完成。
    ///
    /// 为什么不能分段锁（Go 原文，`credential.go:544-546`）：切换与读取必须在同一次
    /// 加锁内完成，拆成「SetPath + 再取一次锁读」会让并发请求各持不同账号时互相串号；
    /// 刷新若在锁外跑，两个并发请求还会双花同一个 refresh token、并把刷新结果回写到
    /// 别人的 auth 文件。行为证据：`tests/p5b_refresh.rs`（两线程异号并发，各自只拿到
    /// 自己号的 token，且没有任何跨号文件读/写）。
    fn headers_for(&self, path: &str) -> Result<HeaderMap, String> {
        let mut g = lock(&self.inner);
        if !path.is_empty() && path != g.path {
            g.path = path.to_string();
            g.cached = None;
            g.mtime = None;
        }
        self.load_if_stale(&mut g)?;
        let mut s = Self::session_locked(&g)?;
        // 兜底：不可用文件绝不允许走到出站（选文件层已过滤；显式路径传入时在这里拦）。
        if g.dec_access.is_empty() {
            return Err(format!(
                "登录文件 {} 的加密 token 无法解密（解密后为空）",
                base_name(&g.path)
            ));
        }
        if self.expired(&s) {
            if self.region.cfg().no_refresh {
                return Err(
                    "国际版 token 已过期（桌面端会自动续期，请打开 WorkBuddy 桌面端刷新登录态）"
                        .to_string(),
                );
            }
            self.refresh(&mut g, &s)?;
            s = Self::session_locked(&g)?;
        }
        let access = g.dec_access.clone();
        Ok(self.build_headers(&s, &access))
    }

    /// Go `expired`：提前 60s 判定过期。
    fn expired(&self, s: &SessionInfo) -> bool {
        now_millis() >= s.expires_at - 60_000
    }

    /// Go `refresh`：调后端刷新 token，写回 auth 文件与缓存（形态保持）。
    ///
    /// **须已持锁**：Go 的 `refresh` 在 `headersFor` 的锁内跑（含网络段），本实现同样如此。
    /// 持锁期间做网络 IO 是刻意的——Go 就是这么序列化的（见 `credential.go:544-546` 的
    /// 「拆锁会串号」警告），并发请求宁可排队也不许串号/双花 refresh token。
    ///
    /// HTTP 走**每次刷新一个一次性 OS 线程**（线程内建 async 客户端 + current-thread
    /// runtime）：`reqwest::blocking::Client` 在 async handler 内首调必 panic
    /// （`Cannot drop a runtime in a context where blocking is not allowed`，tokio
    /// 1.53.3 `blocking/shutdown.rs:51`，debug 断言路径）。该结论来自一次性探针，
    /// 探针已删、原文留档见 `.work/rust-rewrite-r5-evidence.md` 第 1 节；
    /// 常驻用例只覆盖正向面（`tests/p5b_refresh.rs::refresh_http_path_runs_inside_async_handler`
    /// 与文件头注释），故刷新路径上不再有 blocking 客户端。
    fn refresh(&self, g: &mut Inner, s: &SessionInfo) -> Result<(), String> {
        let path = g.path.clone();
        let refresh_token = resolve_token(&s.refresh_raw, s.refresh_encrypted, self.region)
            .map_err(|e| {
                format!(
                    "登录文件 {} 的加密 token 无法解密（{}）",
                    base_name(&path),
                    e
                )
            })?;
        let access = g.dec_access.clone();
        let mut h = self.build_headers(s, &access);
        set_header(&mut h, "X-Refresh-Token", &refresh_token)?;
        set_header(&mut h, "X-Auth-Refresh-Source", "plugin")?;
        let url = format!("{}/v2/plugin/auth/token/refresh", self.base_url);
        let body = refresh_post_blocking(url, h, "{}".to_string())?;
        let out: Value = serde_json::from_str(&body).map_err(|_| {
            // 与 Go 的**已知差异**：Go `json.Unmarshal` 失败时 `out.Code` 为零值 0、`out.Data`
            // 为 `nil`，于是文案是 `刷新 token 失败(code=0): ` 且**没有尾巴**（`len(out.Data)==0`
            // 走同一分支）；这里把整份响应体当尾巴截 200 字给出（信息更多，但不是逐字同源）。
            // 登记在 README 差异 22 一族。
            format!(
                "刷新 token 失败(code=0): {}",
                crate::chat::truncate(&body, 200)
            )
        })?;
        let code = out.get("code").and_then(|x| x.as_i64()).unwrap_or(0);
        let data = out.get("data").cloned().unwrap_or(Value::Null);
        // 与 Go 的**已知差异**（登记在 README 差异 22 一族，本轮未对齐）：Go 的 `out.Data`
        // 是 `json.RawMessage` ⇒ 报错文案里夹的是**上游原字节**；这里把 `data` 解析成了
        // `Value`，文案里的 `data_text` 是按键序重排后的形态。两者只在「刷新失败且
        // `data` 是对象」时可见（真实上游对象键序偶然一致，故门禁没抓到）。
        let data_text = match &data {
            Value::Null => String::new(),
            v => crate::gjson::to_go(v),
        };
        if code != 0 || data.is_null() {
            return Err(format!(
                "刷新 token 失败(code={}): {}",
                code,
                crate::chat::truncate(&data_text, 200)
            ));
        }
        let Value::Object(mut new_auth) = data else {
            return Err(format!(
                "刷新响应异常: data 非对象（{}）",
                crate::chat::truncate(&data_text, 200)
            ));
        };
        if new_auth.get("domain").is_none() && !s.domain.is_empty() {
            new_auth.insert("domain".to_string(), Value::String(s.domain.clone()));
        }
        new_auth.insert(
            "lastRefreshTime".to_string(),
            Value::Number(serde_json::Number::from_f64(now_millis() as f64).unwrap()),
        );
        fill_expires(&mut new_auth, s.expires_at);
        // 加密回写：原文件 token 是信封形态才加密（形态保持，桌面端可读）。
        let new_access = new_auth
            .get("accessToken")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let new_refresh = new_auth
            .get("refreshToken")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if s.access_encrypted || s.refresh_encrypted {
            if !new_access.is_empty() {
                let enc = atrest::encrypt_like(&s.access_raw, &new_access, self.region).map_err(
                    |e| format!("刷新成功但加密回写失败（{e}），未动登录文件"),
                )?;
                new_auth.insert("accessToken".to_string(), enc);
            }
            if !new_refresh.is_empty() {
                let enc = atrest::encrypt_like(&s.refresh_raw, &new_refresh, self.region).map_err(
                    |e| format!("刷新成功但加密回写失败（{e}），未动登录文件"),
                )?;
                new_auth.insert("refreshToken".to_string(), enc);
            }
        }
        if !new_access.is_empty() {
            g.dec_access = new_access;
        }
        if !new_refresh.is_empty() {
            g.dec_refresh = new_refresh;
        }
        if let Some(c) = g.cached.as_mut() {
            c.insert("auth".to_string(), Value::Object(new_auth));
        }
        // 原子写回（Go：MarshalIndent 两空格 + tmp+rename）
        let body = {
            let cached = g
                .cached
                .clone()
                .ok_or_else(|| "凭据缓存缺失".to_string())?;
            crate::gjson::to_go_indent(&Value::Object(cached), 2).into_bytes()
        };
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, &body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        if let Ok(st) = std::fs::metadata(&path) {
            if let Ok(mt) = st.modified() {
                g.mtime = Some(mt);
            }
        }
        Ok(())
    }

    /// Go `buildHeaders`：构造后端请求头（access 取已解密的明文）。
    pub fn build_headers(&self, s: &SessionInfo, access: &str) -> HeaderMap {
        let domain = if s.domain.is_empty() {
            DEFAULT_DOMAIN
        } else {
            s.domain.as_str()
        };
        let mut h = HeaderMap::new();
        let _ = set_header(&mut h, "Content-Type", "application/json");
        let _ = set_header(&mut h, "Accept", "application/json");
        let _ = set_header(&mut h, "Authorization", &format!("Bearer {access}"));
        let _ = set_header(&mut h, "X-User-Id", &s.uid);
        let _ = set_header(&mut h, "X-Enterprise-Id", &s.enterprise_id);
        let _ = set_header(&mut h, "X-Tenant-Id", &s.enterprise_id);
        let _ = set_header(&mut h, "X-Domain", domain);
        // User-Agent 按区域取：CN 沿用原常量值；INTL 用官方 CLI 形（实测生效）
        let _ = set_header(&mut h, "User-Agent", self.region.cfg().user_agent);
        h
    }
}

fn base_name(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string())
}

/// 刷新 token 的同步 POST：**一次性 OS 线程 + 该线程内的 async 客户端**。
///
/// 为什么不是 `reqwest::blocking`（实测证据见 `tests/p5b_refresh.rs`）：blocking 客户端
/// 在 async handler 内首调必 panic——tokio 1.53.3 的 debug 断言会在调用线程上建/拆一个
/// 嵌套 runtime，`blocking/shutdown.rs:51` 抛
/// `Cannot drop a runtime in a context where blocking is not allowed`。
/// 新起的 OS 线程不在任何 async 上下文里，建 runtime 合法；客户端随线程一起销毁，
/// 也不会在调用方线程上析构 runtime（`Arc` 引用就够，无需 join）。
///
/// 超时口径与 Go `&http.Client{Timeout: 20s}` 一致：20s 是整个请求（含读响应体）的预算。
fn refresh_post_blocking(
    url: String,
    headers: HeaderMap,
    body: String,
) -> Result<String, String> {
    let (tx, rx) = mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        let out = (|| -> Result<String, String> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("刷新 token 网络失败: {e}"))?;
            rt.block_on(async move {
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .build()
                    .map_err(|e| format!("刷新 token 网络失败: {e}"))?;
                client
                    .post(&url)
                    .headers(headers)
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| format!("刷新 token 网络失败: {e}"))?
                    .text()
                    .await
                    .map_err(|e| format!("刷新 token 网络失败: {e}"))
            })
        })();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(Duration::from_secs(25)) {
        Ok(r) => r,
        Err(_) => Err("刷新 token 网络失败: 超时".to_string()),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn session_of(g: &Inner) -> Result<SessionInfo, String> {
    let cached = g
        .cached
        .as_ref()
        .ok_or_else(|| "auth 文件结构异常: 缓存为空".to_string())?;
    let bytes = crate::gjson::to_go(&Value::Object(cached.clone()));
    parse_session_info(bytes.as_bytes())
        .map_err(|e| format!("auth 文件结构异常: {e}"))
}

fn resolve_token(raw: &[u8], encrypted: bool, region: Region) -> Result<String, String> {
    if !encrypted {
        return Ok(serde_json::from_slice::<String>(raw).unwrap_or_default());
    }
    atrest::decrypt_envelope_raw(raw, region)
}

fn set_header(h: &mut HeaderMap, name: &str, value: &str) -> Result<(), String> {
    let n = HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?;
    let v = HeaderValue::from_str(value).map_err(|e| e.to_string())?;
    h.insert(n, v);
    Ok(())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Go `fillExpires`：缺 `expiresAt` 时按 `expiresIn` 推算（刷新失败保护）。
fn fill_expires(auth: &mut Map<String, Value>, old_expires_at: i64) {
    let now = now_millis();
    if auth.get("expiresAt").is_none() {
        if let Some(v) = auth.get("expiresIn").and_then(|x| x.as_f64()) {
            auth.insert(
                "expiresAt".to_string(),
                Value::Number(serde_json::Number::from_f64(now as f64 + v * 1000.0).unwrap()),
            );
        } else if old_expires_at > 0 {
            auth.insert(
                "expiresAt".to_string(),
                Value::Number(serde_json::Number::from_f64(old_expires_at as f64).unwrap()),
            );
        }
    }
    if auth.get("refreshExpiresAt").is_none() {
        if let Some(v) = auth.get("refreshExpiresIn").and_then(|x| x.as_f64()) {
            auth.insert(
                "refreshExpiresAt".to_string(),
                Value::Number(serde_json::Number::from_f64(now as f64 + v * 1000.0).unwrap()),
            );
        }
    }
}

/// Go `credentialDomain`：取凭据 `auth.domain`（仅该字段；token 等不读不落）。
pub fn credential_domain(path: &str) -> String {
    let Ok(b) = std::fs::read(path) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&b) else {
        return String::new();
    };
    v.get("auth")
        .and_then(|a| a.get("domain"))
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string()
}

// ---------- trait 实现 ----------

impl CredSource for RealCredential {
    fn auth_dirs(&self) -> Vec<String> {
        find_auth_dirs()
    }

    fn summary(&self) -> Option<CredSummary> {
        // 与 Go `Summary()` 同款：整段在一把锁内（Go `credential.go:589-591` 也是
        // `mu.Lock()` + `defer Unlock` 罩住 `loadIfStale` + `session`）。
        let mut g = lock(&self.inner);
        let path = g.path.clone();
        if path.is_empty() {
            let dirs = find_auth_dirs().join("; ");
            let n = count_encrypted_files(self.region);
            if n > 0 {
                return Some(CredSummary::Err(format!(
                    "找到 {} 区域 {} 个登录文件但 token 均为新版加密形态（桌面端升级后加密存储，代理无法解密），已搜索目录：{}",
                    self.region.as_str(),
                    n,
                    dirs
                )));
            }
            return Some(CredSummary::Err(format!(
                "未找到登录文件，已搜索目录：{dirs}"
            )));
        }
        if let Err(e) = self.load_if_stale(&mut g) {
            return Some(CredSummary::Err(e));
        }
        let s = match Self::session_locked(&g) {
            Ok(s) => s,
            Err(e) => return Some(CredSummary::Err(e)),
        };
        let expired = self.expired(&s);
        let pairs: Vec<(String, Value)> = vec![
            ("uid".to_string(), Value::String(s.uid.clone())),
            (
                "nickname".to_string(),
                Value::String(s.nickname.clone()),
            ),
            (
                "enterpriseName".to_string(),
                Value::String(s.enterprise_name.clone()),
            ),
            (
                "token_expires_at".to_string(),
                Value::Number(serde_json::Number::from_f64(s.expires_at as f64).unwrap()),
            ),
            ("token_expired".to_string(), Value::Bool(expired)),
            ("auth_file".to_string(), Value::String(path)),
        ];
        let mut m: BTreeMap<String, Value> = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k, v);
        }
        Some(CredSummary::Ok(m.into_iter().collect()))
    }

    fn headers(&self) -> Result<HeaderMap, String> {
        self.headers_for("")
    }

    fn headers_for(&self, path: &str) -> Result<HeaderMap, String> {
        self.headers_for(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_dirs_match_go() {
        let d = candidate_auth_dirs("C:\\LAD", "C:\\Home", "C:\\xdg");
        assert_eq!(d.len(), 3);
        assert!(d[0].contains("CodeBuddyExtension"));
        assert!(d[0].starts_with("C:\\LAD"));
        assert!(d[1].contains("Library"));
        assert!(d[2].starts_with("C:\\xdg"));
        // LOCALAPPDATA 缺失 → 从 home 推；xdg 缺失 → 从 home 推
        let d2 = candidate_auth_dirs("", "C:\\Home", "");
        assert_eq!(d2.len(), 3);
        assert!(d2[0].contains("AppData"));
        assert!(d2[2].contains(".local"));
        // 全空 → 零候选
        assert!(candidate_auth_dirs("", "", "").is_empty());
    }

    #[test]
    fn flex_string_and_token_forms_match_go() {
        let sess = parse_session_info(
            br#"{"auth":{"accessToken":{"$wbEncrypted":1,"envelope":"x"},"domain":"d","expiresAt":1},
                 "account":{"uid":"u","nickname":{"$wbEncrypted":1,"envelope":"n"}}}"#,
        )
        .unwrap();
        assert!(sess.access_encrypted, "信封形态要记账");
        assert_eq!(sess.nickname, "", "昵称是加密对象 → 空串，不得整份解析失败");
        assert_eq!(sess.uid, "u");
        let s2 = parse_session_info(br#"{"auth":{"accessToken":"tok"},"account":{"nickname":123}}"#)
            .unwrap();
        assert!(!s2.access_encrypted);
        assert_eq!(s2.nickname, "");
    }

    #[test]
    fn unusable_reasons_match_go_wording() {
        let dir = std::env::temp_dir().join("rs-codebuddy-cred-reasons");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cases: Vec<(&str, &str, &str)> = vec![
            ("missing_token", r#"{"auth":{}}"#, "auth.accessToken 缺失"),
            ("empty_plain", r#"{"auth":{"accessToken":""}}"#, "明文 token 为空"),
            (
                "odd_form",
                r#"{"auth":{"accessToken":42}}"#,
                "token 形态未知",
            ),
        ];
        for (name, body, want) in cases {
            let p = dir.join(format!("{name}.info"));
            std::fs::write(&p, body).unwrap();
            assert_eq!(auth_token_unusable_reason(&p), want, "[{name}]");
        }
        let good = dir.join("good.info");
        std::fs::write(&good, r#"{"auth":{"accessToken":"tok"}}"#).unwrap();
        assert_eq!(auth_token_unusable_reason(&good), "");
        assert!(auth_token_usable(&good));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
