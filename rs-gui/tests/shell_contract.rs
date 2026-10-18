//! shell_contract.rs —— P7c 宿主外壳的门禁：**传真 3 绑定 + 单实例 + Migrate 的差分对照**。
//!
//! 参考值来源 `.work/p7c/p7c-go-reference.json`：由 `.work/rust-rewrite/_p7c_collect_test.go`
//! （P7b 采集器的同款手法：给 `webview.WebView` 写假实现，让**生产闭包**原样交出来再按位置调用）
//! 跑出来的，**不是转抄源码**。
//!
//! 三件事分开说清楚（不夸大覆盖度）：
//!
//! 1. **`ccOpenFax`**：采到的是「端口上已有浮窗」这一支——采集器用 **消息窗**
//!    （`HWND_MESSAGE`，不可见、不入任务栏/Alt+Tab）冒充浮窗驱动同一个生产闭包，
//!    返回的字符串与生产逐字同源。另一支（没有浮窗 → spawn 子进程）会**真的弹一个窗口**
//!    在用户桌面上，本轮纪律是「不动用户桌面」，故**未采**；那支的行为由
//!    `faxtool.rs` 的单测（假 spawn 目标：证明「已有浮窗时不 spawn」「起不来时回 jsonErr」）
//!    与源码锚（见下）分别覆盖。
//! 2. **`ccFaxWin` / `ccFaxClose`**：这两个闭包只在 `runFaxWindow()` 里创建，而该函数的
//!    其余部分会真起窗 + 跑消息循环——采集器一调它就会在用户桌面弹窗。故它们的取证是
//!    **源码锚**（[`fax_bindings_are_anchored_to_go_source`] 逐字断言 `app/main.go` 里那两段
//!    调用的形态）+ **Rust 侧真值表**（注入宿主句柄驱动三个分支）。**不冒充**「Go 侧实调」。
//! 3. **`Migrate` / `UpgradePlaintext`**：真值表逐条对照（同一个库函数，两侧同判）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use rs_gui::bridge::{Bridge, FAX_BINDINGS, BINDINGS, PLUGIN_BINDINGS};
use rs_gui::faxtool::{FaxLauncher, SpawnTarget};
use rs_gui::host::NoWindowHost;
use rs_gui::state::App;

/// 本测试二进制自己的进程级 env 串行锁（P7a 立下的纪律：同抢 `PROXYDECK_DATA_DIR`
/// 的测试必须串行）。`rs_gui::testutil::env_lock` 是 lib 单测段专用（`#[cfg(test)]`），
/// 集成测试拿不到，故这里自己持一把——本文件里只有 upgrade 一例会改它。
fn env_lock() -> &'static std::sync::Mutex<()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &L
}

/// Go 侧采出来的 P7c 基准（`.work/p7c/` 下；.work 是 gitignore 的临时材料目录）。
fn golden() -> Value {
    // 入库副本优先（干净检出可跑）、缺则回落 .work（P1 第 4 轮立下的口径）。
    let vendored = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("p7c-go-reference.json");
    let p = if vendored.exists() {
        vendored
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".work")
            .join("p7c")
            .join("p7c-go-reference.json")
    };
    let body = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "读不到 P7c 黄金 {}（{e}）：先按 .work/rust-rewrite/_p7c_collect_test.go 的用法重采",
            p.display()
        )
    });
    serde_json::from_str(&body).expect("P7c 黄金 JSON 解析失败")
}

fn repo_file(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel);
    let raw = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("读不到 {}（{e}）", p.display()));
    // 读到的可能是 **CRLF**（Go 源码当年是 autocrlf 产物、冻结快照也沿用它）；
    // 锚按 `\n` 写，故读进来先归一化行尾（只影响断言文本，不改磁盘上的文件一个字节）。
    raw.replace("\r\n", "\n")
}

/// 临时数据目录（每个用例一个；用完删）。
fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rs-gui-shell-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/* ---------------- 1. 绑定清单：P7c 没有动那 30 个，且 3 个传真绑定就位 ---------------- */

#[test]
fn bindings_are_26_plus_4_plus_3_and_match_go() {
    let g = golden();
    let go26: Vec<String> = g["bindings26"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        go26,
        BINDINGS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "26 个 bridge.go 绑定名/顺序必须与 Go 黄金逐个一致（P7c 一个都没动）"
    );
    let go_plugin: Vec<String> = g["bindings_plugin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        go_plugin,
        PLUGIN_BINDINGS.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    );
    let go_fax: Vec<String> = g["bindings_fax"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        go_fax,
        FAX_BINDINGS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "传真 3 绑定的名字与顺序必须与 Go 黄金一致"
    );
    assert_eq!(BINDINGS.len() + PLUGIN_BINDINGS.len() + FAX_BINDINGS.len(), 33);
}

/* ---------------- 2. ccOpenFax：与 Go 逐字节同形 ---------------- */

/// 「端口上已有浮窗」这一支的返回值必须与 Go **逐字节同形**（键序 `msg`→`ok`、文案、换行）。
/// Rust 侧用 [`rs_gui::faxtool::set_fax_popup_override`] 把「找浮窗」钉成"找到一个句柄"
/// ——这正是采集器用消息窗达到的同一个状态，且不会在用户桌面上动任何窗口。
#[test]
fn cc_open_fax_existing_popup_matches_go_byte_for_byte() {
    let g = golden();
    let cases = g["fax_open_cases"].as_array().expect("黄金缺 fax_open_cases");
    let case = cases
        .iter()
        .find(|c| c["name"] == "open-fax-existing-popup")
        .expect("黄金缺 open-fax-existing-popup 用例");
    let want = case["out"].as_str().unwrap();

    // 假 spawn 目标：若这条路径真去 spawn，会把标记文件写出来——用它证明「没 spawn」
    let marker = temp_dir("fax-nospawn").join("spawned.txt");
    let launcher = FaxLauncher::with_spawn(SpawnTarget {
        exe: Some("cmd".into()),
        prefix: vec![
            "/c".into(),
            format!("echo spawned>{}", marker.display()),
        ],
    });

    rs_gui::faxtool::set_fax_popup_override(Some(Some(0x1234)));
    let app = Arc::new(App::new("127.0.0.1", "57390", ""));
    let bridge =
        Bridge::new(app, Arc::new(NoWindowHost)).with_fax(Arc::new(launcher));
    let got = bridge.dispatch("ccOpenFax", &[]).unwrap();
    rs_gui::faxtool::set_fax_popup_override(None);

    assert_eq!(got, want, "ccOpenFax（已有浮窗分支）必须与 Go 逐字节同形");
    assert!(!marker.exists(), "已有浮窗时绝不能再拉起子进程（Go 注释：并发多开互相污染判定）");
    let _ = std::fs::remove_dir_all(marker.parent().unwrap());
}

/* ---------------- 3. ccFaxWin / ccFaxClose：源码锚 + Rust 真值表 ---------------- */

/// 这两个绑定的 Go 侧实现在 `app/main.go` 的 `runFaxWindow()` 里（起窗之后），
/// 采集器无法在不弹窗的前提下把它交出来。故这里**逐字断言源码形态**：
/// 改动这两段（换调用、换命令字、改句柄判据）都会让本条红。
///
/// 断言的是 `app/main.go` 的原文（**只读**，本条不修改它一个字节）。
/// 传真三绑定（ccFaxWin / ccFaxClose / ccOpenFax）的 **Rust 实现必须与当年 Go 的
/// 实现形态一致**。
///
/// ⚠ 2026-10-18 Go 参考实现整体移除后，本用例改读**冻结快照**
/// `tests/golden/fax-bindings-go-reference.txt`（就是删除前的 `app/main.go` 原文）。
/// 这样「当年确实是照这个形态移植的」这条证据永久留在仓库里，不随源码删除而丢失；
/// 代价是快照不会再更新——它本来也只用来证明**历史移植**，不参与现役行为判定。
#[test]
fn fax_bindings_are_anchored_to_go_source() {
    let src = repo_file("rs-gui/tests/golden/fax-bindings-go-reference.txt");

    // ccFaxWin：`_ = w.Bind("ccFaxWin", func(cmd string) { if h := w.Window(); h != nil { windowCmd(uintptr(h), cmd) } })`
    // 原文用 tab 缩进；逐字照抄（用显式转义拼，避免编辑器把 tab 换成空格）
    let anchor_win: String = [
        "\t_ = w.Bind(\"ccFaxWin\", func(cmd string) {",
        "\t\tif h := w.Window(); h != nil {",
        "\t\t\twindowCmd(uintptr(h), cmd)",
        "\t\t}",
        "\t})",
    ]
    .join("\n");
    assert!(
        src.contains(&anchor_win),
        "ccFaxWin 的 Go 实现形态变了（Rust 侧按 `windowCmd(hwnd, cmd)` 对齐）：\n{anchor_win}"
    );

    // ccFaxClose：`windowCmd(uintptr(h), "close")`（命令字固定 close）
    let anchor_close: String = [
        "\t_ = w.Bind(\"ccFaxClose\", func() {",
        "\t\tif h := w.Window(); h != nil {",
        "\t\t\twindowCmd(uintptr(h), \"close\")",
    ]
    .join("\n");
    assert!(
        src.contains(&anchor_close),
        "ccFaxClose 的 Go 实现形态变了（Rust 侧按 `windowCmd(hwnd, \"close\")` 对齐）：\n{anchor_close}"
    );

    // ccOpenFax 的两支（已在前台 / spawn）——Rust 侧逐条对齐
    assert!(
        src.contains("if h := findFaxPopup(); h != 0 {"),
        "ccOpenFax 的「已有浮窗」判据变了"
    );
    assert!(
        src.contains("exec.Command(exe, append([]string{\"--fax\"}, appdir.PortableArgs()...)...)"),
        "ccOpenFax 的 spawn argv 形态变了（`--fax` + 便携参数）"
    );
    assert!(
        src.contains("return jsonOK(\"注水专线浮窗已打开\")"),
        "ccOpenFax 的成功文案变了"
    );
    assert!(
        src.contains("return jsonOK(\"注水专线浮窗已在前台\")"),
        "ccOpenFax 的「已在前台」文案变了"
    );
}

/// `ccFaxWin` / `ccFaxClose` 的 Rust 真值表：注入宿主句柄驱动两条路径。
/// Go 的判据是 `w.Window() != nil`——Rust 侧对应 `Host::window_handle()`。
///
/// ⚠ 「有句柄时命令字原样透传」这一条**不在这里**用真窗口测：`windowCmd` 有副作用
/// （`move` 会进系统拖拽模态循环、`close` 会关窗），拿真窗口当夹具会动到用户桌面。
/// 它的判据是 [`fax_bindings_are_anchored_to_go_source`] 的源码锚 +
/// `win32::window_cmd` 自己的实现（`tests` 模块里有 `window_cmd_on_zero_handle_is_a_noop`）。
#[test]
fn cc_fax_win_and_close_follow_go_handle_semantics() {
    let launcher = FaxLauncher::production();

    // 拿不到句柄：什么都不做，返回 JSON null（Go 闭包无返回 → webview 侧 undefined）
    assert_eq!(launcher.fax_win(None, "min"), "null");
    assert_eq!(launcher.fax_win(None, "max"), "null");
    assert_eq!(launcher.fax_win(None, "move"), "null");
    assert_eq!(launcher.fax_close(None), "null");

    // 句柄为 0 等价于「拿不到」（Go `h != nil` 判据对 nil 接口为假）
    assert_eq!(launcher.fax_win(Some(0), "min"), "null");
    assert_eq!(launcher.fax_close(Some(0)), "null");
}

/* ---------------- 4. 单实例：决策边界与 Go 逐个同值 ---------------- */

/// `findFaxPopup` / `faxPopupMutexGuard` 的决策边界（黄金里采到的两个值）：
///   * 采集时本机**没有**浮窗 → `findFaxPopup()` 返回 0（`fax_popup_handle_before` = 0）；
///     造了个消息窗之后 `findFaxPopup()` 立刻能按标题找到它（`..._nonzero` = true）；
///     拆掉之后又回到 0（`find_fax_popup_after_destroy` = 0）。
///   * `faxPopupMutexGuard()` 的返回就是 Go 的 `bool`（`err != ERROR_ALREADY_EXISTS`）：
///     **首次调用**恒为 **true**（本次进程拿到名字 = "由我开浮窗"，
///     `fax_mutex_guard_with_popup` 采的就是这一次）；
///     **名字已被占有**时**第二次调用**为 **false**（`fax_mutex_guard_without_popup` 采的是
///     紧接着的那一次——同一个进程、同一个名字，`CreateMutexW` 报 `ERROR_ALREADY_EXISTS`）。
///     ⇒ 这一对黄金锁的正是「浮窗单实例」的判据：**第一次放行、第二次挡住**。
///
/// Rust 侧用手工 acquire 复现同一张真值表（不占用 `Local\ProxyDeck-FaxWindow` 这个
/// 真名字——那会与用户正在用的浮窗抢，故用同一前缀的测试名，语义完全一致）。
#[test]
fn single_instance_semantics_match_go_truth_table() {
    let g = golden();
    assert_eq!(
        g["fax_mutex_guard_with_popup"].as_bool(),
        Some(true),
        "首次调用必须放行（Go guard=true）"
    );
    assert_eq!(
        g["fax_mutex_guard_without_popup"].as_bool(),
        Some(false),
        "名字已被占有时必须挡住（Go guard=false）"
    );
    assert_eq!(
        g["fax_popup_handle_before"].as_i64(),
        Some(0),
        "采集时本机没有注水浮窗"
    );
    assert_eq!(
        g["find_fax_popup_with_message_window_nonzero"].as_bool(),
        Some(true),
        "按标题精确匹配必须能找到消息窗（findFaxPopup 的判据）"
    );
    assert_eq!(
        g["find_fax_popup_after_destroy"].as_i64(),
        Some(0),
        "消息窗 Destroy 之后 findFaxPopup 必须回到 0"
    );

    // Rust 侧同判：第一次 acquire = 唯一（Go guard=true）；第二次 = 已有实例（false）
    let name = format!("Local\\rs-gui-shelltest-guard-{}", std::process::id());
    let first = rs_gui::single::acquire(&name);
    assert!(first.is_some(), "第一次必须拿到（Go guard=true 同判）");
    assert!(
        rs_gui::single::acquire(&name).is_none(),
        "第二次必须报已有实例（Go guard=false 同判）"
    );

    // 名字常量与黄金一致（跨实现单实例的唯一判据）
    assert_eq!(
        rs_gui::single::FAX_WINDOW_MUTEX,
        g["fax_mutex_name"].as_str().unwrap()
    );
    // 浮窗标题与尺寸也与黄金一致（findFaxPopup 按标题精确匹配）
    assert_eq!(
        rs_gui::win32::FAX_TITLE,
        g["fax_window_title"].as_str().unwrap()
    );
    let size = g["fax_window_size"].as_array().unwrap();
    assert_eq!(
        (
            rs_gui::faxtool::FAX_WIDTH as i64,
            rs_gui::faxtool::FAX_HEIGHT as i64
        ),
        (size[0].as_i64().unwrap(), size[1].as_i64().unwrap())
    );
}

/* ---------------- 5. Migrate / UpgradePlaintext：真值表逐条对照 ---------------- */

/// `legacyStateFiles` 清单**逐条**与 Go 一致：条数 + 需加密的凭据文件集合。
///
/// 为什么单列一条：清单里多出/漏掉一条**不会**让 `Migrate` 的语义测试变红
/// （存量文件按名字查，清单多一条在临时目录里只是"文件不存在→跳过"），
/// 而漏一条 = 那个文件在存量用户机上永远搬不过来、且没有任何报错。
#[test]
fn legacy_state_file_list_matches_go() {
    // Go `internal/appdir/migrate.go` 的 `legacyStateFiles` 共 33 条，其中 credential=true 的 5 条
    assert_eq!(
        rs_gui::migrate::LEGACY_STATE_FILES.len(),
        33,
        "清单条数必须与 Go 的 legacyStateFiles 一致"
    );
    assert_eq!(
        rs_gui::migrate::legacy_credential_files(),
        vec![
            "api-key.txt",
            "tuanjie-accounts.json",
            "tuanjie-providers.json",
            "tuanjie-water-channels.json",
            "vibex-config.json",
        ],
        "需升级为 DPAPI 密文的凭据文件集合必须与 Go 一致（漏一条 = 明文账号长期留在盘上）"
    );
    // 清单里不得有重复项（Go 侧没有；重复会让 notes 里出现两遍）
    let mut names: Vec<&str> = rs_gui::migrate::LEGACY_STATE_FILES.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(names.len(), before, "清单不该有重复文件名");
}

/// `Migrate` 的四条语义与 Go 黄金**逐字段同值**：搬入（含 notes 文案与次序）、
/// 目标已存在（保留目标 + 旧文件 `*.migrated`）、同目录（什么都不做）、无历史文件（静默）。
#[test]
fn migrate_semantics_match_go_truth_table() {
    let g = golden();
    let m = &g["migrate"];

    // ---- 4a 主路径 ----
    {
        let src = temp_dir("mig-src");
        let dst = temp_dir("mig-dst");
        let content = r#"{"accounts":[{"user_id":"u1","access_token":"tok-abc","enabled":true}]}"#;
        std::fs::write(src.join("tuanjie-accounts.json"), content).unwrap();
        std::fs::write(src.join("keymap.json"), r#"[["cmd"]]"#).unwrap();
        std::fs::write(src.join("ProxyDeck.exe"), "exe").unwrap();

        let notes = rs_gui::migrate::migrate(&dst, &src);

        assert_eq!(
            notes.join("；"),
            m["moves_notes"].as_str().unwrap(),
            "Migrate 的 notes 文案与次序必须与 Go 逐字一致（清单顺序也是合同）"
        );
        assert_eq!(m["moves_src_gone"].as_bool(), Some(!src.join("tuanjie-accounts.json").exists()));
        assert_eq!(
            std::fs::read_to_string(dst.join("tuanjie-accounts.json")).unwrap(),
            m["moves_dst_content"].as_str().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("keymap.json")).unwrap(),
            m["moves_keymap"].as_str().unwrap()
        );
        assert!(src.join("ProxyDeck.exe").exists(), "程序本体不该被搬动");
        assert!(!dst.join("tuanjie-accounts.json.migrating").exists());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    // ---- 4b 目标已存在 ----
    {
        let src = temp_dir("mig2-src");
        let dst = temp_dir("mig2-dst");
        std::fs::write(src.join("stats.json"), r#"{"old":1}"#).unwrap();
        std::fs::write(dst.join("stats.json"), r#"{"new":2}"#).unwrap();
        let notes = rs_gui::migrate::migrate(&dst, &src);
        assert_eq!(notes.join("；"), m["existing_notes"].as_str().unwrap());
        assert_eq!(
            std::fs::read_to_string(dst.join("stats.json")).unwrap(),
            m["existing_dst_content"].as_str().unwrap(),
            "目标文件必须保持原样"
        );
        assert_eq!(
            m["existing_src_gone"].as_bool(),
            Some(!src.join("stats.json").exists())
        );
        assert_eq!(
            m["existing_migrated_exists"].as_bool(),
            Some(src.join("stats.json.migrated").exists())
        );
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    // ---- 4c 同目录 ----
    {
        let dir = temp_dir("mig3");
        std::fs::write(dir.join("keymap.json"), r#"[["tj"]]"#).unwrap();
        let notes = rs_gui::migrate::migrate(&dir, &dir);
        assert_eq!(
            notes.len() as i64,
            m["same_dir_notes_len"].as_i64().unwrap(),
            "同目录不该有动作"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("keymap.json")).unwrap(),
            m["same_dir_file_kept"].as_str().unwrap(),
            "同目录调用不得改动文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 4d 无历史文件 ----
    {
        let src = temp_dir("mig4-src");
        let dst = temp_dir("mig4-dst");
        let notes = rs_gui::migrate::migrate(&dst, &src);
        assert_eq!(notes.len() as i64, m["no_files_notes_len"].as_i64().unwrap());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}

/// `UpgradePlaintext` 的形态与 Go 黄金逐字段同值：密文前缀、能解回同一明文、
/// 密文里没有明文 token、非凭据文件不动、二次调用幂等。
#[test]
fn upgrade_plaintext_shape_matches_go_truth_table() {
    let g = golden();
    let up = &g["upgrade_plaintext"];
    let _guard = env_lock().lock().unwrap();
    let dir = temp_dir("upgrade");
    std::env::set_var(rs_gui::appdir::DATA_DIR_ENV, &dir);

    let plain = r#"{"accounts":[{"user_id":"u1","access_token":"tok-secret","enabled":true}]}"#;
    std::fs::write(dir.join("tuanjie-accounts.json"), plain).unwrap();
    std::fs::write(dir.join("stats.json"), r#"{"x":1}"#).unwrap();

    let notes = rs_gui::migrate::upgrade_plaintext();
    assert_eq!(notes.join("；"), up["notes"].as_str().unwrap());

    let raw = std::fs::read(dir.join("tuanjie-accounts.json")).unwrap();
    assert_eq!(
        rs_gui::dpapi::is_protected(&raw),
        up["is_ciphertext"].as_bool().unwrap(),
        "落盘必须是 PDAPI1 密文"
    );
    // 黄金里的 prefix 含一个换行（`PDAPI1\n`），比较前去掉结尾空白便于逐字对照
    assert_eq!(
        String::from_utf8_lossy(&raw[..13]).trim_end(),
        up["prefix"].as_str().unwrap().trim_end(),
        "密文前缀必须与 Go 逐字一致"
    );
    assert!(
        !String::from_utf8_lossy(&raw).contains("tok-secret"),
        "密文里不该出现明文 token"
    );
    assert_eq!(
        rs_gui::appdir::read_file(&dir.join("tuanjie-accounts.json")).unwrap(),
        plain.as_bytes().to_vec(),
        "加密后必须能读回同一内容"
    );
    assert_eq!(
        up["roundtrip_equal"].as_bool(),
        Some(true),
        "黄金里 Go 侧也必须能读回"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("stats.json")).unwrap(),
        up["noncredential_untouched"].as_str().unwrap(),
        "非凭据文件不该被加密"
    );

    let second = rs_gui::migrate::upgrade_plaintext();
    assert_eq!(second.join("；"), up["second_run_notes"].as_str().unwrap(), "二次调用幂等");

    std::env::remove_var(rs_gui::appdir::DATA_DIR_ENV);
    let _ = std::fs::remove_dir_all(&dir);
}

/* ---------------- 6. 版本常量与黄金同值 ---------------- */

#[test]
fn versions_match_go_golden() {
    let g = golden();
    // 黄金是 **Go 侧**采收的（`app_version` = 采收当时的 Go 值）；Rust 侧 v4.0.0 起
    // 即产品本体、版本号与 Go 有意分道，故不再要求两侧同值——只钉形态与 core 同值。
    assert_eq!(g["app_version"].as_str().unwrap(), "v3.20.10");
    assert!(
        rs_gui::APP_VERSION.starts_with('v') && rs_gui::APP_VERSION.len() > 2,
        "Rust 侧版本串形态不对：{:?}",
        rs_gui::APP_VERSION
    );
    assert_eq!(g["core_version"].as_str().unwrap(), rs_gui::CORE_VERSION);
}
