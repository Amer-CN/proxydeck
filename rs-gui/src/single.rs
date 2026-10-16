//! single.rs —— 单实例守卫（对应 Go `app/platform_windows.go` 的 `faxPopupMutexGuard`）。
//!
//! 用**命名互斥体**做进程级单实例（句柄在进程存活期间一直有效；`ERROR_ALREADY_EXISTS`
//! 表示「已经有别的实例拿着这个名字」）：
//!   * 互斥体是内核对象，不需要清理逻辑——进程退出/句柄关闭即释放
//!   * 名字前缀 `Local\` = 当前登录会话内可见（Go 侧同款），跨用户的另一个会话互不干扰
//!
//! Go 里只有**注水专线浮窗**用了它（`Local\ProxyDeck-FaxWindow`）；主 GUI 在 Go 侧
//! **没有**单实例守卫（多开主窗是现行行为）。这里照抄同一范围，不擅自给主 GUI 加锁
//! （那是新行为，登记在 README 的「与 Go 的差异」里）。

use std::ffi::c_void;

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
    fn GetLastError() -> u32;
}

/// `ERROR_ALREADY_EXISTS`（Win32）。
const ERROR_ALREADY_EXISTS: u32 = 183;

/// 注水专线浮窗的单实例名（与 Go `faxPopupMutexGuard` 逐字同源）。
pub const FAX_WINDOW_MUTEX: &str = "Local\\ProxyDeck-FaxWindow";

/// 一个命名互斥体的持有者。
///
/// **故意不实现 `Drop`**：句柄一旦关闭，互斥体在系统里就没了——而 Go 侧的
/// `CreateMutexW` 返回值也是**丢掉不管**（句柄随进程退出关闭），单实例语义正是靠
/// 「进程活着 = 句柄活着」。照抄这个语义，加 `Drop` 反而会提前放掉。
#[derive(Debug)]
pub struct NamedMutex {
    handle: usize,
}

/// 拿一个命名互斥体：`Ok(Some(_))` = 本次是唯一持有者；`Ok(None)` = 已有实例持有。
/// 句柄为 0（创建失败，理论上不会发生）按「本次唯一」处理，与 Go 的
/// `err != ERROR_ALREADY_EXISTS` 判据等价。
pub fn acquire(name: &str) -> Option<NamedMutex> {
    let wide = crate::win32::wide(name);
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if exists {
        return None;
    }
    Some(NamedMutex {
        handle: handle as usize,
    })
}

impl NamedMutex {
    /// 底层句柄（0 = 内核没给，测试与排障用；生产不读它）。
    pub fn raw_handle(&self) -> usize {
        self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名字必须与 Go 逐字一致（互斥体名是跨实现的单实例判据，差一个字节就等于没有单实例）。
    #[test]
    fn fax_mutex_name_matches_go() {
        assert_eq!(FAX_WINDOW_MUTEX, "Local\\ProxyDeck-FaxWindow");
    }

    /// 第一次拿到 = 唯一；同一个名字第二次 = 已有实例（这正是浮窗单实例的判据）。
    #[test]
    fn second_acquire_on_same_name_reports_existing() {
        let name = format!("Local\\rs-gui-single-test-{}", std::process::id());
        let first = acquire(&name).expect("第一次必须拿到");
        assert_ne!(first.raw_handle(), 0, "内核给了真句柄");
        assert!(acquire(&name).is_none(), "第二次必须报「已有实例」");
        // 换一个名字互不影响
        let other = format!("Local\\rs-gui-single-other-{}", std::process::id());
        assert!(acquire(&other).is_some(), "不同名字之间互不干扰");
    }

    /// 同一进程内重复调用也必须报「已有实例」（浮窗双击两个入口几乎同时点击的竞态窗口
    /// 就靠这条挡住）。第一次的守卫在这里刻意不 drop（drop 也只是关句柄、名字仍被本进程持有
    /// 直到内核对象引用清零——所以这里再拿一次仍应报已存在）。
    #[test]
    fn same_process_second_acquire_reports_existing() {
        let name = format!("Local\\rs-gui-single-self-{}", std::process::id());
        let _keep = acquire(&name).expect("第一次必须拿到");
        assert!(
            acquire(&name).is_none(),
            "同进程第二次也必须报「已有实例」"
        );
    }
}
