# rs-tuanjie —— 团结（Codely）插件 Rust 重写

绞杀者路线 **P8a 小轮：凭证链与请求整形**。逐字节对位 `internal/tuanjie/client.go` +
`reqshape.go`（**Go 侧只读不改**）。本小轮**不含**服务与转发编排（`server.go` /
`handlers_extra.go` 等 = P8b）与种子卫兵/注水/基线/CDP（= P8c）。

## uTLS 路线取证结论（2026-10-16 实测；P8a 开工前必须先定的那一条）

Go 侧 `utls_transport.go` 用 `utls.HelloChrome_Auto` 伪装 TLS 指纹，且**每条连接都会置换
扩展序、随机化 GREASE 值与 ECH padding**——所以「逐字节复刻某一次 ClientHello」在物理上
不可能（Go 自己两次连接也不一样）。可比的口径是**密套件集合与顺序、扩展集合、key_share
组、ALPN、JA4 载荷哈希**。两条候选路线都抓了真实 ClientHello 做对照（原始字节与解析器已
入库：`tests/golden/tls/`）：

| 维度 | Go = utls `HelloChrome_Auto` + ALPN 改写 | (a) rustls 默认栈 | (b) `wreq` + Chrome 模拟（BoringSSL） |
|---|---|---|---|
| ClientHello 字节 | 1796 | 257 | 1762 |
| 密套件 | 16：GREASE + TLS1.3 三件套 + 9 ECDHE 系 + 4 RSA 系 | 10，**无 GREASE**、顺序不同 | **16，逐项同序** |
| 扩展 | 18（含 `0xfe0d` ECH padding 250B、`0x44cd` ALPS、`0xff01`、`0x1b`、`0x12`） | 11，**上述全无** | **17，集合相同**（含上述全部） |
| key_share | `GREASE + x25519mlkem768 + x25519`（1263B） | 仅 `x25519`（38B） | 同 Go（1263B） |
| 签名算法 | 8 项 | 4 项 | **8 项逐项相同** |
| ALPN | `http/1.1` | `http/1.1`（可配） | `http/1.1`（`http1_only`） |
| JA4 载荷哈希 | `4560fe214f0b_14c5` | `8cbec4ae9a62_418b`（**不同**） | `4560fe214f0b_14c5`（**相同**） |
| 扩展序 | 逐连接随机置换 | 固定 | 逐连接随机置换（同 Go） |

**结论：路线取 (b)** —— `wreq` + `wreq-util` 的 Chrome 模拟（BoringSSL）才是 Rust 侧的
uTLS 对等物，指纹轴与 Go 版同源；rustls（(a)）在指纹轴上**一眼可辨不是浏览器**。

> 口径说明（免得后人误读表里的数字）：Go 与 rustls 两份抓包都是**打域名**（SNI 在场，扩展数
> 含 SNI 那一项）；wreq 那份是**打 IP**（无 SNI，故扩展 17 = Go 的 18 − SNI，JA4 前缀 `i` 而非
> `d`）。差异**只在 SNI 这一项**，与目标地址写法有关、与 TLS 栈无关；JA4 的载荷哈希
> （密套件哈希 `4560fe214f0b` + 签名算法哈希 `14c5`）与扩展集合的比对不受影响。

**代价（实测）**：(b) 需要本机具备 `cmake` + **NASM** + **libclang**（bindgen）+ MSVC 才能
编译 BoringSSL——本机原本两者都缺（先报 `No CMAKE_ASM_NASM_COMPILER`，补便携 NASM 后推进到
bindgen 报 `Unable to find libclang`，补 libclang 后编译通过）；首次全量编译为分钟级，并新增
`wreq`/`btls` 依赖树。顺带收益：wreq 保留 HTTP/1 **头名大小写**，能消掉下面的差异 2。

**本小轮的选择与边界**：P8a 的公开面是**凭证链**（签名/UA/路径/头集合/体重排），出站 TLS 属
P8 生态级红线、跨 P8a~P8c 的面；且 P8a 验收要求 `cargo test` 在**不预置 NASM/libclang 的裸
环境**可复跑。故本轮交付 rustls 直连（**差异 1 已登记**），把 (b) 的取证产物入库，并把替换点
收敛到一处：`src/transport.rs::build_http_client`（约 30 行）。**P8b 接真实流量前必须换成 (b)。**

复现解析（同一解析器跑两侧原始字节）：
`node rs-tuanjie/tests/golden/tls/parse-clienthello.mjs <任一 .bin>`

## 八项红线对照（Go 行号 ↔ Rust 位置 ↔ 门禁）

| # | 红线 | Go 位置 | Rust 位置 | 门禁断言 |
|---|---|---|---|---|
| 1 | 种子 hex 逐字 `406f00f7…0e98018` | `client.go:44` | `src/sign.rs:18` | `golden_p8a::seed_and_kdf_intermediate_match_go` |
| 2 | `HMAC(seed,"codely-signing-v1") → HMAC(k1, cli_api_key)` | `client.go:311-315` | `src/sign.rs:40-44` | 同上 + 253 条向量里的 `signing_key_hex` |
| 3 | 签名体 `["v1",path,ts].join("\n")` → `v1.<ts>.<base64url>` | `client.go:319-324` | `src/sign.rs:50-58` | `sign_vectors_match_go_byte_for_byte`（253 条） |
| 4 | `x-litellm-session-id` = 体 `litellm_session_id` = `prompt_cache_key` | `client.go:504`、`reqshape.go:175-180` | `src/session.rs`、`src/headers.rs:28,39`、`src/reqshape.rs:270-285` | `forward_wire_matches_go_body_and_header_set`（三元组同值）+ 21 条重排向量 |
| 5 | 换 key 路径 `codely.tuanjie.cn/api/api-token/cli-api-key`（不换源、不带 `?teamId`） | `client.go:37` | `src/lib.rs:39`、`src/client.rs:94` | `endpoints_and_timeouts_match_go` + `key_fetch_hits_red_line_five_path_with_go_header_set`（线上实抓请求行） |
| 6 | UA `codely-cli/<本机 npm 版> (win32; x64)`，读不到才 `rc.58` 兜底 | `client.go:46-90` | `src/ua.rs` | `ua_constants_and_version_regex_match_go`（含 9 条版本正则行为表） |
| 7 | **不带** `X-DashScope-*`（也不带 `x-b3-*`） | `client.go:491-492`（注释） | `src/headers.rs:33` | `no_dashscope_or_b3_headers` + 头集合对照 |
| 8 | 官方 `buildCreateParams` 字段序 + 默认字段，非流式删 `stream` | `reqshape.go:30-46,88-198` | `src/reqshape.rs:155-285` | `reshape_cases_match_go_byte_for_byte`（21 条，逐字节） |

红线 4 的取值口径：**会话三要素同值**（Go 源码用会话池，池内串行请求复用同一 id、并发/超
上限时共享或新建；`promptSeq` 每次租借 +1 进 `metadata.prompt_id`）。池参数（上限 16、
闲置 20~40 分钟、最长 180~330 分钟、逐会话随机）与 `client.go:346-480` 逐条相同。

## 已知差异（**如实登记**，不是"优化空间"）

1. **TLS 指纹（用户终裁：永久接受差异，不换栈）**：用 rustls；差的是 JA4 载荷哈希、
   GREASE、ECH padding、ALPS、后量子 key_share，以及出站 ALPN 含 h2（reqwest 开了
   `http2` 特性，Go 侧只留 `http/1.1`）。背景：官方两条轨道都是纯 Node 默认 TLS，
   Chrome 口音只存在于我们自己的 Go 反代（线上长期被容忍）；wreq+Chrome 取证已做完
   （`tests/golden/tls/`），替换点仍在 `transport.rs:38`，将来若上游按口音执法则
   Go+Rust 一起搬——在那之前不再提此事。
2. **出站头名大小写**：Go `net/http` 会做 `CanonicalMIMEHeaderKey` 规范化，线上是
   `X-Litellm-Session-Id` / `X-Codely-Signature`；hyper/reqwest 一律小写
   （`x-litellm-session-id`）。两侧实抓对照见 `tests/golden/go-outbound-wire.txt` 与
   门禁用例的 `--nocapture` 输出。**头名集合完全一致**（大小写不敏感逐项相同）。
3. **出站头序**：Go 先写 `Host`/`User-Agent`/`Content-Length`，其余自定义头**按 key 排序**，
   最后补 `Accept-Encoding: gzip`；hyper 按插入序、`Host`/`Content-Length` 收尾。
   要逐字节对齐需要换能保序的 HTTP/1 写出层（wreq 只解决大小写，不解决顺序）。
4. **`stream_options: null` + 流式**：Go 的 `json.Unmarshal(null, &map)` 不报错但把 map 解成
   nil，紧接着 `so["include_usage"]=true` 会 **panic: assignment to entry in nil map**
   （`.work/p8a/scratch` 已复现）。Rust 侧按「非对象 → 退回默认 `{"include_usage":true}`」
   处理、不 panic。要修 Go 侧须改 `internal/tuanjie/reqshape.go`（P8 只读不改）。
5. **代理自探测**：`smartProxy` 的 Windows 注册表分支属 P8c（`proxy_windows.go`）；本轮
   reqwest 客户端按默认口径走环境变量代理（与 Go 的第一步一致）。
6. **未移植面**：`server.go`/`handlers_extra.go`/`accounts.go`/`quota.go`/`providers.go`/
   `responses_forward.go`/`anthropic_forward.go`/`media.go`/`probes.go` = P8b 剩余；
   `seedcheck.go`/`water.go`/`baseline.go`/`cdp_browser.go`/`desktop_path_windows.go`/
   `proxy_windows.go` = P8c。

## P8b 切片：KIMI-K3 429 兜底集群 + 会话头 + 纯函数内核（2026-10-16）

> **范围声明（先说清）**：P8b 的目标面是约 **7,500 行 Go**（`server.go` 2,183 +
> `handlers_extra.go` 1,016 + `accounts.go` 553 + `quota.go` 279 + `providers.go` 420 +
> `responses_forward.go` 605 + `anthropic_forward.go` 608 + `media.go` 498 + `probes.go` 461 +
> `channels.go` 289 + `dist.go` 297 + `inflight.go` 176 + `pacing.go` 86 +
> `opencode_session.go` 29），一轮吃不完。本轮交付的是**有门禁的切片**，剩余面在下面第 6 条
> 逐条列出（不静默、不假装做完）。

已迁入（各有真 Go 采收的黄金对照，见 `tests/golden_p8b.rs`）：

| Rust 位置 | Go 来源 | 覆盖 |
|---|---|---|
| `src/pacing.rs` | `pacing.go` 全量 | `PACING_RETRY_DEFAULT_SECS=60` / `PACING_MAX_WAIT_SECS=1800`、`is_pacing_model`（**不分大小写**找 `KIMI`）、`retry_wait_secs`（`TrimSpace`+整数秒、>0 才采信；12 例真值表）、`Pacer` 的 `{"enabled":…}` 落盘形态 + tmp→rename + 读回、`pending` 计数；另含数据目录判定链（`PROXYDECK_DATA_DIR` → 便携标记 → `%APPDATA%\ProxyDeck\`，`appdir.go:84-98`） |
| `src/inflight.rs` | `inflight.go` 全量 | `RequestRegistry`（容量 500、FIFO 淘汰、`Finish` 摘链、耗时降序快照、`LoadOf`）、`next_rid`（12 位零填充小写十六进制，纯格式面拆出 `rid_from_n` 供门禁逐值锁死）、`ActivityLog`（环形 200、新的在前、`seq` 单调、`omitzero` 字段省略口径）、`local_hms`（Windows `GetLocalTime`，对齐 Go 本机时区） |
| `src/opencode_session.rs` | `opencode_session.go` 全量 | 进程级稳定 UUID v4、`x-opencode-session` 头、`base_url` 含 `opencode.ai` 的**大小写敏感**判定（7 例真值表） |
| `src/dist.rs` | `dist.go` 纯函数面 | `parse_dist_answer`（25 例真值表）、`dist_stats_of`（mean/median/stdDev/mode，6 例）、`compare_dist`（余弦 + JS 散度 + 众数分，4 例；含「Cosine 取 round4、jsDiv 取未取整」这条 Go 细节）、`dist_verdict`/`dist_verdict_label`、`fmt_dist_sample_progress`、`round2/round4/abs`（half-away-from-zero 口径） |
| `src/channels.rs` | `channels.go` 静态面 | 八渠道表（顺序/端口/base_url/bearer_from 逐字段）、`is_strong_channel`、`channel_name_of`、`channel_def` |

**P8b 切片未覆盖（后续小轮接手）**：

1. `server.go`：路由表与 CORS、`/health`、`/v1/models`（`mergeProviderModels`/`serveModels`/
   `staticModels`/`handleModelsFull`）、`/model/info`（`modelMeta`/`reasoningTiersFor`/`aliasNote`）、
   `/account-models`、`handleChat`（**池路径 429 兜底即时换号 ≤3 次 + 全池排队 30 分钟预算**、
   `ensureNonEmpty`、`concurrentPick`、14018 耗尽换号、非法 UTF-8 本地 400、
   `stream_options` 补齐、`noteUpstreamError/OK` 告警）、`/v1/stats`、`/quota`、`/kimi-pacing`、
   `/accounts`、`/inflight`、`/activity`、`/v1/images/generations`、`/v1/videos`、`/agnesapi`。
2. `handlers_extra.go`：账号池 HTTP 面与其余 handler。
3. `accounts.go`：账号池全量（401 标死换活号、rescan 复位、`ResumeBudgetLoop`）。
4. `quota.go`：`/quota?uid=`、缓存分键、积分 TOP。
5. `providers.go`：三协议（chat/responses/anthropic）+ 白名单兜底 + `x-opencode-session` 接线
   （本轮的 `opencode_session.rs` 是它的前置件，尚未接进转发路径）。
6. `responses_forward.go` / `anthropic_forward.go`：两条协议的转发与翻回。
7. `media.go`：识图改路由链（vision→fallback→codely-vl）+ Agnes 生图/生视频合同。
8. `probes.go`：注水探针、`max_tokens` 自适应名单、基线内存态；`dist.rs` 里
   `collect_dist_samples` 依赖的 `probe_call`/`probe_target`/`rate_limit_storm` 也在此。
9. `channels.go` 的网络面（`build_channels`/`fetch_local_models`/`channel_target`）与
   `LoadChannelKeys`/`SaveChannelKey` 的 DPAPI 落盘面。

**P8b 切片新增的已知差异（登记）**：

7. **`retry_wait_secs` 的极端量级**：Go 是 `time.Duration(secs) * time.Second`（整数溢出时结果
   可能为负），Rust 侧饱和到 `u64` 秒。真实 `Retry-After` 是秒数、不会有这个量级，故未逐位复刻。
8. **`next_rid()` 的进程级计数器**：语义与 Go 相同（+1、低 48 位零填充），但多测试并行时
   计数器起点不可复位，故门禁锁的是**纯格式面** `rid_from_n(n)`（与黄金的
   `rid_First_5` 逐值相同）而不是某次运行的绝对值。
9. **`ActivityLog` 的 `time` 字段**：Windows 走 `GetLocalTime`（与 Go `time.Now()` 同时区）；
   非 Windows 退到 UTC（本 crate 只跑 Windows，未在其它平台验证）。

## 门禁与复跑

```powershell
# Rust 侧（P8a 门禁：33 单测 + 12 集成；P8b 切片：+14 单测 + 5 集成）
cd rs-tuanjie
$env:CARGO_TARGET_DIR="E:\cargo-target\rs-tuanjie"   # F: 盘空间紧张，构建产物放 E:
cargo test
cargo build --all-targets                            # 0 warning
cargo test --test golden_p8a -- --nocapture          # 看逐条对照证据与两侧线上实抓
cargo test --test golden_p8b -- --nocapture          # P8b 切片逐条对照

# Go 侧零改动核对
cd ..
go test ./internal/tuanjie -count=1                  # 需干净 TMP（见下）
go vet ./internal/tuanjie
git status --short internal/                          # 必须为空（临时采集器跑完即删）
```

⚠ **`go test ./internal/tuanjie` 的 TMP 前提**：本机 `TMP=TEMP=E:\Temp` 里残留
`seedguard-1403084733`，`seedcheck_test.go` 会 glob `os.TempDir()/seedguard-*` 并要求删净，
于是 `TestSeedCheckRegistry` 确定性失败（与 Rust 侧无关）。复跑时把 TMP 指到干净目录即可：

```powershell
$env:TMP="<repo>\.work\p8b\tmp"; $env:TEMP=$env:TMP
go test ./internal/tuanjie -count=1     # ok
```

测试**不读任何真实凭据内容**：`oauth_creds.json` / `tuanjie-accounts.json*` / JWT 的内容
一个字节都不进测试——`fetch_key` 的 access_token 来源经 `Client::with_token_loader` 注入合成值，
出站打的是本机 `127.0.0.1` 假上游，key 用 `sk-synthetic-key`。唯一例外是
`access_token_fp()` 会对真实 `~/.codely-cli/oauth_creds.json` 做 **stat（仅 mtime+size 元数据，
不读内容、不落日志/断言/文件）**。不碰 8788 与任何现行服务端口。

基准再生成链见 `tests/golden/README.md`。
