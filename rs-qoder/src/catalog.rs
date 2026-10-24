//! Qoder 的**动态模型目录**：客户端自带的 `resources\dynamic-text\qoder-cn.json`（CN）
//! / `qoder.json`（INTL）。
//!
//! 为什么不再写死：旧的 `QODER_MODELS` 是 2026-08-30 照界面截图手抄的 13 条，上游早已漂移
//! （2026-10-20 实测目录：新增 `cmodel`/`kmodel_latest`，`q37fmodel`/`gm51model`
//! 已从目录里消失）。客户端自己带一份权威目录，读它即可，不用再靠人眼同步。
//!
//! 文件格式（两区**逐字节相同**，实测 SHA-256 `1BB3AA2C…`）：
//!
//! ```json
//! {"en": {"model.auto.label": "Auto", "model.lite.detail": "..."},
//!  "zh": {"model.auto.label": "Auto", ...}}
//! ```
//!
//! 只取 `model.<key>.label`；`model.efficient` / `model.performance` / `model.ultimate`
//! 是**质量档位**（不是模型，`detail` 写的是 "Standard reasoning at low cost" 这类档位
//! 描述），必须排除；lite（轻量）同属这套四档阶梯——实证：传 lite 上游回
//! ModelNotFoundError: Model "lite" is not available in the current QoderCN catalog（404）。
//!
//! 两条硬性要求：
//! * **顺序稳定**：按 worker key 的字节序（`BTreeMap`）——换机器、换语言都不变；
//! * **读不到就回落**：文件缺失、不是 UTF-8、JSON 坏、解析空 —— 一律用内置静态表
//!   （[`STATIC_MODELS`]，与 `lib.rs` 的 `QODER_MODELS` 是同一份数据），`/v1/models` 永不 500。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Region;

// ---------- 服务端权威清单（live） ----------

/// 服务端下发的模型条目（`pd-init` 控制响应里的 `response.models[]`）。
///
/// **这才是权威清单**：客户端本地 `dynamic-text/*.json` 只是 key→显示名的字典，
/// 上游增删改它不会跟着变（2026-10-24 实测：本地字典还是 10-15 的，服务端已经
/// 把 DeepSeek-V4-Flash 改名 DeepSeek-Flash、Kimi-K3 改名 Kimi-K3.1-Pro，
/// 并下线了 Cantus / Qwen3.7-Plus / Qwen3.7-Max）。
///
/// 只保留对用户有意义的字段；原始条目还有 `serverModel` / `thinking_config` 等
/// 我们暂时用不到的嵌套结构。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveModel {
    /// 传给 `--model` 的真 id（如 `dfmodel`）
    pub value: String,
    /// 显示名（如 `DeepSeek-Flash`）——`/v1/models` 的 `id` 用它
    pub display_name: String,
    pub model_id: String,
    pub description: String,
    pub price_factor: f64,
    pub original_price_factor: f64,
    pub max_input_tokens: i64,
    pub max_output_tokens: i64,
    pub is_vl: bool,
    pub is_reasoning: bool,
    pub is_free: bool,
    pub is_new: bool,
    pub is_default: bool,
    pub default_effort: String,
    pub efforts: Vec<String>,
    pub context_windows: Vec<i64>,
    pub default_context_window: i64,
    pub tags: Vec<String>,
}

impl LiveModel {
    /// 解析一条；`value` 与 `displayName` 缺任何一个就丢弃（没它们没法用）。
    fn parse(v: &serde_json::Value) -> Option<LiveModel> {
        let s = |k: &str| -> String {
            v.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        let value = s("value");
        let display_name = s("displayName");
        if value.trim().is_empty() || display_name.trim().is_empty() {
            return None;
        }
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let i = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
        let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        let strs = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let ints = |k: &str| -> Vec<i64> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default()
        };
        Some(LiveModel {
            model_id: {
                let m = s("modelId");
                if m.is_empty() {
                    value.clone()
                } else {
                    m
                }
            },
            value,
            display_name,
            description: s("description"),
            price_factor: f("priceFactor"),
            original_price_factor: f("originalPriceFactor"),
            max_input_tokens: i("maxInputTokens"),
            max_output_tokens: i("maxOutputTokens"),
            is_vl: b("isVl"),
            is_reasoning: b("isReasoning"),
            is_free: b("isFree"),
            is_new: b("isNew"),
            is_default: b("isDefault"),
            default_effort: s("defaultEffort"),
            efforts: strs("efforts"),
            context_windows: ints("availableContextWindows"),
            default_context_window: i("defaultContextWindow"),
            tags: strs("tags"),
        })
    }
}

/// 解析 `models` 数组。非数组 / 空 / 全条目不可用 ⇒ 空（调用方回落）。
pub fn parse_live(models: &serde_json::Value) -> Vec<LiveModel> {
    let Some(arr) = models.as_array() else {
        return Vec::new();
    };
    arr.iter().filter_map(LiveModel::parse).collect()
}

/// 质量档位（不是模型）：`model.<key>.label` 里的这三条不进模型目录。
pub const QUALITY_TIERS: [&str; 4] = ["lite", "efficient", "performance", "ultimate"];

/// 内置静态表（worker key, 可读名）：动态目录读不到时的兜底。
///
/// 顺序 = worker key 的字节序（与 [`parse`] 的输出顺序同口径，切换来源时
/// `/v1/models` 的条目顺序不变）。
/// 动态目录的中文标签是「轻量」——两者都经 `resolve_model` 可解析。
pub const STATIC_MODELS: [(&str, &str); 13] = [
    ("auto", "Auto"),
    ("cmodel", "Cantus"),
    ("dfmodel", "DeepSeek-V4-Flash"),
    ("dmodel", "DeepSeek-V4-Pro"),
    ("gfmodel", "GLM-5.3-Flash"),
    ("gmodel", "GLM-5.3"),
    ("kmodel", "Kimi-K2.7-Code"),
    ("kmodel_latest", "Kimi-K3"),
    ("mmodel", "MiniMax-M3"),
    ("qfmodel", "Qwen3.8-Flash"),
    ("qmodel", "Qwen3.7-Plus"),
    ("qmodel_38max", "Qwen3.8-Max"),
    ("qmodel_latest", "Qwen3.7-Max"),
];

/// 强制静态表的环境变量（`=1` 生效）：给黄金门禁/可复现对照用。
pub const STATIC_ENV: &str = "QODER_STATIC_MODELS";

/// CN 版安装目录名（`<Program Files>\Qoder CN\`）。
pub const INSTALL_DIR_CN: &str = "Qoder CN";

/// INTL 版安装目录名（`<Program Files>\Qoder\`）。
pub const INSTALL_DIR_INTL: &str = "Qoder";

/// CN 版**旧目录布局**（`<Program Files>\Qoder\Qoder CN\`）——仅作兜底候选，
/// 与 `worker::WORKER_REL_CN_LEGACY` 同口径。
pub const INSTALL_DIR_CN_LEGACY: &str = r"Qoder\Qoder CN";

/// 目录文件相对安装根的路径（不含文件名）。
pub const CATALOG_REL: &str = r"resources\dynamic-text";

/// 目录文件名（按区域）。
pub const CATALOG_FILE_CN: &str = "qoder-cn.json";
pub const CATALOG_FILE_INTL: &str = "qoder.json";

/// 按区域取目录文件名。
pub fn file_name(region: Region) -> &'static str {
    match region {
        Region::Cn => CATALOG_FILE_CN,
        Region::Intl => CATALOG_FILE_INTL,
    }
}

/// 按区域取安装目录名（`<Program Files>\<这个名字>`）。
pub fn install_dir(region: Region) -> &'static str {
    match region {
        Region::Cn => INSTALL_DIR_CN,
        Region::Intl => INSTALL_DIR_INTL,
    }
}

/// `<安装根>\resources\dynamic-text\<区域文件>`。
pub fn catalog_path(root: &Path, region: Region) -> PathBuf {
    root.join(CATALOG_REL).join(file_name(region))
}

/// 安装根候选（与 `worker::find_worker` 同一套 Program Files 推断思路）：
/// `D:\Program Files\<安装目录>` → `%ProgramFiles%\<安装目录>` → CN 旧布局兜底。
pub fn root_candidates(program_files: Option<&str>, region: Region) -> Vec<PathBuf> {
    let rel = install_dir(region);
    let mut candidates = vec![Path::new(r"D:\Program Files").join(rel)];
    if let Some(pf) = program_files {
        if !pf.is_empty() {
            candidates.push(Path::new(pf).join(rel));
        }
    }
    if region == Region::Cn {
        candidates.push(Path::new(r"D:\Program Files").join(INSTALL_DIR_CN_LEGACY));
    }
    candidates
}

/// 第一个**目录文件真的存在**的安装根（找不到回 `None` ⇒ 调用方回落静态表）。
pub fn find_root(program_files: Option<&str>, region: Region) -> Option<PathBuf> {
    root_candidates(program_files, region)
        .into_iter()
        .find(|root| catalog_path(root, region).is_file())
}

/// 解析目录文本 → `(worker_key, label)` 列表，**按 key 字节序**、不含质量档位。
///
/// 语言：先取 `en` 打底、再用 `zh` 覆盖（`zh` 没有的 key 仍用 `en`）；两段都缺 ⇒ 空。
/// 纯函数：不碰文件系统、不读环境变量——单测可直接喂真实文件的文本片段。
pub fn parse(text: &str) -> Vec<(String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    // en 打底，zh 只补缺（不覆盖已有 key）——保证 id 恒为 ASCII，不会漏进「轻量」
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for lang in ["en", "zh"] {
        let Some(map) = v.get(lang).and_then(|x| x.as_object()) else {
            continue;
        };
        for (name, value) in map {
            let Some(key) = model_key(name) else { continue };
            if QUALITY_TIERS.contains(&key) {
                continue;
            }
            let Some(label) = value.as_str() else { continue };
            if label.trim().is_empty() {
                continue;
            }
            labels
                .entry(key.to_string())
                .or_insert_with(|| label.to_string());
        }
    }
    labels.into_iter().collect()
}

/// `model.<key>.label` → `<key>`；其它键（`.detail` / `.description` / 非模型键）回 `None`。
fn model_key(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("model.")?;
    let key = rest.strip_suffix(".label")?;
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// 内置静态表的拥有型副本（调用方拿 `Vec` 直接当目录用）。
pub fn static_models() -> Vec<(String, String)> {
    STATIC_MODELS
        .iter()
        .map(|(key, label)| (key.to_string(), label.to_string()))
        .collect()
}

/// 读目录 + 解析；返回 `(模型列表, 实际使用的文件)`，文件用不上时第二项是 `None`
/// （= 走的内置静态表）。
pub fn load_detailed(root: Option<&Path>, region: Region) -> (Vec<(String, String)>, Option<PathBuf>) {
    if let Some(root) = root {
        let path = catalog_path(root, region);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let parsed = parse(&text);
            if !parsed.is_empty() {
                return (parsed, Some(path));
            }
        }
    }
    (static_models(), None)
}

/// 生产口径：`root` 由调用方给（`lib.rs` 传真实安装根，单测传临时目录）；
/// 读不到 / 解析空 ⇒ 内置静态表。
pub fn load(root: Option<&Path>, region: Region) -> Vec<(String, String)> {
    load_detailed(root, region).0
}

/// `QODER_STATIC_MODELS=1` 是否生效（忽略首尾空白；其它值一律不算）。
pub fn static_forced() -> bool {
    std::env::var(STATIC_ENV)
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// 生产入口：环境变量强制 ⇒ 静态表；否则按 `%ProgramFiles%` 找安装根读动态目录
/// （找不到时 `load_detailed` 自己回落静态表）。
pub fn load_for_env(region: Region) -> (Vec<(String, String)>, Option<PathBuf>) {
    if static_forced() {
        return (static_models(), None);
    }
    let root = find_root(std::env::var("ProgramFiles").ok().as_deref(), region);
    load_detailed(root.as_deref(), region)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实目录文件的文本片段（`D:\Program Files\Qoder CN\resources\dynamic-text\qoder-cn.json`
    /// 的 `model.*.label` 全量 + 一条 `.detail`）。两区文件逐字节相同，故这份夹具对 CN/INTL
    /// 都成立。**故意保留** `efficient`/`performance`/`ultimate` 与 `lite`：前者要被排除、
    /// 后者要被保留，是本次最容易写反的两处。
    const REAL_CATALOG_FIXTURE: &str = r#"{
      "en": {
        "model.auto.label": "Auto",
        "model.lite.label": "Lite",
        "model.lite.detail": "Basic reasoning available on Free (may be slower during peak hours, spec generation is not supported.)",
        "model.cmodel.label": "Cantus",
        "model.dmodel.label": "DeepSeek-V4-Pro",
        "model.gmodel.label": "GLM-5.3",
        "model.kmodel.label": "Kimi-K2.7-Code",
        "model.mmodel.label": "MiniMax-M3",
        "model.qmodel.label": "Qwen3.7-Plus",
        "model.dfmodel.label": "DeepSeek-V4-Flash",
        "model.gfmodel.label": "GLM-5.3-Flash",
        "model.qfmodel.label": "Qwen3.8-Flash",
        "model.ultimate.label": "Ultimate",
        "model.efficient.label": "Efficient",
        "model.performance.label": "Performance",
        "model.qmodel_38max.label": "Qwen3.8-Max",
        "model.kmodel_latest.label": "Kimi-K3",
        "model.qmodel_latest.label": "Qwen3.7-Max"
      },
      "zh": {
        "model.auto.label": "Auto",
        "model.lite.label": "轻量",
        "model.cmodel.label": "Cantus",
        "model.dmodel.label": "DeepSeek-V4-Pro",
        "model.gmodel.label": "GLM-5.3",
        "model.kmodel.label": "Kimi-K2.7-Code",
        "model.mmodel.label": "MiniMax-M3",
        "model.qmodel.label": "Qwen3.7-Plus",
        "model.dfmodel.label": "DeepSeek-V4-Flash",
        "model.gfmodel.label": "GLM-5.3-Flash",
        "model.qfmodel.label": "Qwen3.8-Flash",
        "model.ultimate.label": "极致",
        "model.efficient.label": "经济",
        "model.performance.label": "性能",
        "model.qmodel_38max.label": "Qwen3.8-Max",
        "model.kmodel_latest.label": "Kimi-K3",
        "model.qmodel_latest.label": "Qwen3.7-Max"
      }
    }"#;

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "rs-qoder-catalog-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 验收标准 2：真实目录文本 → 13 条，含 `cmodel`/`kmodel_latest`，
    /// 不含三个质量档位；顺序按 key 字节序。
    #[test]
    fn parses_real_catalog_fixture() {
        let got = parse(REAL_CATALOG_FIXTURE);
        let keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "auto",
                "cmodel",
                "dfmodel",
                "dmodel",
                "gfmodel",
                "gmodel",
                "kmodel",
                "kmodel_latest",

                "mmodel",
                "qfmodel",
                "qmodel",
                "qmodel_38max",
                "qmodel_latest",
            ],
            "13 条且按 worker key 字节序"
        );
        for tier in QUALITY_TIERS {
            assert!(!keys.contains(&tier), "{tier} 是质量档位，不是模型");
        }
        assert_eq!(got.len(), 13);
        // 语言优先 zh：lite 的 zh 标签与 en 不同（轻量 vs Lite）

        // 新增的模型都在列
        assert_eq!(got[1], ("cmodel".to_string(), "Cantus".to_string()));
        assert_eq!(
            got[7],
            ("kmodel_latest".to_string(), "Kimi-K3".to_string())
        );
        assert_eq!(got[0], ("auto".to_string(), "Auto".to_string()));
    }

    /// 语言规则：`en` 打底、`zh` **只补 en 缺失的 key**（不覆盖）；没有语言对象 ⇒ 空。
    ///
    /// 为什么不 zh 优先：真实模型两语言 label 完全相同，唯一有差异的是四个质量档位；
    /// 用 en 打底保证 `/v1/models` 的 id 恒为 ASCII（2026-10-20 曾把「轻量」漏进 id）。
    #[test]
    fn prefers_en_and_fills_from_zh() {
        let got = parse(
            r#"{"en":{"model.auto.label":"Auto","model.cmodel.label":"Cantus"},
                "zh":{"model.gmodel.label":"GLM-5.3","model.qmodel.label":"Qwen3.7-Plus"}}"#,
        );
        assert_eq!(
            got,
            vec![
                ("auto".to_string(), "Auto".to_string()),
                ("cmodel".to_string(), "Cantus".to_string()),
                ("gmodel".to_string(), "GLM-5.3".to_string()),
                ("qmodel".to_string(), "Qwen3.7-Plus".to_string()),
            ],
            "en 优先，zh 只补 en 缺失的 key"
        );

        // en 有、zh 也有 ⇒ 用 en（不覆盖）
        let got = parse(
            r#"{"en":{"model.gmodel.label":"GLM-5.3"},
                "zh":{"model.gmodel.label":"智谱-5.3"}}"#,
        );
        assert_eq!(got, vec![("gmodel".to_string(), "GLM-5.3".to_string())]);

        // 只有 en
        let got = parse(r#"{"en":{"model.gmodel.label":"GLM-5.3"}}"#);
        assert_eq!(got, vec![("gmodel".to_string(), "GLM-5.3".to_string())]);
        // 非对象 / 空 / 坏 JSON ⇒ 空（调用方回落静态表）
        assert!(parse("").is_empty());
        assert!(parse("[]").is_empty());
        assert!(parse("{not json").is_empty());
        assert!(parse(r#"{"en":null}"#).is_empty());
        assert!(parse(r#"{"en":{"model.performance.label":"Performance"}}"#).is_empty());
    }

    /// 内置静态表就是简报 C 的 14 条（key 字节序），且 `catalog` 与 `lib` 同源。
    #[test]
    fn static_table_is_the_13_models() {
        assert_eq!(
            STATIC_MODELS.to_vec(),
            vec![
                ("auto", "Auto"),
                ("cmodel", "Cantus"),
                ("dfmodel", "DeepSeek-V4-Flash"),
                ("dmodel", "DeepSeek-V4-Pro"),
                ("gfmodel", "GLM-5.3-Flash"),
                ("gmodel", "GLM-5.3"),
                ("kmodel", "Kimi-K2.7-Code"),
                ("kmodel_latest", "Kimi-K3"),
                ("mmodel", "MiniMax-M3"),
                ("qfmodel", "Qwen3.8-Flash"),
                ("qmodel", "Qwen3.7-Plus"),
                ("qmodel_38max", "Qwen3.8-Max"),
                ("qmodel_latest", "Qwen3.7-Max"),
            ]
        );
        // 字典序（字节序）真的成立，不能被手滑打乱
        let keys: Vec<&str> = STATIC_MODELS.iter().map(|(k, _)| *k).collect();
        let mut sorted = keys.clone();
        sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        assert_eq!(keys, sorted, "静态表必须按 worker key 字节序");
        assert_eq!(static_models().len(), 13);
        // 三个质量档位一个都不许在表里
        for tier in QUALITY_TIERS {
            assert!(
                !keys.contains(&tier),
                "{tier} 是质量档位，不许进静态表"
            );
        }
    }

    /// 验收标准 3：文件缺失 / JSON 坏 / 解析空 ⇒ 回落静态表。
    #[test]
    fn falls_back_when_missing() {
        for region in [Region::Cn, Region::Intl] {
            assert_eq!(load(None, region), static_models(), "root=None ⇒ 静态表");
        }
        let empty_root = temp_root("empty");
        assert_eq!(load(Some(&empty_root), Region::Cn), static_models());

        let bad_root = temp_root("bad");
        let bad_dir = bad_root.join(CATALOG_REL);
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join(CATALOG_FILE_CN), "{not json").unwrap();
        assert_eq!(load(Some(&bad_root), Region::Cn), static_models(), "坏 JSON ⇒ 静态表");

        // 只有质量档位（解析结果为空）也回落
        std::fs::write(
            bad_dir.join(CATALOG_FILE_CN),
            r#"{"en":{"model.efficient.label":"Efficient"}}"#,
        )
        .unwrap();
        assert_eq!(load(Some(&bad_root), Region::Cn), static_models(), "解析空 ⇒ 静态表");

        let _ = std::fs::remove_dir_all(&empty_root);
        let _ = std::fs::remove_dir_all(&bad_root);
    }

    /// 区域各读各的文件（CN `qoder-cn.json` / INTL `qoder.json`）。
    #[test]
    fn region_picks_its_own_file() {
        let root = temp_root("region");
        let dir = root.join(CATALOG_REL);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(CATALOG_FILE_CN),
            r#"{"zh":{"model.auto.label":"CN-ONLY"}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(CATALOG_FILE_INTL),
            r#"{"zh":{"model.auto.label":"INTL-ONLY"}}"#,
        )
        .unwrap();
        assert_eq!(
            load(Some(&root), Region::Cn),
            vec![("auto".to_string(), "CN-ONLY".to_string())]
        );
        assert_eq!(
            load(Some(&root), Region::Intl),
            vec![("auto".to_string(), "INTL-ONLY".to_string())]
        );
        // 只有 CN 文件的根：INTL 读不到 ⇒ 静态表
        std::fs::remove_file(dir.join(CATALOG_FILE_INTL)).unwrap();
        assert_eq!(load(Some(&root), Region::Intl), static_models());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 安装根候选与 `find_root`：只认「目录文件真的存在」的那一条。
    #[test]
    fn find_root_requires_the_catalog_file() {
        let pf = temp_root("programfiles");
        // 候选形状：CN 3 条（含旧布局兜底）、INTL 2 条
        assert_eq!(root_candidates(Some(pf.to_str().unwrap()), Region::Cn).len(), 3);
        assert_eq!(root_candidates(Some(pf.to_str().unwrap()), Region::Intl).len(), 2);
        assert_eq!(
            root_candidates(Some(pf.to_str().unwrap()), Region::Intl)[0],
            Path::new(r"D:\Program Files").join(INSTALL_DIR_INTL)
        );
        assert_eq!(
            root_candidates(Some(pf.to_str().unwrap()), Region::Cn)[2],
            Path::new(r"D:\Program Files").join(INSTALL_DIR_CN_LEGACY)
        );
        // 临时 ProgramFiles 下铺一份 CN 目录文件
        let root = pf.join(INSTALL_DIR_CN);
        let dir = catalog_path(&root, Region::Cn);
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&dir, REAL_CATALOG_FIXTURE).unwrap();
        // `D:\Program Files\Qoder CN` 在装了 Qoder 的机器上会先命中——同样合法；
        // 这里只锁「命中的那个根一定是候选之一、且目录文件真的存在」。
        let hit = find_root(Some(pf.to_str().unwrap()), Region::Cn).expect("CN 目录文件应能定位到");
        assert!(
            root_candidates(Some(pf.to_str().unwrap()), Region::Cn).contains(&hit),
            "find_root 命中的根不在候选里: {}",
            hit.display()
        );
        let hit_file = catalog_path(&hit, Region::Cn);
        assert!(hit_file.is_file(), "命中的根下没有目录文件: {}", hit_file.display());
        assert_eq!(parse(&std::fs::read_to_string(hit_file).unwrap()).len(), 13);
        let _ = std::fs::remove_dir_all(&pf);
    }

    /// 本机真装了 Qoder 时，真实目录文件也要解析出 14 条（换机没装就 SKIP 并计数）。
    #[test]
    fn parses_installed_catalog_when_present() {
        let root = find_root(None, Region::Cn);
        let Some(root) = root else {
            println!("SKIP(no-qoder-install): 本机没装 Qoder CN，真实目录文件不可得——未跑");
            return;
        };
        let path = catalog_path(&root, Region::Cn);
        let text = std::fs::read_to_string(&path).unwrap();
        let got = parse(&text);
        assert_eq!(got.len(), 13, "真实目录 {} 应解析出 14 个模型", path.display());
        let keys: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        for want in ["cmodel", "kmodel_latest"] {
            assert!(keys.contains(&want), "真实目录缺 {want}");
        }
        for tier in QUALITY_TIERS {
            assert!(!keys.contains(&tier), "真实目录里的 {tier} 是质量档位");
        }
    }

    // ---------- 服务端权威清单（live） ----------

    /// 实测原样（2026-10-24 从 `pd-init` 控制响应导出，只截掉用不到的嵌套）。
    const LIVE_FIXTURE: &str = r#"[
      {"value":"auto","displayName":"Auto","description":"","modelId":"auto","source":"system",
       "isDefault":true,"isEnabled":true,"isReasoning":true,"isVl":true,"priceFactor":0.5,
       "maxInputTokens":200000,"maxOutputTokens":32000,"availableContextWindows":[200000],
       "defaultContextWindow":200000,"tags":[]},
      {"value":"dfmodel","displayName":"DeepSeek-Flash","description":"","modelId":"dfmodel",
       "isVl":true,"isFree":true,"isNew":true,"priceFactor":0,"originalPriceFactor":0.1,
       "maxInputTokens":180000,"maxOutputTokens":32000,"efforts":["high","max","low"],
       "defaultEffort":"high","availableContextWindows":[200000,400000,1000000],
       "defaultContextWindow":200000,"tags":["free"]},
      {"value":"kmodel_latest","displayName":"Kimi-K3.1-Pro","modelId":"kmodel_latest",
       "priceFactor":1,"maxInputTokens":180000,"maxOutputTokens":32000}
    ]"#;

    #[test]
    fn parses_live_catalog() {
        let v: serde_json::Value = serde_json::from_str(LIVE_FIXTURE).unwrap();
        let got = parse_live(&v);
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].value, "dfmodel");
        assert_eq!(got[1].display_name, "DeepSeek-Flash");
        assert!(got[1].is_vl && got[1].is_free && got[1].is_new);
        assert_eq!(got[1].max_input_tokens, 180000);
        assert_eq!(got[1].context_windows, vec![200000, 400000, 1000000]);
        assert_eq!(got[1].efforts, vec!["high", "max", "low"]);
        assert_eq!(got[1].default_effort, "high");
        assert!(got[0].is_default);
        assert_eq!(got[2].display_name, "Kimi-K3.1-Pro");
    }

    /// 没有 `value` / `displayName` 的条目没法用，必须丢弃；非数组 ⇒ 空。
    #[test]
    fn live_parse_drops_unusable_entries() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"value":"x"},{"displayName":"Y"},{"value":"ok","displayName":"OK"}]"#,
        )
        .unwrap();
        let got = parse_live(&v);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].value, "ok");
        // 缺 `modelId` 时用 `value` 兜底
        assert_eq!(got[0].model_id, "ok");
        assert!(parse_live(&serde_json::json!({})).is_empty());
        assert!(parse_live(&serde_json::json!([])).is_empty());
        assert!(parse_live(&serde_json::json!("nope")).is_empty());
    }

    /// 落盘再读回来必须逐字段一致（重启后 `/v1/models` 直接可用）。
    #[test]
    fn live_roundtrips_through_json() {
        let v: serde_json::Value = serde_json::from_str(LIVE_FIXTURE).unwrap();
        let got = parse_live(&v);
        let bytes = serde_json::to_vec(&got).unwrap();
        let back: Vec<LiveModel> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(got, back);
    }
}