//! 工具 schema 的 `$ref` / `$defs` 内联（改代码前必读）。
//!
//! 码道上游对工具参数用的是 **Moonshot（Kimi）风格**的 JSON Schema 校验器，
//! 它不认 `$ref`：2026-11-05 实测，只要请求里带一个 `mcp__craft-anything__*`
//! 这种 `parameters` 含 `$defs` + `$ref` 的工具，整轮请求就 400：
//!
//! ```text
//! Invalid request: tools.function.parameters is not a valid moonshot flavored
//! json schema, details: <At path 'properties.if_tampered': detected infinite
//! recursion without termination condition>
//! ```
//!
//! 做法与 `rs-codebuddy/src/toolrefs.rs` 同源（那边的注释更详细）：
//! * 只认 `#/$defs/` 前缀，取末段作为定义名；
//! * 兄弟键按 2020-12 语义合并，**同名兄弟键优先**；
//! * 循环引用按解析栈截断成 `{"type":"object","description":"cyclic-ref-removed"}`；
//! * 外部引用 / 找不到定义 → 原样保留；
//! * 没有 `$defs` 的工具**一个字节都不改**（返回 0）。

use serde_json::{Map, Value};

/// 内联请求体里所有工具参数的 `$ref`。
/// 返回被改写过的工具数（即原本带 `$defs` 的工具数），0 表示没动。
pub fn sanitize_tool_refs(body: &mut Value) -> usize {
    let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return 0;
    };
    let mut changed = 0usize;
    for raw in tools.iter_mut() {
        let Some(tm) = raw.as_object_mut() else {
            continue;
        };
        // OpenAI 线格式 { type, function: { parameters } }；也兼容扁平的 { parameters }。
        let holder = match tm.get_mut("function") {
            Some(Value::Object(f)) => f,
            _ => tm,
        };
        let Some(params) = holder.get_mut("parameters") else {
            continue;
        };
        let defs = match params.get("$defs").and_then(|d| d.as_object()) {
            Some(d) if !d.is_empty() => d.clone(),
            _ => continue,
        };
        let inlined = inline_schema_refs(params, &defs, &[]);
        *params = inlined;
        if let Some(pm) = params.as_object_mut() {
            pm.remove("$defs");
        }
        changed += 1;
    }
    changed
}

/// 深度遍历，把 `{"$ref":"#/$defs/X"}` 换成就地展开的定义。
pub fn inline_schema_refs(node: &Value, defs: &Map<String, Value>, chain: &[String]) -> Value {
    let m = match node {
        Value::Object(m) => m,
        Value::Array(arr) => {
            return Value::Array(
                arr.iter()
                    .map(|v| inline_schema_refs(v, defs, chain))
                    .collect(),
            )
        }
        other => return other.clone(),
    };
    if let Some(rv) = m.get("$ref").and_then(|v| v.as_str()) {
        if let Some(stripped) = rv.strip_prefix("#/$defs/") {
            let key = stripped.rsplit('/').next().unwrap_or(stripped).to_string();
            if chain.iter().any(|c| c == &key) {
                // 环：截断，别让上游再去判递归
                return serde_json::json!({
                    "type": "object",
                    "description": "cyclic-ref-removed"
                });
            }
            if let Some(target) = defs.get(&key) {
                let mut merged = match target {
                    Value::Object(tm) => tm.clone(),
                    _ => Map::new(),
                };
                for (k, v) in m {
                    if k != "$ref" {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                let mut nc: Vec<String> = chain.to_vec();
                nc.push(key);
                return inline_schema_refs(&Value::Object(merged), defs, &nc);
            }
        }
    }
    let mut out = Map::new();
    for (k, v) in m {
        out.insert(k.clone(), inline_schema_refs(v, defs, chain));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inlines_defs_and_merges_siblings() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "t",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "mode": {"$ref": "#/$defs/Policy", "description": "兄弟键保留"}
                        },
                        "$defs": {
                            "Policy": {"type": "string", "enum": ["fail", "continue"]}
                        }
                    }
                }
            }]
        });
        let n = sanitize_tool_refs(&mut body);
        assert_eq!(n, 1);
        let p = &body["tools"][0]["function"]["parameters"];
        assert_eq!(p["properties"]["mode"]["type"], "string");
        assert_eq!(p["properties"]["mode"]["description"], "兄弟键保留");
        assert!(p.get("$defs").is_none(), "用完的 $defs 必须删掉");
        assert!(body.to_string().find("$ref").is_none());
    }

    #[test]
    fn cyclic_ref_is_truncated() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "t",
                    "parameters": {
                        "type": "object",
                        "properties": {"a": {"$ref": "#/$defs/Node"}},
                        "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}}
                    }
                }
            }]
        });
        assert_eq!(sanitize_tool_refs(&mut body), 1);
        let s = body.to_string();
        assert!(s.contains("cyclic-ref-removed"), "{s}");
        assert!(!s.contains("$ref"), "{s}");
    }

    #[test]
    fn tools_without_defs_are_untouched() {
        let mut body = json!({
            "tools": [{
                "type": "function",
                "function": {"name": "read", "parameters": {"type": "object", "properties": {"p": {"type": "string"}}}}
            }]
        });
        let before = body.clone();
        assert_eq!(sanitize_tool_refs(&mut body), 0);
        assert_eq!(body, before);
    }

    #[test]
    fn flat_tool_shape_is_supported() {
        let mut body = json!({
            "tools": [{
                "name": "t",
                "parameters": {
                    "type": "object",
                    "properties": {"x": {"$ref": "#/$defs/S"}},
                    "$defs": {"S": {"type": "integer"}}
                }
            }]
        });
        assert_eq!(sanitize_tool_refs(&mut body), 1);
        assert_eq!(body["tools"][0]["parameters"]["properties"]["x"]["type"], "integer");
    }

    #[test]
    fn missing_tools_is_noop() {
        let mut body = json!({"messages": []});
        assert_eq!(sanitize_tool_refs(&mut body), 0);
    }
}