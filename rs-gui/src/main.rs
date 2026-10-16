//! rs-gui 可执行入口：单 exe 多子模式（无参 → GUI；`--plugin-*` → 插件服务；
//! `-headless` → 主代理核心）。分发实现见 [`rs_gui::dispatch`]。

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    std::process::exit(rs_gui::dispatch::run(argv));
}
