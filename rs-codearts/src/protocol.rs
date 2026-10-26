//! 请求体规整：只做上游明确要求的字段修正，不改变对话语义。
//!
//! 上游 `InferHub.001001044.400` 报 `Message role cannot empty`：
//! 客户端（例如 DSH）会在消息列表里塞入 `role` 为空/缺失的内部条目。
//! 参考实现 HITZY2002 `internal/upstream/client.go::chatMessagesToOpenAI`
//! 的处理是：**空 role 一律降级为 user**（保留 content）。这里对齐，并补两条：
//!
//! * `role` 去空白、转小写（上游按小写 role 路由）；
//! * `developer` → `system`（OpenAI 新角色，上游只认 system）；
//! * 空 role 但有 `tool_calls` → `assistant`；有 `tool_call_id` → `tool`；
//!   都没有 → `user`；
//! * 缺 `content` 且没有 `tool_calls` 的，补空字符串（避免下游按 null 处理）。

use serde_json::{json, Value};

/// 就地规整 `messages`；返回可读错误（调用方应回 400）。
pub fn normalize_messages(req: &mut Value) -> Result<(), String> {
    let Some(messages) = req.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return Err("缺少 messages 数组".to_string());
    };
    if messages.is_empty() {
        return Err("messages 为空".to_string());
    }
    for (i, m) in messages.iter_mut().enumerate() {
        let Some(obj) = m.as_object_mut() else {
            return Err(format!("messages[{i}] 不是对象"));
        };
        let raw_role = obj
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let role = if raw_role.is_empty() {
            if obj.contains_key("tool_calls") {
                "assistant"
            } else if obj.contains_key("tool_call_id") {
                "tool"
            } else {
                // 参考实现同款：空 role → user，保留 content
                "user"
            }
        } else {
            match raw_role.as_str() {
                "developer" => "system",
                other => other,
            }
        };
        if !obj.contains_key("content") && !obj.contains_key("tool_calls") {
            obj.insert("content".to_string(), json!(""));
        }
        obj.insert("role".to_string(), json!(role));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(msgs: Value) -> Value {
        let mut req = json!({ "messages": msgs });
        normalize_messages(&mut req).expect("应通过");
        req
    }

    #[test]
    fn empty_role_becomes_user_and_keeps_content() {
        let out = run(json!([{ "role": "", "content": "reminder" }]));
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "reminder");
    }

    #[test]
    fn missing_role_with_tool_markers_is_classified() {
        let out = run(json!([
            { "content": "x", "tool_call_id": "call_1" },
            { "content": "y", "tool_calls": [] }
        ]));
        assert_eq!(out["messages"][0]["role"], "tool");
        assert_eq!(out["messages"][1]["role"], "assistant");
    }

    #[test]
    fn developer_maps_to_system_and_roles_are_trimmed() {
        let out = run(json!([
            { "role": " Developer ", "content": "sys" },
            { "role": "USER", "content": "hi" }
        ]));
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["role"], "user");
    }

    #[test]
    fn missing_content_is_filled() {
        let out = run(json!([{ "role": "assistant" }]));
        assert_eq!(out["messages"][0]["content"], "");
    }
}
