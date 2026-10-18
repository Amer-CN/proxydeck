//! media.rs —— 多模态能力判定 + 远程图片预下载。
//!
//! 2026-10-18 用户裁决：**删除**生图/生视频（Agnes 两条链路）、「识图改路由」与
//! 外部服务商三协议转发。本模块只保留仍被其它路径复用的纯函数：
//!
//! * [`multimodal_reject_message`]：请求含非文本内容块（图片/音频等）而目标模型不在
//!   [`BUILTIN_VISION_CAPABLE`] 里 → 给出明确 400 文案（列出可用多模态模型）。
//!   **不再改写 model 后继续请求**（旧 `RerouteIfMedia` 已删）。
//! * [`fetch_remote_images`] + `data:` URI 转换 + 魔术字节嗅探。
//!
//! 端点 `/vision-config`、`/media-config`、`/v1/images/generations`、`/v1/videos*`、
//! `/agnesapi` 与 `MediaState` 已整体删除（路由不再注册 ⇒ 404）。

use std::time::Duration;

/// 团结内置原生多模态模型清单 —— `builtinVisionCapable`（`media.go:109-114`）。
///
/// **用途已变更**：不再作为「识图改路由」的改写白名单，而是「请求含图片/音频时
/// 目标模型能否直收」的拒绝判断依据。
pub const BUILTIN_VISION_CAPABLE: [&str; 4] = [
    "codely-vl",
    "KIMI-K3",
    "GLM-5.3-FLASH",
    "DeepSeek-V4.1-Flash",
];

/// 远程图片下载超时 —— `fetchImgClient`（`media.go:434`）。
pub const FETCH_IMG_TIMEOUT: Duration = Duration::from_secs(15);

/// 远程图片下载上限 —— `io.LimitReader(resp.Body, 10<<20)`（`media.go:504`）。
pub const MAX_IMAGE_BYTES: usize = 10 << 20;

// ---------------------------------------------------------------- 纯函数

/// 是否值得回落重试 —— `shouldFallback`（`media.go:175-177`）：
/// 网络错（-1）/ 5xx / 429 是瞬时故障；4xx 配置错原样透传。
///
/// 原调用方（外部服务商三协议转发路径）已随自定义服务商整体删除（2026-10-18），
/// 函数**保留**（公开 API + 单测），行为不变。
pub fn should_fallback(status: i32) -> bool {
    status == -1 || status >= 500 || status == 429
}

// ---------------------------------------------------------------- 多模态判定

/// 目标模型是否在内置多模态清单里（**大小写不敏感**）。
pub fn is_builtin_vision_capable(model: &str) -> bool {
    !model.is_empty()
        && BUILTIN_VISION_CAPABLE
            .iter()
            .any(|m| model.eq_ignore_ascii_case(m))
}

/// 请求含图片/音频等非文本内容块、且目标模型不在内置多模态清单时，返回给客户端的
/// 400 文案；否则 `None`。
///
/// 语义取代旧的 `RerouteIfMedia`：**不再静默改写 model 后继续请求**，而是明确拒绝并
/// 列出当前可用的内置多模态模型。
pub fn multimodal_reject_message(body: &[u8], model: &str) -> Option<String> {
    if is_builtin_vision_capable(model) {
        return None;
    }
    let kind = detect_media_type(body);
    if kind.is_empty() {
        return None;
    }
    let label = if model.is_empty() { "（未指定）" } else { model };
    let available = BUILTIN_VISION_CAPABLE.join(" / ");
    Some(format!(
        "模型 {label} 不支持图片/音频输入（检测到 {kind}）；当前可用多模态模型：{available}。\
         请改用上述模型，或移除请求中的非文本内容。"
    ))
}

/// 扫描 messages，返回第一个非文本内容块类型 —— `DetectMediaType`（`media.go:272-295`）。
pub fn detect_media_type(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::new();
    };
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return String::new();
    };
    for m in msgs {
        let Some(parts) = m.get("content").and_then(|c| c.as_array()) else {
            continue; // content 是字符串（纯文本），跳过
        };
        for p in parts {
            if let Some(t) = p.get("type").and_then(|t| t.as_str()) {
                if !t.is_empty() && t != "text" {
                    return t.to_string();
                }
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------- 远程图片

/// 下载图片并转 data URI —— `downloadAsDataURI`（`media.go:495-513`）。
pub async fn download_as_data_uri(cl: &reqwest::Client, url: &str) -> Result<String, String> {
    let resp = cl
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let mut mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if status != reqwest::StatusCode::OK {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let mut data = resp
        .bytes()
        .await
        .map_err(|e| e.to_string())?
        .to_vec();
    // Go 的 `io.LimitReader` 是**截断**（不报错）
    if data.len() > MAX_IMAGE_BYTES {
        data.truncate(MAX_IMAGE_BYTES);
    }
    if mime.is_empty() || !mime.starts_with("image/") {
        mime = sniff_image_mime(&data);
    }
    use base64::Engine as _;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&data)
    ))
}

/// 按魔术字节推断图片 MIME —— `sniffImageMIME`（`media.go:516-530`）。
pub fn sniff_image_mime(data: &[u8]) -> String {
    if data.len() >= 4 {
        if data[0] == 0x89 && data[1] == b'P' && data[2] == b'N' && data[3] == b'G' {
            return "image/png".to_string();
        }
        if data[0] == 0xFF && data[1] == 0xD8 {
            return "image/jpeg".to_string();
        }
        if data[0] == b'G' && data[1] == b'I' && data[2] == b'F' {
            return "image/gif".to_string();
        }
        if data[0] == b'R' && data[1] == b'I' && data[2] == b'F' && data[3] == b'F' {
            return "image/webp".to_string();
        }
    }
    "application/octet-stream".to_string()
}

/// 扫描 messages 中 `image_url` 的远程图片，下载后转 base64 data URI 就地替换
/// —— `FetchRemoteImages`（`media.go:440-492`）。返回 `(body, 替换条数)`。
pub async fn fetch_remote_images(cl: &reqwest::Client, body: &[u8]) -> (Vec<u8>, usize) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (body.to_vec(), 0);
    };
    let Some(obj) = v.as_object() else {
        return (body.to_vec(), 0);
    };
    let Some(msgs) = obj.get("messages").and_then(|m| m.as_array()) else {
        return (body.to_vec(), 0);
    };
    let mut msgs = msgs.clone();
    let mut count = 0usize;
    for raw in msgs.iter_mut() {
        let Some(msg) = raw.as_object_mut() else {
            continue;
        };
        let Some(parts) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        for p in parts.iter_mut() {
            let Some(pm) = p.as_object_mut() else {
                continue;
            };
            if pm.get("type").and_then(|t| t.as_str()) != Some("image_url") {
                continue;
            }
            let Some(iu) = pm.get_mut("image_url").and_then(|i| i.as_object_mut()) else {
                continue;
            };
            let url = iu
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                continue; // base64 data URI 或其他：跳过
            }
            match download_as_data_uri(cl, &url).await {
                Ok(data_uri) => {
                    iu.insert("url".to_string(), serde_json::Value::String(data_uri));
                    count += 1;
                }
                Err(e) => {
                    crate::logf(&format!("[tuanjie] fetch-remote-img {url} err={e}"));
                    continue; // 下载失败保留原 URL，让上游自行处理
                }
            }
        }
    }
    if count == 0 {
        return (body.to_vec(), 0);
    }
    let mut root = obj.clone();
    root.insert("messages".to_string(), serde_json::Value::Array(msgs));
    (
        crate::gojson::marshal_go_value(&serde_json::Value::Object(root)),
        count,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_vision_capable_is_case_insensitive() {
        for m in ["codely-vl", "KIMI-K3", "glm-5.3-flash", "DeepSeek-V4.1-Flash", "CODELY-VL"] {
            assert!(is_builtin_vision_capable(m), "m={m}");
        }
        for m in ["codely-core", "my-vl", "", "image-model-x"] {
            assert!(!is_builtin_vision_capable(m), "m={m}");
        }
    }

    #[test]
    fn multimodal_reject_only_for_non_vision_models_with_media() {
        let img = br#"{"model":"codely-core","messages":[{"content":[{"type":"text","text":"x"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}]}]}"#;
        let msg = multimodal_reject_message(img, "codely-core").expect("应拒绝");
        assert!(msg.contains("codely-vl"), "{msg}");
        assert!(msg.contains("KIMI-K3"), "{msg}");
        assert!(msg.contains("image_url"), "{msg}");

        // 内置多模态模型直收（大小写不敏感），不改写、不拒绝
        for m in ["codely-vl", "kimi-k3", "GLM-5.3-FLASH", "DeepSeek-V4.1-Flash"] {
            assert_eq!(multimodal_reject_message(img, m), None, "m={m}");
        }
        // 纯文本请求不拒绝
        let text = br#"{"model":"codely-core","messages":[{"content":"hi"}]}"#;
        assert_eq!(multimodal_reject_message(text, "codely-core"), None);
        // 音频同样触发
        let audio = br#"{"model":"codely-core","messages":[{"content":[{"type":"input_audio"}]}]}"#;
        assert!(multimodal_reject_message(audio, "codely-core")
            .expect("应拒绝")
            .contains("input_audio"));
        // 非 JSON / 无 messages：不拒绝（保持旧 DetectMediaType 的宽容语义）
        assert_eq!(multimodal_reject_message(b"oops", "codely-core"), None);
    }

    #[test]
    fn detect_media_type_returns_first_non_text_block() {
        assert_eq!(detect_media_type(br#"{"messages":[{"content":"x"},{"content":[{"type":"input_audio"}]}]}"#), "input_audio");
        assert_eq!(detect_media_type(br#"{"messages":[{"content":[{"type":"text"}]}]}"#), "");
        assert_eq!(detect_media_type(br#"{"messages":[]}"#), "");
        assert_eq!(detect_media_type(b"oops"), "");
    }

    #[test]
    fn sniff_image_mime_matches_go() {
        assert_eq!(sniff_image_mime(&[0x89, b'P', b'N', b'G']), "image/png");
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 1, 2]), "image/jpeg");
        assert_eq!(sniff_image_mime(b"GIF89a"), "image/gif");
        assert_eq!(sniff_image_mime(b"RIFFxxxx"), "image/webp");
        assert_eq!(sniff_image_mime(b"xxxx"), "application/octet-stream");
        assert_eq!(sniff_image_mime(b""), "application/octet-stream");
    }

    #[test]
    fn should_fallback_only_for_transient_failures() {
        assert!(should_fallback(-1));
        assert!(should_fallback(429));
        assert!(should_fallback(500));
        assert!(should_fallback(503));
        assert!(!should_fallback(0));
        assert!(!should_fallback(400));
        assert!(!should_fallback(404));
    }
}
