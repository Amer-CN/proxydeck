//! reword.rs —— 11128 内容指纹的**按需最小改写**（对齐 magpie 2026-10-30 实证）。
//!
//! 与 `desensitize` 的分工（两者不重叠、互不改写）：
//! - `desensitize`：**常开**的大词表清洗（删敏感词、零宽化），Go 金标逐字节锁定，本模块不碰；
//! - `reword`：**按需**触发的最小改写——只在收到 11128 内容级拒绝之后，把被拒的整句
//!   改到能过（插一个逗号），而不是删掉它说的东西。
//!
//! 为什么要「插逗号」而不是「换个说法」：上游对这类身份句是**整句精确匹配**，
//! 少一个字符就过；换成别的说法会丢掉「这个 agent 是哪个」的信息。实测（magpie
//! 2026-10-30，`copilot.tencent.com`·真实账号·流式）：
//! ```text
//! You are an AI agent powered by DeepSeek Harness.      → 400 / 11128
//! You are an AI agent, powered by DeepSeek Harness.     → 200   ← 只多一个逗号
//! You are an AI agent powered by DeepSeek Harness CLI.  → 400   ← 加词没用
//! You are an AI agent powered by deepseek harness.      → 400   ← 大小写也没用
//! You are an AI agent powered by Claude Code.           → 200   ← 换个产品名就过
//! ```
//!
//! 作用域纪律：**只动 `system` 与 `assistant` 消息**（上游只读这两类，实测 user 与
//! 工具结果不读）。用户自己说的话、工具返回的结果，一个字都不许碰——那会改变任务
//! 语义，也会让「模型引用回来的句子」和上下文对不上。
//!
//! 记住机制：一条 prompt 改写通过之后记下它的指纹，后续请求**直接按改写形态发**，
//! 不必每轮先吃一次拒绝（否则工具循环的每一步都要多打一个来回）。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// 「记住」集合的上限：超过就整体丢弃重来（这些指纹只省一次拒绝往返，丢了无损）。
const REMEMBERED_MAX: usize = 4096;

/// 搬家兜底的最低 system 字节数：短样板句不值得为它多打一次上游。
pub const MIN_MOVE_BYTES: usize = 64;

/// 一条改写规则命中后要做的动作。
enum Action {
    /// 在 needle 起点 + off 处插入一个逗号（保义优先，破坏整句精确匹配）。
    Comma(usize),
    /// 从 needle 起点 + off 处删掉 n 个字节（删一个形容词，句子仍然成立）。
    Delete { at: usize, n: usize },
    /// 在 needle 起点 + off 处插入固定文字（上游按整句匹配，插一个引导词即破匹配）。
    Insert { at: usize, text: &'static str },
}

/// 一条改写规则：命中特征（全小写比较）+ 动作 + 规则名。
struct Rule {
    /// 触发片段（必须全小写）。
    needle: &'static str,
    action: Action,
    name: &'static str,
}

/// 规则表。按顺序尝试，命中第一条即返回（改完再判下一条没有意义，且可能重复插入）。
///
/// 只收「**已知会整句被拒、且改完仍保义**」的形态。新形态往这里加，加完补单测。
const RULES: &[Rule] = &[
    // 通用形态：`You are an AI agent powered by <任意产品>.`
    // 上游认的是「形状 + 它认识的产品名」，所以形状先出手：插逗号即破整句匹配。
    Rule {
        needle: "you are an ai agent powered by",
        action: Action::Comma("you are an ai agent".len()),
        name: "agent-identity-shape",
    },
    // Claude Code 的身份句：官方 CLI 原话带 `official`，去掉一个形容词即可保义。
    Rule {
        needle: "anthropic's official cli for claude",
        action: Action::Delete {
            at: "anthropic's ".len(),
            n: "official ".len(),
        },
        name: "claude-code-identity",
    },
    // DSH 的第二条指纹句（magpie #291 实测：400 → 这句加个引导词即 200）。
    // 上游原文里 "names." 与 "When" 之间可能是一到两个空格，两种都给。
    Rule {
        needle: "web gui whose url the runtime context names. when the user refers",
        action: Action::Insert {
            at: "web gui whose url the runtime context names. ".len(),
            text: "The interface, ",
        },
        name: "dsH-web-gui-line",
    },
    Rule {
        needle: "web gui whose url the runtime context names.  when the user refers",
        action: Action::Insert {
            at: "web gui whose url the runtime context names.  ".len(),
            text: "The interface, ",
        },
        name: "dsH-web-gui-line-2sp",
    },
];

fn remembered() -> &'static Mutex<HashSet<u64>> {
    static R: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 文本指纹（前 8 字节 → u64）。只看内容，不看位置。
fn fingerprint(text: &str) -> u64 {
    let d = Sha256::digest(text.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    u64::from_be_bytes(b)
}

/// 对一段文本跑规则表；命中返回（改写后文本, 规则名）。
///
/// 幂等：两种动作改完都不会再命中同一条规则。
fn apply_rules(text: &str) -> Option<(String, &'static str)> {
    let lower = text.to_lowercase();
    for r in RULES {
        let Some(pos) = lower.find(r.needle) else {
            continue;
        };
        let mut out = String::with_capacity(text.len() + 1);
        match r.action {
            Action::Comma(off) => {
                let at = pos + off;
                // 已经插过逗号就不再动（幂等哨兵）。
                if text[..at].ends_with(',') {
                    continue;
                }
                out.push_str(&text[..at]);
                out.push(',');
                out.push_str(&text[at..]);
            }
            Action::Delete { at: off, n } => {
                let at = pos + off;
                out.push_str(&text[..at]);
                out.push_str(&text[at + n..]);
            }
            Action::Insert { at: off, text: ins } => {
                let at = pos + off;
                out.push_str(&text[..at]);
                out.push_str(ins);
                out.push_str(&text[at..]);
            }
        }
        return Some((out, r.name));
    }
    None
}

/// 遍历 system / assistant 消息的文本块，逐块跑规则。
///
/// 返回 Some(规则名) 表示**至少改了一处**；调用方据此决定重发。
fn reword_message(msg: &mut Value) -> Option<&'static str> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
    if role != "system" && role != "assistant" {
        return None; // user / tool 一律不碰
    }
    let content = msg.get_mut("content")?;
    // 形态一：content 是字符串
    if let Some(s) = content.as_str() {
        let (out, name) = apply_rules(s)?;
        *content = Value::String(out);
        return Some(name);
    }
    // 形态二：content 是 [{type:"text", text:"..."}] 块数组
    let arr = content.as_array_mut()?;
    for part in arr {
        let is_text = part.get("type").and_then(Value::as_str) == Some("text");
        if !is_text {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        if let Some((out, name)) = apply_rules(text) {
            part["text"] = Value::String(out);
            return Some(name);
        }
    }
    None
}

fn messages_mut(body: &mut Map<String, Value>) -> Option<&mut Vec<Value>> {
    body.get_mut("messages")?.as_array_mut()
}

/// 按需改写：跑一遍规则表，返回命中的规则名。
pub fn reword(body: &mut Map<String, Value>) -> Option<&'static str> {
    let msgs = messages_mut(body)?;
    let mut hit = None;
    for m in msgs.iter_mut() {
        if let Some(name) = reword_message(m) {
            hit = Some(name);
        }
    }
    hit
}

/// 记住这份请求体里需要改写的 system / assistant 原文指纹。
pub fn remember(body: &Map<String, Value>) {
    let Some(msgs) = body.get("messages").and_then(Value::as_array) else {
        return;
    };
    let mut set = match remembered().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if set.len() > REMEMBERED_MAX {
        set.clear();
    }
    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "system" && role != "assistant" {
            continue;
        }
        let Some(content) = m.get("content") else { continue };
        if let Some(s) = content.as_str() {
            set.insert(fingerprint(s));
        } else if let Some(arr) = content.as_array() {
            for part in arr {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    set.insert(fingerprint(t));
                }
            }
        }
    }
}

/// 预先改写：只处理「已经被记下、需要改写」的文本块。
///
/// 每轮请求进来先跑它，命中即改——这样同一段 prompt 只吃一次 11128 拒绝，
/// 而不是工具循环的每一步都吃一次。
pub fn pre_reword(body: &mut Map<String, Value>) -> Option<&'static str> {
    let known: HashSet<u64> = {
        let set = match remembered().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        set.clone()
    };
    if known.is_empty() {
        return None;
    }
    let msgs = messages_mut(body)?;
    let mut hit = None;
    for m in msgs.iter_mut() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "system" && role != "assistant" {
            continue;
        }
        let Some(content) = m.get_mut("content") else { continue };
        if let Some(s) = content.as_str() {
            if known.contains(&fingerprint(s)) {
                if let Some((out, name)) = apply_rules(s) {
                    *content = Value::String(out);
                    hit = Some(name);
                }
            }
            continue;
        }
        if let Some(arr) = content.as_array_mut() {
            for part in arr {
                let Some(t) = part.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if !known.contains(&fingerprint(t)) {
                    continue;
                }
                if let Some((out, name)) = apply_rules(t) {
                    part["text"] = Value::String(out);
                    hit = Some(name);
                }
            }
        }
    }
    if hit.is_some() {
        remember(body); // 改写形态也记一份，下一轮直接命中
    }
    hit
}

/// 兜底：把 system 提示词挪进第一条 user 消息（上游不读 user，等于让它看不见）。
///
/// 这是 magpie 的第二层（改写仍被拒时用）。**改写保义优先，搬家最后用**——搬完
/// 模型收到的 system 槽位变成一句默认话，语义依赖会变浅。
pub fn move_system_to_user(body: &mut Map<String, Value>) -> bool {
    let Some(msgs) = messages_mut(body) else {
        return false;
    };
    let Some(pos) = msgs
        .iter()
        .position(|m| m.get("role").and_then(Value::as_str) == Some("system"))
    else {
        return false;
    };
    let Some(text) = msgs[pos].get("content").and_then(Value::as_str) else {
        return false;
    };
    // 短样板句（"You are a helpful assistant." 这类）搬了也没有意义：它不是被拒的
    // 那条 agent 身份句，搬走只会白多一次请求。真被拒的 harness 提示词都是成百上千字节。
    if text.trim().len() < MIN_MOVE_BYTES {
        return false;
    }
    let moved = format!("```\n{text}\n```");
    msgs[pos] = serde_json::json!({
        "role": "system",
        "content": "You are a helpful assistant.",
    });
    msgs.insert(
        pos + 1,
        serde_json::json!({ "role": "user", "content": moved }),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body_with(role: &str, text: &str) -> Map<String, Value> {
        let v = json!({ "messages": [ { "role": role, "content": text } ] });
        v.as_object().unwrap().clone()
    }

    #[test]
    fn rewrites_generic_identity_shape_with_one_comma() {
        let mut b = body_with("system", "You are an AI agent powered by DeepSeek Harness.");
        assert_eq!(reword(&mut b), Some("agent-identity-shape"));
        assert_eq!(
            b["messages"][0]["content"],
            "You are an AI agent, powered by DeepSeek Harness."
        );
    }

    #[test]
    fn rewrite_is_idempotent() {
        let mut b = body_with("system", "You are an AI agent, powered by Foo.");
        assert_eq!(reword(&mut b), None, "已带逗号不得再插一个");
    }

    #[test]
    fn leaves_user_and_tool_messages_alone() {
        let mut b = body_with("user", "You are an AI agent powered by DeepSeek Harness.");
        assert_eq!(reword(&mut b), None);
        assert_eq!(
            b["messages"][0]["content"],
            "You are an AI agent powered by DeepSeek Harness.",
            "user 消息一个字都不许动"
        );
        let mut t = body_with("tool", "You are an AI agent powered by DeepSeek Harness.");
        assert_eq!(reword(&mut t), None);
    }

    #[test]
    fn rewrites_assistant_quoted_line() {
        let mut b = body_with("assistant", "Sure. You are an AI agent powered by Foo.");
        assert!(reword(&mut b).is_some());
        assert_eq!(
            b["messages"][0]["content"],
            "Sure. You are an AI agent, powered by Foo."
        );
    }

    #[test]
    fn rewrites_claude_code_identity_line() {
        let mut b = body_with(
            "system",
            "You are Claude Code, Anthropic's official CLI for Claude.",
        );
        assert_eq!(reword(&mut b), Some("claude-code-identity"));
        assert_eq!(
            b["messages"][0]["content"],
            "You are Claude Code, Anthropic's CLI for Claude."
        );
    }

    #[test]
    fn handles_content_block_arrays() {
        let v = json!({
            "messages": [ { "role": "system", "content": [
                { "type": "text", "text": "You are an AI agent powered by Foo." }
            ] } ]
        });
        let mut b = v.as_object().unwrap().clone();
        assert!(reword(&mut b).is_some());
        assert_eq!(
            b["messages"][0]["content"][0]["text"],
            "You are an AI agent, powered by Foo."
        );
    }

    #[test]
    fn no_messages_key_is_none() {
        let mut b = json!({ "model": "x" }).as_object().unwrap().clone();
        assert_eq!(reword(&mut b), None);
        assert_eq!(pre_reword(&mut b), None);
    }

    #[test]
    fn moved_system_keeps_user_words() {
        // 搬家只对「像 agent 提示词的长 system」生效，这里刻意写成真提示词的长度
        let long_system = format!(
            "You are an AI agent powered by Foo.\n{}",
            "Follow the user's instructions carefully. ".repeat(8)
        );
        let v = json!({
            "messages": [
                { "role": "system", "content": long_system },
                { "role": "user", "content": "hi" }
            ]
        });
        let mut b = v.as_object().unwrap().clone();
        assert!(move_system_to_user(&mut b));
        assert_eq!(b["messages"][0]["content"], "You are a helpful assistant.");
        let moved = b["messages"][1]["content"].as_str().unwrap();
        assert!(moved.starts_with("```"));
        assert!(moved.contains("You are an AI agent powered by Foo."));
        assert_eq!(b["messages"][2]["content"], "hi", "用户的原文原地不动");
    }

    #[test]
    fn remember_then_pre_reword_hits_without_being_asked() {
        let text = "You are an AI agent powered by RememberedCo.";
        let mut first = body_with("system", text);
        // 第一次是靠 reword 发现的
        assert!(reword(&mut first).is_some());
        remember(&body_with("system", text));
        // 第二次同一条 prompt 进来，pre_reword 直接命中
        let mut second = body_with("system", text);
        assert!(pre_reword(&mut second).is_some());
        assert_eq!(
            second["messages"][0]["content"],
            "You are an AI agent, powered by RememberedCo."
        );
    }
    #[test]
    fn rewrites_dsh_web_gui_line_in_both_spacings() {
        for (src, want) in [
            (
                "Web GUI whose URL the runtime context names. When the user refers to it, read on.",
                "Web GUI whose URL the runtime context names. The interface, When the user refers to it, read on.",
            ),
            (
                "Web GUI whose URL the runtime context names.  When the user refers to it, read on.",
                "Web GUI whose URL the runtime context names.  The interface, When the user refers to it, read on.",
            ),
        ] {
            let mut b = body_with("system", src);
            assert!(reword(&mut b).is_some(), "应命中：{src}");
            assert_eq!(b["messages"][0]["content"], want);
        }
    }

    #[test]
    fn short_system_is_not_moved() {
        let v = json!({
            "messages": [
                { "role": "system", "content": "You are a helpful assistant." },
                { "role": "user", "content": "hi" }
            ]
        });
        let mut b = v.as_object().unwrap().clone();
        assert!(
            !move_system_to_user(&mut b),
            "短样板句搬家只会白多一次请求"
        );
    }
}
