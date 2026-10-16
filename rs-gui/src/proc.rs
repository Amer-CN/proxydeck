//! proc.rs —— 进程/端口/外壳的小工具（对应 Go `app/bridge.go` 的 hiddenCmd /
//! portBusy / killByPort / openURL 与 `internal/appdir/acl_windows.go`）。
//!
//! 一律走 `CREATE_NO_WINDOW`：无控制台的 GUI 进程 spawn `netstat`/`taskkill`/`rundll32`
//! 会弹黑窗，必须隐藏（Go `hiddenCmd` 同款）。

use std::process::{Command, Stdio};

/// `CREATE_NO_WINDOW`（Go `hiddenCmd` 的 CreationFlags 同值）。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 建一个隐藏窗口的命令（对应 Go `hiddenCmd`）。
pub fn hidden_cmd(name: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(name);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// 跑一个隐藏命令并取 stdout（失败返回 None）。
pub fn hidden_output(name: &str, args: &[&str]) -> Option<String> {
    let out = hidden_cmd(name, args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 解析 `netstat -ano -p tcp` 的 LISTENING 行，取监听指定端口的 pid（排除自己）。
/// 与 Go `killByPort` 同一套字段语义：`f[1]` 地址后缀、`f[3]` 状态、`f[4]` pid。
pub fn listeners_on_port(netstat_out: &str, port: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in netstat_out.split('\n') {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 || !f[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        if !f[1].ends_with(&format!(":{port}")) {
            continue;
        }
        if let Ok(pid) = f[4].parse::<u32>() {
            if pid > 0 && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// 端口是否有进程监听（不论是否健康）——对应 Go `portBusy`。
pub fn port_busy(port: &str) -> bool {
    match hidden_output("netstat", &["-ano", "-p", "tcp"]) {
        Some(out) => !listeners_on_port(&out, port).is_empty(),
        None => false,
    }
}

/// 结束监听指定端口的进程（排除自己）——对应 Go `killByPort`。
pub fn kill_by_port(port: &str) -> Result<(), String> {
    let out = hidden_output("netstat", &["-ano", "-p", "tcp"])
        .ok_or_else(|| "netstat 执行失败: 无法运行 netstat".to_string())?;
    let self_pid = std::process::id();
    let mut killed = false;
    for pid in listeners_on_port(&out, port) {
        if pid != self_pid {
            let _ = hidden_cmd("taskkill", &["/F", "/PID", &pid.to_string()]).status();
            killed = true;
        }
    }
    if !killed {
        return Err("未找到占用该端口的进程（可能已自行退出）".into());
    }
    Ok(())
}

/// 等端口空出来（只看 LISTENING）——对应 Go `waitPortFree`。
pub fn wait_port_free(port: &str, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !port_busy(port) {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// 用系统默认程序打开 http(s) 链接——对应 Go `openURL`。
pub fn open_url(u: &str) -> Result<(), String> {
    if !u.starts_with("http://") && !u.starts_with("https://") {
        return Err("仅允许打开 http(s) 链接".into());
    }
    hidden_cmd("rundll32", &["url.dll,FileProtocolHandler", u])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 本地时间字符串 `2006-01-02 15:04:05`（Go `time.Now().Format(...)` 同格式，
/// 日志行头用）。不引 chrono：Win32 `GetLocalTime` 一次调用就够。
pub fn now_local_str() -> String {
    let (y, mo, d, h, mi, s) = now_local_parts();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut SystemTime);
}

#[cfg(windows)]
fn now_local_parts() -> (u16, u16, u16, u16, u16, u16) {
    let mut st = SystemTime::default();
    unsafe { GetLocalTime(&mut st) };
    (st.year, st.month, st.day, st.hour, st.minute, st.second)
}

#[cfg(not(windows))]
fn now_local_parts() -> (u16, u16, u16, u16, u16, u16) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = now / 86400;
    let secs = now % 86400;
    // 仅作日志行头，非 Windows 上给个稳定的近似值即可（本项目只在 Windows 跑）
    (1970, 1, 1 + (days % 28) as u16, (secs / 3600) as u16, ((secs % 3600) / 60) as u16, (secs % 60) as u16)
}

/// 写系统剪贴板（CF_UNICODETEXT）——对应 Go 侧 `atotto/clipboard` 的 WriteAll。
#[cfg(windows)]
pub fn clipboard_write_all(text: &str) -> Result<(), String> {
    use std::ffi::c_void;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalLock(handle: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(handle: *mut c_void) -> i32;
        fn GlobalFree(handle: *mut c_void) -> *mut c_void;
    }
    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: *mut c_void) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn CloseClipboard() -> i32;
    }

    // atotto/clipboard 在剪贴板被别的进程占用时会重试；这里同样给几次机会。
    let mut opened = false;
    for _ in 0..10 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !opened {
        return Err("打开剪贴板失败".into());
    }
    let result = (|| -> Result<(), String> {
        unsafe { EmptyClipboard() };
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = wide.len() * 2;
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if mem.is_null() {
            return Err("剪贴板内存分配失败".into());
        }
        let dst = unsafe { GlobalLock(mem) } as *mut u16;
        if dst.is_null() {
            unsafe { GlobalFree(mem) };
            return Err("剪贴板内存锁定失败".into());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            GlobalUnlock(mem);
        }
        if unsafe { SetClipboardData(CF_UNICODETEXT, mem) }.is_null() {
            unsafe { GlobalFree(mem) };
            return Err("写入剪贴板失败".into());
        }
        // 交给系统后不再 GlobalFree（所有权已转移）
        Ok(())
    })();
    unsafe { CloseClipboard() };
    result
}

#[cfg(not(windows))]
pub fn clipboard_write_all(_text: &str) -> Result<(), String> {
    Err("本平台未实现剪贴板写入".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netstat_parsing_matches_go_field_semantics() {
        let out = "  TCP    127.0.0.1:8788         0.0.0.0:0              LISTENING       4321\n\
                   TCP    127.0.0.1:8788         0.0.0.0:0              LISTENING       4321\n\
                   TCP    [::]:18788             [::]:0                 LISTENING       9999\n\
                   TCP    127.0.0.1:8788         127.0.0.1:55555        ESTABLISHED     4321\n";
        assert_eq!(listeners_on_port(out, "8788"), vec![4321], "去重且只认 LISTENING");
        assert_eq!(listeners_on_port(out, "18788"), vec![9999], "IPv6 形态也要认");
        assert!(listeners_on_port(out, "55990").is_empty());
    }

    #[test]
    fn open_url_rejects_non_http() {
        assert_eq!(
            open_url("file:///x").unwrap_err(),
            "仅允许打开 http(s) 链接"
        );
    }

    #[test]
    fn local_time_has_go_layout() {
        let s = now_local_str();
        assert_eq!(s.len(), 19, "格式必须是 2006-01-02 15:04:05：{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[13..14], ":");
    }
}
