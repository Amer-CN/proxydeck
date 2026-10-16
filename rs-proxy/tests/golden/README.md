# P6 黄金基准（`golden-proxy-go.json`）——再采集链、归一化口径、踩过的坑

> 本目录的基准是 `rs-proxy` 门禁的**唯一测量来源**。它由**真 Go**（`internal/proxy` +
> `internal/api` + `internal/server`）采收，不是手写的期望值。

## 现状

| 项 | 值 |
|---|---|
| 文件 | `golden-proxy-go.json` |
| 体积 | 3,864,683 B（≈3.7 MB） |
| SHA-256 | `7AD9A99D09B5A6278D0A9378AB917305A67E7D20BB6BF3BE6F073EE7E36AF03D` |
| 采集时刻 | `2026-10-16T15:33:05+08:00`（`meta.collected_at`） |
| 场景 / 步数 | **52 个场景 / 125 步** |
| `pure` 段 | 别名表 92 条、目录 61 条、转换/提取/编码/金额等 14 组 |
| 逐字段可复现性 | 同一采集器**连跑三次**逐字段比对，除 `meta.collected_at` 外差异 **0 处** |

> 体积里 3.6 MB 是**一条**超长用例（`cc-pipeline #24`：1.8M 字符的 messages）——
> 采集侧已展开 `[BIG:…]` 规格串，出站体因此是真实长度（见「坑 3」）。
> 其余 51 个场景合计约 260 KB。

## 再生成链（三步）

```powershell
# 0) 采集器留档件：.work/rust-rewrite/collect-proxy_test.go
#    （它是 `internal/proxy` 的**同包 `_test.go`**，要指到未导出的缝）
Copy-Item .work/rust-rewrite/collect-proxy_test.go internal/proxy/zz_p6_collect_test.go -Force

# 1) 采收（上游是进程内 httptest 假上游；不出网、不占 55990；数据目录走 PROXYDECK_DATA_DIR 临时目录）
$env:P6_OUT="F:\AIXM\proxydeck\.work\p6\golden-proxy-go.json"
go test ./internal/proxy -run TestZZP6CollectGolden -count=1 -v

# 2) 立即删除临时采集器（纪律：`internal/` 必须回到干净状态）
Remove-Item internal/proxy/zz_p6_collect_test.go -Force
git status --short internal/          # 必须为空

# 3) 入库副本与 .work 副本必须同源（哈希一致）
Copy-Item .work/p6/golden-proxy-go.json rs-proxy/tests/golden/golden-proxy-go.json -Force
Get-FileHash .work/p6/golden-proxy-go.json,rs-proxy/tests/golden/golden-proxy-go.json -Algorithm SHA256
```

采集后必须核对（缺一不可）：

```powershell
go test ./internal/proxy -count=1     # ok
go vet ./internal/proxy               # 退出码 0
git status --short internal/          # 空
```

## 归一口径（**采集器与门禁同源**）

易变字段在**采集时**就换成占位符，基准因此跨天稳定；门禁侧用同一套规则把实得值换成同一批
占位符再比。两侧规则必须一致，改一侧就要改另一侧：

| 占位符 | 规则（采集器 `p6Placeholders` / 门禁 `norm_volatile`） |
|---|---|
| `<DATE>` | `config.date` = `time.Now().Format("2006-01-02")` |
| `<THREADID>` | `threadId` = uuid v4 |
| `<GEN_ID>` | `chatcmpl-`/`msg_`/`resp_`/`fc_` 前缀的生成型 id、`call_id` |
| `<CREATED>` | `created` / `created_at` / `started` 的 10 位 unix 秒 |
| `<UPTIME>` | `/health` 的 `uptimeSec` |
| `<DAYSLEFT>` | `/v1/usage` 的 `daysLeft`（**每天递减**；由 `usage.rs::days_left_from` 现算。门禁侧规则在 `tests/common/mod.rs` 的 `replace_num_field`。⚠ 这条**只在门禁侧**（`norm_volatile`）实现——采集器侧未加同规则，故重采时黄金里仍是采集日原值，靠门禁双侧归一对齐；见 `tests/http_contract.rs::golden_clock_discipline_days_left_is_normalized` 的自检） |
| `<DAY>` | 统计里的本地日键 `"YYYY-MM-DD":{` |
| `<CCVER>` | `x-command-code-version` 头值（npm registry 在线查询，网络一变就变） |
| `<LEN>` / `<PORT>` / `<LOC>` | `Content-Length`、假上游随机端口、`Location` 里的 URL |

**不归一化**（属于测量值，必须逐字节比）：`cached`、`items` 的键序、`statsFile`、
`max_tokens`、`messages` 的 `null` vs `[]`……凡「行为差异会体现出来的字节」一律不放过。

## 基准必须自描述（否则门禁只能靠猜）

采集器把「测量脚手架用的是什么」也写进基准，门禁读它而不是自己猜：

| 字段 | 含义 | 缺省会怎样 |
|---|---|---|
| `proxy_key` | 该场景用的服务端默认 key（段 A `server-default-key`；段 B 三个 nokey 场景是空串；其余 `k`） | 三个「无 key → 401」场景会被比成 401 vs 200 的假差异 |
| `stats_file` | 该场景是否调过 `SetStatsFile`（**只有** `stats-and-calibration` 为 `true`） | `/v1/stats` 的 `statsFile` 会被比成 `"stats.json"` vs `"."` 的假差异 |
| `req_body`（步级） | 该步的请求体（段 D 的步骤没有 scenario 级 `probe`） | 段 D 的 11 步会全发空体（`#1` 得 400） |
| `upstream_spec` | 该场景假上游的状态/类型/体（段 D/G/H 也有自己的假上游） | 缺 `status` 会按 0 起服务 ⇒ 空体 200，测量值全失真 |
| `upstream`（步级） | 该步期望的上游命中列表（`null`/`[]`/`[...]`） | 把 `null` 当「本步不比」会漏掉真命中 |

## 踩过的坑（口径与后果）

1. **`internal/` 必须干净**：采集器是同包 `_test.go`，跑完立即删；留档件放
   `.work/rust-rewrite/collect-proxy_test.go`。`git status --short internal/` 为空才算收工。
2. **上游一律进程内假上游**：`httptest` + 自写 handler，不出网、不占 55990。
   数据目录用 `PROXYDECK_DATA_DIR` 指到 `t.TempDir()`。
3. **`[BIG:n:c]` 规格串必须在采集侧展开**（本轮修复的真缺陷）：早先 `p6Run` 把 13 字节的
   `[BIG:1800000:a]` 原样发给 Go，Go 的 `estTokens` 只数到 13 ⇒ 认为上下文很空 ⇒
   `max_tokens` 保持 `200000`；而门禁读基准时展开成 1.8M 字符再发，真 Go/Rust 的生产逻辑
   都会压到 `1048576 - 900064 - 0 = 148512`。**基准记了一个脚手架造成的假值**。
   现在 `p6Run` 先 `p6ExpandBig` 再发，出站体与基准里比的是同一串。
4. **`null` 切片不是空数组**（Go 语义）：`ConvertMessages` 一条都没产出时返回 **nil**，
   `json.Marshal` 给 `null`（黄金 `responses-convert #6`）；`[]` 只在非 nil 空切片时出现。
   Rust 侧因此要把 `messages` 建模成 `Option<Vec<…>>`。
5. **`filepath.Base("")` 是 `"."` 不是空串**：未设统计文件的 Proxy，`/v1/stats` 的
   `statsFile` 就是 `"."`（黄金 `route-boundaries #17`）。
6. **`/v1/usage` 的缓存形态少一个字节**：Go 先 `writeJSON`（带末尾换行）给客户端，
   再把 `json.Marshal`（**不带**换行）存进 `usageData`；命中缓存时直接写 `usageData`。
   ⇒ 首次响应 1298 字节、缓存响应 1297 字节（黄金 `usage-endpoints` 两步）。
7. **`X-Command-Code-Version` 走 npm registry 在线查询**：查询失败时 Go 回 `"unknown"`。
   采集前确认网络可用，否则基准里会留下 `unknown`（门禁会把 `<CCVER>` 归一化，
   但有值总比没值强——本轮为此重采过一次）。
8. **`items` 是 `json.RawMessage`，必须原文内嵌**：`/internal/usage` 的 `items` 若被重新编码，
   数组元素的键序会按字节序重排，与上游原文不一致。

## 门禁怎么用它

* `rs-proxy/tests/golden_proxy.rs`：**纯函数段**（逐字节比 14 组基准）+ **场景段**
  （起假上游 + 真 HTTP 打被测服务，比下游状态/头/体与上游命中的方法/路径/体/合同头）。
* `rs-proxy/tests/http_contract.rs`：不依赖测量值的硬合同（大体积体、404/405 三件套、
  `json.Encoder` 末尾换行、`/v1/usage` 缓存形态、基准存在性与形态守门）。

跑法（Huorong 会删刚链接出来的测试 exe，用绕行脚本）：

```powershell
pwsh -File .work/rust-rewrite/_p6test.ps1 -Targets lib
pwsh -File .work/rust-rewrite/_p6test.ps1 -Targets golden_proxy -TestArgs '--nocapture'
pwsh -File .work/rust-rewrite/_p6test.ps1 -Targets http_contract
```

> ⚠ 集成测试的 **CWD 是 crate 根**（`rs-proxy/`），所以 `tests/golden/` 与 `.work/` 都是相对它解析的。
