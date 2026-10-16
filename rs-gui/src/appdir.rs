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
//! 文件搬进数据目录）与 `UpgradePlaintext`（存量明文凭据升级为密文），以及插件子进程日志用的
//! `OpenLog` / `StampedWriter`（P7b）。

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
    if let Err(e) = acl_user_only(&tmp) {
        logf(&format!(
            "ACL 收紧失败（文件仍为密文）：{}: {}",
            file_name(path),
            e
        ));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
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

/// 文件是否存在（Go `fileExists`：`os.Stat` 成功即算，目录也算）。
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
pub fn rotate_log(path: &Path) {
    let size = match std::fs::metadata(path) {
        Ok(md) => md.len(),
        Err(_) => return,
    };
    if size <= LOG_MAX_BYTES {
        return;
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
    let _ = std::fs::rename(path, with_suffix(1));
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
}
