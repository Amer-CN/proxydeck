//! `internal/qoder/usage.go` 的 Rust 等价物：实时额度（`/health` 的 `credits.realtime`）。
//!
//! 官方 App 同款接口（`app.asar` 的 `getAccountUsage` 实录）：
//! `GET https://openapi.qoder.com.cn/api/v2/quota/usage`，
//! 头 `Authorization: Bearer <token>` / `Accept: application/json` / `Cosy-ClientType: 1` /
//! `User-Agent: Qoder`。**域名是 CN 版 `.com.cn`**——国际版 `openapi.qoder.sh` 会拒这个 token
//! （`TOKEN_INVALID`）。
//!
//! 获取策略（用户明确要求：**绝不自动轮询官方接口**）：
//! * `force=false`（普通 `/health` 轮询）→ 只回已有缓存，从不访问官方接口；
//! * `force=true`（`/health?refresh=1`，积分卡「↻ 刷新」按钮）→ 现拉一次并更新缓存；
//! * 拉取失败回落旧缓存（有总比没有强）。缓存长期有效，直到下次手动刷新。

use std::time::Duration;

use serde::Deserialize;

use crate::gjson::{b, f, i, obj, s, J};

/// 官方额度接口（CN 版；对应 Go `fetchUsage` 里的字面量）。
pub const USAGE_URL: &str = "https://openapi.qoder.com.cn/api/v2/quota/usage";

/// `/api/v2/quota/usage` 响应里我们关心的字段（对应 Go `usageQuota`）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UsageQuota {
    #[serde(rename = "isQuotaExceeded", default)]
    pub is_quota_exceeded: bool,
    /// `null` ⇒ 与 Go 的 `*struct{...}` 为空指针等价（`usageView` 直接回 nil）。
    #[serde(rename = "userQuota", default)]
    pub user_quota: Option<UserQuota>,
    /// `null` ⇒ Go 得到 nil 切片 ⇒ 视图里是空数组。
    #[serde(rename = "dedicatedResourcePackages", default)]
    pub dedicated_resource_packages: Option<Vec<Package>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UserQuota {
    #[serde(default)]
    pub total: f64,
    #[serde(default)]
    pub used: f64,
    #[serde(default)]
    pub remaining: f64,
    #[serde(default)]
    pub unit: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Package {
    #[serde(default)]
    pub total: f64,
    #[serde(default)]
    pub used: f64,
    #[serde(default)]
    pub remaining: f64,
    #[serde(default)]
    pub available: bool,
    #[serde(rename = "expiresAt", default)]
    pub expires_at: i64,
}

/// 拉一次实时额度（对应 Go `fetchUsage`）。错误串与 Go 同义。
pub async fn fetch_usage(
    client: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<UsageQuota, String> {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("Cosy-ClientType", "1")
        .header("User-Agent", "Qoder")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if status != 200 {
        // Go：io.ReadAll(io.LimitReader(resp.Body, 1024))，然后原文串进错误
        let body = resp.bytes().await.unwrap_or_default();
        let cut = body.len().min(1024);
        return Err(format!(
            "额度接口 HTTP {}: {}",
            status,
            String::from_utf8_lossy(&body[..cut])
        ));
    }
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    // Go：json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&u) —— 超长输入会被截断，
    // 截断后解析多半失败；这里同样只喂前 1MB，保持同一形状。
    let cut = body.len().min(1 << 20);
    serde_json::from_slice::<UsageQuota>(&body[..cut])
        .map_err(|e| format!("额度响应解析失败: {e}"))
}

/// 把 `UsageQuota` 转成 `/health` 的 `credits.realtime` 视图（对应 Go `usageView`）。
/// `userQuota` 缺失 ⇒ `None`（Go 返回 nil，调用方就不挂 `realtime` 键）。
pub fn usage_view(u: &UsageQuota) -> Option<J> {
    let plan = u.user_quota.as_ref()?;
    let mut avail_remaining = 0.0f64;
    let mut packs: Vec<J> = Vec::new();
    for p in u.dedicated_resource_packages.as_deref().unwrap_or(&[]) {
        if p.available {
            avail_remaining += p.remaining;
        }
        packs.push(obj(vec![
            ("total", f(p.total)),
            ("used", f(p.used)),
            ("remaining", f(p.remaining)),
            ("available", b(p.available)),
            ("expiresAt", i(p.expires_at)),
        ]));
    }
    Some(obj(vec![
        (
            "plan",
            obj(vec![
                ("total", f(plan.total)),
                ("used", f(plan.used)),
                ("remaining", f(plan.remaining)),
                ("unit", s(&plan.unit)),
            ]),
        ),
        ("isQuotaExceeded", b(u.is_quota_exceeded)),
        ("packages", J::Arr(packs)),
        ("packagesRemaining", f(avail_remaining)),
        ("totalRemaining", f(plan.remaining + avail_remaining)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "isQuotaExceeded": false,
        "userQuota": {"total": 1000, "used": 250.5, "remaining": 749.5, "unit": "credits"},
        "dedicatedResourcePackages": [
            {"total": 100, "used": 10, "remaining": 90, "available": true, "expiresAt": 1790000000000},
            {"total": 50, "used": 50, "remaining": 0, "available": false, "expiresAt": 0}
        ],
        "unknownField": "ignored"
    }"#;

    #[test]
    fn view_shape_matches_go() {
        let u: UsageQuota = serde_json::from_str(SAMPLE).unwrap();
        let v = usage_view(&u).expect("userQuota 存在，必须给视图");
        assert_eq!(
            v.to_go(),
            concat!(
                "{\"isQuotaExceeded\":false,",
                "\"packages\":[{\"available\":true,\"expiresAt\":1790000000000,",
                "\"remaining\":90,\"total\":100,\"used\":10},",
                "{\"available\":false,\"expiresAt\":0,\"remaining\":0,\"total\":50,\"used\":50}],",
                "\"packagesRemaining\":90,",
                "\"plan\":{\"remaining\":749.5,\"total\":1000,\"unit\":\"credits\",\"used\":250.5},",
                "\"totalRemaining\":839.5}"
            )
        );
    }

    #[test]
    fn missing_user_quota_yields_no_view() {
        let u: UsageQuota = serde_json::from_str(r#"{"isQuotaExceeded":true}"#).unwrap();
        assert!(usage_view(&u).is_none(), "Go 的 usageView 对 nil UserQuota 回 nil");
        let u: UsageQuota = serde_json::from_str(r#"{"userQuota":null}"#).unwrap();
        assert!(usage_view(&u).is_none(), "userQuota=null 同样是 nil");
    }

    #[test]
    fn null_packages_is_empty_array() {
        let u: UsageQuota =
            serde_json::from_str(r#"{"userQuota":{"total":1,"used":0,"remaining":1,"unit":"c"}}"#)
                .unwrap();
        let v = usage_view(&u).unwrap().to_go();
        assert!(v.contains("\"packages\":[]"), "nil 切片 → 空数组: {v}");
        assert!(v.contains("\"packagesRemaining\":0"));
    }

    #[test]
    fn type_mismatch_is_an_error_like_go() {
        // Go 的 Unmarshal 对类型不符报错 ⇒ fetchUsage 回错误 ⇒ 回落旧缓存
        assert!(serde_json::from_str::<UsageQuota>(r#"{"isQuotaExceeded":"x"}"#).is_err());
        assert!(serde_json::from_str::<UsageQuota>(r#"{"userQuota":{"total":"x"}}"#).is_err());
    }
}
