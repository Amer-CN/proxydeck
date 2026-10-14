//! Go `encoding/json` 兼容的 JSON 写出器（编码侧）。
//!
//! 为什么要自己写：Go 的 `json.Marshal` 对 `map` **按键排序**、对字符串做 **HTML 转义**
//! （`<`/`>`/`&` → `\u003c`/`\u003e`/`\u0026`，U+2028/U+2029 也转义），serde_json 两条
//! 都不做。comate 的响应体（chunk / complete / health / error / models）全是 Go 现拼的，
//! 要逐字节对齐只能照抄这套规则——规则来源 `encoding/json/encode.go` 的 `stringEncoder`
//! （Go 1.26.5，`C:\Program Files\Go\src\encoding\json\encode.go:1000-1032`）：
//!
//! * `\b` `\f` `\n` `\r` `\t` 用短转义（**不是** `\u0008`/`\u000c`——Go 源码注释原文
//!   “This encodes bytes < 0x20 except for \b, \f, \n, \r and \t”）；
//! * 其余 < 0x20 用 `\u00xx`（小写十六进制）；
//! * `<` `>` `&` `\u2028` `\u2029` 转义；`/` 不转义；
//! * 非法 UTF-8 → U+FFFD（Rust 的 `&str` 天然合法，这条在 Rust 侧不适用）。
//!
//! 与 comate 版的两处差异（本 crate 需要，见 P3 简报的「积分账本」）：
//! * 新增 `J::Float` —— qoder 的 `/health` 里 `credits.spent` / `spentThisSession`
//!   是 `float64`，Go 的浮点写出规则与 Rust 的 `Display` 不同，见 `push_go_float`；
//! * 其余（字符串转义、map 键排序）与 comate 版逐字一致。

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
///
/// Rust 的 `Display` 永远走定点、`{:e}` 又不带正号也不补零，所以要显式转写：
/// 定点直接用 `{}`（同为「最短可回读」表示）；指数形式补上正号（Go 的 `e+21`）。
/// 期望值由真 Go 采收，锁在 `tests/golden/golden-qoder-go.json` 的 `floats` 段。
pub fn push_go_float(out: &mut String, v: f64) {
    if !v.is_finite() {
        out.push_str("null");
        return;
    }
    let abs = v.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        // 指数形式：`{:e}` 已是「最短可回读」尾数，只需补指数的号与位宽。
        let s = format!("{:e}", v);
        let (mantissa, exp) = s.split_once('e').expect("`{:e}` 必带 e");
        out.push_str(mantissa);
        out.push('e');
        match exp.strip_prefix('-') {
            // 负指数：Go 先补成两位再删前导零，净效果就是「一位或两位以上、无前导零」
            Some(digits) => {
                out.push('-');
                out.push_str(digits.trim_start_matches('0'));
                if digits.trim_start_matches('0').is_empty() {
                    // 理论不可达（abs < 1e-6），兜底成 Go 的形状
                    out.push('0');
                }
            }
            // 正指数：Go 恒带 '+'，且至少两位（1e+21）
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
    // 定点：Rust 的 `{}` 与 Go `AppendFloat('f', -1)` 同为最短可回读十进制。
    out.push_str(&format!("{}", v));
}

/// 一个只用来**写出**的 JSON 值：`Obj` 在序列化时按键排序（等价 Go 的 `map[string]any`）。
#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Int(i64),
    /// `float64`（Go 的浮点写出规则，见 `push_go_float`）。
    Float(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
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
                out.push('{');
                for (i, (k, v)) in sorted.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    push_go_string(out, k);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }

    pub fn to_go(&self) -> String {
        let mut s = String::new();
        self.write_to(&mut s);
        s
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // 规则来源：Go 1.26.5 `encoding/json/encode.go` 的 stringEncoder；期望值由真 Go
    // `json.Marshal` 采收（见 tests/golden/golden-comate-go.json 的 chunks 段，
    // 那条 `chunk-text-controls` 就是 \b \f \n \r \t 与 \u00xx 的混合样本）。
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
    fn object_keys_are_sorted_bytewise() {
        let v = obj(vec![
            ("object", s("chat.completion.chunk")),
            ("id", s("x")),
            ("created", i(1)),
            ("model", s("auto")),
        ]);
        assert_eq!(
            v.to_go(),
            "{\"created\":1,\"id\":\"x\",\"model\":\"auto\",\"object\":\"chat.completion.chunk\"}"
        );
    }

    // 期望值全部由**真 Go** `json.Marshal(float64)` 采收（`tests/golden/golden-qoder-go.json`
    // 的 `floats` 段就是同一批样本）——不是照文档猜的。
    #[test]
    fn floats_match_go_json() {
        let cases: Vec<(f64, &str)> = vec![
            (0.0, "0"),
            (-0.0, "-0"),
            (1.0, "1"),
            (1.25, "1.25"),
            (0.1, "0.1"),
            (3.0, "3"),
            (1234567.891, "1234567.891"),
            (0.1 + 0.2, "0.30000000000000004"),
            (100000000000000000000.0, "100000000000000000000"),
            (1e21, "1e+21"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (-2.5e-8, "-2.5e-8"),
            (1e100, "1e+100"),
            (f64::INFINITY, "null"),
            (f64::NEG_INFINITY, "null"),
            (f64::NAN, "null"),
        ];
        for (input, want) in cases {
            assert_eq!(J::Float(input).to_go(), want, "input={input:?}");
            assert_eq!(f(input).to_go(), want, "input={input:?}");
        }
    }
}
