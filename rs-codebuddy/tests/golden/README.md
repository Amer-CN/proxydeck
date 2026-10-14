# rs-codebuddy 黄金基准（P5a：合同与请求侧）

本目录只放**入库**的黄金基准。当前一档：

| 文件 | 体积 | 内容 |
|---|---|---|
| `golden-codebuddy-go.json` | 118 KB | 12 个 HTTP 场景 / 56 步 + 纯函数四段（tool_choice 15 / tool_refs 17 / fingerprint 4 / summarize_tool 6） |

同源副本另有一份在 `.work/rust-rewrite/golden-codebuddy-go.json`（gitignore，不进库）。
测试的加载顺序是**先本目录、缺则回落 `.work/rust-rewrite/`**（见 `tests/common/mod.rs::golden_path`）；
本目录被删时 `http_contract::golden_files_exist` 会判红，不会静默少覆盖。

## 这份基准是怎么采的

1. 临时 Go 采集器（同包 `_test.go`，跑完立即删除）放到 `internal/codebuddy/`：
   `.work/rust-rewrite/collect-codebuddy-a_test.go` 就是它的留档。
2. 跑：

   ```powershell
   cd F:\AIXM\proxydeck
   Copy-Item .work\rust-rewrite\collect-codebuddy-a_test.go internal\codebuddy\zz_codebuddy_golden_collect_test.go
   $env:CODEBUDDY_GOLDEN_OUT="F:\AIXM\proxydeck\.work\rust-rewrite\golden-codebuddy-go.json"
   go test ./internal/codebuddy -run TestZZCollectCodebuddyGolden -count=1 -v
   Remove-Item internal\codebuddy\zz_codebuddy_golden_collect_test.go     # 跑完立即删
   Copy-Item .work\rust-rewrite\golden-codebuddy-go.json rs-codebuddy\tests\golden\golden-codebuddy-go.json
   ```

3. 采集纪律（写死在采集器里，改动前先读它）：
   * 上游是**进程内假上游**（自写 `RoundTripper`，应答规格记进黄金的 `upstream_spec`）；
     不出网、不占任何现行端口；
   * 登录态是 `CODEBUDDY_AUTH_DIR` 指向的临时目录里的**合成假文件**（token 形如 `tok-<uid>`），
     真实 `workbuddy-desktop*.info` 一个字节都不读；
   * 数据目录（`diag-11133-*.json` 落点）由 `PROXYDECK_DATA_DIR` 指向临时目录；
   * 服务经生产 `s.Start()` 起在 `127.0.0.1:0`，走的就是生产的 `corsWith`+`ServeMux` 路由表。

## 入库前做过的一致性核对

同一采集器连跑两次，把两份产物里**易变字段**（`collected_at`、响应头 `date`、
诊断文件名里的纳秒戳）归一化后逐字段比对，除这些以外每个测量值都被原样复现
（比对脚本口径见 `.work/rust-rewrite/_diffcodebuddy.mjs`，本目录不含脚本）。

## 门禁怎么用这份基准（三条口径）

1. **采集机字面量本地化**：临时目录在黄金里写成 `__AUTHDIR__` / `__DATA__`（占位符刻意不含
   `<`/`>`/`&`——Go 的 `json.Marshal` 会把它们转成 `\u003c` 之类，占位符一进 JSON 就和
   两侧的转义规则打架）。Rust 侧不再做任何路径替换：凭据缝的值直接取自黄金，两侧字符串天然相同。
2. **显式跳过、不静默**：两类跳过硬编码在测试里并**打印 + 计数**——
   * `SKIP(deferred-p5bc)`：属 P5b/P5c 的端点（`/quota`、`/v1/failover`、`/v1/accounts`、`/v1/stats`），
     本轮只在黄金里登记 Go 的实际应答；
   * `SKIP(downstream-p5c)`：客户端没要 `stream` 的 200 响应（Go 会把上游 SSE **聚合**成
     `chat.completion`），本轮只比出站请求与状态码。
   ⚠ **裸 `cargo test` 看不到这些行与汇总**（libtest 默认捕获通过用例的 stdout）——
   要看带 `-- --nocapture`。汇总行形如：

   ```
   HTTP 场景黄金对比：12 个场景 / 56 步逐字节一致；显式跳过 4 步（deferred-p5bc）；已登记 allowance 1 处（content-length）
   ```

3. **allowance 只有一处**：`HEAD /health` 的 `content-length`。黄金里那个数字是「本地化之前」
   的体长（占位符改变了体长），而 HEAD 没有体可以重算——其余 55 步的 `content-length`
   已在采集侧随本地化重算、按实测逐字节比对。HEAD 的自洽另有一条正面证据：
   `tests/http_contract.rs::head_content_length_matches_get_body`。

## 反向验证（门禁非空转）

`.work/rust-rewrite/_mutation-check-codebuddy.ps1` 逐条把实现改坏，6 条全部让门禁变红：

| # | 改坏什么 | 结果 |
|---|---|---|
| M1 | `chat.rs` 的 `TRANSIENT_MAX_RETRY` 2 → 0（不重试） | 红（上游命中数 1 ≠ 3） |
| M2 | `toolrefs.rs` 不再删用过的 `$defs` | 红（`pure.tool_refs` 逐字节不符） |
| M3 | `models.rs` 的探测 TTL 缓存恒不命中 | 红（上游命中数翻倍） |
| M4 | `diag.rs` 的现场上限 10 → 11 | 红（现场文件份数不符） |
| M5 | `gjson.rs` 不转义 `<` | 红（health 体逐字节不符） |
| M6 | `godecode.rs` 顶层 `null` 改成报错 | 红（`null` 体应回 `messages is required`） |

跑完还原并复跑全量（脚本自己会做，且会把 mtime 顶到当前时间——否则 cargo 可能认为没变、
继续用改坏的二进制，P3 踩过这个坑）。
