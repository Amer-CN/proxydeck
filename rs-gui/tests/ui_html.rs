//! ui_html.rs —— 界面文件合同：`rs-gui/ui.html` 必须是 `app/ui.html` 的**逐字节副本**。
//!
//! ui.html 的 JS 与 bridge 键名就是桥接层的接口定义，改一个字节 = 改接口。
//! 三道门禁里的第二、第三道（第一道是 `build.rs` 的构建期字节比对）：
//!   * 内嵌字节的 SHA-256 = 记录常量
//!   * 内嵌字节 = 仓库里 `app/ui.html` 的当前内容（逐字节）

use std::path::Path;

#[test]
fn embedded_ui_html_is_byte_identical_to_app_ui_html() {
    let app_ui = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("app")
        .join("ui.html");
    let app_bytes = std::fs::read(&app_ui).unwrap_or_else(|e| {
        panic!(
            "读不到 {}（{e}）：界面的唯一权威副本在 app/ui.html",
            app_ui.display()
        )
    });

    assert_eq!(
        rs_gui::uihtml::UI_HTML,
        app_bytes.as_slice(),
        "内嵌的 ui.html 必须与 app/ui.html 逐字节一致（不许改一个字节）"
    );
    assert_eq!(
        rs_gui::uihtml::sha256_hex(&app_bytes),
        rs_gui::uihtml::UI_HTML_SHA256,
        "app/ui.html 的哈希变了：说明界面被改过——同步 rs-gui/ui.html 并更新常量前，先确认这是有意的"
    );
    assert_eq!(rs_gui::uihtml::UI_HTML_LEN, app_bytes.len());
    assert_eq!(rs_gui::uihtml::embedded_sha256(), rs_gui::uihtml::UI_HTML_SHA256);
}

/// 界面用到的桥面**必须**被我们提供的绑定覆盖（反向漏网的才是事故：
/// UI 调一个宿主没有的函数 = 界面上那个功能静默失灵）。
///
/// 说明：`ccCalib` 是宿主提供、当前 ui.html **没有调用**的历史绑定（界面里零引用），
/// 它的存在性由 `bridge_contract.rs` 的黄金差分与存在性补跑负责，不归本测试管。
#[test]
fn ui_html_bridge_surface_is_covered_by_our_bindings() {
    let html = String::from_utf8_lossy(rs_gui::uihtml::UI_HTML).to_string();

    // UI 的两种引用形态：`window.ccX` 直呼，与 `call('X')` 经统一入口
    let mut used: std::collections::BTreeSet<String> = Default::default();
    let mut rest = html.as_str();
    while let Some(i) = rest.find("window.cc") {
        let tail = &rest[i + "window.".len()..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        // 注释里出现过裸的「window.cc 桥」这种写法，长度 ≤2 的抽取结果不算函数名
        if name.len() > 2 {
            used.insert(name);
        }
        rest = &rest[i + "window.cc".len()..];
    }
    let mut rest = html.as_str();
    while let Some(i) = rest.find("call('") {
        let tail = &rest[i + "call('".len()..];
        let name: String = tail.chars().take_while(|c| *c != '\'').collect();
        if !name.is_empty() {
            used.insert(format!("cc{name}"));
        }
        rest = &rest[i + "call('".len()..];
    }

    assert!(
        used.len() > 20,
        "抽取到的桥面太少（{used:?}）：抽取规则或界面结构变了"
    );

    // 26 个 bridge.go 绑定 + 4 个插件托管绑定（P7b）+ 3 个注水浮窗绑定（P7c）
    // —— 界面的调用面至此**全部**由本 crate 提供，`later` 清空。
    let later: [&str; 0] = [];
    let mut missing: Vec<&String> = Vec::new();
    for name in &used {
        let provided = rs_gui::bridge::BINDINGS.contains(&name.as_str())
            || rs_gui::bridge::PLUGIN_BINDINGS.contains(&name.as_str())
            || rs_gui::bridge::FAX_BINDINGS.contains(&name.as_str());
        if !provided && !later.contains(&name.as_str()) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "界面调用了我们没有提供的绑定：{missing:?}（26 个 bridge.go + 4 个插件托管 + 3 个传真之外不该有别的）"
    );

    // 三个传真绑定确实被界面用到（否则「已提供」这件事就没被测到）
    for n in ["ccOpenFax", "ccFaxWin", "ccFaxClose"] {
        assert!(
            used.contains(n),
            "界面调用面里应当有 {n}（浮窗三灯/关窗/拉起浮窗都走它）"
        );
    }

    // 桥存在性判据的锚点（合同锚点变了要重新对齐）
    assert!(html.contains("HAS_BRIDGE"));
    assert!(
        html.contains("typeof window.ccGetState === 'function'"),
        "桥存在性判据的锚点变了：合同已变，需重新对齐"
    );
    assert!(
        html.contains("Promise.resolve(window['cc' + fn].apply(null, args))"),
        "统一调用入口的形态变了：桥的返回值约定（串/Promise 皆可）需重新核对"
    );
}

/// 更新日志浮层的版本号必须**唯一且严格降序**。
///
/// 为什么要有这条：2026-10-18 用户发现浮层里连着一排同样的 `v4.2.5`——根因是发版脚本
/// 对 ui.html 做了全局 `replace(旧版本 → 新版本)`，把 `CHANGELOG_DATA` 里的**历史条目**
/// 也一起改了名。每发一版，所有旧条目都被改成当版版本号。
/// 发版脚本只能替换那 6 个固定位置的版本串，**不得对整文件做版本串替换**。
#[test]
fn changelog_versions_are_unique_and_strictly_descending() {
    let app_ui = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("app")
        .join("ui.html");
    let text = std::fs::read_to_string(&app_ui).expect("读 app/ui.html");

    let start = text.find("var CHANGELOG_DATA").expect("找不到 CHANGELOG_DATA");
    let body = &text[start..];
    let end = body.find("\n  ];").expect("CHANGELOG_DATA 数组没闭合");
    let body = &body[..end];

    let mut versions: Vec<(u32, u32, u32)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for chunk in body.split("{ v: '").skip(1) {
        let v = chunk.split('\'').next().unwrap_or("");
        let digits: Vec<u32> = v
            .trim_start_matches('v')
            .split('.')
            .filter_map(|x| x.parse::<u32>().ok())
            .collect();
        assert_eq!(digits.len(), 3, "版本号形态不对：{v}");
        assert!(
            !seen.iter().any(|s| s == v),
            "更新日志里出现重复版本 {v}（典型成因：发版脚本对 ui.html 做了全局版本串替换）"
        );
        seen.push(v.to_string());
        versions.push((digits[0], digits[1], digits[2]));
    }

    assert!(versions.len() >= 10, "更新日志条目太少（{}）", versions.len());
    for w in versions.windows(2) {
        assert!(
            w[0] > w[1],
            "更新日志必须严格降序（新→旧），但 {:?} 之后是 {:?}",
            w[0],
            w[1]
        );
    }
}
