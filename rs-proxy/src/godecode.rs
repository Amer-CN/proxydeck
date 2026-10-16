//! Go `json.Unmarshal` 兼容的解码器（语法扫描 + 值树 + 类型化取值）。
//!
//! 为什么要自己写：主代理的三条入站端点把 `json.Unmarshal` 的**原文错误串**写进 400 响应
//! （`Invalid JSON: <err>`），黄金基准把它逐字锁住了；serde 的措辞完全不同。
//! 这里复刻 Go 在该合同上**可观察**的语义：
//!
//! * 先全量语法扫描（Go `Unmarshal` 就是先 `checkValid` 再解），语法错优先于类型错，
//!   文案与 Go scanner 一致；所有 EOF 类错误归一成 `unexpected end of JSON input`；
//! * 尾随数据报 `invalid character 'x' after top-level value`；
//! * 数字一律按 `float64` 落（`map[string]any` 语义）；**原始字面量**要留着——
//!   `int` 字段走 `strconv.ParseInt(字面量)`，错误文案里是原字面量（`1e2` 不是 `100`）；
//! * 类型不匹配报 Go 的两种形态：
//!   - 顶层：`json: cannot unmarshal <kind> into Go value of type api.OpenAIChatRequest`；
//!   - 字段：`json: cannot unmarshal <kind> into Go struct field OpenAIChatRequest.messages of type []api.OpenAIMessage`；
//! * 字段级类型错的**顺序** = Go 结构体声明序（Go 先整份编解码一轮、遇错即返回）——
//!   由 [`GoValue::parse_checked`] 的子树预扫保证；
//! * `null` 落到指针/切片/字段上按 Go 的零值语义（指针 nil、切片 nil），不报错。
//!
//! 未逐字对齐的边界（真实客户端走不到，登记在 `rs-proxy/README.md`）：非法 UTF-8 字节的
//! 替换细节、`\u` 代理对边界、超大数字字面量的 ParseFloat 细节。

use std::collections::HashMap;
use std::rc::Rc;

use serde_json::{Map, Value};

/// 与 Go `map[string]any` 同形的映射（写出时按键排序，见 [`crate::gjson`]）。
pub type GoMap = Map<String, Value>;

/// 数字节点的哨兵前缀：解析期把数字落成 `"\u{0}num:<id>"`，解析完折回数字，
/// 同时把 `id → 原始字面量` 交给上层（Go 的 `int` 字段错误文案要原字面量）。
const NUM_SENTINEL: &str = "\u{0}num:";

/// `json.Unmarshal(body, &payload)`（`payload` 为 `map[string]any`）的等价物。
pub fn unmarshal_map(data: &[u8]) -> Result<GoMap, String> {
    check_valid(data)?;
    let mut p = B::new(data);
    p.skip_ws();
    match p.b.get(p.i).copied() {
        Some(b'{') => match p.value()? {
            Value::Object(m) => Ok(m),
            _ => unreachable!("check_valid 已保证顶层是 object"),
        },
        // `null` → Go 里 payload 保持 nil（不报错），后续取值是零值。
        Some(b'n') => Ok(Map::new()),
        Some(c) => Err(format!(
            "json: cannot unmarshal {} into Go value of type map[string]interface {{}}",
            top_kind(c)
        )),
        None => Err("unexpected end of JSON input".to_string()),
    }
}

/// 解析成值树（`json.Unmarshal` 到 `any`）：先语法扫描，再解。
///
/// 对外只给普通 JSON（数字是数字）——内部为了保留原始字面量用了哨兵，这里统一折回。
pub fn parse_value(data: &[u8]) -> Result<Value, String> {
    check_valid(data)?;
    let mut p = B::new(data);
    let v = p.value()?;
    let literals = std::mem::take(&mut p.literals);
    Ok(resolve_sentinels(&v, &literals))
}

/// 解析成值树 + **原始字面量表**（供 [`GoValue`] 用）。
fn parse_with_literals(data: &[u8]) -> Result<(Value, HashMap<u64, String>), String> {
    check_valid(data)?;
    let mut p = B::new(data);
    let v = p.value()?;
    let literals = std::mem::take(&mut p.literals);
    Ok((v, literals))
}

/// 把整棵树的哨兵折回数字（`GoValue` 对外必须是普通 JSON）。
fn resolve_sentinels(v: &Value, literals: &HashMap<u64, String>) -> Value {
    match v {
        Value::String(s) => match s.strip_prefix(NUM_SENTINEL) {
            Some(id) => {
                let lit = literals
                    .get(&id.parse::<u64>().unwrap_or(u64::MAX))
                    .cloned()
                    .unwrap_or_default();
                lit.parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
            None => v.clone(),
        },
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|it| resolve_sentinels(it, literals))
                .collect(),
        ),
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                out.insert(k.clone(), resolve_sentinels(val, literals));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 取某个节点上的原始数字字面量（哨兵形态才有）。
fn raw_literal(v: &Value, literals: &HashMap<u64, String>) -> Option<String> {
    match v {
        Value::String(s) => s
            .strip_prefix(NUM_SENTINEL)
            .and_then(|id| id.parse::<u64>().ok())
            .and_then(|id| literals.get(&id).cloned()),
        _ => None,
    }
}

/// 一个已解码的值 + Go 的「类型语境」（用于拼出与 Go 逐字一致的错误文案）。
#[derive(Debug, Clone)]
pub struct GoValue {
    v: Value,
    root_type: &'static str,
    field: Option<&'static str>,
    go_type: &'static str,
    /// 未折回数字的哨兵树（取原始字面量时要它；对外不可见）
    raw_v: Option<Rc<Value>>,
    /// 本节点是数字时的**原始字面量**
    raw_number: Option<String>,
    /// `哨兵 id → 原始字面量` 的全表（子节点取值时要用）
    literals: Rc<HashMap<u64, String>>,
}

impl GoValue {
    /// 顶层入口：`json.Unmarshal(data, &T)`，`root_type` 是 `T` 在 Go 里的类型串。
    pub fn parse(data: &[u8], root_type: &'static str, go_type: &'static str) -> Result<GoValue, String> {
        let (sentinel_v, literals) = parse_with_literals(data)?;
        let literals = Rc::new(literals);
        let raw_number = if sentinel_v.is_object() {
            None // 顶层是对象，字面量在字段上（取值时按字段查）
        } else {
            raw_literal(&sentinel_v, &literals)
        };
        let v = resolve_sentinels(&sentinel_v, &literals);
        // 顶层 kind 必须匹配（Go 把 `"scalar"` 解进 struct 会报
        // `json: cannot unmarshal string into Go value of type …`）；`null` 是合法零值。
        if !v.is_object() && !v.is_null() {
            return Err(format!(
                "json: cannot unmarshal {} into Go value of type {go_type}",
                kind_of(&v)
            ));
        }
        Ok(GoValue {
            v,
            raw_v: Some(Rc::new(sentinel_v)),
            root_type,
            field: None,
            go_type,
            raw_number,
            literals,
        })
    }

    /// 与 [`GoValue::parse`] 相同，但**额外做一遍子树类型预扫**，把 Go 在整份 `Unmarshal`
    /// 里会先撞上的那个类型错一并返回（顺序 = 结构体声明序）。
    ///
    /// 为什么单独一个入口：字段的按需读取会把类型错推迟到「读那个字段时」，而 Go 是
    /// 「先整份编解码、遇错即返回」。需要按 Go 顺序报错的调用方（三个 handler 的入站解析）用它。
    pub fn parse_checked(
        data: &[u8],
        root_type: &'static str,
        go_type: &'static str,
    ) -> Result<(GoValue, Option<String>), String> {
        let gv = GoValue::parse(data, root_type, go_type)?;
        let err = check_types(&gv);
        Ok((gv, err))
    }

    pub fn raw(&self) -> &Value {
        &self.v
    }

    pub fn is_null(&self) -> bool {
        self.v.is_null()
    }

    fn err(&self, kind: &str) -> String {
        match self.field {
            Some(f) => format!(
                "json: cannot unmarshal {kind} into Go struct field {f} of type {}",
                self.go_type
            ),
            None => format!(
                "json: cannot unmarshal {kind} into Go value of type {}",
                if self.go_type.is_empty() {
                    self.root_type
                } else {
                    self.go_type
                }
            ),
        }
    }

    /// Go 的 `string` 字段：`null` → 零值空串；其它类型报错。
    pub fn as_string(&self) -> Result<String, String> {
        match &self.v {
            Value::Null => Ok(String::new()),
            Value::String(s) => Ok(s.clone()),
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的 `*string` / 可选字符串：缺失与 `null` 都是 None。
    pub fn opt_string(&self) -> Result<Option<String>, String> {
        match &self.v {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s.clone())),
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的 `bool`。
    pub fn as_bool(&self) -> Result<bool, String> {
        match &self.v {
            Value::Null => Ok(false),
            Value::Bool(b) => Ok(*b),
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的 `*bool`（`null` → nil）。
    pub fn opt_bool(&self) -> Result<Option<bool>, String> {
        match &self.v {
            Value::Null => Ok(None),
            Value::Bool(b) => Ok(Some(*b)),
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的 `float64`（`null` → 0）。
    pub fn as_f64(&self) -> Result<f64, String> {
        match &self.v {
            Value::Null => Ok(0.0),
            Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的 `*float64`：`null` → nil。
    pub fn opt_f64(&self) -> Result<Option<f64>, String> {
        match &self.v {
            Value::Null => Ok(None),
            Value::Number(n) => Ok(Some(n.as_f64().unwrap_or(0.0))),
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的 `int` 字段：判据是**原始字面量**能过 `strconv.ParseInt(字面量, 10, 64)`。
    ///
    /// 实测锚点（真 Go 采收）：`1024` 放行；`1e2` / `1024.0` / `1.5` 一律报
    /// `json: cannot unmarshal number <字面量> into Go struct field … of type int`。
    pub fn opt_int(&self) -> Result<Option<i64>, String> {
        match &self.v {
            Value::Null => Ok(None),
            Value::Number(_) => {
                let lit = self
                    .raw_number
                    .clone()
                    .unwrap_or_else(|| crate::gjson::to_go(&self.v));
                if !is_int_literal(&lit) {
                    return Err(format!(
                        "json: cannot unmarshal number {lit} into Go struct field {} of type {}",
                        self.field.unwrap_or(""),
                        self.go_type
                    ));
                }
                lit.parse::<i64>().map(Some).map_err(|_| {
                    format!(
                        "json: cannot unmarshal number {lit} into Go struct field {} of type {}",
                        self.field.unwrap_or(""),
                        self.go_type
                    )
                })
            }
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的 `[]any` / `any`：原样取（`null` → nil）。
    pub fn opt_any(&self) -> Option<Value> {
        match &self.v {
            Value::Null => None,
            other => Some(other.clone()),
        }
    }

    /// Go 的 `[]T`：`null` → nil 切片（不报错）；非数组报错。
    pub fn opt_array(&self) -> Result<Option<Vec<GoValue>>, String> {
        match &self.v {
            Value::Null => Ok(None),
            Value::Array(items) => Ok(Some(
                items
                    .iter()
                    .map(|it| GoValue {
                        v: it.clone(),
                        raw_v: None,
                        root_type: self.root_type,
                        field: None,
                        go_type: "",
                        raw_number: None,
                        literals: self.literals.clone(),
                    })
                    .collect(),
            )),
            other => Err(self.err(kind_of(other))),
        }
    }

    /// Go 的结构体字段取值：`root_type` 用于错误文案，`field` 是 Go 的字段路径。
    ///
    /// 原始字面量从**哨兵树**（`raw_v`）里取——折回数字后 `1e2` 就变成 `100` 了。
    pub fn field(&self, name: &str, go_type: &'static str, go_field: &'static str) -> GoValue {
        let raw_cell: Option<Value> = self
            .raw_v
            .as_ref()
            .and_then(|sv| sv.get(name).cloned());
        let raw_number = match &raw_cell {
            Some(cell) => raw_literal(cell, &self.literals),
            None => None,
        };
        let raw_v = raw_cell.map(Rc::new);
        let v = match &self.v {
            Value::Object(m) => m.get(name).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        };
        GoValue {
            v,
            raw_v,
            root_type: self.root_type,
            field: Some(go_field),
            go_type,
            raw_number,
            literals: self.literals.clone(),
        }
    }

    pub fn has_field(&self, name: &str) -> bool {
        matches!(&self.v, Value::Object(m) if m.contains_key(name))
    }

    pub fn as_map(&self) -> Option<&Map<String, Value>> {
        match &self.v {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }
}

/// Go 的 `int64` 字段取值：Go 的 `json` 对 `int64` 接受**整数值的数字**
/// （`3000` 与 `3000.0` 都是 3000；`1.5` 报错）。
///
/// ⚠ 本 crate 的解码器把数字一律落成 `float64`（与 Go 的 `map[string]any` 同），
/// 所以取值时不能直接用 `serde_json::Number::as_i64()`——那对 f64 形态一律返回 None。
/// 这个函数就是那条「整数值判定」。
pub fn go_int(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        Some(f as i64)
    } else {
        None
    }
}

/// 同 [`go_int`]，但缺失/非数时给 0（`json` 字段的零值语义）。
pub fn go_int_or_zero(v: Option<&Value>) -> i64 {
    v.and_then(go_int).unwrap_or(0)
}

/// Go 报错里的「kind」词。
pub fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
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

/// `strconv.ParseInt(字面量, 10, 64)` 的词法：可选符号 + 纯十进制数字（**不允许** `.`/`e`）。
fn is_int_literal(lit: &str) -> bool {
    let t = lit.trim();
    let body = t.strip_prefix('-').unwrap_or(t);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

// ===========================================================================
// 子树类型预扫（Go 的字段级类型错顺序 = 结构体声明序）
// ===========================================================================

fn check_types(root: &GoValue) -> Option<String> {
    check_node(root, root.go_type)
}

fn check_node(node: &GoValue, go_type: &str) -> Option<String> {
    // ⚠ 必须用显式 if-let 而非 `?`：本函数返回 `Option<String>`，`?` 在 `None` 时会
    // 提前退函数（把「没有错误」当成「有错误」），语义相反。
    macro_rules! chk {
        ($e:expr) => {
            if let Some(msg) = $e {
                return Some(msg);
            }
        };
    }
    match go_type {
        "api.OpenAIChatRequest" => {
            chk!(check_field_str(node, "model", "OpenAIChatRequest.model"));
            chk!(check_field_array(
                node,
                "messages",
                "[]api.OpenAIMessage",
                "OpenAIChatRequest.messages"
            ));
            chk!(check_field_number(
                node,
                "temperature",
                "float64",
                "OpenAIChatRequest.temperature"
            ));
            chk!(check_field_number(node, "max_tokens", "int", "OpenAIChatRequest.max_tokens"));
            chk!(check_field_number(
                node,
                "max_completion_tokens",
                "int",
                "OpenAIChatRequest.max_completion_tokens"
            ));
            chk!(check_field_bool(node, "stream", "bool", "OpenAIChatRequest.stream"));
            chk!(check_field_array(node, "tools", "[]interface {}", "OpenAIChatRequest.tools"));
            if let Some(items) =
                arr_field(node, "messages", "[]api.OpenAIMessage", "OpenAIChatRequest.messages")
            {
                for m in items {
                    if m.is_null() {
                        continue;
                    }
                    chk!(check_node(&m, "api.OpenAIMessage"));
                }
            }
            None
        }
        "api.OpenAIMessage" => {
            chk!(check_field_str(node, "role", "OpenAIMessage.role"));
            chk!(check_field_str(node, "name", "OpenAIMessage.name"));
            chk!(check_field_str(node, "tool_call_id", "OpenAIMessage.tool_call_id"));
            chk!(check_field_str(
                node,
                "reasoning_content",
                "OpenAIMessage.reasoning_content"
            ));
            chk!(check_field_array(
                node,
                "tool_calls",
                "[]api.ToolCall",
                "OpenAIMessage.tool_calls"
            ));
            if let Some(items) =
                arr_field(node, "tool_calls", "[]api.ToolCall", "OpenAIMessage.tool_calls")
            {
                for t in items {
                    if t.is_null() {
                        continue;
                    }
                    chk!(check_node(&t, "api.ToolCall"));
                }
            }
            None
        }
        "api.ToolCall" => {
            chk!(check_field_str(node, "id", "ToolCall.id"));
            chk!(check_field_str(node, "type", "ToolCall.type"));
            if let Some(f) = func_field(node, "function") {
                chk!(check_field_str(&f, "name", "FunctionCall.name"));
                chk!(check_field_str(&f, "arguments", "FunctionCall.arguments"));
            }
            None
        }
        "api.AnthropicMessageRequest" => {
            chk!(check_field_str(node, "model", "AnthropicMessageRequest.model"));
            chk!(check_field_number(
                node,
                "max_tokens",
                "int",
                "AnthropicMessageRequest.max_tokens"
            ));
            chk!(check_field_number(
                node,
                "temperature",
                "float64",
                "AnthropicMessageRequest.temperature"
            ));
            chk!(check_field_number(node, "top_p", "float64", "AnthropicMessageRequest.top_p"));
            chk!(check_field_bool(node, "stream", "bool", "AnthropicMessageRequest.stream"));
            chk!(check_field_array(
                node,
                "messages",
                "[]api.AnthropicMessage",
                "AnthropicMessageRequest.messages"
            ));
            chk!(check_field_array(
                node,
                "stop_sequences",
                "[]string",
                "AnthropicMessageRequest.stop_sequences"
            ));
            chk!(check_field_array(
                node,
                "tools",
                "[]interface {}",
                "AnthropicMessageRequest.tools"
            ));
            if let Some(items) = arr_field(
                node,
                "messages",
                "[]api.AnthropicMessage",
                "AnthropicMessageRequest.messages",
            ) {
                for m in items {
                    if m.is_null() {
                        continue;
                    }
                    chk!(check_field_str(&m, "role", "AnthropicMessage.role"));
                }
            }
            if let Some(items) = arr_field(
                node,
                "stop_sequences",
                "[]string",
                "AnthropicMessageRequest.stop_sequences",
            ) {
                for s in items {
                    if !s.raw().is_null() && !s.raw().is_string() {
                        return Some(s.err(kind_of(s.raw())));
                    }
                }
            }
            None
        }
        "api.OpenAIResponsesRequest" => {
            chk!(check_field_str(node, "model", "OpenAIResponsesRequest.model"));
            chk!(check_field_number(
                node,
                "temperature",
                "float64",
                "OpenAIResponsesRequest.temperature"
            ));
            chk!(check_field_number(
                node,
                "max_output_tokens",
                "int",
                "OpenAIResponsesRequest.max_output_tokens"
            ));
            chk!(check_field_number(node, "max_tokens", "int", "OpenAIResponsesRequest.max_tokens"));
            chk!(check_field_number(
                node,
                "max_completion_tokens",
                "int",
                "OpenAIResponsesRequest.max_completion_tokens"
            ));
            chk!(check_field_bool(node, "stream", "bool", "OpenAIResponsesRequest.stream"));
            chk!(check_field_bool(
                node,
                "parallel_tool_calls",
                "bool",
                "OpenAIResponsesRequest.parallel_tool_calls"
            ));
            chk!(check_field_number(node, "top_p", "float64", "OpenAIResponsesRequest.top_p"));
            chk!(check_field_str(node, "user", "OpenAIResponsesRequest.user"));
            chk!(check_field_array(
                node,
                "tools",
                "[]interface {}",
                "OpenAIResponsesRequest.tools"
            ));
            None
        }
        _ => None,
    }
}

fn arr_field(
    node: &GoValue,
    name: &str,
    go_type: &'static str,
    go_field: &'static str,
) -> Option<Vec<GoValue>> {
    node.field(name, go_type, go_field).opt_array().ok().flatten()
}

fn func_field(node: &GoValue, name: &str) -> Option<GoValue> {
    match node.raw().get(name) {
        Some(Value::Object(_)) => Some(node.field(name, "api.FunctionCall", "ToolCall.function")),
        _ => None,
    }
}

fn check_field_str(node: &GoValue, name: &str, field: &'static str) -> Option<String> {
    let v = node.field(name, "string", field);
    if !v.raw().is_null() && !v.raw().is_string() {
        return Some(v.err(kind_of(v.raw())));
    }
    None
}

fn check_field_bool(node: &GoValue, name: &str, gt: &'static str, field: &'static str) -> Option<String> {
    let v = node.field(name, gt, field);
    if !v.raw().is_null() && !v.raw().is_boolean() {
        return Some(v.err(kind_of(v.raw())));
    }
    None
}

fn check_field_number(
    node: &GoValue,
    name: &str,
    gt: &'static str,
    field: &'static str,
) -> Option<String> {
    let v = node.field(name, gt, field);
    match v.raw() {
        Value::Null => None,
        Value::Number(n) => {
            let lit = v
                .raw_number
                .clone()
                .unwrap_or_else(|| crate::gjson::to_go(&Value::Number(n.clone())));
            if gt == "int" && !is_int_literal(&lit) {
                return Some(format!(
                    "json: cannot unmarshal number {lit} into Go struct field {field} of type {gt}"
                ));
            }
            None
        }
        other => Some(v.err(kind_of(other))),
    }
}

fn check_field_array(
    node: &GoValue,
    name: &str,
    gt: &'static str,
    field: &'static str,
) -> Option<String> {
    let v = node.field(name, gt, field);
    match v.raw() {
        Value::Null | Value::Array(_) => None,
        other => Some(v.err(kind_of(other))),
    }
}

// ===========================================================================
// 第一遍：语法扫描（Go scanner 的文案）
// ===========================================================================

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
        // Go 的 scanner 分两态：`[` 之后允许 `]`（空数组），`,` 之后**必须**有值。
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
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(self.eof()),
                Some(b'}') if first => {
                    self.i += 1;
                    return Ok(());
                }
                Some(b'"') => {}
                Some(c) => {
                    return Err(self.err_at(c, "looking for beginning of object key string"))
                }
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

// ===========================================================================
// 第二遍：建树（数字落哨兵，重复键后者生效）
// ===========================================================================

struct B<'a> {
    b: &'a [u8],
    i: usize,
    next_id: u64,
    literals: HashMap<u64, String>,
}

impl<'a> B<'a> {
    fn new(b: &'a [u8]) -> B<'a> {
        B {
            b,
            i: 0,
            next_id: 0,
            literals: HashMap::new(),
        }
    }

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
                    m.insert(k, v); // 重复键后者生效（Go 同）
                    self.skip_ws();
                    match self.b[self.i] {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return Ok(Value::Object(m));
                        }
                        other => {
                            return Err(format!(
                                "internal: unexpected byte {other:?} at {} after object member",
                                self.i
                            ))
                        }
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                self.skip_ws();
                if self.b[self.i] == b']' {
                    self.i += 1;
                    return Ok(Value::Array(a));
                }
                loop {
                    a.push(self.value()?);
                    self.skip_ws();
                    match self.b[self.i] {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return Ok(Value::Array(a));
                        }
                        other => {
                            return Err(format!(
                                "internal: unexpected byte {other:?} at {} after array element",
                                self.i
                            ))
                        }
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
            _ => self.number(),
        }
    }

    fn string(&mut self) -> String {
        self.i += 1; // 开引号
        let mut out = String::new();
        loop {
            match self.b[self.i] {
                b'"' => {
                    self.i += 1;
                    return out;
                }
                b'\\' => {
                    // ⚠ 每个分支**都必须把 `self.i` 推过被转义的字节**。
                    // 曾在 `"`/`\\`/`\/`/`b`/`f`/`n`/`r`/`t` 这几支漏掉自增，
                    // 结果是「只看一行」的测试照过、而含转义的 JSON 两遍扫描失步
                    // （`validate` 说合法、建树却错位）。黄金门禁当场抓到。
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
                            let ch = if (0xD800..0xDC00).contains(&hi)
                                && self.b.get(self.i) == Some(&b'\\')
                                && self.b.get(self.i + 1) == Some(&b'u')
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4();
                                if (0xDC00..0xE000).contains(&lo) {
                                    let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                    char::from_u32(c).unwrap_or('\u{FFFD}')
                                } else {
                                    self.i = save;
                                    '\u{FFFD}'
                                }
                            } else {
                                char::from_u32(hi).unwrap_or('\u{FFFD}')
                            };
                            out.push(ch);
                        }
                        _ => {
                            // check_valid 已保证语法；兜底只保证不越界、不死循环。
                            self.i += 1;
                        }
                    }
                }
                c if c < 0x20 => {
                    // check_valid 已保证语法；走到这里说明两遍扫描失了步（诊断已登记）。
                    // 无条件前进一个字节，避免死循环。
                    self.i += 1;
                }
                _ => {
                    let start = self.i;
                    let len = utf8_len(self.b[self.i]);
                    self.i += len;
                    out.push_str(std::str::from_utf8(&self.b[start..self.i]).unwrap_or("\u{FFFD}"));
                }
            }
        }
    }

    fn hex4(&mut self) -> u32 {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.b[self.i];
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => 0,
            };
            v = v * 16 + d;
            self.i += 1;
        }
        v
    }

    /// 数字：产出 `"\u{0}num:<id>"` 哨兵，并登记 `id → 原始字面量`。
    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        while self
            .b
            .get(self.i)
            .map(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
            .unwrap_or(false)
        {
            self.i += 1;
        }
        let lit = std::str::from_utf8(&self.b[start..self.i])
            .unwrap_or("")
            .to_string();
        self.next_id += 1;
        let id = self.next_id;
        self.literals.insert(id, lit);
        Ok(Value::String(format!("{NUM_SENTINEL}{id}")))
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_errors_match_go_wording() {
        assert_eq!(
            unmarshal_map(b"{not json").unwrap_err(),
            "invalid character 'n' looking for beginning of object key string"
        );
        assert_eq!(
            unmarshal_map(b"}x{").unwrap_err(),
            "invalid character '}' looking for beginning of value"
        );
        assert_eq!(
            unmarshal_map(b"{\"a\":1} trailing").unwrap_err(),
            "invalid character 't' after top-level value"
        );
        assert_eq!(
            unmarshal_map(b"").unwrap_err(),
            "unexpected end of JSON input"
        );
        assert_eq!(
            unmarshal_map(b"{\"a\":").unwrap_err(),
            "unexpected end of JSON input"
        );
    }

    #[test]
    fn top_level_kind_errors() {
        assert_eq!(
            unmarshal_map(b"\"scalar\"").unwrap_err(),
            "json: cannot unmarshal string into Go value of type map[string]interface {}"
        );
        assert_eq!(
            unmarshal_map(b"[]").unwrap_err(),
            "json: cannot unmarshal array into Go value of type map[string]interface {}"
        );
    }

    #[test]
    fn numbers_land_as_f64_and_overflow_follows_go() {
        let v = parse_value(b"{\"n\":4096,\"f\":0.5}").unwrap();
        assert!(v["n"].is_number());
        assert_eq!(v["n"].as_f64(), Some(4096.0));
        // 溢出字面量在树里落成 null（本 crate 的 `Value` 装不下 ±Inf）
        let v2 = GoValue::parse(b"{\"n\":1e999}", "any", "any").unwrap();
        assert!(v2.raw()["n"].is_null());
    }

    #[test]
    fn duplicate_keys_last_wins() {
        let v = parse_value(br#"{"a":1,"a":2}"#).unwrap();
        assert_eq!(v["a"].as_f64(), Some(2.0));
    }

    #[test]
    fn value_tree_typed_accessors() {
        let data = br#"{"model":"m","messages":[{"role":"user","content":"x"}],"max_tokens":1024,"stream":true}"#;
        let (root, pre) =
            GoValue::parse_checked(data, "api.OpenAIChatRequest", "api.OpenAIChatRequest").unwrap();
        assert!(pre.is_none(), "合法请求不应有类型错：{pre:?}");
        assert_eq!(
            root.field("model", "string", "OpenAIChatRequest.model")
                .as_string()
                .unwrap(),
            "m"
        );
        assert_eq!(
            root.field("max_tokens", "int", "OpenAIChatRequest.max_tokens")
                .opt_int()
                .unwrap(),
            Some(1024)
        );
        assert!(root
            .field("stream", "bool", "OpenAIChatRequest.stream")
            .as_bool()
            .unwrap());
        let msgs = root
            .field("messages", "[]api.OpenAIMessage", "OpenAIChatRequest.messages")
            .opt_array()
            .unwrap()
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0]
                .field("role", "string", "OpenAIMessage.role")
                .as_string()
                .unwrap(),
            "user"
        );
        assert_eq!(
            root.field("temperature", "float64", "OpenAIChatRequest.temperature")
                .opt_f64()
                .unwrap(),
            None
        );
    }

    #[test]
    fn field_type_error_matches_go_text() {
        let (root, _) = GoValue::parse_checked(
            br#"{"model":"m","messages":"nope"}"#,
            "api.OpenAIChatRequest",
            "api.OpenAIChatRequest",
        )
        .unwrap();
        let err = root
            .field("messages", "[]api.OpenAIMessage", "OpenAIChatRequest.messages")
            .opt_array()
            .unwrap_err();
        assert_eq!(
            err,
            "json: cannot unmarshal string into Go struct field OpenAIChatRequest.messages of type []api.OpenAIMessage"
        );
    }

    #[test]
    fn int_field_errors_use_the_raw_literal() {
        // Go：`strconv.ParseInt(字面量, 10, 64)` 拒小数/指数形，错误文案里是**原字面量**
        for (body, lit) in [
            (&b"{\"max_tokens\":1.5}"[..], "1.5"),
            (b"{\"max_tokens\":1e2}", "1e2"),
            (b"{\"max_tokens\":1024.0}", "1024.0"),
        ] {
            let (root, pre) =
                GoValue::parse_checked(body, "api.OpenAIChatRequest", "api.OpenAIChatRequest")
                    .unwrap();
            let want = format!(
                "json: cannot unmarshal number {lit} into Go struct field \
                 OpenAIChatRequest.max_tokens of type int"
            );
            assert_eq!(pre.expect("预扫应报出 int 字段的类型错"), want);
            assert_eq!(
                root.field("max_tokens", "int", "OpenAIChatRequest.max_tokens")
                    .opt_int()
                    .unwrap_err(),
                want,
                "按需读取与预扫必须同源同文案"
            );
        }
        let (root, pre) = GoValue::parse_checked(
            b"{\"max_tokens\":1024}",
            "api.OpenAIChatRequest",
            "api.OpenAIChatRequest",
        )
        .unwrap();
        assert!(pre.is_none(), "合法整数字面量不应报错：{pre:?}");
        assert_eq!(
            root.field("max_tokens", "int", "OpenAIChatRequest.max_tokens")
                .opt_int()
                .unwrap(),
            Some(1024)
        );
    }

    #[test]
    fn field_type_errors_are_reported_in_declaration_order() {
        // 与 Go 同：`messages` 声明早于 `max_tokens`，所以先报 messages
        let (_, pre) = GoValue::parse_checked(
            b"{\"model\":\"m\",\"max_tokens\":\"x\",\"messages\":\"nope\"}",
            "api.OpenAIChatRequest",
            "api.OpenAIChatRequest",
        )
        .unwrap();
        assert_eq!(
            pre.expect("预扫应报错"),
            "json: cannot unmarshal string into Go struct field OpenAIChatRequest.messages of type []api.OpenAIMessage"
        );
    }

    #[test]
    fn top_level_kind_error_matches_go() {
        let e = GoValue::parse(b"\"scalar\"", "api.OpenAIChatRequest", "api.OpenAIChatRequest")
            .unwrap_err();
        assert_eq!(
            e,
            "json: cannot unmarshal string into Go value of type api.OpenAIChatRequest"
        );
    }

    #[test]
    fn null_is_zero_value_not_error() {
        let (root, pre) = GoValue::parse_checked(
            br#"{"messages":null,"max_tokens":null}"#,
            "api.OpenAIChatRequest",
            "api.OpenAIChatRequest",
        )
        .unwrap();
        assert!(pre.is_none());
        assert!(root
            .field("messages", "[]api.OpenAIMessage", "OpenAIChatRequest.messages")
            .opt_array()
            .unwrap()
            .is_none());
        assert!(root
            .field("max_tokens", "int", "OpenAIChatRequest.max_tokens")
            .opt_int()
            .unwrap()
            .is_none());
    }
}
