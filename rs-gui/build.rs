/// build.rs —— rs-gui 的**构建期**门禁 + Windows 资源编译（P7d-2）。
///
/// 1. `ui.html` 与 `app/ui.html` 必须逐字节相同（P7a 起）；
///    用字节比对而不是哈希比对：失败信息能直接说清「哪个文件、差多少字节」。
/// 2. `icon.ico` 与 `app/icon.ico` 必须逐字节相同（P7d-2 起）：
///    图标资源是**复用** Go 侧那份，不是新画一份——两份一旦漂移，Rust exe 与
///    Go exe 的窗口/托盘图标就长得不一样。
/// 3. 资源编译（P7d-2 起）：`winres.rc` → exe（`1 ICON "icon.ico"` + `version.rc` 的
///    VERSIONINFO）。走 `embed-resource`（rc.exe / llvm-rc / windres 的发现逻辑是现成的，
///    不自己手搓）。**仅 Windows 目标**：别处整段跳过，不影响其它平台构建。
///
/// `version.rc` 里的版本数字与 `src/lib.rs` 的 `APP_VERSION` 同源这件事，由
/// `tests/winres_contract.rs` 在测试期拦死（**不在 build.rs 里做**：那是「合同」
/// 不是「构建产物」，放测试里失败信息更清楚，也不用在 build.rs 里手搓解析/SHA-256）。

fn main() {
    check_link_sources();
    compile_windows_resources();
}

/// ui.html 与 icon.ico 都必须与 `app/` 侧逐字节相同（「复用原件」的两条门禁）。
fn check_link_sources() {
    check_same_bytes("ui.html", "../app/ui.html", "`Copy-Item app/ui.html rs-gui/ui.html`");
    check_same_bytes("icon.ico", "../app/icon.ico", "`Copy-Item app/icon.ico rs-gui/icon.ico`");
}

/// 逐字节比对两份文件；不同就 panic（附字节数便于定位）。
///
/// 路径一律是**相对 crate 根**的字符串（`std::fs::read` 在 build script 里的 cwd
/// 就是 `CARGO_MANIFEST_DIR`），不引 `std::path::Path`——用了也是白引（会告 unused import）。
fn check_same_bytes(bundled: &str, source: &str, fix: &str) {
    let a = std::fs::read(bundled)
        .unwrap_or_else(|e| panic!("rs-gui/{bundled} 读不到（{e}）：它必须是从 {source} 逐字节复制过来的副本"));
    let b = std::fs::read(source)
        .unwrap_or_else(|e| panic!("{source} 读不到（{e}）：请确认构建目录是仓库内的 rs-gui/"));
    if a != b {
        panic!(
            "{bundled} 与 {source} 不是逐字节相同（rs-gui 侧 {} 字节，app 侧 {} 字节）。\
             这两份文件都是**复用** app 侧原件、只许原样复制：\
              请用 {fix} 覆盖后重编。",
            a.len(),
            b.len()
        );
    }
}

/// 把 `winres.rc`（含 `#include "version.rc"` 与图标行）编译进 exe。
///
/// 只对 Windows 目标生效；`CARGO_CFG_WINDOWS` 由 cargo 注入（不用额外依赖）。
fn compile_windows_resources() {
    println!("cargo:rerun-if-changed=winres.rc");
    println!("cargo:rerun-if-changed=version.rc");
    println!("cargo:rerun-if-changed=icon.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    // 第二个参数 = 额外要链接的清单文件（`&[]`：没有）。与 VERSIONINFO 无关。
    //
    // `manifest_required()`：`NotAttempted`（找不到 rc.exe）与 `Failed`（编失败）都当场红。
    // **不用 `manifest_optional()`**——那会把「没编成」当成功吞掉，
    // 于是 exe 悄悄没有图标/版本号，而构建还是绿的（不接受静默降级）。
    if let Err(r) = embed_resource::compile("winres.rc", embed_resource::NONE).manifest_required() {
        panic!("Windows 资源编译失败（winres.rc → exe）: {r}");
    }
}
