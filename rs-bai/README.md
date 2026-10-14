# rs-bai —— B.AI 插件的 Rust 试点（绞杀者路线第一步）

本 crate 是「不整仓推倒，先拿无红线的边缘插件试点」的第一个产物：把 Go 版
`internal/bai`（8891）的对外合同照搬到 Rust，**先在测试端口独立跑，不注册进
`app/plugins.go`，不改 Go 构建，不动任何现行服务端口**。

- Go 侧参照：`internal/bai/server.go`、`internal/bai/models.go`（矩阵与密钥）、
  `internal/appdir/*`（数据目录与 DPAPI）
- 基线提交：`3a5ee06`（P0）/ `a608f55`（P0 提交）→ 本阶段 P1
- 本阶段状态：**4 条端点全部迁完**（`/health`、`/model/matrix`、`/v1/stats`、其余透传）

## 跑起来

```powershell
cargo build --release
.\target\release\rs-bai.exe --plugin-bai --port 18788          # 测试端口，勿用 8891
curl.exe -s --noproxy "*" http://127.0.0.1:18788/health
curl.exe -s --noproxy "*" "http://127.0.0.1:18788/model/matrix?refresh=1"
curl.exe -s --noproxy "*" http://127.0.0.1:18788/v1/stats
```

参数与 Go 侧同名同形（`--plugin-bai` / `--port` / `--host`），另加三个仅供黄金回放/对照的：
`--upstream <url>`（默认 `https://api.b.ai`）、`--channels-config <path>`、`--stats-path <path>`。
**真实运行不要带后两个**——不带时走与 Go 完全相同的 `internal/appdir` 判定链
（`PROXYDECK_DATA_DIR` → 便携标记 → `%APPDATA%\ProxyDeck\`），两侧读写同一个文件。

## 与 Go 侧对齐的部分

| 项 | Go 侧 | 本 crate |
|---|---|---|
| 上游 base | `https://api.b.ai` | 同（`--upstream` 可改） |
| 路由 | `/health`、`/model/matrix`、`/v1/stats` 本地；其余透传 | 同（四条全迁，见下） |
| `adaptQuirks` 四条改写 | developer→system、`max_tokens>8192→8192`、`reasoning_effort:"max"→"high"`、超 1M token 截断最早轮次 | 逐条复刻，见 `src/adapt.rs` |
| `estTokens` 估算口径 | 4 字符≈1 token；string content / 数组 text / data URI 图 / 外链图 512 / tool_calls.arguments；每条 +4 | 同 |
| **`GET /model/matrix`** | 渠道配置里的 Bearer 拉上游 `/v1/models`，进程内无 TTL 缓存 | 同（`src/matrix.rs`）：字段序 `ok,source,fetched_at,count,data[,err]`、`data` 原样嵌入、非 GET 405 纯文本、失败负缓存 60s、失败保旧值、全程持锁 single-flight、15s 超时、4MB 读入上限 |
| **渠道密钥来源** | `appdir.ReadFile` 读 `tuanjie-water-channels.json`，取 `id=="bai"` 的 key；DPAPI 密文自动解密、旧明文原样读 | 同（`src/crypto.rs`）：`PDAPI1\n`+base64+`CryptUnprotectData`（std FFI 直连 crypt32.dll，**无新依赖**）；缺失/坏文件/无条目 → 空串 |
| **`GET /v1/stats`** | `map[model]{calls,inputTokens,outputTokens,totalTokens}` + `uptimeSec`；`json.Encoder` 口径（末尾换行） | 同（`src/stats.rs`）：字段名/字段序/键序/末尾换行逐字节一致；三者全 0 不入账、`total>0?total:in+out` |
| **`bai-stats.json`** | 数据目录下，`json.Marshal(map)` 无缩进无换行 → `.tmp` + rename 原子落盘；启动读回；缺失/坏 JSON 当空表 | 同（`src/datadir.rs` + `src/stats.rs`） |
| CORS | `*` / `GET, POST, OPTIONS` / `Content-Type, Authorization`；OPTIONS→204 | 同 |
| 代理自探测 | 先看 `HTTPS_PROXY`/`https_proxy`，否则试连 7897/7890/7892/7896/10809/1080 | 同（`src/transport.rs`） |
| 请求体上限 | `bufferBody` 缓冲到 32MB（`server.go:503`） | 同，`MAX_BODY_BYTES` 显式设 `DefaultBodyLimit`（axum 默认只有 2MB） |
| 超时口径 | 转发：只给 30s 连接超时、**无含响应体的总超时**；矩阵拉清单：`context.WithTimeout(15s)` 罩住**整次**拉取 | 同：转发 client 不设 `.timeout()`；矩阵用 `RetryPolicy.deadline`（15s 总预算，**连接、等响应头、读体三段都在内**，`req.send()` 与读体各自套在剩余预算里；client 另设 30s 连接超时同 Go `net.Dialer`） |
| 重试 | 传输错误或 5xx 最多 3 次、间隔 300ms；**单发的门槛**是 `req.Body != nil && req.GetBody == nil`（请求体存在**且**不可重放），4xx 不重试 | 同：转发路径 3 次 / 300ms；矩阵拉清单是 **nil body 的 GET**（`models.go:119` 传 nil → `req.Body==nil`、`GetBody==nil`，门槛两条都不成立）⇒ Go 侧**同样**最多 3 次尝试，本 crate 用 `RetryPolicy{max_attempts: 3}` 同口径（第 3 轮曾读成「一次也不重试」，第 5 轮更正，黄金基准同批重采） |
| 矩阵响应头 | `Content-Type: application/json`（无 charset）；非 GET 405 走 `http.Error` → 纯文本 + `X-Content-Type-Options: nosniff` | 同（逐项由黄金门禁 `resp_headers` 锁住） |
| 矩阵 `data` 嵌入 | `json.Encoder` 对 `json.RawMessage` 走 `appendCompact(escape=true)`：去 token 间空白 + `<`/`>`/`&` → `\uXXXX`、U+2028/2029 → `\u2028/\u2029`（**Go 1.26.5 的实现不看是否在字符串内**） | 同（`compact_json_go`，期望值由 `tests/golden/golden-compact-go.json` 逐字节锁住） |
| 矩阵 `data:null` | `len(RawMessage)==4` → 视为存在 → `ok=true / source=live / count=0 / data:null`，`loaded=true` | 同（对齐前判 fatal，第 3 轮改正） |
| 渠道配置解析容错 | 条目缺 `id`/`key` 按零值容忍；数组里的 `null` **元素**同样容忍（Go 存成零值条目、不报错）；**类型不匹配**才整份解析失败 → 空串 → need_key；取**第一个** `id=="bai"` 条目 | 同（`ChannelKeyCfg` 字段为 `Option` + `#[serde(default)]`；数组读成 `Vec<Option<ChannelKeyCfg>>` 再 `flatten`——第 5 轮 R6 对齐） |
| `/health` 方法 | handler 不判方法，POST 也回 200 + 同一份 health JSON（`json.Encoder`，末尾换行） | 同（路由挂 `any`，body 补末尾换行） |
| 流式及时下推 | `FlushInterval = 50ms` | 流式响应体直通（chunked），逐帧下推 |

## 已知差异（试点期如实登记）

1. **TLS 指纹**：Rust 无 uTLS 对等物，ClientHello 是 rustls 默认形态，与 Go `net/http`
   不同。B.AI 无签名/指纹红线，本轮不处理；团结 8788 才会真正撞上这一条。
2. **出站头**：`Host` 由 URL 决定（同 Go 的 `Director` 改 `req.Host`）；
   `User-Agent`/`Accept-Encoding` 由各自 HTTP 客户端生成，必然与 Go 不同；
   Go `httputil.ReverseProxy` 会补 `X-Forwarded-For`/`X-Forwarded-Host`，本 crate 未补
   （对上游语义无影响，如要对齐在这一处加）。
3. **`/v1/stats` 中间件（`statsWith`）未迁**：Go 在 `/v1/chat/completions` 上挂了一层
   只读统计——请求体里取 `model`、响应体旁路拷贝（上限 8MB）抠 `usage` 入账。
   本 crate 的 `/v1/stats` 端点、累计口径与落盘形态都已对齐（见上表），
   但**没有**在 chat 路径上自动入账。属 P0 chat 转发那一块，登记待做。
4. **`/model/matrix/`（带尾斜杠）**：**两侧行为一致**，都走各自的 catch-all 透传给上游
   （Go 的 `http.ServeMux` 对「注册路径 `/model/matrix`」不因尾斜杠发 301；Rust 侧
   axum 同样落到 fallback）。此处**没有差异**——早先 README 写的「Go 会 301」经 P1 审查
   证伪，已更正；GUI 拉的是无斜杠路径，两条路都不影响。
5. **`bai-stats.json` 内容是 JSON `null` 时两侧行为不同（Go 侧更脆）**：
   Go `loadStats` 在 `json.Unmarshal(b,&m)==nil` 时把 `s.stats` 换成 `null` 解出来的
   **nil map**，随后第一次 `addStat` 写这条 nil map 会
   `panic: assignment to entry in nil map`（P1 采集器实测复现过）。
   Rust 侧把「非法形态」一律当空表（`null` / 坏 JSON / 缺失同处理），不 panic。
   要修 Go 侧这个健壮性问题须改 Go 源码，P1 阶段只读不改，故登记为差异而不是静默跟随。
6. **`--channels-config` / `--stats-path`** 是本 crate 独有的对照参数（Go 侧只有
   `PROXYDECK_DATA_DIR`）。生产不带时数据目录判定与 Go 完全一致。
7. **矩阵 4MB 读入上限的形状**：Go 用 `io.LimitReader(resp.Body, 4<<20)`——**截断**到
   4MB，截断后 JSON 解析失败 → 「上游清单无 data 数组」；Rust 读满后判「超过 4MB」。
   两者都归到 `upstream_error`（HTTP 响应里没有区别），只有日志文案不同。
   **内存形状也不同**：Go 的 `LimitReader` 是边读边弃（最多保留 4MB，峰值约 4MB）；
   Rust 的 `read_limited` 先 `resp.bytes()` 把整段读进内存再判超限，超限时峰值是
   上游体量而不是 4MB。真实清单 ~8KB，触不到这条。
8. **`compact_json_go` 的校验覆盖度**：Go 1.26.5 的 `appendCompact` 走完整 scanner
   （数值/字面量/转义 grammar 全查），Rust 侧只覆盖「能出现在合法上游清单里」的那部分。
   非法输入上可能一侧返回 `None` 而另一侧返回残缺结果——真实上游给的是合法 JSON，
   此差异只在构造输入下可见（`tests/golden/golden-compact-go.json` 锁了 10 个样本）。

## 第 3 轮（P1 审查后补修）改了哪几处

逐条对应 `code-reviewer` 对 P1 的发现（本轮只修真合同分歧，不做无关重构）：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `src/transport.rs`（`fetch_matrix_once`） | 上游 `{"data":null}` 由「判 fatal」改为**成功**：`count=0`、`data` 存 `null` 原文、`loaded=true`（Go 的判据是 RawMessage 字节长度）。新增单测 `matrix_data_null_is_success_like_go` + 黄金场景 `matrix-data-null` |
| 2 | `src/matrix.rs` / `src/stats.rs` | 响应头 `application/json; charset=utf-8` → **`application/json`**（与 Go 的 `Header().Set` 逐字一致）；黄金门禁逐步比对 `resp_headers` |
| 3 | `src/matrix.rs`（405 分支）、`src/lib.rs`（405 补体中间件） | 补 `X-Content-Type-Options: nosniff`（Go `http.Error` 自带）；`http_contract` 的 405 用例加了这条断言 |
| 4 | `src/transport.rs` | 矩阵超时改为**总预算**（`RetryPolicy.deadline`，含读体），不再是每次尝试各 15s；响应体阶段出错不再重试。新增单测 `matrix_deadline_is_a_total_budget_not_per_attempt`；黄金门禁的上游命中数改成**精确增量**断言（此前是 `>0`，它掩盖了这条差异）。**⚠ 同一行里把矩阵 GET 的尝试次数定为 1 是错的**（理由「Go 的 `http.NoBody` 不可重放」系误读，`NoBody` 在 nil-body 请求里根本不会出现），第 5 轮已改回 3，见下方第 5 轮表 |
| 5 | `src/matrix.rs`（`ChannelKeyCfg`） | 条目缺 `id`/`key` 改为逐条容忍（`Option` + `#[serde(default)]`）；类型不匹配才整份失败。单测 + 黄金场景 `matrix-config-tolerance` / `matrix-config-type-error` |
| 6 | `src/matrix.rs`（`compact_json_go`） | `data` 的嵌入按 Go 1.26.5 的 `appendCompact(escape=true)` 复刻：去 token 间空白 + `<`/`>`/`&` → `\uXXXX` + U+2028/2029；期望值来自真 Go `encoding/json` 采的 `tests/golden/golden-compact-go.json`（10 样本），另有黄金场景 `matrix-data-compact` 守 HTTP 全链路 |
| 7 | `src/lib.rs`（路由 + `/health`） | `/health` 由 `get()` 改 `any()`：POST 也回 200 health JSON（Go 的 handler 不判方法）；body 补 `json.Encoder` 的末尾换行。黄金场景 `health-methods` + `http_contract::health_answers_any_method` |
| 8 | `tests/p1_golden.rs` | 删掉 `stats_seed` 死分支；上游命中数由 `>` 改成**精确等值**（并单列 single-flight 的批量命中数 `batch_upstream`、keeps-stale 的预热 `warmup_upstream_mode`，避免把脚手架流量算进被测步骤）；新增 `resp_headers` 逐步比对。5xx 重试次数由黄金门禁的精确命中数断言直接锁死（第 5 轮重采后 502 场景记为 3 次） |
| 9 | `tests/golden/`（新目录，入库） | 把 `fixtures.json`、`golden-adapt-go.json`、`golden-p1-go.json`、`golden-compact-go.json` 复制进仓库；`rs_bai::golden_path()` 与三个测试改为「先入库副本、缺则回落 `.work/rust-rewrite/`」。`.work/` 被清后不再整批转红；`http_contract::golden_files_exist` 守入库副本别被误删。**注：前两个（全量 65MB）已在第 4 轮移出仓库、换成 `*.small.json`，见下方第 4 轮表** |
| 10 | 本文件 | 更正「Go 的 `ServeMux` 会 301」这条**错误表述**（审查证伪：两侧都走 catch-all 透传，行为一致）；登记本轮各项修正与新增的 3 条差异（4MB 截断形状、`compact_json_go` 校验覆盖度、矩阵 GET 的重试口径——最后一条第 5 轮已更正为 3 次尝试） |

## 验证

```powershell
cargo test                       # 20 单测 + P0 黄金 1 + HTTP 合同 10（+1 ignored）+ P1 黄金 2
cargo test --test p1_golden      # P1：矩阵/统计 17 场景 / 42 步 逐字节对照 + 跨语言 DPAPI 往返
cargo test --test golden_adapt   # P0 请求改写黄金门禁
cargo test --test http_contract large_body    # >2MB 体量回归
cargo test --test http_contract slow_stream -- --ignored   # 长流不断流验证（约 10 分钟）
```

**黄金基准**在 `rs-bai/tests/golden/`（见该目录 README）。入库的是**小号基准**
（`golden-adapt-go.small.json` / `fixtures.small.json`，常规用例 F1~F6、F9~F12）
与 `golden-p1-go.json`、`golden-compact-go.json`；全量的 `fixtures.json` /
`golden-adapt-go.json`（65MB 工作树体积，第 4 轮移出仓库）只留在 `.work/rust-rewrite/` 下。
加载顺序是**先入库副本、缺则回落 `.work/rust-rewrite/`**——`.work/` 是 gitignore 的
临时目录，清掉它之后 `cargo test` 仍应全绿：必需的那几份缺了**失败**；只有全量基准里的
F7/F8 两条超大用例允许缺失，且缺失时 `golden_adapt` **逐条打 `SKIP(oversized): …`
并在汇总行打印跳过条数**（`cargo test --test golden_adapt -- --nocapture` 可见），不静默。

## 第 4 轮（黄金基准入库改口径）改了哪几处

只改入库方式，不动任何对齐行为：

| # | 位置 | 改动 |
|---|---|---|
| 1 | `tests/golden/` | `fixtures.json`(27.7MB) 与 `golden-adapt-go.json`(37.8MB) 移出仓库（`.work/rust-rewrite/` 下原样保留）；新增小号基准 `fixtures.small.json` / `golden-adapt-go.small.json`（常规用例 10 条，合计 9KB） |
| 2 | `tests/golden_adapt.rs` | 小号基准必跑（缺了失败）；F7/F8 两条超大用例改成「全量基准在则跑、缺则逐条打 `SKIP(oversized): …` + 汇总打印跳过条数」，不静默 |
| 3 | `tests/http_contract.rs`（`golden_files_exist`） | 守的入库文件清单改成小号那一套（`*.small.json` + `golden-p1-go.json` + `golden-compact-go.json`） |
| 4 | `src/lib.rs`（`golden_path` 文档） | 写明两档口径：必需的缺了失败、只有超大用例那一档可显式跳过 |
| 5 | `tests/golden/README.md` | 写清入库/不入库两档、跳过行为与再生成路径（`mkfixtures.mjs` → `go-adapter` → `mksmall.mjs`） |

`tests/p1_golden.rs` 的基准由 Go 侧（`internal/bai` 包内一次性采集器：**真**
`Server.Start()` + 假上游 + 临时渠道配置）采集：每步记「下游请求 + 当时的上游形态 →
逐字节响应 + 响应头 + 上游命中数」，统计场景还逐步记服务端累计表。Rust 侧用**同一批输入**
回放，逐字节比响应体（只归一化 `fetched_at` / `uptimeSec` 这两个必然不同的数字），
并按**精确增量**核对上游命中数（多一次 = 重试口径与 Go 不同，少一次 = 漏打）。

`reads_go_dpapi_ciphertext` 是跨语言往返：`.work/cli/dpapiwrite.exe`（Go `appdir.WriteFile`）
现产一份**真 DPAPI 密文**夹具（哨兵 key，不是真 key），Rust 侧解开并取到 `id=="bai"` 的 key；
同时验明文读兼容与坏密文回空串。缺夹具工具时测试**失败**而不是跳过。
前置：`go build -o .work/cli/dpapiwrite.exe ./.work/cli/dpapiwrite`

`tests/http_contract.rs` 里两条体量/时长相关的用例值得单独说：
`large_body_reaches_upstream`（3.6MB 请求体，秒级）默认就跑——它守着"32MB 上限别被改回 2MB"；
`slow_stream_is_not_cut_by_total_timeout`（读完 600s 后仍在收帧，约 10 分钟）默认 `#[ignore]`，
守着"别再加回总超时"，改动 `src/transport.rs` 的出站客户端后建议手动跑一次。
注意后者**测试客户端自己不设总超时**：给客户端设时限会让"本 crate 有没有时限"被测不出来。

## 第 5 轮（审查驳回项修正）改了哪几处

`code-reviewer` 驳回第 3 轮第 4 条（「Go 矩阵 GET 一次也不重试」方向搞反）。本轮只做驳回项
与审查点名的低危项，不做无关重构：

| # | 位置 | 改动 |
|---|---|---|
| 1（R1/R2，高） | `src/transport.rs`（`RetryPolicy::matrix_default`） | `max_attempts` 由 **1 改回 3**，注释重写为正确依据（`models.go:119` 传 nil body → `net/http/request.go:906-923` 让 `req.Body==nil` 且跳过整段 `if body != nil` → `GetBody==nil`；`server.go:524` 的门槛 `req.Body != nil && req.GetBody == nil` **不成立** ⇒ 走重试循环） |
| 2（R1/R2，高） | `tests/golden/golden-p1-go.json` + `.work/rust-rewrite/golden-p1-go.json` | **重采**：采集器保留生产重试层（`&retryRoundTripper{..., max: 3}`），两个 502 场景的上游命中数 **1 → 3**；同步修掉与数据矛盾的 desc（case 级 1 处 + 该场景内的步级 2 处）。重采前后逐字段对比：除易变字段（`date` / `fetched_at`）外，实质差异只有这 6 处 |
| 3（R3，中） | `src/transport.rs`（`with_proxy` / `fetch_matrix_once`） | 15s 预算改成**真正全程覆盖**：`matrix_client` 补 30s 连接超时（同 Go `net.Dialer`），`req.send()` 也套在剩余预算里（超时归 fatal，不再开新尝试）。新增单测 `matrix_deadline_covers_waiting_for_response_headers`（上游 accept 后永不应答，2s 预算内必须报错） |
| 4（R4） | `src/matrix.rs`、本文件 | 「Go 1.99」全部改成实际版本 **Go 1.26.5**（1.99 是 rustc 工具链版本，此前张冠李戴） |
| 5（R5） | `src/lib.rs` + `tests/p1_golden.rs` | 405 补体中间件由 `pub` 化后**测试直接复用生产那一份**；此前测试里的自造副本比生产多设了 `nosniff`，注释却写「照抄生产实现」 |
| 6（R6） | `src/matrix.rs`（`channel_key`） | 渠道配置数组含 `null` 元素时改为**逐条容忍**（`Vec<Option<ChannelKeyCfg>>` + `flatten`）：Go 把 `null` 存成零值条目不报错，serde 默认会整份拒绝。期望值由包内一次性探针在真 `baiChannelKey()` 上实测（`[null,{"id":"bai","key":"…"}]` → 取到 key；`[{"id":"bai",…},{"id":123}]` → 整份失败空串） |
| 7（R7） | `.work/cli/recompact/main.go` + 两份 `golden-compact-go.json` | 样本 `real-newline-in-string` 的 desc 与记录值 `out:""` 自相矛盾，改成「Go 的 scanner 在字符串态校验控制字符 → 编码整体失败」；用工具重采，除该 desc 外逐字节不变 |
| 8（R8） | 本文件 差异 7 | 补一句内存形状：Go `LimitReader` 边读边弃（峰值约 4MB），Rust 先整段读入再判超限 |
| 9（R9） | 本文件 第 3 轮表 #8 | 删掉与现实相反的那半句（「5xx 重试次数不在黄金门禁里」——重采后黄金门禁的精确命中数断言正是锁它的地方） |

## 密钥卫生（本 crate 的硬性纪律）

- 渠道配置里的 Bearer **只**在内存里用于两处：`/model/matrix` 拉上游清单、以及客户端自带
  `Authorization` 的透传请求。本 crate **不写任何配置文件**。
- 日志只报「检测到代理」「矩阵已刷新 N 个模型」「刷新失败（原因）」这类事实，
  **不打印 key、不打印代理变量值**（对齐 Go `server.go:202`）。
- `RS_BAI_CAPTURE` 出站校验记录里 `authorization` 一律写成 `Bearer <redacted>`。
- 统计文件只含模型名与 token 计数，**不含任何凭据**。
