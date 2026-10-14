# rs-codebuddy 黄金基准

本目录只放**入库**的黄金基准。当前一档：

| 文件 | 体积 | 内容 |
|---|---|---|
| `golden-codebuddy-go.json` | 222 KB | **27 个 HTTP 场景 / 122 步** + 纯函数四段（tool_choice 15 / tool_refs 17 / fingerprint 4 / summarize_tool 6） |

`scenarios[].scope` 分两段：

* `scope = "P5a"`：合同与请求侧，12 个场景 / 56 步（`tests/golden_codebuddy.rs`）；
* `scope = "P5b"`：账号池 / 凭据 / 额度 / failover，15 个场景 / 66 步（`tests/p5b_golden.rs`）。

同源副本另有一份在 `.work/rust-rewrite/golden-codebuddy-go.json`（gitignore，不进库）。
测试的加载顺序是**先本目录、缺则回落 `.work/rust-rewrite/`**（见 `tests/common/mod.rs::golden_path`）；
本目录被删时 `http_contract::golden_files_exist` 会判红，不会静默少覆盖。

## 这份基准是怎么采的

**两个一次性采集器**（同包 `_test.go`，跑完立即删除）放到 `internal/codebuddy/`：

| 采集器留档（`.work/rust-rewrite/`） | 覆盖 | 输出环境变量 |
|---|---|---|
| `collect-codebuddy-a2_test.go` | P5a 合同场景（含 meta / pure 段） | `CODEBUDDY_GOLDEN_OUT` |
| `collect-codebuddy-p5b_test.go` | P5b 池/凭据/额度/failover 场景 | `CODEBUDDY_P5B_OUT` |

```powershell
cd F:\AIXM\proxydeck
Copy-Item .work\rust-rewrite\collect-codebuddy-a2_test.go   internal\codebuddy\zz_codebuddy_a_collect_test.go
Copy-Item .work\rust-rewrite\collect-codebuddy-p5b_test.go  internal\codebuddy\zz_codebuddy_p5b_collect_test.go
$env:CODEBUDDY_GOLDEN_OUT="F:\AIXM\proxydeck\.work\rust-rewrite\p5a-scenarios-go.json"
$env:CODEBUDDY_P5B_OUT   ="F:\AIXM\proxydeck\.work\rust-rewrite\p5b-scenarios-go.json"
go test ./internal/codebuddy -run 'TestZZCollectCodebuddyGolden|TestZZCollectCodebuddyP5B' -count=1
Remove-Item internal\codebuddy\zz_codebuddy_a_collect_test.go, internal\codebuddy\zz_codebuddy_p5b_collect_test.go  # 跑完立即删
node .work\rust-rewrite\_p5b-merge.mjs   # 打 scope、合并、写入库副本与 .work 副本
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

## 门禁怎么用这份基准（四条口径）

1. **采集机字面量本地化**：临时目录在黄金里写成 `__AUTHDIR__` / `__DATA__`（占位符刻意不含
   `<`/`>`/`&`——Go 的 `json.Marshal` 会把它们转成 `\u003c` 之类，占位符一进 JSON 就和
   两侧的转义规则打架）。Rust 侧把占位符还原成本次运行的临时目录后比对（`RealReplay::localize`）。
2. **显式跳过、不静默**：三处跳过硬编码在测试里并**打印 + 计数**——
   * `SKIP(downstream-p5c)`：客户端没要 `stream` 的 200 响应（Go 会把上游 SSE **聚合**成
     `chat.completion`），本轮只比出站请求与状态码；
   * `scope = "P5b"` 的场景在 P5a 门禁里计入「由 p5b_golden.rs 接管」的计数；
   * `content-length` 头部（派生值，体已逐字节比过）——含绝对路径的体在采集机与本机
     临时目录下长度不同。
   ⚠ **裸 `cargo test` 看不到这些行与汇总**（libtest 默认捕获通过用例的 stdout）——
   要看带 `-- --nocapture`。汇总行形如：

   ```
   HTTP 场景黄金对比：12 个场景 / 56 步逐字节一致；显式跳过 4 步（downstream-only，P5c）+ 15 个 P5b 场景…；已登记 allowance 9 处（content-length）
   P5b 黄金对比：15 个场景 / 66 步键序无关一致（Go 结构体声明序 vs Rust 键序见 crate README 差异 22）；显式跳过下游聚合 1 步（P5c）+ 55 处 content-length（派生头部，体已逐字节比过）
   ```

3. **JSON 体的键序无关**：Go 侧序列化**结构体**的地方（`poolState` / `failoverCfg` /
   `AccountView` / `Quota`）是字段声明序，Rust 侧一律键序（等价 `map[string]any`）。
   `p5b_golden.rs::assert_body_equal` 两侧都按 Go 的规则重序列化后再比——比的是
   **字段集合 + 取值 + 数组顺序**，不是结构体声明序。
4. **墙钟字段归一**（逐条登记在 crate README 差异 17~19）：`last_used`、`inflight`、
   `PackageStartTimeRangeEnd` 的**取值**归一成标记（字段存在性与格式照比）；
   `limited_until` **照比**（它是上游给的绝对时点，两侧可复现）。

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
