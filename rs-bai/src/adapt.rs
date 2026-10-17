// adapt.rs —— Go 侧 `internal/bai/server.go` 的 `adaptQuirks` / `truncateToContext` / `estTokens`
// 的 Rust 对应实现。字节级等价由 `tests/golden_adapt.rs` 逐条 diff 把关，基准分两档：
// 入库的小号基准 `tests/golden/golden-adapt-go.small.json`（常规用例 F1~F6、F9~F12），
// 加上 `.work/rust-rewrite/golden-adapt-go.json` 里的 F7/F8 超大用例（那份缺了则显式跳过并计数）。
// 两档都由逐字副本的 Go 模型（`.work/cli/go-adapter`）产出。
//
// 四条改写（与 Go 侧注释逐条对应）：
//  1. 不认 developer 角色 → 降级为 system（否则 400）
//  2. reasoning_effort 只认 off/low/medium/high → max 改写成 high
//  3. max_tokens 超过 8192 钳到 8192
//  4. messages 超上下文窗口（约 1M token）→ 截断最早对话轮次
// 解析失败的原样放行（让上游报错）；只有真正改动了才重新序列化。

use serde_json::Value;

/// B.ai DeepSeek 系列实测上下文上限（约 1M token）。与 Go 侧 `baiContextLimit` 同值。
pub const BAI_CONTEXT_LIMIT: i64 = 1_000_000;

/// 与 Go 侧 `adaptQuirks` 等价的请求体改写。未改动时**原样返回入参字节**（不重排 JSON）。
pub fn adapt_quirks_body(buf: &[u8]) -> Vec<u8> {
    let mut m: Value = match serde_json::from_slice(buf) {
        Ok(v @ Value::Object(_)) => v,
        _ => return buf.to_vec(),
    };

    let mut changed = false;

    // 1) developer → system，顺带做超窗截断（Go 侧同一分支里做，顺序一致）。
    //    先原地改角色，再交给 truncate_to_context —— 与 Go 的时序相同（改完再看 messages）。
    if let Some(Value::Array(msgs)) = m.get_mut("messages") {
        for raw in msgs.iter_mut() {
            if let Value::Object(msg) = raw {
                if msg.get("role").and_then(Value::as_str) == Some("developer") {
                    msg.insert("role".to_string(), Value::String("system".to_string()));
                    changed = true;
                }
            }
        }
        // 不超窗时 truncate_to_context 回 None——绝不能把 messages 换个空数组写回去
        if let Some(out) = truncate_to_context(msgs) {
            *msgs = out;
            changed = true;
        }    }

    // 2) reasoning_effort: "max" → "high"
    if let Some(Value::String(eff)) = m.get("reasoning_effort") {
        if eff == "max" {
            m["reasoning_effort"] = Value::String("high".to_string());
            changed = true;
        }
    }

    // 3) max_tokens 钳制（Go 侧是 `.(float64)` 类型断言：非数字一律不动，负数/零也不动）
    if let Some(Value::Number(n)) = m.get("max_tokens") {
        if let Some(f) = n.as_f64() {
            if f > 8192.0 {
                m["max_tokens"] = Value::Number(8192.into());
                changed = true;
            }
        }
    }

    if !changed {
        return buf.to_vec();
    }
    // 重新序列化：走 `go_json_bytes`（先递归排键到 Go 的字典序，再编字节）。
    // 不能直接 `serde_json::to_vec`——`serde_json::Map` 的实现取决于 `preserve_order`
    // 特性（见 Cargo.toml 注释），直接编出来的字节会随构建配置变。
    let out = go_json_bytes(&mut m);
    if out.is_empty() {
        buf.to_vec()
    } else {
        out
    }
}

/// 递归按字典序排一个 JSON 值里所有对象的键（数组逐元素递归）。
///
/// 只排**键**，不动任何值、不动数组次序；`serde_json::Map::sort_keys()` 在
/// `preserve_order` 下就是给 IndexMap 重排（两种实现下都可用）。
pub(crate) fn sort_keys(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (_, child) in map.iter_mut() {
                sort_keys(child);
            }
            map.sort_keys();
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                sort_keys(item);
            }
        }
        _ => {}
    }
}

/// 把 JSON 值编成 **Go `encoding/json` 口径**的字节：先递归排键（字典序），再序列化。
///
/// 本 crate 里凡是要把 `Value` 化成字节、且那份字节会被门禁逐字节比对的地方，
/// 一律走这里——`serde_json::Map` 的实现取决于 `preserve_order` 特性（见 Cargo.toml），
/// 直接 `to_vec` 的字节会随构建配置变，那就不是合同了。
pub(crate) fn go_json_bytes(v: &mut Value) -> Vec<u8> {
    sort_keys(v);
    serde_json::to_vec(v).unwrap_or_default()
}

/// 与 Go 侧 `estTokens` 等价的保守估算（4 字符 ≈ 1 token）。
/// 计数口径逐条对齐：string content 按 rune 数、数组 content 取 text / image_url、
/// tool_calls 的 arguments 计入、每条消息 +4 overhead。
pub fn est_tokens(msgs: &[Value]) -> i64 {
    let mut total: i64 = 0;
    for raw in msgs {
        let Value::Object(msg) = raw else { continue };
        match msg.get("content") {
            Some(Value::String(c)) => {
                total += (c.chars().count() as i64) / 4;
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    let Value::Object(p) = part else { continue };
                    match p.get("text").and_then(Value::as_str) {
                        Some(t) if !t.is_empty() => {
                            total += (t.chars().count() as i64) / 4;
                        }
                        _ => {
                            if let Some(img) = p.get("image_url").and_then(Value::as_object) {
                                match img.get("url").and_then(Value::as_str) {
                                    Some(u) if u.starts_with("data:image") => {
                                        // base64 文本的 token 代价（按字节数，对齐 Go 的 len(u)）
                                        total += (u.len() as i64) / 4 / 8;
                                    }
                                    Some(_) => total += 512, // 外链图固定估 512 token
                                    None => {}
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(Value::Array(tcs)) = msg.get("tool_calls") {
            for raw_tc in tcs {
                let Value::Object(tc) = raw_tc else { continue };
                if let Some(fn_obj) = tc.get("function").and_then(Value::as_object) {
                    if let Some(args) = fn_obj.get("arguments").and_then(Value::as_str) {
                        if !args.is_empty() {
                            total += (args.chars().count() as i64) / 4;
                        }
                    }
                }
            }
        }
        total += 4; // 每个消息的 overhead（角色名/结构 ≈ 4 token）
    }
    total
}

/// 与 Go 侧 `truncateToContext` 等价：超窗时从最早的非 system 轮次截起
/// （system 与最后一条永远保留）。
///
/// 返回 `None` = 没超窗、未截断（Go 侧对应 `cut=false`）；`Some(msgs)` = 已截断。
/// 注意 Go 侧那个 `cut` 在"超窗分支末尾"恒为 true（两条 return 都写 true），
/// 所以等价关系是「未超窗 → None」「超窗 → Some」，不存在"超窗但 cut=false"的路径。
pub fn truncate_to_context(msgs: &[Value]) -> Option<Vec<Value>> {
    if est_tokens(msgs) <= BAI_CONTEXT_LIMIT {
        return None;
    }
    let last = msgs.len().saturating_sub(1);
    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    let mut tail: Vec<Value> = Vec::new();
    for (i, raw) in msgs.iter().enumerate() {
        if let Value::Object(msg) = raw {
            if msg.get("role").and_then(Value::as_str) == Some("system") {
                out.push(raw.clone()); // system 永远保留
                continue;
            }
        }
        if i == last {
            out.push(raw.clone()); // 最后一条（当前问题）永远保留
            continue;
        }
        tail.push(raw.clone());
    }
    // 从最老的轮次截起：从尾部倒推"还能保留多少最近的轮次"。
    let mut keep_start = tail.len(); // 默认全删
    let mut acc = est_tokens(&out); // system + 最后一条
    for i in (0..tail.len()).rev() {
        let one = est_tokens(std::slice::from_ref(&tail[i]));
        if acc + one > BAI_CONTEXT_LIMIT {
            break;
        }
        acc += one;
        keep_start = i;
    }
    // 配对保护：保留序列开头不能是孤立 role=tool 消息。
    while keep_start < tail.len() {
        let is_tool = matches!(&tail[keep_start], Value::Object(mm)
            if mm.get("role").and_then(Value::as_str) == Some("tool"));
        if is_tool {
            keep_start += 1;
            continue;
        }
        break;
    }
    out.extend(tail[keep_start..].iter().cloned());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn passthrough_when_nothing_to_change() {
        let b = body(json!({"model":"glm-5.3-flash","messages":[{"role":"system","content":"s"}],"max_tokens":64}));
        assert_eq!(adapt_quirks_body(&b), b, "无改写时字节必须原样（不重排 JSON 键序）");
    }

    #[test]
    fn developer_becomes_system() {
        let b = body(json!({"messages":[{"role":"developer","content":"d"},{"role":"user","content":"u"}]}));
        let out_raw = adapt_quirks_body(&b);
        let out: Value = serde_json::from_slice(&out_raw).unwrap();
        assert_eq!(
            out["messages"][0]["role"],
            "system",
            "Rust 出站: {}\n入参: {}",
            String::from_utf8_lossy(&out_raw),
            String::from_utf8_lossy(&b)
        );
    }

    #[test]
    fn max_tokens_clamped_only_above_8192() {
        let b = body(json!({"messages":[],"max_tokens":99999}));
        let out: Value = serde_json::from_slice(&adapt_quirks_body(&b)).unwrap();
        assert_eq!(out["max_tokens"], 8192);

        let b2 = body(json!({"messages":[],"max_tokens":8192}));
        assert_eq!(adapt_quirks_body(&b2), b2, "恰好 8192 不该被改");
    }

    #[test]
    fn reasoning_effort_max_to_high() {
        let b = body(json!({"messages":[],"reasoning_effort":"max"}));
        let out: Value = serde_json::from_slice(&adapt_quirks_body(&b)).unwrap();
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn bad_json_passes_through() {
        let b = b"not-a-json-body {{".to_vec();
        assert_eq!(adapt_quirks_body(&b), b);
    }

    /// P7d-2：不论 `serde_json::Map` 是 BTreeMap 还是 IndexMap（preserve_order 开/关），
    /// 重排后都必须是 **Go 的字典序**，且逐字节一致。
    ///
    /// 这条测试**不依赖构建配置**：前半句先证明「本构建下 Map 确实是插入序的
    /// IndexMap」，后半句才断言改写输出是字典序——把「特性漂移」这件事钉成可回归的
    /// 事实，而不是靠「碰巧没被并集影响」。
    #[test]
    fn remarshal_is_go_sorted_regardless_of_map_impl() {
        let src = br#"{"zebra":1,"messages":[{"role":"developer","content":"d","zz":2,"aa":3}],"max_tokens":99999}"#;

        // 1) 证明 preserve_order 确实生效（否则这条测试就没测到东西）
        let parsed: Value = serde_json::from_slice(src).unwrap();
        let naive = serde_json::to_string(&parsed).unwrap();
        assert_eq!(
            naive, r#"{"zebra":1,"messages":[{"role":"developer","content":"d","zz":2,"aa":3}],"max_tokens":99999}"#,
            "preserve_order 没生效：Map 是 BTreeMap（插入序已在解析时被排序）——\
             那本条测试就测不到 sort_keys，需按实际实现调整（不要删断言来让它变绿）"
        );

        // 2) 改写后的出站字节 = Go 的字典序（递归，含嵌套对象）
        let out = adapt_quirks_body(src);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"max_tokens":8192,"messages":[{"aa":3,"content":"d","role":"system","zz":2}],"zebra":1}"#,
            "改写后必须是 Go 的字典序（递归排到嵌套对象），与 Map 实现无关"
        );
    }

    /// 未改写时**一个字节都不许动**（哪怕键序不是字典序）——原样透传是合同。
    #[test]
    fn passthrough_keeps_non_sorted_key_order() {
        let b = br#"{"zebra":1,"alpha":2}"#.to_vec();
        assert_eq!(adapt_quirks_body(&b), b, "无改写就原样返回，不许顺手排序");
    }
}
