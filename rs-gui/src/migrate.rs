//! migrate.rs —— exe 同目录历史状态文件搬入 + 存量明文凭据升级（对应 Go
//! `internal/appdir/migrate.go` 全部）。
//!
//! `main.go` 的 `runDataDirInit` 在任何状态文件读写之前调这两件事：
//!   1. [`migrate`]：exe 同目录的历史状态文件搬进数据目录（程序/数据目录分离 v3.20.2 之前
//!      的存量用户，状态都落在 exe 同目录）
//!   2. [`upgrade_plaintext`]：数据目录里仍是明文的凭据文件升级为 DPAPI 密文
//!      （读兼容的写侧收口）
//!
//! 两条硬语义（照抄 Go）：
//!   * **目标已存在同名文件 → 保留目标**（用户目录里的才是权威），旧文件改名 `*.migrated`
//!   * **搬走成功 → 原位置不留残渣**；改名失败 → 删除旧文件（数据已在目标目录，
//!     绝不因为删不掉而留下两份分叉状态）

use std::path::{Path, PathBuf};

use crate::appdir;

/// 一个历史状态文件（旧版全部落在 exe 同目录）。
/// `credential = true` 表示文件内含 token 一类的凭据，需 DPAPI 加密 + 收紧 ACL。
/// 清单本身以 `(名字, 是否凭据)` 的元组表承载（[`LEGACY_STATE_FILES`]），
/// 这里只做「不是目录项」这一点语义上的区分。
#[derive(Clone, Copy)]
struct StateFile {
    name: &'static str,
    #[allow(dead_code)] // 与 Go `stateFile.credential` 同字段；消费方是 legacy_credential_files
    credential: bool,
}

const fn sf(name: &'static str, credential: bool) -> StateFile {
    StateFile { name, credential }
}

/// 需要搬迁的历史状态文件清单（`credential=true` 的还会被加密升级）。
/// **只列状态/日志**：`plugins/` 目录、`ProxyDeck.exe.*` 残骸是程序本体，不搬。
///
/// 逐条对应 Go `legacyStateFiles`（顺序也一致，迁移日志的次序因此可比）。
pub const LEGACY_STATE_FILES: &[(&str, bool)] = &[
    // 主代理（app）
    ("api-key.txt", true),
    ("stats.json", false),
    ("keymap.json", false),
    ("close_mode.txt", false),
    ("notice_dismissed.flag", false),
    ("close_hint_dismissed.flag", false),
    ("headless-error.log", false),
    ("calibration.json", false),
    // 团结 8788
    ("tuanjie-accounts.json", true),
    ("tuanjie-providers.json", true),
    ("tuanjie-media.json", false),
    ("tuanjie-vision.json", false),
    ("tuanjie-water.json", false),
    ("tuanjie-water-channels.json", true),
    ("tuanjie-baselines.json", false),
    ("tuanjie-stats.json", false),
    ("tuanjie-pacing.json", false),
    // WorkBuddy CN 8787 / INTL 8789
    ("codebuddy-pool.json", false),
    ("codebuddy-pool-intl.json", false),
    ("codebuddy-stats.json", false),
    ("codebuddy-stats-intl.json", false),
    ("codebuddy-failover.json", false),
    ("codebuddy-failover-intl.json", false),
    // VibeX 8790
    ("vibex-config.json", true),
    ("vibex-stats.json", false),
    // B.AI 8891
    ("bai-stats.json", false),
    // 各插件启动失败诊断
    ("vibex-plugin-error.log", false),
    ("tuanjie-plugin-error.log", false),
    ("codebuddy-plugin-error.log", false),
    ("codebuddy-intl-plugin-error.log", false),
    ("bai-plugin-error.log", false),
    ("comate-plugin-error.log", false),
    ("qoder-plugin-error.log", false),
];

fn legacy_state_files() -> Vec<StateFile> {
    LEGACY_STATE_FILES
        .iter()
        .map(|(n, c)| sf(n, *c))
        .collect()
}

/// 需加密升级的历史凭据文件名（Go `legacyCredentialFiles`）。
pub fn legacy_credential_files() -> Vec<&'static str> {
    LEGACY_STATE_FILES
        .iter()
        .filter(|(_, c)| *c)
        .map(|(n, _)| *n)
        .collect()
}

/// 两个路径是否同一目录（Go `sameDir`：绝对化 + 清理 + **大小写不敏感**比较）。
fn same_dir(a: &Path, b: &Path) -> bool {
    let (Ok(ca), Ok(cb)) = (std::path::absolute(a), std::path::absolute(b)) else {
        return false;
    };
    // filepath.Clean 在父组件里的语义由 Path::components 归一（`.` 被吃掉）；
    // 大小写不敏感用 lowercase 比较（Windows 语义，与 Go EqualFold 同判）
    let norm = |p: &PathBuf| -> String {
        let mut parts: Vec<String> = Vec::new();
        for c in p.components() {
            match c {
                std::path::Component::CurDir => {}
                other => parts.push(other.as_os_str().to_string_lossy().to_string()),
            }
        }
        parts.join(std::path::MAIN_SEPARATOR_STR).to_lowercase()
    };
    norm(&ca) == norm(&cb)
}

/// 目标是否"已存在"——Go `migrate.go` 的 `fileExists` 语义：`os.Stat` 成功**且不是目录**
/// （`err == nil && !fi.IsDir()`）。
///
/// 注意 Go 同名函数有**两套不同语义**：`bridge.go` 那版是纯 `err == nil`（目录也算），
/// `migrate.go` 这版排除目录。此处对齐的是 migrate 版——目标位置是目录时必须走
/// "拷贝→改名失败→落位失败"路径，而不是"目标已存在"。
fn file_exists(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// 复制文件（Go `copyFile`：0600、写完 `Sync`）。
fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    let body = std::fs::read(src).map_err(|e| e.to_string())?;
    let mut f = std::fs::File::create(dst).map_err(|e| e.to_string())?;
    {
        use std::io::Write;
        f.write_all(&body).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 把 `src` 改名成 `*.migrated`；改名失败则直接删除（内容已在数据目录）。
/// 返回可在日志里打印的短句（与 Go `retire` 逐字同源）。
fn retire(src: &Path, why: &str) -> String {
    let mut dst_os = src.as_os_str().to_os_string();
    dst_os.push(".migrated");
    let dst = PathBuf::from(dst_os);
    if std::fs::rename(src, &dst).is_ok() {
        let base = dst
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        return format!("{why}为 {base}");
    }
    if std::fs::remove_file(src).is_ok() {
        return format!("{why}失败，已删除（内容已在数据目录）");
    }
    format!("{why}失败，原样留在 {}", src.display())
}

/// 把 `src_dir`（exe 同目录）里的历史状态文件一次性搬进 `dst_dir`。
///
/// 语义（Go `Migrate` 的报告第六节第 2 条）：
///   * 目标已存在同名文件 → 保留目标（用户目录里的才是权威），旧文件改名 `*.migrated`
///   * 搬走成功 → 原位置不留残渣；改名失败 → 删除旧文件
///   * 目录（如 `zulu-sessions`）：目标无同名目录时整目录改名过去，否则只做改名标记
///   * 返回可在日志里打印的短句（**不含文件内容**）
pub fn migrate(dst_dir: &Path, src_dir: &Path) -> Vec<String> {
    if dst_dir.as_os_str().is_empty()
        || src_dir.as_os_str().is_empty()
        || same_dir(dst_dir, src_dir)
    {
        return Vec::new();
    }
    match std::fs::metadata(src_dir) {
        Ok(md) if md.is_dir() => {}
        _ => return Vec::new(),
    }
    if std::fs::create_dir_all(dst_dir).is_err() {
        return vec![format!(
            "数据目录创建失败，迁移跳过：{}",
            dst_dir.display()
        )];
    }
    let mut notes = Vec::new();
    for f in legacy_state_files() {
        let src = src_dir.join(f.name);
        // Lstat 语义：符号链接本身也算存在
        let Ok(fi) = std::fs::symlink_metadata(&src) else {
            continue; // 不在旧位置：跳过（绝大多数文件都是这样）
        };
        let dst = dst_dir.join(f.name);
        if fi.is_dir() {
            if std::fs::metadata(&dst).is_ok() {
                let note = retire(&src, "目标已有同名目录，旧的只做改名");
                notes.push(format!("目录 {} 未搬迁：{note}", f.name));
                continue;
            }
            match std::fs::rename(&src, &dst) {
                Err(e) => notes.push(format!("目录 {} 搬迁失败：{e}", f.name)),
                Ok(()) => notes.push(format!("目录 {} 已搬入数据目录", f.name)),
            }
            continue;
        }
        if file_exists(&dst) {
            let note = retire(&src, "改名保留");
            notes.push(format!("{} 目标已存在，旧文件{note}", f.name));
            continue;
        }
        let mut tmp_os = dst.as_os_str().to_os_string();
        tmp_os.push(".migrating");
        let tmp = PathBuf::from(tmp_os);
        let _ = std::fs::remove_file(&tmp);
        if let Err(e) = copy_file(&src, &tmp) {
            notes.push(format!("{} 拷贝失败，保留在原处：{e}", f.name));
            continue;
        }
        if let Err(e) = std::fs::rename(&tmp, &dst) {
            let _ = std::fs::remove_file(&tmp);
            notes.push(format!("{} 落位失败，保留在原处：{e}", f.name));
            continue;
        }
        notes.push(format!("{} 已搬入数据目录", f.name));
        if std::fs::remove_file(&src).is_err() {
            // 删不掉（占用/权限）：改名让位，下次启动不再把它当权威旧文件
            let note = retire(&src, "改名让位");
            notes.push(format!("{} 旧文件删不掉，{note}", f.name));
        }
    }
    notes
}

/// 把数据目录里仍是明文的凭据文件升级成 DPAPI 密文（读兼容的写侧收口）。
/// 已是密文 / 不存在 / 读取失败一律跳过；**加密失败时原文件原样保留，不丢账号**。
pub fn upgrade_plaintext() -> Vec<String> {
    let mut notes = Vec::new();
    for name in legacy_credential_files() {
        let p = appdir::path(name);
        let Ok(b) = std::fs::read(&p) else {
            continue;
        };
        if b.is_empty() {
            continue;
        }
        if crate::dpapi::is_protected(&b) {
            continue;
        }
        if let Err(e) = appdir::write_file(&p, &b) {
            notes.push(format!("{name} 明文升级失败（保持原样）：{e}"));
            continue;
        }
        notes.push(format!("{name} 已升级为 DPAPI 密文"));
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rs-gui-migrate-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 清单必须与 Go `legacyStateFiles` 的**条数与逐条内容**一致（漏一条 = 那个文件
    /// 在存量用户机上永远搬不过来，且没有任何报错）。
    #[test]
    fn legacy_list_matches_go_line_by_line() {
        assert_eq!(LEGACY_STATE_FILES.len(), 33, "Go 清单 33 条");
        let creds = legacy_credential_files();
        assert_eq!(
            creds,
            vec![
                "api-key.txt",
                "tuanjie-accounts.json",
                "tuanjie-providers.json",
                "tuanjie-water-channels.json",
                "vibex-config.json"
            ],
            "需升级为密文的凭据文件（credential=true）共 5 条"
        );
    }

    /// 与 Go `TestMigrateMovesLegacyFiles` 同判：旧文件搬进数据目录、原位置不留残渣、
    /// 内容逐字节一致、不相干的程序本体不动、不留 `.migrating`。
    #[test]
    fn migrate_moves_legacy_files_like_go() {
        let src = tmp("src-moves");
        let dst = tmp("dst-moves");
        let content = r#"{"accounts":[{"user_id":"u1","access_token":"tok-abc","enabled":true}]}"#;
        std::fs::write(src.join("tuanjie-accounts.json"), content).unwrap();
        std::fs::write(src.join("keymap.json"), r#"[["cmd"]]"#).unwrap();
        std::fs::write(src.join("ProxyDeck.exe"), "exe").unwrap();

        let notes = migrate(&dst, &src);

        assert!(
            !src.join("tuanjie-accounts.json").exists(),
            "旧位置不该还有 tuanjie-accounts.json"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("tuanjie-accounts.json")).unwrap(),
            content,
            "搬运后内容必须逐字节一致"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("keymap.json")).unwrap(),
            r#"[["cmd"]]"#
        );
        assert!(src.join("ProxyDeck.exe").exists(), "程序本体不该被搬动");
        assert!(
            !dst.join("tuanjie-accounts.json.migrating").exists(),
            "不该留下 .migrating 临时文件"
        );
        assert!(
            notes.join("；").contains("已搬入数据目录"),
            "应回报搬迁结果，实得 {notes:?}"
        );
    }

    /// 与 Go `TestMigrateKeepsExistingTargetAndRetiresLegacy` 同判：目标已存在 →
    /// 目标原样保留，旧文件改名 `*.migrated`。
    #[test]
    fn migrate_keeps_existing_target_and_retires_legacy() {
        let src = tmp("src-keep");
        let dst = tmp("dst-keep");
        std::fs::write(src.join("stats.json"), r#"{"old":1}"#).unwrap();
        std::fs::write(dst.join("stats.json"), r#"{"new":2}"#).unwrap();

        migrate(&dst, &src);

        assert_eq!(
            std::fs::read_to_string(dst.join("stats.json")).unwrap(),
            r#"{"new":2}"#,
            "目标文件必须保持原样"
        );
        assert!(!src.join("stats.json").exists(), "旧文件不该原样留着");
        assert!(
            src.join("stats.json.migrated").exists(),
            "旧文件应改名为 stats.json.migrated"
        );
    }

    /// 目标位置是**目录**时的 Go 语义（真 Go 探针实测，见简报 P7b 审查补修）：
    /// Go `migrate.go` 的 `fileExists` 是 `err == nil && !fi.IsDir()`——目录不算"已存在"，
    /// 于是走拷贝→改名；改名（文件→已存在目录）在 Windows 必败 ⇒
    /// note 为 `"<name> 落位失败，保留在原处：<OS 原文>"`（OS 原文平台相关，只断前缀）、
    /// 源文件原样保留、目标目录不动、`*.migrating` 被清掉。
    /// 旧实现把目录也当"已存在"（走"目标已存在"分支改名旧文件），与 Go 相反。
    #[test]
    fn migrate_target_is_dir_falls_through_to_placement_failure_like_go() {
        let src = tmp("src-istargetdir");
        let dst = tmp("dst-istargetdir");
        std::fs::write(src.join("close_mode.txt"), "tray").unwrap();
        std::fs::create_dir(dst.join("close_mode.txt")).unwrap();

        let notes = migrate(&dst, &src);
        let hit = notes.iter().find(|n| n.starts_with("close_mode.txt 落位失败，保留在原处："));
        assert!(
            hit.is_some(),
            "目标是目录时应走落位失败（不是目标已存在），实得 {notes:?}"
        );
        assert!(
            !notes.iter().any(|n| n.contains("目标已存在")),
            "目录不能触发目标已存在分支，实得 {notes:?}"
        );
        assert_eq!(
            std::fs::read_to_string(src.join("close_mode.txt")).unwrap(),
            "tray",
            "源文件必须原样保留"
        );
        assert!(
            dst.join("close_mode.txt").is_dir(),
            "目标目录必须原样保留"
        );
        assert!(
            !dst.join("close_mode.txt.migrating").exists(),
            "不该留下 .migrating 临时文件"
        );
    }

    /// 与 Go `TestMigrateSkipsSameDir` / `TestMigrateNoLegacyFiles` 同判：
    /// 同目录什么都不做、无历史文件时静默返回。
    #[test]
    fn migrate_skips_same_dir_and_empty_src_like_go() {
        let dir = tmp("same");
        let p = dir.join("keymap.json");
        std::fs::write(&p, r#"[["tj"]]"#).unwrap();
        assert!(migrate(&dir, &dir).is_empty(), "同目录不该有动作");
        assert!(p.exists(), "同目录调用不得改动文件");
        // 大小写不同但同一目录也要认出来（Go sameDir 用 EqualFold）
        let upper = PathBuf::from(dir.to_string_lossy().to_uppercase());
        assert!(
            migrate(&upper, &dir).is_empty(),
            "大小写不同不算两个目录（Go EqualFold 语义）"
        );

        assert!(migrate(&tmp("dst-empty"), &tmp("src-empty")).is_empty());
        // 源目录不存在 → 静默
        assert!(migrate(&tmp("dst-nosrc"), &tmp("x").join("nope")).is_empty());
    }

    /// 目录分支：目标无同名目录 → 整目录改名过去；有则只做改名标记。
    #[test]
    fn migrate_handles_directory_entries() {
        let src = tmp("src-dir");
        let dst = tmp("dst-dir");
        // 清单里没有目录项，故用一个"名字恰好是目录"的条目来驱动该分支：
        // tuanjie-accounts.json 在旧位置是目录 → 走目录分支
        std::fs::create_dir_all(src.join("tuanjie-accounts.json").join("inner")).unwrap();
        std::fs::write(
            src.join("tuanjie-accounts.json").join("inner").join("x"),
            "y",
        )
        .unwrap();
        let notes = migrate(&dst, &src);
        assert!(
            notes.iter().any(|n| n.contains("目录 tuanjie-accounts.json 已搬入数据目录")),
            "整目录应搬到数据目录：{notes:?}"
        );
        assert!(dst.join("tuanjie-accounts.json").join("inner").join("x").exists());

        // 目标已有同名目录 → 旧的只做改名
        let src2 = tmp("src-dir2");
        let dst2 = tmp("dst-dir2");
        std::fs::create_dir_all(src2.join("stats.json")).unwrap();
        std::fs::create_dir_all(dst2.join("stats.json")).unwrap();
        let notes2 = migrate(&dst2, &src2);
        assert!(
            notes2
                .iter()
                .any(|n| n.contains("目录 stats.json 未搬迁：目标已有同名目录")),
            "实得 {notes2:?}"
        );
        assert!(src2.join("stats.json.migrated").exists(), "旧目录应改名保留");
    }

    /// 与 Go `TestUpgradePlaintextEncryptsCredentialFiles` 同判：明文凭据被换成 DPAPI 密文、
    /// 密文里不含明文 token、能读回同一内容、非凭据文件不动、二次调用幂等。
    #[test]
    fn upgrade_plaintext_encrypts_credentials_like_go() {
        let _g = crate::testutil::env_lock().lock().unwrap();
        let dir = tmp("upgrade");
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);

        let plain = r#"{"accounts":[{"user_id":"u1","access_token":"tok-secret","enabled":true}]}"#;
        std::fs::write(dir.join("tuanjie-accounts.json"), plain).unwrap();
        std::fs::write(dir.join("stats.json"), r#"{"x":1}"#).unwrap();

        let notes = upgrade_plaintext();
        assert!(
            notes.join("；").contains("已升级为 DPAPI 密文"),
            "应回报升级，实得 {notes:?}"
        );
        let raw = std::fs::read(dir.join("tuanjie-accounts.json")).unwrap();
        assert!(
            crate::dpapi::is_protected(&raw),
            "落盘内容应为 DPAPI 密文（PDAPI1 前缀）"
        );
        assert!(
            !String::from_utf8_lossy(&raw).contains("tok-secret"),
            "密文里不该出现明文 token"
        );
        assert_eq!(
            crate::appdir::read_file(&dir.join("tuanjie-accounts.json")).unwrap(),
            plain.as_bytes().to_vec(),
            "加密后应能读回同一内容"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("stats.json")).unwrap(),
            r#"{"x":1}"#,
            "非凭据文件不该被加密"
        );
        // 二次调用幂等（已是密文就跳过）
        let again = upgrade_plaintext();
        assert!(
            !again.join("；").contains("tuanjie-accounts.json"),
            "已是密文不该重复升级，实得 {again:?}"
        );

        std::env::remove_var(appdir::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空文件跳过（Go：`len(b) == 0` 直接 continue）。
    #[test]
    fn upgrade_plaintext_skips_empty_files() {
        let _g = crate::testutil::env_lock().lock().unwrap();
        let dir = tmp("upgrade-empty");
        std::env::set_var(appdir::DATA_DIR_ENV, &dir);
        std::fs::write(dir.join("api-key.txt"), b"").unwrap();
        let notes = upgrade_plaintext();
        assert!(notes.is_empty(), "空文件不该被升级，实得 {notes:?}");
        assert_eq!(std::fs::read(dir.join("api-key.txt")).unwrap().len(), 0);
        std::env::remove_var(appdir::DATA_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
