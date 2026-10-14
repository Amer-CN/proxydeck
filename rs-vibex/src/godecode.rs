//! godecode.rs —— Go `encoding/json` 解码侧的语义复刻（vibex 只用到两处）。
//!
//! 1. `/accounts` 的 `json.NewDecoder(r.Body).Decode(&req)`：**只取第一个 JSON 值**，
//!    尾随数据忽略；`null` 落进 struct 是「无操作」不报错；已知字段类型不匹配才报错；
//!    字段名匹配**大小写不敏感**且重复键后者生效（`config::go_field`）。
//! 2. `/v1/chat/completions` 的 `json.Unmarshal(raw, &map[string]any)`：**整段**必须是
//!    一个 JSON 值（尾随数据算错）；`null` 落进 map 是「无操作」（body 仍是 nil → 调用方 400）；
//!    非对象（数组/标量）报错。
use serde_json::{Map, Value};

use crate::config::go_field;

/// 解析 `/accounts` 的请求体。
///
/// 返回 `Err(())` 等价 Go 的 `Decode` 报错（调用方回 400 `请求体解析失败`）。
pub fn decode_accounts_body(raw: &[u8]) -> Result<(String, String), ()> {
    let mut stream = serde_json::Deserializer::from_slice(raw).into_iter::<Value>();
    let first = match stream.next() {
        Some(Ok(v)) => v,
        // 空体 / 非法 JSON → Go 的 Decode 同样报错
        _ => return Err(()),
    };
    let obj: &Map<String, Value> = match &first {
        // `null` 落进 struct 是无操作：字段保持零值
        Value::Null => return Ok((String::new(), String::new())),
        Value::Object(o) => o,
        // 数组 / 标量落进 struct → UnmarshalTypeError
        _ => return Err(()),
    };
    let mut action = String::new();
    let mut token = String::new();
    for (tag, slot) in [("action", &mut action), ("token", &mut token)] {
        match go_field(obj, tag) {
            None | Some(Value::Null) => {} // 缺键与 null 都是「保持零值」
            Some(Value::String(s)) => *slot = s.clone(),
            Some(_) => return Err(()), // 类型不匹配
        }
    }
    Ok((action, token))
}

/// 解析 `/v1/chat/completions` 的请求体（`json.Unmarshal` 语义）。
///
/// 返回 `Ok(Value::Null)` 表示「解析成功但 body 是 nil」（调用方回 400 对象校验）。
pub fn decode_chat_body(raw: &[u8]) -> Result<Value, ()> {
    let v: Value = serde_json::from_slice(raw).map_err(|_| ())?;
    match v {
        Value::Null => Ok(Value::Null),
        Value::Object(_) => Ok(v),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accounts_body_like_go() {
        assert_eq!(
            decode_accounts_body(br#"{"action":"add","token":"t"}"#).unwrap(),
            ("add".into(), "t".into())
        );
        assert_eq!(
            decode_accounts_body(br#"{"action":"remove"}trailing"#).unwrap(),
            ("remove".into(), String::new()),
            "Go Decoder 忽略尾随数据"
        );
        assert_eq!(
            decode_accounts_body(br#"{"ACTION":"add","Token":"t2"}"#).unwrap(),
            ("add".into(), "t2".into()),
            "字段名大小写不敏感"
        );
        assert_eq!(
            decode_accounts_body(br#"{"action":null,"token":null}"#).unwrap(),
            (String::new(), String::new()),
            "null 落进 string 是无操作"
        );
        assert!(decode_accounts_body(br#"{"action":123}"#).is_err(), "类型不匹配报错");
        assert!(decode_accounts_body(br#"nope"#).is_err());
        assert!(decode_accounts_body(br#""#).is_err(), "空体报错");
        assert!(decode_accounts_body(br#"[1,2]"#).is_err());
        assert!(decode_accounts_body(br#"null"#).is_ok(), "null 进 struct 不报错");
    }

    #[test]
    fn chat_body_like_go() {
        assert!(decode_chat_body(br#"{"messages":[]}"#).is_ok());
        assert_eq!(decode_chat_body(br#"null"#).unwrap(), json!(null));
        assert!(decode_chat_body(br#"{"a":1}x"#).is_err(), "尾随数据报错");
        assert!(decode_chat_body(br#"[1]"#).is_err());
        assert!(decode_chat_body(br#"12"#).is_err());
        assert!(decode_chat_body(br#"{"a":1} "#).is_ok(), "尾随空白可以");
    }
}
