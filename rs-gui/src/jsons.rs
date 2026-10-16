//! jsons.rs —— 与 Go `encoding/json` 同口径的桥接响应构造。
//!
//! 两条必须对齐的 Go 行为：
//!   1. `json.Marshal(map[string]any{...})` **按键排序**输出。⚠ 不能指望 serde_json 的
//!      默认 Map：依赖图里的 `rs-proxy` 打开了 `preserve_order`，Cargo 的特性统一会让
//!      本 crate 的 `serde_json::Map` 也变成保序的（实测踩到：`json!({...})` 出来的键序
//!      是**书写序**）。故本模块用 [`obj`] 显式按 BTreeMap 的键序构造，键序不再依赖特性开关。
//!   2. `json.Marshal` 默认做 HTML 转义：`<` / `>` / `&` / U+2028 / U+2029 输出成
//!      `\u003c` 等。这些字符在 JSON 里只可能出现在字符串字面量中，故整串替换是安全的。
//!
//! 结构体（`stateMsg`）则按**声明顺序**输出，由 serde 的字段顺序保证。

use serde_json::Value;

/// 构造一个**按键排序**的 JSON 对象（Go map marshal 的口径）。
pub fn obj(pairs: &[(&str, Value)]) -> Value {
    let mut sorted: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for (k, v) in pairs {
        sorted.insert((*k).to_string(), v.clone());
    }
    let mut m = serde_json::Map::new();
    for (k, v) in sorted {
        m.insert(k, v); // 按 BTreeMap 迭代序插入 → 序列化出来即键序
    }
    Value::Object(m)
}

/// JSON 字符串字面量（带引号与转义）。
fn quoted(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Go `jsonOK`：`{"msg":...,"ok":true}`（键序为 Go map 的排序结果）。
pub fn json_ok(msg: &str) -> String {
    go_json(format!("{{\"msg\":{},\"ok\":true}}", quoted(msg)))
}

/// Go `jsonErr`：`{"msg":...,"ok":false}`。入参是错误文案（Go 侧传 `err.Error()`）。
pub fn json_err(msg: &str) -> String {
    go_json(format!("{{\"msg\":{},\"ok\":false}}", quoted(msg)))
}

/// 把 serde 输出转成 Go `json.Marshal` 的字节形态（补 HTML 转义）。
pub fn go_json(s: String) -> String {
    if !s.contains(['<', '>', '&', '\u{2028}', '\u{2029}']) {
        return s;
    }
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(ch),
        }
    }
    out
}

/// 序列化一个 serde 值并按 Go 口径输出。
pub fn marshal<T: serde::Serialize>(v: &T) -> String {
    go_json(serde_json::to_string(v).unwrap_or_else(|_| "null".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ok_and_err_key_order_matches_go_map_marshal() {
        // Go: json.Marshal(map[string]any{"ok":true,"msg":"已保存"}) → 键排序 → msg 在前
        assert_eq!(json_ok("已保存"), r#"{"msg":"已保存","ok":true}"#);
        assert_eq!(json_err("缺少模型名"), r#"{"msg":"缺少模型名","ok":false}"#);
    }

    #[test]
    fn html_escaping_matches_go_default() {
        // Go: json.Marshal("<a>&") → "\u003ca\u003e\u0026"
        assert_eq!(json_ok("<a>&"), r#"{"msg":"\u003ca\u003e\u0026","ok":true}"#);
    }

    #[test]
    fn u2028_escaped() {
        assert_eq!(go_json("\u{2028}".to_string()), "\\u2028");
    }

    #[test]
    fn struct_field_order_is_declaration_order() {
        #[derive(serde::Serialize)]
        struct S {
            a: u8,
            b: u8,
        }
        assert_eq!(marshal(&S { a: 1, b: 2 }), r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn obj_sorts_keys_regardless_of_writing_order() {
        // 键序必须是排序结果（Go map marshal 口径），不是书写序
        assert_eq!(
            marshal(&obj(&[("ok", json!(true)), ("mode", json!("ask"))])),
            r#"{"mode":"ask","ok":true}"#
        );
        assert_eq!(
            marshal(&obj(&[
                ("phase", json!("idle")),
                ("msg", json!("")),
                ("pct", json!(-1)),
                ("ok", json!(true)),
            ])),
            r#"{"msg":"","ok":true,"pct":-1,"phase":"idle"}"#
        );
    }

    #[test]
    fn nested_string_values_keep_go_escaping() {
        assert_eq!(
            marshal(&obj(&[("ok", json!(true)), ("url", json!("a<b&c"))])),
            r#"{"ok":true,"url":"a\u003cb\u0026c"}"#
        );
    }
}

