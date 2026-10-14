// matrix.rs —— `GET /model/matrix`（GUI 专用旁路端点），逐条对齐 Go `internal/bai/models.go`。
//
// 为什么要有这个端点（照抄 Go 的设计说明）：`/v1/models` 是逐字节透传，客户端必须自带
// key；而 GUI 没有 key，直接透传只会拿到上游 401（矩阵永远空），且这个无 key 请求被 3 秒
// 轮询驱动，会持续刷 api.b.ai。本端点在服务端用渠道配置里的 key 拉一次清单并缓存，上游只在
// 三种时机被访问：冷启动首次、拉杆点火（?refresh=1）、手动 ↻（?refresh=1）。
//
// 对齐清单（Go 侧行为 → 本模块）：
//   * 方法：非 GET → 405 + 纯文本 `method not allowed` + `X-Content-Type-Options: nosniff`
//     （Go 走 `http.Error`，它固定带这两个响应头）
//   * Content-Type：`application/json`（Go `w.Header().Set` 不追加 `; charset=utf-8`）
//   * 缓存：**无 TTL**；空判用 `loaded` 而不是 count>0（上游合法返回空数组时不该每轮询都重打）
//   * 失败负缓存 60s（含 need_key），窗口内轮询不重试
//   * 失败保旧值：上游挂了但已有旧缓存 → 回旧数据 + source:"cache"，不清空不报错
//   * 端点失败也回 HTTP 200 + ok:false + err 码，不把「没配密钥」变成状态码
//   * 全程持锁 = single-flight：并发请求排队，第一个拉完后面的吃缓存
//   * key 从渠道配置 `tuanjie-water-channels.json` 里 `id=="bai"` 的条目取；**条目缺字段
//     按零值容忍**（Go 的 `json.Unmarshal` 对缺失字段补零），文件缺失/坏/无条目 → 空串
//   * 拉清单：15s 总预算、4MB 读入上限、非 200 即错、`data` 必须存在且是数组
//     （`data:null` 算存在——Go 的 `len(envelope.Data)==4`，视为成功、count=0）
//   * 响应体字段序 `ok,source,fetched_at,count,data,err`；`data` 按 Go 的
//     `appendCompact(data, true)` 语义嵌入：去 token 间空白 + 对 `<`/`>`/`&` 与
//     U+2028/U+2029 做 `\uXXXX` 转义（走 `compact_json_go`）
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::http::{header, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::transport::{fetch_matrix_upstream, RetryPolicy};

/// 渠道配置里 B.AI 的 id（与 Go `baiChannelID` 同值；与注水检测共用同一文件）。
pub const BAI_CHANNEL_ID: &str = "bai";
/// 失败负缓存窗口（与 Go `matrixErrTTL` 同值）。
pub const MATRIX_ERR_TTL_SECS: u64 = 60;
/// 拉清单的上游超时（与 Go `matrixTimeout` 同值）。这是一个**总预算**：
/// Go 是 `context.WithTimeout` 罩住整个拉取（含重试），Rust 侧同一口径见 `RetryPolicy`。
pub const MATRIX_TIMEOUT_SECS: u64 = 15;
/// 上游清单读入上限（与 Go `io.LimitReader(resp.Body, 4<<20)` 同值）。
pub const MATRIX_MAX_BYTES: usize = 4 << 20;

/// 渠道密钥条目（对应 Go `channelKeyCfg`）。
///
/// 字段是 `Option`：Go 的结构体字段**缺失时补零值**（`json.Unmarshal` 不报错），
/// 只有**类型不匹配**（如 `"id": 123`）才让整份解析失败。旧实现用 `String` 会让
/// 「任一条目缺 key」把整份配置判死，与 Go 分歧——第 3 轮对齐。
///
/// 数组元素在读取处包一层 `Option`：Go 对 `null` **元素**同样不报错（存成零值条目），
/// 而 serde 默认会让整个 `Vec<ChannelKeyCfg>` 解析失败——第 5 轮 R6 对齐。
#[derive(serde::Deserialize)]
struct ChannelKeyCfg {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

/// 从渠道配置里取 B.AI 的 Bearer；缺失/坏文件回空串（对应 Go `baiChannelKey`）。
///
/// 用 `crate::crypto::read_file`：旧明文原样读（读兼容），DPAPI 密文自动解密。
/// 失败一律回空串（Go 同口径——调用方看到空串就走 need_key 分支，不区分原因）。
pub fn channel_key(config_path: &std::path::Path) -> String {
    let Ok(b) = crate::crypto::read_file(config_path) else {
        return String::new();
    };
    // 与 Go 一样走一个结构体数组：类型不匹配 → 整份解析失败 → 空串。
    // 元素用 `Option`：`[null]` 这类元素 Go 容忍（存成零值、不报错），serde 默认不容忍，
    // 不包一层就会把「Go 能读到 key」的配置整份判死（第 5 轮 R6）。
    let Ok(cfgs) = serde_json::from_slice::<Vec<Option<ChannelKeyCfg>>>(&b) else {
        return String::new();
    };
    cfgs.into_iter()
        .flatten()
        .find(|c| c.id.as_deref() == Some(BAI_CHANNEL_ID))
        .map(|c| c.key.unwrap_or_default())
        .unwrap_or_default()
}

/// 矩阵缓存（对应 Go `matrixState`）。包级无 TTL：刷新时机由拉杆决定，不由时钟决定。
///
/// 锁用 `tokio::sync::Mutex` 而不是 `std::sync::Mutex`：与 Go 一样要**跨整次上游拉取**持锁
/// （single-flight），而 axum 的 handler future 必须 `Send`；tokio 的锁守卫是可 Send 的。
#[derive(Default)]
pub struct MatrixState {
    inner: tokio::sync::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    loaded: bool,
    data: Vec<u8>,
    count: usize,
    fetched_at: i64,
    err_kind: &'static str,
    err_at: Option<Instant>,
}

impl Inner {
    fn new() -> Self {
        Self {
            // 从未成功抓取时 Go 回的是 `time.Time{}.Unix()` = -62135596800（公元 1 年那一格）。
            // 不是 0——P1 黄金门禁第一次跑就抓到这个差异，故这里如实对齐。
            fetched_at: ZERO_TIME_UNIX,
            ..Default::default()
        }
    }
}

/// Go `time.Time{}.Unix()`（零值时间的 unix 秒）。
pub const ZERO_TIME_UNIX: i64 = -62_135_596_800;

/// 一次刷新的结果要点（供日志，**不含 key**）。
pub struct RefreshOutcome {
    pub ok: bool,
    pub count: usize,
    pub err_kind: &'static str,
    pub err_msg: String,
}

impl MatrixState {
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Mutex::new(Inner::new()),
        }
    }

    /// 取锁（tokio 锁不会中毒，无需要处理 poisoning）。
    async fn lock(&self) -> tokio::sync::MutexGuard<'_, Inner> {
        self.inner.lock().await
    }
    /// 拉一次上游清单（对应 Go `refreshLocked`，调用方须持锁）。
    /// 失败只记状态、不清旧值。
    async fn refresh_locked(
        inner: &mut Inner,
        key: &str,
        client: &reqwest::Client,
        models_url: &str,
    ) -> RefreshOutcome {
        if key.is_empty() {
            inner.err_kind = "need_key";
            inner.err_at = Some(Instant::now());
            return RefreshOutcome {
                ok: false,
                count: inner.count,
                err_kind: "need_key",
                err_msg: String::new(),
            };
        }
        let policy = RetryPolicy::matrix_default();
        match fetch_matrix_upstream(client, models_url, key, &policy).await {
            Ok((data, count)) => {
                inner.data = data;
                inner.count = count;
                inner.fetched_at = now_unix();
                inner.err_kind = "";
                inner.loaded = true;
                inner.err_at = None;
                RefreshOutcome {
                    ok: true,
                    count,
                    err_kind: "",
                    err_msg: String::new(),
                }
            }
            Err(e) => {
                inner.err_kind = "upstream_error";
                inner.err_at = Some(Instant::now());
                RefreshOutcome {
                    ok: false,
                    count: inner.count,
                    err_kind: "upstream_error",
                    err_msg: e,
                }
            }
        }
    }

    /// 处理一次 `/model/matrix` 请求（对应 Go `handleModelMatrix`）。
    pub async fn handle(
        &self,
        method: &Method,
        query: Option<&str>,
        config_path: &std::path::Path,
        client: &reqwest::Client,
        models_url: &str,
    ) -> Response {
        if method != Method::GET {
            // Go 走 `http.Error(w, "method not allowed", 405)`：纯文本 + nosniff。
            return (
                StatusCode::METHOD_NOT_ALLOWED,
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                "method not allowed\n",
            )
                .into_response();
        }
        let force = query_has_refresh_1(query);

        // 全程持锁 = single-flight。注意：这里跨 await 持锁，与 Go 的 `st.mu.Lock/defer Unlock`
        // 跨整次上游拉取完全同形——并发请求排队，第一个拉完后面的直接吃缓存。
        let mut st = self.lock().await;
        let mut fetched_now = false;
        if force || !st.loaded {
            let fresh_enough = match st.err_at {
                // Go: time.Since(st.errAt) >= matrixErrTTL（零值 errAt 时差值为极大的正数）
                None => true,
                Some(t) => t.elapsed().as_secs() >= MATRIX_ERR_TTL_SECS,
            };
            if force || fresh_enough {
                let key = channel_key(config_path);
                let outcome =
                    Self::refresh_locked(&mut st, &key, client, models_url).await;
                if outcome.ok {
                    println!("bai-plugin: 模型矩阵已刷新，{} 个模型", outcome.count);
                } else if !outcome.err_kind.is_empty() && !outcome.err_msg.is_empty() {
                    // 只报「失败 + 原因」，绝不打印 key
                    println!(
                        "bai-plugin: 模型矩阵刷新失败（{}），沿用旧清单 {} 个",
                        outcome.err_msg, outcome.count
                    );
                }
                fetched_now = outcome.ok;
            }
        }

        let (ok, source, err) = if st.loaded {
            (true, if fetched_now { "live" } else { "cache" }, "")
        } else if !st.err_kind.is_empty() {
            (false, "cache", st.err_kind)
        } else {
            (true, "cache", "")
        };

        let body = build_matrix_resp(ok, source, st.fetched_at, st.count, &st.data, err);
        (
            StatusCode::OK,
            // 与 Go `w.Header().Set("Content-Type", "application/json")` 逐字一致：
            // 不带 `; charset=utf-8`（P1 审查点出的「一字之差」）。
            [(header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    }
}

/// 逐字节复刻 Go `encoding/json` 对 `matrixResp` 的输出：
/// 字段序 ok → source → fetched_at → count → data → err，`data` 为上游原文经
/// `appendCompact(escapeHTML=true)` 之后的字节。
fn build_matrix_resp(
    ok: bool,
    source: &str,
    fetched_at: i64,
    count: usize,
    data: &[u8],
    err: &str,
) -> Vec<u8> {
    let mut out = String::with_capacity(data.len() + 128);
    out.push_str("{\"ok\":");
    out.push_str(if ok { "true" } else { "false" });
    out.push_str(",\"source\":");
    crate::push_go_string(&mut out, source);
    out.push_str(",\"fetched_at\":");
    out.push_str(&fetched_at.to_string());
    out.push_str(",\"count\":");
    out.push_str(&count.to_string());
    out.push_str(",\"data\":");
    if data.is_empty() {
        // Go 侧 `json.RawMessage(nil)` 编出来就是字面量 null（不是空、也不是 []）
        out.push_str("null");
    } else {
        match compact_json_go(data) {
            // `data` 走与 Go 相同的 compact + HTML 转义
            Some(s) => out.push_str(&s),
            // compact 失败（Go 侧会让整条 Encode 失败、什么也不写）：不假装成功，
            // 退到字面量 null，让下游拿到一个**合法** JSON 而不是半截垃圾。
            None => out.push_str("null"),
        }
    }
    if !err.is_empty() {
        out.push_str(",\"err\":");
        crate::push_go_string(&mut out, err);
    }
    out.push('}');
    // Go 侧走 `json.NewEncoder(w).Encode(resp)` —— Encoder 会在末尾补一个换行。
    // 不补就差一个字节（P1 黄金门禁第一次跑就是这么抓出来的）。
    out.push('\n');
    out.into_bytes()
}

/// 复刻 Go `encoding/json` 的 `appendCompact(dst, src, escape=true)`（**Go 1.26.5 的实现**）：
/// * 去掉 token 之间的空白（字符串**内部**的空白原样保留，因为只在字符串外跳空白）
/// * `<`/`>`/`&` 一律改写成 `\u003c/\u003e/\u0026`、U+2028/U+2029 改写成
///   `\u2028/\u2029`——**注意 1.26.5 的写法不看是否在字符串内**（逐个字节判，
///   字符串里的 `<` 也照转义），与更早版本「只在字符串外/内各一套」的读法不同；
///   本函数按 1.26.5 的真实行为对齐，期望值由 `tests/golden/golden-compact-go.json` 锁住。
/// * 非法 JSON 返回 `None`（Go 的 `scan.eof() == scanError` → `Encode` 整体失败、什么都不写）
///
/// 为什么要自己写：Rust 侧 `Box<RawValue>` 已经拿着的是未压缩的原文，
/// `Box<RawValue>::to_string()` 不压缩；压成字符串再编码又会重排键序，
/// 都不等于 Go 的行为。唯一能逐字节对齐的办法就是照抄这段扫描。
///
/// 与 Go 的**已知差异（如实登记）**：
/// * Go 1.26.5 用完整 scanner 校验（含数值 grammar、字面量 grammar、转义 grammar），
///   本函数只覆盖「能出现在合法上游清单里」的那部分——非法输入上可能一个返回 `None`
///   而另一个返回残缺结果。真实上游给的是合法 JSON，此差异只在构造输入下可见。
/// * 字符串里未转义的控制字符（如真换行）Go 判错，本函数同样判错（金标样本
///   `real-newline-in-string` 锁着这条）。
pub fn compact_json_go(src: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(src).ok()?; // Go 侧按字节搬运；非 UTF-8 在 Rust 里没法进 String
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut slashes = 0usize;
    while i < b.len() {
        let c = b[i];
        // 1.26.5 的转义与「是否在字符串内」无关：先按字节判这两类，和 Go 的循环同序
        if let Some(esc) = escape_html_byte(c) {
            out.extend_from_slice(esc);
            i += 1;
            continue;
        }
        if c == 0xe2 && i + 2 < b.len() && b[i + 1] == 0x80 && (b[i + 2] & 0xfe) == 0xa8 {
            out.extend_from_slice(if b[i + 2] == 0xa8 {
                b"\\u2028"
            } else {
                b"\\u2029"
            });
            i += 3;
            continue;
        }
        if in_string {
            if c == b'"' {
                if slashes % 2 == 0 {
                    in_string = false;
                }
                slashes = 0;
                out.push(c);
                i += 1;
                continue;
            }
            if c == b'\\' {
                slashes += 1;
                out.push(c);
                i += 1;
                continue;
            }
            // 字符串里的控制字符必须已转义（Go 的 scanner 判非法，Encode 整体失败）
            if c < 0x20 {
                return None;
            }
            slashes = 0;
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_string = true;
                slashes = 0;
                out.push(c);
                i += 1;
            }
            b'\\' => {
                // 字符串外的反斜杠：原样搬运（前面被跳过的空白无需继续跳）
                out.push(c);
                i += 2; // Go 也是 i++ 之后再 i++（即跳过下一个字节），末尾越界时循环自然结束
                while i < b.len() && is_go_space(b[i]) {
                    i += 1;
                }
            }
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b't' | b'f' | b'n' => {
                // 裸字面量 true / false / null：整段辨认后原样搬运
                let rest = &b[i..];
                let lit: &[u8] = if rest.starts_with(b"true") {
                    b"true"
                } else if rest.starts_with(b"false") {
                    b"false"
                } else if rest.starts_with(b"null") {
                    b"null"
                } else {
                    return None;
                };
                out.extend_from_slice(lit);
                i += lit.len();
            }
            _ => {
                // 数字字面量整体搬运（`-3e2` / `2.5` / `1e999` 都原样过）：Go 的 scanner
                // 只在数值语法坏掉时报错，这里按 token 吞到边界再判。
                if c == b'-' || c.is_ascii_digit() {
                    let start = i;
                    while i < b.len()
                        && (b[i].is_ascii_digit() || matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E'))
                    {
                        i += 1;
                    }
                    let ok = i == b.len() || is_go_space(b[i]) || matches!(b[i], b',' | b']' | b'}');
                    if !ok {
                        return None;
                    }
                    out.extend_from_slice(&b[start..i]);
                } else if c >= 0x20 && c <= 0x7e {
                    // 字母在这里只可能落在非法 token 上（`{not json`），Go 会报错
                    if c.is_ascii_alphabetic() {
                        return None;
                    }
                    out.push(c);
                    i += 1;
                } else {
                    return None;
                }
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `<`/`>`/`&` 的 HTML 转义（Go `escapeHTML: true` 的那三个分支）。
fn escape_html_byte(c: u8) -> Option<&'static [u8]> {
    match c {
        b'<' => Some(b"\\u003c"),
        b'>' => Some(b"\\u003e"),
        b'&' => Some(b"\\u0026"),
        _ => None,
    }
}

/// Go `encoding/json` 的 `isSpace` 集合（注意没有 `\v`）。
fn is_go_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// `?refresh=1` 才走旁路（与 Go `r.URL.Query().Get("refresh") == "1"` 同口径：
/// 取该键的**第一个**值，其余值忽略）。
pub fn query_has_refresh_1(query: Option<&str>) -> bool {
    let Some(q) = query else { return false };
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(k) == "refresh" {
            return percent_decode(v) == "1";
        }
    }
    false
}

/// 只解码够用的百分号转义（Go 的 `Query()` 会解 `%XX` 与 `+`）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hi = hex_val(b[i + 1]);
                let lo = hex_val(b[i + 2]);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 当前 unix 秒（Go `time.Now().Unix()`）。
pub fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Go 的零值 time.Time 的 Unix() 就是 -62135596800；这里如实回 0 之外的
        // 系统时间倒推值没有意义，用 0 表示「拿不到」
        Err(_) => 0,
    }
}

/// 给测试用：把上游返回的 `data` 解析出条数（Go 侧同一处也要数组校验）。
/// 与 `transport::fetch_matrix_once` 同口径：`null` 算 0 条（Go 的 nil 切片）。
pub fn count_items(data: &[u8]) -> Result<usize, String> {
    if data == b"null" {
        return Ok(0);
    }
    let v: Value = serde_json::from_slice(data).map_err(|_| "上游 data 不是数组".to_string())?;
    match v {
        Value::Array(items) => Ok(items.len()),
        _ => Err("上游 data 不是数组".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/model/matrix` 响应体：`data` 的嵌入必须是 Go 的 compact + HTML 转义形态。
    /// 期望值来自 Go 侧真 `encoding/json` 采集的基准（`tests/golden/golden-compact-go.json`，
    /// 产出器 `.work/cli/recompact`）。
    #[test]
    fn compact_matches_go_json_encoding() {
        let path = crate::golden_path("golden-compact-go.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读不到 Go compact 基准 {}: {e}", path.display()));
        let doc: Value = serde_json::from_str(&raw).expect("基准 JSON 解析失败");
        let samples = doc["samples"].as_array().expect("基准缺 samples");
        assert!(!samples.is_empty(), "基准 samples 为空");

        let mut checked = 0usize;
        for s in samples {
            let id = s["id"].as_str().unwrap_or("<no-id>");
            let input = s["in"].as_str().expect("基准缺 in").as_bytes();
            let want = s["out"].as_str().expect("基准缺 out");

            let got_compact = compact_json_go(input);
            if want.is_empty() {
                // Go 编码整体失败（非法 JSON）：Rust 侧必须同样判失败，而不是硬编出一个值
                assert!(
                    got_compact.is_none(),
                    "[{id}] Go 在此输入上编码失败，Rust 却编出了 {}",
                    got_compact.unwrap_or_default()
                );
                checked += 1;
                continue;
            }
            let compact = got_compact.unwrap_or_else(|| panic!("[{id}] Rust 拒绝了一段 Go 能编的输入"));
            let got = build_matrix_resp(true, "live", 1_791_991_397, 0, compact.as_bytes(), "");
            assert_eq!(
                String::from_utf8_lossy(&got),
                want,
                "[{id}] 响应体与 Go 的 json.Encoder 输出不一致"
            );
            checked += 1;
        }
        println!("compact 对照通过：{checked} 条与 Go encoding/json 逐字节一致");
    }

    /// 缺字段的条目按零值容忍（Go 的 `json.Unmarshal` 语义），类型不匹配才整份失败。
    ///
    /// 期望值来自 Go 侧实测（`tests/golden/golden-p1-go.json` 的 `matrix-config-tolerance`
    /// 与 `matrix-config-type-error` 两个场景）：Go 取的是**第一个** `id=="bai"` 的条目，
    /// 它缺 key 时就回零值 `""`（于是走 need_key），**不会**继续往后找带 key 的条目。
    /// 最后一条（数组含 `null` 元素）的期望值由第 5 轮的一次性包内探针在真
    /// `baiChannelKey()` 上实测得到（见简报第 5 轮 R6）。
    #[test]
    fn channel_key_tolerates_missing_fields_like_go() {
        let dir = std::env::temp_dir().join(format!("rs-bai-chkey-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("tuanjie-water-channels.json");

        let cases: [(&str, &str); 6] = [
            // 第一个 bai 条目缺 key → Go 回零值 ""（不继续找后面那个带 key 的 bai 条目）
            (
                r#"[{"id":"bai"},{"key":"sk-only-key"},{"id":"other","key":"sk-o"},{"id":"bai","key":"sk-bai-later"}]"#,
                "",
            ),
            // 第一个 bai 条目缺 key、且后面没有第二个 → 同样空串
            (r#"[{"id":"command","key":"sk-c"},{"id":"bai"}]"#, ""),
            // 缺 id 的条目不影响后续
            (r#"[{"key":"sk-no-id"},{"id":"bai","key":"sk-ok"}]"#, "sk-ok"),
            // 两个都合法 → 取第一个（Go 是顺序 for，取到即 return）
            (r#"[{"id":"bai","key":"sk-first"},{"id":"bai","key":"sk-second"}]"#, "sk-first"),
            // 类型不匹配 → Go 整份解析失败 → 空串
            (r#"[{"id":"bai","key":"sk-ok"},{"id":123}]"#, ""),
            // 数组含 null 元素：Go 把 null 存成零值条目（不报错）→ 后面那条 bai 照常取到
            (r#"[null,{"id":"bai","key":"sk-after-null"}]"#, "sk-after-null"),
        ];
        for (i, (body, want)) in cases.iter().enumerate() {
            std::fs::write(&p, body).unwrap();
            assert_eq!(
                channel_key(&p),
                *want,
                "case #{i} 与 Go 的解析容错口径不一致（输入 {body}）"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
