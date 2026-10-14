//! Go `encoding/json` 兼容的解码器——只覆盖 codebuddy 的入站合同：
//! `json.Unmarshal(body, &payload)`，`payload` 是 `map[string]any`。
//!
//! 为什么要自己写：`handleChat` 的 400 响应体里带 Go 的**原文错误串**
//! （`bad json: <err>`），黄金基准把它逐字锁住了；serde 的措辞（"expected value at
//! line 1 column 1" 之类）与 Go 完全不同，用 serde 就对不上。这里复刻 Go 在该合同上
//! **可观察**的那些语义：
//!
//! * 先 `checkValid` 全量语法扫描（Go `Unmarshal` 就是先扫后解），语法错优先于类型错，
//!   文案与 Go scanner 一致；**所有** EOF 类错误都归一成 `unexpected end of JSON input`；
//! * 顶层必须是 object 或 `null`：`null` 视作空 map（Go 里 payload 保持 nil，不报错）；
//!   数组/字符串/数字/布尔报
//!   `json: cannot unmarshal <kind> into Go value of type map[string]interface {}`；
//! * 尾随数据报 `invalid character 'x' after top-level value`（`Unmarshal` 与
//!   `Decoder` 的区别就在这：qoder 那边是 Decoder，尾随数据被忽略）；
//! * 数字一律按 `float64` 落（`map[string]any` 语义），**溢出**（±Inf）报
//!   `json: cannot unmarshal number <原字面量> into Go value of type float64`，
//!   **下溢**（→0）不报错；重复键**后者生效**。
//!
//! 未逐字对齐的边界（真实客户端不会走到，登记在 `rs-codebuddy/README.md`）：非法 UTF-8
//! 字节的替换细节、`\u` 代理对边界、超大数字字面量的 ParseFloat 细节。

use serde_json::{Map, Value};

/// 与 Go `map[string]any` 同形的有序无关映射（写出时按键排序，见 `gjson`）。
pub type GoMap = Map<String, Value>;

/// `json.Unmarshal(body, &payload)`（`payload` 为 `map[string]any`）的等价物。
pub fn unmarshal_map(data: &[u8]) -> Result<GoMap, String> {
    check_valid(data)?;
    let mut p = B { b: data, i: 0 };
    p.skip_ws();
    match p.b.get(p.i).copied() {
        Some(b'{') => match p.value()? {
            Value::Object(m) => Ok(m),
            _ => unreachable!("check_valid 已保证顶层是 object"),
        },
        // `null` → Go 里 payload 保持 nil（不报错），后续 `payload["messages"]` 取零值。
        Some(b'n') => Ok(Map::new()),
        Some(c) => Err(format!(
            "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
            top_kind(c)
        )),
        None => Err("unexpected end of JSON input".to_string()),
    }
}

fn top_kind(c: u8) -> &'static str {
    match c {
        b'[' => "array",
        b'"' => "string",
        b't' | b'f' => "bool",
        _ => "number",
    }
}

// ============ 第一遍：语法扫描（Go scanner 的文案） ============

fn check_valid(data: &[u8]) -> Result<(), String> {
    let mut s = P { b: data, i: 0 };
    s.skip_ws();
    if s.i >= s.b.len() {
        return Err("unexpected end of JSON input".to_string());
    }
    s.scan_value()?;
    s.skip_ws();
    if let Some(&c) = s.b.get(s.i) {
        return Err(s.err_at(c, "after top-level value"));
    }
    Ok(())
}

/// Go `strconv.Quote` 的单字符近似（scanner 报错里出现的字符都用它）。
fn quote_rune(c: char) -> String {
    match c {
        '\'' => "'\\''".to_string(),
        '"' => "'\"'".to_string(),
        '\\' => "'\\\\'".to_string(),
        '\n' => "'\\n'".to_string(),
        '\r' => "'\\r'".to_string(),
        '\t' => "'\\t'".to_string(),
        c if (c as u32) < 0x20 => format!("'\\x{:02x}'", c as u32),
        c => format!("'{}'", c),
    }
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.b.get(self.i).copied()
    }

    fn cur_char(&self) -> char {
        let rest = &self.b[self.i.min(self.b.len())..];
        match std::str::from_utf8(rest) {
            Ok(s) => s.chars().next().unwrap_or('\0'),
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    std::str::from_utf8(&rest[..valid])
                        .ok()
                        .and_then(|s| s.chars().next())
                        .unwrap_or('\u{FFFD}')
                } else {
                    rest.first().map(|b| *b as char).unwrap_or('\0')
                }
            }
        }
    }

    fn err_at(&self, c: u8, what: &str) -> String {
        let ch = if c.is_ascii() { c as char } else { self.cur_char() };
        format!("invalid character {} {}", quote_rune(ch), what)
    }

    fn eof(&self) -> String {
        // Go 的 `Unmarshal` 走 scanner 的 eof 分支：一律这句（不是 Decoder 的 "unexpected EOF"）。
        "unexpected end of JSON input".to_string()
    }

    fn scan_value(&mut self) -> Result<(), String> {
        match self.peek() {
            None => Err(self.eof()),
            Some(b'{') => {
                self.i += 1;
                self.scan_object_body()
            }
            Some(b'[') => {
                self.i += 1;
                self.scan_array_body()
            }
            Some(b'"') => {
                self.scan_string()?;
                Ok(())
            }
            Some(b't') => self.scan_lit("true"),
            Some(b'f') => self.scan_lit("false"),
            Some(b'n') => self.scan_lit("null"),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.scan_number(),
            Some(c) => Err(self.err_at(c, "looking for beginning of value")),
        }
    }

    fn scan_lit(&mut self, lit: &str) -> Result<(), String> {
        for (k, want) in lit.as_bytes().iter().enumerate() {
            match self.b.get(self.i + k) {
                None => return Err(self.eof()),
                Some(got) if got == want => {}
                Some(&got) => {
                    return Err(format!(
                        "invalid character {} in literal {} (expecting '{}')",
                        quote_rune(got as char),
                        lit,
                        *want as char
                    ))
                }
            }
        }
        self.i += lit.len();
        Ok(())
    }

    fn scan_number(&mut self) -> Result<(), String> {
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => {
                while self.b.get(self.i).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    self.i += 1;
                }
            }
            None => return Err(self.eof()),
            Some(&c) => return Err(self.err_at(c, "in numeric literal")),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            let start = self.i;
            while self.b.get(self.i).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                self.i += 1;
            }
            if self.i == start {
                return match self.b.get(self.i) {
                    None => Err(self.eof()),
                    Some(&c) => Err(self.err_at(c, "after decimal point in numeric literal")),
                };
            }
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            let start = self.i;
            while self.b.get(self.i).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                self.i += 1;
            }
            if self.i == start {
                return match self.b.get(self.i) {
                    None => Err(self.eof()),
                    Some(&c) => Err(self.err_at(c, "in exponent of numeric literal")),
                };
            }
        }
        Ok(())
    }

    fn scan_hex4(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = match self.b.get(self.i) {
                Some(&c) => c,
                None => return Err(self.eof()),
            };
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return Err(self.err_at(c, "in \\u hexadecimal character escape")),
            };
            v = v * 16 + d;
            self.i += 1;
        }
        Ok(v)
    }

    fn scan_string(&mut self) -> Result<(), String> {
        self.i += 1; // 开引号
        loop {
            let c = match self.b.get(self.i) {
                Some(&c) => c,
                None => return Err(self.eof()),
            };
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.i += 1;
                    let e = match self.b.get(self.i) {
                        Some(&e) => e,
                        None => return Err(self.eof()),
                    };
                    match e {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.i += 1,
                        b'u' => {
                            self.i += 1;
                            let hi = self.scan_hex4()?;
                            if (0xD800..0xDC00).contains(&hi)
                                && self.b.get(self.i) == Some(&b'\\')
                                && self.b.get(self.i + 1) == Some(&b'u')
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.scan_hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    self.i = save;
                                }
                            }
                        }
                        other => {
                            return Err(format!(
                                "invalid character {} in string escape code",
                                quote_rune(other as char)
                            ))
                        }
                    }
                }
                c if c < 0x20 => {
                    return Err(format!(
                        "invalid character {} in string literal",
                        quote_rune(c as char)
                    ))
                }
                _ => self.i += 1,
            }
        }
    }

    fn scan_array_body(&mut self) -> Result<(), String> {
        // Go 的 scanner 分两态：`[` 之后允许 `]`（空数组），`,` 之后**必须**有值
        // （`[1,]` 是 "looking for beginning of value"，不是合法空尾）。
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(self.eof()),
                Some(b']') if first => {
                    self.i += 1;
                    return Ok(());
                }
                Some(_) => {}
            }
            self.scan_value()?;
            match self.peek() {
                None => return Err(self.eof()),
                Some(b',') => {
                    self.i += 1;
                    first = false;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(c) => return Err(self.err_at(c, "after array element")),
            }
        }
    }

    fn scan_object_body(&mut self) -> Result<(), String> {
        // 同理：`{` 之后允许 `}`（空对象），`,` 之后必须**是键字符串**
        //（`{"a":1,}` 是 "looking for beginning of object key string"，黄金基准锁住了这条）。
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(self.eof()),
                Some(b'}') if first => {
                    self.i += 1;
                    return Ok(());
                }
                Some(b'"') => {}
                Some(c) => return Err(self.err_at(c, "looking for beginning of object key string")),
            }
            self.scan_string()?;
            match self.peek() {
                None => return Err(self.eof()),
                Some(b':') => self.i += 1,
                Some(c) => return Err(self.err_at(c, "after object key")),
            }
            self.scan_value()?;
            match self.peek() {
                None => return Err(self.eof()),
                Some(b',') => {
                    self.i += 1;
                    first = false;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(c) => return Err(self.err_at(c, "after object key:value pair")),
            }
        }
    }
}

// ============ 第二遍：建值（语法已保证合法，只剩数字溢出这一种错） ============

struct B<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> B<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.b[self.i] {
            b'{' => {
                self.i += 1;
                let mut m = Map::new();
                self.skip_ws();
                if self.b[self.i] == b'}' {
                    self.i += 1;
                    return Ok(Value::Object(m));
                }
                loop {
                    self.skip_ws();
                    let k = self.string();
                    self.skip_ws();
                    self.i += 1; // ':'
                    let v = self.value()?;
                    // Go 的 map 语义：重复键后者生效。
                    m.insert(k, v);
                    self.skip_ws();
                    match self.b[self.i] {
                        b',' => {
                            self.i += 1;
                        }
                        b'}' => {
                            self.i += 1;
                            return Ok(Value::Object(m));
                        }
                        _ => unreachable!("check_valid 已保证对象语法"),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut out = Vec::new();
                self.skip_ws();
                if self.b[self.i] == b']' {
                    self.i += 1;
                    return Ok(Value::Array(out));
                }
                loop {
                    out.push(self.value()?);
                    self.skip_ws();
                    match self.b[self.i] {
                        b',' => {
                            self.i += 1;
                        }
                        b']' => {
                            self.i += 1;
                            return Ok(Value::Array(out));
                        }
                        _ => unreachable!("check_valid 已保证数组语法"),
                    }
                }
            }
            b'"' => Ok(Value::String(self.string())),
            b't' => {
                self.i += 4;
                Ok(Value::Bool(true))
            }
            b'f' => {
                self.i += 5;
                Ok(Value::Bool(false))
            }
            b'n' => {
                self.i += 4;
                Ok(Value::Null)
            }
            _ => {
                let start = self.i;
                while self.i < self.b.len()
                    && !matches!(
                        self.b[self.i],
                        b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r'
                    )
                {
                    self.i += 1;
                }
                let lit = String::from_utf8_lossy(&self.b[start..self.i]).to_string();
                match lit.parse::<f64>() {
                    Ok(f) if f.is_finite() => Ok(Value::Number(
                        serde_json::Number::from_f64(f).unwrap_or_else(|| serde_json::Number::from(0)),
                    )),
                    // 溢出（±Inf）：Go 的 convertNumber 用 ParseFloat 的 err 报类型错，文案带原字面量。
                    // 下溢（→0）Go 不报错，Rust 的 parse 同样给 0.0，走上面的正常分支。
                    _ => Err(format!(
                        "json: cannot unmarshal number {} into Go value of type float64",
                        lit
                    )),
                }
            }
        }
    }

    fn string(&mut self) -> String {
        self.i += 1; // 开引号
        let mut out = String::new();
        let mut raw_start = self.i;
        loop {
            let c = self.b[self.i];
            match c {
                b'"' => {
                    if self.i > raw_start {
                        out.push_str(&String::from_utf8_lossy(&self.b[raw_start..self.i]));
                    }
                    self.i += 1;
                    return out;
                }
                b'\\' => {
                    if self.i > raw_start {
                        out.push_str(&String::from_utf8_lossy(&self.b[raw_start..self.i]));
                    }
                    self.i += 1;
                    let e = self.b[self.i];
                    match e {
                        b'"' => {
                            out.push('"');
                            self.i += 1;
                        }
                        b'\\' => {
                            out.push('\\');
                            self.i += 1;
                        }
                        b'/' => {
                            out.push('/');
                            self.i += 1;
                        }
                        b'b' => {
                            out.push('\u{8}');
                            self.i += 1;
                        }
                        b'f' => {
                            out.push('\u{c}');
                            self.i += 1;
                        }
                        b'n' => {
                            out.push('\n');
                            self.i += 1;
                        }
                        b'r' => {
                            out.push('\r');
                            self.i += 1;
                        }
                        b't' => {
                            out.push('\t');
                            self.i += 1;
                        }
                        b'u' => {
                            self.i += 1;
                            let hi = self.hex4();
                            if (0xD800..0xDC00).contains(&hi) {
                                let paired = self.b.get(self.i) == Some(&b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u');
                                if paired {
                                    let save = self.i;
                                    self.i += 2;
                                    let lo = self.hex4();
                                    if (0xDC00..0xE000).contains(&lo) {
                                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                    } else {
                                        self.i = save;
                                        out.push('\u{FFFD}');
                                    }
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                out.push('\u{FFFD}');
                            } else {
                                out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => unreachable!("check_valid 已保证转义合法"),
                    }
                    raw_start = self.i;
                }
                _ => self.i += 1,
            }
        }
    }

    fn hex4(&mut self) -> u32 {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = self.b[self.i];
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => unreachable!("check_valid 已保证 hex 合法"),
            };
            v = v * 16 + d;
            self.i += 1;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_errors_match_go_texts() {
        // 期望值全部来自真 Go 采收（tests/golden/golden-codebuddy-go.json 的 chat-badjson 段）
        let cases: Vec<(&str, &str)> = vec![
            ("", "unexpected end of JSON input"),
            ("   ", "unexpected end of JSON input"),
            ("{", "unexpected end of JSON input"),
            (r#"{"messages" 1}"#, "invalid character '1' after object key"),
            (
                r#"{"messages": tru}"#,
                "invalid character '}' in literal true (expecting 'e')",
            ),
            (r#"{"messages":[],}"#, "invalid character '}' looking for beginning of object key string"),
            (r#"{"messages":[]}x"#, "invalid character 'x' after top-level value"),
            (r#"{"messages":[]}{"a":1}"#, "invalid character '{' after top-level value"),
            (r#"{"messages":[{"role":"user","content":"\q"}]}"#, "invalid character 'q' in string escape code"),
            ("{\"messages\":[{\"role\":\"user\",\"content\":\"a\nb\"}]}", "invalid character '\\n' in string literal"),
        ];
        for (input, want) in cases {
            assert_eq!(unmarshal_map(input.as_bytes()).unwrap_err(), want, "input={input:?}");
        }
    }

    #[test]
    fn top_level_kind_errors_match_go() {
        for (input, kind) in [("[1,2]", "array"), ("\"x\"", "string"), ("42", "number"), ("true", "bool")] {
            assert_eq!(
                unmarshal_map(input.as_bytes()).unwrap_err(),
                format!("json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}")
            );
        }
        assert!(unmarshal_map(b"null").unwrap().is_empty());
    }

    #[test]
    fn duplicate_keys_last_wins_and_numbers_are_float64() {
        let m = unmarshal_map(br#"{"model":"a","model":"b","max_tokens":4096,"t":0.7}"#).unwrap();
        assert_eq!(m["model"], Value::String("b".into()));
        assert_eq!(m["max_tokens"].as_f64(), Some(4096.0));
        assert_eq!(m["t"].as_f64(), Some(0.7));
    }

    #[test]
    fn number_overflow_is_a_type_error() {
        assert_eq!(
            unmarshal_map(br#"{"max_tokens":1e999}"#).unwrap_err(),
            "json: cannot unmarshal number 1e999 into Go value of type float64"
        );
        // 下溢不是错误（Go 实测 1e-999 → 0 且 err=nil）
        assert_eq!(
            unmarshal_map(br#"{"x":1e-999}"#).unwrap()["x"].as_f64(),
            Some(0.0)
        );
    }
}
