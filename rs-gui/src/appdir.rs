//! appdir.rs —— 数据目录与状态文件读写（对应 Go `internal/appdir` 的 GUI 侧常用部分）。
//!
//! 判定链与 Go **逐条同源**（`appdir.go`）：
//!   1. `PROXYDECK_DATA_DIR` 环境变量覆盖（单测/临时实例用）
//!   2. 便携模式（显式 `--portable` 或 exe 同目录的 `portable` 标记文件，且必须是文件）→ exe 同目录
//!   3. `%APPDATA%\ProxyDeck\`
//!   4. 兜底 `%USERPROFILE%\AppData\Roaming\ProxyDeck`
//!
//! 写侧（`write_file`）照抄 Go `appdir.WriteFile`：**一律 DPAPI 加密落盘**
//! （`PDAPI1\n` + base64 + `\n`）、写临时文件 + 改名（不截断既有数据）、
//! 顺手用 `icacls` 给当前用户补一条显式授权（失败只留痕、不阻断）。
//!
//! 未实现（登记在 README，属 P7c 的 `main.go` 启动收口）：`Migrate`（exe 同目录历史状态
//! 文件搬进数据目录）与 `UpgradePlaintext`（存量明文凭据升级为密文）。插件子进程日志用的
//! `OpenLog` / `StampedWriter` 已在本文件实现（P7b 落地，6 条黄金用例锁定）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::dpapi;

/// `%APPDATA%` 下的数据目录名（Go `AppName`）。
pub const APP_NAME: &str = "ProxyDeck";
/// exe 同目录里的便携标记文件名（Go `PortableMarker`）。
pub const PORTABLE_MARKER: &str = "portable";
/// 覆盖数据目录的环境变量（Go `DataDirEnv`）。
pub const DATA_DIR_ENV: &str = "PROXYDECK_DATA_DIR";

static PORTABLE: AtomicBool = AtomicBool::new(false);

/// exe 同目录是否存在 portable 标记（**必须是文件、不能是目录**，同 Go `markerExists`）。
pub fn marker_exists(exe_dir: &Path) -> bool {
    match std::fs::metadata(exe_dir.join(PORTABLE_MARKER)) {
        Ok(md) => !md.is_dir(),
        Err(_) => false,
    }
}

/// 解析便携模式（显式 flag 优先，其次标记文件）。必须在任何状态文件读写之前调用。
pub fn init(portable_flag: bool) {
    PORTABLE.store(portable_flag || marker_exists(&exe_dir()), Ordering::SeqCst);
}

/// 当前是否便携模式。
pub fn portable() -> bool {
    PORTABLE.load(Ordering::SeqCst)
}

/// 要传给子进程（`-headless` / `--plugin-*` / `--fax`）的便携参数。
pub fn portable_args() -> Vec<String> {
    if portable() {
        vec!["--portable".to_string()]
    } else {
        Vec::new()
    }
}

/// exe 所在目录（Go `ExeDir`：exe 定位失败退回工作目录）。
pub fn exe_dir() -> PathBuf {
    if let Ok(p) = std::env::current_exe() {
        if let Some(d) = p.parent() {
            return d.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn data_dir_raw() -> PathBuf {
    if let Ok(d) = std::env::var(DATA_DIR_ENV) {
        let d = d.trim().to_string();
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if portable() {
        return exe_dir();
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            return PathBuf::from(appdata).join(APP_NAME);
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join("AppData")
                .join("Roaming")
                .join(APP_NAME);
        }
    }
    exe_dir()
}

/// 数据目录（不存在则创建；建不出来退回 exe 同目录——宁可退回也不要让调用方拿到空串）。
pub fn data_dir() -> PathBuf {
    let dir = data_dir_raw();
    if std::fs::create_dir_all(&dir).is_err() {
        return exe_dir();
    }
    dir
}

/// 拼一个数据目录下的文件路径（`name` 里的 `/` 按平台分隔符展开，语义同 Go `filepath.Join`）。
pub fn path(name: &str) -> PathBuf {
    let mut p = data_dir();
    for part in name.split('/') {
        if !part.is_empty() {
            p.push(part);
        }
    }
    p
}

/// 读状态文件：明文原样返回，DPAPI 密文自动解密（Go `ReadFile`）。
pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    dpapi::read_file(path)
}

/// 写状态文件：DPAPI 加密 + 临时文件改名 + ACL 收紧（Go `WriteFile`）。
/// 加密失败即返回错误且**不改动原文件**。
pub fn write_file(path: &Path, plain: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let enc = dpapi::protect_file_bytes(plain)?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &enc).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    // ACL 收紧放到**改名之后**的后台线程：`icacls` 是子进程，冷启动实测 ~550ms
    //（v4.0.4 全绑定扫描里 `ccDismiss`/`ccDismissCloseHint` 就是被它拖的）。
    // 安全性不变：文件内容已是 DPAPI 密文、改名也不依赖 ACL 完成——收紧是纵深
    // 防御，晚几毫秒落定没有语义损失。失败照旧记日志。
    {
        let final_path = path.to_path_buf();
        std::thread::spawn(move || {
            if let Err(e) = acl_user_only(&final_path) {
                logf(&format!(
                    "ACL 收紧失败（文件仍为密文）：{}: {}",
                    file_name(&final_path),
                    e
                ));
            }
        });
    }
    Ok(())
}

/// 删除状态文件（缺席不算错误，与 Go `Remove` 同语义）。
pub fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// 文件是否存在（Go `bridge.go` 的 `fileExists`：`os.Stat` 成功即算——**目录也算**）。
///
/// 注意这与 `migrate.rs` 的私有 `file_exists` **不是同一个语义**：后者对齐的是 Go
/// `migrate.go` 的版本（目录不算，`!fi.IsDir()`）。Go 同名函数在两个文件里语义不同，
/// 此处各对各的；`migrate.rs` 从不调用本函数（P7b 审查曾指出旧注释把这层关系写反，已更正）。
pub fn file_exists(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 日志轮转参数（Go `logfile.go`：1 MB × 3 份）。
pub const LOG_MAX_BYTES: u64 = 1 << 20;
pub const LOG_BACKUPS: usize = 3;

/// 往应用日志追加一行（Go `Logf`：落数据目录的 `appdir.log`，写失败一律吞掉）。
pub fn logf(msg: &str) {
    let _ = append_log(&data_dir().join("appdir.log"), msg);
}

/// 追加一行带时间戳的记录（先轮转后写，Go `AppendLog`）。
pub fn append_log(path: &Path, msg: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    rotate_log(path);
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    writeln!(f, "{} {}", crate::proc::now_local_str(), msg).map_err(|e| e.to_string())
}

/// 超过上限就把 `path` 滚成 `.1`，旧的依次后移，最老的丢弃（Go `rotateLog`）。
///
/// 返回**是否真的轮转过**：未到上限 / 读不到大小 / 改名失败一律 `false`。
/// 调用方（`StampedWriter`）据此决定要不要重开文件句柄——轮转后旧句柄仍指向已经改名的
/// `.1`，不重开就会把后续内容继续写进备份（日志看着正常、实际新 `path` 不再增长）。
/// 大小判断只在**本函数里做一处**（`rotate_if_big` 是它的别名，不重复判）。
pub fn rotate_log(path: &Path) -> bool {
    let size = match std::fs::metadata(path) {
        Ok(md) => md.len(),
        Err(_) => return false,
    };
    if size <= LOG_MAX_BYTES {
        return false;
    }
    let with_suffix = |n: usize| -> PathBuf {
        let mut s = path.as_os_str().to_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    };
    let _ = std::fs::remove_file(with_suffix(LOG_BACKUPS));
    for i in (1..LOG_BACKUPS).rev() {
        let _ = std::fs::rename(with_suffix(i), with_suffix(i + 1));
    }
    // 改名失败 = 没轮转（原文件还在原处，旧句柄依然有效）⇒ 调用方不该重开。
    std::fs::rename(path, with_suffix(1)).is_ok()
}

/// 写完后按大小轮转（长驻日志用；Go `rotateIfBig`）。返回是否真的轮转过。
pub fn rotate_if_big(path: &Path) -> bool {
    rotate_log(path)
}

/// 打开（不存在则创建）追加写日志文件，写失败不阻断主流程（Go `OpenLog`：
/// 建目录失败/打开失败一律返回 `None`，调用方按「无日志」继续）。
pub fn open_log(path: &Path) -> Option<std::fs::File> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() && std::fs::create_dir_all(dir).is_err() {
            return None;
        }
    }
    rotate_log(path);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .write(true)
        .open(path)
        .ok()
}

/// 在每行前加时间戳（子进程 stdout/stderr 落盘用）——Go `appdir.StampedWriter`。
///
/// 逐字对齐 Go 的分行口径：按 `\n` 切；**无换行的尾巴也立刻打戳写出**；
/// 空行跳过；`f` 为 `None` 时丢弃全部输出并返回 `len(p)`。
///
/// 轮转（Go `rotateIfBig`）在本实现里多做了两件 Go 没做的事，都是为了让「轮转后继续写」
/// 真的落到当前 `path` 上：
/// 1. `rotate_log` 改名成功后**重开** `f`——旧句柄指向已经改名成 `.1` 的文件，不重开
///    后续内容会继续写进备份，新建的 `path` 再也不增长（Go 有同样的隐患，见模块头差异）；
/// 2. 轮转检查按**累计写入字节数** `written` 触发，不再每行 `metadata`（Windows 上
///    rename 遇占用会重试，每行一次检查等于每行一次潜在的长持锁）。
pub struct StampedWriter {
    f: Option<std::fs::File>,
    path: PathBuf,
    buf: Vec<u8>,
    /// 自上次轮转检查以来写出的字节数（只用于决定「要不要去查一次大小」）。
    written: u64,
}

impl StampedWriter {
    /// 包装一个已打开的日志文件（Go `NewStampedWriter(f)`）。
    pub fn new(path: PathBuf, f: Option<std::fs::File>) -> Self {
        Self {
            f,
            path,
            buf: Vec::new(),
            written: 0,
        }
    }

    /// 直接按路径打开并包装（打不开就丢弃输出，等价 Go `OpenLog` 返回 nil 的分支）。
    pub fn open(path: PathBuf) -> Self {
        let f = open_log(&path);
        Self::new(path, f)
    }

    /// 是否真的挂着日志文件（`false` = 输出被丢弃）。
    pub fn is_open(&self) -> bool {
        self.f.is_some()
    }
}

impl std::io::Write for StampedWriter {
    fn write(&mut self, p: &[u8]) -> std::io::Result<usize> {
        if self.f.is_none() {
            return Ok(p.len());
        }
        self.buf.extend_from_slice(p);
        let mut out: Vec<u8> = Vec::with_capacity(p.len() + 24);
        while !self.buf.is_empty() {
            let line: Vec<u8> = match self.buf.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let line = self.buf[..i].to_vec();
                    self.buf.drain(..=i);
                    line
                }
                // 无换行的尾巴：本行已带时间戳，直接写出
                None => std::mem::take(&mut self.buf),
            };
            if line.is_empty() {
                continue;
            }
            out.extend_from_slice(crate::proc::now_local_str().as_bytes());
            out.push(b' ');
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
        if let Some(f) = self.f.as_mut() {
            f.write_all(&out)?;
        }
        self.written += out.len() as u64;
        // 累计写入没超上限就不去查大小（上限判定只在 `rotate_log` 里做一处）。
        // 超了就查**一次**：无论轮转是否成功都把计数清零，避免「rename 失败 → 每行重试」。
        if self.written > LOG_MAX_BYTES {
            self.written = 0;
            if rotate_if_big(&self.path) {
                // 轮转成功 ⇒ 旧句柄指向 `.1`，必须换到新 `path` 上继续写。
                // 重开失败（极端情况）保留旧句柄：宁可继续写备份，也不要静默丢日志。
                if let Some(nf) = open_log(&self.path) {
                    self.f = Some(nf);
                }
            }
        }
        Ok(p.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.f.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// 给文件补一条「仅当前用户」的显式授权（Go `aclUserOnly`）：
/// `/grant:r` 只替换该用户自己的条目，不动其它条目，所以**绝不会把访问权收没了**。
/// 拿不到用户名时返回错误，调用方只记日志、不阻断读写。
pub fn acl_user_only(path: &Path) -> Result<(), String> {
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return Err("USERNAME 环境变量为空，无法收紧 ACL".into());
    }
    let mut user = user;
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    if !domain.is_empty() && domain != std::env::var("COMPUTERNAME").unwrap_or_default() {
        user = format!("{domain}\\{user}");
    }
    // 隐藏窗口：Go 侧 `acl_windows.go` 用的是裸 exec.Command（GUI 下会闪一下黑窗），
    // 这里统一走 hidden_cmd（README 的「有意差异」登记项：无黑窗闪烁）。
    let out = crate::proc::hidden_cmd(
        "icacls",
        &[&path.to_string_lossy(), "/grant:r", &format!("{user}:(F)")],
    )
    .output()
    .map_err(|e| format!("icacls 失败: {e} ()"))?;
    if !out.status.success() {
        let mut text = String::from_utf8_lossy(&out.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if text.len() > 300 {
            text.truncate(text.floor_char_boundary(300));
            text.push('…');
        }
        return Err(format!("icacls 失败: {} ({})", out.status, text.trim_end()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_env_wins_and_path_joins_like_go() {
        // 同抢进程级 DATA_DIR_ENV 的测试必须串行（见 bridge.rs/webview.rs 的同款锁）。
        let _g = crate::testutil::env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-appdir-join");
        std::env::set_var(DATA_DIR_ENV, &dir);
        let p = path("plugins/tuanjie.log");
        assert_eq!(p, dir.join("plugins").join("tuanjie.log"));
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            "tuanjie.log",
            "filepath.Base 语义"
        );
        std::env::remove_var(DATA_DIR_ENV);
    }

    #[test]
    fn portable_marker_must_be_a_file() {
        let base = std::env::temp_dir().join("rs-gui-appdir-marker");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert!(!marker_exists(&base), "没有标记 → 非便携");
        std::fs::write(base.join(PORTABLE_MARKER), b"1").unwrap();
        assert!(marker_exists(&base), "有标记文件 → 便携");
        let _ = std::fs::remove_file(base.join(PORTABLE_MARKER));
        std::fs::create_dir_all(base.join(PORTABLE_MARKER)).unwrap();
        assert!(!marker_exists(&base), "标记是目录 → 不算，同 Go markerExists");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_file_round_trips_through_dpapi() {
        let dir = std::env::temp_dir().join("rs-gui-appdir-write");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("api-key.txt");
        write_file(&p, b"TESTKEY-ALIGN").unwrap();
        let raw = std::fs::read(&p).unwrap();
        assert!(dpapi::is_protected(&raw), "落盘必须是 PDAPI1 密文");
        assert_eq!(read_file(&p).unwrap(), b"TESTKEY-ALIGN".to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = std::env::temp_dir().join("rs-gui-appdir-remove");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nope.flag");
        assert!(remove(&p).is_ok(), "缺席不算错误");
        std::fs::write(&p, b"1").unwrap();
        assert!(remove(&p).is_ok());
        assert!(!file_exists(&p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮转竞态：`rotate_log` 把 `path` 改名成 `.1` 之后，`StampedWriter` 必须**重开**句柄，
    /// 否则后续内容继续写进 `.1`、新建的 `path` 不再增长（日志看着正常、实际丢失）。
    ///
    /// 反向验证：把 `write()` 里轮转后的 `open_log` 重开去掉 ⇒ 本用例判红
    ///（`TAIL-MARKER` 会落在 `.1` 里，`path` 本身只有空文件）。
    #[test]
    fn rotate_keeps_writing_to_current_log() {
        use std::io::Write;

        // 与同模块其它用例共用一把锁：libtest 默认多线程并行，`StampedWriter::open`
        // 内部会读数据目录/环境变量，不加锁会与本模块其它用例互相踩。
        let _g = crate::testutil::env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-appdir-rotate-reopen");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plugin.log");
        let mut backup = path.as_os_str().to_os_string();
        backup.push(".1");
        let backup = PathBuf::from(backup);

        let mut w = StampedWriter::open(path.clone());
        assert!(w.is_open(), "日志文件应能打开");

        // 按 ~64KB 的块写，累计超过上限（1MB）——轮转必然发生在中途。
        let chunk = format!("{}\n", "A".repeat(1023)).repeat(64);
        let mut sent = 0usize;
        while sent <= LOG_MAX_BYTES as usize {
            w.write_all(chunk.as_bytes()).unwrap();
            sent += chunk.len();
        }
        assert!(backup.exists(), "写超上限后应产生 .1 备份（轮转没触发）");

        // 轮转之后**用同一个 writer** 继续写：这段必须落在 `path` 本身。
        w.write_all(b"TAIL-MARKER\n").unwrap();
        w.flush().unwrap();

        let cur = std::fs::read_to_string(&path).unwrap();
        let old = std::fs::read_to_string(&backup).unwrap();
        assert!(
            cur.contains("TAIL-MARKER"),
            "轮转后的新内容应写进当前日志（实得 {} 字节，备份 {} 字节）",
            cur.len(),
            old.len()
        );
        assert!(
            !old.contains("TAIL-MARKER"),
            "轮转后的新内容不该写进 .1（句柄没重开就是本用例要抓的缺陷）"
        );
        assert!(
            old.len() as u64 > LOG_MAX_BYTES,
            "备份应是轮转走的那份超限内容（实得 {} 字节）",
            old.len()
        );
        assert!(
            (cur.len() as u64) < LOG_MAX_BYTES,
            "当前日志应是轮转后新建的那份（实得 {} 字节）",
            cur.len()
        );

        // 同一个句柄再写超一轮：确认重开不是「只对第一次有效」。
        let mut sent = 0usize;
        while sent <= LOG_MAX_BYTES as usize {
            w.write_all(chunk.as_bytes()).unwrap();
            sent += chunk.len();
        }
        w.write_all(b"SECOND-MARKER\n").unwrap();
        w.flush().unwrap();
        drop(w);

        let cur = std::fs::read_to_string(&path).unwrap();
        assert!(
            cur.contains("SECOND-MARKER"),
            "第二轮之后的写入仍应落在当前日志（实得 {} 字节）",
            cur.len()
        );
        // 备份链里不该出现新内容——无论是 `.1` 还是更老的备份。
        let mut backups: Vec<(PathBuf, String)> = Vec::new();
        for n in 1..=LOG_BACKUPS {
            let mut p = path.as_os_str().to_os_string();
            p.push(format!(".{n}"));
            let p = PathBuf::from(p);
            if let Ok(s) = std::fs::read_to_string(&p) {
                backups.push((p, s));
            }
        }
        assert!(
            !backups.is_empty(),
            "两轮写入后应留下至少一个备份（轮转没发生）"
        );
        for (p, s) in &backups {
            assert!(
                !s.contains("SECOND-MARKER"),
                "轮转后的新内容不该出现在备份 {} 里（句柄没重开就是本用例要抓的缺陷）",
                p.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
