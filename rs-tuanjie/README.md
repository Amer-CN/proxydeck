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
| 5 | 换 key 路径 `codely.tuanjie.cn/api/api-token/cli-api-key`（不换源、不带 `?teamId`） | `client.go:37` | `src/lib.rs:29`、`src/client.rs:94` | `endpoints_and_timeouts_match_go` + `key_fetch_hits_red_line_five_path_with_go_header_set`（线上实抓请求行） |
| 6 | UA `codely-cli/<本机 npm 版> (win32; x64)`，读不到才 `rc.58` 兜底 | `client.go:46-90` | `src/ua.rs` | `ua_constants_and_version_regex_match_go`（含 9 条版本正则行为表） |
| 7 | **不带** `X-DashScope-*`（也不带 `x-b3-*`） | `client.go:491-492`（注释） | `src/headers.rs:33` | `no_dashscope_or_b3_headers` + 头集合对照 |
| 8 | 官方 `buildCreateParams` 字段序 + 默认字段，非流式删 `stream` | `reqshape.go:30-46,88-198` | `src/reqshape.rs:155-285` | `reshape_cases_match_go_byte_for_byte`（21 条，逐字节） |

红线 4 的取值口径：**会话三要素同值**（Go 源码用会话池，池内串行请求复用同一 id、并发/超
上限时共享或新建；`promptSeq` 每次租借 +1 进 `metadata.prompt_id`）。池参数（上限 16、
闲置 20~40 分钟、最长 180~330 分钟、逐会话随机）与 `client.go:346-480` 逐条相同。

## 已知差异（**如实登记**，不是"优化空间"）

1. **TLS 指纹**：本小轮用 rustls（见上）。替换点与代价见上文；差的是 JA4 载荷哈希、
   GREASE、ECH padding、ALPS、后量子 key_share。另：当前 rustls 出站 ALPN 含 h2
   （reqwest 开了 `http2` 特性），Go 侧 ALPN 只留 `http/1.1`——P8b 换 wreq 时用
   `.http1_only()` 一并消除；若换栈延后则该项暴露。
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
   `media.go`/`probes.go`/`pacing.go`… = P8b；`seedcheck.go`/`water.go`/`baseline.go`/
   `cdp_browser.go`/`desktop_path_windows.go`/`proxy_windows.go` = P8c。

## 门禁与复跑

```powershell
# Rust 侧（P8a 门禁：33 单测 + 12 集成）
cd rs-tuanjie
$env:CARGO_TARGET_DIR="E:\cargo-target\rs-tuanjie"   # F: 盘空间紧张，构建产物放 E:
cargo test
cargo test --test golden_p8a -- --nocapture          # 看逐条对照证据与两侧线上实抓

# Go 侧零改动核对
cd ..
go test ./internal/tuanjie -count=1
go vet ./internal/tuanjie
git status --short internal/                          # 必须为空（临时采集器跑完即删）
```

测试**不读任何真实凭据内容**：`oauth_creds.json` / `tuanjie-accounts.json*` / JWT 的内容
一个字节都不进测试——`fetch_key` 的 access_token 来源经 `Client::with_token_loader` 注入合成值，
出站打的是本机 `127.0.0.1` 假上游，key 用 `sk-synthetic-key`。唯一例外是
`access_token_fp()` 会对真实 `~/.codely-cli/oauth_creds.json` 做 **stat（仅 mtime+size 元数据，
不读内容、不落日志/断言/文件）**。不碰 8788 与任何现行服务端口。

基准再生成链见 `tests/golden/README.md`。
