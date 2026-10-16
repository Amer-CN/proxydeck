//! Go `encoding/json` 的逐字节等价物——**红线 8**（请求体重排）的地基。
//!
//! 三件事必须与 Go 完全一致，否则出站体会差字节：
//!
//! 1. **字符串转义**（[`write_go_string`]）：`json.Marshal` 默认 `escapeHTML=true`，
//!    `<`/`>`/`&` → `\u003c`/`\u003e`/`\u0026`；控制字符 → `\u00XX`；U+2028/2029 恒转义；
//!    其余 UTF-8 原样（`/` 不转义、0x7f 原样）。
//! 2. **RawMessage 的 compact+escape**（[`compact_go`]）：`json.Marshal(json.RawMessage)`
//!    会先去 token 间的空白、再对字符串里的 `<`/`>`/`&`/U+2028/U+2029 做转义。
//!    客户端字段值靠这条**原样字节搬运**（数字形态、转义形态都不许被重写）。
//! 3. **`any` 的再序列化**（[`marshal_go_value`]）：`stream_options` 这种被解成
//!    `map[string]any` 再吐出来的值，键**按字节序排序**、数字一律按 float64 走 Go 的
//!    `strconv.AppendFloat` 口径。serde_json 默认（不开 `preserve_order`）的 `Map`
//!    就是按键排序，与 Go 相同。

use serde_json::Value;
use std::fmt::Write as _;

/// 写出一个 Go `json.Marshal(string)` 等价物（含两侧引号）。
pub fn write_go_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for ch in s.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            // escapeHTML（json.Marshal 默认开启）
            '<' => out.extend_from_slice(b"\\u003c"),
            '>' => out.extend_from_slice(b"\\u003e"),
            '&' => out.extend_from_slice(b"\\u0026"),
            '\u{2028}' => out.extend_from_slice(b"\\u2028"),
            '\u{2029}' => out.extend_from_slice(b"\\u2029"),
            c if (c as u32) < 0x20 => {
                let mut buf = String::new();
                let _ = write!(buf, "\\u{:04x}", c as u32);
                out.extend_from_slice(buf.as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// [`write_go_string`] 的返回字符串版本。
pub fn go_string(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len() + 2);
    write_go_string(&mut out, s);
    String::from_utf8(out).expect("转义结果始终是合法 UTF-8")
}

/// Go `json.Marshal(json.RawMessage)` 的 compact 阶段：去 token 间空白 + HTML 转义。
///
/// `escape_html=true` 时对 `<`/`>`/`&`/U+2028/U+2029 做转义（Go 1.26.5 的实现对合法
/// JSON 与「只在字符串内转义」等价——结构字符里不可能出现 `<>&`）。
pub fn compact_go(src: &[u8], escape_html: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < src.len() {
        let c = src[i];
        if in_string {
            match c {
                b'\\' => {
                    out.push(c);
                    if i + 1 < src.len() {
                        out.push(src[i + 1]);
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                b'"' => {
                    out.push(c);
                    in_string = false;
                    i += 1;
                }
                b'<' | b'>' | b'&' if escape_html => {
                    push_html_escape(&mut out, c);
                    i += 1;
                }
                0xE2 if escape_html
                    && i + 2 < src.len()
                    && src[i + 1] == 0x80
                    && (src[i + 2] == 0xA8 || src[i + 2] == 0xA9) =>
                {
                    push_line_sep_escape(&mut out, src[i + 2]);
                    i += 3;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
            continue;
        }
        match c {
            b'"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'<' | b'>' | b'&' if escape_html => {
                push_html_escape(&mut out, c);
                i += 1;
            }
            0xE2 if escape_html
                && i + 2 < src.len()
                && src[i + 1] == 0x80
                && (src[i + 2] == 0xA8 || src[i + 2] == 0xA9) =>
            {
                push_line_sep_escape(&mut out, src[i + 2]);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn push_html_escape(out: &mut Vec<u8>, c: u8) {
    out.extend_from_slice(match c {
        b'<' => b"\\u003c".as_slice(),
        b'>' => b"\\u003e".as_slice(),
        _ => b"\\u0026".as_slice(),
    });
}

fn push_line_sep_escape(out: &mut Vec<u8>, last: u8) {
    out.extend_from_slice(if last == 0xA8 {
        b"\\u2028".as_slice()
    } else {
        b"\\u2029".as_slice()
    });
}

/// Go `json.Marshal` 对 `any` 的再序列化（键排序、数字按 float64）。
pub fn marshal_go_value(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_go_value(&mut out, v);
    out
}

fn write_go_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => {
            // Go 解到 any 一律是 float64 → 走同一个浮点格式化口径
            let f = n.as_f64().unwrap_or(0.0);
            out.extend_from_slice(format_go_float(f).as_bytes());
        }
        Value::String(s) => write_go_string(out, s),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_go_value(out, item);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            // serde_json 默认 Map = BTreeMap（按字节序排序），与 Go 的
            // `json.Marshal(map[string]any)` 排序口径一致
            out.push(b'{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_go_string(out, k);
                out.push(b':');
                write_go_value(out, val);
            }
            out.push(b'}');
        }
    }
}

/// Go `encoding/json` 的 float64 编码（`encode.go` 的 `floatEncoder`）：
/// `|f| < 1e-6 或 >= 1e21` 用 `e` 记法，否则用 `f` 记法；两者都是**最短往返**十进制。
/// `e` 记法收尾会把 `e-0X` 清成 `e-X`（Go 原样如此）。
pub fn format_go_float(f: f64) -> String {
    if f == 0.0 {
        // Go：abs==0 走 'f'，AppendFloat(-0, 'f', -1, 64) = "-0"
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let abs = f.abs();
    if abs < 1e-6 || abs >= 1e21 {
        let s = format!("{f:e}"); // 形如 "1.5e-7" / "1e21"
        let (mant, exp) = s.split_once('e').expect("LowerExp 必带 e");
        let exp_num: i32 = exp.parse().expect("指数是整数");
        let sign = if exp_num < 0 { '-' } else { '+' };
        let mut out = format!("{mant}e{sign}{:02}", exp_num.abs());
        // Go 的 e-0X 清理（仅负一位数指数）
        let b = out.as_bytes();
        let n = b.len();
        if n >= 4 && b[n - 4] == b'e' && b[n - 3] == b'-' && b[n - 2] == b'0' {
            out.remove(n - 2);
        }
        out
    } else {
        format!("{f}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_escaping_matches_go_html_escape() {
        assert_eq!(go_string("hello"), "\"hello\"");
        assert_eq!(go_string("<a>&b"), "\"\\u003ca\\u003e\\u0026b\"");
        assert_eq!(go_string("line\nbreak\ttab\r"), "\"line\\nbreak\\ttab\\r\"");
        assert_eq!(go_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(go_string("路径/cwd"), "\"路径/cwd\"");
        assert_eq!(go_string("del\u{7f}"), "\"del\u{7f}\"");
        assert_eq!(go_string("\u{2028}"), "\"\\u2028\"");
    }

    #[test]
    fn compact_removes_whitespace_only_outside_strings() {
        let got = compact_go(br#"{ "a" : [ 1 , 2 ] , "b" : "x y { }" }"#, true);
        assert_eq!(String::from_utf8(got).unwrap(), r#"{"a":[1,2],"b":"x y { }"}"#);
    }

    #[test]
    fn compact_escapes_html_inside_strings() {
        let got = compact_go(br#"{"a":"<b>&c"}"#, true);
        assert_eq!(
            String::from_utf8(got).unwrap(),
            r#"{"a":"\u003cb\u003e\u0026c"}"#
        );
    }

    #[test]
    fn compact_keeps_escapes_intact() {
        // 反斜杠后的引号不能当成字符串结束
        let got = compact_go(br#"{"a":"x\" y }"}"#, true);
        assert_eq!(String::from_utf8(got).unwrap(), r#"{"a":"x\" y }"}"#);
    }

    #[test]
    fn float_formatting_matches_go() {
        assert_eq!(format_go_float(1.0), "1");
        assert_eq!(format_go_float(1.5), "1.5");
        assert_eq!(format_go_float(0.5), "0.5");
        assert_eq!(format_go_float(-2.25), "-2.25");
        assert_eq!(format_go_float(1e20), "100000000000000000000");
        assert_eq!(format_go_float(1e21), "1e+21");
        assert_eq!(format_go_float(1e-7), "1e-7");
        assert_eq!(format_go_float(1.5e-7), "1.5e-7");
        assert_eq!(format_go_float(1e-6), "0.000001");
    }

    #[test]
    fn value_marshal_sorts_keys_like_go() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":{"z":true,"y":null}}"#).unwrap();
        assert_eq!(
            String::from_utf8(marshal_go_value(&v)).unwrap(),
            r#"{"a":{"y":null,"z":true},"b":1}"#
        );
    }
}
