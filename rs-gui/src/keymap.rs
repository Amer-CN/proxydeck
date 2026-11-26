//! 键位映射（`keyMap.v1`）：纯校验/迁移、文件读取、持久化保存三个职责分开。
//!
//! 桥接层只做协议适配；本模块是 Rust 侧唯一写盘入口。落盘是明文 JSON
//! （同历史 `os.WriteFile`，不是 `appdir.WriteFile` 的 DPAPI 密文形态）。

use std::path::{Path, PathBuf};

use crate::state::App;

/// 脸名白名单（与 ui.html 的 `SUP_FACES` 同源；`md` = 码道，2026-10-26 加入）。
pub const FACES: [&str; 9] = ["cmd", "md", "tj", "comate", "cb", "qoder", "bai", "ext", "oc"];

/// 默认键位映射（与 ui.html 的 `KEYMAP_DEF` 同源思路：B.AI 默认不在任何键下，
/// OPENCODE 是 VIBEX 的副页）。UI 首跑用自身默认；文件缺席时本函数只用于回退。
pub fn default_map() -> Vec<Vec<String>> {
    vec![
        vec!["cmd".to_string()],
        vec!["tj".to_string(), "comate".to_string()],
        vec!["cb".to_string(), "qoder".to_string()],
        vec!["ext".to_string(), "oc".to_string()],
    ]
}

/// 纯校验：1-4 键、每键 ≥1 脸、脸名白名单、同一个脸只出现一次。
pub fn is_valid(map: &[Vec<String>]) -> bool {
    if map.is_empty() || map.len() > 4 {
        return false;
    }
    let mut seen: Vec<&str> = Vec::new();
    for key in map {
        if key.is_empty() {
            return false;
        }
        for face in key {
            if !FACES.contains(&face.as_str()) {
                return false;
            }
            if seen.contains(&face.as_str()) {
                return false;
            }
            seen.push(face.as_str());
        }
    }
    true
}

/// 一次性迁移：旧映射（合法、缺 `oc` 且有 `ext`）把 `oc` 补成 ext 所在键的副页。
/// 纯函数；找不到 ext 或已含 oc 时原样返回。
pub fn migrate_with_opencode(mut map: Vec<Vec<String>>) -> Vec<Vec<String>> {
    if map.iter().any(|key| key.iter().any(|face| face == "oc")) {
        return map;
    }
    if let Some(index) = map.iter().position(|key| key.iter().any(|face| face == "ext")) {
        map[index].push("oc".to_string());
    }
    map
}

/// 迁移标记：存在 = 已迁移过；之后用户手动删掉 oc 不再回填。
pub fn migration_marker() -> PathBuf {
    crate::appdir::path("keymap-oc-migrated.v1")
}

/// 从指定路径读取；返回 `(映射, 是否来自有效文件)`。
///
/// 合法且缺 oc、且迁移标记不存在时执行一次迁移并写回；写回失败不影响返回内存值
/// （用户仍能进入 OPENCODE 甲板），但下次启动会重试迁移。
pub fn load_from(path: &Path, marker: &Path) -> (Vec<Vec<String>>, bool) {
    let Ok(bytes) = std::fs::read(path) else {
        return (default_map(), false);
    };
    match serde_json::from_slice::<Vec<Vec<String>>>(&bytes) {
        Ok(map) if is_valid(&map) => {
            let has_oc = map.iter().any(|key| key.iter().any(|face| face == "oc"));
            if has_oc || crate::appdir::file_exists(marker) {
                return (map, true);
            }
            let migrated = migrate_with_opencode(map);
            if let Ok(raw) = serde_json::to_vec(&migrated) {
                let _ = std::fs::write(path, raw);
            }
            let _ = std::fs::write(marker, b"1");
            (migrated, true)
        }
        _ => (default_map(), false),
    }
}

/// 生产路径读取（数据目录 `keymap.json` + 迁移标记）。
pub fn load() -> (Vec<Vec<String>>, bool) {
    load_from(&App::key_map_file(), &migration_marker())
}

/// 校验后把原始 JSON 字节写到指定路径；失败如实返回错误（不吞）。
pub fn save_to(path: &Path, raw: &str) -> Result<(), String> {
    let map: Vec<Vec<String>> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if !is_valid(&map) {
        return Err("非法键位映射（1-4 键、每键 ≥1 页、页名须在白名单且一页一键）".into());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录 {} 失败: {e}", dir.display()))?;
    }
    std::fs::write(path, raw.as_bytes()).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

/// 生产路径保存。
pub fn save(raw: &str) -> Result<(), String> {
    save_to(&App::key_map_file(), raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rs-gui-keymap-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn validation_matches_contract() {
        assert!(is_valid(&default_map()));
        assert!(!is_valid(&[]));
        assert!(!is_valid(&vec![vec!["cmd".to_string()]; 5]));
        assert!(!is_valid(&vec![vec![]]));
        assert!(!is_valid(&vec![vec!["nope".to_string()]]));
        assert!(is_valid(&vec![vec!["md".to_string()]]));
        assert!(!is_valid(&vec![
            vec!["cmd".to_string()],
            vec!["cmd".to_string()]
        ]));
    }

    #[test]
    fn migration_fills_opencode_once_and_respects_later_removal() {
        let dir = temp_dir("oc");
        let file = dir.join("keymap.json");
        let marker = dir.join("keymap-oc-migrated.v1");
        let old = vec![
            vec!["cmd".to_string()],
            vec!["tj".to_string(), "comate".to_string()],
            vec!["cb".to_string(), "qoder".to_string()],
            vec!["ext".to_string()],
        ];
        std::fs::write(&file, serde_json::to_vec(&old).unwrap()).unwrap();

        let (migrated, saved) = load_from(&file, &marker);
        assert!(saved);
        assert_eq!(migrated[3], vec!["ext".to_string(), "oc".to_string()]);
        assert!(marker.exists(), "迁移应写入一次性标记");
        let on_disk: Vec<Vec<String>> =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(on_disk, migrated, "迁移结果应写回文件");

        // 用户随后主动移除 oc 并保存：标记已存在，不得回填。
        let removed = vec![
            vec!["cmd".to_string()],
            vec!["tj".to_string(), "comate".to_string()],
            vec!["cb".to_string(), "qoder".to_string()],
            vec!["ext".to_string()],
        ];
        save_to(&file, &serde_json::to_string(&removed).unwrap()).unwrap();
        let (again, saved) = load_from(&file, &marker);
        assert!(saved);
        assert_eq!(again[3], vec!["ext".to_string()], "用户显式移除不得被回填");
    }

    #[test]
    fn file_priority_and_invalid_file_falls_back() {
        let dir = temp_dir("file");
        let file = dir.join("keymap.json");
        let marker = dir.join("keymap-oc-migrated.v1");
        let (missing, saved) = load_from(&file, &marker);
        assert!(!saved, "文件缺席 ≠ 已保存");
        assert_eq!(missing, default_map());

        std::fs::write(&file, b"[ [\"nope\"] ]").unwrap();
        let (invalid, saved) = load_from(&file, &marker);
        assert!(!saved, "非法文件必须回退默认且报告未保存");
        assert_eq!(invalid, default_map());

        std::fs::write(&file, b"[ [\"md\"], [\"ext\", \"oc\"] ]").unwrap();
        let (valid, saved) = load_from(&file, &marker);
        assert!(saved);
        assert_eq!(valid[0], vec!["md".to_string()], "文件值优先于默认");
    }

    #[test]
    fn save_failure_is_reported_not_swallowed() {
        let dir = temp_dir("save-fail");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let err = save_to(&blocker.join("keymap.json"), r#"[["cmd"]]"#).unwrap_err();
        assert!(err.contains("创建目录") || err.contains("写入"), "{err}");
        assert!(save_to(&dir.join("keymap.json"), r#"{"bad":true}"#).is_err());
        assert!(save_to(&dir.join("keymap.json"), r#"[["nope"]]"#).is_err());
    }
}
