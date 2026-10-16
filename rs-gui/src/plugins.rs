//! plugins.rs —— 插件注册表（对应 Go `app/plugins.go` 的 `pluginDef` / `pluginDefs` /
//! `pluginHealthURL` / `pluginLog` / `pluginErrLog`）。
//!
//! **本阶段只建注册表**：`ccLiveSnapshot` 要靠它拼六插件端口的探活清单，`ccPluginLog`
//! 要靠它定位日志。**托管逻辑（探活接管、拉杆启停、退出监视、换血）属 P7b**，
//! 见 README 的「P7b/P7c/P7d 接手清单」。

use std::path::PathBuf;

/// 一个插件的注册信息（字段与 Go `pluginDef` 一一对应）。
pub struct PluginDef {
    /// 唯一标识
    pub id: &'static str,
    /// 显示名
    pub name: &'static str,
    /// `plugins` 下目录名（Python 插件用；原生插件为空）
    pub dir: &'static str,
    /// 入口脚本（Python 插件用；原生插件为空）
    pub script: &'static str,
    /// 附加命令行参数（Go 侧 GUI 启动时追加）
    pub args: &'static [&'static str],
    /// 监听端口
    pub port: u16,
    /// 健康检查路径
    pub health: &'static str,
    /// 非空 = 原生内置插件（本 exe 的 `--plugin-<id>` 子模式）
    pub native: &'static str,
}

/// 插件注册表（顺序与 Go `pluginDefs` 逐条一致——`pluginList` 输出顺序依赖它）。
pub const PLUGIN_DEFS: &[PluginDef] = &[
    PluginDef {
        id: "tuanjie",
        name: "团结 Cowork (Codely)",
        dir: "",
        script: "",
        args: &[],
        port: 8788,
        health: "/health",
        native: "tuanjie",
    },
    PluginDef {
        id: "codebuddy",
        name: "WorkBuddy / CodeBuddy",
        dir: "",
        script: "",
        args: &["--desensitize"],
        port: 8787,
        health: "/health",
        native: "codebuddy",
    },
    PluginDef {
        id: "codebuddy-intl",
        name: "WorkBuddy Intl",
        dir: "",
        script: "",
        args: &["--desensitize"],
        port: 8789,
        health: "/health",
        native: "codebuddy-intl",
    },
    PluginDef {
        id: "bai",
        name: "B.AI",
        dir: "",
        script: "",
        args: &[],
        port: 8891,
        health: "/health",
        native: "bai",
    },
    PluginDef {
        id: "comate",
        name: "Comate",
        dir: "",
        script: "",
        args: &[],
        port: 8786,
        health: "/health",
        native: "comate",
    },
    PluginDef {
        id: "qoder",
        name: "Qoder",
        dir: "",
        script: "",
        args: &[],
        port: 8785,
        health: "/health",
        native: "qoder",
    },
    PluginDef {
        id: "vibex",
        name: "VibeX",
        dir: "",
        script: "",
        args: &[],
        port: 8790,
        health: "/health",
        native: "vibex",
    },
];

/// 按 id 取插件定义。
pub fn find(id: &str) -> Option<&'static PluginDef> {
    PLUGIN_DEFS.iter().find(|d| d.id == id)
}

/// 插件健康检查地址（Go `pluginHealthURL`：「http://127.0.0.1:<port><health>」）。
pub fn plugin_health_url(d: &PluginDef) -> String {
    format!("http://127.0.0.1:{}{}", d.port, d.health)
}

/// 插件运行日志路径（Go `pluginLog`：数据目录下 `plugins/<id>.log`）。
pub fn plugin_log_path(d: &PluginDef) -> PathBuf {
    crate::appdir::path(&format!("plugins/{}.log", d.id))
}

/// 原生插件启动失败时子模式写的诊断文件（Go `pluginErrLog`：数据目录下 `<id>-plugin-error.log`）。
pub fn plugin_err_log_path(d: &PluginDef) -> PathBuf {
    crate::appdir::path(&format!("{}-plugin-error.log", d.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_matches_go_ports_and_order() {
        let ids: Vec<&str> = PLUGIN_DEFS.iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            vec![
                "tuanjie",
                "codebuddy",
                "codebuddy-intl",
                "bai",
                "comate",
                "qoder",
                "vibex"
            ],
            "顺序与 Go pluginDefs 一致（pluginList 输出顺序依赖它）"
        );
        assert_eq!(find("tuanjie").unwrap().port, 8788);
        assert_eq!(find("codebuddy").unwrap().port, 8787);
        assert_eq!(find("codebuddy-intl").unwrap().port, 8789);
        assert_eq!(find("bai").unwrap().port, 8891);
        assert_eq!(find("comate").unwrap().port, 8786);
        assert_eq!(find("qoder").unwrap().port, 8785);
        assert_eq!(find("vibex").unwrap().port, 8790);
    }

    #[test]
    fn health_url_and_log_paths() {
        let d = find("tuanjie").unwrap();
        assert_eq!(plugin_health_url(d), "http://127.0.0.1:8788/health");
        assert!(plugin_log_path(d)
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("plugins/tuanjie.log"));
        assert!(plugin_err_log_path(d)
            .to_string_lossy()
            .ends_with("tuanjie-plugin-error.log"));
    }

    #[test]
    fn codebuddy_and_intl_carry_desensitize_like_go() {
        assert_eq!(find("codebuddy").unwrap().args, &["--desensitize"]);
        assert_eq!(find("codebuddy-intl").unwrap().args, &["--desensitize"]);
        assert!(find("tuanjie").unwrap().args.is_empty());
    }
}
