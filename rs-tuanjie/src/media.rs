//! media.rs —— 媒体改路由 + Agnes 生图/生视频合同（`internal/tuanjie/media.go` +
//! `server.go:1569-1763` 的图片/视频端点 + `handlers_extra.go:294-374` 的两个配置端点）。
//!
//! ## 交付面
//!
//! * **媒体配置**：`tuanjie-media.json`（`vision`/`image`/`video`/`vision_capable`/
//!   `vision_fallback`）+ 旧 `tuanjie-vision.json` 只读迁移；`MediaState` 持有全部取值，
//!   `vision` 与旧机制双视图（`/vision-config` 内部转到同一机制）。
//! * **改路由**：[`RerouteIfMedia`]（含图片/音频等内容块且目标模型不识图 → 改写 model）、
//!   [`RewriteImageModel`]（生图统一改写）、[`DetectMediaType`]、
//!   [`VisionCapable`]（内置清单 ∪ 用户扩展 ∪ 识图模型本身）、
//!   [`VisionFallbackChain`]（识图 → 用户回落链 → `codely-vl` 兜尾，去重保序）、
//!   [`shouldFallback`]（网络错/5xx/429 才值得换源）。
//! * **schema 修复**：[`sanitizeForExternal`]（补 `tool_call_id`，学群友
//!   `_sanitize_for_external`）。
//! * **远程图片预下载**：[`FetchRemoteImages`] + `data:` URI 转换 + 魔术字节嗅探。
//! * **端点**：`/vision-config`、`/media-config`、`/v1/images/generations`、
//!   `/v1/videos`（POST）、`/v1/videos/{id}` 与 `/agnesapi`（GET，走 provider 取任务）。
//!
//! **口径**：所有就地改写都走 Go `json.Unmarshal`→`map[string]any`→`json.Marshal`
//! 的同一条路（键按字节序、数字按 float64 的 strconv 口径、RawMessage 先 compact），
//! 用 [`crate::gojson::marshal_go_value`] 保证与 Go 逐字节一致。

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::server::{gobj, query_param, write_err, write_json, AppState, GJ, MAX_BODY_BYTES};

/// 团结的视觉模型（唯一识图入口）—— `media.go:25`。
pub const DEFAULT_VISION_MODEL: &str = "codely-vl";

/// 媒体模型统一配置文件 —— `media.go:186`。
pub const MEDIA_CONFIG_FILE: &str = "tuanjie-media.json";

/// 旧视觉模型配置文件（只读迁移，保留不删）—— `media.go:181`。
pub const VISION_CONFIG_FILE: &str = "tuanjie-vision.json";

/// 内置原生识图模型清单 —— `builtinVisionCapable`（`media.go:109-114`）。
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

/// 视频任务查询响应体上限 —— `io.LimitReader(resp.Body, 8<<20)`（`server.go:1759`）。
pub const MAX_VIDEO_QUERY_BYTES: usize = 8 << 20;

// ---------------------------------------------------------------- 纯函数

/// 按模型名判定类别 —— `ModelKind`（`media.go:47-56`）：含 `image` → image；
/// 含 `video` → video；其余 → chat（都**不分大小写**）。
pub fn model_kind(model: &str) -> &'static str {
    let l = model.to_lowercase();
    if l.contains("image") {
        "image"
    } else if l.contains("video") {
        "video"
    } else {
        "chat"
    }
}

/// 是否值得回落重试 —— `shouldFallback`（`media.go:175-177`）：
/// 网络错（-1）/ 5xx / 429 是瞬时故障；4xx 配置错原样透传。
pub fn should_fallback(status: i32) -> bool {
    status == -1 || status >= 500 || status == 429
}

/// 单个 chat 内容块 → 生存图改写后的 body —— `RewriteImageModel`（`media.go:241-268`）。
/// 返回 `(body, 改写说明)`；未改写时说明为空串。
pub fn rewrite_image_model(body: &[u8], image_model: &str) -> (Vec<u8>, String) {
    if image_model.is_empty() {
        return (body.to_vec(), String::new());
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (body.to_vec(), String::new());
    };
    let Some(model) = v.get("model").and_then(|m| m.as_str()) else {
        return (body.to_vec(), String::new());
    };
    if model.is_empty() || model_kind(model) != "image" || model == image_model {
        return (body.to_vec(), String::new());
    }
    let Some(obj) = v.as_object() else {
        return (body.to_vec(), String::new());
    };
    let mut m = obj.clone();
    m.insert(
        "model".to_string(),
        serde_json::Value::String(image_model.to_string()),
    );
    let nb = crate::gojson::marshal_go_value(&serde_json::Value::Object(m));
    (nb, format!("{model}→{image_model}"))
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

/// 修复客户端历史消息里缺失的 `tool_call_id` —— `sanitizeForExternal`（`media.go:363-431`）。
/// 无 messages 或解析失败时原样返回。
pub fn sanitize_for_external(body: &[u8]) -> Vec<u8> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };
    let Some(obj) = v.as_object() else {
        return body.to_vec();
    };
    let Some(msgs) = obj.get("messages").and_then(|m| m.as_array()).cloned() else {
        return body.to_vec();
    };
    let mut pending_ids: Vec<String> = Vec::new();
    let mut out_msgs: Vec<serde_json::Value> = Vec::with_capacity(msgs.len());
    for raw in msgs {
        let Some(msg) = raw.as_object() else {
            out_msgs.push(raw);
            continue;
        };
        let mut msg = msg.clone();
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        if role == "assistant" {
            let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()).cloned() else {
                out_msgs.push(serde_json::Value::Object(msg));
                continue;
            };
            let mut new_tcs: Vec<serde_json::Value> = Vec::with_capacity(tcs.len());
            for raw_tc in tcs {
                let Some(tc) = raw_tc.as_object() else {
                    new_tcs.push(raw_tc);
                    continue;
                };
                let mut tc = tc.clone();
                let mut cid = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                if cid.is_empty() {
                    cid = format!("call_{}", crate::inflight::next_rid());
                    tc.insert("id".to_string(), serde_json::Value::String(cid.clone()));
                }
                pending_ids.push(cid);
                new_tcs.push(serde_json::Value::Object(tc));
            }
            msg.insert("tool_calls".to_string(), serde_json::Value::Array(new_tcs));
            out_msgs.push(serde_json::Value::Object(msg));
        } else if role == "tool" {
            let tcid = msg
                .get("tool_call_id")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if !tcid.is_empty() {
                out_msgs.push(serde_json::Value::Object(msg));
                continue;
            }
            if !pending_ids.is_empty() {
                let id = pending_ids.remove(0);
                msg.insert("tool_call_id".to_string(), serde_json::Value::String(id));
                out_msgs.push(serde_json::Value::Object(msg));
                continue;
            }
            // 无对应调用：降级为 user 文本消息
            let text = match msg.get("content") {
                Some(serde_json::Value::Array(parts)) => {
                    let texts: Vec<String> = parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .filter(|t| !t.is_empty())
                        .map(|t| t.to_string())
                        .collect();
                    texts.join(" ")
                }
                Some(serde_json::Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            msg.insert(
                "role".to_string(),
                serde_json::Value::String("user".to_string()),
            );
            msg.insert(
                "content".to_string(),
                serde_json::Value::String(format!("[tool result] {text}")),
            );
            msg.remove("tool_call_id");
            out_msgs.push(serde_json::Value::Object(msg));
        } else {
            out_msgs.push(serde_json::Value::Object(msg));
        }
    }
    let mut root = obj.clone();
    root.insert(
        "messages".to_string(),
        serde_json::Value::Array(out_msgs),
    );
    crate::gojson::marshal_go_value(&serde_json::Value::Object(root))
}

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

/// Go `url.QueryEscape` 的等价物（空格 → `+`，保 `A-Za-z0-9-_.~`）。
pub fn go_query_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------- MediaState

struct MediaInner {
    vision: String,
    image: String,
    video: String,
    vision_capable: Vec<String>,
    vision_fallback: Vec<String>,
}

/// 媒体模型状态（识图/生图/生视频 + 用户扩展识图清单 + 回落链）。
///
/// Go 侧是包级全局（`visionModel` / `mediaCfg` / `visionCapableExtra` /
/// `visionFallbackExtra`）；Rust 收进 `AppState` 一份，语义与读写时机逐条相同
/// （`NewServer` 时 `LoadMediaConfig()`，端点读写同一份）。
pub struct MediaState {
    inner: RwLock<MediaInner>,
    media_path: Option<PathBuf>,
    vision_path: Option<PathBuf>,
}

impl MediaState {
    /// 生产构造：数据目录的两个配置文件（`LoadMediaConfig` = `LoadVisionConfig` +
    /// media.json 覆盖）。
    pub fn new() -> Self {
        Self::with_paths(
            Some(crate::pacing::data_path(MEDIA_CONFIG_FILE)),
            Some(crate::pacing::data_path(VISION_CONFIG_FILE)),
        )
    }

    /// 显式路径（`None` = 不读不写盘；**绝不碰真实数据目录**）。
    pub fn with_paths(media_path: Option<PathBuf>, vision_path: Option<PathBuf>) -> Self {
        let ms = MediaState {
            inner: RwLock::new(MediaInner {
                vision: String::new(),
                image: String::new(),
                video: String::new(),
                vision_capable: Vec::new(),
                vision_fallback: Vec::new(),
            }),
            media_path,
            vision_path,
        };
        ms.load();
        ms
    }

    fn lock(&self) -> std::sync::RwLockWriteGuard<'_, MediaInner> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, MediaInner> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// `LoadMediaConfig`（`media.go:193-217`）：旧 vision 机制先行（含旧文件回退），
    /// 再读 media.json 覆盖；缺失/非法 = 全部默认值。
    pub fn load(&self) {
        self.load_vision_config();
        let Some(path) = &self.media_path else {
            return;
        };
        let Ok(raw) = std::fs::read(path) else {
            return;
        };
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Cfg {
            vision: String,
            image: String,
            video: String,
            vision_capable: Vec<String>,
            vision_fallback: Vec<String>,
        }
        let Ok(cfg) = serde_json::from_slice::<Cfg>(&raw) else {
            return;
        };
        {
            let mut g = self.lock();
            g.vision = cfg.vision.trim().to_string();
            g.image = cfg.image.trim().to_string();
            g.video = cfg.video.trim().to_string();
            g.vision_capable = cfg.vision_capable;
            g.vision_fallback = cfg.vision_fallback;
        }
    }

    /// `LoadVisionConfig`（`media.go:299-311`）：旧文件只读迁移。
    fn load_vision_config(&self) {
        let Some(path) = &self.vision_path else {
            return;
        };
        let Ok(raw) = std::fs::read(path) else {
            return;
        };
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Cfg {
            model: String,
        }
        let Ok(cfg) = serde_json::from_slice::<Cfg>(&raw) else {
            return;
        };
        if cfg.model.is_empty() {
            return;
        }
        self.set_vision(&cfg.model);
    }

    /// `VisionModel()`（`media.go:59-66`）：空值回退默认。
    pub fn vision(&self) -> String {
        let v = self.read().vision.clone();
        if v.is_empty() {
            DEFAULT_VISION_MODEL.to_string()
        } else {
            v
        }
    }

    /// `SetVisionModel`（`media.go:69-76`）：空串回退默认值。
    pub fn set_vision(&self, m: &str) {
        let mut g = self.lock();
        let m = m.trim();
        g.vision = if m.is_empty() {
            DEFAULT_VISION_MODEL.to_string()
        } else {
            m.to_string()
        };
    }

    /// `ImageModel()` —— 空 = 不改写。
    pub fn image(&self) -> String {
        self.read().image.clone()
    }

    /// `SetImageModel`（空串 = 清空回到默认「不改写」）。
    pub fn set_image(&self, m: &str) {
        self.lock().image = m.trim().to_string();
    }

    /// `VideoModel()` —— 空 = 不改写。
    pub fn video(&self) -> String {
        self.read().video.clone()
    }

    /// `SetVideoModel`。
    pub fn set_video(&self, m: &str) {
        self.lock().video = m.trim().to_string();
    }

    /// `VisionCapable`（`media.go:120-141`）：识图模型本身 ∪ 内置清单 ∪ 用户扩展，
    /// **大小写不敏感**。
    pub fn vision_capable(&self, model: &str) -> bool {
        if model.is_empty() {
            return false;
        }
        let vision = self.vision();
        if model.eq_ignore_ascii_case(&vision) {
            return true;
        }
        for m in BUILTIN_VISION_CAPABLE {
            if model.eq_ignore_ascii_case(m) {
                return true;
            }
        }
        self.read()
            .vision_capable
            .iter()
            .any(|m| model.eq_ignore_ascii_case(m))
    }

    /// `VisionFallbackChain`（`media.go:149-170`）：识图 → 用户回落链 → codely-vl
    /// 兜尾，**去重保序、去空**。
    pub fn vision_fallback_chain(&self) -> Vec<String> {
        let g = self.read();
        let mut chain: Vec<String> = vec![self.vision()];
        chain.extend(g.vision_fallback.iter().cloned());
        chain.push(DEFAULT_VISION_MODEL.to_string());
        let mut out: Vec<String> = Vec::new();
        for m in chain {
            if m.is_empty() {
                continue;
            }
            if out.iter().any(|e| e.eq_ignore_ascii_case(&m)) {
                continue;
            }
            out.push(m);
        }
        out
    }

    /// 用户扩展识图清单（`vision_capable`）。
    pub fn vision_capable_extra(&self) -> Vec<String> {
        self.read().vision_capable.clone()
    }

    /// 用户回落链（`vision_fallback`）。
    pub fn vision_fallback_extra(&self) -> Vec<String> {
        self.read().vision_fallback.clone()
    }

    /// `SaveMediaConfig`（`media.go:220-236`）：三类模型 + 扩展清单 + 回落链。
    pub fn save_media_config(&self) -> Result<(), String> {
        let Some(path) = self.media_path.clone() else {
            return Ok(());
        };
        let extra = self.vision_capable_extra();
        let fb = self.vision_fallback_extra();
        let value = serde_json::json!({
            "vision": self.vision(),
            "image": self.image(),
            "video": self.video(),
            "vision_capable": extra,
            "vision_fallback": fb,
        });
        // Go 是 `json.MarshalIndent(map[string]any, "", "  ")`（键按字节序、两空格缩进）
        let raw = marshal_indent_go(&value);
        std::fs::write(&path, raw).map_err(|e| e.to_string())
    }

    /// `SaveVisionConfig`（`media.go:315-324`）：旧文件 + 同步写 media.json
    /// （一个机制两个视图）。
    pub fn save_vision_config(&self) -> Result<(), String> {
        if let Some(path) = self.vision_path.clone() {
            let raw = format!(
                "{{\n  \"model\": {}\n}}",
                crate::gojson::go_string(&self.vision())
            );
            std::fs::write(&path, raw).map_err(|e| e.to_string())?;
        }
        self.save_media_config()
    }

    /// `RerouteIfMedia`（`media.go:328-355`）：含媒体内容且目标模型不识图时，
    /// 就地改写 body 的 model。返回 `(body, 说明)`；未改写说明为空串。
    pub fn reroute_if_media(&self, body: &[u8]) -> (Vec<u8>, String) {
        let vision = self.vision();
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
            return (body.to_vec(), String::new());
        };
        let Some(model) = v.get("model").and_then(|m| m.as_str()) else {
            return (body.to_vec(), String::new());
        };
        if model.is_empty() {
            return (body.to_vec(), String::new());
        }
        if self.vision_capable(model) {
            return (body.to_vec(), String::new()); // 原生识图模型：直接放行
        }
        let media = detect_media_type(body);
        if media.is_empty() {
            return (body.to_vec(), String::new());
        }
        let Some(obj) = v.as_object() else {
            return (body.to_vec(), String::new());
        };
        let mut m = obj.clone();
        m.insert("model".to_string(), serde_json::Value::String(vision.clone()));
        let nb = crate::gojson::marshal_go_value(&serde_json::Value::Object(m));
        (
            nb,
            format!("{model}→{vision}（检测到 {media}）"),
        )
    }
}

/// `json.MarshalIndent(v, "", "  ")` 的等价物（键按字节序、两空格缩进、无末尾换行）。
pub fn marshal_indent_go(v: &serde_json::Value) -> Vec<u8> {
    // serde_json 的 pretty 是 2 空格缩进；但空数组/空对象的写法与 Go 不同
    // （Go：`[]` / `{}`；serde：`[]` / `{}` 同样）——直接用 serde 的 pretty。
    // 注意数字形态：serde 的 `to_string_pretty` 会保留 f64 的 `1.0`，Go 写 `1`
    // ——故先把 Value 规范化（走 gojson 的 float 口径）再手写缩进。
    let mut out = Vec::new();
    write_indent_go(&mut out, v, 0);
    out
}

fn write_indent_go(out: &mut Vec<u8>, v: &serde_json::Value, depth: usize) {
    let pad = |out: &mut Vec<u8>, d: usize| {
        for _ in 0..d {
            out.extend_from_slice(b"  ");
        }
    };
    match v {
        serde_json::Value::Object(m) if !m.is_empty() => {
            out.push(b'{');
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.push(b'\n');
                pad(out, depth + 1);
                crate::gojson::write_go_string(out, k);
                out.extend_from_slice(b": ");
                write_indent_go(out, val, depth + 1);
            }
            out.push(b'\n');
            pad(out, depth);
            out.push(b'}');
        }
        serde_json::Value::Array(a) if !a.is_empty() => {
            out.push(b'[');
            for (i, val) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.push(b'\n');
                pad(out, depth + 1);
                write_indent_go(out, val, depth + 1);
            }
            out.push(b'\n');
            pad(out, depth);
            out.push(b']');
        }
        // 标量与空容器：走 gojson 的紧凑口径（数字按 Go 的 float 形态）
        other => out.extend_from_slice(&crate::gojson::marshal_go_value(other)),
    }
}

// ---------------------------------------------------------------- 远程图片

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

// ---------------------------------------------------------------- 端点

/// `GET|POST /vision-config`（`handleVisionConfig`，`handlers_extra.go:297-320`）。
pub async fn handle_vision_config(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if req.method() == Method::POST {
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap_or_default();
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Req {
            model: String,
        }
        // Go 的 `Decode` 读第一个值、顶层须是对象（数组/串一律报错 ⇒ 400）
        let req = decode_go_object::<Req>(&body);
        let model = req.map(|r| r.model).unwrap_or_default();
        if model.is_empty() {
            return write_err(StatusCode::BAD_REQUEST, "body 需为 {\"model\":\"...\"}");
        }
        if model_kind(&model) != "chat" {
            return write_err(
                StatusCode::BAD_REQUEST,
                &format!(
                    "识图模型需为理解类（chat）模型，{} 是 {} 类，请走对应的选择器",
                    model,
                    model_kind(&model)
                ),
            );
        }
        st.media.set_vision(&model);
        if let Err(e) = st.media.save_vision_config() {
            return write_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("持久化失败: {e}"));
        }
        crate::logf(&format!("[tuanjie] vision model 配置更新 model={model}"));
    }
    write_json(&gobj(vec![("model", GJ::Str(st.media.vision()))]))
}

/// `GET|POST /media-config`（`handleMediaConfig`，`handlers_extra.go:326-374`）。
pub async fn handle_media_config(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if req.method() == Method::POST {
        let body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
            .await
            .unwrap_or_default();
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Req {
            vision: Option<String>,
            image: Option<String>,
            video: Option<String>,
        }
        let Some(r) = decode_go_object::<Req>(&body) else {
            return write_err(
                StatusCode::BAD_REQUEST,
                "body 需为 {\"vision\"|\"image\"|\"video\"}（可只传变化项）",
            );
        };
        if r.vision.is_none() && r.image.is_none() && r.video.is_none() {
            return write_err(
                StatusCode::BAD_REQUEST,
                "body 需为 {\"vision\"|\"image\"|\"video\"}（可只传变化项）",
            );
        }
        if let Some(v) = r.vision.as_deref() {
            if !v.is_empty() && model_kind(v) != "chat" {
                return write_err(
                    StatusCode::BAD_REQUEST,
                    &format!("识图选择器需为理解类（chat）模型，{v} 是 {} 类", model_kind(v)),
                );
            }
        }
        if let Some(v) = r.image.as_deref() {
            if !v.is_empty() && model_kind(v) != "image" {
                return write_err(
                    StatusCode::BAD_REQUEST,
                    &format!("生图选择器需为生图类（image）模型，{v} 是 {} 类", model_kind(v)),
                );
            }
        }
        if let Some(v) = r.video.as_deref() {
            if !v.is_empty() && model_kind(v) != "video" {
                return write_err(
                    StatusCode::BAD_REQUEST,
                    &format!("生视频选择器需为视频类（video）模型，{v} 是 {} 类", model_kind(v)),
                );
            }
        }
        if let Some(v) = r.vision.as_deref() {
            st.media.set_vision(v);
        }
        if let Some(v) = r.image.as_deref() {
            st.media.set_image(v);
        }
        if let Some(v) = r.video.as_deref() {
            st.media.set_video(v);
        }
        if let Err(e) = st.media.save_media_config() {
            return write_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("持久化失败: {e}"));
        }
        crate::logf(&format!(
            "[tuanjie] media config 配置更新 vision={} image={} video={}",
            st.media.vision(),
            st.media.image(),
            st.media.video()
        ));
    }
    // `vision_fallback`：回落链去掉首个（= vision 本身）—— `handlers_extra.go:372`
    let chain = st.media.vision_fallback_chain();
    let tail: Vec<GJ> = chain
        .iter()
        .skip(1)
        .map(|m| GJ::Str(m.clone()))
        .collect();
    write_json(&gobj(vec![
        ("vision", GJ::Str(st.media.vision())),
        ("image", GJ::Str(st.media.image())),
        ("video", GJ::Str(st.media.video())),
        ("vision_fallback", GJ::Arr(tail)),
    ]))
}

/// `POST /v1/images/generations`（`handleImagesGenerations`，`server.go:1574-1625`）。
pub async fn handle_images_generations(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if req.method() != Method::POST {
        return crate::server::method_not_allowed(None);
    }
    let mut body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap_or_default()
        .to_vec();
    let mut model = parse_model_field(&body);
    if model_kind(&model) == "video" {
        return json_error(
            StatusCode::BAD_REQUEST,
            &format!(
                "video 模型不支持生图端点，请走 POST /v1/videos（异步任务制，返回 task_id/video_id 后用 GET /v1/videos/{{id}} 取结果）"
            ),
            None,
        );
    }
    let (nb, note) = rewrite_image_model(&body, &st.media.image());
    if !note.is_empty() {
        body = nb;
        model = st.media.image();
        crate::logf(&format!("[tuanjie] images model 改写 {note}"));
    }
    let Some(prov) = st.providers.match_model(&model) else {
        return json_error(
            StatusCode::NOT_FOUND,
            &format!("unknown model for images endpoint: {model}（需先在外部账号里配置该模型的 provider）"),
            Some("model_not_found"),
        );
    };
    match crate::providers::forward_external(&st, body, &model, false, &prov, "/images/generations")
        .await
    {
        Ok(resp) => resp,
        Err(st_code) => {
            if st_code == -1 {
                write_json(&gobj(vec![(
                    "error",
                    gobj(vec![
                        (
                            "message",
                            GJ::Str(format!(
                                "{} 生图请求失败（网络错误，详见实时动态）",
                                prov.name
                            )),
                        ),
                        ("type", GJ::Str("server_error".to_string())),
                    ]),
                )]))
            } else {
                write_err(
                    StatusCode::from_u16(st_code as u16).unwrap_or(StatusCode::BAD_GATEWAY),
                    &format!(
                        "{} 生图请求失败（HTTP {}，限流或暂时故障），请稍后重试",
                        prov.name, st_code
                    ),
                )
            }
        }
    }
}

/// `POST /v1/videos`（`handleVideoCreate`，`server.go:1631-1688`）。
pub async fn handle_video_create(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Response {
    if req.method() != Method::POST {
        return crate::server::method_not_allowed(None);
    }
    let mut body = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .unwrap_or_default()
        .to_vec();
    let mut model = parse_model_field(&body);
    if model_kind(&model) != "video" {
        return json_error(
            StatusCode::BAD_REQUEST,
            &format!(
                "videos 端点仅接受 video 类模型（名字含 video），当前: {model}；生图请走 POST /v1/images/generations"
            ),
            None,
        );
    }
    let vm = st.media.video();
    if !vm.is_empty() && model != vm {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
            if let Some(obj) = v.as_object() {
                let mut m = obj.clone();
                m.insert("model".to_string(), serde_json::Value::String(vm.clone()));
                body = crate::gojson::marshal_go_value(&serde_json::Value::Object(m));
                crate::logf(&format!("[tuanjie] videos model 改写 {model}→{vm}"));
                model = vm;
            }
        }
    }
    let Some(prov) = st.providers.match_model(&model) else {
        return json_error(
            StatusCode::NOT_FOUND,
            &format!("unknown model for videos endpoint: {model}（需先在外部账号里配置该模型的 provider）"),
            Some("model_not_found"),
        );
    };
    match crate::providers::forward_external(&st, body, &model, false, &prov, "/videos").await {
        Ok(resp) => resp,
        Err(code) => {
            if code == -1 {
                write_json(&gobj(vec![(
                    "error",
                    gobj(vec![
                        (
                            "message",
                            GJ::Str(format!(
                                "{} 视频请求失败（网络错误，详见实时动态）",
                                prov.name
                            )),
                        ),
                        ("type", GJ::Str("server_error".to_string())),
                    ]),
                )]))
            } else {
                write_err(
                    StatusCode::from_u16(code as u16).unwrap_or(StatusCode::BAD_GATEWAY),
                    &format!(
                        "{} 视频任务创建失败（HTTP {}，限流或暂时故障），请稍后重试",
                        prov.name, code
                    ),
                )
            }
        }
    }
}

/// `GET /v1/videos/{id}` 与 `GET /agnesapi?video_id=`（`handleVideoQuery`，`server.go:1698-1763`）。
pub async fn handle_video_query(
    State(st): State<Arc<AppState>>,
    uri: Uri,
    req: axum::extract::Request,
) -> Response {
    if req.method() != Method::GET {
        return crate::server::method_not_allowed(None);
    }
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let mut model_name = query_param(&query, "model_name").unwrap_or_default();
    if model_name.is_empty() {
        model_name = st.media.video();
    }
    let video_id: String;
    let target: String;
    if let Some(rest) = path.strip_prefix("/v1/videos/") {
        if rest.is_empty() || rest.contains('/') {
            return write_err(StatusCode::BAD_REQUEST, "路径需为 /v1/videos/{id}");
        }
        video_id = rest.to_string();
        let mut t = format!("/videos/{video_id}");
        if !model_name.is_empty() {
            t.push_str("?model_name=");
            t.push_str(&go_query_escape(&model_name));
        }
        target = t;
    } else if let Some(vid) = query_param(&query, "video_id") {
        if vid.is_empty() {
            return write_err(StatusCode::BAD_REQUEST, "需带 video_id（/v1/videos/{id} 或 ?video_id=）");
        }
        video_id = vid.clone();
        let mut t = format!("/agnesapi?video_id={}", go_query_escape(&vid));
        if !model_name.is_empty() {
            t.push_str("&model_name=");
            t.push_str(&go_query_escape(&model_name));
        }
        target = t;
    } else {
        return write_err(StatusCode::BAD_REQUEST, "需带 video_id（/v1/videos/{id} 或 ?video_id=）");
    }
    let provs = st.providers.list();
    if provs.is_empty() {
        return json_error(
            StatusCode::NOT_FOUND,
            &format!("未配置外部 provider，无法查询视频任务 {video_id}"),
            Some("provider_not_found"),
        );
    }
    let prov = provs[0].clone();
    let url = format!("{}{}", crate::providers::external_base_url(&prov.base_url), target);
    let cl = match reqwest::Client::builder().timeout(Duration::from_secs(60)).build() {
        Ok(c) => c,
        Err(e) => {
            return write_err(StatusCode::BAD_GATEWAY, &format!("{} 请求失败: {e}", prov.name))
        }
    };
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", prov.api_key)) {
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    headers.insert(reqwest::header::ACCEPT, HeaderValue::from_static("application/json"));
    let resp = match cl.get(&url).headers(headers).send().await {
        Ok(r) => r,
        Err(e) => {
            return write_err(StatusCode::BAD_GATEWAY, &format!("{} 请求失败: {e}", prov.name))
        }
    };
    let status = resp.status();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = crate::chat::read_body_bounded(resp, MAX_VIDEO_QUERY_BYTES).await;
    let mut out = (
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
        Body::from(body),
    )
        .into_response();
    if let Ok(v) = HeaderValue::from_str(&ct) {
        out.headers_mut().insert(axum::http::header::CONTENT_TYPE, v);
    }
    out
}

/// 从 body 里取 `model` 字段（`json.Unmarshal(body, &struct{Model string})`）。
fn parse_model_field(body: &[u8]) -> String {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct Req {
        model: String,
    }
    decode_go_object::<Req>(body)
        .map(|r| r.model)
        .unwrap_or_default()
}

/// Go `json.NewDecoder(r.Body).Decode(&struct{…})` 的等价入口：读**第一个** JSON 值
/// （尾随数据容忍，Go 的 `Decode` 读完一个值就停），且顶层必须是**对象**。
///
/// 为什么要显式挡：serde 的派生实现接受"序列形式"的 struct——`["x"]` 会被解成第一个
/// 字段而不是报错，与 Go 的行为相反；`null` 在 Go 里对 struct 是 no-op、serde 会报错。
pub(crate) fn decode_go_object<T: serde::de::DeserializeOwned>(body: &[u8]) -> Option<T> {
    let v = crate::server::first_json_value(body)?;
    if !v.is_object() {
        return None;
    }
    serde_json::from_value::<T>(v).ok()
}

/// Go 的 `json.NewEncoder(w).Encode(map[string]any{"error":{...}})` 形态
/// （`Content-Type: application/json` + 指定状态码 + 末尾换行）。
fn json_error(status: StatusCode, message: &str, code: Option<&str>) -> Response {
    let mut inner: Vec<(&str, GJ)> = vec![
        ("message", GJ::Str(message.to_string())),
        ("type", GJ::Str("invalid_request_error".to_string())),
    ];
    if let Some(c) = code {
        inner.push(("code", GJ::Str(c.to_string())));
    }
    let body = gobj(vec![("error", gobj(inner))]);
    (
        status,
        [(axum::http::header::CONTENT_TYPE, crate::server::JSON_CONTENT_TYPE)],
        body.to_go_json_line(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_kind_is_case_insensitive_substring() {
        assert_eq!(model_kind("agnes-image-2.0"), "image");
        assert_eq!(model_kind("Agnes-Video-2.5"), "video");
        assert_eq!(model_kind("codely-core"), "chat");
        assert_eq!(model_kind(""), "chat");
        // image 优先于 video（Go 的顺序：先判 image）
        assert_eq!(model_kind("image-video-x"), "image");
    }

    #[test]
    fn rewrite_image_model_only_for_image_class() {
        let (out, note) = rewrite_image_model(br#"{"model":"agnes-image-9.9","prompt":"x"}"#, "agnes-image-2.0");
        assert_eq!(note, "agnes-image-9.9→agnes-image-2.0");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"model":"agnes-image-2.0","prompt":"x"}"#
        );
        // 空目标、非 image 类、已相等、非 JSON 一律原样
        let cases = [
            (br#"{"model":"agnes-image-9.9"}"#.as_slice(), ""),
            (br#"{"model":"codely-core"}"#.as_slice(), "agnes-image-2.0"),
            (br#"{"model":"agnes-image-2.0"}"#.as_slice(), "agnes-image-2.0"),
            (b"{not json".as_slice(), "agnes-image-2.0"),
        ];
        for (body, target) in cases {
            let (out, note) = rewrite_image_model(body, target);
            assert_eq!(out, body, "body={:?} target={target}", std::str::from_utf8(body));
            assert!(note.is_empty());
        }
    }

    #[test]
    fn detect_media_type_returns_first_non_text_block() {
        assert_eq!(detect_media_type(br#"{"messages":[{"content":"x"},{"content":[{"type":"input_audio"}]}]}"#), "input_audio");
        assert_eq!(detect_media_type(br#"{"messages":[{"content":[{"type":"text"}]}]}"#), "");
        assert_eq!(detect_media_type(br#"{"messages":[]}"#), "");
        assert_eq!(detect_media_type(b"oops"), "");
    }

    fn media_state(vision: &str, capable: &[&str], fallback: &[&str]) -> MediaState {
        let ms = MediaState::with_paths(None, None);
        ms.set_vision(vision);
        {
            let mut g = ms.lock();
            g.vision_capable = capable.iter().map(|s| s.to_string()).collect();
            g.vision_fallback = fallback.iter().map(|s| s.to_string()).collect();
        }
        ms
    }

    #[test]
    fn reroute_rewrites_only_for_non_vision_capable() {
        let ms = media_state("codely-vl", &[], &[]);
        let body = br#"{"model":"codely-core","messages":[{"content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA"}}]}]}"#;
        let (out, note) = ms.reroute_if_media(body);
        assert_eq!(note, "codely-core→codely-vl（检测到 image_url）");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(r#""model":"codely-vl""#), "{text}");
        // 内置识图清单放行（KIMI-K3 / GLM-5.3-FLASH / DeepSeek-V4.1-Flash / codely-vl）
        for m in ["KIMI-K3", "glm-5.3-flash", "DeepSeek-V4.1-Flash", "CODELY-VL"] {
            let b = format!(r#"{{"model":"{m}","messages":[{{"content":[{{"type":"image_url"}}]}}]}}"#);
            let (out, note) = ms.reroute_if_media(b.as_bytes());
            assert!(note.is_empty(), "m={m}");
            assert_eq!(out, b.as_bytes(), "m={m}");
        }
        // 无媒体内容 ⇒ 不改写
        let (out, note) = ms.reroute_if_media(br#"{"model":"codely-core","messages":[{"content":"hi"}]}"#);
        assert!(note.is_empty());
        assert_eq!(out, br#"{"model":"codely-core","messages":[{"content":"hi"}]}"#);
    }

    #[test]
    fn reroute_honours_user_extensions_and_fallback_chain() {
        let ms = media_state("my-vl", &["custom-vl"], &["codely-vl", "hidden-vl"]);
        assert!(ms.vision_capable("CUSTOM-VL"));
        // 回落链：vision → 用户链 → codely-vl 兜尾，去重保序
        assert_eq!(
            ms.vision_fallback_chain(),
            vec![
                "my-vl".to_string(),
                "codely-vl".to_string(),
                "hidden-vl".to_string(),
            ]
        );
        let ms2 = media_state("codely-vl", &[], &["a", "A", ""]);
        assert_eq!(
            ms2.vision_fallback_chain(),
            vec!["codely-vl".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn sanitize_for_external_fills_and_degrades_tool_ids() {
        // 缺 id 的 tool_call 补齐；后面的 tool 消息按序回填
        let body = br#"{"messages":[{"role":"assistant","tool_calls":[{"type":"function","function":{"name":"f","arguments":"{}"}}]},{"role":"tool","content":"r"}]}"#;
        let out = sanitize_for_external(body);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let cid = v["messages"][0]["tool_calls"][0]["id"].as_str().unwrap().to_string();
        assert!(cid.starts_with("call_"), "{cid}");
        assert_eq!(v["messages"][1]["tool_call_id"], serde_json::json!(cid));
        // 无对应调用 ⇒ 降级为 user 文本
        let body = br#"{"messages":[{"role":"tool","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}]}"#;
        let v: serde_json::Value = serde_json::from_slice(&sanitize_for_external(body)).unwrap();
        assert_eq!(v["messages"][0]["role"], serde_json::json!("user"));
        assert_eq!(v["messages"][0]["content"], serde_json::json!("[tool result] a b"));
        assert!(v["messages"][0].get("tool_call_id").is_none());
        // 已有 id / 无 messages / 非 JSON：原样
        // 已有 id：不改语义，但会走 Go 的「解成 map 再 Marshal」⇒ 键排序后的形态
        let ok = br#"{"messages":[{"role":"tool","tool_call_id":"x","content":"r"}]}"#;
        assert_eq!(
            String::from_utf8(sanitize_for_external(ok)).unwrap(),
            r#"{"messages":[{"content":"r","role":"tool","tool_call_id":"x"}]}"#
        );
        assert_eq!(sanitize_for_external(b"{not json"), b"{not json");
        assert_eq!(sanitize_for_external(br#"{"other":1}"#), br#"{"other":1}"#);
    }

    #[test]
    fn sniff_and_query_escape_match_go() {
        assert_eq!(sniff_image_mime(&[0x89, b'P', b'N', b'G']), "image/png");
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 1, 2]), "image/jpeg");
        assert_eq!(sniff_image_mime(b"GIF89a"), "image/gif");
        assert_eq!(sniff_image_mime(b"RIFFxxxx"), "image/webp");
        assert_eq!(sniff_image_mime(b"xxxx"), "application/octet-stream");
        assert_eq!(sniff_image_mime(b""), "application/octet-stream");
        // Go `url.QueryEscape`：空格 → `+`，保 `A-Za-z0-9-_.~`
        assert_eq!(go_query_escape("a b"), "a+b");
        assert_eq!(go_query_escape("x/y"), "x%2Fy");
        assert_eq!(go_query_escape("-_.~"), "-_.~");
        assert_eq!(go_query_escape("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn media_state_loads_and_saves_like_go() {
        let dir = std::env::temp_dir().join(format!(
            "rs-tuanjie-media-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let media = dir.join("tuanjie-media.json");
        let vision = dir.join("tuanjie-vision.json");
        // 旧文件只读迁移 + media.json 覆盖
        std::fs::write(&vision, br#"{"model":"old-vl"}"#).unwrap();
        std::fs::write(
            &media,
            br#"{"vision":"new-vl","image":"img-x","video":"vid-y","vision_capable":["c1"],"vision_fallback":["fb1"]}"#,
        )
        .unwrap();
        let ms = MediaState::with_paths(Some(media.clone()), Some(vision.clone()));
        assert_eq!(ms.vision(), "new-vl");
        assert_eq!(ms.image(), "img-x");
        assert_eq!(ms.video(), "vid-y");
        assert_eq!(ms.vision_capable_extra(), vec!["c1".to_string()]);
        assert_eq!(ms.vision_fallback_extra(), vec!["fb1".to_string()]);
        // 写回：键按字节序、两空格缩进（Go 的 MarshalIndent(map)）
        ms.save_media_config().expect("落盘");
        let raw = std::fs::read_to_string(&media).unwrap();
        assert_eq!(
            raw,
            "{\n  \"image\": \"img-x\",\n  \"video\": \"vid-y\",\n  \"vision\": \"new-vl\",\n  \
             \"vision_capable\": [\n    \"c1\"\n  ],\n  \"vision_fallback\": [\n    \"fb1\"\n  ]\n}"
        );
        // 旧文件兼容层：写旧文件 + 同步写新文件
        ms.save_vision_config().expect("落盘旧文件");
        assert_eq!(
            std::fs::read_to_string(&vision).unwrap(),
            "{\n  \"model\": \"new-vl\"\n}"
        );
        // 缺文件 ⇒ 默认值
        let ms2 = MediaState::with_paths(None, None);
        assert_eq!(ms2.vision(), DEFAULT_VISION_MODEL);
        assert_eq!(ms2.image(), "");
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn decode_go_object_rejects_seq_form_like_go() {
        #[derive(serde::Deserialize, Default)]
        #[serde(default)]
        struct Req {
            model: String,
        }
        assert_eq!(decode_go_object::<Req>(br#"{"model":"x"}"#).unwrap().model, "x");
        // 顶层不是对象 ⇒ None（Go 的 `Decode` 对数组/串报错；serde 的派生会接受序列形式）
        assert!(decode_go_object::<Req>(br#"["x"]"#).is_none());
        assert!(decode_go_object::<Req>(b"\"x\"").is_none());
        assert!(decode_go_object::<Req>(b"null").is_none());
        // 尾随数据容忍（Go 的 Decoder 读完一个值就停）
        assert_eq!(
            decode_go_object::<Req>(br#"{"model":"y"}x"#).unwrap().model,
            "y"
        );
        // 空体 / 非法 JSON ⇒ None
        assert!(decode_go_object::<Req>(b"").is_none());
        assert!(decode_go_object::<Req>(b"{not json").is_none());
    }
}
