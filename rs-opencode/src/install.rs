//! install.rs —— 一键安装 opencode（npm tgz → 校验 → 进程内解包 → 原子落位）。
//!
//! 设计约束（任务简报 §1）：
//! * **绝不自动触发**：只有 `POST /install/start`（或用户把 `autoInstall` 设成 true）
//!   才会下载 95 MB 的包；默认配置 `autoInstall = false`。
//! * **只用进程内解包**：`flate2` + `tar`，不 shell 调 `tar.exe`，不写任何临时可执行脚本。
//! * **先校验后落位**：下载到 `*.downloading`，用 registry 元数据的 `dist.integrity`
//!   （SRI `sha512-<base64>`）校验整包，再解出 `package/bin/opencode.exe`，
//!   最后原子 rename 成 `opencode.exe`。任何失败都清掉临时文件，不留半成品。
//! * **进度可观测**：`downloaded` / `total` 用原子计数，`/install/status` 随时可读。
//!
//! 落位：`<数据目录>/opencode/<version>/opencode.exe`
//! （数据目录判定链见 `datadir.rs`：PROXYDECK_DATA_DIR → 便携模式 → %APPDATA%\ProxyDeck）。
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha512};
use tokio::io::AsyncWriteExt;

use crate::datadir;

/// 钉死版本：2.0.32 已用真实 opencode 跑通；2.1.2 未验证，不默认用。
pub const DEFAULT_VERSION: &str = "2.0.32";
/// 「已安装」判据：opencode.exe 至少 100 MB（真实产物约 203 MB）。
pub const MIN_EXE_BYTES: u64 = 100 * 1024 * 1024;

/// npm 包名与包内路径。
const PACKAGE: &str = "@opencode/cli-windows-x64";
const ENTRY: &str = "package/bin/opencode.exe";
const EXE_NAME: &str = "opencode.exe";
const TMP_SUFFIX: &str = ".downloading";

/// 下载 UA（registry 不校验，但别用空 UA 给 CDN 添麻烦）。
const USER_AGENT: &str = "ProxyDeck-opencode-installer/1.0 (+https://github.com/Amer-CN/proxydeck)";

/// `GET /install/status` 的 JSON 形状（字段名与简报逐字一致）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
    pub downloading: bool,
    pub downloaded: u64,
    pub total: u64,
    pub error: Option<String>,
}

/// 内部错误：取消要和普通失败分开（取消不是错误，不写进 status.error）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PipelineError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PipelineError::Cancelled => write!(f, "cancelled"),
            PipelineError::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// 进程级安装状态（单例）。只放原子量 + 一个错误槽，避免锁住下载热路径。
#[derive(Default)]
struct Progress {
    /// 有安装任务在跑（下载或解包）。
    busy: AtomicBool,
    /// 取消标志：下载循环每轮检查一次。
    cancel: AtomicBool,
    downloaded: AtomicU64,
    total: AtomicU64,
    error: Mutex<Option<String>>,
}

fn progress() -> &'static Progress {
    static PROGRESS: OnceLock<Progress> = OnceLock::new();
    PROGRESS.get_or_init(Progress::default)
}

fn set_error(msg: Option<String>) {
    let p = progress();
    let mut guard = p.error.lock().unwrap_or_else(|e| e.into_inner());
    *guard = msg;
}

fn take_error() -> Option<String> {
    let p = progress();
    let guard = p.error.lock().unwrap_or_else(|e| e.into_inner());
    guard.clone()
}

fn data_root() -> PathBuf {
    datadir::data_dir().join("opencode")
}

/// 当前安装状态。`installed` 以磁盘为准（每次重新扫版本目录，不缓存）。
pub async fn status() -> InstallStatus {
    let p = progress();
    let downloading = p.busy.load(Ordering::Acquire);
    let found = scan_installed(&data_root(), MIN_EXE_BYTES);
    InstallStatus {
        installed: found.is_some(),
        version: found.as_ref().map(|(v, _)| v.clone()),
        path: found.as_ref().map(|(_, path)| path.display().to_string()),
        downloading,
        downloaded: if downloading {
            p.downloaded.load(Ordering::Relaxed)
        } else {
            0
        },
        total: if downloading {
            p.total.load(Ordering::Relaxed)
        } else {
            0
        },
        error: take_error(),
    }
}

/// 启动后台安装。
///
/// 幂等语义：
/// * 已有任务在跑 → `Err("already running")`（HTTP 层回 409，不重复下 95 MB）。
/// * 目标版本已存在且大小合理 → 直接 `Ok(())`（status 会显示 installed）。
pub async fn start(version: &str) -> Result<(), String> {
    let version = version.trim();
    if !valid_version(version) {
        return Err(format!("非法的 opencode 版本号: {version:?}"));
    }
    let p = progress();
    if p.busy.swap(true, Ordering::AcqRel) {
        return Err("already running".to_string());
    }
    p.cancel.store(false, Ordering::Release);
    p.downloaded.store(0, Ordering::Relaxed);
    p.total.store(0, Ordering::Relaxed);
    set_error(None);

    let root = data_root();
    let target = root.join(version).join(EXE_NAME);
    if is_complete_exe(&target, MIN_EXE_BYTES) {
        p.busy.store(false, Ordering::Release);
        crate::logf(&format!(
            "安装：opencode {version} 已存在（{}），无需下载",
            target.display()
        ));
        return Ok(());
    }

    let version = version.to_string();
    let dir = root.join(&version);
    tokio::spawn(async move {
        let result = install(&version, &dir, p).await;
        match result {
            Ok(path) => {
                crate::logf(&format!("安装：完成 → {}", path.display()));
            }
            Err(PipelineError::Cancelled) => {
                crate::logf("安装：已取消，临时文件已清理");
            }
            Err(PipelineError::Failed(e)) => {
                crate::logf(&format!("安装：失败（{e}）"));
                set_error(Some(e));
            }
        }
        p.downloaded.store(0, Ordering::Relaxed);
        p.total.store(0, Ordering::Relaxed);
        p.busy.store(false, Ordering::Release);
    });
    Ok(())
}

/// 请求取消。下载循环下一轮看到标志就退出并删临时文件；不阻塞调用方。
pub async fn cancel() {
    progress().cancel.store(true, Ordering::Release);
    crate::logf("安装：收到取消请求");
}

/// 版本号只允许数字/字母/`.`/`-`/`_`：它会拼进路径，拒绝 `..` 之类。
fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 32
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !version.starts_with('.')
}

// ------------------------------ 安装主流程 ------------------------------

/// 安装入口：负责临时文件的失败清理，业务细节在 `install_inner`。
async fn install(version: &str, dir: &Path, p: &Progress) -> Result<PathBuf, PipelineError> {
    let target = dir.join(EXE_NAME);
    let tgz = dir.join(format!("cli-windows-x64-{version}.tgz{TMP_SUFFIX}"));
    let exe_tmp = dir.join(format!("{EXE_NAME}{TMP_SUFFIX}"));
    let result = install_inner(version, dir, &target, &tgz, &exe_tmp, p).await;
    if result.is_err() {
        cleanup(&tgz);
        cleanup(&exe_tmp);
    }
    result
}

async fn install_inner(
    version: &str,
    dir: &Path,
    target: &Path,
    tgz: &Path,
    exe_tmp: &Path,
    p: &Progress,
) -> Result<PathBuf, PipelineError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| PipelineError::Failed(format!("创建安装目录 {} 失败: {e}", dir.display())))?;
    if p.cancel.load(Ordering::Acquire) {
        return Err(PipelineError::Cancelled);
    }

    let integrity = fetch_integrity(version).await;
    if integrity.is_none() {
        // 简报要求：允许跳过校验，但必须在日志里明说，绝不静默。
        crate::logf(
            "安装：取不到 registry 的 dist.integrity，本次跳过 sha512 校验（来源 npm registry）",
        );
    }

    let sources = [
        format!("https://registry.npmjs.org/{PACKAGE}/-/cli-windows-x64-{version}.tgz"),
        format!("https://registry.npmmirror.com/{PACKAGE}/-/cli-windows-x64-{version}.tgz"),
    ];
    let mut last_error: Option<String> = None;
    for url in &sources {
        if p.cancel.load(Ordering::Acquire) {
            return Err(PipelineError::Cancelled);
        }
        crate::logf(&format!("安装：下载 {url}"));
        match download(url, tgz, p).await {
            Ok(()) => {}
            Err(PipelineError::Cancelled) => return Err(PipelineError::Cancelled),
            Err(PipelineError::Failed(e)) => {
                crate::logf(&format!("安装：下载源失败（{e}），尝试下一个源"));
                last_error = Some(e);
                cleanup(tgz);
                continue;
            }
        }
        if p.cancel.load(Ordering::Acquire) {
            cleanup(tgz);
            return Err(PipelineError::Cancelled);
        }
        // 校验 → 解包 → 落位；校验失败时删包换镜像源再试一次（官方 CDN 偶发截断）。
        match finish_install(target, tgz, exe_tmp, integrity.as_deref(), p).await {
            Ok(path) => return Ok(path),
            Err(PipelineError::Cancelled) => return Err(PipelineError::Cancelled),
            Err(PipelineError::Failed(e)) => {
                crate::logf(&format!("安装：包处理失败（{e}），尝试下一个源"));
                last_error = Some(e);
                continue;
            }
        }
    }
    Err(PipelineError::Failed(
        last_error.unwrap_or_else(|| "没有可用的下载源".to_string()),
    ))
}

/// 校验 + 解包 + 原子落位；任何失败都清理 tgz 与 `*.downloading`。
async fn finish_install(
    target: &Path,
    tgz: &Path,
    exe_tmp: &Path,
    integrity: Option<&str>,
    p: &Progress,
) -> Result<PathBuf, PipelineError> {
    let result = finish_install_inner(target, tgz, exe_tmp, integrity, p).await;
    if result.is_err() {
        cleanup(tgz);
        cleanup(exe_tmp);
    }
    result
}

async fn finish_install_inner(
    target: &Path,
    tgz: &Path,
    exe_tmp: &Path,
    integrity: Option<&str>,
    p: &Progress,
) -> Result<PathBuf, PipelineError> {
    if let Some(sri) = integrity {
        let tgz_path = tgz.to_path_buf();
        let sri = sri.to_string();
        match tokio::task::spawn_blocking(move || verify_integrity(&tgz_path, &sri)).await {
            Ok(Ok(())) => crate::logf("安装：整包 sha512 与 registry integrity 一致"),
            Ok(Err(e)) => return Err(PipelineError::Failed(e)),
            Err(e) => return Err(PipelineError::Failed(format!("校验任务失败: {e}"))),
        }
    }
    if p.cancel.load(Ordering::Acquire) {
        return Err(PipelineError::Cancelled);
    }

    // 解包到 *.downloading；tar 只读 `package/bin/opencode.exe` 这一条。
    let tgz_path = tgz.to_path_buf();
    let exe_path = exe_tmp.to_path_buf();
    match tokio::task::spawn_blocking(move || extract_exe(&tgz_path, &exe_path)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(PipelineError::Failed(e)),
        Err(e) => return Err(PipelineError::Failed(format!("解包任务失败: {e}"))),
    }
    if p.cancel.load(Ordering::Acquire) {
        return Err(PipelineError::Cancelled);
    }
    if !is_complete_exe(exe_tmp, 1) {
        return Err(PipelineError::Failed(
            "解包后的 opencode.exe 不存在或为空".to_string(),
        ));
    }

    // 目标可能是一个中断留下的残片（<100MB）；先删再原子改名。
    let _ = tokio::fs::remove_file(target).await;
    tokio::fs::rename(exe_tmp, target)
        .await
        .map_err(|e| PipelineError::Failed(format!("落位 {} 失败: {e}", target.display())))?;
    cleanup(tgz);
    Ok(target.to_path_buf())
}

// ------------------------------ 下载 + 校验 ------------------------------

/// 取 registry 元数据里的 `dist.integrity`（SRI）。官方源失败再试镜像；都失败返回 None。
async fn fetch_integrity(version: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;
    let urls = [
        format!("https://registry.npmjs.org/{PACKAGE}/{version}"),
        format!("https://registry.npmmirror.com/{PACKAGE}/{version}"),
    ];
    for url in &urls {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(meta) => {
                        if let Some(sri) = meta
                            .get("dist")
                            .and_then(|d| d.get("integrity"))
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                        {
                            return Some(sri.trim().to_string());
                        }
                        crate::logf(&format!("安装：{url} 的元数据里没有 dist.integrity"));
                    }
                    Err(e) => crate::logf(&format!("安装：解析 {url} 元数据失败（{e}）")),
                },
                Err(e) => crate::logf(&format!("安装：读取 {url} 元数据失败（{e}）")),
            },
            Ok(resp) => crate::logf(&format!("安装：取元数据 {url} 返回 HTTP {}", resp.status())),
            Err(e) => crate::logf(&format!("安装：取元数据 {url} 失败（{e}）")),
        }
    }
    None
}

/// 流式下载到 `dest`，边下边更新 `downloaded`。
async fn download(url: &str, dest: &Path, p: &Progress) -> Result<(), PipelineError> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        // 95 MB 的整包下载：给足 30 分钟，避免慢网被误判失败。
        .timeout(Duration::from_secs(1800))
        .build()
        .map_err(|e| PipelineError::Failed(format!("构造下载客户端失败: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| PipelineError::Failed(format!("下载 {url} 失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(PipelineError::Failed(format!(
            "下载 {url} 返回 HTTP {status}"
        )));
    }
    p.downloaded.store(0, Ordering::Relaxed);
    p.total
        .store(resp.content_length().unwrap_or(0), Ordering::Relaxed);
    pump(resp.bytes_stream(), dest, p).await
}

/// 把字节流写进 `dest`。取消时关文件、删临时文件并返回 `Cancelled`。
async fn pump<S, E>(mut stream: S, dest: &Path, p: &Progress) -> Result<(), PipelineError>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| PipelineError::Failed(format!("创建临时文件 {} 失败: {e}", dest.display())))?;
    while let Some(item) = stream.next().await {
        if p.cancel.load(Ordering::Acquire) {
            drop(file);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(PipelineError::Cancelled);
        }
        let chunk = item.map_err(|e| PipelineError::Failed(format!("下载流中断: {e}")))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| PipelineError::Failed(format!("写入临时文件失败: {e}")))?;
        p.downloaded
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    if p.cancel.load(Ordering::Acquire) {
        drop(file);
        let _ = tokio::fs::remove_file(dest).await;
        return Err(PipelineError::Cancelled);
    }
    file.flush()
        .await
        .map_err(|e| PipelineError::Failed(format!("刷新临时文件失败: {e}")))?;
    Ok(())
}

/// 整包 sha512 与 SRI 比对（支持空格分隔的多算法串，只认 `sha512-`）。
fn verify_integrity(path: &Path, integrity: &str) -> Result<(), String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("打开下载包 {} 失败: {e}", path.display()))?;
    let mut hasher = Sha512::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("读取下载包失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    if sri_matches(digest.as_slice(), integrity) {
        Ok(())
    } else {
        Err("下载包的 sha512 与 registry integrity 不一致".to_string())
    }
}

fn sri_matches(digest: &[u8], integrity: &str) -> bool {
    integrity
        .split_whitespace()
        .filter_map(|token| token.strip_prefix("sha512-"))
        .any(|b64| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map(|expected| expected.as_slice() == digest)
                .unwrap_or(false)
        })
}

// ------------------------------ 解包 ------------------------------

/// 只取 `package/bin/opencode.exe`，写到 `dest`；其余条目一律跳过（不落盘、不校验路径）。
fn extract_exe(tgz: &Path, dest: &Path) -> Result<(), String> {
    let file =
        std::fs::File::open(tgz).map_err(|e| format!("打开下载包 {} 失败: {e}", tgz.display()))?;
    let gz = GzDecoder::new(std::io::BufReader::new(file));
    let mut archive = tar::Archive::new(gz);
    let entries = archive
        .entries()
        .map_err(|e| format!("读取 tgz 失败: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("读取 tgz 条目失败: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("读取 tgz 条目路径失败: {e}"))?;
        let normalized = path.to_string_lossy().replace('\\', "/");
        if normalized.trim_start_matches("./") != ENTRY {
            continue;
        }
        let mut out = std::fs::File::create(dest)
            .map_err(|e| format!("创建 {} 失败: {e}", dest.display()))?;
        std::io::copy(&mut entry, &mut out).map_err(|e| format!("解出 {ENTRY} 失败: {e}"))?;
        out.flush()
            .map_err(|e| format!("刷新 {} 失败: {e}", dest.display()))?;
        let len = out.metadata().map(|m| m.len()).unwrap_or(0);
        if len == 0 {
            let _ = std::fs::remove_file(dest);
            return Err(format!("解出的 {ENTRY} 是空文件"));
        }
        return Ok(());
    }
    Err(format!("tgz 里没有 {ENTRY}"))
}

// ------------------------------ 磁盘扫描 ------------------------------

fn is_complete_exe(path: &Path, min_bytes: u64) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() >= min_bytes)
        .unwrap_or(false)
}

/// 扫 `<数据目录>/opencode/*/opencode.exe`，按版本号取最新（与 discover 同口径）。
fn scan_installed(root: &Path, min_bytes: u64) -> Option<(String, PathBuf)> {
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let exe = dir.join(EXE_NAME);
            if is_complete_exe(&exe, min_bytes) {
                let name = dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                candidates.push((name, exe));
            }
        }
    }
    crate::discover::sort_latest_by_version(candidates)
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ------------------------------ 单测 ------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use futures_util::stream;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rs-opencode-install-{tag}-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        std::fs::create_dir_all(&dir).expect("创建测试目录");
        dir
    }

    /// 构造一个小 tgz，含 `package/bin/opencode.exe` 与一条无关条目。
    fn make_tgz(exe_bytes: &[u8]) -> Vec<u8> {
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (name, data) in [
            ("package/package.json", b"{}".as_slice()),
            (ENTRY, exe_bytes),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, data)
                .expect("写入 tar 条目");
        }
        builder
            .into_inner()
            .expect("取回 gzip writer")
            .finish()
            .expect("结束 gzip")
    }

    fn wrong_sri() -> String {
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode([0u8; 64])
        )
    }

    fn good_sri(data: &[u8]) -> String {
        let mut hasher = Sha512::new();
        hasher.update(data);
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(hasher.finalize().as_slice())
        )
    }

    fn leftover_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        name.ends_with(TMP_SUFFIX).then_some(name)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn normal_install_lands_target_and_removes_tmp() {
        let dir = temp_dir("ok");
        let version = "9.9.9-test";
        let install_dir = dir.join("opencode").join(version);
        std::fs::create_dir_all(&install_dir).unwrap();
        let tgz_bytes = make_tgz(b"fake-opencode-exe");
        let tgz = install_dir.join("pkg.tgz.downloading");
        std::fs::write(&tgz, &tgz_bytes).unwrap();
        let target = install_dir.join(EXE_NAME);
        let exe_tmp = install_dir.join(format!("{EXE_NAME}{TMP_SUFFIX}"));
        let p = Progress::default();

        let placed = finish_install(&target, &tgz, &exe_tmp, Some(&good_sri(&tgz_bytes)), &p)
            .await
            .expect("正常包应安装成功");
        assert_eq!(placed, target);
        assert_eq!(std::fs::read(&target).unwrap(), b"fake-opencode-exe");
        assert!(!tgz.exists(), "成功路径应删掉下载包");
        assert!(!exe_tmp.exists(), "临时 exe 应被 rename 走");
        assert!(leftover_files(&install_dir).is_empty());

        // 只解出目标这一条，不落别的文件。
        let names: Vec<String> = std::fs::read_dir(&install_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec![EXE_NAME.to_string()], "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn integrity_mismatch_fails_without_leftovers() {
        let dir = temp_dir("bad-sri");
        let install_dir = dir.join("opencode").join("2.0.32-test");
        std::fs::create_dir_all(&install_dir).unwrap();

        let tgz_bytes = make_tgz(b"not-the-right-bytes");
        let tgz = install_dir.join("pkg.tgz.downloading");
        std::fs::write(&tgz, &tgz_bytes).unwrap();
        let target = install_dir.join(EXE_NAME);
        let exe_tmp = install_dir.join(format!("{EXE_NAME}{TMP_SUFFIX}"));
        let p = Progress::default();

        let err = finish_install(&target, &tgz, &exe_tmp, Some(&wrong_sri()), &p)
            .await
            .expect_err("校验必须失败");
        assert!(err.to_string().contains("sha512"), "{err}");
        assert!(leftover_files(&install_dir).is_empty(), "残留临时文件");
        assert!(!tgz.exists(), "失败必须删掉下载包");
        assert!(!exe_tmp.exists());
        assert!(!target.exists(), "失败不得落位");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_integrity_skips_check_and_still_installs() {
        let dir = temp_dir("no-sri");
        let install_dir = dir.join("opencode").join("2.0.32-test");
        std::fs::create_dir_all(&install_dir).unwrap();
        let tgz = install_dir.join("pkg.tgz.downloading");
        std::fs::write(&tgz, make_tgz(b"unsigned")).unwrap();
        let target = install_dir.join(EXE_NAME);
        let exe_tmp = install_dir.join(format!("{EXE_NAME}{TMP_SUFFIX}"));
        let p = Progress::default();

        finish_install(&target, &tgz, &exe_tmp, None, &p)
            .await
            .expect("没有 integrity 时允许跳过校验");
        assert_eq!(std::fs::read(&target).unwrap(), b"unsigned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sri_accepts_correct_digest_and_multiple_algorithms() {
        let data = b"hello world";
        let mut hasher = Sha512::new();
        hasher.update(data);
        let digest = hasher.finalize();
        let good = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(digest.as_slice())
        );
        assert!(sri_matches(digest.as_slice(), &good));
        assert!(sri_matches(digest.as_slice(), &format!("sha1-AAAA {good}")));
        assert!(!sri_matches(digest.as_slice(), &wrong_sri()));
        assert!(!sri_matches(digest.as_slice(), "sha1-AAAA"));
    }

    #[tokio::test]
    async fn cancel_stops_stream_and_removes_tmp_file() {
        let dir = temp_dir("cancel");
        let dest = dir.join("pkg.tgz.downloading");
        let p = Progress::default();
        // 第 2 块数据产生时置取消位；下一轮循环顶部必须退出。
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = (0..8)
            .map(|i| {
                if i == 1 {
                    p.cancel.store(true, Ordering::Release);
                }
                Ok(bytes::Bytes::from(vec![i as u8; 16]))
            })
            .collect();
        let err = pump(stream::iter(chunks), &dest, &p)
            .await
            .expect_err("取消后必须返回错误");
        assert_eq!(err, PipelineError::Cancelled);
        assert!(!dest.exists(), "取消后临时文件必须删除");
        assert!(p.downloaded.load(Ordering::Relaxed) < 8 * 16, "应在中途停");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cancel_before_any_chunk_leaves_no_file() {
        let dir = temp_dir("cancel-early");
        let dest = dir.join("pkg.tgz.downloading");
        let p = Progress::default();
        p.cancel.store(true, Ordering::Release);
        let err = pump(
            stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                b"x",
            ))]),
            &dest,
            &p,
        )
        .await
        .expect_err("已取消时必须直接退出");
        assert_eq!(err, PipelineError::Cancelled);
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_target_is_recognized_as_installed() {
        let dir = temp_dir("idempotent");
        let target = dir.join(EXE_NAME);
        std::fs::write(&target, b"0123456789").unwrap();
        // 真实阈值 100MB 太大，这里用缩小的阈值验证判据本身。
        assert!(is_complete_exe(&target, 4));
        assert!(!is_complete_exe(&target, 11));
        assert!(!is_complete_exe(&dir.join("missing.exe"), 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_picks_latest_version_and_ignores_partial() {
        let dir = temp_dir("scan");
        for (name, size) in [("2.0.9", 8u64), ("2.0.32", 8), ("3.0.0", 2)] {
            let vdir = dir.join(name);
            std::fs::create_dir_all(&vdir).unwrap();
            std::fs::write(vdir.join(EXE_NAME), vec![0u8; size as usize]).unwrap();
        }
        let found = scan_installed(&dir, 4).expect("应有已安装版本");
        assert_eq!(found.0, "2.0.32", "3.0.0 只有 2 字节，应被当成残片忽略");
        assert_eq!(found.1, dir.join("2.0.32").join(EXE_NAME));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_validation_rejects_path_tricks() {
        assert!(valid_version("2.0.32"));
        assert!(valid_version("2.0.32-test_1"));
        assert!(!valid_version(""));
        assert!(!valid_version("../evil"));
        assert!(!valid_version("a/b"));
        assert!(!valid_version(".hidden"));
    }
}
