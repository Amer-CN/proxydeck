# rs-proxy —— ProxyDeck 主代理的 Rust 等价实现（绞杀者路线 P6）

> **状态：测试端口可跑的试点件，未接入 GUI / 未切流。** 不动 55990，不改 `app/`，不参与发版。
>
> 替换对象：`internal/proxy`（9 文件）+ `internal/api`（4 文件）+ `internal/server`（1 文件）
> + `internal/appdir` 的主代理由到的那部分 + `internal/clientcwd` + `internal/version`。
> 对外是 **55990 上的主代理**：CommandCode 路线（出站 `/alpha/generate`）+ 三条入站协议
> （OpenAI chat / OpenAI Responses / Anthropic Messages）。

## 一、怎么跑

```powershell
# 构建注意：F: 盘紧张，target 指到 E:
$env:CARGO_TARGET_DIR = 'E:\cargo-target\rs-proxy'
cd rs-proxy
cargo run -- --port 18790            # 测试端口，别用 55990
```

命令行（对齐 `app/plugin_modes.go` 的主代理分支）：

| 参数 | 含义 |
|---|---|
| `--port <n>` | 监听端口（默认 55990；测试请显式给测试端口） |
| `--host <h>` | 监听地址（默认 `127.0.0.1`） |
| `--api-key <k>` | 服务端默认 key；留空则读数据目录下的 `api-key.txt` |
| `--debug` | 打印请求/响应体（`truncateLog` 截断 20000 字节） |
| `--portable` | 便携模式（数据目录 = exe 同级，见 `appdir` 判定链） |

## 二、合同清单（对齐表）

**C1~C14 纯函数 / 结构面**（黄金 `pure` 段逐字节锁死）

| # | 合同 | Go 位置 | Rust |
|---|---|---|---|
| C1 | 模型别名表 92 条，键值逐条 + 条数一致 | `model.go` `modelAliases` | `model.rs` `MODEL_ALIASES` |
| C2 | `MapModel` 归一化规则（trim/小写/去前缀） | `model.go` `MapModel` | `model.rs` `map_model` |
| C3 | 模型目录 61 条（id / owned_by / on_go）+ 厂商→id 排序 | `models.go` `catalogModels` | `models.rs` `CATALOG_MODELS` |
| C4 | 套餐门槛（`checkPlanModel` 的放行/拒绝文案） | `anthropic.go` | `anthropic.rs` `check_plan_model` |
| C5 | `ConvertMessages`（角色/内容块/tool-call/tool-result 翻译） | `convert.go` | `convert.rs` |
| C6 | `ExtractSystem`（system + developer 换行拼接；rest 为空时是 nil） | `convert.go` | `convert.rs` |
| C7 | `estTokens`（**字节/2 + 64**，只数 string 与 text 块 + tool_calls） | `convert.go` | `convert.rs` |
| C8 | `parseContextOverflow`（`requested N tokens (M in the messages`） | `proxy.go` | `build.rs` |
| C9 | `normalizeFinishReason`（`tool-calls`→`tool_calls` 等） | `proxy.go` | `stream.rs` |
| C10 | `planLabel`（go/goat/pro/max_10x/… ） | `stats.go` | `stats.rs` |
| C11 | `trimBearer`（去 `Bearer ` 前缀但**不改 key 大小写**） | `usage.go` | `usage.rs` |
| C12 | `estimateCost`（按官方单价 × 本地 token） | `stats.go` | `stats.rs` |
| C13 | Go `encoding/json` 写出规则（键序/浮点/`<>&` 转义/U+2028） | `encoding/json` | `gjson.rs` |
| C14 | Go `encoding/json` 解码规则（逐字错误文案/字段序/`null` 语义） | `encoding/json` | `godecode.rs` |

**C15~C40 路由与协议面**

| # | 合同 | 说明 |
|---|---|---|
| C15 | 路由表逐条：`/v1/chat/completions`、`/chat/completions`、`/v1/responses`、`/v1/messages`、`/messages`、`/v1/models`、`/v1/usage`、`/v1/stats`、`/v1/calibration`、`/health` | 全部挂 `any`，方法判定留在 handler（与 Go 的 `HandleFunc` 同） |
| C16 | 兜底 404 = `http.NotFound`：`404 page not found\n` + `text/plain; charset=utf-8` + `nosniff` | `http.rs::h_not_found` |
| C17 | `GET /v1/chat/completions` 等 → 405，body 走 `writeOpenAIError` | 黄金 `edge-*-get` |
| C18 | `GET /v1/messages` → 405，body 走 `writeAnthropicError`（**另一套形状**） | 黄金 `edge-msg-get` |
| C19 | `/health` 不看方法：任何方法 200 + `{"status":"ok"}` + `text/plain; charset=utf-8` | 黄金 `route-boundaries #8~#10` |
| C20 | `?plan=go` **精确匹配**（`plan=GO` 等同不带）；`/v1/models` 是本地目录、不打上游 | 黄金 `route-boundaries #0~#4` |
| C21 | `/v1/models` 体 = `json.Encoder` 形态（**末尾换行**） | 黄金 `resp_len` |
| C22 | `/v1/stats` 键序字节序：`calibrated,cost,models,started,statsFile,today,total` + **末尾换行** | 黄金 `stats-and-calibration` |
| C23 | `statsFile` = `filepath.Base(p.Stats.file)`；未设文件时是 **`"."`** | 黄金 `route-boundaries #17` |
| C24 | `/v1/calibration` POST：`model` 缺失 → 400、`tokens` 非数字 → 400、成功体写死 `{"ok":true,"msg":"校准已保存"}`（**无末尾换行**） | 黄金 `stats-and-calibration #3~#8` |
| C25 | 无 key → 401：chat/responses 用 OpenAI 形状、messages 用 Anthropic 形状、usage 用 OpenAI 形状 | 黄金 `edge-*-nokey` |
| C26 | 客户端 `Authorization` 优先于服务端默认 key | 黄金 `edge-chat-noauth-stream` / `chat-nonstream` |
| C27 | 出站五头：`Content-Type` / `Authorization` / `x-command-code-version` / `x-cli-environment: production` / `Accept: text/event-stream` | `build.rs::create_upstream_request` |
| C28 | 出站体字段序 = Go 结构体声明序（`config,memory,taste,skills,params,threadId`） | 黄金 `cc-pipeline` 28 步 |
| C29 | `max_tokens`：默认 64000 → 钳 200000 → 再按 `CC_CONTEXT_LIMIT - estTokens - len(system)/2` 压（下限 1024） | `build.rs::build_request` |
| C30 | `messages` 的 **nil vs 空切片**：一条都没产出时 Go 给 `null` | 黄金 `responses-convert #6` |
| C31 | context 超限重试：读首行报错 → 压缩 `max_tokens` → **重发一次**（上游命中 2 次） | 黄金 `edge-chat-overflow-retry` |
| C32 | 上游 4xx 透传状态码、5xx → 502，错误体文案 `Upstream error: <body>` | 黄金 `edge-chat-upstream-*` |
| C33 | 流式下游 SSE 逐事件（`data: ...\n\n`）+ `[DONE]` + `finish_reason` 归一 | 黄金 `chat-stream*` |
| C34 | anthropic 流式用 `event: <name>\ndata: <body>\n\n`（**两行**） | `anthropic.rs` |
| C35 | Responses 流式事件序列（`response.created`/`output_item.added`/…/`completed`） | 黄金 `resp-stream*` |
| C36 | `/v1/usage` 四端点聚合 + `planLabel`/`daysLeft`/`percentUsed` 口径 | 黄金 `usage-endpoints` |
| C37 | `/v1/usage` 成功才缓存（`errors==0 && totalTokens>0`），TTL 20s | `usage.rs::USAGE_TTL` |
| C38 | **`/v1/usage` 缓存响应少一个字节**：Go 先 `writeJSON`（带换行）给客户端，再存 `json.Marshal`（无换行） | 黄金 `usage-endpoints` 两步 1298/1297 |
| C39 | `/internal/usage` 的 `items` 是 `json.RawMessage` ⇒ **原文内嵌**（不重编码） | `usage.rs::raw_member_of` |
| C40 | `/v1/usage` 的 whoami 失败降级：200 + `errors[]` + `usageUrl`（不打后续端点） | 黄金 `usage-degraded` |

**C41~C48 数据与统计面**

| # | 合同 | 说明 |
|---|---|---|
| C41 | `bai-stats` 式的 `UsageStats`：`/v1/stats` 的 `models/today/total` 结构、三者全 0 不入账 | `stats.rs` |
| C42 | 统计落盘形态 `{"models":{…},"started":N}`（结构体声明序、`days` 分桶） | `stats.rs::encode_stats` |
| C43 | 校准覆盖：有校准的模型按官网值等比缩放，余数落在 `cacheWriteTokens` | `http.rs::h_stats` |
| C44 | 统计节流落盘（≥2s）+ 读时兜底 | `stats.rs` |
| C45 | `/v1/calibration` 的 `calibration.json` 读写（同目录） | `stats.rs` |
| C46 | `ProbeKey`（whoami 免费端点探活，**只回状态字**，不回 key/响应体） | `probe.rs` |
| C47 | `GetCommandCodeVersion`（npm registry + 30 分钟缓存，失败回 `"unknown"`） | `version.rs` |
| C48 | 数据目录判定链 `PROXYDECK_DATA_DIR` → 便携标记 → `%APPDATA%\ProxyDeck\` | `lib.rs` + `main.rs` |

## 三、门禁设计

| 目标 | 内容 |
|---|---|
| `tests/golden_proxy.rs` | **黄金门禁**：纯函数段（C1~C14 逐字节）+ 场景段（52 场景 / 125 步：下游状态/头/体 + 上游方法/路径/体/合同头） |
| `tests/http_contract.rs` | **硬合同**：3.6MB 体到达上游、404/405 三件套、`json.Encoder` 末尾换行、`/v1/usage` 缓存形态、无 key 401 三形状、基准存在性守门 |
| `tests/golden/golden-proxy-go.json` | 真 Go 采收的基准（3.7 MB，SHA-256 `7AD9A99D…`），再采集链见 `tests/golden/README.md` |

跑法（Huorong 会删刚链接的测试 exe，必须用绕行脚本）：

```powershell
pwsh -File .work/rust-rewrite/_p6test.ps1 -Targets lib
pwsh -File .work/rust-rewrite/_p6test.ps1 -Targets golden_proxy
pwsh -File .work/rust-rewrite/_p6test.ps1 -Targets http_contract
```

## 四、已知差异（登记，不静默跟随）

1. **TLS 指纹**：Go 侧 `Appdir`/主代理用的是标准 `net/http` 栈，Rust 侧是 `reqwest(default-tls)`。
   两者的 JA3/JA4 不同——这是圆桌 A/C 席点名的「Rust 无 uTLS 对等物」的同一个问题，
   本阶段不处理（主代理出站是 `api.commandcode.ai`，不在团结凭证链红线上）。
2. **出站默认头由各自客户端生成**：`User-Agent`、`Accept-Encoding`、`Host`、`Content-Length`。
   采集侧只记**合同头**（`accept`/`authorization`/`content-type`/`x-command-code-version`/`x-cli-environment`），
   其余不进基准。Go `net/http` 会给 `Accept-Encoding: gzip`，reqwest 默认不发——**未对齐**。
3. **`file_dir()` 在「只有文件名」时形态不同**：Go `filepath.Dir("stats.json") == "."`，
   Rust `Path::new("stats.json").parent() == ""`。**拼接结果相同**
   （`PathBuf::from("").join("x") == "x"`），而 `file_dir()` 只被用于拼接，故未改形态。
   生产路径永远是绝对路径。
4. **`X-Forwarded-For` / `Host` 改写**：Go 主代理这条路径**没有**用 `httputil.ReverseProxy`
   （是自己拼 `http.Request` + `Client.Do`），因此也没有 Go 那套 `X-Forwarded-For` 追加；
   两侧都不发该头，无差异。CLAUDE.md 坑 2 说的 Host 改写属于**其他插件**的反代路径。
5. **`/v1/usage` 的 `fetched_at` / `cached` 字段**：两侧都没实现这两个字段（黄金里也没有）。
   采集与回放只覆盖实际存在的字段。
6. **`appdir` 只迁了主代理真正用到的那部分**：数据目录判定链（含 **exe 同目录 `portable` 标记文件**，
   同 `appdir.go:53-55` 的 `markerExists`：是文件、不是目录）、`api-key.txt` 读取
   （**明文与 `PDAPI1` 密文两形态都支持**——`src/cred_crypt.rs` 从 `rs-bai` 逐字搬来，
   与 `app/main.go` 经 `appdir.ReadFile` 的路径一致；此条原先写作「主代理这条路径不经过 DPAPI」，
   是**错误断言**，P6 修正轮 F2 已修）。
   **仍未迁**：`appdir` 的 DPAPI **写入**、ACL 加固、日志轮转、`migrate.go` 迁移逻辑——
   主代理这条路径不经过它们；若将来要整体替换 `appdir`，这几项必须补。
7. **未覆盖：`/v1/usage` 的 20s 缓存过期后行为**：门禁只锁「首次 1298 字节 / 缓存命中
   1297 字节」两条；「TTL 到期后重新打上游」需要真实等待 20s，未入门禁（代码路径与首次相同）。
8. **未覆盖：真上游端到端**。所有验收都在假上游上做（不出网、不烧真额度）——这是 P0 以来
   的一贯纪律，主代理链路与 55990 的线上实例**未做过**并发对照。
9. **`src/proxy.rs` 已删除**：早期草稿（`Proxy` 当初拟独立成模块，后来并入 `lib.rs`），
   不在模块树里、不参与编译；本轮收尾时删除以免与 `lib.rs` 的同名定义混淆。
10. **`opencode_session` 字段是空壳**：Go 主代理**没有** `x-opencode-session` 这件事
    （那是 `internal/tuanjie` 的 opencode Zen 路径，`internal/proxy` 全文无 `opencode` 字样）。
    Rust 侧保留了同名字段但恒为 `None`，仅为将来留位——**不是**合同项。
11. **孤立/边界分歧（P6 修正轮审查点名，逐条登记；影响面见备注）**：
    ① **F7** `Responses` 非流式/`completed` 在「无文本无工具」时 Go 的 `output` 为 `nil` → `null`，
    Rust 恒 `[]`（`OpenAIResponse.output` 未用 Option 建模）；黄金未覆盖。
    ② **F9** `/v1/usage` 带 `cursor` 时上游 query 键序：Go `url.Values.Encode()` 按字母序，
    Rust 走插入序；真实上游按语义取参，影响面低。
    ③ **F10** 上游回 `{"items":null}` 时 Go 写 `"items":null`（RawMessage 非 nil），Rust 跳过该键。
    ④ **F11** `/v1/calibration` 的 `parse_form` 只解析 body，Go `r.FormValue` 会合并 URL query
    ——经查询串传参时 Go 接受、Rust 不接受。
    ⑤ **F12** 校准写盘失败的 500 文案：Go 用 `err.Error()` 原文，Rust 固定「写入校准失败」。
    ⑥ **F13** 突变脚本定义 11 条、已落盘日志 8 条（缺的 3 条中含「无 key 401→200」）——
    `401` 合同本身另有黄金 + 硬合同独立覆盖，未跑的那三条待补跑。
    ⑦ **F14** 验收 1 的活体对照（20 组）结果表只打印未落盘，「差异 0」不可复核。
    ⑧ **F16** `version.rs::warm_up()` 会把结果填进缓存，而 Go `init()` 丢弃预热结果
    （Go 疑似笔误，Rust 行为更优，属未登记偏差）；`golden_proxy.rs` 有一处双分支同值死代码。
12. **重试失败路径已对齐（P6 修正轮 F3）**：Go 的 `streamChatWithRetry` 重发失败时由
    `callUpstreamChecked` 写出**真实状态码与消息**；Rust 原先 `chat.rs` 捏造
    `502 "upstream error: retry failed"`、`responses.rs`/`anthropic.rs` 甚至静默回 200 空体。
    三处现已改为按 Go 写真实错误（`err(st, msg, "api_error")`）。
    ⚠ **覆盖缺口（如实登记）**：「overflow 重发后**仍**失败」这条分支目前**没有黄金场景、也没有单测**
    ——黄金只锁了重试**成功**的形态；该分支与已覆盖的「首次调用失败」同构（同一 `err()` 出口），
    故判定为低风险，但补场景前不得声称"已覆盖"。
13. **`daysLeft` 已双侧归一（P6 修正轮 F1）**：该字段由「距周期结束的天数」算出、**每天递减**，
    采集日定值不能直接比；门禁 `norm_volatile` 已加 `replace_num_field("daysLeft", "<DAYSLEFT>")`
    （与 `days_left_from` 一起，F8 的 `trunc` vs `floor` 也已对齐 Go 的 `int(Hours()/24)+1`）。
    不加这条，黄金在采集次日必红。
14. **验收口径更正（P6 修正轮 F5）**：P6 简报原验收 4 写「自定义服务商配置读写（`protocol` 留空=保留原值、
    白名单兜底）」——**该功能不在 `internal/proxy` 里**，它属 `internal/tuanjie` 的 `tuanjie-providers.json`
    与 `isZenResponsesModel` 白名单（团结阶段范围），`internal/proxy` 全文零命中。P6 的三协议指的是
    **入站 API 形状**（OpenAI chat / OpenAI Responses / Anthropic Messages）。本子项**不适用**，
    简报已同步作废；验收 4 实际等价物是统计/用量字段（已达成）。

## 五、本轮（P6 接续轮）修了什么

| # | 位置 | 改动 | 证据 |
|---|---|---|---|
| 1 | `tests/golden/golden-proxy-go.json`（+`.work/p6/`） | **重采**：步级 `req_body`、场景级 `proxy_key`/`stats_file`/`upstream_spec`、段 D 的 `upstream` 命中、`[BIG:…]` 展开 | 连采三次逐字段只差 `collected_at` |
| 2 | `tests/golden_proxy.rs` | 读基准的 `proxy_key`/`stats_file`/`req_body`；`upstream` 三种形态（`null`/`[]`/`[{…}]`）都按「期望次数=列表长度」比；段 E 起 `FakeUsage` 并跑「无 key」那一步 | 52 场景 / 125 步全绿 |
| 3 | `tests/common/mod.rs` | 新增 `FakeUsage` / `FakeUsageDown`；`norm_volatile` 补 `<DAY>` 日键归一化 | 同上 |
| 4 | `src/http.rs` | `/v1/models`、`/v1/stats` 补 `json.Encoder` 末尾换行；新增 `http.NotFound` 兜底（体 + Content-Type + nosniff） | 黄金 `route-boundaries` / `http_contract::fallback_404_matches_go_http_not_found` |
| 5 | `src/stats.rs` | `file_base()` 未设文件时 `""` → `"."`（Go `filepath.Base("")`） | 黄金 `route-boundaries #17` + 新单测 `file_base_and_dir_match_go_filepath` |
| 6 | `src/anthropic.rs` | `drop_tool_results` 的剩余内容：只认字符串 → **原样保留数组**（Go 把它交给下游 `parseContent` 逐块翻译） | 黄金 `anthropic-convert #4` |
| 7 | `src/api.rs` + `src/build.rs` | `CCChatParams.messages` 改 `Option<Vec<…>>`：一条都没产出时写 `null`（Go nil 切片） | 黄金 `responses-convert #6` |
| 8 | `src/usage.rs` | `items` 改**原文内嵌**（`raw_member_of`）；末尾换行只加在下游响应、缓存存不带换行的形态 | 黄金 `usage-endpoints` 两步 1298/1297 + 新单测 |
| 9 | `tests/http_contract.rs`（新建） | 硬合同 10 条 | 10 passed |
| 10 | `tests/golden/README.md`（新建） | 再采集链 + 归一口径 + 8 个坑 | — |
| 11 | `src/proxy.rs`（删除） | 早期草稿，不在模块树 | `cargo test` 全绿 |

## 六、构建与测试

```powershell
$env:CARGO_TARGET_DIR = 'E:\cargo-target\rs-proxy'
cd rs-proxy
cargo test                                    # 见下方门禁三条
# 火绒会删刚链接的 exe，若 `cargo test` 报 os error 5，用绕行脚本：
pwsh -File ../.work/rust-rewrite/_p6test.ps1 -Targets lib
```

Go 侧只读对照（**不改任何 Go 源码**）：

```powershell
go test ./internal/proxy ./internal/api ./internal/server ./internal/appdir ./internal/clientcwd ./internal/version -count=1
go vet   ./internal/proxy ./internal/api ./internal/server ./internal/appdir ./internal/clientcwd ./internal/version
```
