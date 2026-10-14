//! cred.rs —— 凭据缝（**P5a 只定义契约，不实现凭据本身**）。
//!
//! 为什么需要这条缝：P5a 的范围是「合同与请求侧」，而 `/health`、`/v1/models`（探测）、
//! `/v1/chat/completions`（出站头）三处都要凭据层的东西——它们在 Go 里分别是
//! `Credential.Summary()`、`Credential.Headers(ctx)` 与 `findAuthDirs()`。凭据文件格式、
//! at-rest 解密（DPAPI）、region 过滤、token 刷新属 **P5b**，本轮不碰真实登录态。
//!
//! 因此本模块把这三样抽成 trait：
//! * `summary()` —— Go `Summary()` 的两种返回（正常块 / 错误文案）；
//! * `headers()` —— Go `Headers(ctx)`（Err = 探测跳过、chat 502，与 Go 同形）；
//! * `auth_dirs()` —— Go `findAuthDirs()`（`/health` 的 `auth_search_dirs`）。
//!
//! P5a 的默认实现是 [`PendingCredential`]：**如实报「凭据层未接入」**，不是假装成功：
//! `/health` 回 `status=error` 并写明原因；`/v1/models` 走 Go 在「headers 取不到」时的
//! 回退分支（整池候选）；`/v1/chat/completions` 回 502（与 Go 凭据不可用时的形状一致）。
//!
//! [`StubCredential`] 是**测试缝**（`--cred-stub <json>`）：给活体对照用固定值喂缝，
//! 让两侧在同一组合成凭据下逐字节比 `/health`。P5b 落地后这两个实现都会被真实凭据层替换。

use reqwest::header::HeaderMap;
use serde_json::Value;

/// `/health` 的 credential 块，或错误文案（Go `Credential.Summary()` 的两种形态）。
#[derive(Debug, Clone)]
pub enum CredSummary {
    /// 正常：键值对（写出时按键排序）。
    Ok(Vec<(String, Value)>),
    /// 出错：`/health` 会把它放进 `message` 并把 `status` 置为 `error`。
    Err(String),
}

/// 凭据层对外可见的三件事（P5b 实现）。
pub trait CredSource: Send + Sync {
    /// Go `findAuthDirs()`：登录态搜索过哪些目录。
    fn auth_dirs(&self) -> Vec<String>;
    /// Go `Credential.Summary()`；`None` 对应 Go 里 summary 为 nil（不写 `credential` 键）。
    fn summary(&self) -> Option<CredSummary>;
    /// Go `Credential.Headers(ctx)`：出站请求头。
    fn headers(&self) -> Result<HeaderMap, String>;
}

/// P5a 默认实现：凭据层尚未接入（属 P5b）。
pub struct PendingCredential;

impl CredSource for PendingCredential {
    fn auth_dirs(&self) -> Vec<String> {
        Vec::new()
    }

    fn summary(&self) -> Option<CredSummary> {
        Some(CredSummary::Err(
            "（P5a）凭据层尚未接入：登录态读取 / at-rest 解密 / region 过滤属 P5b".to_string(),
        ))
    }

    fn headers(&self) -> Result<HeaderMap, String> {
        Err("（P5a）凭据层尚未接入：登录态读取 / at-rest 解密 / region 过滤属 P5b".to_string())
    }
}

/// 测试缝：`--cred-stub <json>` 指定的固定凭据（活体对照用）。
pub struct StubCredential {
    pub auth_dirs: Vec<String>,
    pub summary: Option<CredSummary>,
    pub headers: HeaderMap,
    /// `false` 时 `headers()` 回错误（模拟 Go 侧「凭据不可用」分支）。
    pub headers_ok: bool,
}

impl StubCredential {
    /// 从 JSON 载入：`{"auth_dirs":[…],"summary":{…},"headers":{…},"headers_ok":bool}`。
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
                        let mut pairs = Vec::new();
                        if let Some(m) = s.get("pairs").and_then(|x| x.as_object()) {
                            for (k, val) in m {
                                pairs.push((k.clone(), val.clone()));
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
        let mut headers = HeaderMap::new();
        if let Some(m) = v.get("headers").and_then(|x| x.as_object()) {
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
        let headers_ok = v
            .get("headers_ok")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        Ok(StubCredential {
            auth_dirs,
            summary,
            headers,
            headers_ok,
        })
    }
}

impl CredSource for StubCredential {
    fn auth_dirs(&self) -> Vec<String> {
        self.auth_dirs.clone()
    }

    fn summary(&self) -> Option<CredSummary> {
        self.summary.clone()
    }

    fn headers(&self) -> Result<HeaderMap, String> {
        if self.headers_ok {
            Ok(self.headers.clone())
        } else {
            Err("（stub）凭据不可用".to_string())
        }
    }
}
