//! Go `encoding/json` 兼容的解码器——只覆盖 qoder 的入站请求体 `chatRequest`。
//!
//! 为什么要自己写：`handleChat` 的 400 响应体里带 Go 的**原文错误串**
//! （`请求体解析失败: <err>`），黄金基准把它逐字锁住了；serde 的措辞（"expected value at
//! line 1 column 1" 之类）与 Go 完全不同，用 serde 就对不上。这里只实现 Go 解码器在本
//! 合同上**可观察**的那些分支：
//!
//! * 顶层必须是 object（`null` 视为空请求、无错）；其余类型报
//!   `json: cannot unmarshal <kind> into Go value of type qoder.chatRequest`；
//! * 字段匹配 `model` / `stream` / `messages`：**先精确、再忽略大小写**，重复键**后者生效**；
//! * `null` 落在任一字段上是 no-op（Go 的语义），不是类型错误；
//! * 类型不符报 `json: cannot unmarshal <kind> into Go struct field chatRequest.<字段> of type <Go 类型>`，
//!   且以**文档顺序**第一个出错处为准（Go 是流式逐 token 解码）；
//! * 只取第一个 JSON 值，**尾随数据被忽略**（`json.Decoder.Decode` 的语义）；
//! * 语法错误的文案与 Go 的 scanner 一致（见 `tests/golden/golden-qoder-go.json` 的 once 段）。
//!
//! 未逐字对齐的边界（真实客户端不会走到，登记在 `rs-qoder/README.md`）：非法/孤立代理对
//! 之外的 `\u` 边界、`chatRequest.messages` 里数字字面量溢出时的文案（Go 报
//! `cannot unmarshal number 1e999 …`；`/credits/set` 那条走 `decode_spent_field`，
//! **已按 Go 的原文案对齐**）、非 ASCII 的忽略大小写匹配（Go 用 Unicode simple fold，
//! 这里用 ASCII 折叠）。

use serde_json::Value;

/// 与 Go `internal/qoder.chatRequest` 同形的最小字段集。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub stream: bool,
    pub messages: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Null => "null",
            Kind::Bool => "bool",
            Kind::Number => "number",
            Kind::String => "string",
            Kind::Array => "array",
            Kind::Object => "object",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Model,
    Stream,
    Messages,
}

/// 按 Go 的规则匹配结构体字段名（先精确、再忽略大小写；`chatRequest` 的 json tag 是小写名）。
fn match_field(key: &str) -> Option<Field> {
    let exact = match key {
        "model" => Some(Field::Model),
        "stream" => Some(Field::Stream),
        "messages" => Some(Field::Messages),
        _ => None,
    };
    if exact.is_some() {
        return exact;
    }
    if key.eq_ignore_ascii_case("model") {
        Some(Field::Model)
    } else if key.eq_ignore_ascii_case("stream") {
        Some(Field::Stream)
    } else if key.eq_ignore_ascii_case("messages") {
        Some(Field::Messages)
    } else {
        None
    }
}

/// Go 的 `%q`（rune 版）近似：够覆盖 scanner 报错里会出现的字符。
fn quote_rune(c: char) -> String {
    match c {
        '\'' => "'\\''".to_string(),
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

    /// 跳过空白后取当前字节（并把游标停在该字节上）。
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

    // ---------- 值 ----------

    fn scan_value(&mut self) -> Result<Kind, String> {
        match self.peek() {
            None => Err("unexpected EOF".to_string()),
            Some(b'{') => {
                self.i += 1;
                self.scan_object_body()?;
                Ok(Kind::Object)
            }
            Some(b'[') => {
                self.i += 1;
                self.scan_array_body()?;
                Ok(Kind::Array)
            }
            Some(b'"') => {
                self.scan_string()?;
                Ok(Kind::String)
            }
            Some(b't') => {
                self.scan_lit("true")?;
                Ok(Kind::Bool)
            }
            Some(b'f') => {
                self.scan_lit("false")?;
                Ok(Kind::Bool)
            }
            Some(b'n') => {
                self.scan_lit("null")?;
                Ok(Kind::Null)
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => {
                self.scan_number()?;
                Ok(Kind::Number)
            }
            Some(c) => Err(self.err_at(c, "looking for beginning of value")),
        }
    }

    fn scan_lit(&mut self, lit: &str) -> Result<(), String> {
        for (k, want) in lit.as_bytes().iter().enumerate() {
            match self.b.get(self.i + k) {
                None => return Err("unexpected EOF".to_string()),
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
        // 整数部分：0 | [1-9][0-9]*
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => {
                while self.b.get(self.i).map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    self.i += 1;
                }
            }
            None => return Err("unexpected EOF".to_string()),
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
                    None => Err("unexpected EOF".to_string()),
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
                    None => Err("unexpected EOF".to_string()),
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
                None => return Err("unexpected EOF".to_string()),
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

    /// 调用前游标必须在 `"` 上；返回解码后的字符串（含转义处理，非法 UTF-8 按 Go 回 U+FFFD）。
    fn scan_string(&mut self) -> Result<String, String> {
        self.i += 1; // 吃掉开引号
        let mut out = String::new();
        let mut raw_start = self.i;
        loop {
            let c = match self.b.get(self.i) {
                Some(&c) => c,
                None => return Err("unexpected EOF".to_string()),
            };
            match c {
                b'"' => {
                    if self.i > raw_start {
                        out.push_str(&String::from_utf8_lossy(&self.b[raw_start..self.i]));
                    }
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    if self.i > raw_start {
                        out.push_str(&String::from_utf8_lossy(&self.b[raw_start..self.i]));
                    }
                    self.i += 1;
                    let e = match self.b.get(self.i) {
                        Some(&e) => e,
                        None => return Err("unexpected EOF".to_string()),
                    };
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
                            let hi = self.scan_hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                // 高代理：尝试配一个低代理（Go 的 utf16.DecodeRune 语义）
                                let paired = self.b.get(self.i) == Some(&b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u');
                                if paired {
                                    let save = self.i;
                                    self.i += 2;
                                    let lo = self.scan_hex4()?;
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
                        other => {
                            return Err(format!(
                                "invalid character {} in string escape code",
                                quote_rune(other as char)
                            ))
                        }
                    }
                    raw_start = self.i;
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
        // '[' 已消费
        loop {
            match self.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(_) => {}
            }
            self.scan_value()?;
            match self.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(c) => return Err(self.err_at(c, "after array element")),
            }
        }
    }

    /// 只做语法校验与游标推进（顶层对象以外的地方用）。
    fn scan_object_body(&mut self) -> Result<(), String> {
        loop {
            match self.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(b'"') => {}
                Some(c) => return Err(self.err_at(c, "looking for beginning of object key string")),
            }
            self.scan_string()?;
            match self.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b':') => self.i += 1,
                Some(c) => return Err(self.err_at(c, "after object key")),
            }
            self.scan_value()?;
            match self.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(c) => return Err(self.err_at(c, "after object key:value pair")),
            }
        }
    }
}

fn type_err(kind: Kind, field: Field) -> String {
    let (name, goty) = match field {
        Field::Model => ("model", "string"),
        Field::Stream => ("stream", "bool"),
        Field::Messages => ("messages", "[]interface {}"),
    };
    format!(
        "json: cannot unmarshal {} into Go struct field chatRequest.{} of type {}",
        kind.name(),
        name,
        goty
    )
}

/// Go 侧 `/credits/set` 那个**匿名结构体**在 `reflect` 里的类型串（类型错误文案用它）。
const SPENT_STRUCT_TYPE: &str = "struct { Spent float64 \"json:\\\"spent\\\"\" }";

/// `json.NewDecoder(io.LimitReader(body, 64<<10)).Decode(&struct{Spent float64 `json:"spent"`}{})`
/// 的等价物——Go `handleCreditsSet` 的请求体解析（`server.go:230`）。
///
/// 三条与 serde 全量解析**不同**的 Go 语义，都在这里复刻（真 Go 采收见黄金 `credits_set` 段）：
/// * **只取第一个 JSON 值**，之后的东西一律不看（`Decode` 一次即返回）——
///   `{"spent":0}x` 在 Go 侧是成功的，不是一个解析错误；
/// * 数字**溢出**（`1e999`）时 `strconv.ParseFloat` 报 `ErrRange` ⇒ `UnmarshalTypeError`，
///   文案里带**原字面量**；**下溢**（`1e-999` → 0）Go 不报错，这里同样不报错；
/// * 匹配键按**文档顺序**后者覆盖前者，但任一键解码出错**先返回第一个错误**。
pub fn decode_spent_field(bytes: &[u8]) -> Result<f64, String> {
    let mut p = P { b: bytes, i: 0 };
    let first = match p.peek() {
        // Decode 在空输入上返回 io.EOF（错误串就是 "EOF"）
        None => return Err("EOF".to_string()),
        Some(c) => c,
    };
    if first == b'n' {
        p.scan_lit("null")?;
        return Ok(0.0); // 顶层 null → 结构体零值，无错；尾随数据同样不看
    }
    if first != b'{' {
        let k = p.scan_value()?;
        return Err(format!(
            "json: cannot unmarshal {} into Go value of type {}",
            k.name(),
            SPENT_STRUCT_TYPE
        ));
    }
    p.i += 1; // '{'

    let mut spent = 0.0f64;
    loop {
        match p.peek() {
            None => return Err("unexpected EOF".to_string()),
            // 第一个值到此结束：Go 的 Decode 不再往下看，尾随数据合法与否都无所谓
            Some(b'}') => break,
            Some(b'"') => {}
            Some(c) => return Err(p.err_at(c, "looking for beginning of object key string")),
        }
        let key = p.scan_string()?;
        match p.peek() {
            None => return Err("unexpected EOF".to_string()),
            Some(b':') => p.i += 1,
            Some(c) => return Err(p.err_at(c, "after object key")),
        }
        if key == "spent" || key.eq_ignore_ascii_case("spent") {
            spent = match p.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b'n') => {
                    p.scan_lit("null")?;
                    spent // null 落在 float64 上是 no-op（Go 的语义）
                }
                Some(c) if c == b'-' || c.is_ascii_digit() => {
                    let start = p.i;
                    p.scan_number()?;
                    let lit = String::from_utf8_lossy(&p.b[start..p.i]).to_string();
                    // Go 的 ParseFloat 对溢出（结果 ±Inf）报 ErrRange；下溢回 0 且**不报错**，
                    // Rust 的 parse 同样把下溢解成 0.0、溢出解成 ±inf ⇒ 判据取 is_infinite。
                    let v: f64 = lit.parse().unwrap_or(f64::INFINITY);
                    if v.is_infinite() {
                        return Err(format!(
                            "json: cannot unmarshal number {lit} into Go struct field .spent of type float64"
                        ));
                    }
                    v
                }
                Some(_) => {
                    let k = p.scan_value()?;
                    return Err(format!(
                        "json: cannot unmarshal {} into Go struct field .spent of type float64",
                        k.name()
                    ));
                }
            };
        } else {
            p.scan_value()?;
        }
        match p.peek() {
            None => return Err("unexpected EOF".to_string()),
            Some(b',') => p.i += 1,
            Some(b'}') => break,
            Some(c) => return Err(p.err_at(c, "after object key:value pair")),
        }
    }
    Ok(spent)
}

/// 按 Go `json.Decoder.Decode` 的语义解码 chatRequest；返回的错误串即 Go 的原文文案。
pub fn decode_chat_request(bytes: &[u8]) -> Result<ChatRequest, String> {
    let mut p = P { b: bytes, i: 0 };
    let first = match p.peek() {
        // go 的 Decode 在空输入上返回 io.EOF（错误串就是 "EOF"）
        None => return Err("EOF".to_string()),
        Some(c) => c,
    };
    if first == b'n' {
        p.scan_lit("null")?;
        return Ok(ChatRequest::default()); // null → 结构体零值，无错
    }
    if first != b'{' {
        let k = p.scan_value()?;
        return Err(format!(
            "json: cannot unmarshal {} into Go value of type qoder.chatRequest",
            k.name()
        ));
    }
    p.i += 1; // '{'

    let mut req = ChatRequest::default();
    loop {
        match p.peek() {
            None => return Err("unexpected EOF".to_string()),
            Some(b'}') => {
                // 对象结束：Go 的 Decode 到此就返回了（尾随数据不解析），游标不必再推进
                break;
            }
            Some(b'"') => {}
            Some(c) => return Err(p.err_at(c, "looking for beginning of object key string")),
        }
        let key = p.scan_string()?;
        match p.peek() {
            None => return Err("unexpected EOF".to_string()),
            Some(b':') => p.i += 1,
            Some(c) => return Err(p.err_at(c, "after object key")),
        }
        match match_field(&key) {
            None => {
                p.scan_value()?;
            }
            Some(Field::Model) => match p.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b'n') => {
                    p.scan_lit("null")?;
                    req.model.clear();
                }
                Some(b'"') => req.model = p.scan_string()?,
                Some(_) => {
                    let k = p.scan_value()?;
                    return Err(type_err(k, Field::Model));
                }
            },
            Some(Field::Stream) => match p.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b'n') => {
                    p.scan_lit("null")?;
                    req.stream = false;
                }
                Some(b't') => {
                    p.scan_lit("true")?;
                    req.stream = true;
                }
                Some(b'f') => {
                    p.scan_lit("false")?;
                    req.stream = false;
                }
                Some(_) => {
                    let k = p.scan_value()?;
                    return Err(type_err(k, Field::Stream));
                }
            },
            Some(Field::Messages) => match p.peek() {
                None => return Err("unexpected EOF".to_string()),
                Some(b'n') => {
                    p.scan_lit("null")?;
                    req.messages.clear();
                }
                Some(b'[') => {
                    let start = p.i;
                    p.i += 1; // 消费 '['（scan_array_body 的约定与 Go 的 scanner 一致）
                    p.scan_array_body()?;
                    let end = p.i;
                    // 语法已经手工校验过，这里只做取值；失败（例如数字溢出）按 Go 的类型
                    // 错误口径兜底。
                    match serde_json::from_slice::<Vec<Value>>(&p.b[start..end]) {
                        Ok(v) => req.messages = v,
                        Err(_) => {
                            return Err(
                                "json: cannot unmarshal number into Go value of type float64"
                                    .to_string(),
                            )
                        }
                    }
                }
                Some(_) => {
                    let k = p.scan_value()?;
                    return Err(type_err(k, Field::Messages));
                }
            },
        }
        match p.peek() {
            None => return Err("unexpected EOF".to_string()),
            Some(b',') => p.i += 1,
            Some(b'}') => {
                // 同上：解析到此结束
                break;
            }
            Some(c) => return Err(p.err_at(c, "after object key:value pair")),
        }
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(body: &str) -> String {
        decode_chat_request(body.as_bytes()).unwrap_err()
    }

    // 期望串直接取自 tests/golden/golden-qoder-go.json 的 once 段（真 Go 采收）。
    #[test]
    fn error_messages_match_go_golden() {
        assert_eq!(err(""), "EOF");
        assert_eq!(err("   "), "EOF");
        assert_eq!(
            err("\"hi\""),
            "json: cannot unmarshal string into Go value of type qoder.chatRequest"
        );
        assert_eq!(
            err("[1,2]"),
            "json: cannot unmarshal array into Go value of type qoder.chatRequest"
        );
        assert_eq!(err("x"), "invalid character 'x' looking for beginning of value");
        assert_eq!(
            err("{1:2}"),
            "invalid character '1' looking for beginning of object key string"
        );
        assert_eq!(err("{\"messages\":[{\"role\":\"user\""), "unexpected EOF");
        assert_eq!(
            err("{not json"),
            "invalid character 'n' looking for beginning of object key string"
        );
        assert_eq!(
            err("{\"model\":123}"),
            "json: cannot unmarshal number into Go struct field chatRequest.model of type string"
        );
        assert_eq!(
            err("{\"stream\":\"yes\"}"),
            "json: cannot unmarshal string into Go struct field chatRequest.stream of type bool"
        );
        assert_eq!(
            err("{\"messages\":\"x\"}"),
            "json: cannot unmarshal string into Go struct field chatRequest.messages of type []interface {}"
        );
        assert_eq!(
            err("{\"model\":{}}"),
            "json: cannot unmarshal object into Go struct field chatRequest.model of type string"
        );
        assert_eq!(
            err("{\"model\":[]}"),
            "json: cannot unmarshal array into Go struct field chatRequest.model of type string"
        );
        assert_eq!(
            err("{\"messages\":true}"),
            "json: cannot unmarshal bool into Go struct field chatRequest.messages of type []interface {}"
        );
        assert_eq!(
            err("{\"stream\":1}"),
            "json: cannot unmarshal number into Go struct field chatRequest.stream of type bool"
        );
    }

    #[test]
    fn success_semantics_match_go() {
        // 重复键后者生效
        let r = decode_chat_request(
            br#"{"model":"glm-5.3","model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(r.model, "deepseek-v4-flash");

        // 尾随数据被忽略（json.Decoder.Decode 只取第一个值）
        let r = decode_chat_request(br#"{"model":"auto","messages":[{"role":"user","content":"hi"}]}TRAILING"#)
            .unwrap();
        assert_eq!(r.model, "auto");
        assert_eq!(r.messages.len(), 1);

        // 未知字段被忽略；model 先精确、再忽略大小写
        let r = decode_chat_request(br#"{"MODEL":"deepseek-v4-flash","extra":{"deep":1}}"#).unwrap();
        assert_eq!(r.model, "deepseek-v4-flash");

        // null 落在字段上是 no-op
        let r = decode_chat_request(br#"{"model":null,"stream":null,"messages":null}"#).unwrap();
        assert!(r.model.is_empty() && !r.stream && r.messages.is_empty());

        // 顶层 null 是空请求
        let r = decode_chat_request(b"null").unwrap();
        assert_eq!(r, ChatRequest::default());

        // stream 真值
        let r = decode_chat_request(br#"{"stream":true}"#).unwrap();
        assert!(r.stream);
    }
}
