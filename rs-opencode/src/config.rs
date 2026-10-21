//! config.rs —— `%APPDATA%\ProxyDeck\opencode.json` 的读取与默认值。
//!
//! 配置不存在 / 读不出来 / JSON 坏掉时一律退回默认值并打日志，**绝不让插件起不来**
//! （第一版没有 UI 可改配置，用户手改坏 JSON 时服务仍要能跑）。
use std::path::PathBuf;

use serde::Deserialize;

use crate::datadir;

/// 配置文件名（数据目录下）。
pub const CONFIG_FILE: &str = datadir::CONFIG_FILE;

/// 与 `/api/model` 的 `providerID` 对照：只路由这两个上游。
pub const DEFAULT_PROVIDERS: [&str; 2] = ["opencode", "opencode-go"];
/// 地区限制模型（中国大陆不可用），默认过滤。
pub const DEFAULT_DENYLIST: [&str; 1] = ["beam-free"];
/// 默认路由模型（免费档；不存在时由模型池退到列表里的第一个可用项）。
pub const DEFAULT_MODEL: &str = "opencode/deepseek-v4.1-flash-free";
/// 单轮对话总超时（秒）。
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
/// 429 后单模型冷却（秒）。
pub const DEFAULT_COOLDOWN_SECONDS: u64 = 60;

/// 插件配置。字段名对应用户可手改的 JSON camelCase。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// 允许路由的 providerID 白名单。
    pub providers: Vec<String>,
    /// 模型黑名单：对 `<providerID>/<modelID>` 做大小写不敏感的子串匹配。
    pub model_denylist: Vec<String>,
    /// 入站请求未指定模型（或指定模型正在冷却）时的首选模型。
    pub default_model: String,
    /// 单轮对话总超时（秒）。超时杀会话并回 504。
    pub timeout_seconds: u64,
    /// 单模型撞 429 后的冷却时间（秒），内存态。
    pub cooldown_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            providers: DEFAULT_PROVIDERS.iter().map(|s| s.to_string()).collect(),
            model_denylist: DEFAULT_DENYLIST.iter().map(|s| s.to_string()).collect(),
            default_model: DEFAULT_MODEL.to_string(),
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            cooldown_seconds: DEFAULT_COOLDOWN_SECONDS,
        }
    }
}

impl Config {
    /// 过滤空串并夹住下界：配置写 `0` 时不至于把超时/冷却算成"立即生效"。
    fn normalized(mut self) -> Self {
        self.providers = self
            .providers
            .into_iter()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        self.model_denylist = self
            .model_denylist
            .into_iter()
            .map(|m| m.trim().to_lowercase())
            .filter(|m| !m.is_empty())
            .collect();
        self.default_model = self.default_model.trim().to_string();
        if self.default_model.is_empty() {
            self.default_model = DEFAULT_MODEL.to_string();
        }
        self.timeout_seconds = self.timeout_seconds.max(1);
        self.cooldown_seconds = self.cooldown_seconds.max(1);
        self
    }
}

/// 解析配置文本（缺字段走默认值，坏 JSON 报错给 `load` 打日志用）。
pub fn parse(text: &str) -> Result<Config, String> {
    let cfg: Config =
        serde_json::from_str(text).map_err(|e| format!("opencode.json 解析失败: {e}"))?;
    Ok(cfg.normalized())
}

/// 读取配置：`path` 为空时用 `<数据目录>/opencode.json`。
///
/// 文件不存在是正常路径（第一次跑），其它读取/解析错误也只打日志、回默认值。
pub fn load(path: Option<PathBuf>) -> Config {
    let path = path.unwrap_or_else(|| datadir::path(CONFIG_FILE));
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse(&text) {
            Ok(cfg) => {
                crate::logf(&format!("配置：{}", path.display()));
                cfg
            }
            Err(e) => {
                crate::logf(&format!("{e}；改用默认配置"));
                Config::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::logf(&format!("配置：未找到 {}，使用默认值", path.display()));
            Config::default()
        }
        Err(e) => {
            crate::logf(&format!(
                "配置：读取 {} 失败（{e}），使用默认值",
                path.display()
            ));
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_brief() {
        let c = Config::default();
        assert_eq!(c.providers, vec!["opencode", "opencode-go"]);
        assert_eq!(c.model_denylist, vec!["beam-free"]);
        assert_eq!(c.default_model, "opencode/deepseek-v4.1-flash-free");
        assert_eq!(c.timeout_seconds, 300);
        assert_eq!(c.cooldown_seconds, 60);
    }

    #[test]
    fn partial_json_keeps_defaults_and_normalizes() {
        let c = parse(
            r#"{
                "providers": [" opencode "],
                "modelDenylist": ["BEAM-FREE", " "],
                "defaultModel": "  ",
                "timeoutSeconds": 0,
                "cooldownSeconds": 0
            }"#,
        )
        .unwrap();
        assert_eq!(c.providers, vec!["opencode"]);
        assert_eq!(c.model_denylist, vec!["beam-free"]);
        assert_eq!(c.default_model, DEFAULT_MODEL);
        assert_eq!(c.timeout_seconds, 1);
        assert_eq!(c.cooldown_seconds, 1);
    }

    #[test]
    fn empty_json_uses_defaults() {
        let c = parse("{}").unwrap();
        assert_eq!(c.providers, Config::default().providers);
        assert_eq!(c.timeout_seconds, 300);
    }

    #[test]
    fn broken_json_is_an_error() {
        assert!(parse("{not json").is_err());
    }
}
