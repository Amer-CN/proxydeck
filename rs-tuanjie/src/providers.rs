//! providers.rs —— 外部账号（external provider）：管理、信息展示、**三协议分流**
//! 与 `forwardExternal` 的通用 chat 转发面（`internal/tuanjie/providers.go` +
//! `server.go:1432-1567` + `handlers_extra.go:228-292` 的对应物）。
//!
//! ## 交付面
//!
//! * **配置与信息展示**：`ExternalProvider` / `ProviderStore`（`List`/`Match`/`AllModels`/
//!   `Add`/`AddModel`/`RemoveModel`/`Update`/`Remove`/`Infos`/`Invalidate`），
//!   落盘 `tuanjie-providers.json`（经 `crypto::read_file`/`write_file`，与 Go 的
//!   `appdir.ReadFile`/`WriteFile` 同源：旧明文读兼容 + DPAPI 密文自动解密）。
//! * **协议分流**：`ProviderProtocol`（trim+小写归一到 chat/responses/anthropic）、
//!   `externalProtocolRoute`（protocol 字段优先，空值走 `isZenResponsesModel` 白名单兜底）。
//! * **转发**：`forward_external`（分流 + 出站 `/chat/completions` 的 OpenAI 兼容路径）、
//!   `external_base_url`（去尾斜杠 + 补 `/v1`）。
//!
//! ## 已登记的口径差异（详见 README「已知差异」）
//!
//! * 网络级错误的**错误文案**：Go 是 `Get "http://…": dial tcp …` 形态，Rust 是
//!   `error sending request for url (…)`。它只进 `error` 字段/日志文案，**不进**
//!   差分金标的可比面（金标不覆盖网络错场景，见 `tests/golden/README.md`）。
//! * `smartProxyTransport` 的 Windows 注册表分支与 TLS 指纹属 P8c / P8b 生态线。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::chat::UsageScanner;
use futures_util::StreamExt;
use crate::server::{gobj, write_json, AppState, GJ, MAX_BODY_BYTES};

/// 计费查询缓存时长（学群友 120s）—— `providers.go:22`。
pub const PROVIDER_TTL: Duration = Duration::from_secs(120);

/// 单个 provider 计费/模型查询超时 —— `providers.go:27`（运行时可注入，见
/// [`ProviderStore::with_fetch_timeout`]，生产恒 2.5s）。
pub const PROVIDER_FETCH_TIMEOUT: Duration = Duration::from_millis(2500);

/// 配置文件（数据目录）—— `providers.go:74`。
pub const PROVIDERS_FILE: &str = "tuanjie-providers.json";

/// Zen Responses 协议模型名 —— `responses_forward.go:34`。
pub const ZEN_RESPONSES_MODEL: &str = "muse-spark-1.3-contributor-free";

/// 外部出站统一超时（Go `http.Client{Timeout: 120s}`）—— `server.go:1470`。
pub const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(120);

/// 出站错误体读取上限 —— Go `io.LimitReader(resp.Body, 1<<20)`。
pub const MAX_UPSTREAM_ERR_BYTES: usize = 1 << 20;

/// 非流式响应体读取上限 —— Go `io.LimitReader(resp.Body, 64<<20)`。
pub const MAX_UPSTREAM_BODY_BYTES: usize = 64 << 20;

// ---------------------------------------------------------------- 配置结构

/// 用户配置的外部 provider（`tuanjie-providers.json`）—— `providers.go:34-40`。
///
/// 字段序与 `omitempty` 口径逐条对齐 Go 的 struct tag：`models`/`protocol` 为空时
/// 不写进 JSON（`omitempty`），`name`/`base_url`/`api_key` 恒写。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExternalProvider {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
}

/// 单个 provider 的展示信息（**不含 api_key**）—— `providers.go:45-57`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderInfo {
    pub name: String,
    pub base_url: String,
    pub config_models: Vec<String>,
    pub models: Vec<String>,
    pub protocol: String,
    pub ok: bool,
    pub error: String,
    pub sub_status: String,
    pub sub: Option<serde_json::Map<String, serde_json::Value>>,
    pub usage_status: String,
    pub usage: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ProviderInfo {
    /// Go `json.Marshal(ProviderInfo)` 的逐字节等价物：字段按**声明序**、`omitempty`
    /// 逐条照抄（`models`/`config_models` 恒写、可为 `[]`）。
    pub fn to_gj(&self) -> GJ {
        let mut pairs: Vec<(&str, GJ)> = vec![
            ("name", GJ::Str(self.name.clone())),
            ("base_url", GJ::Str(self.base_url.clone())),
            (
                "config_models",
                GJ::Arr(self.config_models.iter().map(|m| GJ::Str(m.clone())).collect()),
            ),
            (
                "models",
                GJ::Arr(self.models.iter().map(|m| GJ::Str(m.clone())).collect()),
            ),
        ];
        if !self.protocol.is_empty() {
            pairs.push(("protocol", GJ::Str(self.protocol.clone())));
        }
        pairs.push(("ok", GJ::Bool(self.ok)));
        if !self.error.is_empty() {
            pairs.push(("error", GJ::Str(self.error.clone())));
        }
        if !self.sub_status.is_empty() {
            pairs.push(("subscription_status", GJ::Str(self.sub_status.clone())));
        }
        if let Some(sub) = &self.sub {
            pairs.push(("subscription", json_map_to_gj(sub)));
        }
        if !self.usage_status.is_empty() {
            pairs.push(("usage_status", GJ::Str(self.usage_status.clone())));
        }
        if let Some(usage) = &self.usage {
            pairs.push(("usage", json_map_to_gj(usage)));
        }
        GJ::ObjOrdered(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }
}

/// `/v1/models` 合并用的 provider 模型条目 —— `providers.go:126-148` 的匿名 struct
/// （字段声明序 id / object / owned_by）。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    pub owned_by: String,
}

impl ModelEntry {
    pub fn to_gj(&self) -> GJ {
        GJ::ObjOrdered(vec![
            ("id".to_string(), GJ::Str(self.id.clone())),
            ("object".to_string(), GJ::Str("model".to_string())),
            ("owned_by".to_string(), GJ::Str(self.owned_by.clone())),
        ])
    }
}

/// `map[string]any` 的 Go 口径写出（键按字节序、数字按 float64）—— 计费响应体搬运。
fn json_map_to_gj(m: &serde_json::Map<String, serde_json::Value>) -> GJ {
    let raw = crate::gojson::marshal_go_value(&serde_json::Value::Object(m.clone()));
    GJ::Raw(String::from_utf8_lossy(&raw).into_owned())
}

// ---------------------------------------------------------------- 纯函数面

/// 归一化出站协议 —— `ProviderProtocol`（`providers.go:154-163`）：
/// trim + 小写，仅 `responses`/`anthropic` 两值有效，其余（含空）一律 `chat`。
pub fn provider_protocol(p: &ExternalProvider) -> String {
    match p.protocol.trim().to_lowercase().as_str() {
        "responses" => "responses".to_string(),
        "anthropic" => "anthropic".to_string(),
        _ => "chat".to_string(),
    }
}

/// Zen 老配置白名单 —— `isZenResponsesModel`（`responses_forward.go:40-42`）：
/// 模型正是那一个且 base_url 含 `opencode.ai/zen`。
pub fn is_zen_responses_model(p: &ExternalProvider, model: &str) -> bool {
    model == ZEN_RESPONSES_MODEL && p.base_url.contains("opencode.ai/zen")
}

/// 出站协议分流 —— `externalProtocolRoute`（`responses_forward.go:47-59`）。
/// `protocol` 字段为空（旧配置）时走 [`is_zen_responses_model`] 兜底。
pub fn external_protocol_route(p: &ExternalProvider, model: &str) -> String {
    match p.protocol.as_str() {
        "responses" => "responses".to_string(),
        "anthropic" => "anthropic".to_string(),
        "" => {
            if is_zen_responses_model(p, model) {
                "responses".to_string()
            } else {
                "chat".to_string()
            }
        }
        _ => "chat".to_string(),
    }
}

/// 转发去向：OpenAI chat / Responses / Anthropic。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Chat,
    Responses,
    Anthropic,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Chat => "chat",
            Protocol::Responses => "responses",
            Protocol::Anthropic => "anthropic",
        }
    }
}

/// `externalProtocolRoute` 的枚举版（供调度点用）。
pub fn route_of(p: &ExternalProvider, model: &str) -> Protocol {
    match external_protocol_route(p, model).as_str() {
        "responses" => Protocol::Responses,
        "anthropic" => Protocol::Anthropic,
        _ => Protocol::Chat,
    }
}

/// 规范化 provider base_url —— `externalBaseURL`（`server.go:1434-1440`）：
/// trim + 去尾斜杠 + 不含 `/v1` 后缀时补上。
pub fn external_base_url(base: &str) -> String {
    let mut s = base.trim().to_string();
    while s.ends_with('/') {
        s.pop();
    }
    if !s.ends_with("/v1") {
        s.push_str("/v1");
    }
    s
}

/// 错误信息截断到 120 字节 —— `truncateErr`（`providers.go:408-413`）。
/// Go 切的是字节（`err[:120]`，切断多字节字符也不崩——Go 字符串本就是字节袋，
/// 后续 JSON 编码器再写成 U+FFFD）；Rust 的 `String` 放不下非法 UTF-8，
/// 故退到 120 以内最后的字符边界（纯 ASCII 输入与 Go 逐字节一致）。
/// 直接 `err[..120]` 会在边界落进多字节字符时 **panic**（P8b 第五片审查 #6）。
pub fn truncate_err(err: &str) -> String {
    if err.len() <= 120 {
        return err.to_string();
    }
    let mut end = 120;
    while end > 0 && !err.is_char_boundary(end) {
        end -= 1;
    }
    err[..end].to_string()
}

/// `getJSON` 的两类错误（对应 Go 的 `statusError` 与其余 error）。
#[derive(Debug, Clone, PartialEq)]
pub enum GetJsonError {
    /// `statusError`：非 200 的上游状态码。
    Status(u16),
    /// 网络/解码错误，文案即 `err.Error()`。
    Other(String),
}

impl GetJsonError {
    pub fn text(&self) -> String {
        match self {
            GetJsonError::Status(code) => format!("HTTP {code}"),
            GetJsonError::Other(msg) => msg.clone(),
        }
    }
}

/// `statusOf`（`providers.go:443-453`）：成功 → `"200"`；状态码错 → `"HTTP <code>"`；
/// 其余错误 → `err.Error()`（>120 字节截断）。
pub fn status_of(err: Option<&GetJsonError>) -> String {
    match err {
        None => "200".to_string(),
        Some(GetJsonError::Status(code)) => format!("HTTP {code}"),
        Some(GetJsonError::Other(msg)) => truncate_err(msg),
    }
}

/// `getJSON`（`providers.go:415-436`）：GET → 200 才解 JSON 对象，否则错误。
pub async fn get_json(
    cl: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<serde_json::Map<String, serde_json::Value>, GetJsonError> {
    let mut req = cl.get(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| GetJsonError::Other(format!("Get {url:?}: {e}")))?;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(GetJsonError::Status(resp.status().as_u16()));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| GetJsonError::Other(e.to_string()))?;
    decode_json_object(&body)
}

/// 解码成 `map[string]any`（`json.Decoder.Decode(&m)` 的等价物，含 Go 的错误文案）。
///
/// Go 的文案由 `encoding/json` 定：空输入是 `EOF`；类型不符是
/// `json: cannot unmarshal <类型> into Go value of type map[string]interface {}`。
/// 这里复刻**可复现的常见形态**（金标覆盖 array/number/string/bool 类型错、`null`
/// 与空体；语法错的逐字文案属平台面，见 README 差异）。
pub fn decode_json_object(
    body: &[u8],
) -> Result<serde_json::Map<String, serde_json::Value>, GetJsonError> {
    let trimmed = trim_ascii(body);
    if trimmed.is_empty() {
        return Err(GetJsonError::Other("EOF".to_string()));
    }
    match serde_json::from_slice::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(m)) => Ok(m),
        Ok(serde_json::Value::Null) => {
            // Go：`json.Unmarshal("null", &m)` 对 map 是 **no-op**（m 保持 nil、无错）
            Ok(serde_json::Map::new())
        }
        Ok(v) => Err(GetJsonError::Other(format!(
            "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
            go_json_kind(&v)
        ))),
        Err(e) => Err(GetJsonError::Other(e.to_string())),
    }
}

fn go_json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Null => "null",
        serde_json::Value::Object(_) => "object",
    }
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = b.len();
    while start < end
        && (b[start] == b' ' || b[start] == b'\t' || b[start] == b'\n' || b[start] == b'\r')
    {
        start += 1;
    }
    while end > start
        && (b[end - 1] == b' ' || b[end - 1] == b'\t' || b[end - 1] == b'\n' || b[end - 1] == b'\r')
    {
        end -= 1;
    }
    &b[start..end]
}

// ---------------------------------------------------------------- ProviderStore

/// 缓存条目 —— `cachedInfo`（`providers.go:68-71`）。
#[derive(Debug, Clone)]
struct CachedInfo {
    info: ProviderInfo,
    fetched: Instant,
}

struct Inner {
    list: Vec<ExternalProvider>,
    cache: BTreeMap<String, CachedInfo>,
    refreshing: BTreeSet<String>,
}

/// 外部 provider 配置 + 计费信息缓存 —— `ProviderStore`（`providers.go:60-66`）。
pub struct ProviderStore {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
    http: reqwest::Client,
}

impl ProviderStore {
    /// 生产构造：从数据目录的 `tuanjie-providers.json` 加载（缺失 = 空）
    /// —— `NewProviderStore()`（`providers.go:79-86`）。
    pub fn new() -> Self {
        Self::with_path(Some(crate::pacing::data_path(PROVIDERS_FILE)))
    }

    /// 显式路径（`None` = 不读不写盘，测试用；**绝不碰真实数据目录**）。
    pub fn with_path(path: Option<PathBuf>) -> Self {
        let ps = ProviderStore {
            inner: Mutex::new(Inner {
                list: Vec::new(),
                cache: BTreeMap::new(),
                refreshing: BTreeSet::new(),
            }),
            path,
            http: build_fetch_client(PROVIDER_FETCH_TIMEOUT),
        };
        if let Some(p) = ps.path.clone() {
            if let Ok(raw) = crate::crypto::read_file(&p) {
                if let Ok(list) = serde_json::from_slice::<Vec<ExternalProvider>>(&raw) {
                    ps.inner.lock().unwrap().list = list;
                }
            }
        }
        ps
    }

    /// 测试缝：注入计费查询超时（生产恒 [`PROVIDER_FETCH_TIMEOUT`]）。
    #[doc(hidden)]
    pub fn with_fetch_timeout(mut self, d: Duration) -> Self {
        self.http = build_fetch_client(d);
        self
    }

    /// 落盘（含 api_key → 加密）—— `saveLocked`（`providers.go:88-95`）。
    /// 失败只在日志留痕，内存里的配置不丢。
    fn save_locked(&self, list: &[ExternalProvider]) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(raw) = serde_json::to_vec_pretty(list) else {
            return;
        };
        if let Err(e) = crate::crypto::write_file(path, &raw) {
            crate::logf(&format!(
                "[tuanjie] 外部账号配置落盘失败（内存态不受影响）：{e}"
            ));
        }
    }

    /// 全部配置（含 api_key，仅供内部/管理端校验）—— `List`（`providers.go:98-104`）。
    pub fn list(&self) -> Vec<ExternalProvider> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).list.clone()
    }

    /// model 命中某 provider 的 models 时返回副本 —— `Match`（`providers.go:108-123`）。
    pub fn match_model(&self, model: &str) -> Option<ExternalProvider> {
        if model.is_empty() {
            return None;
        }
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .list
            .iter()
            .find(|p| p.models.iter().any(|m| m == model))
            .cloned()
    }

    /// 所有外部模型条目 —— `AllModels`（`providers.go:126-148`）。
    pub fn all_models(&self) -> Vec<ModelEntry> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for p in &inner.list {
            for m in &p.models {
                out.push(ModelEntry {
                    id: m.clone(),
                    owned_by: p.name.clone(),
                });
            }
        }
        out
    }

    /// 添加（重名拒绝）+ 归一化 —— `Add`（`providers.go:166-191`）。
    pub fn add(&self, p: ExternalProvider) -> bool {
        let name = p.name.trim().to_string();
        let base = p.base_url.trim().trim_end_matches('/').to_string();
        let key = p.api_key.trim().to_string();
        let protocol = provider_protocol(&ExternalProvider {
            protocol: p.protocol.clone(),
            ..Default::default()
        });
        let models: Vec<String> = p
            .models
            .iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        if name.is_empty() || base.is_empty() || key.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.list.iter().any(|e| e.name == name) {
            return false;
        }
        inner.list.push(ExternalProvider {
            name,
            base_url: base,
            api_key: key,
            models,
            protocol,
        });
        let list = inner.list.clone();
        drop(inner);
        self.save_locked(&list);
        true
    }

    /// 追加模型 —— `AddModel`（`providers.go:194-214`）。
    pub fn add_model(&self, provider_name: &str, model: &str) -> bool {
        let model = model.trim();
        if model.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..inner.list.len() {
            if inner.list[i].name == provider_name {
                if inner.list[i].models.iter().any(|m| m == model) {
                    return false;
                }
                inner.list[i].models.push(model.to_string());
                let list = inner.list.clone();
                drop(inner);
                self.save_locked(&list);
                return true;
            }
        }
        false
    }

    /// 删除模型 —— `RemoveModel`（`providers.go:217-233`）。
    pub fn remove_model(&self, provider_name: &str, model: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..inner.list.len() {
            if inner.list[i].name != provider_name {
                continue;
            }
            let Some(j) = inner.list[i].models.iter().position(|m| m == model) else {
                return false;
            };
            inner.list[i].models.remove(j);
            let list = inner.list.clone();
            drop(inner);
            self.save_locked(&list);
            return true;
        }
        false
    }

    /// 编辑（api_key/protocol 留空 = 保留原值）—— `Update`（`providers.go:239-264`）。
    pub fn update(&self, name: &str, p: ExternalProvider) -> bool {
        let name = name.trim().to_string();
        let mut base = p.base_url.trim().to_string();
        while base.ends_with('/') || base.ends_with('\\') {
            base.pop();
        }
        let key = p.api_key.trim().to_string();
        let protocol = p.protocol.trim().to_string();
        if name.is_empty() || base.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..inner.list.len() {
            if inner.list[i].name != name {
                continue;
            }
            inner.list[i].base_url = base;
            if !key.is_empty() {
                inner.list[i].api_key = key;
            }
            if !protocol.is_empty() {
                inner.list[i].protocol = provider_protocol(&ExternalProvider {
                    protocol: protocol.clone(),
                    ..Default::default()
                });
            }
            inner.cache.remove(&name);
            let list = inner.list.clone();
            drop(inner);
            self.save_locked(&list);
            return true;
        }
        false
    }

    /// 删除 —— `Remove`（`providers.go:267-279`）。
    pub fn remove(&self, name: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(i) = inner.list.iter().position(|e| e.name == name) else {
            return false;
        };
        inner.list.remove(i);
        inner.cache.remove(name);
        let list = inner.list.clone();
        drop(inner);
        self.save_locked(&list);
        true
    }

    /// 清空缓存 —— `Invalidate`（`providers.go:350-354`）。
    pub fn invalidate(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cache
            .clear();
    }

    /// 展示信息（stale-while-revalidate）—— `Infos`（`providers.go:285-320`）。
    ///
    /// 冷启动（无缓存）**同步等**（并行 + `fetch_timeout` 超时）；过期但有旧值时
    /// 立刻返回旧值、刷新丢给后台单飞。`refreshing` 防同一 provider 并发重拉。
    pub async fn infos(self: &Arc<Self>) -> Vec<ProviderInfo> {
        let mut out: Vec<ProviderInfo>;
        let mut cold: Vec<(ExternalProvider, usize)> = Vec::new();
        let mut warm: Vec<(ExternalProvider, usize)> = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let list = inner.list.clone();
            out = Vec::with_capacity(list.len());
            for (i, p) in list.iter().enumerate() {
                let cached = inner.cache.get(&p.name).cloned();
                match &cached {
                    Some(c) => out.push(c.info.clone()),
                    None => out.push(base_info(p)),
                }
                if let Some(c) = &cached {
                    if c.fetched.elapsed() < PROVIDER_TTL {
                        continue;
                    }
                }
                if inner.refreshing.contains(&p.name) {
                    continue;
                }
                inner.refreshing.insert(p.name.clone());
                if cached.is_some() {
                    warm.push((p.clone(), i));
                } else {
                    cold.push((p.clone(), i));
                }
            }
        }
        if !cold.is_empty() {
            let fetched = self.fetch_many(cold).await;
            for (idx, info) in fetched {
                out[idx] = info;
            }
        }
        if !warm.is_empty() {
            let me = self.clone();
            tokio::spawn(async move {
                let _ = me.fetch_many(warm).await;
            });
        }
        out
    }

    /// 并行拉取一批并写缓存（同时复位 refreshing），返回 (idx, info) —— `fetchInto`
    /// （`providers.go:330-347`）。后台刷新路径不写调用方切片（`out = nil` 的等价物）。
    async fn fetch_many(&self, jobs: Vec<(ExternalProvider, usize)>) -> Vec<(usize, ProviderInfo)> {
        let futures = jobs.into_iter().map(|(p, idx)| {
            let http = self.http.clone();
            async move {
                let info = fetch_provider_info(&http, &p).await;
                (p.name, idx, info)
            }
        });
        let results = futures_util::future::join_all(futures).await;
        let mut out = Vec::with_capacity(results.len());
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            for (name, idx, info) in results {
                inner.cache.insert(
                    name.clone(),
                    CachedInfo {
                        info: info.clone(),
                        fetched: Instant::now(),
                    },
                );
                inner.refreshing.remove(&name);
                out.push((idx, info));
            }
        }
        out
    }
}

fn build_fetch_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("构造 provider 查询客户端")
}

/// 仅由本地配置拼出的展示信息 —— `baseInfo`（`providers.go:359-363`）。
/// `protocol` 透传原始值（空值原样给空串，前端徽章只认 responses/anthropic）。
pub fn base_info(p: &ExternalProvider) -> ProviderInfo {
    ProviderInfo {
        name: p.name.clone(),
        base_url: p.base_url.clone(),
        config_models: p.models.clone(),
        models: Vec::new(),
        protocol: p.protocol.clone(),
        ..Default::default()
    }
}

/// 查单个 provider：模型列表 + 订阅 + 用量 —— `fetchProviderInfo`（`providers.go:371-405`）。
///
/// **OK 口径**：只以 `/models` 连通性为准（转发真正依赖的端点）；订阅/用量端点
/// 仅供展示，失败只进状态字段、不影响 OK。
pub async fn fetch_provider_info(http: &reqwest::Client, p: &ExternalProvider) -> ProviderInfo {
    let mut info = base_info(p);
    let auth = format!("Bearer {}", p.api_key);
    let hdr: Vec<(&str, &str)> = vec![
        ("Authorization", auth.as_str()),
        ("Accept", "application/json"),
    ];

    let models_url = format!("{}/models", p.base_url);
    match get_json(http, &models_url, &hdr).await {
        Err(e) => {
            info.ok = false;
            info.error = truncate_err(&e.text());
        }
        Ok(r) => {
            info.ok = true;
            if let Some(serde_json::Value::Array(ms)) = r.get("data") {
                for m in ms {
                    if let Some(id) = m.get("id").and_then(|x| x.as_str()) {
                        if !id.is_empty() {
                            info.models.push(id.to_string());
                        }
                    }
                }
            }
        }
    }

    let sub = get_json(
        http,
        &format!("{}/dashboard/billing/subscription", p.base_url),
        &hdr,
    )
    .await;
    info.sub_status = status_of(sub.as_ref().err());
    if let Ok(sub) = sub {
        info.sub = Some(sub);
    }
    let usage = get_json(
        http,
        &format!("{}/dashboard/billing/usage", p.base_url),
        &hdr,
    )
    .await;
    info.usage_status = status_of(usage.as_ref().err());
    if let Ok(usage) = usage {
        info.usage = Some(usage);
    }
    info
}

// ---------------------------------------------------------------- HTTP 端点

/// `GET|POST /providers`（`handleProviders`，`handlers_extra.go:228-292`）。
pub async fn handle_providers(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    let method = req.method().clone();
    if method == Method::GET {
        let infos = st.providers.infos().await;
        return write_json(&gobj(vec![
            ("ok", GJ::Bool(true)),
            (
                "providers",
                GJ::Arr(infos.iter().map(|i| i.to_gj()).collect()),
            ),
        ]));
    }
    if method != Method::POST {
        return crate::server::method_not_allowed(None);
    }
    let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap_or_default();
    let Some(reqj) = decode_provider_body(&body) else {
        return write_json(&gobj(vec![
            ("ok", GJ::Bool(false)),
            ("msg", GJ::Str("请求体解析失败".to_string())),
        ]));
    };
    let get_s = |k: &str| {
        reqj.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let action = get_s("action");
    let name = get_s("name");
    let msg = |ok: bool, text: &str| {
        write_json(&gobj(vec![
            ("ok", GJ::Bool(ok)),
            ("msg", GJ::Str(text.to_string())),
        ]))
    };
    match action.as_str() {
        "add" => {
            let p = ExternalProvider {
                name: name.clone(),
                base_url: get_s("base_url"),
                api_key: get_s("api_key"),
                models: reqj
                    .get("models")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|x| x.as_str().unwrap_or("").to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
                protocol: get_s("protocol"),
            };
            if st.providers.add(p) {
                st.providers.invalidate();
                msg(true, "自定义服务商已添加")
            } else {
                msg(false, "添加失败（名称/base_url/key 不能为空，或名称已存在）")
            }
        }
        "edit" => {
            let p = ExternalProvider {
                name: name.clone(),
                base_url: get_s("base_url"),
                api_key: get_s("api_key"),
                protocol: get_s("protocol"),
                ..Default::default()
            };
            if st.providers.update(&name, p) {
                st.providers.invalidate();
                msg(true, "自定义服务商已更新")
            } else {
                msg(false, "更新失败（账号不存在或 base_url 为空）")
            }
        }
        "remove" => {
            if st.providers.remove(&name) {
                st.providers.invalidate();
                msg(true, "自定义服务商已删除")
            } else {
                msg(false, "未找到该自定义服务商")
            }
        }
        "addmodel" => {
            if st.providers.add_model(&name, &get_s("model")) {
                st.providers.invalidate();
                msg(true, "模型已添加")
            } else {
                msg(false, "添加失败（账号不存在或模型已存在/为空）")
            }
        }
        "removemodel" => {
            if st.providers.remove_model(&name, &get_s("model")) {
                st.providers.invalidate();
                msg(true, "模型已删除")
            } else {
                msg(false, "删除失败（账号或模型不存在）")
            }
        }
        _ => msg(false, "未知 action"),
    }
}

/// `handleProviders` 的请求体判据（Go `json.NewDecoder(r.Body).Decode(&struct{…})`）：
/// 读第一个值；顶层须是对象（`null` = 全缺省）；六个已知字段出现时类型须对得上
/// （string / []string，显式 `null` 视为缺省）；未知字段忽略。
fn decode_provider_body(body: &[u8]) -> Option<serde_json::Value> {
    let v = crate::server::first_json_value(body)?;
    let o = match &v {
        serde_json::Value::Null => return Some(v),
        serde_json::Value::Object(o) => o,
        _ => return None,
    };
    for k in ["action", "name", "base_url", "api_key", "model", "protocol"] {
        if let Some(f) = o.get(k) {
            if !f.is_string() && !f.is_null() {
                return None;
            }
        }
    }
    if let Some(f) = o.get("models") {
        let ok = f.is_null()
            || f.as_array()
                .map(|a| a.iter().all(|x| x.is_string() || x.is_null()))
                .unwrap_or(false);
        if !ok {
            return None;
        }
    }
    Some(v)
}

// ---------------------------------------------------------------- 转发

/// `forwardExternal`（`server.go:1450-1567`）。
///
/// 返回 `Ok(已写出的响应)` = Go 的 `0`（干净完成或已向客户端写出，**不可重试**）；
/// `Err(status)` = Go 的 `>0`（上游状态码，尚未写任何字节）或 `-1`（网络级错误）。
/// 调用方（识图回落链 / 图片视频端点）据此决定回落或写终态。
pub async fn forward_external(
    st: &Arc<AppState>,
    body: Vec<u8>,
    model: &str,
    wants_stream: bool,
    prov: &ExternalProvider,
    path: &str,
) -> Result<Response, i32> {
    let started = Instant::now();
    if path == "/chat/completions" {
        // `sanitizeForExternal` schema 修复 + 协议分流（`server.go:1452-1465`）。
        let body = crate::media::sanitize_for_external(&body);
        match route_of(prov, model) {
            Protocol::Responses => {
                return crate::responses_forward::forward_external_responses(
                    st,
                    body,
                    model,
                    wants_stream,
                    prov,
                    started,
                )
                .await;
            }
            Protocol::Anthropic => {
                return crate::anthropic_forward::forward_external_anthropic(
                    st,
                    body,
                    model,
                    wants_stream,
                    prov,
                    started,
                )
                .await;
            }
            Protocol::Chat => {
                return forward_external_chat(st, body, model, wants_stream, prov, path, started)
                    .await
            }
        }
    }
    forward_external_chat(st, body, model, wants_stream, prov, path, started).await
}

/// 外部出站请求的公共头（`server.go:1483-1486` / `responses_forward.go:564-571` /
/// `anthropic_forward.go:563-571`）：认证头由调用方给，其余三件套 + opencode 会话头。
pub fn external_headers(prov: &ExternalProvider, auth: (&str, String), accept: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&auth.1) {
        h.insert(
            reqwest::header::HeaderName::from_bytes(auth.0.as_bytes()).expect("头名合法"),
            v,
        );
    }
    h.insert(
        reqwest::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(v) = HeaderValue::from_str(accept) {
        h.insert(reqwest::header::ACCEPT, v);
    }
    crate::opencode_session::apply_opencode_session(&prov.base_url, &mut h);
    h
}

/// OpenAI 兼容的通用外部转发（`server.go:1466-1567`）。
pub async fn forward_external_chat(
    st: &Arc<AppState>,
    body: Vec<u8>,
    model: &str,
    wants_stream: bool,
    prov: &ExternalProvider,
    path: &str,
    started: Instant,
) -> Result<Response, i32> {
    let pname = prov.name.clone();
    let rid = st.registry.register(model, &pname, wants_stream);
    let url = format!("{}{}", external_base_url(&prov.base_url), path);
    let cl = reqwest::Client::builder()
        .timeout(EXTERNAL_TIMEOUT)
        .build()
        .map_err(|_| -1)?;
    let headers = external_headers(prov, ("Authorization", format!("Bearer {}", prov.api_key)), "application/json");

    // 传输层「快失败」重试一次（`server.go:1477-1497`）：≤10s 内失败才重试。
    let mut attempt = 0;
    let resp = loop {
        let req = cl.post(&url).headers(headers.clone()).body(body.clone());
        match req.send().await {
            Ok(r) => break Ok(r),
            Err(e) => {
                if attempt == 0 && started.elapsed() <= Duration::from_secs(10) {
                    st.activity.add(
                        "info",
                        &format!("{model} · {pname} 传输错误重试: {e}"),
                        model,
                        &pname,
                        started.elapsed().as_millis() as i64,
                        0,
                        0,
                    );
                    crate::logf(&format!(
                        "[tuanjie] external model={model} provider={pname} 传输错误重试: {e}"
                    ));
                    attempt += 1;
                    continue;
                }
                break Err(e);
            }
        }
    };
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            let latency = started.elapsed().as_millis() as i64;
            st.activity.add(
                "error",
                &format!("{model} · {pname} 异常: {e}"),
                model,
                &pname,
                latency,
                0,
                0,
            );
            crate::logf(&format!(
                "[tuanjie] external chat model={model} provider={pname} err={e}"
            ));
            st.registry.finish(&rid);
            return Err(-1);
        }
    };
    let status = resp.status();
    let latency = started.elapsed().as_millis() as i64;
    if status != reqwest::StatusCode::OK {
        let err_headers = resp.headers().clone();
        let err_body = crate::chat::read_body_bounded(resp, MAX_UPSTREAM_ERR_BYTES).await;
        st.activity.add(
            "error",
            &format!("{model} · 外部源 {pname} 上游返回 {}", status.as_u16()),
            model,
            &pname,
            latency,
            0,
            status.as_u16() as i32,
        );
        crate::logf(&format!(
            "[tuanjie] external chat model={model} provider={pname} status={}",
            status.as_u16()
        ));
        st.registry.finish(&rid);
        if crate::media::should_fallback(status.as_u16() as i32) {
            return Err(status.as_u16() as i32);
        }
        // Go `server.go:1517`：4xx 透传**只**设 Content-Type（取上游值，缺失则空），
        // 不复制其它头——此前用 `copy_header` 全复制，与同片 responses/anthropic 两条
        // 路径不一致（P8b 第五片审查 #2）。
        let mut out_headers = HeaderMap::new();
        let ct = err_headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if let Ok(v) = axum::http::HeaderValue::from_str(ct) {
            out_headers.insert(axum::http::header::CONTENT_TYPE, v);
        }
        return Ok((status, out_headers, Body::from(err_body)).into_response());
    }
    crate::logf(&format!(
        "[tuanjie] external chat model={model} provider={pname} status=200"
    ));

    // 响应头复制 + 两个固定头（`server.go:1527-1529`）。
    let mut out_headers = HeaderMap::new();
    crate::chat::copy_header(&mut out_headers, resp.headers());
    out_headers.insert("X-Accel-Buffering", HeaderValue::from_static("no"));
    out_headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));

    if wants_stream {
        // 流式逐块透传 + usage 扫描（`server.go:1530-1550`）；流尽时补 `ok` 事件 +
        // 配平 registry（Go 是 handler 返回时的 defer）。
        let stc = st.clone();
        let pname2 = pname.clone();
        let model2 = model.to_string();
        let scanner = UsageScanner::new(model);
        let guard = crate::chat::StreamGuard::with_callback(st.registry.clone(), rid.clone(), move || {
            stc.activity.add(
                "ok",
                &format!("外部源 {pname2} · {model2}（流式）"),
                &model2,
                &pname2,
                started.elapsed().as_millis() as i64,
                0,
                200,
            );
        });
        let stc2 = st.clone();
        let stream = futures_util::stream::unfold(
            (resp.bytes_stream(), scanner, guard),
            move |(mut inner, mut scan, mut g)| {
                let stc2 = stc2.clone();
                async move {
                    match inner.next().await {
                        Some(Ok(chunk)) => {
                            scan.feed(&stc2.stats, &chunk);
                            stc2.registry.touch(g.rid(), chunk.len() as i64);
                            let out: Result<Vec<u8>, std::io::Error> = Ok(chunk.to_vec());
                            Some((out, (inner, scan, g)))
                        }
                        _ => {
                            scan.finish();
                            g.done();
                            None
                        }
                    }
                }
            },
        );
        return Ok((StatusCode::OK, out_headers, Body::from_stream(stream)).into_response());
    }

    let resp_body = crate::chat::read_body_bounded(resp, MAX_UPSTREAM_BODY_BYTES).await;
    st.registry.touch(&rid, resp_body.len() as i64);
    st.activity.add(
        "ok",
        &format!("外部源 {pname} · {model}"),
        model,
        &pname,
        started.elapsed().as_millis() as i64,
        0,
        200,
    );
    st.registry.finish(&rid);
    // 非流式：usage 入账（`server.go:1553-1565`）。
    record_external_usage(st, model, &resp_body);
    Ok((StatusCode::OK, out_headers, Body::from(resp_body)).into_response())
}

/// 非流式外部响应的 usage 入账 —— `server.go:1553-1565`。
pub fn record_external_usage(st: &AppState, model: &str, body: &[u8]) {
    #[derive(serde::Deserialize)]
    struct Done {
        #[serde(default)]
        usage: Option<Usage>,
    }
    #[derive(serde::Deserialize)]
    struct Usage {
        #[serde(default)]
        prompt_tokens: i64,
        #[serde(default)]
        completion_tokens: i64,
        #[serde(default)]
        total_tokens: i64,
    }
    let Ok(done) = serde_json::from_slice::<Done>(body) else {
        return;
    };
    let Some(u) = done.usage else {
        return;
    };
    st.stats
        .add(model, u.prompt_tokens, u.completion_tokens, u.total_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov(name: &str, base: &str, protocol: &str) -> ExternalProvider {
        ExternalProvider {
            name: name.to_string(),
            base_url: base.to_string(),
            api_key: "k".to_string(),
            models: vec!["m".to_string()],
            protocol: protocol.to_string(),
        }
    }

    #[test]
    fn provider_protocol_normalizes_like_go() {
        // `ProviderProtocol`：trim + 小写，仅 responses/anthropic 有效，其余归 chat
        assert_eq!(provider_protocol(&prov("p", "u", " responses ")), "responses");
        assert_eq!(provider_protocol(&prov("p", "u", "ANTHROPIC")), "anthropic");
        assert_eq!(provider_protocol(&prov("p", "u", "")), "chat");
        assert_eq!(provider_protocol(&prov("p", "u", "zzz")), "chat");
    }

    #[test]
    fn route_reads_raw_protocol_field_like_go() {
        // 分流只看 protocol 的**原值**（归一化只在 Add/Update 落盘时做）——
        // 金标 `ext-chat-unormalized-protocol` 锁的就是这条。
        assert_eq!(
            external_protocol_route(&prov("p", "u", " ANTHROPIC "), "m"),
            "chat"
        );
        assert_eq!(
            external_protocol_route(&prov("p", "u", "anthropic"), "m"),
            "anthropic"
        );
        // 空值走 Zen 白名单兜底
        assert_eq!(
            external_protocol_route(
                &prov("zen", "https://x/opencode.ai/zen", ""),
                ZEN_RESPONSES_MODEL
            ),
            "responses"
        );
        assert_eq!(
            external_protocol_route(&prov("zen", "https://x/opencode.ai/zen", ""), "other"),
            "chat"
        );
        // 模型对但 base 不对 ⇒ 不兜底（`strings.Contains` 大小写敏感）
        assert_eq!(
            external_protocol_route(&prov("zen", "https://x/other", ""), ZEN_RESPONSES_MODEL),
            "chat"
        );
        assert_eq!(
            external_protocol_route(
                &prov("zen", "https://x/OPENCODE.AI/ZEN", ""),
                ZEN_RESPONSES_MODEL
            ),
            "chat"
        );
    }

    #[test]
    fn external_base_url_appends_v1_exactly_once() {
        assert_eq!(external_base_url("https://h/"), "https://h/v1");
        assert_eq!(external_base_url("  https://h  "), "https://h/v1");
        assert_eq!(external_base_url("https://h/v1"), "https://h/v1");
        assert_eq!(external_base_url("https://h/v1/"), "https://h/v1");
        // 非 `/v1` 结尾一律补（Go 是 `HasSuffix`，`/v1x` 也会被补）
        assert_eq!(external_base_url("https://h/v1x"), "https://h/v1x/v1");
    }

    #[test]
    fn truncate_and_status_shapes_match_go() {
        let long = "a".repeat(200);
        assert_eq!(truncate_err(&long).len(), 120);
        assert_eq!(truncate_err("short"), "short");
        assert_eq!(status_of(None), "200");
        assert_eq!(status_of(Some(&GetJsonError::Status(404))), "HTTP 404");
        assert_eq!(
            status_of(Some(&GetJsonError::Other("boom".to_string()))),
            "boom"
        );
        assert_eq!(status_of(Some(&GetJsonError::Other(long))).len(), 120);
    }

    #[test]
    fn decode_json_object_mirrors_go_type_errors() {
        assert!(decode_json_object(br#"{"a":1}"#).is_ok());
        // Go：null 解进 map 是 no-op（nil map，无错）
        assert!(decode_json_object(b"null").unwrap().is_empty());
        // Go：空输入是 EOF
        assert_eq!(
            decode_json_object(b"   ").unwrap_err(),
            GetJsonError::Other("EOF".to_string())
        );
        // Go：类型不符的文案
        assert_eq!(
            decode_json_object(b"[1,2]").unwrap_err(),
            GetJsonError::Other(
                "json: cannot unmarshal array into Go value of type map[string]interface {}"
                    .to_string()
            )
        );
        assert!(matches!(
            decode_json_object(b"\"x\"").unwrap_err(),
            GetJsonError::Other(m) if m.contains("cannot unmarshal string")
        ));
        assert!(matches!(
            decode_json_object(b"3").unwrap_err(),
            GetJsonError::Other(m) if m.contains("cannot unmarshal number")
        ));
        assert!(matches!(
            decode_json_object(b"true").unwrap_err(),
            GetJsonError::Other(m) if m.contains("cannot unmarshal bool")
        ));
    }

    #[test]
    fn provider_info_json_keeps_declaration_order_and_omitempty() {
        let info = ProviderInfo {
            name: "p".into(),
            base_url: "u".into(),
            config_models: vec!["m".into()],
            models: vec![],
            protocol: String::new(),
            ok: true,
            error: String::new(),
            sub_status: "HTTP 404".into(),
            sub: None,
            usage_status: String::new(),
            usage: None,
        };
        let json = String::from_utf8(info.to_gj().to_go_json_line()).unwrap();
        assert_eq!(
            json,
            "{\"name\":\"p\",\"base_url\":\"u\",\"config_models\":[\"m\"],\
             \"models\":[],\"ok\":true,\"subscription_status\":\"HTTP 404\"}\n"
        );
    }

    #[test]
    fn store_add_update_remove_semantics_match_go() {
        // 临时目录（**绝不碰真实数据目录**）
        let dir = std::env::temp_dir().join(format!(
            "rs-tuanjie-providers-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join(PROVIDERS_FILE);
        let ps = ProviderStore::with_path(Some(path.clone()));

        // 归一化：名字/base 去空白与尾斜杠、models 去空白、protocol 归一
        assert!(ps.add(ExternalProvider {
            name: "  newp  ".into(),
            base_url: " http://x/  ".into(),
            api_key: " k1 ".into(),
            models: vec![" m1 ".into(), "".into(), "m2".into()],
            protocol: " Responses ".into(),
        }));
        let list = ps.list();
        assert_eq!(list[0].name, "newp");
        assert_eq!(list[0].base_url, "http://x");
        assert_eq!(list[0].api_key, "k1");
        assert_eq!(list[0].models, vec!["m1".to_string(), "m2".to_string()]);
        assert_eq!(list[0].protocol, "responses");
        // 重名拒绝
        assert!(!ps.add(prov("newp", "http://y", "chat")));
        // 缺 key 拒绝
        assert!(!ps.add(ExternalProvider {
            name: "nokey".into(),
            base_url: "http://x".into(),
            ..Default::default()
        }));
        // 编辑：api_key/protocol 留空 = 保留原值，models 不动
        assert!(ps.update(
            "newp",
            ExternalProvider {
                name: "newp".into(),
                base_url: "http://y/\\".into(),
                api_key: String::new(),
                protocol: String::new(),
                models: vec!["zzz".into()],
            }
        ));
        let list = ps.list();
        assert_eq!(list[0].base_url, "http://y");
        assert_eq!(list[0].api_key, "k1");
        assert_eq!(list[0].protocol, "responses");
        assert_eq!(list[0].models.len(), 2);
        // addmodel / removemodel
        assert!(ps.add_model("newp", "m3"));
        assert!(!ps.add_model("newp", "m3"));
        assert!(ps.remove_model("newp", "m1"));
        assert!(!ps.remove_model("newp", "m1"));
        // 落盘读回（DPAPI/明文双形态走同一 reader）
        let ps2 = ProviderStore::with_path(Some(path));
        assert_eq!(ps2.list(), ps.list());
        assert!(ps.remove("newp"));
        assert!(!ps.remove("newp"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
