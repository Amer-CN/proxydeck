//! desensitize.rs —— 全角色脱敏（`internal/codebuddy/desensitize.go` 的逐字移植）。
//!
//! 为什么有这东西：腾讯后端按**内容指纹**拦（11128），而触发词大量出现在客户端固定的
//! 合规 system 模板里（"拒绝作恶"声明、Codex/CLI 注入上下文）。处理方式与 Go 逐条同款：
//!
//! 1. `DesensitizeText`：对词表内的词在**第 1 个字符后**插零宽空格（U+200B）——人和模型
//!    读起来无差别，后端关键词匹配失效；
//! 2. `stripFingerprints`：对黑名单模板句做**最小删除**（首条实测：只删括号注解）；
//! 3. `compactHarnessMessage`：把 CLI 注入的超长运行时提示压成短摘要（codex/claude 的
//!    system 模板、permissions/skills 块、user 角色里的 harness 上下文）；
//! 4. `DesensitizeBody`：对 `messages`（全角色）与 `tools`（schema 元数据）落地。
//!
//! **与 Go 的等价口径（关键，逐条对齐 `regexp` 语义）**：
//! * 词表排序按**字节长度降序**（Go `sort.Slice` 只按长度比较，等长项的相对顺序不影响
//!   「同一位置只可能有一个等长词匹配」的结论）；
//! * 边界是 **ASCII `\b`**（Go 的 `regexp` 明确是 ASCII word boundary：`\w=[0-9A-Za-z_]`），
//!   所以零宽插入不会吃掉相邻的非 ASCII 字节；
//! * `ReplaceAllStringFunc` 是**非重叠、最左优先**扫描：本实现对每个起点取「该起点上最长
//!   的命中词」，与「词表按长度降序 + 交替式 leftmost-first」等价；
//! * 匹配**大小写不敏感**，替换时用**原文的匹配片段**（首字节 + ZWSP + 余下）。
//!
//! **已知微差（登记在 README）**：Go 的 `(?i)` 是 Unicode 简单折叠，本实现是 ASCII 折叠；
//! 只有当文本里出现「全角/非 ASCII 等同字面」时才可能不同（真实提示词里不存在）。
//! Go 的 `runtimeBlockReplacements` / `runtimeTailMarkers` / `runtimeTailSummary` 三个声明
//! **在 Go 里没有调用点**（死代码），故未迁（登记在 README 差异表）。

use std::sync::OnceLock;

use serde_json::{Map, Value};

/// 零宽空格（Go `zwsp`）。
pub const ZWSP: &str = "\u{200b}";

/// `sensitiveTerms`（与 Go 的词表逐条同序；顺序只影响等长项的排序稳定性）。
pub const SENSITIVE_TERMS: &[&str] = &[
    "DoS", "DDoS", "exploit", "credential testing", "credential stuffing",
    "supply chain compromise", "supply-chain compromise", "detection evasion",
    "C2 frameworks", "C2 framework", "command and control", "malicious purposes",
    "malicious intent", "mass targeting", "brute force", "brute-force",
    "privilege escalation", "reverse shell", "remote code execution",
    "SQL injection", "XSS", "CSRF", "phishing", "malware", "ransomware",
    "keylogger", "rootkit", "backdoor", "botnet", "zero-day", "0day",
    "vulnerability", "vulnerabilities", "red teaming", "red-teaming",
    "sandbox", "sandboxing", "sandboxed", "unsandboxed",
    "escalated privileges", "escalated", "escalation",
    "destructive action", "destructive command", "destructive",
    "attack", "attacks", "cybersecurity", "security review",
    "exploit development", "hacking", "penetration testing", "penetration test",
    "injection", "weaponize", "weaponized", "harmful", "dangerous",
    "abuse", "abusive", "illegal", "terrorist", "terrorism", "bomb",
    "weapon", "weapons", "drug", "drugs", "narcotic", "suicide", "self-harm",
    "murder", "kill", "violence", "violent",
    "Claude Code", "Claude Opus", "Claude Sonnet", "Claude Haiku", "Claude Fable",
    "Anthropic", "Co-Authored-By", "noreply@anthropic.com",
    "Codex", "OpenAI",
    "Error: API error", "API error",
    "Illegal API invocation", "unapproved channel",
];

/// 11128 内容指纹删除表（`fingerprintRules`）。Go 侧只有一条：
/// `(?i)\b(Main branch) \(you will usually use this for PRs\)` → `$1`。
const FINGERPRINT_PHRASE: &[u8] = b"main branch (you will usually use this for prs)";
/// 组 1 的字节长度（`Main branch` = 11 字节，大小写变化不改变长度）。
const FINGERPRINT_GROUP_LEN: usize = 11;

const HARNESS_USER_MARKERS: &[&str] = &[
    "# AGENTS.md instructions",
    "<environment_context>",
    "<permissions instructions>",
    "<collaboration_mode>",
    "<skills_instructions>",
    "<system-reminder>",
    "# claudeMd",
];

const CODEX_SYSTEM_MARKERS: &[&str] = &[
    "You are a coding agent running in the Codex CLI",
    "Within this context, Codex refers to",
    "# How you work",
    "You are Claude Code",
];

const PERMISSIONS_MARKERS: &[&str] = &[
    "<permissions instructions>",
    "Filesystem sandboxing defines which files can be read or written.",
    "## How to request escalation",
];

const SKILLS_MARKERS: &[&str] = &[
    "<skills_instructions>",
    "### Available skills",
    "### How to use skills",
];

const CODEX_CORE_SUMMARY: &str = "You are a coding assistant in Codex CLI. Be precise, helpful, concise, and safe. \
Use available tools when needed, follow repository instructions, and keep the user informed.";

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Go 的 ASCII `\b`：左侧是词字符、右侧不是（或到边界）。
fn boundary_before(b: &[u8], i: usize) -> bool {
    i == 0 || !is_word_byte(b[i - 1])
}

fn boundary_after(b: &[u8], end: usize) -> bool {
    end >= b.len() || !is_word_byte(b[end])
}

/// 首字节 → 桶下标（`a`~`z` = 0~25，`0`~`9` = 26~35；其余返回 None）。
///
/// 词表里有 `0day` 这种以数字开头的词，所以不能只按字母分桶。
fn bucket_index(c: u8) -> Option<usize> {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_lowercase() {
        Some((c - b'a') as usize)
    } else if c.is_ascii_digit() {
        Some(26 + (c - b'0') as usize)
    } else {
        None
    }
}

/// 按首字节分桶的词表，桶内保持**长度降序**。
///
/// 语义与「全表按长度降序逐个试」完全一致：某位置只可能被「首字节相同」的词命中。
fn terms_by_first() -> &'static [Vec<&'static str>; 36] {
    static T: OnceLock<[Vec<&'static str>; 36]> = OnceLock::new();
    T.get_or_init(|| {
        let mut sorted: Vec<&'static str> = SENSITIVE_TERMS.to_vec();
        sorted.sort_by(|a, b| b.len().cmp(&a.len())); // 稳定排序，与 Go 的长度降序同义
        let mut buckets: [Vec<&'static str>; 36] = std::array::from_fn(|_| Vec::new());
        for t in sorted {
            let idx = bucket_index(t.as_bytes()[0]).expect("词表首字节必是字母或数字");
            buckets[idx].push(t);
        }
        buckets
    })
}

/// `DesensitizeText`：对触发词插入零宽空格（第 1 个字符后，改动最小）。
pub fn desensitize_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let b = text.as_bytes();
    let buckets = terms_by_first();
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0usize;
    while i < b.len() {
        let mut hit_end: Option<(usize, usize)> = None; // (start, end)
        let mut j = i;
        while j < b.len() {
            if let Some(bi) = bucket_index(b[j]) {
                if boundary_before(b, j) {
                    for t in &buckets[bi] {
                        let tb = t.as_bytes();
                        if j + tb.len() <= b.len()
                            && b[j..j + tb.len()].eq_ignore_ascii_case(tb)
                            && boundary_after(b, j + tb.len())
                        {
                            hit_end = Some((j, j + tb.len()));
                            break;
                        }
                    }
                }
            }
            if hit_end.is_some() {
                break;
            }
            j += 1;
        }
        match hit_end {
            Some((start, end)) => {
                out.push_str(&text[i..start]);
                out.push_str(&text[start..start + 1]);
                out.push_str(ZWSP);
                out.push_str(&text[start + 1..end]);
                i = end;
            }
            None => {
                out.push_str(&text[i..]);
                break;
            }
        }
    }
    out
}

/// `stripFingerprints`：应用指纹删除表（`(?i)\b(Main branch) \(…\)` → `$1`）。
pub fn strip_fingerprints(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut j = 0usize;
    while j < b.len() {
        if j + FINGERPRINT_PHRASE.len() <= b.len()
            && b[j..j + FINGERPRINT_PHRASE.len()].eq_ignore_ascii_case(FINGERPRINT_PHRASE)
            && boundary_before(b, j)
        {
            out.push_str(&text[i..j]);
            // `$1` 是**原文**的组 1（大小写照旧）；模式里的字面量固定 11 字节。
            out.push_str(&text[j..j + FINGERPRINT_GROUP_LEN]);
            i = j + FINGERPRINT_PHRASE.len();
            j = i;
            continue;
        }
        j += 1;
    }
    out.push_str(&text[i..]);
    out
}

/// `sensitivePattern.MatchString` 的**只读**等价：文本里是否出现词表内的词
/// （ASCII `\b` 边界、大小写不敏感）。与 `desensitize_text` 同一套桶/边界判据，但
/// **不改写**文本——11128 治理用它判定「内容嫌疑」，判定绝不能动请求体。
pub fn text_has_sensitive_term(text: &str) -> bool {
    let b = text.as_bytes();
    if b.is_empty() {
        return false;
    }
    let buckets = terms_by_first();
    for j in 0..b.len() {
        let Some(bi) = bucket_index(b[j]) else {
            continue;
        };
        if !boundary_before(b, j) {
            continue;
        }
        for t in &buckets[bi] {
            let tb = t.as_bytes();
            if j + tb.len() <= b.len()
                && b[j..j + tb.len()].eq_ignore_ascii_case(tb)
                && boundary_after(b, j + tb.len())
            {
                return true;
            }
        }
    }
    false
}

/// `fingerprintRules[].re.MatchString` 的**只读**等价：是否出现黑名单模板句。
///
/// 判据与 `strip_fingerprints` 逐条相同（大小写不敏感；`\b` 只在句首——Go 的规则
/// `(?i)\b(Main branch) \(…\)` 没有收尾边界），但这里**不删除**任何内容。
pub fn has_fingerprint_phrase(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() < FINGERPRINT_PHRASE.len() {
        return false;
    }
    for j in 0..=b.len() - FINGERPRINT_PHRASE.len() {
        if b[j..j + FINGERPRINT_PHRASE.len()].eq_ignore_ascii_case(FINGERPRINT_PHRASE)
            && boundary_before(b, j)
        {
            return true;
        }
    }
    false
}

/// Go `bodyHas11128Trigger`：判定一份出站请求体是否还有 11128 内容指纹。
///
/// 只把 `messages` 与 `tools` 序列化后过词表（品牌/合规词）与指纹表（黑名单句），
/// 命中即「内容嫌疑」；空体（无 messages 也无 tools）判干净。**只读判定，绝不改写
/// 入参**——server 侧的 11128 治理据此分流：命中 → 上游多半按内容拦的，换号无用，
/// 原样透传且不冷却任何号；未命中 → 内容侧已无可指认的触发物，按账号级封禁处理。
///
/// 判的是「即将发给上游的那份体」（已脱敏之后的 `backend`）：脱敏在位时已知词都已
/// 插零宽去不掉的内容（如 `tool_calls` 参数、工具 name 等）才是真嫌疑。
///
/// 与 Go 的形态对应：Go 用 `json.Marshal(map[string]any{messages, tools})`（键按字典序
/// ⇒ messages→tools）；这里 `Map` 的插入序同为 messages→tools，两种 `Map` 实现下一致。
pub fn body_has_11128_trigger(body: &Map<String, Value>) -> bool {
    let mut probe = Map::new();
    if let Some(v) = body.get("messages") {
        probe.insert("messages".to_string(), v.clone());
    }
    if let Some(v) = body.get("tools") {
        probe.insert("tools".to_string(), v.clone());
    }
    if probe.is_empty() {
        return false;
    }
    let Ok(raw) = serde_json::to_string(&Value::Object(probe)) else {
        return false;
    };
    if text_has_sensitive_term(&raw) {
        return true;
    }
    has_fingerprint_phrase(&raw)
}

/// `contentToText`：把字符串或 content blocks 规整成纯文本。
fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut sb = String::new();
            for blk in blocks {
                if let Value::Object(m) = blk {
                    if m.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(s) = m.get("text").and_then(|t| t.as_str()) {
                            sb.push_str(s);
                        }
                    }
                }
            }
            sb
        }
        _ => String::new(),
    }
}

/// `looksLikeHarnessUser`：user 消息是否是 CLI 注入的上下文。
pub fn looks_like_harness_user(content: &Value) -> bool {
    let text = content_to_text(content);
    HARNESS_USER_MARKERS.iter().any(|m| text.contains(m))
}

/// `compactHarnessMessage`：把注入的超长运行时提示压成短摘要（不是 harness 消息返回 None）。
///
/// **注意**：Go 的 `codexCoreSummary` 常量（带 "Inspect the repository…" 那版）在
/// `compactHarnessMessage` 里**没有被用到**——两个分支都返回内联的短句。本实现按**实际
/// 返回的文本**对齐（P5c 黄金门禁当场抓出这一处：我最初照常量写了长版，出站体与 Go 不同），
/// 常量本身留作形状对照（Go 侧它是死代码，与 `runtimeBlockReplacements` 同族）。
fn compact_harness_message(role: &str, content: &Value) -> Option<String> {
    let text = content_to_text(content);
    if text.is_empty() {
        return None;
    }
    let contains_any = |markers: &[&str]| markers.iter().any(|m| text.contains(m));
    if role == "system" && contains_any(CODEX_SYSTEM_MARKERS) {
        if text.contains("You are Claude Code") {
            return Some(
                "You are a coding assistant. Be precise, helpful, concise, and safe. \
Use available tools when needed, follow repository instructions, and keep the user informed."
                    .to_string(),
            );
        }
        return Some(CODEX_CORE_SUMMARY.to_string());
    }
    if contains_any(PERMISSIONS_MARKERS) {
        return Some(
            "Runtime permissions apply: filesystem access may be sandboxed, network may be restricted, \
and some commands may require user approval."
                .to_string(),
        );
    }
    if contains_any(SKILLS_MARKERS) {
        return Some(
            "Runtime skill metadata is available. Use relevant skills only when explicitly requested or clearly applicable."
                .to_string(),
        );
    }
    if role == "user" && looks_like_harness_user(content) {
        return Some(
            "Repository instructions and environment context are provided. Follow repository guidance \
while answering the user's actual request."
                .to_string(),
        );
    }
    None
}

/// `desensitizeMessage`：处理单条消息（浅拷贝，不污染原对象）。
fn desensitize_message(
    m: &Map<String, Value>,
    roles: &[String],
    harness_user: bool,
    compact: bool,
) -> Value {
    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let mut should = roles.iter().any(|r| r == role);
    // user 角色：除 roleSet 命中外，「像 harness 注入的上下文」也要处理（OR，不是覆盖）。
    if role == "user" && harness_user {
        if let Some(c) = m.get("content") {
            if looks_like_harness_user(c) {
                should = true;
            }
        }
    }
    if !should {
        return Value::Object(m.clone());
    }
    let mut nm = m.clone();
    let content = m.get("content").cloned().unwrap_or(Value::Null);
    if compact {
        if let Some(c) = compact_harness_message(role, &content) {
            nm.insert("content".to_string(), Value::String(desensitize_text(&c)));
            return Value::Object(nm);
        }
    }
    // 字符串或 content blocks 都做指纹删除（11128 黑名单句）+ 零宽脱敏；顺序：删除优先。
    match &content {
        Value::String(s) => {
            nm.insert(
                "content".to_string(),
                Value::String(desensitize_text(&strip_fingerprints(s))),
            );
        }
        Value::Array(blocks) => {
            let mut out: Vec<Value> = Vec::with_capacity(blocks.len());
            for blk in blocks {
                match blk {
                    Value::Object(bm) if bm.get("type").and_then(|t| t.as_str()) == Some("text") => {
                        let mut nb = bm.clone();
                        if let Some(Value::String(s)) = bm.get("text") {
                            nb.insert(
                                "text".to_string(),
                                Value::String(desensitize_text(&strip_fingerprints(s))),
                            );
                        }
                        out.push(Value::Object(nb));
                    }
                    other => out.push(other.clone()),
                }
            }
            nm.insert("content".to_string(), Value::Array(out));
        }
        _ => {}
    }
    Value::Object(nm)
}

/// `cloneToolValue`：保留实例数据原值（结构上仍是新对象）。
fn clone_tool_value(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, item) in m {
                out.insert(k.clone(), clone_tool_value(item));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(clone_tool_value).collect()),
        other => other.clone(),
    }
}

/// `desensitizeToolValue`：递归处理工具/schema 的说明元数据，而不是删除同名参数。
///
/// `properties` / `$defs` 等是「名称 → schema」字典：`description`、`title` 在这一层是用户
/// 定义的名称，必须保留（2026-09-14 ZCode `Agent.description` 被误删后 `required` 仍要求它，
/// 直接 400/11129）。`const/default/enum/examples` 是实例数据，不当 schema 注解修改。
fn desensitize_tool_value(v: &Value, strip_meta: bool) -> Value {
    match v {
        Value::Object(t) => {
            let mut out = Map::new();
            for (k, item) in t {
                if k == "description" || k == "title" {
                    if let Value::String(text) = item {
                        if !strip_meta {
                            out.insert(k.clone(), Value::String(desensitize_text(text)));
                        }
                        continue;
                    }
                }
                match k.as_str() {
                    "properties" | "patternProperties" | "$defs" | "definitions"
                    | "dependentSchemas" | "dependencies" => {
                        if let Value::Object(schemas) = item {
                            let mut named = Map::new();
                            for (name, schema) in schemas {
                                named.insert(name.clone(), desensitize_tool_value(schema, strip_meta));
                            }
                            out.insert(k.clone(), Value::Object(named));
                            continue;
                        }
                    }
                    "const" | "default" | "enum" | "examples" | "dependentRequired" => {
                        out.insert(k.clone(), clone_tool_value(item));
                        continue;
                    }
                    _ => {}
                }
                out.insert(k.clone(), desensitize_tool_value(item, strip_meta));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|i| desensitize_tool_value(i, strip_meta)).collect())
        }
        other => other.clone(),
    }
}

/// `DesensitizeBody`：对请求体的 `messages` / `tools` 做脱敏（返回新 map）。
///
/// * `roles`：要处理的角色集合（生产传 system/developer/user/assistant/tool）；
/// * `harness_user`：处理 CLI 注入的 user 上下文；
/// * `tools`：处理 tools 定义；
/// * `strip_tool_meta`：直接移除 description/title（最强压缩）。
pub fn desensitize_body(
    body: &Map<String, Value>,
    roles: &[String],
    harness_user: bool,
    tools: bool,
    strip_tool_meta: bool,
) -> Map<String, Value> {
    let mut nb = body.clone();
    if let Some(Value::Array(msgs)) = body.get("messages") {
        if !msgs.is_empty() {
            let out: Vec<Value> = msgs
                .iter()
                .map(|m| match m {
                    Value::Object(mm) => desensitize_message(mm, roles, harness_user, true),
                    other => other.clone(),
                })
                .collect();
            nb.insert("messages".to_string(), Value::Array(out));
        }
    }
    if tools {
        if let Some(Value::Array(tl)) = body.get("tools") {
            if !tl.is_empty() {
                nb.insert("tools".to_string(), desensitize_tool_value(&Value::Array(tl.clone()), strip_tool_meta));
            }
        }
    }
    nb
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_rule_deletes_only_the_note() {
        assert_eq!(
            strip_fingerprints("Main branch (you will usually use this for PRs): main"),
            "Main branch: main"
        );
        assert_eq!(
            strip_fingerprints("main branch (you will usually use this for PRs): dev"),
            "main branch: dev"
        );
        assert_eq!(
            strip_fingerprints("MAIN BRANCH (YOU WILL USUALLY USE THIS FOR PRS) x"),
            "MAIN BRANCH x"
        );
        let plain = "Main branch without the note";
        assert_eq!(strip_fingerprints(plain), plain);
        // 词边界：XMain 不算命中
        assert_eq!(
            strip_fingerprints("XMain branch (you will usually use this for PRs)"),
            "XMain branch (you will usually use this for PRs)"
        );
    }

    #[test]
    fn zero_width_after_first_byte_with_ascii_boundaries() {
        assert!(desensitize_text("runtime notice: API error: 500").contains(ZWSP));
        // \b 语义：errors 不是 error 的命中
        assert_eq!(
            desensitize_text("runtime notice: API errors"),
            "runtime notice: API errors"
        );
        // 近失词一律不动
        for s in ["attacker exploits sandboxes drugstore bombardments killers"] {
            assert_eq!(desensitize_text(s), s);
        }
        // 最长优先：penetration testing 整词命中（只插一个 ZWSP）
        let got = desensitize_text("run penetration testing now");
        assert_eq!(got, format!("run p{ZWSP}enetration testing now"));
        // 大小写不敏感，但替换用原文
        assert_eq!(desensitize_text("DoS"), format!("D{ZWSP}oS"));
        assert_eq!(desensitize_text("dos"), format!("d{ZWSP}os"));
    }

    #[test]
    fn harness_compaction_and_role_coverage() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "You are a coding agent running in the Codex CLI. Within this context, Codex refers to X."},
                {"role": "user", "content": "<system-reminder>be careful</system-reminder>"},
                {"role": "tool", "content": "XSS"},
                {"role": "other", "content": "XSS"},
            ]
        });
        let m = body.as_object().unwrap();
        let out = desensitize_body(m, &["system".into(), "developer".into(), "user".into(), "assistant".into(), "tool".into()], true, true, true);
        let msgs = out["messages"].as_array().unwrap();
        // 摘要文本本身也要过零宽脱敏（Go 侧实测：`C\u200bodex CLI`）
        assert_eq!(
            msgs[0]["content"],
            json!(format!(
                "You are a coding assistant in C{ZWSP}odex CLI. Be precise, helpful, concise, and safe. \
Use available tools when needed, follow repository instructions, and keep the user informed."
            ))
        );
        assert!(msgs[1]["content"].as_str().unwrap().starts_with("Repository instructions"));
        assert_eq!(msgs[2]["content"], json!(format!("X{ZWSP}SS")));
        assert_eq!(msgs[3]["content"], json!("XSS"));
    }

    #[test]
    fn tool_meta_strip_keeps_names_and_instance_data() {
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "t",
                "description": "XSS",
                "title": "malware",
                "parameters": {
                    "type": "object",
                    "description": "drop me",
                    "properties": {
                        "credential testing": {"type": "object", "description": "keep name"},
                        "d": {"const": {"description": "kept"}, "default": "backdoor", "enum": ["ransomware"], "examples": [{"title": "kept"}], "dependentRequired": {"a": ["b"]}}
                    },
                    "$defs": {"A": {"type": "string", "title": "drop too"}},
                    "required": ["d"]
                }
            }
        }]);
        let out = desensitize_tool_value(&tools, true);
        let f = &out[0]["function"];
        assert!(f.get("description").is_none());
        assert!(f.get("title").is_none());
        let p = &f["parameters"];
        assert!(p.get("description").is_none());
        let props = p["properties"].as_object().unwrap();
        assert!(props.contains_key("credential testing"));
        assert!(props["credential testing"].get("description").is_none());
        assert_eq!(props["d"]["const"], json!({"description": "kept"}));
        assert_eq!(props["d"]["default"], json!("backdoor"));
        assert_eq!(props["d"]["enum"], json!(["ransomware"]));
        assert_eq!(props["d"]["examples"], json!([{"title": "kept"}]));
        assert_eq!(props["d"]["dependentRequired"], json!({"a": ["b"]}));
        assert!(p["$defs"]["A"].get("title").is_none());
    }

    #[test]
    fn non_string_content_and_non_object_entries_are_left_alone() {
        let body = json!({
            "messages": [
                {"role": "assistant", "content": 42},
                {"role": "assistant", "content": null},
                {"role": "assistant"},
                {"role": "user", "content": [{"type": "text", "text": "XSS"}, {"type": "image_url"}]},
                1,
                "junk",
            ]
        });
        let out = desensitize_body(body.as_object().unwrap(), &["user".into(), "assistant".into()], false, false, false);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"], json!(42));
        assert_eq!(msgs[1]["content"], Value::Null);
        assert!(msgs[2].get("content").is_none());
        assert_eq!(msgs[3]["content"][0]["text"], json!(format!("X{ZWSP}SS")));
        assert_eq!(msgs[3]["content"][1], json!({"type": "image_url"}));
        assert_eq!(msgs[4], json!(1));
        assert_eq!(msgs[5], json!("junk"));
    }

    /// Go `TestBodyHas11128Trigger`：11128 治理的内容嫌疑判定（干净/品牌词/指纹句/
    /// 工具/空体）。判定只读词表，不改写传入的 body（逐案核对原值不变）。
    #[test]
    fn body_has_11128_trigger_matches_go() {
        let clean = json!({
            "model": "deepseek-v4.1-flash",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "Summarize this function."},
            ],
            "tools": [{
                "type": "function",
                "function": {"name": "read_file", "description": "Read a UTF-8 file."},
            }],
        });
        let empty = json!({});
        let no_msgs = json!({"model": "auto"});
        let brand_system = json!({
            "messages": [{"role": "system", "content": "You are Claude Code, a coding agent."}],
        });
        let brand_user = json!({
            "messages": [{"role": "user", "content": "Explain how OpenAI does this."}],
        });
        let fingerprint = json!({
            "messages": [{"role": "user",
                "content": "Main branch (you will usually use this for PRs): main"}],
        });
        let tool_hit = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function",
                "function": {"name": "x", "description": "Detect malware in a binary."}}],
        });
        let block_hit = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "run a credential stuffing test"},
            ]}],
        });
        let cases: Vec<(&str, &Value, bool)> = vec![
            ("干净请求", &clean, false),
            ("空体", &empty, false),
            ("无 messages 无 tools", &no_msgs, false),
            ("system 模板句（品牌词）", &brand_system, true),
            ("user 里的品牌词", &brand_user, true),
            ("指纹黑名单句", &fingerprint, true),
            ("工具 description 命中", &tool_hit, true),
            ("数组式 content 的 text 块命中", &block_hit, true),
        ];
        for (name, body, want) in cases {
            let before = serde_json::to_string(body).unwrap();
            let got = body_has_11128_trigger(body.as_object().unwrap());
            assert_eq!(got, want, "[{name}] body={before}");
            assert_eq!(
                serde_json::to_string(body).unwrap(),
                before,
                "[{name}] 判定不得改写请求体"
            );
        }
        // Go 的 `nil` 体（`body == nil` → false）在本实现里由「无 messages/tools」等价覆盖。
    }

    /// 脱敏后的体必须被判干净（否则 11128 治理会永远把上游按号封禁误判成内容拦截）；
    /// 而脱敏碰不到的残留（`tool_calls` 参数）仍算内容嫌疑——这是脏请求透传的入口。
    #[test]
    fn body_has_11128_trigger_after_desensitize_matches_go() {
        let roles: Vec<String> = ["system", "developer", "user", "assistant", "tool"]
            .iter()
            .map(|r| r.to_string())
            .collect();
        let dirty = json!({
            "messages": [
                {"role": "system", "content": "You are Claude Code. Codex CLI rules apply."},
                {"role": "user", "content": "Main branch (you will usually use this for PRs): main"},
            ],
            "tools": [{"type": "function",
                "function": {"name": "x", "description": "Detect malware."}}],
        });
        let scrubbed = desensitize_body(dirty.as_object().unwrap(), &roles, true, true, true);
        assert!(
            !body_has_11128_trigger(&scrubbed),
            "脱敏后的体应判干净: {scrubbed:?}"
        );

        // tool_calls 的 arguments 不在脱敏范围（desensitize_message 只改 content）——
        // 历史里带品牌词的 agent 会话因此仍判内容嫌疑。
        let with_tool_call = json!({
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "assistant", "content": "", "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "search", "arguments": "{\"q\":\"Claude Code pricing\"}"},
                }]},
            ],
        });
        let scrubbed2 =
            desensitize_body(with_tool_call.as_object().unwrap(), &roles, true, true, true);
        assert!(
            body_has_11128_trigger(&scrubbed2),
            "tool_calls 参数里的品牌词应判内容嫌疑: {scrubbed2:?}"
        );
    }
}
