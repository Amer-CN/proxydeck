//! KIMI-K3 429 兜底开关 —— `internal/tuanjie/pacing.go` 的逐条对应物（P8b 切片）。
//!
//! Go 侧事实（只读核对，行号取自 `pacing.go`）：
//! - `pacingRetryDefault = 60s`（`:21`）、`pacingMaxWait = 30min`（`:22`）——兜底等待与总预算；
//! - `IsPacingModel`（`:55-57`）= `strings.Contains(strings.ToUpper(model), "KIMI")`；
//! - `RetryWait`（`:61-67`）= `Atoi(TrimSpace(h))`，**>0 的整数秒**才采信，其余（空/非数字/0/负数）回 60s；
//! - `Pacer.load/save`（`:70-99`）= 数据目录 `tuanjie-pacing.json`，形态 `{"enabled":<bool>}`
//!   （Go 的 `json.Marshal(map[string]bool)` → 单键、无缩进、无换行），落盘走 `tmp + rename`。
//!
//! 数据目录判定链（`internal/appdir/appdir.go:84-98`）在 **P8c 已整块迁进
//! [`crate::appdir`]**（该模块现在是 appdir 全量的落点：判定链 / 迁移 / 明文升级 /
//! 日志轮转 / 启动收口）。本模块只**转出**这几个名字，保持既有调用点与测试缝不变。

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// `pacing.go:21` —— Retry-After 缺省（一个窗口）。
pub const PACING_RETRY_DEFAULT_SECS: u64 = 60;

/// `pacing.go:22` —— 单请求总兜底预算。
pub const PACING_MAX_WAIT_SECS: u64 = 30 * 60;

/// 数据目录覆盖环境变量名 —— `appdir.go:26`（转发自 [`crate::appdir`]）。
pub const DATA_DIR_ENV: &str = crate::appdir::DATA_DIR_ENV;

/// 便携标记文件名 —— `appdir.go:24`（转发自 [`crate::appdir`]）。
pub const PORTABLE_MARKER: &str = crate::appdir::PORTABLE_MARKER;

/// `%APPDATA%` 下的数据目录名 —— `appdir.go:22`（转发自 [`crate::appdir`]）。
pub const APP_NAME: &str = crate::appdir::APP_NAME;

/// 数据目录（对齐 `appdir.dataDir()`）—— `appdir.go:84-98`。
pub fn data_dir() -> PathBuf {
    crate::appdir::data_dir()
}

/// 便携标记是否在 exe 同目录（**是文件、不是目录**）—— `appdir.go:33-36`。
pub fn portable_marker_exists() -> bool {
    crate::appdir::marker_exists()
}

/// 当前是否便携模式 —— `appdir.go:39`（由 [`crate::appdir::init`] 按 flag/标记解析）。
pub fn portable() -> bool {
    crate::appdir::portable()
}

/// 数据目录（不存在则建；失败退回 exe 同目录）—— `appdir.go:72-78`。
pub fn data_dir_ensure() -> PathBuf {
    crate::appdir::data_dir_ensure()
}

/// 数据目录下的某个文件路径 —— `appdir.go:82`。
pub fn data_path(name: &str) -> PathBuf {
    crate::appdir::data_path(name)
}

/// 429 兜底是否适用于该模型：`pacing.go:55-57`（**不分大小写**地找 "KIMI"）。
pub fn is_pacing_model(model: &str) -> bool {
    model.to_uppercase().contains("KIMI")
}

/// `Retry-After` 头 → 等待秒数：`pacing.go:61-67`。
///
/// 与 Go 的差异（**已登记的边界，非静默**）：Go 是 `time.Duration(secs) * time.Second`，
/// `secs` 极大时整数溢出、结果可能为负；Rust 侧饱和到 `u64` 秒。真实 `Retry-After`
/// 不会有这个量级（HTTP 头是秒数），故不作逐位复刻。
pub fn retry_wait_secs(retry_after: &str) -> u64 {
    match retry_after.trim().parse::<i64>() {
        Ok(secs) if secs > 0 => secs as u64,
        _ => PACING_RETRY_DEFAULT_SECS,
    }
}

/// KIMI-K3 429 兜底开关（线程安全）——`pacing.go:26-31`。
pub struct Pacer {
    enabled: AtomicBool,
    pending: AtomicI64,
    path: Option<PathBuf>,
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new()
    }
}

impl Pacer {
    /// 创建开关并从数据目录读回状态 —— `Pacer{}` + `load()`（`pacing.go:34-39`）。
    pub fn new() -> Self {
        let mut p = Pacer {
            enabled: AtomicBool::new(false),
            pending: AtomicI64::new(0),
            path: Some(data_path("tuanjie-pacing.json")),
        };
        p.load();
        p
    }

    /// 不落盘的实例（单测/测试缝用；Go 侧 `path == ""` 时同样不读不写）。
    pub fn detached() -> Self {
        Pacer {
            enabled: AtomicBool::new(false),
            pending: AtomicI64::new(0),
            path: None,
        }
    }

    /// 开关状态 —— `Enabled()`（`pacing.go:42`）。
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// 切换开关并持久化 —— `SetEnabled`（`pacing.go:45-48`）。
    pub fn set_enabled(&self, v: bool) {
        self.enabled.store(v, Ordering::SeqCst);
        self.save();
    }

    /// 当前被 429 兜底接管的进行中请求数 —— `Pending()`（`pacing.go:51`）。
    pub fn pending(&self) -> i64 {
        self.pending.load(Ordering::SeqCst)
    }

    /// 兜底接管计数 +1 / −1（Go 侧由 `handleChat` 的 `pending.Add(1)/Add(-1)` 驱动）。
    pub fn pending_inc(&self) -> i64 {
        self.pending.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 见 [`Pacer::pending_inc`]。
    pub fn pending_dec(&self) -> i64 {
        self.pending.fetch_sub(1, Ordering::SeqCst) - 1
    }

    /// 设置持久化路径（测试缝；Go 侧 path 由 `appdir.Path` 在构造时定死）。
    pub fn with_path(mut self, path: Option<PathBuf>) -> Self {
        self.path = path;
        self
    }

    /// `pacing.go:70-84`：读回 `{"enabled":bool}`；文件缺失/解析失败一律保持原值。
    fn load(&mut self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Ok(b) = std::fs::read(&path) else {
            return;
        };
        let Ok(v) = serde_json::from_slice::<PacingFile>(&b) else {
            return;
        };
        self.enabled.store(v.enabled, Ordering::SeqCst);
    }

    /// `pacing.go:87-99`：`json.Marshal(map[string]bool{"enabled":…})` 的字节形态
    /// （单键、无缩进、无换行），`tmp + rename` 原子落盘。
    fn save(&self) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let bytes = pacing_file_bytes(self.enabled.load(Ordering::SeqCst));
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        if let Ok(mut f) = std::fs::File::create(&tmp) {
            if f.write_all(&bytes).is_ok() {
                drop(f);
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct PacingFile {
    enabled: bool,
}

/// `json.Marshal(map[string]bool{"enabled":v})` 的逐字节形态。
pub fn pacing_file_bytes(enabled: bool) -> Vec<u8> {
    format!("{{\"enabled\":{}}}", enabled).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_wait_matches_go_table() {
        // 与黄金基准同表（tests/golden/golden-p8b-go.json → pacing.retry_wait）
        for (input, want) in [
            ("", 60u64),
            (" ", 60),
            ("0", 60),
            ("-1", 60),
            ("abc", 60),
            ("60", 60),
            (" 120 ", 120),
            ("1.5", 60),
            ("+30", 30),
            ("99999999999999999999", 60),
            ("3600", 3600),
            ("\t90\n", 90),
        ] {
            assert_eq!(retry_wait_secs(input), want, "RetryWait({input:?})");
        }
    }

    #[test]
    fn pacing_file_bytes_is_go_shape() {
        assert_eq!(pacing_file_bytes(true), b"{\"enabled\":true}");
        assert_eq!(pacing_file_bytes(false), b"{\"enabled\":false}");
    }
}
