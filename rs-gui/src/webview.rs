//! webview.rs —— GUI 外壳（wry + tao）。Windows 上 wry 底层就是 WebView2，与 Go 版
//! `webview_go` 同一个渲染器。
//!
//! 形态（P7c 补全宿主外壳后）：
//!   * 窗口（540×760，标题同 Go `appTitle`）+ **无边框** + **窗口图标** + **DPI 感知**
//!   * 界面经 `127.0.0.1:随机端口` 的进程内 HTTP 服务提供（**与 Go 同款**：
//!     `http://localhost` 属安全上下文，ui.html 的 `navigator.clipboard` 等 API 才可用）
//!   * 33 个桥函数（26 + 4 插件托管 + 3 传真）注入 + IPC 派发到 [`Bridge::dispatch`]
//!   * **托盘常驻**（起窗即挂图标）+ **600ms 可见性自愈**（一键更新重启后旧窗仍持前台时
//!     新窗会停在不可见——Go 第 47 轮那条自愈）
//!
//! 与 Go `main()` 的对应次序：`setDPIAware`（flag.Parse 之前）→ 起界面服务 → 建窗 →
//! `setFrameless`/`setWindowIcon` → `trayStartup` → 等 600ms → `showWindowIfHidden`
//! → `bindAll` → Navigate → Run。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use tao::dpi::LogicalSize;
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::http::Request;
use wry::{WebView, WebViewBuilder};

use crate::bridge::{Bridge, BINDINGS, FAX_BINDINGS, PLUGIN_BINDINGS};
use crate::host::{Host, NoWindowHost, WindowHost};
use crate::state::App;
use crate::uihtml::UI_HTML;
use crate::win32;

/// 起 GUI 并进入消息循环（无参启动）。返回 `Err` 表示窗口/WebView 建不起来。
pub fn run_gui(app: Arc<App>) -> Result<(), String> {
    // 界面服务必须先起：WebView 创建时就要 Navigate 到它
    let url = serve_ui_html()?;

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(crate::APP_TITLE)
        .with_inner_size(LogicalSize::new(540.0, 760.0))
        .build(&event_loop)
        .map_err(|e| format!("创建窗口失败: {e}"))?;

    let hwnd = window_hwnd(&window);
    // 自绘无边框 + 窗口图标（Go `main.go:277-286`：同步一次）
    if let Some(h) = hwnd {
        win32::set_frameless(h);
        win32::set_window_icon(h);
    }
    let host: Arc<dyn Host> = match hwnd {
        Some(h) => Arc::new(WindowHost::new(h, move || post_wm_close(h))),
        None => Arc::new(NoWindowHost),
    };
    let bridge = Bridge::new(Arc::clone(&app), host);

    // WebView 与 IPC 回调互相需要：用一个共享槽把 WebView 交给回调（同线程，Rc 足够）
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
        .with_url(url)
        .build(&window)
        .map_err(|e| {
            format!("创建 WebView 失败（未检测到 WebView2 运行时？）: {e}")
        })?;
    *slot.borrow_mut() = Some(webview);
    window.set_visible(true);

    // 托盘常驻：图标从进程起来就挂着（Go `main.go:299` 之前那处 `trayStartup`）。
    // 起不来只写日志，界面照常可用。
    crate::host::startup_tray(hwnd);

    // 一键更新重启的自愈（Go `main.go:287-297`）：新进程起窗时旧窗可能仍持前台
    // （前台锁定规则让新窗创建后停在不可见），延迟 600ms 检查并带出。
    std::thread::sleep(std::time::Duration::from_millis(600));
    if let Some(h) = hwnd {
        win32::show_window_if_hidden(h);
    }

    // 后台刷新循环（等价 Go `bindAll` 里起的两个常驻 goroutine：更新检查 + 官网用量）
    app.start_background();

    let slot_loop = Rc::clone(&slot);
    event_loop.run(move |_event, _target, control_flow| {
        let _keep_alive = &slot_loop;
        *control_flow = ControlFlow::Wait;
    });
}

/// 解析一条 IPC 消息并派发：`{"id":N,"name":"ccX","args":[...]}` → `(id, JSON 串字面量)`。
pub fn handle_ipc(bridge: &Bridge, body: &str) -> Option<(u64, String)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let id = v.get("id")?.as_u64()?;
    let name = v.get("name")?.as_str()?.to_string();
    let args: Vec<serde_json::Value> = v
        .get("args")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let out = bridge
        .dispatch(&name, &args)
        .unwrap_or_else(|e| crate::jsons::json_err(&e));
    Some((id, serde_json::to_string(&out).ok()?))
}

/// 注入的 JS 桥：把 33 个绑定名（26 个 bridge.go + 4 个插件托管 + 3 个传真）定义成全局函数，
/// 返回 Promise（ui.html 用 `Promise.resolve(window['cc'+fn](...))`，串与 Promise 都吃）。
pub fn bridge_shim() -> String {
    let names: Vec<String> = BINDINGS
        .iter()
        .chain(PLUGIN_BINDINGS.iter())
        .chain(FAX_BINDINGS.iter())
        .map(|n| format!("\"{n}\""))
        .collect();
    format!(
        r#"(function(){{
  var pending = {{}}, seq = 0;
  window.__pd_bridge_call = function(name, args) {{
    return new Promise(function(resolve) {{
      var id = ++seq;
      pending[id] = resolve;
      window.ipc.postMessage(JSON.stringify({{ id: id, name: name, args: args }}));
    }});
  }};
  window.__pd_bridge_return = function(id, out) {{
    var r = pending[id];
    if (r) {{ delete pending[id]; r(out); }}
  }};
  [{names}].forEach(function(n) {{
    window[n] = function() {{
      return window.__pd_bridge_call(n, Array.prototype.slice.call(arguments));
    }};
  }});
}})();"#,
        names = names.join(", ")
    )
}

/// 进程内最小 HTTP 服务：只伺候 ui.html 一个响应（对齐 Go `main.go:248-259` 的
/// `http.ServeMux` + `http.Serve`，含 `Cache-Control: no-store`）。
pub fn serve_ui_html() -> Result<String, String> {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let ln = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("无法创建本地界面服务：{e}"))?;
    let addr = ln.local_addr().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        for stream in ln.incoming() {
            let Ok(mut s) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Cache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    UI_HTML.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(UI_HTML);
                let _ = s.flush();
            });
        }
    });
    Ok(format!("http://{addr}/"))
}

/// 取窗口的 Win32 句柄（拿不到就退回「无窗口宿主」，桥按 Go 的句柄不可用路径如实报错）。
#[cfg(windows)]
pub fn window_hwnd<W: tao::rwh_06::HasWindowHandle>(w: &W) -> Option<usize> {
    match w.window_handle().ok()?.as_raw() {
        tao::rwh_06::RawWindowHandle::Win32(h) => Some(h.hwnd.get() as usize),
        _ => None,
    }
}

#[cfg(not(windows))]
pub fn window_hwnd<W: tao::rwh_06::HasWindowHandle>(_w: &W) -> Option<usize> {
    None
}

/// 关窗：PostMessage WM_CLOSE（与 Go `trayCloseMain` / `windowCmd(hwnd,"close")` 同款语义：
/// 消息循环干净返回 → 正常退出，不整窗销毁）。
#[cfg(windows)]
pub fn post_wm_close(hwnd: usize) {
    crate::win32::window_cmd(hwnd, "close");
}

#[cfg(not(windows))]
pub fn post_wm_close(_hwnd: usize) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_defines_all_33_bindings() {
        let js = bridge_shim();
        for n in BINDINGS {
            assert!(js.contains(&format!("\"{n}\"")), "注入脚本缺少 {n}");
        }
        for n in PLUGIN_BINDINGS {
            assert!(js.contains(&format!("\"{n}\"")), "注入脚本缺少插件托管绑定 {n}");
        }
        for n in FAX_BINDINGS {
            assert!(js.contains(&format!("\"{n}\"")), "注入脚本缺少传真绑定 {n}");
        }
        assert!(js.contains("window.ipc.postMessage"), "必须走 wry 的 IPC 通道");
        assert!(js.contains("Array.prototype.slice.call(arguments)"), "实参按位置透传");
    }

    #[test]
    fn ipc_dispatch_returns_json_literal_for_eval() {
        // 与 bridge.rs 的关窗/键位测试同抢进程级 DATA_DIR_ENV：必须串行，否则对方写进
        // 本目录的 close_mode.txt 会把这里期望的 "ask" 变成 "tray"/"quit"（并行必现 flake）。
        let _g = crate::testutil::env_lock().lock().unwrap();
        let dir = std::env::temp_dir().join("rs-gui-webview-ipc");
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var(crate::appdir::DATA_DIR_ENV, &dir);
        let bridge = Bridge::new(
            Arc::new(App::new("127.0.0.1", "57399", "")),
            Arc::new(NoWindowHost),
        );
        let (id, text) = handle_ipc(
            &bridge,
            r#"{"id":7,"name":"ccCloseModeGet","args":[]}"#,
        )
        .unwrap();
        assert_eq!(id, 7);
        // 回的是**字符串字面量**（evaluate_script 里直接当 JS 值用）
        assert_eq!(text, r#""{\"mode\":\"ask\",\"ok\":true}""#);
        assert!(handle_ipc(&bridge, "not json").is_none());
        std::env::remove_var(crate::appdir::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
