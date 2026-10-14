# rs-codebuddy —— CodeBuddy/WorkBuddy 插件（8787 CN + 8789 INTL）的 Rust 重写

> **P5a：合同与请求侧**（绞杀者路线第 5 阶段的第一小轮）。等价替换 `internal/codebuddy`
> 的**请求/合同面**，先在测试端口独立跑通，**不注册进 `app/plugins.go`**、不动 Go 构建、
> 不碰任何现行服务端口（尤其 8787/8789 上跑着的 Go 版）。
> 账号池/凭据/额度/failover 属 **P5b**，脱敏/SSE cleaner/统计属 **P5c**（交接清单见文末）。

## 一、这一轮做了什么（与 Go 的对应）

| # | 合同 | Go 位置（`internal/codebuddy/`） | Rust 位置 |
|---|---|---|---|
| 1 | 路由表 + CORS（OPTIONS 直接 204） | `server.go:80-91` `corsWith`、`:166-190` `Start` | `src/lib.rs` `router`/`cors`/`add_cors` |
| 2 | `/health`（不看方法；status/service/backend/region/mode/auth_search_dirs/uptimeSec + credential 块或 status=error + message） | `server.go:201-228` | `src/lib.rs` `handle_health` |
| 3 | 候选池 15 条（CN）/ 15 条（INTL） | `server.go:605-612`、`region.go:79-84` | `src/region.rs` `MODEL_CANDIDATES`/`INTL_CANDIDATES` |
| 4 | `/v1/models`：每小时在线探测过滤（TTL 1h）+ 按池序输出 + 全失败回退整池；INTL `NoProbe` 零上游 | `server.go:616-672` | `src/models.rs` `models` |
| 5 | 探测本身：最小流式请求、25s、并发 10、"读到数据即可用"、INTL 强制首条 system | `server.go:676-706` | `src/models.rs` `probe_one`/`probe_all` |
| 6 | `/model/info`：`modelMeta` **只填实测值**，未命中不硬造字段、`note` 缺省补空串；INTL 空 meta | `server.go:737-783` | `src/models.rs` `meta_entries`/`handle_model_info` |
| 7 | `/v1/chat/completions` 入站解析（64MB 截断读、Go `json.Unmarshal` 逐字错误文案、`messages is required`、model 缺省 auto、stream 判定） | `server.go:1128-1149` | `src/chat.rs` `handle_chat`、`src/godecode.rs` |
| 8 | 出站体重建：20 个白名单字段 + `stream=true` + `stream_options` 缺省 + 键排序/HTML 转义的 `json.Marshal` 等价物 | `server.go:788-794, 1151-1164`、`mustJSONReader` | `src/chat.rs`、`src/gjson.rs` |
| 9 | `normalizeToolChoice`（对象式降级、无 tools 删除、怪值原样） | `server.go:912-931` | `src/toolrefs.rs` `normalize_tool_choice` |
| 10 | `sanitizeToolRefs`/`inlineSchemaRefs`（`$ref`/`$defs` 无损内联、兄弟键 2020-12 合并、循环截断、外部引用保留、无 `$ref` 原样、零残留） | `server.go:941-1015` | `src/toolrefs.rs` `sanitize_tool_refs`/`inline_schema_refs` |
| 11 | `developer` → `system`；INTL 自动补首条 system | `server.go:880-901, 1171-1191` | `src/toolrefs.rs` `normalize_roles`/`msgs_have_system`、`src/chat.rs` |
| 12 | 11133 退避重试（上限 2 次，800ms/1600ms）+ 每次落一份现场（上限 10 份、0600） | `server.go:562-573, 1316-1342`、`dumpRequestBody` `:1112-1126` | `src/chat.rs`、`src/diag.rs` `dump_request_body` |
| 13 | 11133 的 `requestFingerprint`（不含正文的可 grep 指纹，含 sha256 前 6 字节） | `server.go:1073-1108` | `src/diag.rs` `request_fingerprint` |
| 14 | 11129 的 `summarizeToolsForDiag`（只取出站 tools 与 tool_choice） | `server.go:1020-1068` | `src/diag.rs` `summarize_tools_for_diag` |
| 15 | 上游非 200 的 `writeErr` 形状（状态码 + `error.message` + `type=upstream_error` + 末尾换行） | `server.go:1770-1776` | `src/chat.rs` `write_err` |
| 16 | `ServeMux` 兜底 404 文案（`404 page not found\n` + nosniff） | Go 标准库 | `src/lib.rs` `handle_fallback` |
| 17 | 区域差异：base_url / 候选池 / 强制 system / 探测开关 / UA 常量 / 凭据过滤后缀常量 | `region.go` 全文 | `src/region.rs` |
| 18 | 数据目录判定链（`PROXYDECK_DATA_DIR` → 便携标记 → `%APPDATA%\ProxyDeck`） | `internal/appdir/appdir.go` | `src/datadir.rs` |

## 二、怎么跑

```powershell
cd rs-codebuddy
$env:CARGO_TARGET_DIR="E:\cargo-target\rs-codebuddy"   # F: 盘紧张时的外置目标目录
cargo test                        # 24 个测试：15 单测 + 2 黄金 + 7 合同
cargo test -- --nocapture         # 看「跳过/allowance」汇总行

# 起一个测试实例（**别用 8787/8789**）
cargo run -- --port 18786 --region INTL
# 或者按黄金门禁的活体对照脚本跑：
pwsh -NoProfile -File ..\.work\rust-rewrite\_live-compare-codebuddy.ps1
```

参数：`--plugin-codebuddy`（占位，与 Go 侧同名）、`--port`、`--host`、`--region CN|INTL`、
`--upstream-base`（对照/回放用）、`--cred-stub <json>`（**测试缝**，见第四节）、`--diag-dir`（对照用）。
`--desensitize` **传了就拒绝启动**（属 P5c，不静默忽略）。

## 三、门禁与证据

* **黄金门禁** `tests/golden_codebuddy.rs`：纯函数 42 条 + HTTP 12 场景 / 56 步逐字节对照；
  基准与再生成链见 `tests/golden/README.md`。
* **HTTP 合同** `tests/http_contract.rs`：路由边界（404/405/CORS/尾斜杠）、P5b/P5c 端点的 501、
  HEAD 的 `content-length` 自洽、**>2MB 请求体必须逐字节到上游**（防退回 axum 的 2MB 默认上限）、
  未接入凭据时 `/health` 如实报错、`--desensitize` 拒绝启动。
* **反向验证**：`.work/rust-rewrite/_mutation-check-codebuddy.ps1` 六条突变全部让门禁变红。
* **活体对照**：`.work/rust-rewrite/_live-compare-codebuddy.ps1` 把真进程起在 127.0.0.1:18786
  （INTL），与黄金（Go 侧经真实 HTTP 栈采收）比 `/v1/models`、`/model/info`、`/health`、
  404/405/OPTIONS：**7 条一致 / 0 条不一致**（零出网、零现行端口）。

## 四、凭据缝（P5a 的边界，P5b 的接口）

`/health` 的 `credential`、`/v1/models` 的探测头、`/v1/chat/completions` 的出站头，在 Go 里都来自
凭据层（`Credential.Summary()` / `Credential.Headers(ctx)` / `findAuthDirs()`）。凭据文件格式、
at-rest 解密（DPAPI）、region 过滤、token 刷新属 P5b，本轮**不碰真实登录态**，只定义缝：

```rust
pub trait CredSource {                       // src/cred.rs
    fn auth_dirs(&self) -> Vec<String>;      // Go findAuthDirs()
    fn summary(&self) -> Option<CredSummary>; // Go Credential.Summary()
    fn headers(&self) -> Result<HeaderMap, String>; // Go Credential.Headers(ctx)
}
```

* 生产默认实现 `PendingCredential`：`/health` 回 `status=error` 并写明「凭据层尚未接入（P5b）」；
  `/v1/models` 走 Go 在「headers 取不到」时的回退分支（整池候选）；chat 回 502。**不是假装成功。**
* 测试缝 `StubCredential`（`--cred-stub <json>`）：活体对照与黄金回放用它喂固定值。
* **P5b 落地时要做的**：用真实凭据层替换这两者，并在 `buildHeaders` 里用上 `region.rs` 的
  `user_agent` / `domain_suffixes`（本轮只登记常量，注入点不在本轮）。

## 五、已知差异（逐条，数字编号供引用）

1. **下游 200 响应本轮是最小透传**（**本小轮最大的不等价点**）：流式按上游原字节转发（Go 会过
   `sseFieldCleaner` 剔掉空字符串键）、非流式原样转发上游体（Go 会用 `collectStream` 把 SSE 聚合成
   `chat.completion`）。→ P5c。黄金门禁对这些步打 `SKIP(downstream-p5c)` 并计数，只比出站请求与状态码。
2. **TLS 指纹**：Rust 无 uTLS 对等物（沿用 P0 起的既有登记口径）。
3. **头序/头名大小写/默认头**：hyper 发出的头名是小写（Go 会规范化成 `X-User-Id`）；reqwest 不会
   自动补 Go `net/http` 那些默认头。**值**逐字节一致（门禁比的是值，头名大小写不敏感）。
4. **脱敏未实现（且生产默认开着）**：`app/plugins.go` 对 CN 与 INTL **都**传 `--desensitize`，
   所以本轮 Rust 实例**不能当生产用**——`--desensitize` 会拒绝启动（不静默忽略），
   而黄金里的 chat 场景是**未脱敏形态**（采集器用的是零值 Server，`desensitize=false`）。
   `DesensitizeBody` 与指纹清洗表属 P5c；验收 2 的「出站体逐字节一致」在 P5a 只对
   **未脱敏配置**成立。
5. **账号池的三条换号分支不参与**（401/403 标死换号、429 配额冷却换号、14018 额度耗尽换号）：
   Go 侧全部包在 `s.pool != nil` 里，零值 Server（无池）同样不走；本轮无池 ⇒ 形状一致，
   但**有池时的行为未经对照**。→ P5b。
6. **hy4 限流兜底不参与**：`failoverDecide` 的改写/trip 依赖 failover 配置（P5b）；Go 默认配置下
   `hy4-preview` 撞 429 会切 fallback，本轮不会。`/health` 的 `hy4_limit` 键在无窗口时两侧都不写。
7. **CN 的缺省 `reasoning_effort` 注入未实现**：生产 Go 在 CN、客户端未带该字段、且模型矩阵命中
   配置档位时注入（缺省配置 = `max`，`server.go:348`+`:1231-1239`）。本轮不注入。
   **黄金基准也没覆盖这条**——采集时用的是零值 Server（`default_reasoning` 为空 ⇒ 不注入），
   与 Go 的「文件缺失 ⇒ 缺省 max」不同。这条属 P5b（档位来自 `codebuddy-failover.json`）。
8. **诊断目录的解析时机**：Go 每次落盘现读 `appdir.DataDir()`；Rust 启动时解析一次
   （`--diag-dir` 可覆盖）。差异只在运行中改环境变量时可见。
9. **探测的并发与超时形状**：并发上限（10）与单条超时（25s）同 Go，但 Go 是一个 `context` 罩住
   整次请求，Rust 用两个 `timeout` 分别罩「发送」与「首个数据块」；上游命中的**先后顺序**两侧不同
   （门禁对探测场景按 body 排序比对）。
10. **`probeOne` 的非 EOF 读错误视为「可用」**：这是 Go 的原逻辑（只把 `EOF` 当失败），Rust 逐字照抄。
11. **`HEAD /health` 的 `content-length` 未与 Go 比对**：黄金里那个数字是占位符本地化**之前**的体长；
    唯一的一处 allowance。HEAD 的自洽另有一条正面证据（见第三节）。
12. **未覆盖**：路径清洗形态（`//v1/models`、`../`）——`ServeMux` 与 axum 对这类请求的处理未对照；
    64MB 截断边界；`--cred-stub` 是测试缝，不是产品参数。

## 六、P5b / P5c 必须接手的清单

**P5b（账号池与凭据）**
1. 实现 `CredSource` 的真实版：`FindAuthFileForRegion`（按 `auth.domain` 过滤 + mtime 最新 +
   可用性判定）、`wbToken` 的 `$wbEncrypted` 解密（`atrest*.go`，DPAPI）、`buildHeaders`
   （Content-Type/Accept/Authorization/`X-User-Id`/`X-Enterprise-Id`/`X-Tenant-Id`/`X-Domain`/UA）。
2. `pool.go`：扫描入池（同 uid 去重留 mtime 最新）、停用/移除/恢复落盘 `codebuddy-pool(-intl).json`、
   Dead/冷却/调用计数（进程内存态）、`Pick`/`Touch`/`IncLoad`/`DecLoad`/`MarkDead`/`MarkLimited`/`MarkExhausted`。
3. chat 的三条换号分支 + 200 体里的 14018 Peek 分支（上限 = 池大小）。
4. `failover`：`loadFailover`/`saveFailover`（`codebuddy-failover.json`）、`/v1/failover` GET/POST、
   `hy4Limited`/`hy4Trip`、`parseQuotaReset` 的 CN/INTL 关键字族、`/health` 的 `hy4_limit`。
5. **CN 缺省 `reasoning_effort` 注入**（差异 7），以及 `reasoningDefaultFor` 的档位判定。
6. `quota.go`：`/quota`（`?uid=`、按区+号分键的 5 分钟缓存、401 凭据失效展示）。
7. `/v1/accounts`（GET 快照 / POST rescan|clear_cool|clear_exhausted|toggle|remove|restore）。
8. 交接时把上面两处 **501 占位**换成真实现，并在黄金里放开对应的 `deferred` 步。

**P5c（出站与下游）**
1. `desensitize.go` 全角色脱敏（`fingerprintRules`、黑名单模板句的括号注解删除）与 `--desensitize` 放行。
2. `sseFieldCleaner`（剔空字符串键）+ `usageScanner` 的 `data:` 行 usage 提取。
3. `collectStream`（非流式聚合：content 拼接、tool_calls 槽位按 index 合并、`finish_reason`
   缺省规则、`chatcmpl-<6 字节 hex>` id、`finish_reason_debug` 字段）。
4. `codebuddy-stats.json` 的按模型累计 + 节流落盘 + `/v1/stats`。
5. 11128/11129 的日志诊断（`summarizeToolsForDiag` 已在 `src/diag.rs`，接线即可）。

## 七、采集器与临时文件登记

* `internal/codebuddy/zz_codebuddy_golden_collect_test.go`——P5a 黄金采集器，**跑完已删除**，
  原样留档 `.work/rust-rewrite/collect-codebuddy-a_test.go`。删除后核验：
  `git status --short internal/` 空、`go test ./internal/codebuddy -count=1` 与
  `go vet ./internal/codebuddy` 干净（Go 包回到未污染状态）。
* `.work/rust-rewrite/` 下：`golden-codebuddy-go.json`（与入库副本同源）、`_live-compare-codebuddy.ps1`、
  `_mutation-check-codebuddy.ps1`（+ `.log`）、`_live-stub-intl.json`、`_diffcodebuddy.mjs`。
* 本 crate 未改任何 Go 源码、未动版本串 8 处 / `CHANGELOG.md` / 根 `README.md` / `*.exe` / tag。
