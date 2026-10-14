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
