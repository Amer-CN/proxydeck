//! 码道账号文件互斥：Windows 用独占文件句柄，进程退出即由内核释放。
//!
//! 锁文件允许残留；残留文件不代表锁仍被占用。取得锁后调用方必须重新读盘，
//! 不能信任取得锁前的内存快照。

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

#[derive(Debug)]
pub enum LockError {
    Io(String),
    Timeout(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(s) | Self::Timeout(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for LockError {}

pub struct AccountFileGuard {
    _file: File,
    path: PathBuf,
}

impl Drop for AccountFileGuard {
    fn drop(&mut self) {
        // 不删除锁文件：Windows 独占句柄释放即为解锁，删除会让等待者与持有者路径分叉。
        let _ = &self.path;
    }
}

fn lock_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(".codearts-locks")
}

fn hashed_lock_path(data_dir: &Path, namespace: &str, id: &str) -> PathBuf {
    let digest = Sha256::digest(format!("{namespace}\0{id}").as_bytes());
    lock_dir(data_dir).join(format!(
        "{namespace}-{}.lock",
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

fn open_exclusive(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options.open(path)
}

fn acquire(path: PathBuf, timeout: Duration, label: &str) -> Result<AccountFileGuard, LockError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LockError::Io(format!("创建锁目录 {} 失败: {e}", parent.display())))?;
    }
    let deadline = Instant::now() + timeout;
    loop {
        match open_exclusive(&path) {
            Ok(file) => {
                return Ok(AccountFileGuard {
                    _file: file,
                    path,
                })
            }
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(LockError::Timeout(format!(
                        "等待 {label} 锁超时（{}）：{e}",
                        path.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// 单账号续期锁；网络 await 期间只持有这个 OS 句柄，不持有普通同步 Mutex。
pub fn acquire_account_lock(data_dir: &Path, id: &str) -> Result<AccountFileGuard, LockError> {
    acquire(
        hashed_lock_path(data_dir, "account", id),
        Duration::from_secs(30),
        &format!("账号 {id}"),
    )
}

/// 短期配置事务锁；稳定 ID 分配与文件读—改—写必须持有。
pub fn acquire_config_lock(data_dir: &Path) -> Result<AccountFileGuard, LockError> {
    acquire(
        hashed_lock_path(data_dir, "config", "accounts"),
        Duration::from_secs(30),
        "配置事务",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_by_drop() {
        let dir = std::env::temp_dir().join(format!("rs-codearts-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = acquire_account_lock(&dir, "a").unwrap();
        let second = acquire(
            hashed_lock_path(&dir, "account", "a"),
            Duration::from_millis(30),
            "test",
        );
        assert!(matches!(second, Err(LockError::Timeout(_))));
        drop(first);
        assert!(acquire_account_lock(&dir, "a").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
