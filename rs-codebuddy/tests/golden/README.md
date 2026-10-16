# rs-codebuddy 黄金基准

本目录只放**入库**的黄金基准。当前一档：

| 文件 | 体积 | 内容 |
|---|---|---|
| `golden-codebuddy-go.json` | 298 KB（305,562 B） | **35 个 HTTP 场景 / 149 步**（P5a 12/56 + P5b 15/66 + P5c 8/27）+ 纯函数五段（tool_choice 15 / tool_refs 17 / fingerprint 4 / summarize_tool 6 / collect_eof 5） |

`scenarios[].scope` 分三段：

* `scope = "P5a"`：合同与请求侧，12 个场景 / 56 步（`tests/golden_codebuddy.rs`）；
* `scope = "P5b"`：账号池 / 凭据 / 额度 / failover，15 个场景 / 66 步（`tests/p5b_golden.rs`）；
* `scope = "P5c"`：脱敏 / SSE 清洗 / 非流式聚合 / 统计 / 路径清洗 / 积分失败措辞，
  8 个场景 / 27 步（`tests/p5c_golden.rs`）。

`pure.collect_eof`（5 例）是 P5c 审查后补修新增的**纯函数段**（2026-10-16）：`collectStream` 的
「EOF 残行」合同，由真 Go `collectStream` 采收，见 crate README 差异 42。

同源副本另有一份在 `.work/rust-rewrite/golden-codebuddy-go.json`（gitignore，不进库）。
测试的加载顺序是**先本目录、缺则回落 `.work/rust-rewrite/`**（见 `tests/common/mod.rs::golden_path`）；
本目录被删时 `http_contract::golden_files_exist` 会判红，不会静默少覆盖。

## 这份基准是怎么采的

**三个一次性采集器**（同包 `_test.go`，跑完立即删除）放到 `internal/codebuddy/`：

| 采集器留档（`.work/rust-rewrite/`） | 覆盖 | 输出环境变量 |
|---|---|---|
| `collect-codebuddy-a2_test.go` | P5a 合同场景（含 meta / pure 段） | `CODEBUDDY_GOLDEN_OUT` |
| `collect-codebuddy-p5b_test.go` | P5b 池/凭据/额度/failover 场景 | `CODEBUDDY_P5B_OUT` |
| `zz_codebuddy_p5c_collect_test.go` | P5c 脱敏（`desensitize=true` 的生产口径）/ SSE 清洗 / 非流式聚合 / 大流 Peek / 统计 / 路径清洗 / 积分失败措辞，外加 11133 退避断连实验（`meta.p5c_cancel`） | `CODEBUDDY_P5C_OUT` + `CODEBUDDY_P5C_CANCEL_OUT` |

```powershell
cd F:\AIXM\proxydeck
Copy-Item .work\rust-rewrite\collect-codebuddy-a2_test.go   internal\codebuddy\zz_codebuddy_a_collect_test.go
Copy-Item .work\rust-rewrite\collect-codebuddy-p5b_test.go  internal\codebuddy\zz_codebuddy_p5b_collect_test.go
$env:CODEBUDDY_GOLDEN_OUT="F:\AIXM\proxydeck\.work\rust-rewrite\p5a-scenarios-go.json"
$env:CODEBUDDY_P5B_OUT   ="F:\AIXM\proxydeck\.work\rust-rewrite\p5b-scenarios-go.json"
go test ./internal/codebuddy -run 'TestZZCollectCodebuddyGolden|TestZZCollectCodebuddyP5B' -count=1
Remove-Item internal\codebuddy\zz_codebuddy_a_collect_test.go, internal\codebuddy\zz_codebuddy_p5b_collect_test.go  # 跑完立即删
node .work\rust-rewrite\_p5b-merge.mjs   # 打 scope、合并、写入库副本与 .work 副本

# P5c（2026-10-16 新增；不依赖上面两个一次性采集器，但用了**常驻**的 pool_test.go 里的 writeAuthInfo 夹具）
Copy-Item .work\rust-rewrite\zz_codebuddy_p5c_collect_test.go internal\codebuddy\zz_codebuddy_p5c_collect_test.go
$env:CODEBUDDY_P5C_OUT       ="F:\AIXM\proxydeck\.work\rust-rewrite\p5c-scenarios-go.json"
$env:CODEBUDDY_P5C_CANCEL_OUT="F:\AIXM\proxydeck\.work\rust-rewrite\p5c-cancel-go.json"
go test ./internal/codebuddy -run 'TestZZCollectCodebuddyP5C' -count=1 -v      # 约 7s（断连实验另有 ~5s）
Remove-Item internal\codebuddy\zz_codebuddy_p5c_collect_test.go                  # 跑完立即删
node .work\rust-rewrite\_p5c-merge.mjs         # 文本级拼接：只插 meta.p5c / meta.p5c_cancel 与替换 scenarios
node .work\rust-rewrite\_unskip-downstream.mjs # 摘掉 P5a/P5b 里 8 处 outbound-only（P5c 之后下游可比了）
```

采集纪律（写死在采集器里，改动前先读它）：

* **装配走生产路径** `NewServerForRegion`：真凭据层（`RealCredential`）+ 真账号池 +
  真 failover 配置。**不是零值 Server**——零值口径会让「生产缺省 `reasoning_effort` 注入」
  这类行为从基准里消失（P5a 最初那版的坑，P5b 已修正）。
* 上游是**进程内假上游**（自写 `RoundTripper`，应答规格记进黄金的 `upstream_spec`）；
  不出网、不占任何现行端口。P5b 追加**规则表**（`rules[]`：`path` / `path_prefix` 命中）与
  **按号分派**（`per_user`：按 `X-User-Id` 给各自独立的应答序列）两种形态。
* 登录态是 `CODEBUDDY_AUTH_DIR` 指向的临时目录里的**合成假文件**（含明文与 `$wbEncrypted`
  两种；secret 由 `WORKBUDDY_AT_REST_SECRET` 注入，与黄金 meta 里的
  `atrest_secret_fixture` 同值），真实 `workbuddy-desktop*.info` 一个字节都不读。
  每个夹具都记 `mtime_unix_ms`——池内排序按 mtime 新→旧，回放侧要落回同一时刻。
* 数据目录（池状态 / failover 配置 / `diag-11133-*.json`）由 `PROXYDECK_DATA_DIR` 指向临时目录。
* 服务经生产 `s.Start()` 起在 `127.0.0.1:0`，走的就是生产的 `corsWith`+`ServeMux` 路由表。
* **429 报文里的重置时点写成远期固定值** `2099-01-01 00:00:00 UTC+8`（两个采集器各一处/两处）——
  这条不是风格问题，写错就是「门禁在采集日之后永久变红」，见下面「时钟纪律」节。

## 门禁怎么用这份基准（四条口径）

1. **采集机字面量本地化**：临时目录在黄金里写成 `__AUTHDIR__` / `__DATA__`（占位符刻意不含
   `<`/`>`/`&`——Go 的 `json.Marshal` 会把它们转成 `\u003c` 之类，占位符一进 JSON 就和
   两侧的转义规则打架）。Rust 侧把占位符还原成本次运行的临时目录后比对（`RealReplay::localize`）。
2. **显式跳过、不静默**：跳过硬编码在测试里并**打印 + 计数**——
   * ~~`SKIP(downstream-p5c)`~~ **P5c 起不再跳过**：原先客户端没要 `stream` 的 200 响应只比出站
     请求与状态码（Go 会聚合成 `chat.completion`），现在这些步（P5a/P5b 共 **8 处**
     `body_compare: "outbound-only"`）的响应体也逐字节比，只有聚合体里的 `created` /
     `chatcmpl-<hex>` 归一成标记（`normalize_volatile_json`）；
   * `scope = "P5b"` 的场景在 P5a 门禁里计入「由 p5b_golden.rs 接管」的计数；
   * `content-length` 头部（派生值，体已逐字节比过）——含绝对路径的体在采集机与本机
     临时目录下长度不同。
   ⚠ **裸 `cargo test` 看不到这些行与汇总**（libtest 默认捕获通过用例的 stdout）——
   要看带 `-- --nocapture`。汇总行形如：

   ```
   HTTP 场景黄金对比：12 个场景 / 56 步逐字节一致；显式跳过 0 步（downstream-only，P5c）+ 23 个 P5b 场景（由 p5b_golden.rs 的生产装配门禁接管）；已登记 allowance 51 处（content-length）
   P5b 黄金对比：15 个场景 / 66 步「键序无关一致」（JSON 响应体与落盘文件按字段集合+取值比，非 JSON 与上游请求体逐字节）；显式跳过下游聚合 0 步（P5c）+ 56 处 content-length（派生头部，体已比过）
   P5c 黄金对比：8 个场景 / 27 步（其中脱敏硬门禁 3 步：出站体逐字节比）
   ```
   （上面三份是 **2026-10-16 P5c 实测原文**；P5c 之前那版写的是「显式跳过 7 步 / 1 步、allowance 44 处」。）

3. **JSON 体的键序无关**：Go 侧序列化**结构体**的地方（`poolState` / `failoverCfg` /
   `AccountView` / `Quota`）是字段声明序，Rust 侧一律键序（等价 `map[string]any`）。
   `p5b_golden.rs::assert_body_equal` 两侧都按 Go 的规则重序列化后再比——比的是
   **字段集合 + 取值 + 数组顺序**，不是结构体声明序。
4. **墙钟字段归一**（逐条登记在 crate README 差异 17~19）：`last_used`、`inflight`、
   `PackageStartTimeRangeEnd` 的**取值**归一成标记（字段存在性与格式照比）；
   `limited_until` **照比**（它是上游给的绝对时点，两侧可复现——前提见下节「时钟纪律」）。

## 🔴 时钟纪律：黄金不得内嵌「采集当天的绝对时点」

**2026-10-16 实测的永久红（教训原文）**：`chat-pool-429` / `chat-hy4-trip` 的假上游 429 报文里
内嵌的是**采集当天**的重置时点（`Quota exceeded, will reset at 2026-10-16 06:20:02 UTC+8`）。
这个字面量不是装饰——它**驱动行为**：`MarkLimited` 记冷却窗口 → 池快照的 `cooling` /
`limited_until`、hy4 兜底的 `hy4_limit.active` / `until`、以及「换号重发还是 hy4 trip」的分支选择。
采集时（10-15）该时点在将来 ⇒ Go 记 `cooling:true`；等墙钟走过那一刻，同一个字面量就变成
**过去** ⇒ 任何实现（含一个字都没改的原实现）都只会算出 `cooling:false`。
**门禁自采集日 +1 天的 06:20 起永久变红，且与代码无关**（stash 掉工作区改动仍必红）。
P5a 时代的 `chat-upstream-errors` 也带同一个字面量（那里它不直接进响应体，但决定
「429 后能否换号重发」这条路径，同属这一类）。

**纪律（再生基准时逐条遵守）**：

1. **驱动行为的绝对时点一律用远期固定值**：本基准统一 `2099-01-01 00:00:00 UTC+8`
   （`will reset at 2099-01-01 00:00:00 UTC+8`）。这样 `cooling` 恒为 true、
   `limited_until` / `hy4_limit.until` 都是稳定值。
2. 真需要「**已过期**」形态时，用**恒定过去**的固定值（年份 ≤ 2000），不要用「采集日前后几天」。
3. **禁止**把时点放在 `2001..2090` 这段：它随墙钟流逝会从「将来」翻成「过去」——
   正是上面那条永久红的机理。
4. `PackageStartTimeRangeEnd`（积分请求体里的 `time.Now()`）属**测量值**，由门禁的
   `norm_upstream_body` 归一成 `__NOW__`，不在此列；但它的**格式**仍被锁住。
5. 夹具里的 token 过期时间（`expiresAt` / `refreshExpiresAt` / `token_expires_at`）
   一律 `4102416000000`（2100 年），别改成「采集时刻 + N 天」。

**自动自检**（不是靠人记）：`tests/p5b_golden.rs::golden_absolute_times_are_not_clock_dependent`
扫**整份**黄金（含 P5a 段）并断言——

* 输入侧：每个 `will reset at <YYYY-MM-DD HH:MM:SS>` 的年份 ∈ `{≥2090} ∪ {≤2000}`；
* 期望侧：由它派生的 `accounts[].limited_until`、`hy4_limit.until` 同样落在该窗口内
  （`0001-01-01…` 是「未限流」常量占位，跳过但仍在门禁里照比）；
* 两处都断言「扫到的条数 > 0」，**自检不会因为基准被换掉而空转**。

违反纪律时的失败长这样（把 2099 改回 2026 复现）：

```
[时钟纪律] 黄金里内嵌的重置时点落在会翻转的窗口里：2026-10-16 06:20:02（年份 2026）。
改用远期固定值（年份 ≥ 2090）或恒定过去的固定值（≤ 2000）——见 tests/golden/README.md
```


## 反向验证（门禁非空转）

`.work/rust-rewrite/_mutation-check-codebuddy.ps1` 逐条把实现改坏，突变全部让门禁变红：

| # | 改坏什么 | 结果 |
|---|---|---|
| M1 | `chat.rs` 的 `TRANSIENT_MAX_RETRY` 2 → 0（不重试） | 红（上游命中数 1 ≠ 3） |
| M2 | `toolrefs.rs` 不再删用过的 `$defs` | 红（`pure.tool_refs` 逐字节不符） |
| M3 | `models.rs` 的探测 TTL 缓存恒不命中 | 红（上游命中数翻倍） |
| M4 | `diag.rs` 的现场上限 10 → 11 | 红（现场文件份数不符） |
| M5 | `gjson.rs` 不转义 `<` | 红（health 体逐字节不符） |
| M6 | `godecode.rs` 顶层 `null` 改成报错 | 红（`null` 体应回 `messages is required`） |
| M7 | `pool.rs` 的 `Pick` 不跳过 `Dead` | 红（chat-pool-401 的每号命中数不符） |
| M8 | `failover.rs` 的 `parse_quota_reset` 少认一族关键字 | 红（chat-pool-429 的 cooling/换号不符） |
| M9 | `chat.rs` 不注入缺省 `reasoning_effort` | 红（reasoning-default 的上游体缺字段） |
| M10 | `credential.rs` 的区域过滤放宽（不校验 domain） | 红（INTL 场景拿到 CN 号） |
| M11 | `atrest.rs` 的 AAD framing 少一个字节 | 红（credentials 场景的加密态解不出来） |
| M12 | `pool.rs` 的状态落盘多写 `Dead` | 红（池状态落盘内容与 Go 不符） |

跑完还原并复跑全量（脚本自己会做，且会把 mtime 顶到当前时间——否则 cargo 可能认为没变、
继续用改坏的二进制，P3 踩过这个坑）。
