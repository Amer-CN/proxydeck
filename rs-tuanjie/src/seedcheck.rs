//! seedcheck.rs —— 种子卫兵（`internal/tuanjie/seedcheck.go` 的逐条对应物）。
//!
//! 两条独立轨道都盯（只盯 npm 会漏桌面端先轮换种子那种情形）：
//!
//! 1. **本机发行件层**：npm 全局安装的 `bundle/gemini.js` + 桌面端内置的
//!    `cli/bin/win32-x64/codely.exe`（路径取自卸载表项 `InstallLocation`，
//!    见 [`crate::winreg`]），任一文件「在但特征串消失」即告警；
//! 2. **在线层**：查 npm registry 的 `dist-tags.latest`，**版本号没变零下载**；
//!    变了（或重启后首验）才下 `.tgz` 解出 `bundle/gemini.js` 做同样的特征核验。
//!
//! ## 与 Go 的等价性
//!
//! * 状态机（`seedAlert` 六个字段 + 三态 online）逐条对应，`/health` 的
//!   `seed_alert/seed_signal/seed_latest/seed_online` 由本模块快照；
//! * 扫描口径（流式分块 + `needle.len()-1` 重叠）与 Go 一致，**只是分块大小不同**
//!   （1 MiB 对 1 MiB，同值）；
//! * `labelDesktopSources` / `installHint` / `evalSeedScan` / `combineSeedAlerts` /
//!   `scanCliSources` 都是纯函数，黄金门禁逐值锁死。
//!
//! ## 差异（登记）
//!
//! 1. **tar 层**：Go 用 `archive/tar`，Rust 用 [`crate::gziptar`] 的 uSTAR 只读实现
//!    （不校验 tar 头校验和、不支持稀疏文件/多卷）——真实 npm 包不受影响，
//!    见该模块头注释的覆盖面说明；
//! 2. **注册表读取**走 `reg.exe` 子进程（`crate::winreg`），非 Windows API；
//!    判据等价，细节见该模块头注释；
//! 3. Go 的 `readCoworkInstallLocation` 是包级**可注入变量**（测试替身），Rust 侧
//!    同样把注册表读入做成可注入参数（[`run_seed_guard_check_with`]），
//!    生产路径不带注入。

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::gziptar;
use crate::sign::CODEY_SIGNING_SEED_HEX;

/// 官方 CLI 签名头名（`seedcheck.go:42`）——发行件里只做存在性核对。
pub const SEED_FEATURE_HEADER_NAME: &str = "X-Codely-Signature";

/// npm registry packument 地址（`seedcheck.go:46`）。
pub const SEED_REGISTRY_PACKUMENT_URL: &str = "https://registry.npmjs.org/@unity-china/codely-cli";

/// 包内特征文件相对路径（`seedcheck.go:47`）。
pub const SEED_BUNDLE_REL_PATH: &str = "bundle/gemini.js";

/// registry 查询超时（`seedcheck.go:48`）。
pub const SEED_REGISTRY_TIMEOUT: Duration = Duration::from_secs(15);
/// tgz 下载兜底超时（`seedcheck.go:49`，16MB 级慢网也能完成）。
pub const SEED_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// packument 响应读全上限（`seedcheck.go:50`）。
pub const SEED_REGISTRY_BODY_LIMIT: u64 = 4 << 20;
/// 单版本文档响应上限（`seedcheck.go:51`）。
pub const SEED_VERSION_DOC_LIMIT: u64 = 1 << 20;
/// tarball 下载/解包上限（`seedcheck.go:52`）。
pub const SEED_TARBALL_MAX_BYTES: u64 = 64 << 20;

/// 分块扫描的块大小（`seedcheck.go:202` 的 `chunkSize`）。
const CHUNK_SIZE: usize = 1 << 20;

// ---------------------------------------------------------------- 状态

/// 在线层状态字 —— `seedAlert.online`（`seedcheck.go:70`）。
pub const ONLINE_UNCHECKED: &str = "unchecked";
pub const ONLINE_OK: &str = "ok";
pub const ONLINE_REGISTRY_ERROR: &str = "registry_error";
pub const ONLINE_DOWNLOAD_ERROR: &str = "download_error";
pub const ONLINE_SEED_MISSING: &str = "seed_missing";

/// 官方 CLI 签名种子轮换告警状态 —— `seedAlert`（`seedcheck.go:63-74`）。
#[derive(Debug, Default)]
struct AlertInner {
    active: bool,
    since: Option<Instant>,
    /// 告警时为原因；非告警时为最近一次扫描状态（`"ok"` / `"cli_not_found"`）。
    signal: String,
    latest: String,
    online: String,
    online_active: bool,
    online_signal: String,
    checked_version: String,
}

/// 线程安全的告警状态（Go 用 `sync.Mutex` 包同一组字段）。
#[derive(Debug)]
pub struct SeedAlert {
    inner: Mutex<AlertInner>,
}

impl Default for SeedAlert {
    fn default() -> Self {
        Self::new()
    }
}

impl SeedAlert {
    /// `NewServer` 里的 `seedAlert{online: "unchecked"}`。
    pub fn new() -> Self {
        SeedAlert {
            inner: Mutex::new(AlertInner {
                online: ONLINE_UNCHECKED.to_string(),
                ..Default::default()
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AlertInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `/health` 的四字段快照 —— `handleHealth` 的 `seedAlert` 段（`server.go:270-275`）。
    pub fn snapshot(&self) -> SeedSnapshot {
        let g = self.lock();
        SeedSnapshot {
            active: g.active,
            signal: g.signal.clone(),
            latest: g.latest.clone(),
            online: g.online.clone(),
        }
    }

    /// 在线层当前告警结论 —— `seedOnlineVerdict`（`seedcheck.go:466-470`）。
    fn online_verdict(&self) -> (bool, String) {
        let g = self.lock();
        (g.online_active, g.online_signal.clone())
    }

    /// 置位/解除 —— `applySeedAlert`（`seedcheck.go:563-579`）。
    ///
    /// `active`/`signal` 都没变化时**什么都不做**（不重复打日志）。
    /// 返回给调用方决定是否打日志：`Some("置位"/"解除")` 表示状态翻转。
    fn apply(&self, active: bool, signal: &str) -> Option<&'static str> {
        let mut g = self.lock();
        if g.active == active && g.signal == signal {
            return None;
        }
        let was = g.active;
        g.active = active;
        g.signal = signal.to_string();
        if active && !was {
            g.since = Some(Instant::now());
            return Some("置位");
        }
        if !active && was {
            g.since = None;
            return Some("解除");
        }
        None
    }

    /// 告警置位时刻（`since`；未告警为 `None`）。
    pub fn since(&self) -> Option<Instant> {
        self.lock().since
    }

    /// 直接置状态（**测试缝**：Go 侧测试改写包内字段，这里给同样的入口；
    /// 生产路径只用 [`run_seed_guard_check_with`]）。
    #[doc(hidden)]
    pub fn set_online_for_test(&self, online: &str, active: bool, signal: &str, latest: &str) {
        let mut g = self.lock();
        g.online = online.to_string();
        g.online_active = active;
        g.online_signal = signal.to_string();
        g.latest = latest.to_string();
    }

    /// 已核验版本号（测试可读）。
    pub fn checked_version(&self) -> String {
        self.lock().checked_version.clone()
    }
}

/// `/health` 需要的四字段 —— `server.go:270-275`。
#[derive(Debug, Clone, PartialEq)]
pub struct SeedSnapshot {
    pub active: bool,
    pub signal: String,
    pub latest: String,
    pub online: String,
}

// ---------------------------------------------------------------- 纯函数面

/// 一个待核验的官方 CLI 发行件 —— `seedSource`（`seedcheck.go:78-81`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SeedSource {
    pub label: String,
    pub path: String,
}

/// 桌面端轨道的一个安装候选 —— `desktopCand`（`seedcheck.go:85-88`）。
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopCand {
    pub dir: String,
    pub path: String,
}

/// 桌面端候选去重 + 打标签 —— `labelDesktopSources`（`seedcheck.go:94-113`）。
///
/// 空路径丢弃；同一路径只留一份；**去重后只剩一份时 label 不带后缀**，
/// **多于一份时全部加盘符后缀**（两边对称）。
pub fn label_desktop_sources(cands: &[DesktopCand]) -> Vec<SeedSource> {
    let mut uniq: Vec<&DesktopCand> = Vec::new();
    for c in cands {
        if c.path.is_empty() || uniq.iter().any(|u| u.path == c.path) {
            continue;
        }
        uniq.push(c);
    }
    uniq.iter()
        .map(|c| {
            let mut label = "桌面端 CLI".to_string();
            if uniq.len() > 1 {
                label.push('(');
                label.push_str(&install_hint(&c.dir));
                label.push(')');
            }
            SeedSource {
                label,
                path: c.path.clone(),
            }
        })
        .collect()
}

/// 安装目录短标识 —— `installHint`（`seedcheck.go:161-169`）：优先盘符（`D:`），
/// 无盘符（UNC/相对路径）退回目录末段，都取不到返回空串。
///
/// ⚠ UNC 形态：Go 的 `filepath.VolumeName("\\\\server\\share\\app")` 返回
/// `\\server\share`（**不含**冒号），而 `installHint` 结尾无条件 `+= ":"` ——
/// 故 UNC 的 hint 是 `\\server\share:`（带尾冒号）。这是**实测值**（门禁锁死），
/// 不是笔误，不"顺手改对"。
pub fn install_hint(dir: &str) -> String {
    if let Some(v) = volume_name(dir) {
        return format!("{}:", v.trim_end_matches(':'));
    }
    let cleaned = dir.trim_end_matches(['\\', '/']);
    let base = cleaned.rsplit(['\\', '/']).next().unwrap_or("");
    if base.is_empty() || base == "." {
        return String::new();
    }
    base.to_string()
}

/// `filepath.VolumeName`：`D:\x` / `D:/x` → `D:`；`\\server\share\x` → `\\server\share`。
fn volume_name(p: &str) -> Option<String> {
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return Some(p[..2].to_string());
    }
    if p.starts_with("\\\\") {
        // UNC：`\\host\share`（前两段）
        let rest = &p[2..];
        let mut it = rest.split(['\\', '/']).filter(|s| !s.is_empty());
        let host = it.next()?;
        let share = it.next()?;
        return Some(format!("\\\\{host}\\{share}"));
    }
    None
}

/// 本机官方 CLI 发行件候选（两条轨道）—— `localCliSources`（`seedcheck.go:121-157`）。
///
/// `install_location` = 卸载表项里的 `InstallLocation`（读不到传空串）。
pub fn local_cli_sources(
    appdata: Option<&str>,
    home: Option<&str>,
    install_location: &str,
    localappdata: Option<&str>,
) -> Vec<SeedSource> {
    let mut out: Vec<SeedSource> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let add = |label: &str, p: String, out: &mut Vec<SeedSource>, seen: &mut Vec<String>| {
        if p.is_empty() || seen.iter().any(|s| s == &p) {
            return;
        }
        seen.push(p.clone());
        out.push(SeedSource {
            label: label.to_string(),
            path: p,
        });
    };

    // 轨道 1：npm 用户级全局安装（rc.x）
    if let Some(ad) = appdata.filter(|s| !s.is_empty()) {
        add(
            "npm CLI",
            join(ad, &["npm", "node_modules", "@unity-china", "codely-cli", "bundle", "gemini.js"]),
            &mut out,
            &mut seen,
        );
    }
    if let Some(h) = home.filter(|s| !s.is_empty()) {
        add(
            "npm CLI",
            join(
                h,
                &["AppData", "Roaming", "npm", "node_modules", "@unity-china", "codely-cli", "bundle", "gemini.js"],
            ),
            &mut out,
            &mut seen,
        );
    }

    // 轨道 2：桌面端内置（release.x）
    let mut cands: Vec<DesktopCand> = Vec::new();
    if !install_location.is_empty() {
        cands.push(DesktopCand {
            dir: install_location.to_string(),
            path: crate::winpaths::cowork_cli_exe(install_location),
        });
    }
    if let Some(la) = localappdata.filter(|s| !s.is_empty()) {
        let d = crate::winpaths::cowork_fallback_dir(la);
        cands.push(DesktopCand {
            dir: d.clone(),
            path: crate::winpaths::cowork_cli_exe(&d),
        });
    }
    for s in label_desktop_sources(&cands) {
        add(&s.label, s.path, &mut out, &mut seen);
    }
    out
}

/// `filepath.Join` 的等价物（只用 `\`，Windows 侧唯一形态）。
fn join(base: &str, parts: &[&str]) -> String {
    let mut s = base.trim_end_matches(['\\', '/']).to_string();
    for p in parts {
        s.push('\\');
        s.push_str(p.trim_matches(['\\', '/']));
    }
    s
}

/// 一次扫描结果 → 卫兵状态 —— `evalSeedScan`（`seedcheck.go:270-283`）。
pub fn eval_seed_scan(scanned: bool, seed_found: bool, header_found: bool) -> (bool, String) {
    match (scanned, seed_found, header_found) {
        (false, _, _) => (false, "cli_not_found".to_string()),
        (true, true, true) => (false, "ok".to_string()),
        (true, false, false) => (true, "签名种子与签名头名均未命中".to_string()),
        (true, false, true) => (true, "签名种子未命中（官方可能已轮换种子）".to_string()),
        (true, true, false) => (true, "签名头名未命中（官方可能已改签名方案）".to_string()),
    }
}

/// 流式核对两个特征串 —— `containsSeedFeatures`（`seedcheck.go:195-229`）。
///
/// 分块读、块间保留 `needle.len()-1` 的重叠（跨块命中不漏）；两特征都命中提前返回。
/// `seed`/`header` 由调用方给（生产传本仓红线常量，测试传**合成**特征串）。
pub fn contains_seed_features<R: Read>(
    r: &mut R,
    seed: &str,
    header: &str,
) -> Result<(bool, bool), String> {
    let needles: [&[u8]; 2] = [seed.as_bytes(), header.as_bytes()];
    let mut found = [false, false];
    let overlap = std::cmp::max(seed.len(), header.len()).saturating_sub(1);
    let mut chunk = vec![0u8; CHUNK_SIZE];
    let mut window: Vec<u8> = Vec::with_capacity(CHUNK_SIZE + overlap);
    loop {
        match r.read(&mut chunk) {
            Ok(0) => return Ok((found[0], found[1])),
            Ok(n) => {
                window.extend_from_slice(&chunk[..n]);
                for (i, needle) in needles.iter().enumerate() {
                    if needle.is_empty() {
                        continue;
                    }
                    if !found[i] && contains(&window, needle) {
                        found[i] = true;
                    }
                }
                if found[0] && found[1] {
                    return Ok((true, true));
                }
                if window.len() > overlap {
                    let keep = window[window.len() - overlap..].to_vec();
                    window = keep;
                }
            }
            Err(e) => return Err(format!("读文件失败: {e}")),
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

/// [`contains_seed_features`] 的**多探针**形态（门禁用；生产固定两串）：
/// 按给定顺序逐串核验，与两探针版同一条分块扫描逻辑。
#[doc(hidden)]
pub fn contains_seed_features_all<R: Read>(
    r: &mut R,
    needles: &[&str],
) -> Result<Vec<bool>, String> {
    if needles.len() != 2 {
        return Err("本入口只服务两探针场景（与 containsSeedFeatures 同形）".into());
    }
    let (a, b) = contains_seed_features(r, needles[0], needles[1])?;
    Ok(vec![a, b])
}

/// 扫单个发行件 —— `scanSeedBundle`（`seedcheck.go:233-242`）：`scanned=false` 表示
/// 文件不存在或读失败（**不算告警**）。
pub fn scan_seed_bundle(path: &str, seed: &str, header: &str) -> (bool, bool, bool) {
    if !std::path::Path::new(path).is_file() {
        return (false, false, false);
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return (false, false, false);
    };
    match contains_seed_features(&mut f, seed, header) {
        Ok((sf, hf)) => (true, sf, hf),
        Err(_) => (false, false, false),
    }
}

/// 扫全部候选 —— `scanCliSources`（`seedcheck.go:247-265`）：任一个「文件在但特征
/// 消失」即告警（signal 带来源名）；全部扫不到记 `cli_not_found`（非告警）。
pub fn scan_cli_sources(srcs: &[SeedSource], seed: &str, header: &str) -> (bool, String) {
    let mut scanned_labels: Vec<String> = Vec::new();
    for src in srcs {
        let (scanned, sf, hf) = scan_seed_bundle(&src.path, seed, header);
        if !scanned {
            continue;
        }
        scanned_labels.push(src.label.clone());
        let (a, sig) = eval_seed_scan(scanned, sf, hf);
        if a {
            return (true, format!("{}：{}", src.label, sig));
        }
    }
    if scanned_labels.is_empty() {
        return (false, "cli_not_found".to_string());
    }
    (
        false,
        format!("ok（已核 {}）", scanned_labels.join("+")),
    )
}

/// 两层合成 —— `combineSeedAlerts`（`seedcheck.go:540-549`）：在线层优先，
/// 两层都正常时用本机层信号。
pub fn combine_seed_alerts(
    local_active: bool,
    local_signal: &str,
    online_active: bool,
    online_signal: &str,
) -> (bool, String) {
    if online_active {
        return (true, online_signal.to_string());
    }
    if local_active {
        return (true, local_signal.to_string());
    }
    (false, local_signal.to_string())
}

// ---------------------------------------------------------------- registry 在线层

/// 查 packument 的 `dist-tags.latest` —— `queryRegistryLatest`（`seedcheck.go:288-319`）。
pub async fn query_registry_latest(client: &reqwest::Client, packument_url: &str) -> Result<String, String> {
    let resp = client
        .get(packument_url)
        .timeout(SEED_REGISTRY_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("registry 请求失败: {e}"))?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("registry 状态 {}", resp.status().as_u16()));
    }
    let body = read_bounded(resp, SEED_REGISTRY_BODY_LIMIT).await?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("registry 响应解析失败: {e}"))?;
    let latest = v
        .get("dist-tags")
        .and_then(|d| d.get("latest"))
        .and_then(|l| l.as_str())
        .unwrap_or("");
    if latest.is_empty() {
        return Err("dist-tags 无 latest".to_string());
    }
    Ok(latest.to_string())
}

/// 查具体版本文档的 `dist.tarball` —— `queryVersionTarball`（`seedcheck.go:323-354`）。
pub async fn query_version_tarball(
    client: &reqwest::Client,
    packument_url: &str,
    version: &str,
) -> Result<String, String> {
    let url = format!("{packument_url}/{version}");
    let resp = client
        .get(&url)
        .timeout(SEED_REGISTRY_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("版本文档请求失败: {e}"))?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("版本文档状态 {}", resp.status().as_u16()));
    }
    let body = read_bounded(resp, SEED_VERSION_DOC_LIMIT).await?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("版本文档解析失败: {e}"))?;
    let tb = v
        .get("dist")
        .and_then(|d| d.get("tarball"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if tb.is_empty() {
        return Err("版本文档无 dist.tarball".to_string());
    }
    Ok(tb.to_string())
}

async fn read_bounded(resp: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读响应失败: {e}"))?;
    let mut v = bytes.to_vec();
    if v.len() as u64 > limit {
        v.truncate(limit as usize); // Go 的 `io.LimitReader` 同样截断
    }
    Ok(v)
}

/// 下载 tarball 并核验特征 —— `verifySeedTarball`（`seedcheck.go:377-462`）。
///
/// 返回 `(seed_found, header_found, file_found, err)`：`err` 非空 = 下载/解包层失败
/// （调用方记 `download_error`）；`err` 空且 `file_found=false` = 包里没有该文件。
pub async fn verify_seed_tarball(
    client: &reqwest::Client,
    tarball_url: &str,
    seed: &str,
    header: &str,
) -> (bool, bool, bool, Option<String>) {
    let resp = match client
        .get(tarball_url)
        .timeout(SEED_DOWNLOAD_TIMEOUT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return (false, false, false, Some(format!("tarball 请求失败: {e}"))),
    };
    if resp.status() != reqwest::StatusCode::OK {
        return (
            false,
            false,
            false,
            Some(format!("tarball 状态 {}", resp.status().as_u16())),
        );
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return (false, false, false, Some(format!("读 tarball 失败: {e}"))),
    };
    if bytes.len() as u64 > SEED_TARBALL_MAX_BYTES {
        return (
            false,
            false,
            false,
            Some(format!("超过 {SEED_TARBALL_MAX_BYTES} 字节上限")),
        );
    }
    verify_seed_tarball_bytes(&bytes, seed, header)
}

/// 纯字节版（下载层之外的内核；门禁直接喂真 tar.gz 字节）。
pub fn verify_seed_tarball_bytes(
    gz: &[u8],
    seed: &str,
    header: &str,
) -> (bool, bool, bool, Option<String>) {
    let out = match gziptar::extract_from_gzip(
        gz,
        SEED_BUNDLE_REL_PATH,
        SEED_TARBALL_MAX_BYTES,
    ) {
        Ok(o) => o,
        Err(e) => return (false, false, false, Some(e)),
    };
    if !out.found {
        return (false, false, false, None); // tar 读完没有 bundle/gemini.js
    }
    let mut cur = std::io::Cursor::new(out.content);
    match contains_seed_features(&mut cur, seed, header) {
        Ok((sf, hf)) => (sf, hf, true, None),
        Err(e) => (false, false, false, Some(e)),
    }
}

// ---------------------------------------------------------------- 一轮 & 后台循环

/// 一轮的输入（把环境读、注册表读、网络都收成参数，生产入口 [`SeedGuard::run_once`]）。
pub struct GuardDeps<'a> {
    pub client: &'a reqwest::Client,
    pub packument_url: &'a str,
    pub seed: &'a str,
    pub header: &'a str,
    /// 本机候选发行件（生产由 [`local_cli_sources`] 现算）。
    pub sources: Vec<SeedSource>,
    /// **是否跑在线层**（测试关掉它就不会碰 npm 公网）。
    pub online: bool,
}

/// 一轮结果（供调用方打日志/断言）。
#[derive(Debug, Clone, PartialEq)]
pub struct GuardRound {
    pub active: bool,
    pub signal: String,
    pub local_active: bool,
    pub local_signal: String,
    pub online_active: bool,
    pub online_signal: String,
}

/// 种子卫兵（Go 侧是 `Server` 上的两个字段 + 几个方法，Rust 侧独立成对象，
/// 由 `AppState` 持有一份）。
#[derive(Debug)]
pub struct SeedGuard {
    pub alert: SeedAlert,
}

impl Default for SeedGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl SeedGuard {
    pub fn new() -> Self {
        SeedGuard {
            alert: SeedAlert::new(),
        }
    }

    /// 在线层一轮 —— `checkSeedRegistry`（`seedcheck.go:475-535`）：
    /// 版本没变**零下载**；查询/下载失败只改状态不告警。
    async fn check_registry(&self, deps: &GuardDeps<'_>) -> (bool, String) {
        let latest = match query_registry_latest(deps.client, deps.packument_url).await {
            Ok(v) => v,
            Err(_) => {
                {
                    let mut g = self.alert.lock();
                    g.latest.clear();
                    g.online = ONLINE_REGISTRY_ERROR.to_string();
                }
                return self.alert.online_verdict();
            }
        };
        {
            let mut g = self.alert.lock();
            let checked = g.checked_version.clone();
            g.latest = latest.clone();
            if checked == latest {
                // 版本没变 → 不下载；online 按**已核结论**重算（防上一轮 registry_error
                // 把状态永久卡脏，`seedcheck.go:488-496`）。
                g.online = if g.online_active {
                    ONLINE_SEED_MISSING.to_string()
                } else {
                    ONLINE_OK.to_string()
                };
                let active = g.online_active;
                let signal = g.online_signal.clone();
                return (active, signal);
            }
        }

        let tarball_url = match query_version_tarball(deps.client, deps.packument_url, &latest).await
        {
            Ok(u) => u,
            Err(_) => {
                self.alert.lock().online = ONLINE_DOWNLOAD_ERROR.to_string();
                return self.alert.online_verdict();
            }
        };
        let (sf, hf, found, err) =
            verify_seed_tarball(deps.client, &tarball_url, deps.seed, deps.header).await;
        if err.is_some() {
            self.alert.lock().online = ONLINE_DOWNLOAD_ERROR.to_string();
            return self.alert.online_verdict();
        }

        // 特征核验走到了结果（无论命中与否都记已核版本，同版本告警期间不重复下载）
        let (active, signal) = if !found {
            (true, format!("{latest} 包内未找到 {SEED_BUNDLE_REL_PATH}"))
        } else {
            let (a, esig) = eval_seed_scan(true, sf, hf);
            if a {
                (true, format!("{latest} {esig}"))
            } else {
                (false, String::new())
            }
        };
        {
            let mut g = self.alert.lock();
            g.checked_version = latest;
            g.online = if active {
                ONLINE_SEED_MISSING.to_string()
            } else {
                ONLINE_OK.to_string()
            };
            g.online_active = active;
            g.online_signal = signal.clone();
        }
        (active, signal)
    }

    /// 扫一轮 —— `runSeedGuardCheck`（`seedcheck.go:554-559`）。
    ///
    /// 返回本轮结论 + 是否发生了状态翻转（`Some("置位"/"解除")`，供调用方打日志）。
    pub async fn run_once(&self, deps: &GuardDeps<'_>) -> (GuardRound, Option<&'static str>) {
        let (local_active, local_signal) = scan_cli_sources(&deps.sources, deps.seed, deps.header);
        let (online_active, online_signal) = if deps.online {
            self.check_registry(deps).await
        } else {
            self.alert.online_verdict()
        };
        let (active, signal) = combine_seed_alerts(
            local_active,
            &local_signal,
            online_active,
            &online_signal,
        );
        let flip = self.alert.apply(active, &signal);
        (
            GuardRound {
                active,
                signal,
                local_active,
                local_signal,
                online_active,
                online_signal,
            },
            flip,
        )
    }
}

/// 生产轮入口 —— 现算本机候选 + 走 npm 公网（`SeedGuardLoop` 的每次迭代）。
pub async fn run_seed_guard_check(
    guard: &SeedGuard,
    client: &reqwest::Client,
) -> (GuardRound, Option<&'static str>) {
    let appdata = std::env::var("APPDATA").ok();
    let home = std::env::var("USERPROFILE").ok();
    let localappdata = std::env::var("LOCALAPPDATA").ok();
    let install_location = read_cowork_install_location();
    let sources = local_cli_sources(
        appdata.as_deref(),
        home.as_deref(),
        &install_location,
        localappdata.as_deref(),
    );
    let deps = GuardDeps {
        client,
        packument_url: SEED_REGISTRY_PACKUMENT_URL,
        // 生产恒为本仓红线常量；测试永远走注入版本，绝不读真实发行件断言种子值。
        seed: CODEY_SIGNING_SEED_HEX,
        header: SEED_FEATURE_HEADER_NAME,
        sources,
        online: true,
    };
    guard.run_once(&deps).await
}

/// 读团结 Cowork 桌面端安装目录 —— `readCoworkInstallLocation`（`seedcheck.go:173-178`
/// + `desktop_path_windows.go`）：HKCU 卸载表项里 `DisplayName == "Tuanjie Cowork"`
/// 的那一项的 `InstallLocation`；没有返回空串（**非 Windows 平台恒空**，同 Go 的
/// `regReadCoworkInstallLocation == nil` 约定）。
pub fn read_cowork_install_location() -> String {
    #[cfg(windows)]
    {
        read_cowork_install_location_with("reg")
    }
    #[cfg(not(windows))]
    {
        String::new()
    }
}

/// 同上，命令行可注入（测试塞假 `reg` 脚本，不读真实注册表）。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn read_cowork_install_location_with(program: &str) -> String {
    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    // Go 侧 `ReadSubKeyNames` 拿的是子键名；`reg.exe` **没有** `/subkeys` 开关
    // （传了报 "Invalid syntax" 退出码 1——P8c 审查抓到的真 bug，此前整条注册表
    // 轨道静默失效）。普通 `reg query <key>` 本来就列子键（每行一个全路径、
    // 无前导空白），`parse_subkeys` 认这些行。
    let subkeys = subkeys_of(program, &format!("HKCU\\{UNINSTALL}"));
    for name in subkeys {
        let key = format!("HKCU\\{UNINSTALL}\\{name}");
        let vals = crate::winreg::query_values_with(program, &key);
        let display = vals
            .iter()
            .find(|(n, _)| n == "DisplayName")
            .map(|(_, v)| v.data.clone())
            .unwrap_or_default();
        if display != "Tuanjie Cowork" {
            continue;
        }
        if let Some((_, v)) = vals.iter().find(|(n, _)| n == "InstallLocation") {
            return v.data.clone();
        }
    }
    String::new()
}

/// 普通 `reg query <key>` 的子键名列表（没有该键 → 空表）。
///
/// 真机实测：`reg query` 无 `/subkeys` 开关（传了报 Invalid syntax）；
/// 普通查询本来就列子键——每个子键一行全路径、**无**前导空白（值行才有前导空白），
/// 首行是被查的键自身。只收**直接**子键（ deeper 的要加 `/s` 才列出来，这里不处理）。
pub fn subkeys_of(program: &str, key: &str) -> Vec<String> {
    let Ok(out) = std::process::Command::new(program)
        .args(["query", key])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_subkeys(&String::from_utf8_lossy(&out.stdout), key)
}

/// 解析普通 `reg query` 输出：无前导空白、以 `key\` 开头、且剩余部分不含 `\` 的行
/// 是直接子键全名，取最后一段。比较键前缀时大小写不敏感（注册表路径本就不敏感，
/// `reg.exe` 回显的大小写与查询串未必一致）。
///
/// 注意：`reg.exe` 会把查询串里的缩写 hive（`HKCU` 等）回显成全称
/// （`HKEY_CURRENT_USER`），故比较前两侧都要过 `normalize_hive`，
/// 否则前缀永远对不上（P8c 审查修复时真机抓到的第二层）。
fn parse_subkeys(text: &str, key: &str) -> Vec<String> {
    let want = normalize_hive(key);
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue; // 值行
        }
        let full = line.trim();
        if full.is_empty() {
            continue;
        }
        let norm = normalize_hive(full);
        if norm.eq_ignore_ascii_case(&want) {
            continue; // 首行的键自身
        }
        // 前缀是 ASCII（hive 全称 + 标准路径）；先卡字符边界再切，比较本身不敏感大小写。
        if norm.len() < want.len() + 1 || !norm.is_char_boundary(want.len()) {
            continue;
        }
        let (head, rest) = norm.split_at(want.len());
        if !head.eq_ignore_ascii_case(&want) {
            continue;
        }
        let Some(rest) = rest.strip_prefix('\\') else {
            continue;
        };
        if rest.is_empty() || rest.contains('\\') {
            continue; // 非直接子键
        }
        // 名字从**原行**取（归一化只用于比较，大小写按回显原样）。
        let name = full.rsplit('\\').next().unwrap_or(full);
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

/// 注册表 hive 缩写展开（只处理行首第一段；其余部分原样保留）。
fn normalize_hive(path: &str) -> String {
    const HIVES: [(&str, &str); 5] = [
        ("HKCU", "HKEY_CURRENT_USER"),
        ("HKLM", "HKEY_LOCAL_MACHINE"),
        ("HKCR", "HKEY_CLASSES_ROOT"),
        ("HKU", "HKEY_USERS"),
        ("HKCC", "HKEY_CURRENT_CONFIG"),
    ];
    let first_len = path.find('\\').unwrap_or(path.len());
    let (first, rest) = path.split_at(first_len);
    for (abbr, full) in HIVES {
        if first.eq_ignore_ascii_case(abbr) {
            return format!("{full}{rest}");
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_seed_scan_table_matches_go() {
        assert_eq!(eval_seed_scan(false, false, false), (false, "cli_not_found".into()));
        assert_eq!(eval_seed_scan(true, true, true), (false, "ok".into()));
        assert_eq!(
            eval_seed_scan(true, false, false),
            (true, "签名种子与签名头名均未命中".into())
        );
        assert_eq!(
            eval_seed_scan(true, false, true),
            (true, "签名种子未命中（官方可能已轮换种子）".into())
        );
        assert_eq!(
            eval_seed_scan(true, true, false),
            (true, "签名头名未命中（官方可能已改签名方案）".into())
        );
    }

    #[test]
    fn install_hint_takes_drive_then_basename() {
        assert_eq!(install_hint(r"D:\Program Files (x86)"), "D:");
        assert_eq!(install_hint("C:/x/y"), "C:");
        // UNC：Go 的 VolumeName 返回 `\\server\share`（无冒号），installHint 无条件补
        // 冒号 → 带尾冒号（实测值，门禁已锁）
        assert_eq!(install_hint(r"\\server\share\app"), r"\\server\share:");
        assert_eq!(install_hint(""), "");
        assert_eq!(install_hint("."), "");
    }

    #[test]
    fn desktop_labels_are_symmetric() {
        let one = vec![DesktopCand {
            dir: "D:\\a".into(),
            path: "D:\\a\\codely.exe".into(),
        }];
        let ls = label_desktop_sources(&one);
        assert_eq!(ls.len(), 1);
        assert_eq!(ls[0].label, "桌面端 CLI", "只剩一份时不加后缀");

        let two = vec![
            DesktopCand {
                dir: "D:\\a".into(),
                path: "D:\\a\\codely.exe".into(),
            },
            DesktopCand {
                dir: "C:\\b".into(),
                path: "C:\\b\\codely.exe".into(),
            },
        ];
        let ls = label_desktop_sources(&two);
        assert_eq!(ls.len(), 2);
        assert_eq!(ls[0].label, "桌面端 CLI(D:)");
        assert_eq!(ls[1].label, "桌面端 CLI(C:)");

        // 同一路径两份 → 去重后只剩一份 → 不加后缀
        let dup = vec![
            DesktopCand {
                dir: "D:\\a".into(),
                path: "D:\\a\\codely.exe".into(),
            },
            DesktopCand {
                dir: "X:\\z".into(),
                path: "D:\\a\\codely.exe".into(),
            },
        ];
        assert_eq!(label_desktop_sources(&dup).len(), 1);
        assert_eq!(label_desktop_sources(&dup)[0].label, "桌面端 CLI");
        // 空路径丢弃
        assert!(label_desktop_sources(&[DesktopCand {
            dir: "D:\\a".into(),
            path: String::new()
        }])
        .is_empty());
    }

    #[test]
    fn local_sources_order_and_shape() {
        let srcs = local_cli_sources(
            Some(r"C:\Users\u\AppData\Roaming"),
            Some(r"C:\Users\u"),
            r"D:\Program Files (x86)",
            Some(r"C:\Users\u\AppData\Local"),
        );
        let dump: Vec<String> = srcs.iter().map(|s| format!("{} => {}", s.label, s.path)).collect();
        // npm 轨道两个候选路径在**同一台机器上通常重合**（`%APPDATA%` ==
        // `%USERPROFILE%\AppData\Roaming`），Go 的 `seen` 去重后只剩一条 —— 故 4 个候选
        // 去重后是 3 条（这里的 appdata 与 home 就重合）。换一对不重合的入参来验 4 条。
        assert_eq!(srcs.len(), 3, "{dump:#?}");
        assert_eq!(srcs[0].label, "npm CLI");
        assert!(srcs[0]
            .path
            .ends_with(r"npm\node_modules\@unity-china\codely-cli\bundle\gemini.js"));
        assert_eq!(srcs[1].label, "桌面端 CLI(D:)");
        assert_eq!(srcs[2].label, "桌面端 CLI(C:)");

        let srcs4 = local_cli_sources(
            Some(r"X:\appdata"),
            Some(r"C:\Users\u"),
            r"D:\Program Files (x86)",
            Some(r"C:\Users\u\AppData\Local"),
        );
        let dump4: Vec<String> = srcs4
            .iter()
            .map(|s| format!("{} => {}", s.label, s.path))
            .collect();
        assert_eq!(srcs4.len(), 4, "npm 两个候选不重合时是 4 条：{dump4:#?}");
        // appdata 候选在前、home 候选在后（与 Go 的 `add` 调用顺序一致）
        assert_eq!(srcs4[0].label, "npm CLI");
        assert!(
            srcs4[0].path.starts_with(r"X:\appdata\npm"),
            "{}",
            srcs4[0].path
        );
        assert_eq!(srcs4[1].label, "npm CLI");
        assert!(
            srcs4[1].path.starts_with(r"C:\Users\u\AppData\Roaming\npm"),
            "{}",
            srcs4[1].path
        );
        assert_eq!(srcs4[2].label, "桌面端 CLI(D:)");
        assert_eq!(srcs4[3].label, "桌面端 CLI(C:)");
    }

    #[test]
    fn contains_features_across_chunk_boundary() {
        // 把一个特征串横跨分块边界放置：前后各填到 1 MiB 边界上
        let seed = "SYNTHETICSEED0001";
        let header = "X-Codely-Signature";
        let mut data = vec![b'a'; CHUNK_SIZE - 5];
        data.extend_from_slice(seed.as_bytes()); // 从 CHUNK_SIZE-5 起，跨块
        data.extend_from_slice(b"zzz");
        data.extend_from_slice(header.as_bytes());
        let mut cur = std::io::Cursor::new(data);
        assert_eq!(
            contains_seed_features(&mut cur, seed, header).unwrap(),
            (true, true)
        );
    }

    #[test]
    fn contains_features_reports_partial() {
        let seed = "SYNTHETICSEED0001";
        let header = "X-Codely-Signature";
        let data = format!("xxx{seed}yyy");
        let mut cur = std::io::Cursor::new(data.into_bytes());
        assert_eq!(
            contains_seed_features(&mut cur, seed, header).unwrap(),
            (true, false)
        );
    }

    #[test]
    fn subkeys_parser_reads_plain_query_output() {
        // 真机 `reg query` 原样格式：子键行无前导空白、首行是被查的键自身、值行有前导空白。
        // （旧测试喂的是带缩进的假格式——P8c 审查抓包：假格式永远测不出真 bug。）
        let key = r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
        let dump = "\r\nHKEY_CURRENT_USER\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\r\nHKEY_CURRENT_USER\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{A1B2}\r\nHKEY_CURRENT_USER\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Tuanjie Cowork\r\n    DisplayName    REG_SZ    x\r\n\r\n";
        assert_eq!(
            parse_subkeys(dump, key),
            vec!["{A1B2}", "Tuanjie Cowork"]
        );
        // 大小写不敏感 + 只收直接子键。
        let dump2 = "hkcu\\software\\microsoft\\windows\\currentversion\\uninstall\r\nHKCU\\SOFTWARE\\A\\B\r\n";
        assert!(parse_subkeys(dump2, key).is_empty());
    }

    /// 活体验证（仅 Windows）：注册表桌面端轨道必须真找到本机 Cowork 安装目录。
    /// 不用假 reg 脚本自证——直接调生产函数读真实注册表。
    /// 本机没装 Cowork 时提前返回（不断言失败，CI 友好）。
    #[test]
    #[cfg(windows)]
    fn cowork_install_location_resolves_on_this_machine() {
        let probe = std::process::Command::new("reg")
            .args([
                "query",
                r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Tuanjie Cowork",
                "/v",
                "DisplayName",
            ])
            .output();
        let installed = probe
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !installed {
            eprintln!("SKIP: 本机未装 Tuanjie Cowork，跳过活体断言");
            return;
        }
        let loc = read_cowork_install_location();
        assert!(
            !loc.is_empty(),
            "注册表轨道必须返回非空安装目录"
        );
        assert!(
            loc.ends_with("Tuanjie Cowork"),
            "安装目录应指向 Cowork，实得 {loc:?}"
        );
        assert!(
            std::path::Path::new(&loc).is_dir(),
            "安装目录必须真实存在：{loc:?}"
        );
    }

    #[test]
    fn combine_prefers_online() {
        assert_eq!(
            combine_seed_alerts(true, "本地", true, "在线"),
            (true, "在线".to_string())
        );
        assert_eq!(
            combine_seed_alerts(true, "本地", false, ""),
            (true, "本地".to_string())
        );
        assert_eq!(
            combine_seed_alerts(false, "ok", false, ""),
            (false, "ok".to_string())
        );
    }

    #[test]
    fn scan_cli_sources_reports_label_and_missing() {
        // 不存在的路径：scanned=false → cli_not_found（非告警）
        let miss = vec![SeedSource {
            label: "npm CLI".into(),
            path: r"Z:\__p8c_nonexistent__\gemini.js".into(),
        }];
        assert_eq!(
            scan_cli_sources(&miss, "SEEDX", "X-Codely-Signature"),
            (false, "cli_not_found".to_string())
        );
    }
}
