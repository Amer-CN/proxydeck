//! release_sync.rs —— 版本号「一处改、处处同步」门禁（2026-11-24 复盘新增）。
//!
//! 为什么有这条：版本号在仓库里手抄在 **6 处 ui.html 锚点 + 4 处 version.rc +
//! README 徽章 + CHANGELOG.md 顶条 + CLAUDE.md「当前状态」**，全靠人记。
//! 真实事故：
//!   * v3.10.0：漏改 ui.html 的 `UI_CUR`，应用自报旧版并误弹「发现新版本」框；
//!   * 2026-10-18：发版脚本全局替换版本串，把 12 条历史 CHANGELOG 全改成同一版号；
//!   * 2026-11-24 复盘当场：v4.10.2 已 tag/已构建，CLAUDE.md 的「当前状态」还写着
//!     v4.10.0 —— 交接文档说谎，下一个会话据此误判。
//!
//! 判据：**所有锚点必须等于 `rs_gui::APP_VERSION`**；任何一处找不到锚点也红
//! （锚点改名 ≠ 这条检查可以静默跳过）。
//!
//! 这里只查仓库内文件（确定性）。`tag` 与工作区状态属于发版动作，见
//! `tools/check-release-sync.ps1`（发版前跑）。

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rs-gui 应有上级目录")
        .to_path_buf()
}

/// 取前三个数字段：`v4.10.2` / `4,10,2,0` / `4.10.2 · 主题` 都归一到 `4.10.2`。
fn norm(raw: &str) -> String {
    let s = raw.trim().trim_start_matches('v');
    let mut out: Vec<String> = Vec::new();
    for part in s.split(|c: char| !c.is_ascii_digit()) {
        if !part.is_empty() {
            out.push(part.to_string());
        }
        if out.len() == 3 {
            break;
        }
    }
    while out.len() < 3 {
        out.push("0".to_string());
    }
    format!("{}.{}.{}", out[0], out[1], out[2])
}

fn slice<'a>(text: &'a str, from: &str, to: &str, what: &str) -> &'a str {
    let start = text
        .find(from)
        .unwrap_or_else(|| panic!("找不到锚点「{from}」（{what}）——界面/文档结构变了要重对齐，不许静默跳过"));
    let tail = &text[start + from.len()..];
    let end = tail
        .find(to)
        .unwrap_or_else(|| panic!("锚点「{from}」后面找不到结束标记「{to}」（{what}）"));
    &tail[..end]
}

#[test]
fn every_version_anchor_matches_app_version() {
    let want = norm(rs_gui::APP_VERSION);
    let root = repo_root();
    let mut checked: Vec<(&str, String)> = Vec::new();

    // ---- app/ui.html：用户能看见的 5 处 + 更新日志首条 ----
    let ui = std::fs::read_to_string(root.join("app/ui.html")).expect("读 app/ui.html");
    checked.push((
        "ui.html etchVer",
        norm(slice(&ui, "id=\"etchVer\">", "<", "顶栏蚀刻")),
    ));
    checked.push((
        "ui.html 铭牌 aria-label",
        norm(slice(
            &ui,
            "class=\"pt-nameplate\" aria-label=\"已安装版本 ",
            "\"",
            "铭牌无障碍标注",
        )),
    ));
    checked.push((
        "ui.html 铭牌正文",
        norm(slice(&ui, "class=\"pt-nameplate\" aria-label=\"已安装版本 ", "</div>", "铭牌正文")
            .split('>')
            .last()
            .unwrap_or("")),
    ));
    checked.push((
        "ui.html buildNo",
        norm(slice(&ui, "id=\"buildNo\">", "<", "构建号")),
    ));
    checked.push((
        "ui.html verChip",
        norm(slice(&ui, "id=\"verChip\">", "<", "版本胶囊")),
    ));
    checked.push((
        "ui.html UI_CUR",
        norm(slice(&ui, "var UI_CUR = '", "'", "更新检查比对值")),
    ));
    let log_start = ui
        .find("var CHANGELOG_DATA = [")
        .unwrap_or_else(|| panic!("找不到锚点「var CHANGELOG_DATA = [」（更新日志数组）"));
    checked.push((
        "ui.html CHANGELOG_DATA 首条",
        norm(slice(
            &ui[log_start..],
            "{ v: '",
            "'",
            "更新日志首条",
        )),
    ));

    // ---- rs-gui/version.rc：四个数字 + 两个字符串 ----
    // 注意：文件头注释里也出现过 `FILEVERSION 4,9,1,0` 的历史示例（就是这条门禁
    // 第一次跑时抓到的假锚点），所以必须锚在**行首两空格**的真实 VERSIONINFO 行上。
    let rc = std::fs::read_to_string(root.join("rs-gui/version.rc")).expect("读 version.rc");
    checked.push((
        "version.rc FILEVERSION",
        norm(slice(&rc, "\n  FILEVERSION", "\n", "文件版本")),
    ));
    checked.push((
        "version.rc PRODUCTVERSION",
        norm(slice(&rc, "\n  PRODUCTVERSION", "\n", "产品版本")),
    ));
    checked.push((
        "version.rc FileVersion",
        norm(slice(&rc, "VALUE \"FileVersion\"", "\n", "字符串版本")
            .split('"')
            .nth(1)
            .unwrap_or("")),
    ));
    checked.push((
        "version.rc ProductVersion",
        norm(slice(&rc, "VALUE \"ProductVersion\"", "\n", "产品字符串版本")
            .split('"')
            .nth(1)
            .unwrap_or("")),
    ));

    // ---- README 徽章 ----
    let readme = std::fs::read_to_string(root.join("README.md")).expect("读 README.md");
    checked.push((
        "README 版本徽章",
        norm(slice(&readme, "Version-v", "-", "shields.io 徽章")),
    ));

    // ---- CHANGELOG.md 顶条 ----
    let changelog = std::fs::read_to_string(root.join("CHANGELOG.md")).expect("读 CHANGELOG.md");
    let first_line = changelog.lines().next().unwrap_or("");
    assert!(
        first_line.starts_with("## v"),
        "CHANGELOG.md 第一行应是 `## vX.Y.Z · 主题`，实得：{first_line:?}"
    );
    checked.push((
        "CHANGELOG.md 顶条",
        norm(first_line.trim_start_matches("## ")),
    ));

    // ---- CLAUDE.md「当前状态」标题 ----
    // 必须锚在**行首**标题上：升版本清单那段也引用过「## 当前状态（日期，vX.Y.Z 已发布）」
    // 这个模板（门禁第一次跑就抓到了这个假锚点，解析出 0.0.0）。
    let claude = std::fs::read_to_string(root.join("CLAUDE.md")).expect("读 CLAUDE.md");
    let heading = slice(
        &claude,
        "\n## 当前状态（",
        "\n",
        "交接文档当前状态标题（行首）",
    );
    let after_comma = heading
        .split('，')
        .nth(1)
        .unwrap_or_else(|| panic!("CLAUDE.md 当前状态标题应是「（日期，vX.Y.Z 已发布）」，实得：{heading:?}"));
    let ver_part = after_comma.trim().trim_end_matches("已发布）").trim();
    checked.push(("CLAUDE.md 当前状态", norm(ver_part)));

    // ---- 统一断言 ----
    let bad: Vec<String> = checked
        .iter()
        .filter(|(_, got)| got != &want)
        .map(|(name, got)| format!("{name} = {got}"))
        .collect();
    assert!(
        bad.is_empty(),
        "版本号不同源（APP_VERSION = {want}）：\n  {}\n\
         发版时这些位置必须一起改：ui.html 6 处锚点、version.rc 4 处、README 徽章、\
         CHANGELOG.md 顶条、CLAUDE.md 当前状态。改动清单见 CLAUDE.md「升版本必须同步的位置」。",
        bad.join("\n  ")
    );
}
