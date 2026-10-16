//! faxtool.rs —— 注水专线浮窗（对应 Go `app/main.go` 的 `bindFaxLauncher` + `runFaxWindow`）。
//!
//! 三个绑定（ui.html 调用面里最后 3 个未提供项）：
//!   * `ccOpenFax`（`main.go:64-82`）——已有浮窗 → 置前并回「注水专线浮窗已在前台」；
//!     否则 spawn **本 exe** 的 `--fax`（带上便携参数，保证父子读同一个数据目录），
//!     成功回「注水专线浮窗已打开」，失败 `jsonErr`
//!   * `ccFaxWin`（`main.go:131-135`）——浮窗三灯直绑：`windowCmd(hwnd, cmd)`；拿不到句柄时空操作
//!   * `ccFaxClose`（`main.go:143-147`）——`windowCmd(hwnd, "close")` → `PostMessage WM_CLOSE`
//!
//! `windows.rs` 侧无窗口宿主（`NoWindowHost`）的 `open_fax` 走**同一段代码**——
//! 「找浮窗」在无窗口时同样返回 0，「spawn 本 exe」在测试里会打到真 exe，
//! 故门禁里用 [`SpawnTarget`] 的测试缝把目标换成假后端（见 `tests/shell_contract.rs`）。

use std::sync::Mutex;

use crate::appdir;
use crate::jsons;
use crate::single;
use crate::win32;

/// 浮窗尺寸（Go `runFaxWindow`：`w.SetSize(580, 784)` + `setClientSize` 二次钉回）。
pub const FAX_WIDTH: i32 = 580;
/// 浮窗高度。
pub const FAX_HEIGHT: i32 = 784;

/// `--fax` 子进程的启动目标（生产 = 本 exe；门禁用假后端，见 README 的测试缝登记）。
#[derive(Clone, Debug, Default)]
pub struct SpawnTarget {
    /// 显式可执行文件（`None` = 本 exe，即 Go 的 `os.Executable()`）
    pub exe: Option<std::path::PathBuf>,
    /// 前置参数（测试缝用：把假后端脚本作为 argv[1..] 交给解释器）
    pub prefix: Vec<String>,
}

impl SpawnTarget {
    /// 解析出「程序 + 前置参数」；定位失败时给与 Go 同源的文案（`jsonErr(err)` 的正文）。
    pub fn resolve(&self) -> Result<(std::path::PathBuf, Vec<String>), String> {
        match &self.exe {
            Some(p) => Ok((p.clone(), self.prefix.clone())),
            None => std::env::current_exe()
                .map(|p| (p, Vec::new()))
                .map_err(|e| e.to_string()),
        }
    }
}

/// 浮窗启动器（生产用默认目标；门禁注入假后端）。
#[derive(Default)]
pub struct FaxLauncher {
    spawn: SpawnTarget,
}

impl FaxLauncher {
    /// 生产启动器：spawn 本 exe。
    pub fn production() -> Self {
        Self::default()
    }

    /// 测试缝：显式启动目标。
    pub fn with_spawn(spawn: SpawnTarget) -> Self {
        Self { spawn }
    }

    /// `ccOpenFax`（Go `bindFaxLauncher` 的闭包体）。
    ///
    /// 已有浮窗 → 置前 + `jsonOK("注水专线浮窗已在前台")`（**不再重复拉起**：
    /// 每台浮窗的发送都是全流程真检测、真烧配额，并发多开互相污染判定）；
    /// 否则 spawn `本 exe --fax [--portable]`（`cmd.Start()` 后不等待，浮窗独立存活）。
    pub fn open_fax(&self) -> String {
        if let Some(h) = fax_popup_handle() {
            win32::foreground_window(h);
            return jsons::json_ok("注水专线浮窗已在前台");
        }
        let (prog, mut args) = match self.spawn.resolve() {
            Ok(v) => v,
            Err(e) => return jsons::json_err(&e),
        };
        args.push("--fax".to_string());
        args.extend(appdir::portable_args());
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        match crate::proc::hidden_cmd(&prog.to_string_lossy(), &refs).spawn() {
            // Go: cmd.Start() 成功后**不 Wait**（浮窗独立存活，主窗关闭不影响）
            Ok(_child) => jsons::json_ok("注水专线浮窗已打开"),
            // Go 的 `exec.Command(exe,…).Start()` 失败时 `err.Error()` 会带程序路径
            // （`fork/exec <exe>: …`）——这里按同一形态把路径补进文案。
            Err(e) => jsons::json_err(&format!("启动 {} 失败: {e}", prog.display())),
        }
    }

    /// `ccFaxWin`（Go `main.go:131`）：`windowCmd(hwnd, cmd)`——拿不到句柄就什么都不做。
    /// 返回值是 `null`（Go 的闭包无返回，webview 侧得到 undefined）。
    ///
    /// ⚠ 有句柄时**真的**会把命令打到 Win32 那一层（`move`/`min`/`max`/`close`），
    /// 故单测只喂 `hwnd = 0`（= 拿不到句柄那条分支，不产生任何窗口副作用）；
    /// 「有句柄时命令字原样透传」由 `tests/shell_contract.rs` 的源码锚 +
    /// `win32::window_cmd` 自己的存在性锁定，**不**拿真实窗口当测试夹具。
    pub fn fax_win(&self, hwnd: Option<usize>, cmd: &str) -> String {
        if let Some(h) = hwnd {
            win32::window_cmd(h, cmd);
        }
        "null".to_string()
    }

    /// `ccFaxClose`（Go `main.go:143`）：`windowCmd(hwnd, "close")`。
    pub fn fax_close(&self, hwnd: Option<usize>) -> String {
        if let Some(h) = hwnd {
            win32::window_cmd(h, "close");
        }
        "null".to_string()
    }
}

/// 浮窗句柄（`None` = 本机没有注水浮窗）。生产走 `FindWindowW` 按标题精确匹配；
/// 测试可用 [`set_fax_popup_override`] 注入（**只影响本进程**，生产路径不经过它）。
pub fn fax_popup_handle() -> Option<usize> {
    if let Some(h) = fax_popup_override().lock().unwrap().as_ref().copied() {
        return h;
    }
    let h = win32::find_fax_popup();
    if h == 0 {
        None
    } else {
        Some(h)
    }
}

#[allow(clippy::type_complexity)]
fn fax_popup_override() -> &'static Mutex<Option<Option<usize>>> {
    static O: std::sync::OnceLock<Mutex<Option<Option<usize>>>> = std::sync::OnceLock::new();
    O.get_or_init(|| Mutex::new(None))
}

/// 测试缝：把「找浮窗」钉成给定结果（`None` = 钉成"找不到"；`Some(h)` = 钉成"句柄 h"）。
/// 传 `Some(None)` 恢复真实查询。**门禁专用**——生产代码从不调用。
pub fn set_fax_popup_override(v: Option<Option<usize>>) {
    *fax_popup_override().lock().unwrap() = v;
}

/// `runFaxWindow`：`--fax` 子进程侧的进程级单实例保险。
///
/// 返回 `Some(guard)` = 本次是唯一浮窗（守卫必须**活到进程退出**——句柄一关，
/// 名字就没了）；返回 `None` = 已有浮窗：把它置前，本进程应静默退出（Go 返回 0）。
pub fn fax_popup_mutex_guard() -> Option<single::NamedMutex> {
    match single::acquire(single::FAX_WINDOW_MUTEX) {
        Some(g) => Some(g),
        None => {
            if let Some(h) = fax_popup_handle() {
                win32::foreground_window(h);
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三个绑定的返回形状（Go 闭包无返回 → webview 侧是 undefined；Rust 侧用 `null` 字面量）。
    #[test]
    fn fax_bindings_return_shapes_match_go() {
        let l = FaxLauncher::production();
        // 拿不到句柄：什么都不做，回 null（不是空串、不是 {}）
        assert_eq!(l.fax_win(None, "min"), "null");
        assert_eq!(l.fax_close(None), "null");
        // 给 0 句柄等价于「拿不到」（Go 的 `w.Window() != nil` 判据）
        assert_eq!(l.fax_win(Some(0), "min"), "null");
        assert_eq!(l.fax_close(Some(0)), "null");
    }

    /// 已有浮窗的分支：置前 + 「已在前台」文案（**不再拉起子进程**——用假目标证明没 spawn）。
    #[test]
    fn open_fax_with_existing_popup_only_foregrounds() {
        let marker = std::env::temp_dir().join(format!(
            "rs-gui-fax-should-not-spawn-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let l = FaxLauncher::with_spawn(SpawnTarget {
            exe: Some("cmd".into()),
            prefix: vec![
                "/c".into(),
                format!("echo spawned>{}", marker.display()),
            ],
        });
        set_fax_popup_override(Some(Some(0x1234))); // 钉成"有浮窗"（用无效句柄，置前是安全空操作）
        let out = l.open_fax();
        set_fax_popup_override(None);
        assert_eq!(out, r#"{"msg":"注水专线浮窗已在前台","ok":true}"#);
        assert!(!marker.exists(), "已有浮窗时绝不能再拉起子进程");
    }

    /// spawn 失败（目标不存在）→ `jsonErr`，且**不 panic**。
    #[test]
    fn open_fax_reports_spawn_failure() {
        let l = FaxLauncher::with_spawn(SpawnTarget {
            exe: Some(std::path::PathBuf::from(
                "Z:\\definitely-missing\\rs-gui-nope.exe",
            )),
            prefix: Vec::new(),
        });
        set_fax_popup_override(Some(None)); // 钉成"找不到浮窗"，逼进 spawn 分支
        let out = l.open_fax();
        set_fax_popup_override(None);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], serde_json::Value::Bool(false));
        assert!(
            v["msg"].as_str().unwrap().contains("Z:\\definitely-missing"),
            "错误文案必须带程序路径（Go jsonErr(err) 同形态）: {out}"
        );
    }

    /// 真单实例：拿住守卫时第二次必失败（浮窗单实例的判据本身）。
    /// ⚠ 本用例**不**调 `fax_popup_mutex_guard`（那会占住 `Local\ProxyDeck-FaxWindow`
    /// 这个真名字，与别的用例/真实浮窗抢），只用 `single` 的独立名字验证语义。
    #[test]
    fn single_instance_semantics_are_process_wide() {
        let name = format!("Local\\rs-gui-faxtool-test-{}", std::process::id());
        let g = single::acquire(&name).expect("第一次拿到");
        assert_ne!(g.raw_handle(), 0);
        assert!(single::acquire(&name).is_none(), "第二次必须报已有实例");
    }
}
