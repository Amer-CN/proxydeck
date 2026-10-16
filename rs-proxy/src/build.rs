//! `internal/proxy/proxy.go` 的请求构造与上游调用部分。
//!
//! 逐字节对齐的要点：
//! * 出站体是 `api.CCRequestBody` 的**声明序** JSON（[`crate::api::CCRequestBody::to_go`]）；
//! * `max_tokens` 的钳制与上下文压缩顺序（先钳 200000，再按剩余上下文压）；
//! * 上游头集合固定五项（`Content-Type` / `Authorization` / `x-command-code-version` /
//!   `x-cli-environment` / `Accept`）；其余头由各自的 HTTP 客户端生成（见 README 差异）。

use std::time::Duration;

use crate::api::{
    CCChatParams, CCConfig, CCRequestBody, OpenAIChatRequest,
};
use crate::convert::{convert_messages, convert_tools, est_tokens, extract_system};
use crate::gjson::to_go_bytes;
use crate::{model, util};

/// CommandCode 上游默认地址。
pub const DEFAULT_BASE_URL: &str = "https://api.commandcode.ai";

/// Go `defaultTimeout`（300s）：**含响应体的总超时**。
///
/// ⚠ 这是主代理与 bai/vibex 那几处不同的口径——Go 这里确实设了 `http.Client{Timeout: 300s}`，
/// 与 `rs-bai` P2 里「Go 对上游无总超时」的结论**不是同一处**。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// `ccContextLimit`：上游模型的上下文上限（token）。
pub const CC_CONTEXT_LIMIT: i64 = 1048576;

/// `BuildRequest`：OpenAI chat 请求 → CommandCode 请求体。
pub fn build_request(req: &OpenAIChatRequest) -> CCRequestBody {
    let model = model::map_model(&req.model);
    let (system, msgs) = extract_system(&req.messages);
    let cc_messages = convert_messages(&msgs);

    let mut temperature = 0.3f64;
    let mut max_tokens: i64 = 64000;
    if let Some(t) = req.temperature {
        temperature = t;
    }
    if let Some(mt) = req.max_tokens {
        max_tokens = mt;
    }
    if let Some(mct) = req.max_completion_tokens {
        max_tokens = mct;
    }
    // CommandCode 拒绝超过 200000 的 max_tokens：钳到接口上限
    if max_tokens > 200000 {
        max_tokens = 200000;
    }
    // 上下文感知：messages 已占用的 token + 输出预算必须 ≤ 模型 1M 上下文上限。
    // estTokens 估算偏保守（字节/2），不会低估。
    let room = CC_CONTEXT_LIMIT - est_tokens(&msgs) - system.len() as i64 / 2;
    if room < max_tokens {
        max_tokens = room;
        if max_tokens < 1024 {
            max_tokens = 1024;
        }
    }

    let tools = convert_tools(&req.tools);

    CCRequestBody {
        config: CCConfig {
            working_dir: ".".to_string(),
            date: util::local_day(util::unix_now()),
            environment: "cli".to_string(),
            structure: Vec::new(),
            is_git_repo: false,
            current_branch: String::new(),
            main_branch: "main".to_string(),
            git_status: String::new(),
            recent_commits: Vec::new(),
        },
        memory: String::new(),
        taste: String::new(),
        skills: String::new(),
        params: CCChatParams {
            model,
            // `ConvertMessages` 全程 append：一条都没产出时 Go 的切片是 **nil**（编成 `null`），
            // 不是空数组。见 `CCChatParams::messages` 的注释与黄金 `responses-convert #6`。
            messages: if cc_messages.is_empty() { None } else { Some(cc_messages) },
            tools,
            system,
            max_tokens,
            temperature,
            stream: true,
        },
        thread_id: util::uuid_v4(),
    }
}

/// `CreateUpstreamRequest` 的等价物：出站体的字节（供 reqwest 构造请求）。
pub fn cc_body_bytes(body: &CCRequestBody) -> Vec<u8> {
    to_go_bytes(&crate::gjson::s(body.to_go()))
        .into_iter()
        .collect::<Vec<u8>>()
        .clone()
}

/// 出站请求的完整形态（方法 + URL + 头 + 体），供 reqwest 与门禁直接用。
pub struct UpstreamRequestSpec {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

/// `CreateUpstreamRequest`：构造出站请求（含固定头集合）。
pub fn create_upstream_request(
    base_url: &str,
    body: &CCRequestBody,
    api_key: &str,
    version: &str,
) -> UpstreamRequestSpec {
    UpstreamRequestSpec {
        method: "POST",
        url: format!("{base_url}/alpha/generate"),
        headers: vec![
            ("Content-Type", "application/json".to_string()),
            ("Authorization", format!("Bearer {api_key}")),
            ("x-command-code-version", version.to_string()),
            ("x-cli-environment", "production".to_string()),
            ("Accept", "text/event-stream".to_string()),
        ],
        body: body.to_go().into_bytes(),
    }
}

/// `contextOverflowRe`：从上游报错里抠 messages 已用的 token 数。
///
/// Go 的正则：`requested \d+ tokens \((\d+) in the messages`。
pub fn parse_context_overflow(err_msg: &str) -> i64 {
    const MARK: &str = " tokens (";
    let mut rest = err_msg;
    while let Some(pos) = rest.find(MARK) {
        // 要求 MARK 之前是 "requested <digits>"
        let head = &rest[..pos];
        let ok = head
            .rsplit_once("requested ")
            .map(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false);
        if ok {
            let tail = &rest[pos + MARK.len()..];
            let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            let after = &tail[digits.len()..];
            if !digits.is_empty() && after.starts_with(" in the messages") {
                if let Ok(n) = digits.parse::<i64>() {
                    if n > 0 {
                        return n;
                    }
                }
            }
        }
        rest = &rest[pos + MARK.len()..];
    }
    0
}

/// 便捷：出站体的 JSON 文本（门禁对照用）。
pub fn cc_body_json(body: &CCRequestBody) -> String {
    body.to_go()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::OpenAIMessage;
    use serde_json::{json, Value};

    fn req_with(model: &str, content: Value) -> OpenAIChatRequest {
        OpenAIChatRequest {
            model: model.to_string(),
            messages: vec![OpenAIMessage {
                role: "user".to_string(),
                content: Some(content),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn defaults_temperature_and_max_tokens() {
        let body = build_request(&req_with("m", json!("x")));
        assert_eq!(body.params.temperature, 0.3);
        assert_eq!(body.params.max_tokens, 64000);
        assert!(body.params.stream, "出站恒 stream:true");
        assert_eq!(body.config.working_dir, ".");
        assert_eq!(body.config.environment, "cli");
        assert_eq!(body.config.main_branch, "main");
        assert!(!body.config.is_git_repo);
    }

    #[test]
    fn max_tokens_clamp_and_context_shrink() {
        // 超 200000 钳到 200000
        let mut r = req_with("m", json!("x"));
        r.max_tokens = Some(1_000_000);
        assert_eq!(build_request(&r).params.max_tokens, 200000);
        // max_completion_tokens 覆盖 max_tokens
        r.max_tokens = Some(5000);
        r.max_completion_tokens = Some(999999);
        assert_eq!(build_request(&r).params.max_tokens, 200000);
        // 上下文压缩：1.8M 字节 → estTokens = 900064 → room = 148512
        let big = "a".repeat(1_800_000);
        let mut r2 = req_with("m", json!(big));
        r2.max_tokens = Some(384000);
        assert_eq!(build_request(&r2).params.max_tokens, 1048576 - 900064);
    }

    #[test]
    fn parse_context_overflow_matches_go() {
        let msg = "This model's maximum context length is 1048576 tokens. However, \
                   you requested 1048593 tokens (848593 in the messages, 200000 in the \
                   completion). Please reduce the length of the messages or completion.";
        assert_eq!(parse_context_overflow(msg), 848593);
        assert_eq!(parse_context_overflow("unrelated error"), 0);
        assert_eq!(parse_context_overflow(""), 0);
        assert_eq!(
            parse_context_overflow("... requested 100 tokens (42 in the messages, 58 in the completion)"),
            42
        );
        // "0 in the messages" 不返回（Go 要求 n > 0）
        assert_eq!(
            parse_context_overflow("... requested 100 tokens (0 in the messages, 100 in the completion)"),
            0
        );
    }

    #[test]
    fn upstream_headers_are_the_fixed_five() {
        let body = build_request(&req_with("m", json!("x")));
        let spec = create_upstream_request("http://127.0.0.1:1", &body, "k", "1.2.3");
        assert_eq!(spec.method, "POST");
        assert_eq!(spec.url, "http://127.0.0.1:1/alpha/generate");
        let names: Vec<&str> = spec.headers.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            vec![
                "Content-Type",
                "Authorization",
                "x-command-code-version",
                "x-cli-environment",
                "Accept"
            ]
        );
        assert_eq!(spec.headers[1].1, "Bearer k");
        assert_eq!(spec.headers[4].1, "text/event-stream");
    }
}

