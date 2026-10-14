# 黄金基准（rs-vibex）

`golden-vibex-go.json` —— P4 的**唯一**对齐基准，由 Go 侧临时采集器
`internal/vibex/zz_vibex_golden_collect_test.go` 真实跑出来（采集器跑完即删，留档
`.work/rust-rewrite/collect-vibex_test.go`）。

## 这份基准里有什么

| 段 | 内容 |
|---|---|
| `meta` | 采集时刻、假凭据（`fake_tokens` A/B/C）、UA/初始 cwd/默认 base_url、**归一化规则清单** |
| `pure` | 19 组纯函数输入输出对（配置 normalize、JWT、daysLeft、str/truthy、appsFrom、initRootMeta、buildPrompt、claudeEvents、oaiUsage、foldSync、unauthErr、wsURL…） |
| `http` | 19 个场景、共 60 步：每步记 **下游响应**（状态/契约头/体）+ **上游观测**（方法/路径/契约头/体）+ **WS 帧序与握手头** + **上游命中计数** |
| `cdp` | 4 个自动探测场景（cookie 命中 / localStorage 兜底 / 都没有 / 探活 401），附 CDP 调用序与拉起次数 |
| `floats` | Go `json.Marshal(float64)` 的写出样本 |

**基准里没有任何真实凭据**：token 全部是合成串（payload 带 `"fake":true`，签名段是字面量
`FAKESIG`）。上游是采集器进程内起的**假 VibeX**（REST + 手写 WS 握手），全程不出网、
不占任何现行端口。

## 怎么重采

```powershell
# 0) 前提：Go 工具链在位；采集器留档在 .work/rust-rewrite/collect-vibex_test.go
Copy-Item .work\rust-rewrite\collect-vibex_test.go internal\vibex\zz_vibex_golden_collect_test.go

# 1) 采集（写进 .work/）
$env:VIBEX_GOLDEN_OUT = "$PWD\.work\rust-rewrite\golden-vibex-go.json"
go test ./internal/vibex -run TestZZCollectVibexGolden -count=1 -timeout 900s

# 2) 采集器跑完立即删除（同包 _test.go 只是临时脚手架）
Remove-Item internal\vibex\zz_vibex_golden_collect_test.go

# 3) 入库（测试优先读 rs-vibex/tests/golden/，缺则回落 .work/rust-rewrite/）
Copy-Item .work\rust-rewrite\golden-vibex-go.json rs-vibex\tests\golden\golden-vibex-go.json
```

采集完必须复核：`git status --short internal/` 空、`go test ./internal/vibex -count=1` 与
`go vet ./internal/vibex` 干净。

## 归一化规则（断言侧按 `meta.normalize` 执行）

| 规则 | 为什么 |
|---|---|
| `http://127.0.0.1:<port>` → `<PORT>` | 假上游是随机端口；`Origin`/`Referer` 里带着它 |
| `"created":<数字>` → `<CREATED>` | 墙钟 |
| `"id":"chatcmpl-<hex>"` → `<ID>` | 每请求随机 |
| `"uptimeSec":<数字>` → `<UPTIME>` | 进程存活时长 |
| `"days_left":<int>` / `"days_left_max":<int>` → `<DAYS>` | 随墙钟漂移；**公式**另由 `pure.days_left` 逐条锁死 |

其余**逐字节比对**。

## 两档 / SKIP 口径

本基准**只有一档**：全量入库（262 KB < 1 MB），**没有需要跳过的超大用例**，因此
`cargo test` 的汇总行里「显式跳过 0 组」是常量而不是省略。若将来要加超大场景，
按 P1/P3 的口径办：超大档留在 `.work/`，缺失时**显式打印 `SKIP(...)` 并计数**，不静默跳过。

⚠ 与 P1/P3 相同的可见性边界：汇总行（含跳过/允许差异计数）走 `println!`，
**裸 `cargo test` 看不到**，要看带 `-- --nocapture`。

## 与 Go 基准的差异（README 主文档「已知差异」节是权威）

* 浮点最短表示在极大值上的取整方向差异：断言侧要求「两串回读相差 ≤ 1 ULP 且
  `|v| >= 1e17`」才放行，并单独计数打印（纯函数段的汇总行）。
* 非 ASCII 上游头值（`username: 小明`）：**已收口，无 allowance**。客户端（Go 与 Rust）
  都原样发出；假上游（axum/hyper 服务端）也**保留**原始字节——早先观察不到是测试脚手架
  `pick_headers` 用 `HeaderValue::to_str()` 有损读取所致（非 ASCII 必然 `Err` → 空串），
  已改为按字节无损读取，门禁对这类头与其余头一样**逐字节比对**。根因与实测见
  `rs-vibex/README.md` 差异 6。
