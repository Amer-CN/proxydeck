# rs-comate —— Comate 插件的 Rust 重写（P2，绞杀者路线）

`internal/comate`（8786，百度文心快码；托管 zulu serve 8792）的 Rust 等价实现。
**本阶段只在测试端口跑通，不注册进 `app/plugins.go`、不动 Go 构建、不碰任何现行服务端口。**

```
rs-comate --plugin-comate --port 18786          # 手动在测试端口拉起（默认端口仍是 Go 的 8786）
```

与 Go 侧同一套判定：登录态 `%USERPROFILE%\.comate\cli-login.json`、zulu CLI 探测
（`COMATE_ZULU` → ProgramFiles → `D:\Program Files (x86)`）、`COMATE_CWD` / `COMATE_MODE`
两条环境开关、上游 `http://127.0.0.1:8792/api/v1/conversations/init`。

**对照/回放专用参数**（真实运行不要带，带了就和 Go 版不是同一个环境）：

```
--upstream <url>        覆盖上游 base（默认与 Go 的 upstreamBase 同值）
--license-file <path>   覆盖登录态文件
--zulu <path>           覆盖 zulu CLI 路径
```

## 对齐表

| Go（`internal/…`） | Rust | 门禁锁在哪 |
|---|---|---|
| `comate/translate.go` `flattenMessages` | `src/translate.rs::flatten_messages` | 黄金 part1 `flatten` 7 条 + 出站体 21 条 |
| `comate/translate.go` `chunkJSON` / `chunkReasoningJSON` / `completeJSON` | `src/translate.rs` 同名函数 | 黄金 part1 `chunks` 13 条（含 `\b\f`、HTML 转义、U+2028） |
| Go `encoding/json` 的 map 排序 + HTML 转义 | `src/gjson.rs` | 同上（13 条里 6 条专测转义/键序） |
| `comate/server.go` `handleChat` 的请求体解析与 400 文案 | `src/godecode.rs`（Go 兼容解码器） | 黄金 part1 `once` 段 12 条畸形体 |
| `comate/server.go` `readFullAnswer` + `bufio.Scanner` 1MB 上限 | `src/translate.rs::read_full_answer` / `LineScanner` | 黄金 part1 `readfull` 8 条（含 1.8MB 单行） |
| `comate/server.go` `handleChatStream` 读循环 | `src/lib.rs::StreamRun` | 黄金 part2 14 条（下游帧逐字节 + `[DONE]`） |
| `comate/server.go` `handleChatOnce` / `writeError` | `src/lib.rs` | 黄金 part1 `once` 37 条 |
| `comate/server.go` `handleModels` / `shortModelID` / `fallbackModels` | `src/lib.rs` | 黄金 part1 `models` 12 条（含 zulu 调用计数） |
| `comate/server.go` `cachedModels` 10 分钟缓存 | `src/lib.rs::cached_models` + `ModelsCache` | `models-cached-second-call`（1 次）、`models-list-error-twice` / `models-null-list-twice`（2 次） |
| `comate/server.go` `resolveModel`（精确 → 忽略大小写短名） | `src/lib.rs::resolve_model` | 黄金 part1 `out/wire` 21 条 |
| `comate/zulu.go` 登录态 / `findZulu` / `zuluHealth` / `listModels` / `ensureZuluServe` | `src/zulu.rs` | 黄金 part1（探活计数、`list-model` 调用计数）+ `tests/http_contract.rs`（spawn argv、超时） |
| `clientcwd.Extract` / `Dir` | `src/cwd.rs` | 黄金 part1 `out` 段的 cwd 三条 + 单测 |
| `comate/server.go` `corsWith` | `src/lib.rs::cors` | `tests/http_contract.rs::cors_is_on_every_response_and_options_short_circuits` |
| `net/http.ServeMux` 的 404 / 路径清洗 307 | `src/lib.rs::handle_fallback` + `clean_path` | 黄金 part1 `routes` 14 条（含 `//v1/models` 与三条 `..` 清洗） |
| `handleChat` 的 32MB `io.LimitReader` | `src/lib.rs::read_capped_body` | `tests/http_contract.rs::oversized_body_is_truncated_not_rejected`（33MB） |

## 测试

```
cargo test                    # 14 单测 + 3 黄金门禁 + 10 合同 = 27 条
cargo test -- --nocapture     # 看黄金覆盖汇总（libtest 默认吞掉通过用例的输出）
```

黄金基准两份都**已入库**（`tests/golden/`，合计 142,743 B），无需 `.work/` 即可跑；
再生成流程、采集器的纪律与「超长 SSE 只存可重建规格」的口径见
[`tests/golden/README.md`](tests/golden/README.md)。

**门禁不是空转**（`.work/rust-rewrite/_mutation-check.ps1` 实测：改坏一处必红）：

| 改坏的地方 | 门禁反应 |
|---|---|
| `gjson` 的 `<` 转义去掉 | part1 RED（chunk 字节不一致） |
| 扫描器 1MB 上限失效 | part1 RED（`read-oversize` 不再报 `token too long`） |
| 失败也写进模型缓存 | part1 RED（`models-list-error-twice` 调用计数不符） |
| `cleanPath` 快路径返回 `np` | part1 RED（`/health/` 变成 307） |
| 短名解析忽略大小写失效 | part1 RED（`out-model-shortname-casefold` 出站体不符） |
| 405 文案改字 / 登录态 TrimSpace 去掉 | part1 RED（once 段逐字不符） |
| 流式不发 `[DONE]` / `thinking-delta` 不透出 | part2 RED（下游帧不符） |

## 已知差异（登记，不静默跟随）

平台级（与 P0/P1 同一类，圆桌点名的风险项，本阶段不处理）：

1. **HEAD 请求的 `content-length`**：Go 对「handler 没写体」的 HEAD 响应（如 HEAD `//v1/models`
   的 307）**不带** `content-length`，hyper 一定补 `content-length: 0`。门禁里这一种形态被
   **显式**放行（只放行「HEAD 且恰为 0」），另见 `tests/common/mod.rs::assert_headers_like_go`。
2. **出站请求头**：`postUpstream` 到 8792 的头由各自的 HTTP 客户端生成（Go：`Go-http-client/1.1`
   + `Accept-Encoding: gzip`；Rust：`reqwest` 的 UA + `accept: */*`）。上游是本机 zulu，
   不进指纹判定轴；与 P0/P1 登记的同类差异。
3. **上游连接失败时的文案**：Go 是 `Post "http://…": dial tcp 127.0.0.1:8792: connectex: …`，
   Rust 是 reqwest 的措辞。**只影响 502 的 message 文本**，形状（502 + `upstream_error`）一致。
4. **环境变量读取时机**：Go 每请求读 `COMATE_MODE` / `COMATE_CWD` / `findZulu()`，
   Rust 在启动时解析一次。插件进程的环境在生命周期内不变，运行时行为等价（登记备查）。
5. **SSE 事件字段名大小写**：Go 的 `json.Unmarshal` 忽略大小写匹配（`KIND` 也认），
   Rust 侧按小写精确匹配。真实上游全小写。
6. **`resolveModel` 的非 ASCII 大小写折叠**：Go 用 `strings.EqualFold`（Unicode simple fold），
   Rust 侧用 ASCII 折叠。模型 id 全 ASCII。
7. **`readFullAnswer` 的内存形状**：Go 用 1MB 缓冲的 scanner 边读边判，
   Rust 先把整条响应体读进内存再按同一规则切行（行长上限与错误文案一致）。
8. **`\u` 代理对边界**：孤立/非法代理对的措辞按 U+FFFD 处理，与 Go 在极端输入下的
   二次解析顺序可能不同（黄金里无此形态）。
9. **子进程失败文案**：`list-model` / `serve` 启动失败的错误串是各自运行时的措辞
   （Go 的 `exit status 1` vs Rust 的 `exit status: 1`），只进日志、不进 HTTP 响应；
   `zulu serve 启动超时（20 秒未就绪）` 这句**进响应**，已逐字对齐。
10. **`clientcwd` 的字节偏移**：索引语义照搬 Go（在 `lower` 上算偏移、回 `prompt` 切片）；
    若某语言的小写转换改变了字节长度（极端 Unicode），两边都会漂，真实提示词里未遇到。

## 未覆盖（如实登记）

1. **`503 no_zulu` 分支没有黄金基准**：本机 `D:\Program Files (x86)\Comate\...\zulu.cmd` 存在，
   `findZulu()` 永远非空 ⇒ Go 侧采不到这条。Rust 侧按 Go 源码逐字对齐文案，
   并由 `tests/http_contract.rs::missing_zulu_and_missing_license_report_go_messages` 锁定
   （**源码级对齐、无运行时基准**）。
2. **与真 zulu（8792）的端到端对话未做**：真 zulu 上跑着用户正在用的实例，简报明令不许动；
   会话请求体与下游帧的合同由黄金基准锁住（采集时上游是合成的），
   但没有「真实 zulu 回真答案」的活体验证。
3. **超 32MB 请求体的连接行为**：Go 的 server 会在 handler 返回后尝试读掉剩余体再关连接，
   Rust 侧到 32MB 即停读。实测 33MB 时客户端仍能拿到 400（见合同用例），
   但极端并发下的连接复用行为未逐字节对比。

## 云直连（2026-11-15 起是默认传输）

`COMATE_TRANSPORT=cloud`（默认）→ 直接打 `https://comate.baidu.com`，**不碰本机 zulu serve**；
`COMATE_TRANSPORT=zulu` → 走上面那套老 zulu serve（黄金基准与兜底用）。

为什么换：zulu 的会话接口字段全集里**没有 `tools`**，客户端工具结构上注入不进去；
而官方 CLI 的 HTTP 层本来就是这套 HTTPS。2026-11-15 真机实测（同一台机、同一个号）：

| | zulu serve | 云直连 |
|---|---|---|
| 非流式一问一答 | 16~22s | **2.0s** |
| 流式首字 | 7~10s | **1.5~2s** |
| 工具调用 | 不支持（客户端工具） | **`tool_calls` 正常往返** |
| usage | 恒 0 | 真实 token（含 `cached_tokens`） |
| reasoning | 有（1.8.1 元素帧） | 有（`reasoningDelta`） |
| 依赖 | zulu serve + comate-engine 内核 | 无本地进程 |

协议（`src/cloud.rs`，照 Comate2API / magpie-comate-auth 的公开实现对了一遍）：

```
GET  /api/v2/api/models/available?key=<license>&type=ZULU&...   → 15 个模型 + 能力字段
POST /api/aidevops/autocomate/rest/autowork/v2/conversation      → data.id
POST .../v2/task                                                → data.taskId
POST .../v2/execute                                             → SSE
```

认证只要头 `login-name` / `Uuap-login-name` = license（**无签名、无设备绑定**），
外加 `X-Source: COMATE`、`plugin-version: zulucli-1.8.1`、`x-skip-fcnap: yes`。

**客户端工具怎么注入**（`src/cloud_chat.rs`）：OpenAI 的 `tools` 被塞进
`agentInfo.mcpInfos`，声明成一个名叫 `openai_client` 的「假 MCP 服务器」，
同时给 `agentPrompt` 写清「用 UseMcpTool 调、参数放 arguments」。
模型回 `FUNCTION_CALL_*` 帧（`input` 是**按 key 字符串追加**的碎片），
`ToolCallAcc` 拼成完整调用后翻成标准 `tool_calls`；客户端把结果带回来时，
凭 `tool_call_id` 找回 `/conversation`+`/task` 原会话，用 `query=""` +
`toolUseResults`（native id/name/params + 结果文本）续接——**不新建会话**。

其它细节：

- `/v1/models` 仍只暴露**去后缀短名**（`glm-5.3`），请求时用目录映射回完整
  `modelId`；`modelKey` 必须完整 id，用显示名会 `model.unavailable`。
- `auto`/空模型 → 目录里 `isDefault` 的那条（没有就第一条）。
- 非流式响应的 `usage` 是 `inputTokens + cachedInputTokens`（Comate 的 inputTokens
  **不含**缓存命中），另带 `prompt_tokens_details.cached_tokens`。
- 流式最后一块是 `choices: []` + `usage`（OpenAI 规范），然后 `[DONE]`。
- 环境变量：`COMATE_TRANSPORT`（cloud/zulu）、`COMATE_CLOUD_BASE`（改入口，契约测试用）、
  `COMATE_PLUGIN_VERSION`（默认 `zulucli-1.8.1`，跟着 Comate 更新走）、`COMATE_LICENSE`
  （压过 IDE 设置与 cli-login）。

**没做**（如实登记）：图片输入（`attachImages` 上传链路）、`max_tokens`/`temperature`
透传、`response_format` 结构化输出、多轮跨请求的会话复用（每轮新建会话，只有工具续接复用）、
会员积分展示（`/api/user/agent/balance` 要 Cookie，license 头不够）。

合同测试：`tests/cloud_contract.rs` 用进程内假云复刻三步协议，锁住
「出站 modelKey 是完整 id」「auth 头与 uploadBaseInfo」「工具调用 ↔ tool_calls」
「工具结果续接 query 为空且 toolUseResults 带 native 参数」。

## 2026-11-15 适配（Comate 1.108.2 / zulu 1.8.1 更新后）

更新后三条实测结论（真机验证，不是推测）：

1. **`PLATFORM=saas` 是硬门槛**。zulu 1.8.1 的 `PLATFORM` 默认 `internal`，会去打
   `comate.baidu-int.com`（百度内网，公网解析不了）→ 会话创建拿不到 `conversationId/taskId`
   → 内核抛「您当前会话执行遇到了异常，请检测网络后重试！」。
   取值只能是 `saas` / `internal` / `poc`，**大写 `SAAS` 不在映射表里（等于没设）**。
   现在 `list-model` 与 `serve` 两个子进程都显式注入 `PLATFORM=saas`（`src/zulu.rs`）。
2. **流式协议换了一套**。旧：`delta-batch` 里 `text-delta`/`thinking-delta` 增量 + `kind=completed.content` 全文。
   新：`delta-batch` 里 `element-add`（首段）/ `element-patch`（**累计全文，不是增量**）
   + 结束帧 `{"type":"task_done","status":"completed"}`；失败时正文位置先给一个
   `EXCEPTION` 元素（`content` 是人话错误）。`src/translate.rs::SseAcc` 两套一起消化，
   旧的逐字节行为由黄金基准锁着（顶层 `element-add` 的 REASON 仍按 Go 丢掉）。
   新协议的 `REASON` 现在会透成 `reasoning_content`（`delta-batch` 内的元素帧）。
3. **登录态有两个账号**。`~/.comate/cli-login.json`（CLI 登录）与
   `%APPDATA%\Comate\User\settings.json` 的 `baidu.comate.license`（IDE 登录）这次是**不同账号**。
   用户在用的、买会员的是 IDE 那个，所以生产解析顺序改为
   `COMATE_LICENSE` → IDE `settings.json` → `cli-login.json`（`src/zulu.rs::LicenseSource`）。
   两个 license 都能过 `zulu run`；`serve` 的 `-l` 必须与请求里的 `license` 一致，否则 403。

附带修掉的两个稳定性问题：

- **`zulu serve` 是单例**：它读 `~/.comate/zulu-serve.pid`，命中就「复用」并立刻退出，
  **完全不看 `--port`/`--license`**（实测 `serve --port 8892` 直接回 `{"port":8792,"reused":true}`）。
  Comate IDE 自己拉过 serve（例如 8779）时，我们 spawn 的进程会原地退出、8792 永远不健康
  （旧表现：`zulu serve 启动超时（20 秒未就绪）`）。现在 spawn 前若 pid 文件指向别的端口，
  先 `taskkill` 那个实例再拉自己的（`src/zulu.rs::stop_serve_from_pid_file`）。
  代价：IDE 重启后第一次对话会把它的 serve 换成我们的（两者都用 IDE license，之后会互相复用）。
- **403 自愈**：若 8792 上已有 serve 但启动时 `-l` 是另一个账号，第一次请求会 403
  `does not match zulu serve auth`；现在按当前 license 重启 serve 并重试一次（`post_upstream_retry`）。

模型目录（`zulu list-model` 实时）这次变成 15 个：新增 `deepseek-v4.1-flash`、
`ernie-5.5-flash-preview`，去掉 `glm-5.0-fc`、`kimi-k2.5-oneapi`。
`FALLBACK_MODELS` 常量**有意没动**——它是 list-model 失败时的兜底，且被 Go 黄金基准
（`models-fallback-*`）逐字节锁着；要更新得连黄金一起重采，留作下版待办。

已实测通过（2026-11-15）：非流式 200 真答案；流式正文 + `reasoning_content` + `stop` + `[DONE]`；
`/v1/models` 15 个实时目录；外部 serve 占别的端口时自动换血；license 不匹配时 403 自愈。
新增 4 条单测锁新协议（`src/translate.rs` 底部，用真机抓的帧形态）。
