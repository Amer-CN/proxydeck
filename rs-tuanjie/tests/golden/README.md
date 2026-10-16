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
