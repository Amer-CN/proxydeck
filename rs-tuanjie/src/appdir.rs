//! exe 目录——`metadata.cwd` 字段的真值来源。
//!
//! Go 侧是 `server.go:100` 的 `exeDirForProgram()` → `appdir.ExeDir()`
//! (`internal/appdir/appdir.go:57-68`)：先 `os.Executable()`，能 `EvalSymlinks` 就解析，
//! 返回其目录；失败退回工作目录；再失败退回 `"."`。
//!
//! 本函数只服务 `metadata.cwd` 一处（数据目录判定属 P8c/P6 的面），故不引入 appdir 全量。

use std::path::PathBuf;

/// exe 所在目录（对齐 `appdir.ExeDir()`）。
pub fn exe_dir() -> PathBuf {
    if let Ok(p) = std::env::current_exe() {
        let p = p.canonicalize().unwrap_or(p);
        if let Some(dir) = p.parent() {
            return dir.to_path_buf();
        }
    }
    if let Ok(wd) = std::env::current_dir() {
        return wd;
    }
    PathBuf::from(".")
}

/// `metadata.cwd` 的字符串形态（Go 侧直接塞 `string`）。
pub fn exe_dir_string() -> String {
    exe_dir().to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_dir_is_absolute_and_nonempty() {
        let d = exe_dir_string();
        assert!(!d.is_empty());
        assert!(PathBuf::from(&d).is_absolute(), "exe 目录应是绝对路径: {d}");
    }
}
