//! Go `encoding/json` 兼容的 JSON 写出器（编码侧）。
//!
//! 为什么要自己写：Go 的 `json.Marshal` 对 `map` **按键排序**（字节序）、对字符串做
//! **HTML 转义**（`<`/`>`/`&` → `\u003c`/`\u003e`/`\u0026`，U+2028/U+2029 也转义），
//! serde_json 两条都不做。codebuddy 的入站体要**原样重建后出站**（`mustJSONReader` →
//! `json.Marshal`），响应体（health / models / model info / error）也全是 Go 现拼的，
//! 要逐字节对齐只能照抄这套规则——规则来源 `encoding/json/encode.go` 的 `stringEncoder`
//! （Go 1.26.5，`C:\Program Files\Go\src\encoding\json\encode.go:1000-1032`）：
//!
//! * `\b` `\f` `\n` `\r` `\t` 用短转义（**不是** `\u0008`/`\u000c`）；
//! * 其余 < 0x20 用 `\u00xx`（小写十六进制）；
//! * `<` `>` `&` `\u2028` `\u2029` 转义；`/` 不转义。
//!
//! 数字：Go 的 `json.Unmarshal` 把**所有**数字落成 `float64`（`map[string]any` 语义），
//! 再 `json.Marshal` 回来时走 `floatEncoder` 的规则（见 `push_go_float`）——所以
//! 「入站 `4096` → 出站 `4096`」这类往返必须按浮点规则写，不能按整数写。

use serde_json::{Map, Value};

/// 按 Go 的规则写一个 JSON 字符串字面量（含双引号）。
pub fn push_go_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Go `encoding/json` 对 `float64` 的写出规则（`encode.go` 的 `floatEncoder`）：
///
/// ```text
/// if IsInf || IsNaN      -> "null"（JSON，不是 JS 版的 "Infinity"/"NaN"）
/// abs := |f|
/// fmt := 'f'; if abs != 0 && (abs < 1e-6 || abs >= 1e21) { fmt = 'e' }
/// b = strconv.AppendFloat(b, f, fmt, -1, 64)
/// ```
pub fn push_go_float(out: &mut String, v: f64) {
    if !v.is_finite() {
        out.push_str("null");
        return;
    }
    let abs = v.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        let s = format!("{:e}", v);
        let (mantissa, exp) = s.split_once('e').expect("`{:e}` 必带 e");
        out.push_str(mantissa);
        out.push('e');
        match exp.strip_prefix('-') {
            Some(digits) => {
                out.push('-');
                let trimmed = digits.trim_start_matches('0');
                if trimmed.is_empty() {
                    out.push('0');
                } else {
                    out.push_str(trimmed);
                }
            }
            None => {
                out.push('+');
                if exp.len() < 2 {
                    out.push('0');
                }
                out.push_str(exp);
            }
        }
        return;
    }
    out.push_str(&format!("{}", v));
}

/// 按 Go `json.Marshal` 的规则写出一个 `serde_json::Value`。
///
/// 数字一律按 `float64` 规则写——与本 crate 的解码器（`godecode`）配套：它把入站数字
/// 全部落成 `f64`，与 Go 的 `map[string]any` 一模一样。
pub fn write_go_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64() {
            Some(f) => push_go_float(out, f),
            None => push_go_float(out, f64::NAN),
        },
        Value::String(s) => push_go_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_go_value(out, it);
            }
            out.push(']');
        }
        Value::Object(m) => {
            // Go 对 map 的键排序是字节序（string 直接比大小），与 Rust 的 as_bytes() 同义。
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_go_string(out, k);
                out.push(':');
                write_go_value(out, &m[*k]);
            }
            out.push('}');
        }
    }
}

/// `json.Marshal(v)` 的等价物（返回字符串）。
pub fn to_go(v: &Value) -> String {
    let mut s = String::new();
    write_go_value(&mut s, v);
    s
}

/// `json.Marshal(v)` 的等价物（返回字节，用于出站请求体）。
pub fn to_go_bytes(v: &Value) -> Vec<u8> {
    to_go(v).into_bytes()
}

/// 便捷构造：`{...}`（键值对；写出时按键排序，与 Go 同）。
pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

/// 便捷构造：`"..."`。
pub fn s(v: impl Into<String>) -> Value {
    Value::String(v.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 期望值由真 Go `json.Marshal` 采收（规则来源见模块头注释）。
    #[test]
    fn strings_and_keys_match_go() {
        assert_eq!(to_go(&s("plain")), "\"plain\"");
        assert_eq!(
            to_go(&s("quote\"back\\slash")),
            "\"quote\\\"back\\\\slash\""
        );
        assert_eq!(to_go(&s("<a> & \"b\"")), "\"\\u003ca\\u003e \\u0026 \\\"b\\\"\"");
        assert_eq!(
            to_go(&s("行一\n行二\t制表\u{2028}完")),
            "\"行一\\n行二\\t制表\\u2028完\""
        );
        assert_eq!(to_go(&s("\u{1}")), "\"\\u0001\"");
        assert_eq!(to_go(&s("斜杠/不转义")), "\"斜杠/不转义\"");
        // 键排序按字节序（Go map 语义）
        assert_eq!(
            to_go(&json!({"object": "chat.completion.chunk", "id": "x", "created": 1})),
            "{\"created\":1,\"id\":\"x\",\"object\":\"chat.completion.chunk\"}"
        );
    }

    // 数字走 Go 的 float64 规则：入站 4096 → 出站 4096（不是 4096.0）；1e999 之类按 null 兜底。
    #[test]
    fn numbers_match_go_float_rules() {
        assert_eq!(to_go(&json!(4096)), "4096");
        assert_eq!(to_go(&json!(0.7)), "0.7");
        assert_eq!(to_go(&json!(-3)), "-3");
        assert_eq!(push_go_float_str(1e21), "1e+21");
        assert_eq!(push_go_float_str(1e-7), "1e-7");
        assert_eq!(push_go_float_str(1.5e-7), "1.5e-7");
        assert_eq!(push_go_float_str(f64::INFINITY), "null");
    }

    fn push_go_float_str(v: f64) -> String {
        let mut s = String::new();
        push_go_float(&mut s, v);
        s
    }
}
