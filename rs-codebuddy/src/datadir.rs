// datadir.rs —— 数据目录解析（对应 Go 侧 `internal/appdir/appdir.go` 的判定链）。
//
// 为什么要它：`diag-11133-*.json` 现场取证必须落在**与 Go 版同一个目录**，否则排查时
// 两边各写一份。规则（逐条对 Go）：
//   * `PROXYDECK_DATA_DIR` 非空 → 用它（单测/临时实例用）
//   * 便携模式（exe 同目录存在 `portable` 标记）→ exe 同目录
//   * 否则 `%APPDATA%\ProxyDeck\`；APPDATA 缺失再退 `%USERPROFILE%\AppData\Roaming\ProxyDeck`
//   * 全都不成立 → exe 同目录
//
// 与 Go 的已知差异（不影响本阶段对齐，登记在 crate README）：Go 的 `DataDir()` 会顺带
// `MkdirAll(0o700)`，本模块只解析路径不建目录——建目录动作留给真正写盘的那一处
// （`diag::dump_request_body` 落盘前自建），只读路径不会因为解析路径而产生副作用。
use std::path::{Path, PathBuf};

/// 数据目录覆盖变量（与 Go `appdir.DataDirEnv` 同名同义）。
pub const DATA_DIR_ENV: &str = "PROXYDECK_DATA_DIR";
/// `%APPDATA%` 下的数据目录名（与 Go `appdir.AppName` 同值）。
pub const APP_NAME: &str = "ProxyDeck";
/// 便携标记文件名（与 Go `appdir.PortableMarker` 同值）。
pub const PORTABLE_MARKER: &str = "portable";

fn env_nonempty(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// exe 所在目录；拿不到就退回当前工作目录（Go `appdir.ExeDir` 同口径）。
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
    std::fs::metadata(&m).map(|md| !md.is_dir()).unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        let tmp = std::env::temp_dir().join("rs-codebuddy-datadir-probe");
        std::env::set_var(DATA_DIR_ENV, &tmp);
        assert_eq!(data_dir(), tmp);
        std::env::set_var(DATA_DIR_ENV, "   ");
        assert_ne!(data_dir().to_string_lossy().trim(), "");
        std::env::remove_var(DATA_DIR_ENV);
    }
}
