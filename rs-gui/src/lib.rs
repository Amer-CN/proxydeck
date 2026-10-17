//! rs-gui —— ProxyDeck GUI 宿主的 Rust 重写（绞杀者路线 P7a~P7c）。
//!
//! 形态（用户裁决，见简报 P7 形态裁决）：**单 exe 多子模式**——`rs-gui.exe` 同时承载
//! GUI 与各插件子模式，`-headless` 走主代理；六个 Rust crate 作为**库依赖**接入，
//! 由 [`dispatch`] 按 flag 分发。
//!
//! 阶段边界：
//!   * P7a 桥接层（26 个 `w.Bind`，见 [`bridge`]）+ 状态聚合（[`state`]）+ 分发骨架
//!   * P7b 插件托管（[`plugins`]：注册表 / 探活接管 / 拉杆启停 / 退出监视 / 停堆复验
//!     + `ccPlugin*` 四绑定）
//!   * P7c 宿主外壳：[`win32`]（无边框/图标/DPI/窗口命令/可见性自愈）、[`tray`]（托盘整套）、
//!     [`single`]（命名互斥体单实例）、[`faxtool`] + [`faxwindow`]（注水浮窗与 3 个绑定）、
//!     [`rollover`]（换血）、[`migrate`]（历史状态搬入 + 明文凭据升级）
//!
//! 打包（单 exe 图标/版本资源、多二进制取舍）属 P7d，清单见 `rs-gui/README.md`。

pub mod appdir;
pub mod bridge;
pub mod dispatch;
pub mod dpapi;
pub mod faxtool;
pub mod faxwindow;
pub mod host;
pub mod httpc;
pub mod jsons;
pub mod keyprobe;
pub mod migrate;
pub mod plugins;
pub mod proc;
pub mod rollover;
pub mod single;
pub mod state;
pub mod tray;
pub mod uihtml;
pub mod update;
pub mod webview;
pub mod win32;

/// 插件托管层的真实行为测试（起假后端进程 / 占测试端口；只在测试构建里编）。
#[cfg(test)]
mod plugin_hosting_tests;

/// 宿主外壳层的真实行为测试（换血起假进程 / Migrate 临时目录 / headless 端口清理；
/// 只在测试构建里编）。
#[cfg(test)]
mod shell_tests;

/// 应用版本（v4.0.0 起 Rust 即产品本体；Go 侧冻结在 v3.20.11）。
pub const APP_VERSION: &str = "v4.0.5";
/// 代理核心版本（与 Go `coreVersion` 同值）。
pub const CORE_VERSION: &str = "v1.2.0";
/// 窗口标题（与 Go `appTitle` 同值）。
pub const APP_TITLE: &str = "ProxyDeck · 多平台代理控制台";

#[cfg(test)]
pub mod testutil {
    use std::sync::Mutex;

    /// 单元测试里改环境变量（`PROXYDECK_DATA_DIR` / `APPDATA`）必须串行，
    /// 否则同一个测试二进制内的并行用例会互相踩数据目录。
    pub fn env_lock() -> &'static Mutex<()> {
        static L: Mutex<()> = Mutex::new(());
        &L
    }
}
