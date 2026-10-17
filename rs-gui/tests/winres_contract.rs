//! winres_contract.rs —— Windows 资源的**合同测试**（P7d-2）。
//!
//! `build.rs` 只做「编得进去」这件事；「编进去的东西对不对」在这里锁死：
//!
//! 1. 图标是 `app/icon.ico` 的**逐字节副本**（复用，不新画；两份漂移 = Rust exe 与
//!    Go exe 的窗口/托盘图标不一致）；
//! 2. `version.rc` 的四个版本数字与 `src/lib.rs` 的 `APP_VERSION` 同源
//!    （同一个 exe 不能有两种版本号：属性页说一套、`--version` 说另一套）；
//! 3. `winres.rc` 与两条资源 ID 的约定一致（图标 ID 必须是 1 —— `win32.rs` 的
//!    `LoadIconW(hInst, 1)` 与 `tray.rs` 的 `LoadImageW(hInst, 1, …)` 都按 1 取；
//!    换成别的 ID，窗口与托盘会静默退回系统默认图标）；
//! 4. VERSIONINFO 的 ID 与图标 ID **不是同一张表**（`1 VERSIONINFO` 与 `1 ICON` 可以
//!    共存），这里把它写成断言，免得后来人「顺手统一编号」把图标撞掉。

use std::path::{Path, PathBuf};

fn crate_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn read(name: &str) -> String {
    let p = crate_file(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不到 {}（{e}）", p.display()))
}

#[test]
fn icon_is_a_byte_identical_copy_of_app_icon() {
    let ours =
        std::fs::read(crate_file("icon.ico")).expect("rs-gui/icon.ico 必须存在（资源清单引用它）");
    let app_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("app")
        .join("icon.ico");
    let theirs = std::fs::read(&app_path)
        .unwrap_or_else(|e| panic!("读不到 {}（{e}）", app_path.display()));

    assert_eq!(
        ours, theirs,
        "rs-gui/icon.ico 必须是 app/icon.ico 的逐字节副本（图标是复用，不是重画）"
    );
    assert_eq!(
        ours.len(),
        32_880,
        "app/icon.ico 的字节数变了：确认这是有意的，再同步本断言"
    );
    assert_eq!(
        rs_gui::uihtml::sha256_hex(&ours),
        "73ca0076018cd27a259fdcf80bf9aa3abc87d268a5b3b2651bcac9e937b505b7",
        "icon.ico 的 SHA-256 变了：这一份是 app/icon.ico 的副本，变了说明原件或副本被改过"
    );
}

#[test]
fn version_rc_numbers_match_app_version() {
    let rc = read("version.rc");
    let app = rs_gui::APP_VERSION.trim_start_matches('v');
    let segs: Vec<&str> = app.split('.').collect();
    assert_eq!(
        segs.len(),
        3,
        "APP_VERSION 必须是三段位：{}",
        rs_gui::APP_VERSION
    );

    // FILEVERSION / PRODUCTVERSION：二进制形态 `X,Y,Z,0`
    let want_csv = format!("{},{},{},0", segs[0], segs[1], segs[2]);
    for key in ["FILEVERSION", "PRODUCTVERSION"] {
        let got = rc_line_value(&rc, key)
            .unwrap_or_else(|| panic!("version.rc 里找不到 {key}（必须显式写出，好让本测试核对）"));
        assert_eq!(
            got, want_csv,
            "version.rc 的 {key} 与 src/lib.rs 的 APP_VERSION（{}）不同源：\
             发版时两处必须一起改，否则 exe 属性页与 `--version` 会说两套版本",
            rs_gui::APP_VERSION
        );
    }

    // 字符串表：显示形态 `X.Y.Z.0`
    let want_dotted = format!("{app}.0");
    for key in ["FileVersion", "ProductVersion"] {
        let got = string_table_value(&rc, key)
            .unwrap_or_else(|| panic!("version.rc 的字符串表里找不到 {key}"));
        assert_eq!(
            got, want_dotted,
            "version.rc 字符串表的 {key} 与 APP_VERSION 不同源（属性页显示的就是它）"
        );
    }

    // 核心版本只有一处写法（Comments），也一并锁住
    let comments =
        string_table_value(&rc, "Comments").expect("version.rc 字符串表里必须有 Comments");
    assert!(
        comments.contains(rs_gui::CORE_VERSION),
        "Comments（{comments}）里应当含 CORE_VERSION（{}）",
        rs_gui::CORE_VERSION
    );
}

#[test]
fn winres_rc_keeps_the_resource_id_conventions() {
    let rc = read("winres.rc");

    // 图标行：ID 必须是 1（win32.rs / tray.rs 都按 1 取）
    let icon_lines: Vec<&str> = rc
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && l.contains("ICON"))
        .collect();
    assert_eq!(
        icon_lines.len(),
        1,
        "winres.rc 里应当只有一行图标声明（现状 {icon_lines:?}）"
    );
    assert_eq!(
        icon_lines[0], "1 ICON \"icon.ico\"",
        "图标行必须是 `1 ICON \"icon.ico\"`：ID 换成别的，win32.rs 的 LoadIconW(hInst, 1) \
         与 tray.rs 的 LoadImageW(hInst, 1, …) 就都取不到图标（静默退回系统默认图标）"
    );

    // VERSIONINFO 块：ID=1 与图标 ID 不在同一张表，可以共存；写死防「顺手统一编号」
    assert!(
        rc.contains("#include \"version.rc\""),
        "winres.rc 必须包含 version.rc（否则 exe 没有版本资源）"
    );
    let ver = read("version.rc");
    assert!(
        ver.contains("1 VERSIONINFO"),
        "version.rc 的 VERSIONINFO 资源 ID 必须是 1（与图标 ID 共存，不是同一张表）"
    );

    // 资源里不许出现中文：rc.exe 的 .res 代码页随环境变（CLAUDE.md 坑 13 同族问题）
    for (name, text) in [("winres.rc", &rc), ("version.rc", &ver)] {
        let non_ascii: Vec<char> = text.chars().filter(|c| !c.is_ascii()).collect();
        assert!(
            non_ascii.is_empty(),
            "{name} 里出现了非 ASCII 字符 {non_ascii:?}：.rc 一律只用 ASCII（代码页随环境变）"
        );
    }
}

/* ---------------- .rc 取值的两个小工具（与 build.rs 的判据同形） ---------------- */

/// 取 `KEY <值>`（`FILEVERSION 3,20,10,0` 这种无引号形态），已去掉空白。
fn rc_line_value(rc: &str, key: &str) -> Option<String> {
    for line in rc.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let Some(rest) = t.strip_prefix(key) else {
            continue;
        };
        if rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue; // 别把 FILEVERSIONX 当成 FILEVERSION
        }
        let v: String = rest
            .split("//")
            .next()
            .unwrap_or(rest)
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == ',')
            .collect();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

/// 取字符串表里 `VALUE "KEY", "VALUE"` 的 VALUE。
fn string_table_value(rc: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    for line in rc.lines() {
        let t = line.trim();
        if t.starts_with("//") || !t.starts_with("VALUE") {
            continue;
        }
        let Some(rest) = t["VALUE".len()..].trim_start().strip_prefix(&needle) else {
            continue;
        };
        let rest = rest.trim_start().trim_matches(',').trim_start();
        let inner = rest.strip_prefix('"')?;
        let end = inner.find('"')?;
        return Some(inner[..end].to_string());
    }
    None
}
