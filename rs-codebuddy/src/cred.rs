//! cred.rs —— 凭据缝（**P5b：真实实现已落地，见 `credential.rs` / `atrest.rs`**）。
//!
//! 为什么留着这条缝：`/health`、`/v1/models`（探测）、`/v1/chat/completions`（出站头）、
//! `/quota`、`pool.rs`（换号时按账号重取头）都要凭据层的东西——它们在 Go 里分别是
//! `Credential.Summary()`、`Credential.Headers(ctx)`、`Credential.HeadersFor(ctx, path)`
//! 与 `findAuthDirs()`。抽成 trait 后：
//! * **生产**走 [`RealCredential`](crate::credential::RealCredential)（P5b 落地，逐条对齐
//!   Go `Credential`：区域过滤 / at-rest 解密 / token 刷新 / 回写）；
//! * **测试**可用 [`StubCredential`]（`--cred-stub <json>`）喂固定值，让两侧在同一组合成
//!   凭据下逐字节比 `/health`／出站头（P5a 的黄金就是这么采的）。

use std::collections::BTreeMap;

use reqwest::header::HeaderMap;
use serde_json::Value;

/// `/health` 的 credential 块，或错误文案（Go `Credential.Summary()` 的两种形态）。
///
/// `Ok` 用 `BTreeMap` 承载：Go 的 `map[string]any` 在 `json.Marshal` 时按键排序，
/// 有序结构让「写出即 Go 序」这件事在类型上就成立。
#[derive(Debug, Clone)]
pub enum CredSummary {
    Ok(BTreeMap<String, Value>),
    Err(String),
}

/// 凭据层对外可见的几件事。
pub trait CredSource: Send + Sync {
    /// Go `findAuthDirs()`：登录态搜索过哪些目录（`/health` 无凭据文件时的报错文案用）。
    fn auth_dirs(&self) -> Vec<String>;
    /// Go `Credential.Summary()`；`None` 对应 Go 里 summary 为 nil（不写 `credential` 键）。
    fn summary(&self) -> Option<CredSummary>;
    /// Go `Credential.Headers(ctx)`：出站请求头（当前号）。
    fn headers(&self) -> Result<HeaderMap, String>;
    /// Go `Credential.HeadersFor(ctx, path)`：按指定 auth 文件取头（账号池换号后重建头）。
    /// `path` 空 = 当前文件（与 `headers()` 完全一致）。
    fn headers_for(&self, path: &str) -> Result<HeaderMap, String>;
}

/// `--cred-stub <json>` 指定的固定凭据（活体对照/黄金回放用）。
pub struct StubCredential {
    pub auth_dirs: Vec<String>,
    pub summary: Option<CredSummary>,
    pub headers: HeaderMap,
    /// `false` 时 `headers()` 回错误（模拟 Go 侧「凭据不可用」分支）。
    pub headers_ok: bool,
    /// 按 auth 文件路径给不同头（P5b 池场景：两个号两套头）。
    /// 键为空串表示「当前文件」的兜底。
    pub headers_by_path: BTreeMap<String, HeaderMap>,
}

impl StubCredential {
    /// 从 JSON 载入：
    /// `{"auth_dirs":[…],"summary":{…},"headers":{…},"headers_ok":bool,"headers_by_path":{…}}`。
    pub fn from_json(v: &Value) -> Result<StubCredential, String> {
        let auth_dirs = v
            .get("auth_dirs")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let summary = match v.get("summary") {
            None | Some(Value::Null) => None,
            Some(s) => {
                let kind = s.get("kind").and_then(|x| x.as_str()).unwrap_or("");
                match kind {
                    "ok" => {
                        let mut pairs = BTreeMap::new();
                        if let Some(m) = s.get("pairs").and_then(|x| x.as_object()) {
                            for (k, val) in m {
                                pairs.insert(k.clone(), val.clone());
                            }
                        }
                        Some(CredSummary::Ok(pairs))
                    }
                    "error" => Some(CredSummary::Err(
                        s.get("message")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )),
                    other => return Err(format!("cred-stub: 未知 summary.kind={other}")),
                }
            }
        };
        let headers = parse_headers(v.get("headers"))?;
        let mut headers_by_path = BTreeMap::new();
        if let Some(m) = v.get("headers_by_path").and_then(|x| x.as_object()) {
            for (k, val) in m {
                headers_by_path.insert(k.clone(), parse_headers(Some(val))?);
            }
        }
        let headers_ok = v
            .get("headers_ok")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        Ok(StubCredential {
            auth_dirs,
            summary,
            headers,
            headers_ok,
            headers_by_path,
        })
    }
}

fn parse_headers(v: Option<&Value>) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    if let Some(m) = v.and_then(|x| x.as_object()) {
        for (k, val) in m {
            if let Some(s) = val.as_str() {
                let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                    .map_err(|e| format!("cred-stub: 头名非法 {k}: {e}"))?;
                let hv = reqwest::header::HeaderValue::from_str(s)
                    .map_err(|e| format!("cred-stub: 头值非法 {k}: {e}"))?;
                headers.insert(name, hv);
            }
        }
    }
    Ok(headers)
}

impl CredSource for StubCredential {
    fn auth_dirs(&self) -> Vec<String> {
        self.auth_dirs.clone()
    }

    fn summary(&self) -> Option<CredSummary> {
        self.summary.clone()
    }

    fn headers(&self) -> Result<HeaderMap, String> {
        self.headers_for("")
    }

    fn headers_for(&self, path: &str) -> Result<HeaderMap, String> {
        if !self.headers_ok {
            return Err("（stub）凭据不可用".to_string());
        }
        if let Some(h) = self.headers_by_path.get(path) {
            return Ok(h.clone());
        }
        // 未登记的路径回落到通用头（P5a 黄金里所有请求用同一份头）。
        Ok(self.headers.clone())
    }
}
