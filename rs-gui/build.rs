//! build.rs —— ui.html 的**构建期**门禁：`rs-gui/ui.html` 必须与 `app/ui.html`
//! 逐字节相同（ui.html 是桥接层的接口合同，改一个字节就是改接口）。
//!
//! 用字节比对而不是哈希比对：失败信息能直接说清「哪个文件、差多少字节」，
//! 且不引入 build-dependencies（sha2 在测试期另有一道哈希断言）。

use std::path::Path;

fn main() {
    let bundled = Path::new("ui.html");
    let source = Path::new("../app/ui.html");
    println!("cargo:rerun-if-changed=ui.html");
    println!("cargo:rerun-if-changed=../app/ui.html");

    let a = std::fs::read(bundled).unwrap_or_else(|e| {
        panic!(
            "rs-gui/ui.html 读不到（{e}）：ui.html 必须是从 app/ui.html 逐字节复制过来的副本"
        )
    });
    let b = std::fs::read(source).unwrap_or_else(|e| {
        panic!("app/ui.html 读不到（{e}）：请确认构建目录是仓库内的 rs-gui/")
    });
    if a != b {
        panic!(
            "ui.html 与 app/ui.html 不是逐字节相同（rs-gui 侧 {} 字节，app 侧 {} 字节）。\
             ui.html 的 JS 与 bridge 键名是合同，**不许改一个字节**：\
             请用 `Copy-Item app/ui.html rs-gui/ui.html` 原样覆盖后重编。",
            a.len(),
            b.len()
        );
    }
}
