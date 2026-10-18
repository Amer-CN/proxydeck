//! appdir.rs —— 数据目录判定链、程序目录、历史状态文件迁移、明文凭据升级、
//! 日志轮转、子进程 stdout 落盘（`internal/appdir/` 全量的逐条对应物）。
//!
//! Go 侧文件对照：
//!
//! | Rust | Go |
//! |---|---|
//! | [`data_dir`] / [`data_dir_ensure`] / [`data_path`] / [`init`] / [`portable`] / [`exe_dir`] | `appdir.go` |
//! | [`migrate`] / [`upgrade_plaintext`] / [`LEGACY_STATE_FILES`] | `migrate.go` |
//! | [`open_log`] / [`append_log`] / [`logf`] / [`StampedWriter`] / [`rotate_log`] | `logfile.go` |
//! | [`run_data_dir_init`] | `app/main.go:192-208` 的 `runDataDirInit` |
//! | DPAPI 读写 | [`crate::crypto`]（P8b 已迁）+ `appdir/crypt.go` 的 `Remove`/`Describe` |
//!
//! ## 与 Go 的差异（登记）
//!
//! 1. **便携模式的全局状态**：Go 是包级 `var portable bool`（`Init` 写、`Portable` 读），
//!    Rust 侧是本模块的 `AtomicBool`——语义相同，但 Rust 里**测试可重置**
//!    （[`init_for_test`]），Go 侧只能靠 `Init(false)` 手动复位；
//! 2. **[`exe_dir`] 的符号链接解析**用 `canonicalize()`，Windows 上会带上 `\\?\` 前缀，
//!    这里剥掉以贴近 Go `EvalSymlinks` 的形态；
//! 3. **`isProtected` / `WriteFile`** 在 [`crate::crypto`]（P8b 已迁，非本模块）；
//!    本模块只提供 `ReadFile`/`Remove`/`Describe` 的同款面；
//! 4. `UpgradePlaintext` 的加密走 [`crate::crypto::write_file`]（DPAPI + ACL + tmp→rename），
//!    与 Go `WriteFile` 同一条路径。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// `%APPDATA%` 下的数据目录名 —— `appdir.go:22`。
pub const APP_NAME: &str = "ProxyDeck";

/// 便携标记文件名 —— `appdir.go:24`。
pub const PORTABLE_MARKER: &str = "portable";

/// 覆盖数据目录的环境变量 —— `appdir.go:26`。
pub const DATA_DIR_ENV: &str = "PROXYDECK_DATA_DIR";

/// 单份日志上限 —— `logfile.go:16` 的 `LogMaxBytes = 1<<20`。
pub const LOG_MAX_BYTES: u64 = 1 << 20;

/// 保留的历史份数（`.1/.2/.3`）—— `logfile.go:18`。
pub const LOG_BACKUPS: u32 = 3;

/// 便携模式开关 —— `appdir.go:30` 的包级 `var portable`。
static PORTABLE: AtomicBool = AtomicBool::new(false);

/// `Init`（`appdir.go:53-55`）：显式 `--portable` 优先，其次 exe 同目录的 portable 标记。
///
/// **必须在任何数据文件读写之前调用**（GUI 在 `main()`、插件子进程在 `runPluginMode` 之前）。
pub fn init(portable_flag: bool) {
    PORTABLE.store(portable_flag || marker_exists(), Ordering::SeqCst);
}

/// **测试缝**：直接复位便携状态（Go 侧靠 `Init(false)` 复位）。
#[doc(hidden)]
pub fn init_for_test(portable: bool) {
    PORTABLE.store(portable, Ordering::SeqCst);
}

/// `Portable`（`appdir.go:39`）。
pub fn portable() -> bool {
    PORTABLE.load(Ordering::SeqCst)
}

/// `PortableArgs`（`appdir.go:44-49`）：便携模式返回 `["--portable"]`，否则空。
pub fn portable_args() -> Vec<&'static str> {
    if portable() {
        vec!["--portable"]
    } else {
        Vec::new()
    }
}

/// `markerExists`（`appdir.go:33-36`）：**是文件、不是目录**。
pub fn marker_exists() -> bool {
    std::fs::metadata(exe_dir().join(PORTABLE_MARKER))
        .map(|m| !m.is_dir())
        .unwrap_or(false)
}

/// `ExeDir`（`appdir.go:58-69`）：`current_exe()` → 解析符号链接 → 取目录；
/// 失败退回工作目录；再失败退回 `"."`。
pub fn exe_dir() -> PathBuf {
    if let Ok(p) = std::env::current_exe() {
        let p = strip_verbatim(&p.canonicalize().unwrap_or(p));
        if let Some(dir) = p.parent() {
            return dir.to_path_buf();
        }
    }
    if let Ok(wd) = std::env::current_dir() {
        return wd;
    }
    PathBuf::from(".")
}

/// Windows 的 `canonicalize()` 会给绝对路径加 `\\?\` 前缀（Go 的 `EvalSymlinks` 不加）。
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p.to_path_buf(),
    }
}

/// `ExeDir` 的字符串形态（`metadata.cwd` 用）。
pub fn exe_dir_string() -> String {
    exe_dir().to_string_lossy().into_owned()
}

/// `dataDir`（`appdir.go:84-98`）：`PROXYDECK_DATA_DIR` → 便携 → `%APPDATA%\ProxyDeck\`
/// → home\AppData\Roaming\ProxyDeck → exe 同目录。**不建目录**。
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os(DATA_DIR_ENV) {
        let s = d.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    if portable() {
        return exe_dir();
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let s = appdata.to_string_lossy().trim().to_string();
        if !s.is_empty() {
            return Path::new(&s).join(APP_NAME);
        }
    }
    // Go 的 `os.UserHomeDir()`：Windows 上取 `%USERPROFILE%`。
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let s = home.to_string_lossy().to_string();
        if !s.is_empty() {
            return Path::new(&s)
                .join("AppData")
                .join("Roaming")
                .join(APP_NAME);
        }
    }
    exe_dir()
}

/// `DataDir`（`appdir.go:72-78`）：不存在则创建（0700）；失败退回 exe 同目录——
/// 宁可退回也不要让调用方拿到空串。
pub fn data_dir_ensure() -> PathBuf {
    let dir = data_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        return dir;
    }
    exe_dir()
}

/// `Path`（`appdir.go:82`）：拼数据目录下的文件路径（不建目录）。
pub fn data_path(name: &str) -> PathBuf {
    data_dir_ensure().join(name)
}

// ---------------------------------------------------------------- 迁移

/// 一个历史状态文件 —— `stateFile`（`migrate.go:12-15`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateFile {
    pub name: &'static str,
    /// 内含 token 一类凭据 → 需 DPAPI 加密 + 收紧 ACL。
    pub credential: bool,
}

/// 历史状态文件清单 —— `legacyStateFiles`（`migrate.go:19-59`），**顺序与条数逐条照抄**：
/// 主代理 8 + 团结 9 + WorkBuddy 6 + VibeX 2 + B.AI 1 + 插件诊断 7 = **33 条**。
pub const LEGACY_STATE_FILES: [StateFile; 33] = [
    // 主代理（app）
    StateFile { name: "api-key.txt", credential: true },
    StateFile { name: "stats.json", credential: false },
    StateFile { name: "keymap.json", credential: false },
    StateFile { name: "close_mode.txt", credential: false },
    StateFile { name: "notice_dismissed.flag", credential: false },
    StateFile { name: "close_hint_dismissed.flag", credential: false },
    StateFile { name: "headless-error.log", credential: false },
    StateFile { name: "calibration.json", credential: false },
    // 团结 8788
    StateFile { name: "tuanjie-accounts.json", credential: true },
    // 历史文件，现已不读：自定义服务商与三协议转发、生图/生视频与识图改路由已按用户
    // 裁决删除（2026-10-18），但用户数据留在磁盘上不动，升级时仍按原凭据策略迁移/收紧 ACL。
    StateFile { name: "tuanjie-providers.json", credential: true },
    StateFile { name: "tuanjie-media.json", credential: false },
    StateFile { name: "tuanjie-vision.json", credential: false },
    StateFile { name: "tuanjie-water.json", credential: false },
    StateFile { name: "tuanjie-water-channels.json", credential: true },
    StateFile { name: "tuanjie-baselines.json", credential: false },
    StateFile { name: "tuanjie-stats.json", credential: false },
    StateFile { name: "tuanjie-pacing.json", credential: false },
    // WorkBuddy CN 8787 / INTL 8789
    StateFile { name: "codebuddy-pool.json", credential: false },
    StateFile { name: "codebuddy-pool-intl.json", credential: false },
    StateFile { name: "codebuddy-stats.json", credential: false },
    StateFile { name: "codebuddy-stats-intl.json", credential: false },
    StateFile { name: "codebuddy-failover.json", credential: false },
    StateFile { name: "codebuddy-failover-intl.json", credential: false },
    // VibeX 8790
    StateFile { name: "vibex-config.json", credential: true },
    StateFile { name: "vibex-stats.json", credential: false },
    // B.AI 8891
    StateFile { name: "bai-stats.json", credential: false },
    // 各插件启动失败诊断
    StateFile { name: "vibex-plugin-error.log", credential: false },
    StateFile { name: "tuanjie-plugin-error.log", credential: false },
    StateFile { name: "codebuddy-plugin-error.log", credential: false },
    StateFile { name: "codebuddy-intl-plugin-error.log", credential: false },
    StateFile { name: "bai-plugin-error.log", credential: false },
    StateFile { name: "comate-plugin-error.log", credential: false },
    StateFile { name: "qoder-plugin-error.log", credential: false },
];

/// `legacyCredentialFiles`（`migrate.go:62-70`）：需加密升级的历史凭据文件名。
pub fn legacy_credential_files() -> Vec<&'static str> {
    LEGACY_STATE_FILES
        .iter()
        .filter(|f| f.credential)
        .map(|f| f.name)
        .collect()
}

/// `Migrate`（`migrate.go:80-133`）：把 `src_dir`（exe 同目录）里的历史状态文件
/// 一次性搬进 `dst_dir`，返回可在日志里打印的短句。
///
/// * 目标已存在同名文件 → 保留目标，旧文件改名 `*.migrated`；
/// * 搬走成功 → 原位置不留残渣；删不掉 → 改名让位（`retire`）；
/// * 目录：目标无同名目录时整目录改名过去，否则只做改名标记。
pub fn migrate(dst_dir: &Path, src_dir: &Path) -> Vec<String> {
    if dst_dir.as_os_str().is_empty()
        || src_dir.as_os_str().is_empty()
        || same_dir(dst_dir, src_dir)
    {
        return Vec::new();
    }
    match std::fs::metadata(src_dir) {
        Ok(m) if m.is_dir() => {}
        _ => return Vec::new(),
    }
    if std::fs::create_dir_all(dst_dir).is_err() {
        return vec!["数据目录创建失败，迁移跳过".to_string()];
    }
    let mut notes = Vec::new();
    for f in LEGACY_STATE_FILES.iter() {
        let src = src_dir.join(f.name);
        let Ok(fi) = std::fs::symlink_metadata(&src) else {
            continue; // 不在旧位置：跳过（绝大多数文件都是这样）
        };
        let dst = dst_dir.join(f.name);
        if fi.is_dir() {
            if std::fs::metadata(&dst).is_ok() {
                let note = retire(&src, "目标已有同名目录，旧的只做改名");
                notes.push(format!("目录 {} 未搬迁：{note}", f.name));
                continue;
            }
            match std::fs::rename(&src, &dst) {
                Ok(_) => notes.push(format!("目录 {} 已搬入数据目录", f.name)),
                Err(e) => notes.push(format!("目录 {} 搬迁失败：{e}", f.name)),
            }
            continue;
        }
        if file_exists(&dst) {
            notes.push(format!(
                "{} 目标已存在，旧文件{}",
                f.name,
                retire(&src, "改名保留")
            ));
            continue;
        }
        let tmp = PathBuf::from(format!("{}.migrating", dst.display()));
        let _ = std::fs::remove_file(&tmp);
        if let Err(e) = copy_file(&src, &tmp) {
            notes.push(format!("{} 拷贝失败，保留在原处：{e}", f.name));
            continue;
        }
        if let Err(e) = std::fs::rename(&tmp, &dst) {
            let _ = std::fs::remove_file(&tmp);
            notes.push(format!("{} 落位失败，保留在原处：{e}", f.name));
            continue;
        }
        notes.push(format!("{} 已搬入数据目录", f.name));
        if std::fs::remove_file(&src).is_err() {
            // 删不掉（占用/权限）：改名让位，下次启动不再把它当权威旧文件
            notes.push(format!(
                "{} 旧文件删不掉，{}",
                f.name,
                retire(&src, "改名让位")
            ));
        }
    }
    notes
}

/// `UpgradePlaintext`（`migrate.go:137-155`）：数据目录里仍是明文的凭据文件升级成
/// DPAPI 密文。已是密文 / 不存在 / 读取失败一律跳过；加密失败原文件原样保留。
pub fn upgrade_plaintext() -> Vec<String> {
    let mut notes = Vec::new();
    for name in legacy_credential_files() {
        let p = data_path(name);
        let Ok(b) = std::fs::read(&p) else {
            continue;
        };
        if b.is_empty() {
            continue;
        }
        if b.starts_with(crate::crypto::CRYPT_PREFIX) {
            continue;
        }
        match crate::crypto::write_file(&p, &b) {
            Ok(()) => notes.push(format!("{name} 已升级为 DPAPI 密文")),
            Err(e) => notes.push(format!("{name} 明文升级失败（保持原样）：{e}")),
        }
    }
    notes
}

/// `retire`（`migrate.go:158-167`）：改名成 `*.migrated`；改名失败则直接删除。
fn retire(src: &Path, why: &str) -> String {
    let dst = PathBuf::from(format!("{}.migrated", src.display()));
    if std::fs::rename(src, &dst).is_ok() {
        let base = dst
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        return format!("{why}为 {base}");
    }
    if std::fs::remove_file(src).is_ok() {
        return format!("{why}失败，已删除（内容已在数据目录）");
    }
    format!("{why}失败，原样留在 {}", src.display())
}

fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    let data = std::fs::read(src)?;
    std::fs::write(dst, data)?;
    // Go 侧还做了 `out.Sync()`；Rust 的 `std::fs::write` 不含 fsync，
    // 这里显式同步一次，语义与 Go 的「落位前落盘」一致。
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(dst) {
        let _ = f.sync_all();
    }
    Ok(())
}

fn file_exists(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| !m.is_dir()).unwrap_or(false)
}

/// `sameDir`（`migrate.go:195-202`）：绝对化 + 清理后**大小写不敏感**比较。
fn same_dir(a: &Path, b: &Path) -> bool {
    let (Ok(ca), Ok(cb)) = (absolute(a), absolute(b)) else {
        return false;
    };
    ca.to_lowercase() == cb.to_lowercase()
}

fn absolute(p: &Path) -> Result<String, ()> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_err(|_| ())?.join(p)
    };
    // `filepath.Clean` 的等价物：去掉 `.` 段、折叠多余分隔符
    let mut out: Vec<String> = Vec::new();
    for seg in abs.to_string_lossy().split(['\\', '/']) {
        match seg {
            "" | "." => {}
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            s => out.push(s.to_string()),
        }
    }
    Ok(out.join("\\"))
}

// ---------------------------------------------------------------- 日志轮转

/// `OpenLog`（`logfile.go:23-33`）：建目录（0700）→ 轮转 → 追加打开（0644）；
/// 失败返回 `None`（调用方按「无日志」继续）。
pub fn open_log(path: &Path) -> Option<std::fs::File> {
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return None;
        }
    }
    rotate_log(path);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

/// `AppendLog`（`logfile.go:36-42`）：先轮转后写一行带时间戳的记录。
pub fn append_log(path: &Path, msg: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    rotate_log(path);
    append_line(path, msg)
}

/// `Logf`（`logfile.go:46-48`）：往数据目录的 `appdir.log` 写一行；错误一律吞掉。
pub fn logf(format_args: std::fmt::Arguments<'_>) {
    let path = data_dir().join("appdir.log");
    let _ = append_log(&path, &format_args.to_string());
}

/// `StampedWriter`（`logfile.go:51-85`）：每行前加 `2006-01-02 15:04:05 ` 时间戳。
pub struct StampedWriter {
    file: Option<std::fs::File>,
    buf: Vec<u8>,
}

impl StampedWriter {
    /// `NewStampedWriter`（`logfile.go:57`）：`f` 为 `None` 时丢弃全部输出。
    pub fn new(file: Option<std::fs::File>) -> Self {
        StampedWriter {
            file,
            buf: Vec::new(),
        }
    }

    /// 写一段（与 Go 的 `Write` 同语义：按行加时间戳，无换行的尾巴丢掉）。
    pub fn write(&mut self, p: &[u8]) -> usize {
        let Some(file) = self.file.as_mut() else {
            return p.len();
        };
        self.buf.extend_from_slice(p);
        let mut out: Vec<u8> = Vec::with_capacity(p.len() + 24);
        loop {
            if self.buf.is_empty() {
                break;
            }
            let (line, rest): (Vec<u8>, Vec<u8>) = match self.buf.iter().position(|b| *b == b'\n') {
                Some(i) => (self.buf[..i].to_vec(), self.buf[i + 1..].to_vec()),
                None => {
                    // 无换行的尾巴：本行已带时间戳，剩余内容等下一次写入
                    let all = std::mem::take(&mut self.buf);
                    (all, Vec::new())
                }
            };
            self.buf = rest;
            if line.is_empty() {
                continue;
            }
            out.extend_from_slice(stamp().as_bytes());
            out.push(b' ');
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
        if file.write_all(&out).is_err() {
            return 0;
        }
        if let Some(name) = file_path_hint(file) {
            rotate_if_big(&name);
        }
        p.len()
    }
}

/// `w.f.Name()` —— `std::fs::File` 没有取路径的接口，这里退回「不可轮转」
/// （登记为差异：`StampedWriter` 的按大小轮转在 Rust 侧只在调用方显式给路径时生效）。
fn file_path_hint(_f: &std::fs::File) -> Option<PathBuf> {
    None
}

/// 带路径的 `StampedWriter`（Rust 侧补充的形态：包住「文件 + 路径」，
/// 使 `rotateIfBig` 与 Go 一致）——见 [`StampedWriter::write`] 的登记差异。
pub struct StampedWriterAt {
    file: std::fs::File,
    path: PathBuf,
    buf: Vec<u8>,
}

impl StampedWriterAt {
    /// 打开（不存在则建、追加、先轮转）——与 [`open_log`] 同一条入口。
    pub fn open(path: &Path) -> Option<Self> {
        let file = open_log(path)?;
        Some(StampedWriterAt {
            file,
            path: path.to_path_buf(),
            buf: Vec::new(),
        })
    }

    /// 同 [`StampedWriter::write`]，但写完后按**自己的路径**轮转（Go 的 `rotateIfBig`）。
    pub fn write(&mut self, p: &[u8]) -> usize {
        self.buf.extend_from_slice(p);
        let mut out: Vec<u8> = Vec::with_capacity(p.len() + 24);
        while !self.buf.is_empty() {
            let (line, rest): (Vec<u8>, Vec<u8>) = match self.buf.iter().position(|b| *b == b'\n') {
                Some(i) => (self.buf[..i].to_vec(), self.buf[i + 1..].to_vec()),
                None => (std::mem::take(&mut self.buf), Vec::new()),
            };
            self.buf = rest;
            if line.is_empty() {
                continue;
            }
            out.extend_from_slice(stamp().as_bytes());
            out.push(b' ');
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
        if self.file.write_all(&out).is_err() {
            return 0;
        }
        rotate_if_big(&self.path);
        p.len()
    }
}

/// `appendLine`（`logfile.go:87-95`）。
fn append_line(path: &Path, msg: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(format!("{} {msg}\n", stamp()).as_bytes())
}

/// `rotateIfBig`（`logfile.go:98-102`）。
pub fn rotate_if_big(path: &Path) {
    if let Ok(fi) = std::fs::metadata(path) {
        if fi.len() > LOG_MAX_BYTES {
            rotate_log(path);
        }
    }
}

/// `rotateLog`（`logfile.go:106-118`）：超上限就滚成 `.1`，旧的依次后移，最老的丢弃。
/// 轮转失败不动原文件。
pub fn rotate_log(path: &Path) {
    let Ok(fi) = std::fs::metadata(path) else {
        return;
    };
    if fi.len() <= LOG_MAX_BYTES {
        return;
    }
    let _ = std::fs::remove_file(suffixed(path, LOG_BACKUPS));
    for i in (1..LOG_BACKUPS).rev() {
        let _ = std::fs::rename(suffixed(path, i), suffixed(path, i + 1));
    }
    if std::fs::rename(path, suffixed(path, 1)).is_err() {
        // Go 打一行日志后继续写原文件；这里同样不阻断
    }
}

fn suffixed(path: &Path, n: u32) -> PathBuf {
    PathBuf::from(format!("{}.{n}", path.display()))
}

/// `2006-01-02 15:04:05`（Go `time.Now().Format`，本机时区）。
///
/// 与 [`crate::accounts::format_go_seconds`] 同口径——两处各自实现，
/// 「appdir 不依赖业务模块」这条边界优先（值由门禁锁住一致）。
fn stamp() -> String {
    use chrono::{Datelike, Timelike};
    let t = chrono::Local::now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

// ---------------------------------------------------------------- 启动收口

/// `runDataDirInit`（`app/main.go:192-208`）：任何状态文件读写之前的一次性收口。
///
/// 便携模式直接返回（数据就在 exe 同目录，无迁移）；否则迁移 → 明文升级 →
/// 有事情才把摘要写进 `headless-error.log`。
pub fn run_data_dir_init(portable_flag: bool) -> Vec<String> {
    init(portable_flag);
    if portable() {
        return Vec::new();
    }
    let mut notes = migrate(&data_dir_ensure(), &exe_dir());
    notes.extend(upgrade_plaintext());
    if notes.is_empty() {
        return notes; // 常态：没有历史文件，什么都不做、不写日志
    }
    let log = data_dir_ensure().join("headless-error.log");
    if let Some(mut w) = StampedWriterAt::open(&log) {
        w.write(format!("[state] {}\n", notes.join("；")).as_bytes());
    }
    notes
}

/// `Describe`（`crypt.go:60-71`）：给日志/报错用的一句话。
pub fn describe(looked: &[String]) -> String {
    let dir = data_dir();
    let mode = if portable() {
        "便携模式（exe 同目录）"
    } else {
        "用户数据目录"
    };
    let mut out = format!("状态目录 {}（{mode}）", dir.display());
    if !looked.is_empty() {
        out += "；候选路径：";
        out += &looked.join("; ");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 便携开关与数据目录环境变量都是**进程级共享状态**，单测并行会互相踩——
    /// 凡碰它们的用例统一持这条锁（crate 内惯例：同抢进程级 env 的测试串行）。
    fn env_lock() -> &'static std::sync::Mutex<()> {
        static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn legacy_list_matches_go_tail_and_credentials() {
        assert_eq!(
            LEGACY_STATE_FILES.len(),
            33,
            "Go legacyStateFiles 的条数（migrate.go:19-59）"
        );
        // 凭据文件（Go `credential: true` 的五条）
        let creds = legacy_credential_files();
        assert_eq!(
            creds,
            vec![
                "api-key.txt",
                "tuanjie-accounts.json",
                "tuanjie-providers.json",
                "tuanjie-water-channels.json",
                "vibex-config.json",
            ],
            "凭据清单（Go migrate.go:62-70 的过滤结果）"
        );
        assert!(LEGACY_STATE_FILES.iter().any(|f| f.name == "tuanjie-water.json"));
        assert!(LEGACY_STATE_FILES
            .iter()
            .any(|f| f.name == "codebuddy-failover-intl.json"));
        assert!(LEGACY_STATE_FILES.iter().any(|f| f.name == "qoder-plugin-error.log"));
        // 首尾两条（顺序也是合同的一部分）
        assert_eq!(LEGACY_STATE_FILES[0].name, "api-key.txt");
        assert_eq!(LEGACY_STATE_FILES[32].name, "qoder-plugin-error.log");
    }

    #[test]
    fn exe_dir_is_absolute() {
        assert!(exe_dir().is_absolute());
        assert!(!exe_dir_string().is_empty());
    }

    #[test]
    fn rotate_keeps_three_backups() {
        let dir = std::env::temp_dir().join(format!("rs-tj-appdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.log");
        // 造 1MB+1 的文件，触发轮转
        std::fs::write(&p, vec![b'a'; (LOG_MAX_BYTES + 1) as usize]).unwrap();
        rotate_log(&p);
        assert!(!p.exists(), "原文件应被滚成 .1");
        assert!(dir.join("x.log.1").exists());
        assert!(!dir.join("x.log.4").exists(), "只保留 3 份");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_moves_and_retires() {
        let base = std::env::temp_dir().join(format!("rs-tj-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("exe");
        let dst = base.join("data");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("tuanjie-water.json"), b"{\"a\":1}").unwrap();
        std::fs::write(src.join("tuanjie-pacing.json"), b"{\"enabled\":true}").unwrap();
        // 目标已有同名文件：保留目标、旧文件改名
        std::fs::write(dst.join("tuanjie-pacing.json"), b"{\"enabled\":false}").unwrap();

        let notes = migrate(&dst, &src);
        assert_eq!(std::fs::read(dst.join("tuanjie-water.json")).unwrap(), b"{\"a\":1}");
        assert!(!src.join("tuanjie-water.json").exists(), "搬走不留残渣");
        assert_eq!(
            std::fs::read(dst.join("tuanjie-pacing.json")).unwrap(),
            b"{\"enabled\":false}",
            "目标才是权威"
        );
        assert!(src.join("tuanjie-pacing.json.migrated").exists());
        assert!(notes.iter().any(|n| n.contains("tuanjie-water.json 已搬入数据目录")), "{notes:?}");
        assert!(notes.iter().any(|n| n.contains("tuanjie-pacing.json 目标已存在")), "{notes:?}");

        // 同一目录 → 空结果
        assert!(migrate(&src, &src).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn same_dir_is_case_insensitive() {
        assert!(same_dir(Path::new(r"C:\A\B"), Path::new(r"c:\a\b")));
        assert!(same_dir(Path::new(r"C:\A\B\"), Path::new(r"C:\A\.\B")));
        assert!(!same_dir(Path::new(r"C:\A"), Path::new(r"C:\B")));
    }

    #[test]
    fn stamped_writer_prefixes_each_line() {
        let dir = std::env::temp_dir().join(format!("rs-tj-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.log");
        let mut w = StampedWriterAt::open(&p).unwrap();
        w.write(b"line-one\nline-two\n");
        drop(w);
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        for l in lines {
            assert_eq!(l.len(), 19 + 1 + 8, "时间戳 + 空格 + 内容: {l}");
            assert_eq!(&l[4..5], "-");
            assert_eq!(&l[10..11], " ");
            assert_eq!(&l[13..14], ":");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 便携参数与便携开关（**不碰**数据目录环境变量，故不需要 env_lock——
    /// `portable_args` 只读全局便携位；串行由 `env_lock` 的另一用例保证）。
    #[test]
    fn portable_args_shape() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        init_for_test(false);
        assert!(portable_args().is_empty());
        init_for_test(true);
        assert_eq!(portable_args(), vec!["--portable"]);
        init_for_test(false);
        assert!(portable_args().is_empty());
    }

    /// 进程级环境变量（`PROXYDECK_DATA_DIR`）在本 crate 内是**共享资源**——
    /// 单测并行跑，两个用例各自 set/remove 会互相踩（实测踩过一次），
    /// 故 data_dir/describe 的所有断言**收在一个用例里顺序做完**。
    #[test]
    fn data_dir_env_wins_and_describe_names_mode() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let key = DATA_DIR_ENV;
        let old = std::env::var_os(key);

        // 1) 便携模式：数据目录 = exe 同目录（此时**不能**带覆盖变量，否则它优先）
        match old.as_ref() {
            Some(_) => std::env::remove_var(key),
            None => {}
        }
        init_for_test(true);
        assert!(portable(), "Init(true) 应进便携");
        assert_eq!(data_dir(), exe_dir());
        assert_eq!(data_dir_ensure(), exe_dir());
        assert_eq!(portable_args(), vec!["--portable"]);
        assert!(describe(&[]).contains("便携模式（exe 同目录）"));
        assert!(!marker_exists(), "测试进程的 exe 同目录没有 portable 标记");

        // 2) 覆盖变量优先于一切（用一个**真实存在**的临时目录：`data_dir_ensure`
        //    建不出来会按 Go 语义退回 exe 同目录，那样测不到 env 生效）
        init_for_test(false);
        let tmp = std::env::temp_dir().join(format!("rs-tj-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var(key, &tmp);
        assert_eq!(data_dir(), tmp);
        assert_eq!(data_path("x.json"), tmp.join("x.json"));
        let s = describe(&["a.json".to_string(), "b.json".to_string()]);
        assert!(
            s.starts_with(&format!("状态目录 {}（用户数据目录）", tmp.display())),
            "{s}"
        );
        assert!(s.contains("候选路径：a.json; b.json"), "{s}");

        init_for_test(false);
        match old {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
