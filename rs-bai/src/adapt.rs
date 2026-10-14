// adapt.rs —— Go 侧 `internal/bai/server.go` 的 `adaptQuirks` / `truncateToContext` / `estTokens`
// 的 Rust 对应实现。字节级等价由 `tests/golden_adapt.rs` 对 `.work/rust-rewrite/golden-adapt-go.json`
// 逐条 diff 把关（那份 golden 由逐字副本的 Go 模型产出）。
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
    serde_json::to_vec(&m).unwrap_or_else(|_| buf.to_vec())
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
}
