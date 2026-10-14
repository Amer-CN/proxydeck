# rs-comate 黄金基准（入库）

两份基准都是**真 Go 侧 `internal/comate` 采收**的，Rust 实现按它们逐字节对齐
（门禁：`tests/golden_comate.rs`）。

| 文件 | 内容 | 体积 | SHA-256 |
|---|---|---|---|
| `golden-comate-go.json` | part 1：纯函数（`flatten`/`chunk`/`complete`/`readFullAnswer`）、生产 `postUpstream` 的真实出站字节（`out`/`wire`）、真 HTTP 路径（`health`/`models`/`once`/`routes`） | 105,735 B | `5D65CC53EF931E7212D1604A41EEE069C35748FAD37A24D078E604AD69EAC4F3` |
| `golden-comate-stream-go.json` | part 2：流式读循环的下游帧（14 例，含 `[DONE]`、`reasoning_content`、502 分支） | 37,008 B | `5D4A1A94C553ECEC8F8B66D8A4FB61F213E319F52558275B73DCAFC3D1CAB771` |

两份都**完整入库**（合计 142,743 B < 1 MB），所以门禁**没有任何跳过**：
`.work/` 被清掉也照样全绿，`.work/rust-rewrite/` 下的同名文件只是重新采集时的落点。
`tests/common/mod.rs::golden_path()` 的加载顺序是「先入库目录、缺则回落 `.work/`」。

## 怎么再采（三步链）

采集需要**临时往 `internal/comate/` 放一个同包的 `_test.go`**（只有同包才能指到
`flattenMessages` / `chunkJSON` / `readFullAnswer` / `handleChat` 这些未导出的缝）。
采集完**必须立即删除**，删除后这三条必须干净：

```powershell
git status --short internal/            # 必须空
go test ./internal/comate -count=1      # 必须 ok（证明包还在未污染状态）
go vet ./internal/comate                # 必须退出码 0
```

### 第 0 步：生成流式采集器（由脚本机械抽取，勿手改）

`internal/comate` 的流式 handler 里，上游地址是**生产常量** `127.0.0.1:8792`，
本机该端口跑着真 zulu（不许动），所以无法用假上游占端口注入响应。做法是：
由 `.work/rust-rewrite/mkcomate-splice.mjs` 从 `internal/comate/server.go` **逐行机械抽取**
读循环（两个标记字符串之间），生成一个同包测试文件 `internal/comate/zz3_comate_spliced_stream_test.go`：
采集器里跑的是**生产那几行**，不是转述版。生成器自带校验（标记唯一 + 关键分支计数），
漂移即拒绝生成。

```powershell
node .work/rust-rewrite/mkcomate-splice.mjs     # 写 internal/comate/zz3_comate_spliced_stream_test.go
```

### 第 1 / 2 步：两个采集器

`zz_comate_golden_collect_test.go`（采集器 #1）**不在仓库里**——它是本轮的一次性临时文件，
收尾时已按纪律从 `internal/comate/` 删除（`git status --short internal/` 为空、
`go test`/`go vet` 干净）。**原样留档在 `.work/rust-rewrite/collect-comate-part1_test.go`**，
重采时把它复制回包内、改回 `_test.go` 后缀即可（它必须与采集器 #2 同时在包里，
因为 #2 复用了 #1 里的 `hop` / `zuluHit` / `rtState` / `clientSystem` / 假 zulu 等脚手架）：

```powershell
Copy-Item .work\rust-rewrite\collect-comate-part1_test.go internal\comate\zz_comate_golden_collect_test.go
# 采集器 #2 由生成器产出（见上一步）：
node .work/rust-rewrite/mkcomate-splice.mjs

# 采集器 #1：part 1（不依赖 8792；上游经 http.DefaultTransport 拦截器合成）
$env:COMATE_GOLDEN_OUT="F:\AIXM\proxydeck\.work\rust-rewrite\golden-comate-go.json"
go test ./internal/comate -run TestZZCollectComateGolden -count=1 -v

# 采集器 #2：part 2
$env:COMATE_STREAM_GOLDEN_OUT="F:\AIXM\proxydeck\.work\rust-rewrite\golden-comate-stream-go.json"
go test ./internal/comate -run TestZZCollectComateStream -count=1 -v

# 采完把两份回填到入库目录（先确认内容，再覆盖）
Copy-Item .work\rust-rewrite\golden-comate-go.json        rs-comate\tests\golden\ -Force
Copy-Item .work\rust-rewrite\golden-comate-stream-go.json rs-comate\tests\golden\ -Force

# 采完立即删掉两个临时采集器（**这一步不许省**）
Remove-Item internal\comate\zz_comate_golden_collect_test.go, internal\comate\zz3_comate_spliced_stream_test.go
git status --short internal/    # 空
```

## 采集器的三条纪律（踩过的坑都在这）

1. **打网关的跳必须用 `clientRaw()`**（`Transport=systemTransport` + `CheckRedirect=ErrUseLastResponse`）。
   拦截器挂在 `http.DefaultTransport` 上；用 `http.DefaultClient` 打本地网关，请求会被自己的
   拦截器截走、采回假上游的合成响应——上一轮那份 part 1 基准就是这么废的
   （`health`/`models`/`once` 三段 body 全是字面量 `"ok"`，物证留在
   `.work/rust-rewrite/golden-comate-go.round1-contaminated.json`）。
2. **绝不碰 8792 与真实 `~/.comate/cli-login.json`**：`t.Setenv("USERPROFILE", 临时目录)`、
   `t.Setenv("COMATE_ZULU", 假zulu.cmd)`；`zuluHealth()` 走拦截器 ⇒ 既不会真连 8792、
   也不会误判「不健康」而 spawn。真 zulu 全程照跑。
3. **假 zulu 用 Go 编译的小程序**（不是 `.cmd`）：`list-model` 要精确字节（`.cmd` 的 `echo`
   会补 CRLF 让 `encoding/json` 报错），同时把每次调用追加进 `FAKE_ZULU_LOG`，
   用来锁 10 分钟模型缓存的命中次数。

## 超长 SSE：只存「可机械重建的规格」

`once-oversize-line` / `read-oversize` / `stream-oversize-line` 三条用例的 SSE 各带
614,400 个「长」（1.84 MB）。它们的结论（越过 `bufio.Scanner` 的 1 MB 上限 → `token too long`）
与原文体积无关，所以基准里**不存原文**，只存：

```json
{ "sse_omitted_bytes": 1843205,
  "sse_prefix": "data: {\"kind\":\"completed\",\"status\":\"ok\",\"content\":\"",
  "sse_repeat": "长", "sse_repeat_count": 614400,
  "sse_suffix": "\"}\n\n" }
```

重建规则 `prefix + repeat × count + suffix` 在两侧各实现一次
（采集器 `sseCtx()` / 门禁 `tests/common/mod.rs::sse_from_ctx()`），逐字节等价。
这条把基准从 3.7 MB 压到 99 KB，同时**一条用例都没丢**。

## 可复现性核对

两次采集的差异比较（归一化易变字段后逐字节比）：

```powershell
Copy-Item .work\rust-rewrite\golden-comate-go.json .work\rust-rewrite\_repro-a1.json -Force
$env:COMATE_GOLDEN_OUT="F:\AIXM\proxydeck\.work\rust-rewrite\golden-comate-go.json"
go test ./internal/comate -run TestZZCollectComateGolden -count=1
node .work\rust-rewrite\_diffcomate.mjs .work\rust-rewrite\_repro-a1.json .work\rust-rewrite\golden-comate-go.json
```

实测结论：除 `once` 段里少数 `resp_headers.content-length` 外**完全一致**——
那个值内嵌了随机的 `chatcmpl-comate-<rand>` 位数（同一响应体换个 id 长度就差一两个字节），
属于已知且可解释的易变字段。流式基准两次采集逐字节一致。

## 门禁的归一化口径

Rust 侧回放时按采集器**同一套规则**归一化后再比（`tests/common/mod.rs`）：

* 忽略 `date`（时钟）与 `meta.collected_at`；
* `"created":<数字>` → `"created":<CREATED>`、`"id":"chatcmpl-comate-<数字>"` → `"id":"<ID>"`
  （`chatcmpl-comate-0` 这个固定 id 是抽取循环用来过归一化的，见生成器）；
* `uptimeSec` 不参与逐字比（进程内现生成）；
* `content-length`：非 HEAD 时只校验「等于本次响应体长度」（黄金那个值带随机 id 位数）；
  HEAD 时与黄金值逐字比（HEAD 没有响应体，长度就是 GET 会回多少字节，稳定）。
