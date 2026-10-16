//! rs-gui —— ProxyDeck GUI 宿主的 Rust 重写（绞杀者路线 P7a：桥接层与状态）。
//!
//! 形态（用户裁决，见简报 P7 形态裁决）：**单 exe 多子模式**——`rs-gui.exe` 同时承载
//! GUI 与各插件子模式，`-headless` 走主代理；六个 Rust crate 作为**库依赖**接入，
//! 由 [`dispatch`] 按 flag 分发。
//!
//! 阶段边界（P7a）：桥接层（26 个 `w.Bind`，见 [`bridge`]）+ 状态聚合（[`state`]）+
//! 分发骨架（[`dispatch`]）。插件托管（[`plugins`] 的启停/换血）、窗口外壳（托盘/无边框/
//! 单实例）、打包属 P7b/P7c/P7d，清单见 `rs-gui/README.md`。

pub mod appdir;
pub mod bridge;
pub mod dispatch;
pub mod dpapi;
pub mod host;
pub mod httpc;
pub mod jsons;
pub mod keyprobe;
pub mod plugins;
pub mod proc;
pub mod state;
pub mod uihtml;
pub mod update;
pub mod webview;

/// 应用版本（与 Go `app/main.go` 的 `appVersion` 同值；ui.html 的 `UI_CUR` 也认它）。
/// ⚠ 发版时的 8 处版本串清单里，Rust 侧对应这一处（P7d 打包时要补进清单）。
pub const APP_VERSION: &str = "v3.20.10";
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
