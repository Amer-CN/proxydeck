//! 数据目录解析：与 rs-bai / rs-opencode / rs-gui 同一套判定链。
//!
//! * `PROXYDECK_DATA_DIR` 非空 → 用它（单测/临时实例）
//! * 便携模式（exe 同目录有 `portable` 标记）→ exe 同目录
//! * 否则 `%APPDATA%\ProxyDeck\`；APPDATA 缺失再退 `%USERPROFILE%\AppData\Roaming`
//! * 全都不成立 → exe 同目录
//!
//! 只解析路径，不建目录（建目录留给真正写盘的那一处）。

use std::path::{Path, PathBuf};

/// 数据目录覆盖变量（与其它 crate 同名同义）。
pub const DATA_DIR_ENV: &str = "PROXYDECK_DATA_DIR";
/// `%APPDATA%` 下的数据目录名。
pub const APP_NAME: &str = "ProxyDeck";
/// 便携标记文件名。
pub const PORTABLE_MARKER: &str = "portable";

/// 账号池文件名（落在数据目录，已 gitignore）。
pub const ACCOUNTS_FILE: &str = "codearts-accounts.json";

fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// exe 所在目录；拿不到就退回当前工作目录。
pub fn exe_dir() -> PathBuf {
    if let Ok(p) = std::env::current_exe() {
        if let Ok(rp) = p.canonicalize() {
            if let Some(d) = rp.parent() {
                return d.to_path_buf();
            }
        }
        if let Some(d) = p.parent() {
            return d.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 是否便携模式。
pub fn is_portable() -> bool {
    std::fs::metadata(exe_dir().join(PORTABLE_MARKER))
        .map(|md| !md.is_dir())
        .unwrap_or(false)
}

/// 解析数据目录（不创建）。
pub fn data_dir() -> PathBuf {
    if let Some(d) = env_nonempty(DATA_DIR_ENV) {
        return PathBuf::from(d);
    }
    if is_portable() {
        return exe_dir();
    }
    if let Some(appdata) = env_nonempty("APPDATA") {
        return Path::new(&appdata).join(APP_NAME);
    }
    if let Some(home) = env_nonempty("USERPROFILE") {
        return Path::new(&home)
            .join("AppData")
            .join("Roaming")
            .join(APP_NAME);
    }
    exe_dir()
}

/// 数据目录下的文件路径。
pub fn path(name: &str) -> PathBuf {
    data_dir().join(name)
}
