# rs-comate —— Comate 插件（8786）

百度文心快码（Comate）订阅 → 本地 OpenAI 兼容接口。GUI 已注册：
`rs-gui/src/dispatch.rs` 把 `--plugin-comate` 路由到 `rs_comate::run`，生产端口 **8786**。

```
rs-comate --plugin-comate --port 8786      # 生产（GUI 拉杆也是这条）
rs-comate --plugin-comate --port 18786     # 测试端口，别碰 8786
```

## 两条传输（`COMATE_TRANSPORT`，默认 `cloud`）

| | `cloud`（默认） | `zulu`（回退） |
|---|---|---|
| 上游 | `https://comate.baidu.com` 直连 | 本机 `zulu serve`（8792） |
| 客户端工具 | **`tool_calls` 正常往返** | 结构上不支持（会话体里没有 `tools`） |
| usage | 真实 token（含 `cached_tokens`） | 恒 0 |
| 本地依赖 | 无 | zulu serve + comate-engine 内核 |
| 真机耗时（一问一答） | **约 2s** | 16~22s |
| 用途 | 生产默认 | 对照 / 兜底 / 黄金基准 |

`zulu` 路线保留完整的 Go 等价合同（见文末对照表），`cloud` 路线由
`tests/cloud_contract.rs` 的进程内假云锁住。

## 云直连

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

- `/v1/models` 只暴露**去后缀短名**（`glm-5.3`），请求时用目录映射回完整
  `modelId`；`modelKey` 必须完整 id，用显示名会 `model.unavailable`。
- `auto`/空模型 → 目录里 `isDefault` 的那条（没有就第一条）；目录失败时按
  `FALLBACK_MODELS` 兜底解析。
- 非流式响应的 `usage` 是 `inputTokens + cachedInputTokens`（Comate 的 inputTokens
  **不含**缓存命中），另带 `prompt_tokens_details.cached_tokens`。
- 流式最后一块是 `choices: []` + `usage`（OpenAI 规范），然后 `[DONE]`。
- 错误语义：`EXCEPTION` 元素 → 502 人话；未知模型 → 400 并列可用清单；
  首块之前失败才回 502，已下发后只结束流。

**没做**（如实登记）：图片输入（`attachImages` 上传链路）、`max_tokens`/`temperature`
透传（实测 `modelParams.maxTokens` 被上游接受但**忽略**，透传=假承诺）、
`response_format` 结构化输出、多轮跨请求的会话复用（每轮新建会话，只有工具续接复用）、
会员积分展示（`/api/user/agent/balance` 要 Cookie，license 头不够）。

## 登录态与环境变量

登录态按优先级解析（`src/zulu.rs::LicenseSource`，每次请求现读）：

1. `COMATE_LICENSE`（显式指定，压过一切）
2. Comate IDE 设置：`%APPDATA%\Comate\User\settings.json` 的 `baidu.comate.license`
3. CLI 登录态：`%USERPROFILE%\.comate\cli-login.json`

2026-11-15 实测：更新后 IDE 号与 CLI 号**可能不是同一个账号**，会员权益跟百度账号走，
所以默认用 IDE 那个；启动日志会打印「来源 ide-settings（账号 xxx）」并在两号不同时警告
（账号名只进本机日志，不进仓库）。

| 变量 | 作用 |
|---|---|
| `COMATE_TRANSPORT` | `cloud`（默认）/ `zulu` |
| `COMATE_LICENSE` | 指定 license |
| `COMATE_CLOUD_BASE` | 改云入口（契约测试用） |
| `COMATE_PLUGIN_VERSION` | 默认 `zulucli-1.8.1`，Comate 更新后跟着改 |
| `COMATE_CWD` / `COMATE_MODE` | **仅 zulu 路线**：钉死工作目录 / 模式（Ask 只读） |
| `COMATE_ZULU` | **仅 zulu 路线**：zulu CLI 路径覆盖 |

`--upstream` / `--license-file` / `--zulu` 是回放/对照参数，真实运行不要带。

## 测试

```
cargo test        # 28 单测 + 6 云合同 + 3 黄金 + 10 HTTP 合同 = 47 条
```

黄金基准两份已入库（`tests/golden/`，合计 142,743 B），跑测试不需要 `.work/`；
采集口径见 [`tests/golden/README.md`](tests/golden/README.md)。

门禁不是空转：2026-10 绞杀者阶段用临时 mutation 脚本逐项验证过（脚本未入库）——
改坏 `gjson` 的 `<` 转义、扫描器 1MB 上限、缓存写入条件、`cleanPath` 快路径、
模型短名大小写折叠、405 文案、`[DONE]` / `thinking-delta`，对应黄金用例都会 RED。

## 已知差异（zulu 路线，登记不静默跟随）

1. **HEAD 请求的 `content-length`**：Go 对「handler 没写体」的 HEAD 响应（如 HEAD `//v1/models`
   的 307）**不带** `content-length`，hyper 一定补 `content-length: 0`。门禁里这一种形态被
   **显式**放行（只放行「HEAD 且恰为 0」），另见 `tests/common/mod.rs::assert_headers_like_go`。
2. **出站请求头**：zulu 路线到 127.0.0.1:8792 的头由各自 HTTP 客户端生成（Go：
   `Go-http-client/1.1` + `Accept-Encoding: gzip`；Rust：`reqwest` 的 UA + `accept: */*`），
   上游是本机、不进指纹轴。云直连路线是另一套（`login-name` 等，见上）。
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
10. **`cwd` 的字节偏移**：索引语义照搬 Go（在 `lower` 上算偏移、回 `prompt` 切片）；
    若某语言的小写转换改变了字节长度（极端 Unicode），两边都会漂，真实提示词里未遇到。

## 未覆盖（如实登记）

1. **`503 no_zulu` 分支没有黄金基准**：本机 `D:\Program Files (x86)\Comate\...\zulu.cmd` 存在，
   `findZulu()` 永远非空 ⇒ Go 侧采不到这条。Rust 侧按 Go 源码逐字对齐文案，
   并由 `tests/http_contract.rs::missing_zulu_and_missing_license_report_go_messages` 锁定
   （**源码级对齐、无运行时基准**）。
2. **云直连的边界未覆盖**：图片、结构化输出、采样参数、跨请求会话复用（见上文「没做」）；
   `cloud` 路径只有进程内假云合同 + 2026-11-15 真机活体验证，**没有**像 zulu 那样的
   逐字节黄金基准。
3. **超 32MB 请求体的连接行为**：Go 的 server 会在 handler 返回后尝试读掉剩余体再关连接，
   Rust 侧到 32MB 即停读。实测 33MB 时客户端仍能拿到 400（见合同用例），
   但极端并发下的连接复用行为未逐字节对比。

## 历史：zulu 1.8.1 适配与 Go 对照表

2026-11-15 Comate 更新（1.108.2 / zulu 1.8.1）后，zulu 路线踩过三件事，
现在都由 `cloud` 默认路线绕过、只在 `COMATE_TRANSPORT=zulu` 时相关：

1. **`PLATFORM=saas` 是硬门槛**：zulu 1.8.1 默认 `PLATFORM=internal`，会去连
   `comate.baidu-int.com`（公网解析不了）→ 会话拿不到 `conversationId/taskId` →
   统一报「检测网络后重试」。取值只认小写 `saas`/`internal`/`poc`，**大写 `SAAS` 等于没设**。
   现在 `list-model` / `serve` 两个子进程都注入（`src/zulu.rs`）。
2. **流式协议换了一套**：旧 `text-delta`/`thinking-delta` + `completed`；
   新 `element-add` / `element-patch`（**累计全文**）+ `type=task_done`，失败给 `EXCEPTION` 元素。
   `src/translate.rs::SseAcc` 两套通吃，旧行为由黄金锁着。
3. **`zulu serve` 是单例**：读 `~/.comate/zulu-serve.pid`，命中就复用并立刻退出，
   **不看 `--port`/`--license`**。现在 spawn 前若 pid 在别的端口先 `taskkill`；
   8792 上 license 不匹配时 `post_upstream_retry` 重启再试一次。

下表是 2026-10 绞杀者阶段「Rust 逐字节等价 Go 实现」的对照记录。
**Go 实现已于 2026-10-18 整体删除**，表只作历史索引，门禁仍照常跑。

| Go（`internal/…`，已删除） | Rust | 门禁锁在哪 |
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