//! config.rs —— 插件配置（对应 Go `internal/vibex/config.go`）。
//!
//! C1~C9：缺省值、`base_url` 裸域补 `/vc`、`tenant_id` 生成落盘、配置文件整份读写（DPAPI + ACL）、
//! `persistToken` 的同号收敛、token 三来源优先级。
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::crypto;
use crate::datadir;
use crate::gjson::{st, J};
use crate::jwt::decode_jwt;
use crate::logf;

/// 缺省值（对齐 SPEC-T1 §3.1 的 DEFAULTS）。
pub const DEFAULT_BASE_URL: &str = "https://vibex.runninghub.cn/vc";
pub const DEFAULT_APP_NAME: &str = "vibex2api";
pub const DEFAULT_APP_TYPE: &str = "web";
pub const DEFAULT_TURN_TIMEOUT_SEC: i64 = 900;
pub const DEFAULT_READY_TIMEOUT_SEC: i64 = 90;
pub const DEFAULT_QUEUE_LIMIT: usize = 4;
pub const DEFAULT_HEARTBEAT_SEC: i64 = 15;
/// WS 会话初始工作目录（`init`/`init_root` 的 `cwd`）。
pub const WS_INIT_CWD: &str = "/workspace/app";
/// 图片块占位文案（与 Go `imagePlaceholder` 同值）。
pub const IMAGE_PLACEHOLDER: &str = "[图片内容不支持, 已忽略]";
/// 缺省任务指令（用简体中文；对方实现是繁体，不照抄）。
pub const DEFAULT_INSTRUCTION: &str = "请直接以「助手」的身份回答最后一条用户消息: 直接给出回答内容本身; \
                                       这是一次纯问答, 无需创建或修改任何项目文件。";

/// 402/429 命中后该 token 的冷却时长（Go `tokenCooldown = 10 * time.Minute`）。
pub const TOKEN_COOLDOWN_MS: i64 = 10 * 60 * 1000;

/// 上游 UA（照抄 SPEC-T1 §3.3，逐字）。
pub const UPSTREAM_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                               (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 插件自身版本（与 ProxyDeck 版本串无关；`/health` 的 `version` 字段）。
pub const PLUGIN_VERSION: &str = "1.0.0";

/// 插件配置；JSON 字段名与 SPEC-T1 §3.1 的 config.json 对齐（字段顺序 = Go struct 声明顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub base_url: String,
    pub app_name: String,
    pub app_type: String,
    pub model: String,
    pub tenant_id: String,
    pub tokens: Vec<String>,
    pub fresh_session: bool,
    pub turn_timeout_sec: i64,
    pub ready_timeout_sec: i64,
    pub queue_limit: i64,
    pub heartbeat_sec: i64,
    pub include_thinking: bool,
    pub expose_tools: bool,
    pub chat_instruction: String,
    pub connect_mode: String,
    /// 配置文件路径（`tenant_id` 生成后回写用）；不出现在 JSON 里。
    pub path: PathBuf,
}

impl Config {
    /// 缺省值（等价 Go `LoadConfig` 里那份字面量）。
    pub fn defaults(path: PathBuf) -> Config {
        Config {
            base_url: DEFAULT_BASE_URL.to_string(),
            app_name: DEFAULT_APP_NAME.to_string(),
            app_type: DEFAULT_APP_TYPE.to_string(),
            model: String::new(),
            tenant_id: String::new(),
            tokens: Vec::new(),
            fresh_session: true,
            turn_timeout_sec: DEFAULT_TURN_TIMEOUT_SEC,
            ready_timeout_sec: DEFAULT_READY_TIMEOUT_SEC,
            queue_limit: DEFAULT_QUEUE_LIMIT as i64,
            heartbeat_sec: DEFAULT_HEARTBEAT_SEC,
            include_thinking: false,
            expose_tools: false,
            chat_instruction: String::new(),
            connect_mode: "auto".to_string(),
            path,
        }
    }

    /// `baseURL()`：TrimRight `/`。
    pub fn base_url(&self) -> String {
        self.base_url.trim_end_matches('/').to_string()
    }

    pub fn turn_timeout_ms(&self) -> i64 {
        self.turn_timeout_sec * 1000
    }
    pub fn ready_timeout_ms(&self) -> i64 {
        self.ready_timeout_sec * 1000
    }
    pub fn heartbeat_ms(&self) -> i64 {
        self.heartbeat_sec * 1000
    }

    /// `normalize`：把空值/非法值收敛回缺省；裸域 `base_url` 自动补 `/vc`。
    pub fn normalize(&mut self) {
        if self.base_url.trim().is_empty() {
            self.base_url = DEFAULT_BASE_URL.to_string();
        }
        // REST 网关要 /vc 前缀（WS 由 base_url 取 host 直连，不受影响）：
        // 裸域（无 path）自动补 /vc，自带 path 的自定义地址保持原样。
        let trimmed = self.base_url.trim_end_matches('/').to_string();
        if let Some((scheme, rest)) = split_scheme(&trimmed) {
            let (host, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            // 只有「裸域补 /vc」这一条分支会改写 `base_url`；自带 path 的自定义地址
            // **一个字都不动**（Go 只在补 /vc 的分支里回写 `u.String()`，其余分支
            // `c.BaseURL` 保持原值——含末尾斜杠。读的时候 `baseURL()` 才 TrimRight）。
            if !host.is_empty() && (path.is_empty() || path == "/") {
                self.base_url = format!("{scheme}://{host}/vc");
            }
        }
        if self.app_name.trim().is_empty() {
            self.app_name = DEFAULT_APP_NAME.to_string();
        }
        if self.app_type.trim().is_empty() {
            self.app_type = DEFAULT_APP_TYPE.to_string();
        }
        if self.turn_timeout_sec <= 0 {
            self.turn_timeout_sec = DEFAULT_TURN_TIMEOUT_SEC;
        }
        if self.ready_timeout_sec <= 0 {
            self.ready_timeout_sec = DEFAULT_READY_TIMEOUT_SEC;
        }
        if self.queue_limit <= 0 {
            self.queue_limit = DEFAULT_QUEUE_LIMIT as i64;
        }
        if self.heartbeat_sec <= 0 {
            self.heartbeat_sec = DEFAULT_HEARTBEAT_SEC;
        }
        if self.connect_mode.trim().is_empty() {
            self.connect_mode = "auto".to_string();
        }
    }

    /// 配置文件内容（Go `json.MarshalIndent(c, "", "  ")` 的等价物：struct 字段顺序 + 两空格）。
    pub fn to_go_indent2(&self) -> String {
        let pairs: Vec<(&str, J)> = vec![
            ("base_url", J::Str(self.base_url.clone())),
            ("app_name", J::Str(self.app_name.clone())),
            ("app_type", J::Str(self.app_type.clone())),
            ("model", J::Str(self.model.clone())),
            ("tenant_id", J::Str(self.tenant_id.clone())),
            (
                "tokens",
                if self.tokens.is_empty() {
                    J::Null
                } else {
                    J::Arr(self.tokens.iter().map(|t| J::Str(t.clone())).collect())
                },
            ),
            ("fresh_session", J::Bool(self.fresh_session)),
            ("turn_timeout", J::Int(self.turn_timeout_sec)),
            ("ready_timeout", J::Int(self.ready_timeout_sec)),
            ("queue_limit", J::Int(self.queue_limit)),
            ("heartbeat_s", J::Int(self.heartbeat_sec)),
            ("include_thinking", J::Bool(self.include_thinking)),
            ("expose_tools", J::Bool(self.expose_tools)),
            ("chat_instruction", J::Str(self.chat_instruction.clone())),
            ("connect_mode", J::Str(self.connect_mode.clone())),
        ];
        st(pairs).to_go_indent2()
    }

    /// `persist`：落盘（DPAPI 加密 + ACL 收紧 + tmp→rename）。
    pub fn persist(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Err("配置文件路径为空".into());
        }
        crypto::write_file(&self.path, self.to_go_indent2().as_bytes())
    }

    /// `persistToken`：把新 token 追加进配置文件并同步内存 `tokens`。
    /// 同号去重：同 `sub` 只留最后一条（最新登录串），无 sub 的按串保留。
    pub fn persist_token(&mut self, token: &str) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Err("配置文件路径为空".into());
        }
        let token = token.trim();
        if token.is_empty() {
            return Err("token 为空".into());
        }
        let mut tokens: Vec<String> = self
            .tokens
            .iter()
            .filter(|t| !t.trim().is_empty())
            .cloned()
            .collect();
        if !tokens.iter().any(|t| t == token) {
            tokens.push(token.to_string());
        }
        // 从后往前扫，同 sub 只留最后一条；再反转回原顺序。
        let mut seen: Vec<String> = Vec::new();
        let mut out: Vec<String> = Vec::new();
        for t in tokens.iter().rev() {
            let sub = decode_jwt(t).map(|i| i.sub).unwrap_or_default();
            if sub.is_empty() {
                out.push(t.clone());
                continue;
            }
            if seen.iter().any(|s| s == &sub) {
                continue;
            }
            seen.push(sub);
            out.push(t.clone());
        }
        out.reverse();
        let prev = self.tokens.clone();
        self.tokens = out;
        if let Err(e) = self.persist() {
            self.tokens = prev;
            return Err(e);
        }
        Ok(())
    }
}

/// 极简 scheme 拆分（等价 `url.Parse` 对 `https://host/path` 的 scheme/host 提取）。
fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let idx = s.find("://")?;
    let scheme = &s[..idx];
    if scheme.is_empty() {
        return None;
    }
    for c in scheme.chars() {
        if !(c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return None;
        }
    }
    Some((scheme, &s[idx + 3..]))
}

/// 从 JSON 对象里取一个字段（Go `encoding/json` 的字段匹配：**文档顺序**、大小写不敏感、
/// 重复键后者生效）。
pub fn go_field<'a>(obj: &'a Map<String, Value>, tag: &str) -> Option<&'a Value> {
    let want = tag.to_lowercase();
    let mut found: Option<&Value> = None;
    for (k, v) in obj {
        if k.to_lowercase() == want {
            found = Some(v);
        }
    }
    found
}

/// 把 JSON 对象灌进 `Config`（只认已知键，多余键忽略；解析失败不阻断启动）。
pub fn apply_json(cfg: &mut Config, v: &Value) -> bool {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return false,
    };
    let get_str = |tag: &str| -> Option<String> {
        match go_field(obj, tag) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    };
    let get_bool = |tag: &str| -> Option<bool> {
        match go_field(obj, tag) {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    };
    let get_int = |tag: &str| -> Option<i64> {
        match go_field(obj, tag) {
            Some(Value::Number(n)) => n.as_f64().map(|f| f as i64),
            _ => None,
        }
    };
    if let Some(v) = get_str("base_url") {
        cfg.base_url = v;
    }
    if let Some(v) = get_str("app_name") {
        cfg.app_name = v;
    }
    if let Some(v) = get_str("app_type") {
        cfg.app_type = v;
    }
    if let Some(v) = get_str("model") {
        cfg.model = v;
    }
    if let Some(v) = get_str("tenant_id") {
        cfg.tenant_id = v;
    }
    if let Some(Value::Array(items)) = go_field(obj, "tokens") {
        cfg.tokens = items
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
    }
    if let Some(v) = get_bool("fresh_session") {
        cfg.fresh_session = v;
    }
    if let Some(v) = get_int("turn_timeout") {
        cfg.turn_timeout_sec = v;
    }
    if let Some(v) = get_int("ready_timeout") {
        cfg.ready_timeout_sec = v;
    }
    if let Some(v) = get_int("queue_limit") {
        cfg.queue_limit = v;
    }
    if let Some(v) = get_int("heartbeat_s") {
        cfg.heartbeat_sec = v;
    }
    if let Some(v) = get_bool("include_thinking") {
        cfg.include_thinking = v;
    }
    if let Some(v) = get_bool("expose_tools") {
        cfg.expose_tools = v;
    }
    if let Some(v) = get_str("chat_instruction") {
        cfg.chat_instruction = v;
    }
    if let Some(v) = get_str("connect_mode") {
        cfg.connect_mode = v;
    }
    true
}

/// `LoadConfig` 的输入来源（替代 Go 的包级 flag/env 全局量，便于测试隔离）。
pub struct LoadInputs {
    pub config_path: Option<PathBuf>,
    pub flag_token: String,
    pub env_token: String,
}

/// 读配置：缺省值 ← 配置文件（明文或 DPAPI 密文）← 环境变量覆盖 token。
/// `tenant_id` 为空 → 生成 32 位 hex 并立即落盘（失败不阻断）。
pub fn load_config(inputs: &LoadInputs) -> Config {
    let path = match &inputs.config_path {
        Some(p) if !p.as_os_str().is_empty() => p.clone(),
        _ => datadir::path(datadir::CONFIG_FILE),
    };
    let mut cfg = Config::defaults(path.clone());
    if let Ok(b) = crypto::read_file(&path) {
        let v = crate::util::json_unmarshal(&b);
        // 解析失败就用缺省值，不阻断启动
        let _ = apply_json(&mut cfg, &v);
    }
    cfg.normalize();
    if cfg.tenant_id.is_empty() {
        cfg.tenant_id = rand_hex(16);
        let _ = cfg.persist(); // 落盘失败不阻断（只影响 X-Tenant-Id 的稳定性）
    }
    cfg
}

/// `tokenSources`：环境变量 `RH_ACCESSTOKEN` 优先，其次 `--vibex-token`，
/// 最后配置文件 `tokens` 数组；去重保序（池按此顺序轮换）。
pub fn token_sources(cfg: &Config, flag_token: &str, env_token: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(cfg.tokens.len() + 2);
    let add = |v: &str, out: &mut Vec<String>| {
        let v = v.trim();
        if !v.is_empty() && !out.iter().any(|x| x == v) {
            out.push(v.to_string());
        }
    };
    add(env_token, &mut out);
    add(flag_token, &mut out);
    for t in &cfg.tokens {
        add(t, &mut out);
    }
    out
}

/// `randHex(n)`：n 字节随机数的 hex 串（tenant_id 用）。
pub fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    if rand_bytes(&mut b).is_err() {
        return String::new();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 填一段系统熵（WS 握手 key、掩码 key 都用它）。
pub fn rand_bytes(buf: &mut [u8]) -> Result<(), String> {
    getrandom(buf).map_err(|_| "取系统随机数失败".to_string())
}

/// 取系统熵（Windows 用 `BCryptGenRandom`，其它平台读 `/dev/urandom`）。
#[cfg(windows)]
fn getrandom(buf: &mut [u8]) -> Result<(), ()> {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            alg: *mut core::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG = 0x00000002
    let ok = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            0x0000_0002,
        )
    };
    if ok == 0 {
        Ok(())
    } else {
        Err(())
    }
}

#[cfg(not(windows))]
fn getrandom(buf: &mut [u8]) -> Result<(), ()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|_| ())
}

/// 启动日志里用的路径描述（不含 token）。
pub fn describe_path(p: &Path) -> String {
    p.display().to_string()
}

/// 把 token 来源统计成一句可安全打印的日志（绝不打印 token 本体）。
pub fn describe_sources(cfg: &Config, flag_token: &str, env_token: &str) -> String {
    let total = token_sources(cfg, flag_token, env_token).len();
    let from_file = cfg.tokens.iter().filter(|t| !t.trim().is_empty()).count();
    let from_env = usize::from(!env_token.trim().is_empty());
    let from_flag = usize::from(!flag_token.trim().is_empty());
    logf(&format!(
        "token 来源：环境变量 {from_env} / 命令行 {from_flag} / 配置文件 {from_file}，合计 {total}（token 本体不打印）"
    ));
    format!("tokens={total}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::str_of;

    #[test]
    fn normalize_adds_vc_only_for_bare_hosts() {
        let mut c = Config::defaults(PathBuf::new());
        c.base_url = "https://vibex.runninghub.cn".into();
        c.normalize();
        assert_eq!(c.base_url, "https://vibex.runninghub.cn/vc");

        let mut c = Config::defaults(PathBuf::new());
        c.base_url = "https://vibex.runninghub.cn/".into();
        c.normalize();
        assert_eq!(c.base_url, "https://vibex.runninghub.cn/vc");

        let mut c = Config::defaults(PathBuf::new());
        c.base_url = "https://vibex.runninghub.cn/vc".into();
        c.normalize();
        assert_eq!(c.base_url, "https://vibex.runninghub.cn/vc");

        let mut c = Config::defaults(PathBuf::new());
        c.base_url = "http://127.0.0.1:9/custom/path".into();
        c.normalize();
        assert_eq!(c.base_url, "http://127.0.0.1:9/custom/path");

        let mut c = Config::defaults(PathBuf::new());
        c.base_url = "  ".into();
        c.turn_timeout_sec = 0;
        c.ready_timeout_sec = -1;
        c.queue_limit = 0;
        c.heartbeat_sec = 0;
        c.app_name = "".into();
        c.app_type = "".into();
        c.connect_mode = "".into();
        c.normalize();
        assert_eq!(c.base_url, DEFAULT_BASE_URL);
        assert_eq!(c.turn_timeout_sec, DEFAULT_TURN_TIMEOUT_SEC);
        assert_eq!(c.ready_timeout_sec, DEFAULT_READY_TIMEOUT_SEC);
        assert_eq!(c.queue_limit, DEFAULT_QUEUE_LIMIT as i64);
        assert_eq!(c.heartbeat_sec, DEFAULT_HEARTBEAT_SEC);
        assert_eq!(c.connect_mode, "auto");
    }

    #[test]
    fn config_json_roundtrip_and_indent() {
        let mut c = Config::defaults(PathBuf::from("x.json"));
        c.tokens = vec!["t1".into()];
        let text = c.to_go_indent2();
        assert!(text.starts_with("{\n  \"base_url\": \"https://vibex.runninghub.cn/vc\",\n"), "{text}");
        let v: Value = serde_json::from_str(&text).expect("应是合法 JSON");
        let mut back = Config::defaults(PathBuf::new());
        apply_json(&mut back, &v);
        assert_eq!(back.base_url, c.base_url);
        assert_eq!(back.tokens, c.tokens);
        assert_eq!(back.fresh_session, true);
    }

    #[test]
    fn go_field_is_case_insensitive_and_last_wins() {
        let v: Value = serde_json::from_str(r#"{"ACTION":"add","action":"remove"}"#).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(str_of(go_field(obj, "action")), "remove");
        let v: Value = serde_json::from_str(r#"{"Action":"add"}"#).unwrap();
        assert_eq!(str_of(go_field(v.as_object().unwrap(), "action")), "add");
        assert!(go_field(v.as_object().unwrap(), "token").is_none());
    }

    #[test]
    fn rand_hex_is_32_chars() {
        let h = rand_hex(16);
        assert_eq!(h.len(), 32);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
