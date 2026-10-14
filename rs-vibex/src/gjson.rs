//! Go `encoding/json` 兼容的 JSON 写出器（编码侧）。
//!
//! 来源：`rs-qoder/src/gjson.rs`（P3）的扩展版，三处按 vibex 需要加：
//! * `J::Struct`：**按插入顺序**写出（Go 的 struct 走字段声明顺序，`map` 才排序）；
//! * `J::from_value`：把上游原样搬来的 JSON 值按 Go 的 `any`（全部数字都是 `float64`）
//!   规则重新编码 —— vibex 有大量「解包后原样嵌入响应」的字段
//!   （`billing_tier`/`description`/`init_root_meta` 整块）；
//! * `write_to_indent`：`json.MarshalIndent(c, "", "  ")` 的两空格缩进（配置文件落盘用）。
//!
//! 规则来源 `encoding/json/encode.go` 的 `stringEncoder`（Go 1.26.5，
//! `C:\Program Files\Go\src\encoding\json\encode.go`）：
//!
//! * `\b` `\f` `\n` `\r` `\t` 用短转义；其余 < 0x20 用 `\u00xx`（小写十六进制）；
//! * `<` `>` `&` `\u2028` `\u2029` 转义；`/` 不转义；
//! * `map` 键按**字节序**排序；struct 按字段顺序。

use serde_json::Value;

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
/// if fmt == 'e' && b 形如 "<...>e-0X" { 删掉那个 '0' }   // 1e-07 -> 1e-7
/// ```
pub fn push_go_float(out: &mut String, v: f64) {
    if !v.is_finite() {
        out.push_str("null");
        return;
    }
    let abs = v.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        // 指数形式：Rust 的 `{:e}` 不给正号（Go 给 `e+21`），也不按 Go 的规则去掉 `e-07` 的 0。
        let s = format!("{:e}", v);
        let (mant, exp) = s.split_once('e').unwrap_or((s.as_str(), "0"));
        let mut exp = exp.to_string();
        let neg = exp.starts_with('-');
        if neg || exp.starts_with('+') {
            exp.remove(0);
        }
        // Go 去掉指数里多余的前导零：e-07 -> e-7，e+21 保持两位（>=10 不动）
        let trimmed = exp.trim_start_matches('0');
        let exp = if trimmed.is_empty() { "0" } else { trimmed };
        out.push_str(mant);
        out.push('e');
        out.push(if neg { '-' } else { '+' });
        out.push_str(exp);
        return;
    }
    // 定点：Rust 的 `{}` 与 Go `AppendFloat('f', -1)` 同为最短可回读十进制。
    out.push_str(&format!("{}", v));
}

/// 一个只用来**写出**的 JSON 值。
#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Int(i64),
    /// `float64`（Go 的浮点写出规则，见 `push_go_float`）。
    Float(f64),
    Str(String),
    Arr(Vec<J>),
    /// 等价 Go 的 `map[string]any`：写出时按**字节序**排序键。
    Obj(Vec<(String, J)>),
    /// 等价 Go 的 struct：写出时**保持**给定的字段顺序。
    Struct(Vec<(String, J)>),
}

impl J {
    /// 单行紧凑写法（`json.Marshal`）。
    pub fn write_to(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Int(v) => out.push_str(&v.to_string()),
            J::Float(v) => push_go_float(out, *v),
            J::Str(s) => push_go_string(out, s),
            J::Arr(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.write_to(out);
                }
                out.push(']');
            }
            J::Obj(pairs) => {
                // Go 对 map 的键排序是**字节序**（string 直接比大小）——Rust 的
                // `as_bytes()` 比较与之同义。
                let mut sorted: Vec<&(String, J)> = pairs.iter().collect();
                sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                write_pairs_compact(out, sorted);
            }
            J::Struct(pairs) => {
                let ordered: Vec<&(String, J)> = pairs.iter().collect();
                write_pairs_compact(out, ordered);
            }
        }
    }

    /// 两空格缩进（`json.MarshalIndent(v, "", "  ")`）。
    pub fn to_go_indent2(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, "");
        out
    }

    fn write_pretty(&self, out: &mut String, indent: &str) {
        match self {
            J::Arr(items) if !items.is_empty() => {
                let inner = format!("{indent}  ");
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('\n');
                    out.push_str(&inner);
                    it.write_pretty(out, &inner);
                }
                out.push('\n');
                out.push_str(indent);
                out.push(']');
            }
            J::Obj(pairs) if !pairs.is_empty() => {
                let mut sorted: Vec<&(String, J)> = pairs.iter().collect();
                sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                write_pairs_pretty(out, sorted, indent);
            }
            J::Struct(pairs) if !pairs.is_empty() => {
                let ordered: Vec<&(String, J)> = pairs.iter().collect();
                write_pairs_pretty(out, ordered, indent);
            }
            other => other.write_to(out),
        }
    }

    /// 单行紧凑写法（`json.Marshal`）。
    pub fn to_go(&self) -> String {
        let mut s = String::new();
        self.write_to(&mut s);
        s
    }

    /// 从 `serde_json` 的值转成 Go 口径的 J（数字一律当 `float64`）。
    pub fn from_value(v: &Value) -> J {
        match v {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Number(n) => J::Float(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => J::Str(s.clone()),
            Value::Array(items) => J::Arr(items.iter().map(J::from_value).collect()),
            Value::Object(m) => J::Obj(m.iter().map(|(k, v)| (k.clone(), J::from_value(v))).collect()),
        }
    }
}

fn write_pairs_compact(out: &mut String, pairs: Vec<&(String, J)>) {
    out.push('{');
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_go_string(out, k);
        out.push(':');
        v.write_to(out);
    }
    out.push('}');
}

fn write_pairs_pretty(out: &mut String, pairs: Vec<&(String, J)>, indent: &str) {
    let inner = format!("{indent}  ");
    out.push('{');
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('\n');
        out.push_str(&inner);
        push_go_string(out, k);
        // `json.MarshalIndent` 在键后写 `": "`（紧凑模式才是 `":"`）
        out.push_str(": ");
        v.write_pretty(out, &inner);
    }
    out.push('\n');
    out.push_str(indent);
    out.push('}');
}

pub fn s(v: impl Into<String>) -> J {
    J::Str(v.into())
}

pub fn i(v: i64) -> J {
    J::Int(v)
}

pub fn f(v: f64) -> J {
    J::Float(v)
}

pub fn b(v: bool) -> J {
    J::Bool(v)
}

pub fn arr(items: Vec<J>) -> J {
    J::Arr(items)
}

pub fn obj(pairs: Vec<(&str, J)>) -> J {
    J::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

pub fn st(pairs: Vec<(&str, J)>) -> J {
    J::Struct(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_escaping_matches_go() {
        let cases: Vec<(&str, &str)> = vec![
            ("plain", "\"plain\""),
            ("quote\"back\\slash", "\"quote\\\"back\\\\slash\""),
            ("<a> & \"b\"", "\"\\u003ca\\u003e \\u0026 \\\"b\\\"\""),
            ("行一\n行二\t制表\r回车\u{8}\u{c}", "\"行一\\n行二\\t制表\\r回车\\b\\f\""),
            ("行\u{2028}分隔\u{2029}完", "\"行\\u2028分隔\\u2029完\""),
            ("\u{1}", "\"\\u0001\""),
            ("斜杠/不转义", "\"斜杠/不转义\""),
            ("中文é中", "\"中文é中\""),
        ];
        for (input, want) in cases {
            assert_eq!(J::Str(input.to_string()).to_go(), want, "input={input:?}");
        }
    }

    #[test]
    fn floats_match_go_json() {
        // 期望值由真 Go `json.Marshal(map[string]any{"v": f})` 采收
        // （见 tests/golden/golden-vibex-go.json 的 `floats` 段）。
        let cases: Vec<(f64, &str)> = vec![
            (0.0, "0"),
            (1.0, "1"),
            (1.25, "1.25"),
            (0.1, "0.1"),
            (403.0, "403"),
            (3.0, "3"),
            (-2.5, "-2.5"),
            (1000000.0, "1000000"),
        ];
        for (input, want) in cases {
            assert_eq!(J::Float(input).to_go(), want, "input={input:?}");
        }
    }

    #[test]
    fn object_keys_are_sorted_bytewise_structs_keep_order() {
        let o = obj(vec![("object", s("list")), ("data", arr(vec![]))]);
        assert_eq!(o.to_go(), "{\"data\":[],\"object\":\"list\"}");
        let s1 = st(vec![("id", s("x")), ("object", s("model"))]);
        assert_eq!(s1.to_go(), "{\"id\":\"x\",\"object\":\"model\"}");
    }
}
