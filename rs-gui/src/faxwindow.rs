//! faxwindow.rs —— 注水专线独立浮窗（对应 Go `app/main.go:87-151` 的 `runFaxWindow`）。
//!
//! 形态与 Go 逐条同源：
//!   * **独立小窗**，与主 GUI 互不干扰、**主窗关闭仍存活**（它是 GUI spawn 的独立进程）
//!   * 复用 GUI 的窗口创建路径：进程内 HTTP 服务内嵌 ui.html（`127.0.0.1` 随机端口）→
//!     挂全桥（弹窗内 `HAS_BRIDGE=true`，真渠道/真检测/真历史）→ `Navigate` 加 `#fax`
//!     独立模式 → 消息循环
//!   * 标题 `注水专线 · WATER PROBE FX-01`（`ccOpenFax` 与单实例守卫都按它找窗）
//!   * **580×784 原生渲染**：不用 CSS zoom（缩放毁文字清晰度，1:1 才是可读性的根）
//!   * 去原生标题栏 + 吃掉 DWM 保留边框 + 把客户区钉回 580×784（`SetSize` 会被库按
//!     标题栏 metrics 外扩成 596×823，实测在案）+ 窗口图标
//!   * 三灯直绑桥：`ccFaxWin` / `ccFaxClose`（Go 第 17 轮：不依赖 `ccWindowCmd`，
//!     因为 SC_MAXIMIZE 在 frameless 窗语义异常、继承桥挂载与 `#fax` 页加载时序不可控）
//!
//! 单实例：进程名互斥体（`Local\ProxyDeck-FaxWindow`）——已有浮窗就把它置前、本进程自退，
//! **绝不重复弹出**（每台浮窗的发送都是全流程真检测、真烧 quota，并发多开互相污染判定）。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::http::Request;
use wry::{WebView, WebViewBuilder};

use crate::bridge::{Bridge, BINDINGS, FAX_BINDINGS, PLUGIN_BINDINGS};
use crate::faxtool::{self, FAX_HEIGHT, FAX_WIDTH};
use crate::host::{Host, WindowHost};
use crate::state::App;
use crate::uihtml::UI_HTML;
use crate::webview::{bridge_shim, handle_ipc, serve_ui_html, window_hwnd};
use crate::win32;

/// 起注水专线浮窗（`--fax` 分支）。返回 `Err` 表示窗口/WebView 建不起来。
///
/// 调用方（`dispatch`）已先做单实例判定（[`faxtool::fax_popup_mutex_guard`]）——
/// 已有浮窗时**根本不会走到这里**，不会起窗、不会闪一下。
pub fn run_fax_window(app: Arc<App>) -> Result<(), String> {
    let url = serve_ui_html()?;

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(win32::FAX_TITLE)
        .with_inner_size(tao::dpi::LogicalSize::new(
            FAX_WIDTH as f64,
            FAX_HEIGHT as f64,
        ))
        .build(&event_loop)
        .map_err(|e| format!("创建浮窗失败: {e}"))?;

    // 无边框 + 客户区铺满 + 尺寸钉回：与 Go `runFaxWindow` 里那两处（同步 + Dispatch 二次保险）
    // 同一组动作——这里在起消息循环之前一次做完，语义相同（Go 的 Dispatch 是给"句柄早期未就绪"
    // 兜底，Rust 的 `build()` 返回时句柄已经可用，故无需二次）。
    let hwnd = window_hwnd(&window);
    if let Some(h) = hwnd {
        win32::set_frameless(h);
        win32::frameless_full_client(h);
        win32::set_client_size(h, FAX_WIDTH, FAX_HEIGHT);
        win32::set_window_icon(h);
    }

    let launcher = faxtool::FaxLauncher::production();
    let host: Arc<dyn Host> = match hwnd {
        Some(h) => Arc::new(WindowHost::new(h, move || crate::webview::post_wm_close(h))),
        // 拿不到句柄：三灯/关窗都退化成空操作（Go 的 `w.Window() == nil` 同分支）
        None => Arc::new(crate::host::NoWindowHost),
    };
    let bridge = Bridge::new(Arc::clone(&app), host).with_fax(Arc::new(launcher));

    let slot: Rc<RefCell<Option<WebView>>> = Rc::new(RefCell::new(None));
    let slot_for_ipc = Rc::clone(&slot);
    let webview = WebViewBuilder::new()
        .with_initialization_script(bridge_shim())
        .with_ipc_handler(move |req: Request<String>| {
            if let Some((id, text)) = handle_ipc(&bridge, req.body()) {
                if let Some(wv) = slot_for_ipc.borrow().as_ref() {
                    let js = format!("window.__pd_bridge_return({id}, {text});");
                    let _ = wv.evaluate_script(&js);
                }
            }
        })
        // `#fax` = 界面内的独立浮窗模式（Go：`w.Navigate("http://…/#fax")`）
        .with_url(format!("{url}#fax"))
        .build(&window)
        .map_err(|e| format!("创建浮窗失败（未检测到 WebView2 运行时？）: {e}"))?;
    *slot.borrow_mut() = Some(webview);
    window.set_visible(true);

    let slot_loop = Rc::clone(&slot);
    event_loop.run(move |event, _target, control_flow| {
        // 原生关窗（WM_CLOSE）→ 退出消息循环、进程自然结束。
        // 浮窗的关闭按钮走 `ccFaxClose` → `windowCmd(hwnd,"close")` = PostMessage WM_CLOSE
        // （`main.go:143-147`），所以**这条必须处理**，否则浮窗关不掉。
        // 与主窗同款（见 `webview.rs` 的说明）。
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
            return;
        }
        let _keep_alive = &slot_loop;
        *control_flow = ControlFlow::Wait;
    });
}

/// 桥名清单自检（门禁用）：33 个名字必须都被注入脚本覆盖。
/// Go 侧浮窗挂的是**同一个** `bindAll` + 两个直绑桥，故名字集合与主窗一致 + 3 个传真绑定。
pub fn fax_binding_names() -> Vec<String> {
    BINDINGS
        .iter()
        .chain(PLUGIN_BINDINGS.iter())
        .chain(FAX_BINDINGS.iter())
        .map(|s| s.to_string())
        .collect()
}

/// 浮窗页面里确实存在 `#fax` 分支的锚点（ui.html 是**只读合同**，这里只断言、不改）。
pub fn ui_html_has_fax_branch() -> bool {
    let html = String::from_utf8_lossy(UI_HTML);
    html.contains("location.hash === '#fax'")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 33 个桥名（26 + 4 + 3）与主窗注入面一致；浮窗不另开一套。
    #[test]
    fn fax_window_exposes_all_33_bindings() {
        let names = fax_binding_names();
        assert_eq!(names.len(), 33);
        for n in ["ccOpenFax", "ccFaxWin", "ccFaxClose"] {
            assert!(names.contains(&n.to_string()), "缺 {n}");
        }
    }

    /// 界面里必须有 `#fax` 分支（浮窗的整块功能都靠这个 hash 切进去）。
    /// 这条是**对只读合同 ui.html 的断言**，不是对它做任何改动。
    #[test]
    fn ui_html_carries_the_fax_branch() {
        assert!(
            ui_html_has_fax_branch(),
            "ui.html 必须含 `location.hash === '#fax'` 分支（浮窗模式入口）"
        );
    }
}
