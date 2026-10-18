//! Go `encoding/json` 兼容的 JSON 写出器（编码侧）。
//!
//! 为什么不用 serde_json 直接序列化：Go 的 `json.Marshal` 与 serde 有**三处**逐字节差异，
//! COMMAND 代理的对外响应与出站请求体全是 Go 现拼的，任一处不同就是「看起来对、字节不对」：
//!
//! 1. **浮点**：Go 走 `strconv.AppendFloat(b, f, fmt, -1, 64)`，其中 `fmt = 'f'`，
//!    仅当 `abs != 0 && (abs < 1e-6 || abs >= 1e21)` 时用 `'e'`。`'e'` 形态下 `strconv`
//!    本身写两位指数（`1.5e-07`），但 **`encoding/json` 之后会做一次 cleanup：把
//!    `e-0X` 的前导零去掉**（`e-09` → `e-9`），正指数不受影响（`1e+21` 原样）。
//!    真 Go 实测：`json.Marshal(1.5e-7) == "1.5e-7"`、`json.Marshal(1e-9) == "1e-9"`、
//!    `json.Marshal(1e21) == "1e+21"`——本模块 [`expand_sci`] 走的就是这条**不补零**形态；
//!    `fmt` 的 `%g`/`%v` 才是两位指数（`1e+06`/`1.5e-07`），那条走 [`expand_sci_padded`]。
//!    serde_json 两个形态都不给（阈值不切、指数不补）。
//! 2. **map 键序**：Go 对 `map` 按键的**字节序**输出；`serde_json` 默认保序（插入序）。
//! 3. **HTML 转义**：`<` `>` `&` `\u2028` `\u2029` 转义为 `\uXXXX`；serde 不转义。
//!
//! 规则来源：Go 1.26.5 `encoding/json/encode.go` 的 `stringEncoder` / `floatEncoder`。
//! 浮点的期望值由真 Go 采收（`.work/cli/gofloat`），并作为本模块单测的用例。

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
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 把 `f64` 写成 Go `strconv.FormatFloat(f, fmt, -1, 64)` 的形态（`fmt` 由 Go 的阈值定）。
///
/// 三步：
/// 1. 取**最短往返十进制**：`format!("{:e}", v)` 给出 `[-]<d>[.<ddd>]e<exp>`，与 Go 的
///    shortest 算法同源（都保证「最短且唯一往返」）。
/// 2. 按 Go 的阈值定形态：`fmt = 'f'`，仅当 `abs != 0 && (abs < 1e-6 || abs >= 1e21)` 用 `'e'`。
/// 3. 按该形态铺开：`'f'` 转定位记数（末尾**不**去零——尾数 [1,4] 就写 `1.4`，[1,0] 写 `1`），
///    `'e'` 写 `d[.ddd]e±EE`（指数至少两位）。
pub fn push_go_float(out: &mut String, v: f64) {
    if v.is_nan() || v.is_infinite() {
        // Go：IsInf/IsNaN → "null"（JSON 形态，不是 JS 的 Infinity/NaN）
        out.push_str("null");
        return;
    }
    if v == 0.0 {
        // Go 的 appendFloat：abs==0 走 'f'，写出 "0"；负零保号（"-0"）
        out.push_str(if v.is_sign_negative() { "-0" } else { "0" });
        return;
    }
    let abs = v.abs();
    let sci = abs < 1e-6 || abs >= 1e21;
    let (neg, digits, exp) = shortest_digits(v);
    let body = if sci {
        expand_sci(&digits, exp)
    } else {
        expand_fixed(&digits, exp)
    };
    if neg {
        out.push('-');
    }
    out.push_str(&body);
}

/// 取最短往返十进制的 `(负数?, 尾数数字, 指数)`，语义为 `d[0].d[1..] × 10^exp`。
fn shortest_digits(v: f64) -> (bool, Vec<u8>, i32) {
    let s = format!("{v:e}");
    let (mant, exp) = match s.split_once('e') {
        Some((m, e)) => (m.to_string(), e.parse::<i32>().expect("`{:e}` 必带整数指数")),
        None => (s.clone(), 0),
    };
    let neg = mant.starts_with('-');
    let mant = mant.trim_start_matches('-');
    let digits: Vec<u8> = mant.bytes().filter(|b| b.is_ascii_digit()).collect();
    (neg, digits, exp)
}

/// `'f'` 形态展开：`digits × 10^exp` 写成定位记数，不补也不去有效零。
///
/// `pub(crate)` 供 [`crate::convert::go_format_g`] 复用（`fmt` 的 `%v` 用同一套铺开规则，
/// 只差阈值）。
pub(crate) fn expand_fixed(digits: &[u8], exp: i32) -> String {
    let n = digits.len() as i32;
    let point = exp + 1; // 小数点前的位数
    let mut out = String::with_capacity(digits.len() + 4);
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..(-point) {
            out.push('0');
        }
        for d in digits {
            out.push(*d as char);
        }
        return out;
    }
    if point >= n {
        for d in digits {
            out.push(*d as char);
        }
        for _ in 0..(point - n) {
            out.push('0');
        }
        return out;
    }
    for (i, d) in digits.iter().enumerate() {
        if i as i32 == point {
            out.push('.');
        }
        out.push(*d as char);
    }
    out
}

/// `'e'` 形态展开：`d[.ddd]e±E`。
///
/// ⚠ **指数不补零**：Go 的 `json` 走 `strconv.AppendFloat(f, 'e', -1, 64)`，输的是
/// `1e-7` / `6.02e+23`（`strconv` 的 `%e` 直读形态）；只有 `fmt` 的 `%e` 才补成两位。
/// `pub(crate)` 供 [`crate::convert::go_format_g`] 复用。
pub(crate) fn expand_sci(digits: &[u8], exp: i32) -> String {
    let mut out = String::with_capacity(digits.len() + 8);
    out.push(digits[0] as char);
    if digits.len() > 1 {
        out.push('.');
        for d in &digits[1..] {
            out.push(*d as char);
        }
    }
    out.push('e');
    if exp < 0 {
        out.push('-');
        out.push_str(&(-exp).to_string());
    } else {
        out.push('+');
        out.push_str(&exp.to_string());
    }
    out
}

/// `'e'` 形态展开，**指数补足两位**——`fmt` 的 `%g`/`%v` 走的是这条（`1e+06` / `1.5e-07`），
/// 与 [`expand_sci`]（`json`/`strconv` 的不补零形态）刻意分开：真 Go 实测
/// `fmt.Sprintf("%v", 1e6) == "1e+06"` 而 `json.Marshal(1.5e-7) == "1.5e-7"`。
/// `pub(crate)` 供 [`crate::convert::go_format_g`] 复用。
pub(crate) fn expand_sci_padded(digits: &[u8], exp: i32) -> String {
    let mut out = String::with_capacity(digits.len() + 8);
    out.push(digits[0] as char);
    if digits.len() > 1 {
        out.push('.');
        for d in &digits[1..] {
            out.push(*d as char);
        }
    }
    out.push('e');
    let (sign, mag) = if exp < 0 { ('-', -exp) } else { ('+', exp) };
    out.push(sign);
    let digits_str = mag.to_string();
    if digits_str.len() < 2 {
        out.push('0');
    }
    out.push_str(&digits_str);
    out
}

/// 按 Go `json.Marshal` 的规则写出一个 `serde_json::Value`。
///
/// 数字一律按浮点规则写——与 [`crate::godecode`] 配套（它把入站数字全部落成 `f64`，
/// 与 Go 的 `map[string]any` 一模一样）。
pub fn write_go_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match n.as_f64() {
            Some(f) => push_go_float(out, f),
            None => out.push_str("null"),
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
        Value::Object(m) => write_go_object(out, m),
    }
}

/// Go 对 `map[string]any` 的写出：键按**字节序**排序。
pub fn write_go_object(out: &mut String, m: &Map<String, Value>) {
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

/// 结构体写出用的小构造器：按**声明序**写字段（Go 的结构体编码不问键序，按声明序），
/// 并把 `omitempty` 的判定显式写出来（`*T == nil` / `s == ""`，与 Go 同）。
///
/// 为什么不直接用 serde derive：浮点会落到 serde 的形态（见模块头注释），
/// 而COMMAND 代理的响应里有 `temperature`、`cost` 这类浮点字段，必须走 [`push_go_float`]。
pub struct GoObj {
    out: String,
    first: bool,
}

impl GoObj {
    pub fn new() -> GoObj {
        GoObj {
            out: String::from("{"),
            first: true,
        }
    }

    fn sep(&mut self, key: &str) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        push_go_string(&mut self.out, key);
        self.out.push(':');
    }

    /// 强制写出一个字段（Go 里无 `omitempty` 的字段）。
    pub fn field(&mut self, key: &str, v: &Value) -> &mut Self {
        self.sep(key);
        write_go_value(&mut self.out, v);
        self
    }

    /// 整数字段（Go 的 int/int64 编码，不经浮点）。
    pub fn field_i64(&mut self, key: &str, v: i64) -> &mut Self {
        self.sep(key);
        self.out.push_str(&v.to_string());
        self
    }

    /// 浮点字段（走 Go 的 float 规则）。
    pub fn field_f64(&mut self, key: &str, v: f64) -> &mut Self {
        self.sep(key);
        push_go_float(&mut self.out, v);
        self
    }

    /// 字符串字段（无 `omitempty`）。
    pub fn field_str(&mut self, key: &str, v: &str) -> &mut Self {
        self.sep(key);
        push_go_string(&mut self.out, v);
        self
    }

    /// `omitempty` 字符串：空串不写。
    pub fn opt_str(&mut self, key: &str, v: &str) -> &mut Self {
        if v.is_empty() {
            return self;
        }
        self.field_str(key, v)
    }

    /// `omitempty` 指针：`None` 不写。
    pub fn opt_value(&mut self, key: &str, v: Option<&Value>) -> &mut Self {
        match v {
            Some(v) => self.field(key, v),
            None => self,
        }
    }

    /// 原始 JSON 片段（已按 Go 规则写好），用于嵌套结构体/数组。
    pub fn raw(&mut self, key: &str, raw: &str) -> &mut Self {
        self.sep(key);
        self.out.push_str(raw);
        self
    }

    pub fn finish(mut self) -> String {
        self.out.push('}');
        self.out
    }
}

impl Default for GoObj {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷构造：`{...}`（键值对；写出时按键排序，与 Go 的 map 语义一致）。
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

/// `json.MarshalIndent(v, "", "  ")` 的等价物（**没有**末尾换行）。
pub fn to_go_indent(v: &Value, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let mut s = String::new();
    write_go_indent(&mut s, v, &pad);
    s
}

fn write_go_indent(out: &mut String, v: &Value, pad: &str) {
    match v {
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            let inner = format!("{pad}  ");
            out.push('[');
            out.push('\n');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner);
                write_go_indent(out, it, &inner);
            }
            out.push('\n');
            out.push_str(pad);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            let inner = format!("{pad}  ");
            out.push('{');
            out.push('\n');
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(",\n");
                }
                out.push_str(&inner);
                push_go_string(out, k);
                out.push_str(": ");
                write_go_indent(out, &m[*k], &inner);
            }
            out.push('\n');
            out.push_str(pad);
            out.push('}');
        }
        other => write_go_value(out, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(v: f64) -> String {
        let mut s = String::new();
        push_go_float(&mut s, v);
        s
    }

    // 期望值全部由真 Go 采收（`.work/cli/gofloat`）
    #[test]
    fn floats_match_go_json_marshal() {
        let cases: &[(f64, &str)] = &[
            (1.4e-6, "0.0000014"),
            (0.3, "0.3"),
            (1.5e-7, "1.5e-7"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (1e-7, "1e-7"),
            (429.5, "429.5"),
            (0.1, "0.1"),
            (5.5, "5.5"),
            (1e22, "1e+22"),
            (123000.0, "123000"),
            (-0.0, "-0"),
            (1e-6, "0.000001"),
            (123456789012345680000.0, "123456789012345680000"),
            (3e-7, "3e-7"),
            (6.02e23, "6.02e+23"),
            (0.00001, "0.00001"),
            (123456.789, "123456.789"),
            (2.5e-7, "2.5e-7"),
        ];
        for (v, want) in cases {
            assert_eq!(&f(*v), want, "push_go_float({v:?})");
        }
    }

    #[test]
    fn floats_reparse_to_the_same_value() {
        for v in [1.4e-6f64, 0.3, 1.5e-7, 1e21, 1e20, 1e-7, 0.1, 6.02e23, 123456.789] {
            let s = f(v);
            let back: f64 = s.parse().unwrap_or_else(|e| panic!("{s} 不能解析: {e}"));
            assert_eq!(back.to_bits(), v.to_bits(), "{s} 往返不一致");
        }
    }

    #[test]
    fn special_floats_are_null() {
        assert_eq!(f(f64::NAN), "null");
        assert_eq!(f(f64::INFINITY), "null");
        assert_eq!(f(f64::NEG_INFINITY), "null");
    }

    #[test]
    fn strings_and_keys_match_go() {
        assert_eq!(to_go(&s("plain")), "\"plain\"");
        assert_eq!(to_go(&s("quote\"back\\slash")), "\"quote\\\"back\\\\slash\"");
        assert_eq!(
            to_go(&s("<a> & \"b\"")),
            "\"\\u003ca\\u003e \\u0026 \\\"b\\\"\""
        );
        assert_eq!(
            to_go(&s("行一\n行二\t制表\u{2028}完")),
            "\"行一\\n行二\\t制表\\u2028完\""
        );
        assert_eq!(to_go(&s("\u{1}")), "\"\\u0001\"");
        assert_eq!(to_go(&s("\u{8}\u{c}")), "\"\\b\\f\"");
        assert_eq!(to_go(&s("斜杠/不转义")), "\"斜杠/不转义\"");
        assert_eq!(
            to_go(&json!({"object": "chat.completion.chunk", "id": "x", "created": 1})),
            "{\"created\":1,\"id\":\"x\",\"object\":\"chat.completion.chunk\"}"
        );
    }

    #[test]
    fn nested_and_null_shapes() {
        assert_eq!(to_go(&Value::Null), "null");
        assert_eq!(to_go(&json!([])), "[]");
        assert_eq!(to_go(&json!({})), "{}");
        assert_eq!(
            to_go(&json!([1,"a",null,true,{"k":[]}])),
            "[1,\"a\",null,true,{\"k\":[]}]"
        );
    }

    #[test]
    fn go_obj_keeps_declaration_order() {
        let mut o = GoObj::new();
        o.field_str("z", "first-declared")
            .field_i64("a", 1)
            .opt_str("skip", "")
            .opt_value("none", None);
        assert_eq!(o.finish(), "{\"z\":\"first-declared\",\"a\":1}");
    }
}
