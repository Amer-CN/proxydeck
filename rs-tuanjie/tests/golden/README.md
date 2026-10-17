# P8a 黄金基准（入库副本）

P8a 门禁（`../golden_p8a.rs`）优先读本目录，缺则回落 `.work/p8a/`（与 rs-bai 同规：**干净检出
也能跑**，不依赖 gitignore 的 `.work/`）。

| 文件 | 内容 | 来源 |
|---|---|---|
| `golden-p8a-go.json` | 签名向量 253 条（含 `signing_key_hex`）/ 重排用例 21 条 / UA 常量与版本正则行为表 / 头集合 / 端点与超时 | Go 侧一次性采集器现采 |
| `go-outbound-wire.txt` | Go `net/http` **线上实抓**的原始请求字节（头序、大小写、自动补头） | 同一采集器现采 |
| `tls/go-clienthello-sni.bin` | Go utls `HelloChrome_Auto` + ALPN 改写后的真实 ClientHello（1796B） | `utlsHandshake(ctx, raw, "codely-litellm.tuanjie.cn:443")` |
| `tls/rustls-clienthello-sni.bin` | 候选 (a) rustls 默认栈的 ClientHello（257B） | `.work/p8a/rustls-probe` |
| `tls/wreq-clienthello-chrome.bin` | 候选 (b) `wreq` + Chrome 模拟（BoringSSL）的 ClientHello（1762B） | `.work/p8a/wreq-probe` |
| `tls/parse-clienthello.mjs` | 两侧共用解析器（扩展序/密套件/key_share/ALPN/JA3/JA4） | — |

解析任意一份：`node tests/golden/tls/parse-clienthello.mjs tests/golden/tls/*.bin`

## 再生成链（P8a；**2026-10-16 P8b 已把采集器源码留档并实测复现**）

采集器源码留档在 `.work/p8a/`（gitignore，不进库）：
`zz_p8a_golden_test.go`（签名/重排/UA/头集合/线上字节）、`zz_p8a_hello_sni_test.go`（ClientHello 记录）。

1. **签名 / 重排 / UA / 头集合 / 线上字节**：把留档的 `zz_p8a_golden_test.go` 拷回
   `internal/tuanjie/`，**编译成固定目录里的测试二进制再跑**（关键：`exe_dir`/`metadata.cwd`
   取自 `appdir.ExeDir()`，用 `go test` 直跑会落在随机 `go-buildNNNN` 目录里，基准就对不上）：

   ```powershell
   $d = "E:\Temp\go-build2757689516\b001"        # 入库基准记录的那个 exe 目录（长度必须一致）
   New-Item -ItemType Directory -Force -Path $d | Out-Null
   go test -c -o "$d\p8a.test.exe" ./internal/tuanjie
   $env:P8A_GOLDEN_OUT="<repo>\.work\p8a\golden.json"
   $env:P8A_WIRE_OUT="<repo>\.work\p8a\wire.txt"
   $env:P8A_TS="1792162280"                       # 固定签名时间戳（见下）
   & "$d\p8a.test.exe" "-test.run=TestZZCollectP8aGolden"   # PowerShell 下必须用 = 形式
   ```

   **跑完立即从 `internal/` 删除该临时文件**——删除后 `git status --short internal/` 必须为空、
   `go vet ./internal/tuanjie` 退出码 0。采集器**不读真实凭据**：会话/密钥全是合成值。

2. **两个时变口径**（不处理就永远对不上，别误判成采集器写错）：
   - `exe_dir` 与 21 条用例的 `cwd` = 测试二进制所在目录。基准里记的是
     `E:\Temp\go-build2757689516\b001`（32 字符）——它同时决定出站体的 `cwd` 字段长度与
     `Content-Length`，所以**必须把二进制放在同名同长的目录里**才能真正逐字节复现。
   - 签名头（`header_set` 与线上抓包各一处）里的 unix 秒是**实时时钟**。`P8A_TS` 把它固定：
     采集器用的是生产 `SignLitellm(path, key, P8A_TS)`（只注入时钟输入，不另写算法），
     并顺带自校验「生产头里的签名 == 用其自身 ts 复算的值」。
     入库基准的 ts = **1792162280**。

   **2026-10-16 实测**：按上面两步重采，两份产物与入库副本**零归一化、逐字节相同**——
   `golden-p8a-go.json` SHA-256 `5BFBBA40EEE55B293BA57F9C337C27A9AA20E3DE105ED4F4D24A688E3EB84D37`（95,558 B）、
   `go-outbound-wire.txt` SHA-256 `0931A49AC3A2D7FE67B9A935CD5912A4CC351C53F053D0EA70651615A905AFFF`（820 B）；
   连跑两次同哈希。

3. 把 `.work/p8a/golden.json` 与 `.work/p8a/wire.txt` 复制回本目录（两份逐字节相同）。
4. **ClientHello 证据**（只在需要复核 uTLS 路线时重采）：同一个留档的
   `zz_p8a_hello_sni_test.go` 兼顾两种形态，SNI 由 `P8A_HELLO_ADDR` 决定：

   ```powershell
   $env:P8A_HELLO_ADDR="codely-litellm.tuanjie.cn:443"   # 生产同款（SNI 在场）
   $env:P8A_HELLO_OUT="<repo>\.work\p8a\go-clienthello-sni.bin"
   & "$d\p8a.test.exe" "-test.run=TestZZCollectHello"
   # IP 形态（无 SNI，对齐 wreq 探针口径）：P8A_HELLO_ADDR="127.0.0.1:443"
   ```

   ClientHello **每次抓都不一样**（GREASE 值、扩展序随机置换、ECH padding 随机，连抓两次的
   JA3 都不同），验收口径是解析器给出的**密套件表 / 扩展集合 / 签名算法 / key_share 长度 /
   JA4 载荷哈希**一致，不是逐字节。2026-10-16 重采实测：1796 B、
   `JA4=d130f10h1_4560fe214f0b_14c5`、cipherCount=16、extCount=18，与入库副本同值。
   解析：`node tests/golden/tls/parse-clienthello.mjs <各 .bin>`。
   Rust 侧两个候选探针在 `.work/p8a/{rustls-probe,wreq-probe}`（`wreq` 探针需要
   `cmake` + `NASM` + `libclang`）。

## 基线采集时的机器事实（写下来免得后人误判）

- 采集时本机 npm 全局 `@unity-china/codely-cli` 版本 = `1.0.0-rc.60`（于是 UA 解析值 =
  `codely-cli/1.0.0-rc.60 (win32; x64)`）。**UA 版本号随本机安装走**，换机后这条会变——门禁用
  「模板 + 兜底常量 + 版本正则行为表」锁死，不锁死具体版本号。
- `metadata.cwd` 在基准里是**采集机 Go 测试二进制的目录**（`E:\Temp\go-build…`）。Rust 侧由
  `reshape_chat_body_with_cwd(.., cwd)` 注入同一值做逐字节比对；生产路径取 exe 目录。

## P8b 切片的基准（`golden-p8b-go.json`）

覆盖 `pacing.go` / `inflight.go` / `opencode_session.go` / `dist.go`（纯函数）/
`channels.go`（静态表）。采集器留档 `.work/p8b/zz_p8b_collect_test.go`。

**再生成链**（把留档的采集器拷回 `internal/tuanjie/`，跑完立即删除）：

```powershell
New-Item -ItemType Directory -Force -Path "<repo>\.work\p8b\datadir" | Out-Null
$env:P8B_GOLDEN_OUT="<repo>\.work\p8b\golden.json"
$env:P8B_DATA_DIR="<repo>\.work\p8b\datadir"     # 必须给：隔离 tuanjie-pacing.json，绝不碰真实数据目录
go test ./internal/tuanjie -run TestZZCollectP8b -count=1 -v
```

**时变口径只有一处、且已消除**：`pacing` 段的两个文件内容断言走 `P8B_DATA_DIR` 下的临时
`tuanjie-pacing.json`（`{"enabled":true}` / `{"enabled":false}`），与墙钟无关；
`inflight` 段的 `rid_first_5` 依赖「进程内计数器从 0 起」，用 `-run` 只跑本用例即成立
（`rid_from_n(1..5)` 是纯函数面，Rust 侧锁的就是它）。
`dist` 段全是纯函数；`channels` 段是静态表。

2026-10-16 实测：`go test ./internal/tuanjie -run TestZZCollectP8b -count=1 -v` 产出
11,523 B；`cargo test --test golden_p8b` 5/5 通过（`--nocapture` 打印逐段对照证据）。
**生成本基准时 `dist.go` 的 `collectDistSamples` 与 `channels.go` 的网络面未采**——它们不在
本切片（见 `rs-tuanjie/README.md` 的「P8b 切片未覆盖」清单）。

## P8b 第二片的基准（`golden-p8b-quota-go.json`）

覆盖 `quota.go`：缓存键（`sha256(token)[:8]`）、TTL 常量、请求形态（路径/头）、
`Quota` 的 JSON 逐字节形态，以及 **13 个行为用例**（live → cache → force → 503 → 负缓存 →
失败回退旧值 → 200 坏 JSON → 请求形态 → 视图层 4 条）。采集器留档
`.work/p8b/zz_p8b_quota_collect_test.go`。

**再生成链**（把留档的采集器拷回 `internal/tuanjie/`，跑完**立即删除**）：

```powershell
Copy-Item <repo>\.work\p8b\zz_p8b_quota_collect_test.go <repo>\internal\tuanjie\
$env:P8B_OUT="<repo>\.work\p8b\golden-p8b-quota-go.json"    # 必须是**绝对路径**（go test 的 cwd 在包目录）
go test ./internal/tuanjie -run TestZZCollectP8bQuota -count=1 -v
Remove-Item <repo>\internal\tuanjie\zz_p8b_quota_collect_test.go
Copy-Item <repo>\.work\p8b\golden-p8b-quota-go.json <repo>\rs-tuanjie\tests\golden\ -Force
```

**采集器怎么喂上游（两个坑，照做才采得到）**：

1. `quota.go:26` 的 `billingSummaryURL` 是**包级 https 常量**，无法改指 ⇒ 采集器给这个客户端
   换一个「标准库 Transport + `DialTLSContext` 拨号重定向到 httptest 假上游」的实现
   （先例：`pacing_pool_test.go:72-93`）。**不能覆写 `smartProxyTransport`**——它是 uTLS
   实现（自带 RoundTripper、不认 `DialTLSContext`），覆写无效、请求会真出网。
2. 假上游要回**官网原始报文**（snake_case 的 `/api/user/billing/usage/summary` 响应），
   而黄金里的 `body` 是 **Go 提取后的 `Quota` JSON**。第一版就拿错了（拿提取后的体喂上游 →
   全部字段落成零），症状是 `live` 用例 `total=0`。

**第三个坑（第一次采集真踩到）**：`NewAccountPool()` 会调 `EnsureLocalAccount()` → 读
`~/.codely-cli/oauth_creds.json`。采集器必须**把 `USERPROFILE`/`HOME` 指到空临时目录**
（逼它返回「读取团结登录态失败」），否则会把**真实本地账号**（含真实 sub）并进采集用的池里
——违反「不读真实凭据」纪律。第一次采集正是如此（池里混进第三个号），第二次加隔离后重采：
除 `meta.collected_at` 外**与第一次逐字节相同**（说明那次并没有把真实数据写进基准）。

**时变口径**：全部无墙钟依赖——包的积分缓存**按 token 分键**（每个用例一把独立合成 token），
假上游的重置时刻是 `2099-01-01`，`token` 是合成 JWT（`exp=4102444800`）。
其余字段（`request_id` 之类）只在真出网时才有，本基准里不存在。

2026-10-17 实测：产出 20,513 B、13 个用例；`cargo test --test p8b_quota_diff` 3/3 通过。
两份副本同哈希 `D2F94AF3C6D0C3AE34F2DEA30FAA7120F3132517D5B5FDA170A5CB81AAC92F85`。
**采集器绝不出网、不碰 8788、不读真实登录态**：数据目录走 `t.TempDir()` +
`PROXYDECK_DATA_DIR`，账号池文件是合成账号，`USERPROFILE` 指向空目录。

## P8b 第三片：**没有新的入库基准**（对照方式不同，别照 P8a/P8b 去找）

第三片（server 骨架与路由）的对照**不落 golden JSON**，而是**两侧同跑一圈固定请求**、
把每次响应的 `{status, content_type, body}` 写成报告再逐条 diff：

```powershell
# 一键复跑（Rust 侧起服务子进程 + Go 侧起真 Server + 逐条 diff + 清临时采集器）
pwsh -File <repo>\.work\p8b3\run-diff.ps1

# 反向验证：5 条突变逐条让门禁变红、逐条还原复绿
node <repo>\.work\p8b3\mutate.mjs
```

**为什么不用 golden JSON**：这一片的被测面是**整个 HTTP 服务**（路由匹配、方法判定、
CORS 头、状态码、Content-Type、`writeJSON` 的末尾换行），用静态基准只能锁住"响应体"，
锁不住"路由/头/方法"这些骨架行为；两侧同输入并排跑才覆盖得到。

**两侧同输入的三个关键**（少一个就采不出可比的对照）：

1. **同一份用例表**：`rs-tuanjie/tests/p8b3_scenario/mod.rs` 的 `cases()` 是唯一来源，
   Go 侧采集器（`.work/p8b3/zz_p8b3_collect_test.go` 留档）逐条照抄（含 `setup` 前置动作）。
2. **同一份合成登录态**：合成 JWT（`sub=uid-p8b3-synthetic`、`exp` 远期）写进两侧各自的
   临时 `USERPROFILE/.codely-cli/oauth_creds.json`；两侧 `PROXYDECK_DATA_DIR` 都指临时目录，
   每轮**从干净状态起**（上一轮的 POST 会改落盘文件 —— 踩过一次，`accounts-get` 因此对不上）。
3. **同一个假上游**：三条路径（换 key / 计费 / 模型清单）都回固定合成报文。Go 侧不改源码，
   靠把 `http.DefaultTransport` 换成「改写出口 Host」的 RoundTripper；Rust 侧走
   `RS_TUANJIE_TEST_BASES` 测试缝 —— 两侧的 URL/头/UA/签名/超时全部走各自**生产实现**。

**两个采集坑（照做才对得上）**：

1. **采集器自己的 HTTP 客户端必须显式带 transport**：Go 侧把 `http.DefaultTransport` 换成了
   出口改写器，`http.DefaultClient` 会把「打本机被测服务」的请求**一起改写走假上游** ——
   症状是探针打 `/health` 收到假上游的 404，查了半天才发现打错了对象。
2. **假上游要关连接（`Connection: close`）**：逐请求应答的假上游不关连接时，客户端会复用
   已被关掉的 socket，症状是随机的 `转发请求网络失败`（`models-get-final` 变 502）、
   两侧上游命中数凭空差一条。

## P8b 第四片的基准（`golden-p8b4-go.json`）

覆盖 `handleChat` 的池路径 + 单账号路径的**六个确定性场景**（假上游按 `cli_api_key` 逐次给
应答）：`chat-200`、`chat-429-switch-then-200`（429 即时换号两跳后恢复）、`chat-402-swap`
（402 禁用该号并换号）、`chat-retry-5xx`（502 可重试：首发 + 3 次重发）、`chat-noretry-400`
（`invalid model name` 确定性错误：**不重发**）、`chat-empty-then-200`（池路径**不**嗅探：
首份 200 空体原样透传）。每条记 `{status, content_type, body, retry_after, cache_control,
chat_order, chat_calls}`。

采集器留档 `.work/p8b4/zz_p8b4_collect_test.go`（一次性，跑完即从 `internal/tuanjie/` 删除）。

**再生成链**：

```powershell
Copy-Item <repo>\.work\p8b4\zz_p8b4_collect_test.go <repo>\internal\tuanjie\
$env:PROXYDECK_DATA_DIR = "<隔离临时目录>"      # 必须给（采集器内部还会 t.Setenv，双保险）
$env:P8B4_GOLDEN_OUT = "<repo>\.work\p8b4\golden-p8b4-go.json"
go test ./internal/tuanjie -run TestZZCollectP8B4 -count=1 -v
Remove-Item <repo>\internal\tuanjie\zz_p8b4_collect_test.go
Copy-Item <repo>\.work\p8b4\golden-p8b4-go.json <repo>\rs-tuanjie\tests\golden\ -Force
```

**采集器的两个坑（照做才对得上）**：

1. **头要从 `rec.Header()` 取，不要 `rec.Result().Header`**：`httptest.ResponseRecorder`
   的 `Result()` 会把某些头搬进 `http.Response` 的结构化字段，第一版用它取 `Cache-Control`
   拿到的是空串 —— 于是 Rust 侧那条 `Cache-Control` 断言**形同虚设**（去掉实现里的 insert
   仍然全绿）。改取 `rec.Header()` 后 `cache_control` 才真的是 `no-cache`，突变实验随之变红。
2. **换了脚本必须同时清命中计数**：Rust 侧假上游（`tests/p8b3_scenario/mod.rs`）
   多个场景共用一个 `FakeUpstream`，`set_chat_script` 若只换脚本不重置 `chat_calls`，
   新场景会从脚本中间取应答（`chat-empty-then-200` 因此取到第二条，把「池路径不嗅探」
   误读成「嗅探了」）。

**时变口径**：全部无墙钟依赖 —— `Retry-After`/换号退避由测试注入固定值，假上游应答是常量，
`handleChat` 的 `dur`/`elapsed` 不进基准。

2026-10-17 实测：`cargo test --test p8b4_diff` **2/2 通过**，6 场景逐条一致
（`status` / `content-type` / `body` 逐字节 / `Retry-After` / `Cache-Control` /
上游命中序与逐号命中数）。

## P8b 第五片的基准（`golden-p8b5-go.json`，131 KB）

覆盖 **60 步 HTTP 用例 + 9 条探针体 + 1 轮探针 + 1 组基线**：媒体改路由/远程图片预下载、
外部 provider 的三条转发路径（OpenAI chat / `/responses` / `/v1/messages`，含流式、4xx 透传、
5xx→空 200、异常形态）、`/providers` 的 GET/POST 全覆盖、`/media-config` 与 `/vision-config`、
生图/生视频四个端点、`probeCall` 的 max_tokens 自适应名单、整轮 `RunPipelineProbes` 结果、
以及基线库的加载/迁移/回写字节。

**与其余基准的结构差异**：这份金标里还带一份 `scenario`（场景定义，`{UP}` 占位符保留）——
Rust 侧**按同一份定义跑同一串请求**，不靠两套手写清单对齐。要改场景就改采集器并重采，
不要在 Rust 侧另写。

采集器留档 `.work/p8b5/zz_p8b5_collect_test.go`（一次性，跑完即从 `internal/tuanjie/` 删除）。

**再生成链**：

```powershell
Copy-Item <repo>\.work\p8b5\zz_p8b5_collect_test.go <repo>\internal\tuanjie\
$env:PROXYDECK_DATA_DIR = "<隔离临时目录>"
$env:P8B5_GOLDEN_OUT = "<repo>\.work\p8b5\golden-p8b5-go.json"
go test ./internal/tuanjie -run TestZZCollectP8B5 -count=1
Remove-Item <repo>\internal\tuanjie\zz_p8b5_collect_test.go
Copy-Item <repo>\.work\p8b5\golden-p8b5-go.json <repo>\rs-tuanjie\tests\golden\ -Force
```

**采集器的四个坑（照做才对得上）**：

1. **媒体配置是 Go 的包级全局**：`LoadMediaConfig` 只在进程启动读一次盘，同进程里连续
   `NewServer()` 会继承上一条用例的 `vision/image/video`。采集器每个场景前显式复位
   （`p8b5ResetMediaGlobals`），否则比的是上一条用例的配置（Rust 侧 `MediaState` 是
   per-`AppState`，天然没有这个问题）。
2. **步骤分派要用去查询串的路径**：`/agnesapi?video_id=…` 若按带参路径判会落进 `handleChat`
   分支（第一版就踩了：金标里那条 `agnesapi` 是 chat 的 405，而不是任务查询）。
3. **`script` 的两类键**：`sk-key-<uid>` 是团结域的 cli_api_key（按号给应答），其余是
   **完整 path**（外部 provider）。放错桶就等于该用例的上游没有脚本 ⇒ 404。
4. **断言「未改动」时别只比响应体**：远程图片用例的出站体里带 `metadata`/`litellm_session_id`/
   `prompt_cache_key`（每请求随机）与 `cwd`（各自进程的 exe 目录）——Rust 侧比对时先摘掉这三键，
   否则全是假红（摘掉的是**会话层**，其逐字节合同在 `go-outbound-wire.txt` 里）。

**时变口径**：`created`（`time.Now().Unix()`）与 `chatcmpl-<rid>` 在两侧都做**点替换**
（`<TS>` / `<ID>`）；其余字节逐位比。上游端口不同 ⇒ `https?://127.0.0.1:<port>` 一律换成 `<UP>`。

2026-10-17 实测：`cargo test --test p8b5_diff` **1/1 通过**，
`60 步 HTTP 用例（出站体逐字节 21 / 规范化 12）+ 9 条探针体 + 1 轮探针 + 1 组基线；失败 0 条`。
反向验证 8/9 条突变 RED（1 条 GREEN 是 PNG 嗅探分支的已知覆盖边界，见 `rs-tuanjie/README.md`）。




## P8c 的基准（`golden-p8c-go.json`，18,892 B + `p8c-synthetic-seed.tgz` 297 B）

覆盖 **108 项**：种子扫描（合成 tgz 双轨道 + registry 零下载口径）、注水（三题/漂移/
环形/落盘）、baseline 比对面、CDP 探测消息、代理三步判定、appdir 清单/迁移/轮转。
合成 tgz 由 `.work/p8c/mktgz.py` 确定性生成（两侧解**同一份字节**）。

**⚠ 再生成链断档（P8c 审查后如实登记）**：采集器源码（`zz_p8b_collect_test.go` 同款的一次性
Go 文件）**没有留档**——执行轮跑完即删，`.work/p8c/` 里只有产物（golden/tgz/日志），
没有源码。与 P8a/P8b/P8b5（源码都在 `.work`）不同，P8c 的金标当前**不可重采**。
金标本身有效（已入库，Rust 门禁逐条对照它）；若将来 Go 侧这些模块有变，需按
`tests/p8c_diff.rs` 的场景清单重写采集器（场景定义见该文件头注释与金标 `name` 键）。
2026-10-17 实测：`cargo test --test p8c_diff` **1/1 通过，108 项逐条一致**。

## P8d-3 的基准（`golden-p8d3-go.json`，15,019 B）

反竞品身份脱敏（`desensitizeAgentIdentity`）的期望向量：**55 条用例**，每条是
`{name, note, in, out}`——`in` 是请求体原文，`out` 是 Go 真函数的返回字节
（`null` = Go 返回 `nil`，即不改写）。26 条命中改写 / 29 条返回 `nil`。

门禁在 `src/reqshape.rs` 的单测 `golden_go_desensitize_vectors`（`cargo test` 默认跑），
不做归一化，直接比对整段字节。覆盖：命中与未命中、user 消息、tools 描述、
分段数组（含非对象段与 `text` 非字符串）、大小写与空白容忍、冠词形式、
`<`/`>`/`&` 三处 HTML 转义、原始数字形态（`1.50`/大整数/`1e3`）、键序重排、
深嵌套 200 层、以及 15 类非法/边缘输入（非法 JSON / `null` / `messages` 非数组 /
`role` 非字符串 / 消息非对象 …）。

**再生成**：采集器源码留档 `.work/p8d3/zz_p8d3_collect_test.go`（gitignore，不进库）。
拷回 `internal/tuanjie/` → 跑 `TestZZCollectP8d3Golden`（输出路径由 `P8D3_GOLDEN_OUT` 给）
→ **立即删除** → 把产物复制回本目录。采集器只调真函数，不读凭据，合成请求体。
完整命令与实测输出见 `../../README.md` 的「P8d-3」节。
