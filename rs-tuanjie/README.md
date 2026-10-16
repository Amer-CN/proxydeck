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

## P8b 第二片：账号池（accounts）与积分（quota）（2026-10-17）

**本轮交付两块**（各有门禁：行为测试 + 对真 Go 采收基准的差分对照）：

| Rust 位置 | Go 来源 | 覆盖 |
|---|---|---|
| `src/accounts.rs` | `accounts.go` 全量（584 行） | `Account`/`AccountStatus` 字段与 JSON 形态、`AccountPool` 的 `reload`（缺字段/坏 JSON 容忍、非 local 无 token 不入池）、`save_locked`（**DPAPI 密文落盘**，明文 token 不落盘）、`pick`（enabled/非 402 → 认领过滤 → GLM 资格 → 负载最低 → 轮询；**另跳过 Dead 与冷却中的号**）、`pick_with_token`（最多试池大小次）、`mark_budget_exceeded(_at)`、`resume_scan_once`（到点复查 → 清标记 / 顺延 5 分钟）、`inc_load`/`dec_load`（钳 0）/`load_of`、`status`（JWT exp → 剩余小时、`未知` 兜底）、`add`/`remove`/`toggle`/`set_glm`/`set_models`/`sorted_user_ids`/`get`、`ensure_local_account`/`local_account`、`is_glm_model`/`jwt_expiry`/`jwt_sub`/`split_jwt` |
| ↑ 另加（服务层换号所需，Go 团结侧没有这三样） | 语义源 `internal/codebuddy/pool.go`（照抄「骨架同源」那条线） | `mark_dead`（401/403 标死，`pick` 永不再选，复活只靠 `rescan`）、`mark_limited`/`clear_cool`（429 冷却到期懒判定）、`rescan`（**Dead 复位**、冷却与 `enabled` 继承、文件已删的号出池） |
| `src/quota.rs` | `quota.go` 全量（301 行） | `Quota`/`DailyQuota`/`GiftQuota`/`PlanQuota`/`PlanWindow`/`MonthQuota`/`SourceSpend` 的字段与**逐字节 JSON 形态**（字段声明序 + `omitempty` + Go 浮点口径）、`quota_cache_key`（`sha256(token)[:8]` 十六进制）、`QuotaCache`（成功 5min / 失败负缓存 60s、命中改写 `live→cache`）、`fetch_quota_with_token`（15s 请求级超时、2MB 截断、三条错误文案）、`parse_summary`（字符串型 points → `ParseFloat` 失败取 0、`by_source` 同名合并 + 按积分降序）、`fetch_quota_cached`（`force` 旁路、失败回退旧 live 值）、`QuotaView`（`/quota` 的两条固定文案） |
| `src/crypto.rs` | `internal/appdir/crypt.go` + `crypt_windows.go` + `acl_windows.go`（副本取自 `rs-vibex/src/crypto.rs`） | `PDAPI1\n` + base64 密文的**读+写**、明文旧文件读兼容、DPAPI `CryptProtectData`/`CryptUnprotectData`（当前用户作用域）、tmp→`icacls` 收紧→rename |

**门禁（两条，都有真 Go 采收的对照）**：

* `tests/p8b_pool_behavior.rs`（**13 用例 + 1 ignored**）：合成账号 + 合成 JWT + 临时目录，
  「假上游」是确定性的状态码序列（401 / timeout / 200），锁住
  **401 标死 → 换活号 → 全死则如实透传（有界，不循环）**、**超时不标死也不换号**、
  冷却跳过与到期恢复、`rescan` 复活死号但保留停用意图、`loads`/`use_count`/`last_used`
  的内存态口径、本地账号的 token 缝、`status` 的 JWT 过期视图。
* `tests/p8b_quota_diff.rs`（3 用例）：对 `tests/golden/golden-p8b-quota-go.json`
  （真 Go 采收，见 `tests/golden/README.md`）逐例比 `source`/`err`/**响应体逐字节**/上游命中次数，
  覆盖 live → cache → force → 503 → 负缓存 → 失败回退旧值 → 坏 JSON → 视图层文案；
  纯函数段直接比缓存键、TTL 常量与请求形态（路径/Accept/UA/Host）。

## 已知差异（**如实登记**，不是"优化空间"）

> 编号 1~6 属 P8a（凭证链），7~9 属 P8b 第一片（兜底内核），10 起属 P8b 第二片（账号池/积分）。

10. **Dead/冷却标记是 Go 团结侧没有的**：`internal/tuanjie/accounts.go` 里
   没有任何「凭据失效/冷却」标记——它的 401 只是被注释提到（`server.go:461`）、402 走
   `budget_exceeded`。本片按 `internal/codebuddy/pool.go`（同源骨架）的语义补上
   `mark_dead`/`mark_limited`/`clear_cool`，作为 **server 池路径换号的地基**；
   落地时以「行为测试 + 与 P8a/P8b 既有口径一致」为准，**不是**对 `accounts.go` 的逐字复刻。
   两处命名/分层差异：① `loads` 与 Dead/冷却都只在内存里（`reload` 只重建成员、不动计数，
   重启才清零）；② 402 的 `budget_exceeded`/`budget_reset_at` **随池落盘**，故 `rescan`
   重读文件后它仍在，清它走 `toggle(uid, true)`（`accounts.go:477-479`）——与内存态的 Dead 不同层。
11. **`budget_reset_at` 的零值会被写出**：Go 的 `time.Time` 是结构体，
   `omitempty` **对它无效** ⇒ 每个账号都会写出 `"budget_reset_at":"0001-01-01T00:00:00Z"`。
   Rust 侧按 Go 实测行为照写（`marshal_accounts_indent`），门禁 `accounts_file_shape_is_go_marshal_indent`
   锁住这一行。**注意别照「omitempty = 省略零值」的直觉去改。**
12. **JSON 库的错误措辞不同**：响应体不是合法 JSON 时，Go 是
   `encoding/json`（`invalid character 'o' in literal null (expecting 'u')`），
   Rust 是 `serde_json`（`expected ident at line 1 column 2`）——两侧前缀
   （`官网积分接口响应解析失败: `）逐字一致。差分门禁按「前缀逐字 + `err` 值归一化后整体逐字节」比对，
   措辞本身不对齐（与 P5c 登记的「标准库 JSON 措辞」同族）。
13. **`by_source` 并列名次的次序**：Go 用 map 归并后 `sort.Slice` 只比 `Points`
   （**等分时次序不确定**），Rust 用 `BTreeMap` 归并 + 同分时按 label 兜底 ⇒
   同一输入下 Rust 的输出**稳定**、Go 可能每次不同。只有等分并列才可能看出差别。
14. **`Accept-Encoding: gzip` 由各自客户端补**：Go `net/http` 与 reqwest（开了
   `gzip` 特性）都会补这个头，值相同（`gzip`）；但**头序**与其余自动头（`host` 端口、
   `Connection`）仍属平台级差异（同下面第 2/3 条）。
15. **`is_glm_model` 的多字节输入**：Go 逐字节原地小写（多字节 UTF-8 会被打散成
   无效序列），Rust 同样只折 ASCII 字节 ⇒ 对「全角/非 ASCII 前缀」的判定两侧一致，
   但**都不保证**对任意 UTF-8 输入有语义意义（只服务模型名，真实取值是 ASCII）。
16. **`Quota` 的请求级超时覆盖面**：Go 的 `defer cancel()` 只罩 `client.Do`
   （拿到响应头即返回，**读体不在 15s 预算内**）；reqwest 的请求级 timeout 连读体一起罩
   （更严，不会少读）。行为上不产生差异，但极端慢读场景两侧表现可能不同。
17. **（未覆盖）`/quota` 的 HTTP 端点装配**：`handleQuota` 的 `?uid`/`?refresh=1` 解析、
   `writeJSON` 的形状与 CORS 属 server 骨架（下一小轮），本轮只把**分支文案**
   （`QuotaView::not_in_pool` / `token_error`）与缓存/拉取语义做齐。


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
3. ~~`accounts.go`：账号池全量（401 标死换活号、rescan 复位、`ResumeBudgetLoop`）。~~
   **【P8b 第二片已迁】** 见下节「P8b 第二片」（`src/accounts.rs`）。
4. ~~`quota.go`：`/quota?uid=`、缓存分键、积分 TOP。~~
   **【P8b 第二片已迁】** 见下节（`src/quota.rs`）；仅 HTTP 端点装配仍归 server 骨架。
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

**P8b 第二片新增的已知差异（登记）**：

10. **`parse_summary` 的类型不匹配处理**：Go 的 `json.Unmarshal` 遇到类型不匹配
    （如 points 给数字、`exhausted` 给字符串）**整体报错**；Rust `unwrap_or(0/false)`
    静默取零继续成功。违"如实报错优先"口径，暂未修（上游字段形态稳定，Risk 低）。
11. **JWT 解码表**：用标准 base64 表（`+/`），Go 用 RawURLEncoding（`-_`）。
    纯 ASCII payload 数学上永不产生 62/63（golden/测试 token 因此全兼容）；
    非 ASCII claims 时 Rust `jwt_sub`/`jwt_expiry` 静默失败 ⇒ 本地账号不入池。
12. **2MB 内存形状**：`quota.rs` 先 `resp.bytes()` 全量读入再截断；Go `LimitReader`
    边读边弃、峰值 2MB（rs-bai P1 同类已登记 R8，此处同理）。
13. **等分并列次序**：Go 侧 map 迭代次序本身不确定，Rust 用 label 排序兜底 = 确定输出。
14. **`AccountStatus` 经 serde 写出时 f64 写 `2.0` 而 Go 写 `2`**：server 片装配
    `/accounts` 端点时必须走 gojson 口径（Go 浮点最短表示），不得直接 `serde_json::to_string`。

## 门禁与复跑

```powershell
# Rust 侧（P8a 门禁：33 单测 + 12 集成；P8b 第一片：+14 单测 + 5 集成；
#           P8b 第二片：+13 单测 + 13 池行为 + 3 quota 差分）
cd rs-tuanjie
$env:CARGO_TARGET_DIR="E:\cargo-target\rs-tuanjie"   # F: 盘空间紧张，构建产物放 E:
cargo test                                           # 60 单测 + 12 + 5 + 13(+1 ignored) + 3 = 93 项
cargo build --all-targets                            # 0 warning
cargo test --test golden_p8a -- --nocapture          # 看逐条对照证据与两侧线上实抓
cargo test --test golden_p8b -- --nocapture          # P8b 第一片逐条对照
cargo test --test p8b_pool_behavior -- --nocapture   # 第二片：401 标死/换号/rescan/冷却
cargo test --test p8b_quota_diff -- --nocapture      # 第二片：quota 差分（含假上游请求记录）

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
