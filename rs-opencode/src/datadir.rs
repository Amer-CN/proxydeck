//! datadir.rs —— 数据目录解析（沿用 `rs-bai/src/datadir.rs` 的判定链）。
//!
//! 本 crate 只需要一个文件名常量：`opencode.json`。规则：
//!   * `PROXYDECK_DATA_DIR` 非空 → 用它（单测/临时实例用）
//!   * 便携模式（exe 同目录存在 `portable` 标记）→ exe 同目录
//!   * 否则 `%APPDATA%\ProxyDeck\`；APPDATA 缺失再退 `%USERPROFILE%\AppData\Roaming\ProxyDeck`
//!   * 全都不成立 → exe 同目录
//!
//! 与 `rs-bai` 一样只解析路径、不建目录：建目录/写盘留给真正需要写的那一处。
use std::path::{Path, PathBuf};

/// 数据目录覆盖变量（与其它 `rs-*` 同名同义）。
pub const DATA_DIR_ENV: &str = "PROXYDECK_DATA_DIR";
/// `%APPDATA%` 下的数据目录名。
pub const APP_NAME: &str = "ProxyDeck";
/// 便携标记文件名。
pub const PORTABLE_MARKER: &str = "portable";

/// 插件配置文件名。
pub const CONFIG_FILE: &str = "opencode.json";

/// 环境变量取值：空串按未设置处理（Go 用 `strings.TrimSpace`）。
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

/// 是否便携模式（exe 同目录存在 `portable` 标记文件）。
pub fn is_portable() -> bool {
    let m = exe_dir().join(PORTABLE_MARKER);
    std::fs::metadata(&m)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `PROXYDECK_DATA_DIR` 是进程级变量，多用例并行会互相踩，所以只留一条。
    #[test]
    fn env_override_and_join() {
        let tmp = std::env::temp_dir().join("rs-opencode-datadir-probe");
        std::env::set_var(DATA_DIR_ENV, &tmp);
        assert_eq!(data_dir(), tmp);
        assert_eq!(path(CONFIG_FILE), tmp.join(CONFIG_FILE));
        std::env::remove_var(DATA_DIR_ENV);
    }
}
