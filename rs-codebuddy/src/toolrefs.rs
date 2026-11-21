//! toolrefs.rs —— Go `server.go` 的三个入站改写纯函数：
//! `normalizeToolChoice` / `sanitizeToolRefs`(+`inlineSchemaRefs`) / `normalizeRoles`(+`msgsHaveSystem`)。
//!
//! 逐字对 `internal/codebuddy/server.go` 的同名函数（行号见 crate README 对齐表），
//! 语义要点：
//! * `tool_choice` 对象式 → 字符串式（腾讯后端该字段是 Go string，收对象回 400）；
//!   `type` 为 auto/none/required 用该字符串，其余（含强制指定式）降级 `"required"`；
//!   请求体没有 `tools` 键（哪怕值为 null）→ 直接删 `tool_choice`；已是字符串/怪值原样。
//! * `$ref`/`$defs` 无损内联：只认 `#/$defs/` 前缀，兄弟键按 2020-12 合并（**同名兄弟键
//!   优先**），循环引用按解析栈截断成 `{"type":"object","description":"cyclic-ref-removed"}`，
//!   外部引用/缺失定义原样保留，无 `$ref` 的节点一律**复制后原样**（键值不变）；
//!   用过的顶层 `parameters.$defs` 丢弃。
//! * `developer` → `system`（腾讯按角色名判渠道，见 Go 注释里的实测）。

use serde_json::{Map, Value};

/// Go `normalizeToolChoice`。
pub fn normalize_tool_choice(body: &mut Map<String, Value>) {
    let Some(tc) = body.get("tool_choice") else {
        return;
    };
    if !body.contains_key("tools") {
        body.remove("tool_choice");
        return;
    }
    let Some(m) = tc.as_object() else {
        return; // 字符串式或其他怪值：原样透传
    };
    let t = m.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let out = match t {
        "auto" | "none" | "required" => t,
        _ => "required",
    };
    body.insert("tool_choice".to_string(), Value::String(out.to_string()));
}

/// Go `sanitizeToolRefs`。
pub fn sanitize_tool_refs(body: &mut Map<String, Value>) {
    let Some(Value::Array(tools)) = body.get_mut("tools") else {
        return;
    };
    for raw in tools.iter_mut() {
        let Some(tm) = raw.as_object_mut() else {
            continue;
        };
        let Some(fnv) = tm.get_mut("function").and_then(|v| v.as_object_mut()) else {
            continue;
        };
        let Some(params) = fnv.get_mut("parameters").and_then(|v| v.as_object()) else {
            continue;
        };
        let defs = params
            .get("$defs")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let inlined = inline_schema_refs(&Value::Object(params.clone()), &defs, &[]);
        fnv.insert("parameters".to_string(), inlined);
        if let Some(pm) = fnv.get_mut("parameters").and_then(|v| v.as_object_mut()) {
            pm.remove("$defs");
        }
    }
}

/// Go `inlineSchemaRefs`。
pub fn inline_schema_refs(node: &Value, defs: &Map<String, Value>, chain: &[String]) -> Value {
    let m = match node {
        Value::Object(m) => m,
        Value::Array(arr) => {
            return Value::Array(arr.iter().map(|v| inline_schema_refs(v, defs, chain)).collect())
        }
        other => return other.clone(),
    };
    if let Some(rv) = m.get("$ref").and_then(|v| v.as_str()) {
        if let Some(stripped) = rv.strip_prefix("#/$defs/") {
            // Go: `key := rv[strings.LastIndex(rv, "/")+1:]` —— 取末段（不是前缀之后的全串）
            let key = stripped.rsplit('/').next().unwrap_or(stripped).to_string();
            if chain.iter().any(|c| c == &key) {
                return crate::gjson::obj(vec![
                    ("type", Value::String("object".to_string())),
                    (
                        "description",
                        Value::String("cyclic-ref-removed".to_string()),
                    ),
                ]);
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

/// Go `normalizeRoles`：把 `developer` 角色降级为 `system`（就地改写）。
pub fn normalize_roles(messages: &mut [Value]) {
    for raw in messages.iter_mut() {
        if let Some(m) = raw.as_object_mut() {
            if m.get("role").and_then(|v| v.as_str()) == Some("developer") {
                m.insert("role".to_string(), Value::String("system".to_string()));
            }
        }
    }
}

/// Go `msgsHaveSystem`。
pub fn msgs_have_system(messages: &[Value]) -> bool {
    messages.iter().any(|m| {
        m.as_object().and_then(|o| o.get("role")).and_then(|v| v.as_str()) == Some("system")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gjson::to_go;

    fn parse(s: &str) -> Map<String, Value> {
        crate::godecode::unmarshal_map(s.as_bytes()).unwrap()
    }

    // 期望值由真 Go 采收（tests/golden/golden-codebuddy-go.json 的 pure.tool_choice 段）
    #[test]
    fn tool_choice_fixtures_match_go() {
        let cases: Vec<(&str, &str)> = vec![
            (
                r#"{"tools":[{"type":"function"}],"tool_choice":{"type":"function","function":{"name":"a"}}}"#,
                r#"{"tool_choice":"required","tools":[{"type":"function"}]}"#,
            ),
            (
                r#"{"tools":[{"type":"function"}],"tool_choice":{"type":"auto"}}"#,
                r#"{"tool_choice":"auto","tools":[{"type":"function"}]}"#,
            ),
            (
                r#"{"tools":[{"type":"function"}],"tool_choice":"auto"}"#,
                r#"{"tool_choice":"auto","tools":[{"type":"function"}]}"#,
            ),
            (
                r#"{"tools":[{"type":"function"}],"tool_choice":[{"type":"auto"}]}"#,
                r#"{"tool_choice":[{"type":"auto"}],"tools":[{"type":"function"}]}"#,
            ),
            (
                r#"{"tools":[{"type":"function"}],"tool_choice":null}"#,
                r#"{"tool_choice":null,"tools":[{"type":"function"}]}"#,
            ),
            (r#"{"tool_choice":{"type":"function","function":{"name":"a"}}}"#, r#"{}"#),
            (
                r#"{"tools":null,"tool_choice":{"type":"auto"}}"#,
                r#"{"tool_choice":"auto","tools":null}"#,
            ),
            (
                r#"{"tools":"nope","tool_choice":{"type":"auto"}}"#,
                r#"{"tool_choice":"auto","tools":"nope"}"#,
            ),
            (
                r#"{"tools":[{"type":"function"}]}"#,
                r#"{"tools":[{"type":"function"}]}"#,
            ),
        ];
        for (input, want) in cases {
            let mut body = parse(input);
            normalize_tool_choice(&mut body);
            assert_eq!(to_go(&Value::Object(body)), want, "input={input}");
        }
    }

    #[test]
    fn ref_inlining_matches_go_shape() {
        // 兄弟键合并（同名兄弟键优先）+ $defs 丢弃
        let mut body = parse(
            r##"{"tools":[{"type":"function","function":{"name":"t","parameters":{"$ref":"#/$defs/A","description":"override","$defs":{"A":{"type":"string","description":"orig"}}}}}]}"##,
        );
        sanitize_tool_refs(&mut body);
        assert_eq!(
            to_go(&Value::Object(body)),
            r#"{"tools":[{"function":{"name":"t","parameters":{"description":"override","type":"string"}},"type":"function"}]}"#
        );

        // 无 $ref 的工具一字不动（比较**写出后**的字节：Go 侧 map 是无序的，
        // 判断"没改"只能比 marshal 结果，不能比输入原文的键序）
        let plain = r#"{"tools":[{"type":"function","function":{"name":"t","parameters":{"type":"object","properties":{"a":{"type":"string"}},"required":["a"]}}}]}"#;
        let mut body = parse(plain);
        let before = to_go(&Value::Object(body.clone()));
        sanitize_tool_refs(&mut body);
        assert_eq!(to_go(&Value::Object(body)), before);

        // 循环截断 + 外部引用保留
        let mut body = parse(
            r##"{"tools":[{"type":"function","function":{"name":"t","parameters":{"type":"object","properties":{"a":{"$ref":"#/$defs/A"},"b":{"$ref":"https://x/y"}},"$defs":{"A":{"type":"object","properties":{"n":{"$ref":"#/$defs/A"}}}}}}}]}"##,
        );
        sanitize_tool_refs(&mut body);
        assert_eq!(
            to_go(&Value::Object(body)),
            r#"{"tools":[{"function":{"name":"t","parameters":{"properties":{"a":{"properties":{"n":{"description":"cyclic-ref-removed","type":"object"}},"type":"object"},"b":{"$ref":"https://x/y"}},"type":"object"}},"type":"function"}]}"#
        );
    }

    #[test]
    fn roles_are_normalized() {
        let mut msgs = vec![
            serde_json::json!({"role":"developer","content":"a"}),
            serde_json::json!({"role":"user","content":"b"}),
            Value::String("junk".into()),
        ];
        assert!(!msgs_have_system(&msgs));
        normalize_roles(&mut msgs);
        assert_eq!(msgs[0]["role"], Value::String("system".into()));
        assert_eq!(msgs[1]["role"], Value::String("user".into()));
        assert!(msgs_have_system(&msgs));
    }
}

/// 保证**首条**是 system（dsh-connect-workbuddy 记录的 11128 三道门之一：
/// 「首条是不是 system」）。
///
/// - 后面有 system 但不在首位 → 挪到最前（`Some("move")`），**永远做**：只动顺序、不加内容；
/// - 一条 system 都没有 → 仅在 `allow_prepend` 时补一条最小的（`Some("prepend")`）。
///
/// 为什么补 system 要开关：**CN 实测 user-first 也回 200**，说明这道门在 CN 现在并不强制；
/// 凭空插一条 system 会改写每个无 system 的请求（还有黄金基准锁着「不补」），
/// 所以 CN 默认只做"挪"不做"补"，INTL 仍按历史实测强制补
/// （`force_system` 或 `CODEBUDDY_FORCE_SYSTEM=1`）。
pub fn ensure_system_first(
    messages: &mut Vec<Value>,
    default_system: &str,
    allow_prepend: bool,
) -> Option<&'static str> {
    let is_system = |m: &Value| {
        m.as_object().and_then(|o| o.get("role")).and_then(Value::as_str) == Some("system")
    };
    if messages.first().map(is_system).unwrap_or(false) {
        return None;
    }
    if let Some(pos) = messages.iter().position(is_system) {
        let sys = messages.remove(pos);
        messages.insert(0, sys);
        return Some("move");
    }
    if !allow_prepend {
        return None;
    }
    messages.insert(
        0,
        serde_json::json!({ "role": "system", "content": default_system }),
    );
    Some("prepend")
}

#[cfg(test)]
mod system_first_tests {
    use super::ensure_system_first;
    #[test]
    fn system_first_moves_or_prepends() {
        use serde_json::json;
        let d = "You are a helpful assistant.";

        // 已经是首条 → 不动
        let mut m = vec![json!({"role":"system","content":"S"}), json!({"role":"user","content":"u"})];
        assert_eq!(ensure_system_first(&mut m, d, true), None);
        assert_eq!(m.len(), 2);

        // system 在后面 → 挪到最前，内容原样
        let mut m = vec![
            json!({"role":"user","content":"u"}),
            json!({"role":"assistant","content":"a"}),
            json!({"role":"system","content":"S"}),
        ];
        assert_eq!(ensure_system_first(&mut m, d, true), Some("move"));
        assert_eq!(m[0]["content"], "S", "挪动不改内容");
        assert_eq!(m.len(), 3);

        // 完全没有 → 补最小 system
        let mut m = vec![json!({"role":"user","content":"u"})];
        assert_eq!(ensure_system_first(&mut m, d, true), Some("prepend"));
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[0]["content"], serde_json::json!(d));

        // 空列表 → 也要补
        let mut m: Vec<serde_json::Value> = vec![];
        assert_eq!(ensure_system_first(&mut m, d, true), Some("prepend"));
        assert_eq!(m.len(), 1);

        // 不允许补时：无 system 就一个字都不动（CN 的默认行为）
        let mut m = vec![json!({"role":"user","content":"u"})];
        assert_eq!(ensure_system_first(&mut m, d, false), None);
        assert_eq!(m.len(), 1);
        // 但"挪"仍然做
        let mut m = vec![json!({"role":"user","content":"u"}), json!({"role":"system","content":"S"})];
        assert_eq!(ensure_system_first(&mut m, d, false), Some("move"));
        assert_eq!(m[0]["role"], "system");
    }
}


