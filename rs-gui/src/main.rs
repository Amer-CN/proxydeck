//! rs-gui 可执行入口：单 exe 多子模式（无参 → GUI；`--plugin-*` → 插件服务；
//! `-headless` → 主代理核心）。分发实现见 [`rs_gui::dispatch`].

//! Windows 下不带控制台窗口启动（对齐 Go 构建的 `-H windowsgui`；否则每个后端
//! 进程都会弹一个黑窗口）。从已有控制台启动时（如 `--version` 自查）输出仍正常
//! ——该标记只决定"是否自动新建控制台"，不掐已有句柄。
#![windows_subsystem = "windows"]

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    std::process::exit(rs_gui::dispatch::run(argv));
}
