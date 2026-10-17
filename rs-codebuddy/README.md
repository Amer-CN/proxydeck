# rs-codebuddy —— CodeBuddy/WorkBuddy 插件（8787 CN + 8789 INTL）的 Rust 重写

> **P5a：合同与请求侧**；**P5b：账号池 / 凭据 / 额度 / failover**；**P5c：脱敏 / 下游 / 统计**
> （绞杀者路线第 5 阶段的三小轮）。
> 等价替换 `internal/codebuddy`，先在测试端口独立跑通，**不注册进 `app/plugins.go`**、
> 不动 Go 构建、不碰任何现行服务端口（尤其 8787/8789 上跑着的 Go 版）。
> P5c 之后本 crate 与 Go 的行为面已收口（剩余差异见第五节，全部是平台级或 Go 侧缺陷登记）。

## 一、这一轮做了什么（与 Go 的对应）

### P5a：合同与请求侧

| # | 合同 | Go 位置（`internal/codebuddy/`） | Rust 位置 |
|---|---|---|---|
| 1 | 路由表 + CORS（OPTIONS 直接 204） | `server.go:80-91` `corsWith`、`:166-190` `Start` | `src/lib.rs` `router`/`cors`/`add_cors` |
| 2 | `/health`（不看方法；status/service/backend/region/mode/auth_search_dirs/uptimeSec + credential 块或 status=error + message） | `server.go:201-228` | `src/lib.rs` `handle_health` |
| 3 | 候选池 15 条（CN）/ 15 条（INTL） | `server.go:605-612`、`region.go:79-84` | `src/region.rs` `MODEL_CANDIDATES`/`INTL_CANDIDATES` |
| 4 | `/v1/models`：每小时在线探测过滤（TTL 1h）+ 按池序输出 + 全失败回退整池；INTL `NoProbe` 零上游 | `server.go:616-672` | `src/models.rs` `models` |
| 5 | 探测本身：最小流式请求、25s、并发 10、"读到数据即可用"、INTL 强制首条 system | `server.go:676-706` | `src/models.rs` `probe_one`/`probe_all` |
| 6 | `/model/info`：`modelMeta` **只填实测值**，未命中不硬造字段、`note` 缺省补空串；INTL 空 meta | `server.go:737-783` | `src/models.rs` `meta_entries`/`handle_model_info` |
| 7 | `/v1/chat/completions` 入站解析（64MB 截断读、Go `json.Unmarshal` 逐字错误文案、`messages is required`、model 缺省 auto、stream 判定） | `server.go:1128-1149` | `src/chat.rs` `handle_chat`、`src/godecode.rs` |
| 8 | 出站体重建：20 个白名单字段 + `stream=true` + `stream_options` 缺省 + 键排序/HTML 转义的 `json.Marshal` 等价物 | `server.go:788-794, 1151-1164`、`mustJSONReader` | `src/chat.rs`、`src/gjson.rs` |
| 9 | `normalizeToolChoice`（对象式降级、无 tools 删除、怪值原样） | `server.go:912-931` | `src/toolrefs.rs` `normalize_tool_choice` |
| 10 | `sanitizeToolRefs`/`inlineSchemaRefs`（`$ref`/`$defs` 无损内联、兄弟键 2020-12 合并、循环截断、外部引用保留、无 `$ref` 原样、零残留） | `server.go:941-1015` | `src/toolrefs.rs` `sanitize_tool_refs`/`inline_schema_refs` |
| 11 | `developer` → `system`；INTL 自动补首条 system | `server.go:880-901, 1171-1191` | `src/toolrefs.rs` `normalize_roles`/`msgs_have_system`、`src/chat.rs` |
| 12 | 11133 退避重试（上限 2 次，800ms/1600ms）+ 每次落一份现场（上限 10 份、0600） | `server.go:562-573, 1316-1342`、`dumpRequestBody` `:1112-1126` | `src/chat.rs`、`src/diag.rs` `dump_request_body` |
| 13 | 11133 的 `requestFingerprint`（不含正文的可 grep 指纹，含 sha256 前 6 字节） | `server.go:1073-1108` | `src/diag.rs` `request_fingerprint` |
| 14 | 11129 的 `summarizeToolsForDiag`（只取出站 tools 与 tool_choice） | `server.go:1020-1068` | `src/diag.rs` `summarize_tools_for_diag` |
| 15 | 上游非 200 的 `writeErr` 形状（状态码 + `error.message` + `type=upstream_error` + 末尾换行） | `server.go:1770-1776` | `src/chat.rs` `write_err` |
| 16 | `ServeMux` 兜底 404 文案（`404 page not found\n` + nosniff） | Go 标准库 | `src/lib.rs` `handle_fallback` |
| 17 | 区域差异：base_url / 候选池 / 强制 system / 探测开关 / UA 常量 / 凭据过滤后缀 | `region.go` 全文 | `src/region.rs` |
| 18 | 数据目录判定链（`PROXYDECK_DATA_DIR` → 便携标记 → `%APPDATA%\ProxyDeck`） | `internal/appdir/appdir.go` | `src/datadir.rs` |

### P5b：账号池 / 凭据 / 额度 / failover

| # | 合同 | Go 位置 | Rust 位置 |
|---|---|---|---|
| 1 | `findAuthDirs` / `candidateAuthDirs`（`CODEBUDDY_AUTH_DIR` 逃生口 + 三候选目录去重） | `credential.go:213-252` | `src/credential.rs` `find_auth_dirs`/`candidate_auth_dirs` |
| 2 | `FindAuthFileForRegion`：区域过滤（`auth.domain` 后缀）+ 可用性优先（明文非空 / 信封可解密）+ 同目录 mtime 最新 + 零可用时区分「没有文件」与「全解不出来」 | `credential.go:261-316` | `src/credential.rs` `find_auth_file_for_region` |
| 3 | `authTokenUsable` / `authTokenUnusableReason`（五种短原因，逐字文案） | `credential.go:123-164` | `src/credential.rs` `auth_token_usable`/`auth_token_unusable_reason` |
| 4 | `wbToken` 形态（明文/信封/其余置空） + `flexString`（昵称是加密对象也不丢整份登录态） | `credential.go:45-98` | `src/credential.rs` `parse_session_info`/`wb_token_field`/`flex_string` |
| 5 | at-rest `$wbEncrypted` 解密：密钥链（sha256→sha256→hex[:16]）、AAD（sym-v1 framing）、AES-256-GCM | `atrest.go:75-262` | `src/atrest.rs` `derive_atrest_key`/`aad_for_key_id`/`decrypt_envelope_raw` |
| 6 | 取 key 路径纪律：`WORKBUDDY_AT_REST_SECRET` → `WORKBUDDY_ELECTRON_PATH` → 注册表卸载表（DisplayIcon / UninstallString 反推），**不拼默认安装路径** | `atrest.go:84-151`、`atrest_win.go` | `src/atrest.rs` `ensure_atrest_keys`/`electron_candidates`/`exe_from_icon`/`dir_from_uninstall` |
| 7 | 密钥缓存（进程级，成功常驻）+ 失败 60s 负缓存 | `atrest.go:64-73, 90-95` | `src/atrest.rs` `KeyCache`/`ATREST_ERR_TTL` |
| 8 | `buildHeaders`（Content-Type/Accept/Authorization/`X-User-Id`/`X-Enterprise-Id`/`X-Tenant-Id`/`X-Domain`/**区域 UA**） | `credential.go:517-533` | `src/credential.rs` `build_headers` |
| 9 | `headersFor`（**单锁罩全程**：切号 + 读 + 刷新（含网络）+ 建头都在同一把锁内；空 access 拦截；INTL `NoRefresh` 如实报错）+ token 刷新与 `$wbEncrypted` 形态保持回写 | `credential.go:538-586, 410-492` | `src/credential.rs` `headers_for`/`refresh`；行为证据 `tests/p5b_refresh.rs` |
| 10 | `Summary`（uid/昵称/企业/expires_at/expired/auth_file；无文件时报「N 个加密文件」或「未找到登录文件」） | `credential.go:589-617` | `src/credential.rs` `summary` |
| 11 | `scanAuthFiles`：同 uid 去重留 mtime 最新、按 mtime 新→旧排序、不可用文件不入池 | `pool.go:149-212` | `src/pool.rs` `scan_auth_files` |
| 12 | 池状态落盘 `codebuddy-pool(-intl).json`（**只落 Enabled/Removed**、0600、tmp+rename） | `pool.go:55-118` | `src/pool.rs` `pool_state_path`/`save_state_locked` |
| 13 | `Pick`（轮询 + 跳过冷却/死/耗尽/停用/移出）/ `Touch` / `IncLoad` / `DecLoad` | `pool.go:241-410` | `src/pool.rs` 同名方法 |
| 14 | `MarkDead`（401/403）/ `MarkLimited`（429/6004 冷却）/ `MarkExhausted`（14018）/ `ClearExhausted` / `ClearCool` | `pool.go:261-335, 524-534` | `src/pool.rs` 同名方法 |
| 15 | `Toggle` / `Remove` / `Restore`（改内存 + 全量落盘，不动 auth 文件） | `pool.go:323-369` | `src/pool.rs` 同名方法 |
| 16 | `Rescan`（继承冷却与 Enabled/Removed；Dead/Exhausted 复位） | `pool.go:497-519` | `src/pool.rs` `rescan` |
| 17 | `Snapshot` / `AccountView`（13 字段、`limited_until` 为 `time.Time` 的 MarshalJSON 形态、`last_used` 为 RFC3339 秒精度） | `pool.go:442-487` | `src/pool.rs` `snapshot`/`AccountView::to_json`、`src/gotime.rs` |
| 18 | `/v1/accounts` GET + POST 六个 action（rescan/clear_cool/clear_exhausted/toggle/remove/restore；未知 action 400、未知 uid `ok:false`、非 GET/POST 405、无池 500） | `server.go:481-532` | `src/lib.rs` `handle_accounts` |
| 19 | chat 三条换号分支（401/403 标死换活号、429 冷却换号、14018 换号）+ 200 体 14018 Peek（`bufio.Peek(64KB)`） | `server.go:1261-1423` | `src/chat.rs` `handle_chat` |
| 20 | failover 配置：`loadFailover`（缺省 `enabled:true`/`deepseek-v4-pro`/`max`；坏文件保持缺省；非法档位落回 max）/ `saveFailover` | `server.go:322-393` | `src/failover.rs` `Failover::load`/`save` |
| 21 | `/v1/failover` GET/POST（fallback 必须命中候选池否则 400；POST 响应同构带 `hy4_limit`）+ `hy4LimitState` + `hy4Limited`/`hy4Trip` | `server.go:397-467, 810-829` | `src/failover.rs`、`src/lib.rs` `handle_failover` |
| 22 | `parseQuotaReset`（CN `Quota exceeded` / CN 中文 `频率限制` / INTL `frequency limit`，UTC+8 固定时区） | `server.go:832-869` | `src/failover.rs` `parse_quota_reset` |
| 23 | `failoverDecide` + **CN 缺省 `reasoning_effort` 注入**（档位来自配置，缺省 max） | `server.go:544-590, 1223-1239` | `src/failover.rs` `failover_decide`/`reasoning_default_for`、`src/chat.rs` |
| 24 | `quota.go`：`/quota?uid=` + `refresh=1`、按区+号分键的 5 分钟缓存、失败回退**该号自己的**缓存、401/403 标死、`buildQuota` 汇总口径 | `quota.go` 全文 | `src/quota.rs`、`src/lib.rs` `handle_quota` |

### P5c：脱敏 / 下游 / 统计（生产口径等价的本轮硬门禁）

| # | 合同 | Go 位置 | Rust 位置 |
|---|---|---|---|
| 1 | **全角色脱敏** `DesensitizeText`（词表 + 第 1 字符后插 U+200B + ASCII `\b` 边界 + 大小写不敏感 + 最长优先） | `desensitize.go:18-135` | `src/desensitize.rs` `desensitize_text` |
| 2 | 指纹删除表 `fingerprintRules`（`(?i)\b(Main branch) \(you will usually use this for PRs\)` → `$1`，**组 1 保留原文大小写**） | `desensitize.go:49-65` | `src/desensitize.rs` `strip_fingerprints` |
| 3 | harness 压缩 `compactHarnessMessage`（codex/claude system 模板、permissions/skills 块、user 角色里的 harness 上下文 → 短摘要，摘要自身再过零宽） | `desensitize.go:169-203` | `src/desensitize.rs` `compact_harness_message` |
| 4 | `DesensitizeBody`：全角色（system/developer/user/assistant/tool）+ content blocks 只动 text 块 + `tools` 元数据（description/title 删除、`properties`/`$defs` 名保留、const/default/enum/examples/dependentRequired 原样克隆） | `desensitize.go:205-331` | `src/desensitize.rs` `desensitize_body`/`desensitize_tool_value` |
| 5 | `--desensitize` 放行（生产 `app/plugins.go` 对 CN/INTL 都传） | `plugins.go:42,52` | `src/lib.rs` `Args`/`AppState::production_with_data_dir` |
| 6 | `sseFieldCleaner`：逐行剔空字符串 `content`/`reasoning_content` 与空数组 `tool_calls`；不可解析行/[DONE]/非 data 行原样；跨块缓冲 + flush 冲尾；重写按 `json.Marshal` 形态 | `server.go:1544-1628` | `src/sse.rs` `SseFieldCleaner` |
| 7 | `usageScanner`：`data:` 行的 `usage` 提取入账（int64 语义、`[DONE]` 跳过） | `server.go:1503-1537` | `src/sse.rs` `UsageScanner` |
| 8 | `collectStream` 非流式聚合（content 拼接、tool_calls 按 index 合并、`arguments` 累加、`finish_reason` 缺省规则、`usage` 透传或补零、`chatcmpl-<6 字节 hex>`） | `server.go:1635-1756` | `src/collect.rs` `collect_stream` |
| 9 | 200 体 14018 判定窗口 = Go `bufio.NewReader` + `Peek(64KB)`；**不命中时 peek 到的字节一字不丢**（Go `3d4627f` 把 `br` 接回 `resp.Body`） | `server.go:1282-1332` | `src/chat.rs` `ChunkReader::peek`（`PEEK_WINDOW`） |
| 10 | `codebuddy-stats(-intl).json` 按模型累计 + **每笔立即原子落盘**（Go 无节流）+ `/v1/stats`（不看方法） | `server.go:249-317` | `src/stats.rs`、`src/lib.rs` `handle_stats` |
| 11 | `ServeMux` 路径清洗与 **307** 重定向（`//v1/models`、点段；`Location`/`Content-Type`/体逐字节） | Go 1.26.5 `net/http/server.go:2671-2696`、`func Redirect` | `src/pathclean.rs`、`src/lib.rs` `path_clean_mw` |
| 12 | 11133 退避期间的**客户端断开**（Go `r.Context().Done()` → 499，且不再打上游） | `server.go:1333-1339` | `src/conn.rs`（自定义 `Listener` + `ConnectInfo`）、`src/chat.rs` `wait_or_cancelled` |
| 13 | 11128 / 11129 两条诊断日志（文案逐字、gating 条件与截断长度同 Go） | `server.go:1306-1315` | `src/chat.rs`（`_cmp11128.mjs` 逐字比对过） |

## 二、怎么跑

```powershell
cd rs-codebuddy
$env:CARGO_TARGET_DIR="E:\cargo-target\rs-codebuddy"   # F: 盘紧张时的外置目标目录
cargo test                                            # 58 单测 + 3 P5a 黄金 + 8 合同 + 5 P5b 门禁 + 2 hy4 + 3 刷新 + 2 P5c 黄金 + 3 P5c 接线 = 84
cargo test -- --nocapture                             # 看「跳过/allowance」汇总行

# 起一个测试实例（**别用 8787/8789**）
cargo run -- --port 18786 --region INTL --desensitize
```

> ⚠ 本机环境注意（若出现 `could not execute process … (os error 5)`）：火绒（`HipsDaemon`）
> 会在链接完成后**秒删** `E:\cargo-target` 下新生成的测试 `.exe`，`cargo test` 于是报
> 「拒绝访问」。绕过办法：`cargo test --no-run` 之后**立刻**把二进制拷到工作区再跑
> （`.work/rust-rewrite/_cbtest.ps1` 就是干这个的，跑的是**链接产物的逐字节副本**）。

参数：`--plugin-codebuddy`（占位，与 Go 侧同名）、`--port`、`--host`、`--region CN|INTL`、
`--upstream-base`（对照/回放用）、`--cred-stub <json>`（测试缝，见第四节）、`--auth-dir`、
`--diag-dir`、`--pool-dir`（后三个都是对照用覆盖；默认走 `CODEBUDDY_AUTH_DIR` /
`appdir::data_dir()`）。
`--desensitize` **传了就拒绝启动**（属 P5c，不静默忽略）。

## 三、门禁与证据

* **P5a 黄金门禁** `tests/golden_codebuddy.rs`：纯函数 47 条 + `scope=P5a` 的 12 个场景 / 56 步
  逐字节对照（比 **状态码 + 响应头白名单 + 响应体 + 上游看到的请求体与头**）。
  （纯函数段含 `pure.collect_eof` 5 条：**EOF 残行**合同，真 Go `collectStream` 采收，见差异 42。）
* **P5b 黄金门禁** `tests/p5b_golden.rs`：`scope=P5b` 的 **15 个场景 / 66 步**对照
  （比状态码 + 响应头 + 响应体 + 上游请求 + **每号命中数** + **池状态/failover 落盘内容**
  + **auth 文件回放后字节不变**）。响应体与落盘文件按**键序无关一致**比——Go 侧那几处是
  **结构体声明序**，Rust 侧一律键序（`gjson::to_go` 等价 `map[string]any` 的语义），
  两侧都按 Go 的 `json.Marshal` 规则重序列化后比「字段集合 + 取值 + 数组顺序」（见差异 22）。
  另有：
  * **`modelMeta` 全表逐键门禁**（35 个键，含候选池外的 `deepseek-v3.2` 等）；
  * **区域配置与候选池**逐字段门禁（CN/INTL 的 base_url / UA / force_system / no_refresh / no_probe）。
  * 基准与再生成链见 `tests/golden/README.md`。
* **HTTP 合同** `tests/http_contract.rs`：路由边界（404/405/CORS/尾斜杠）、
  `/v1/stats`（P5c）仍 501、P5b 三条端点已放开（`/quota` 不再 501、无池 `/v1/accounts` 500、
  `/v1/failover` **生产缺省** `enabled:true`）、`//v1/models` 的**已知差异对照**（Rust 404 / Go 307）、
  HEAD 的 `content-length` 自洽、**>2MB 请求体必须逐字节到上游**（防退回 axum 的 2MB 默认上限）、
  凭据层报错时 `/health` 如实报错、`--desensitize` 拒绝启动。
* **刷新链与并发** `tests/p5b_refresh.rs`（P5b 修正轮补）：近过期夹具 + 假刷新端点，
  覆盖「明文 / `$wbEncrypted` 信封」两种形态的回写（**解密后明文 + 键集合**比对，
  nonce 随机不能逐字节比）；两线程异号 `headers_for` 的确定性对撞 + 持续交错，
  断言「传进去哪个文件就只拿哪个号的 token、且没有任何跨号读/写」。
* **hy4 trip 守卫** `tests/p5b_hy4_guard.rs`（P5b 修正轮补）：11133 退避重试之后那一发
  429 **不许**触发模型级兜底（`attempt` 是轮次、每轮自增），对照组证明首轮 429 照常 trip。
* **P5c 黄金门禁** `tests/p5c_golden.rs`：`scope=P5c` 的 **8 个场景 / 27 步**——脱敏（CN 全角色 +
  INTL）、SSE 逐行清洗、非流式聚合、统计入账与落盘、路径清洗、积分失败措辞。比的是
  状态码 + 响应头白名单 + 响应体 + **上游看到的请求体与头**（脱敏后逐字节） + 统计文件。
  其中 **3 步是脱敏硬门禁**（`desensitize=true` 的生产装配下出站体逐字节比）。
  另有 `stats_malformed_shapes_match_go`（P5c-b 补、P5c-c 更正判据）：把 11 例畸形/边界 `codebuddy-stats.json`
  形态喂给生产装配的 HTTP 栈，与真 Go（**生产构造路径**）采收的期望值比
  `/v1/stats` 与落盘内容（差异 35 的依据），实测**11 例全部逐字节一致、panic 0 例**。
* **P5c 行为级接线** `tests/p5c_wiring.rs`：
  * chat 路径的 `Inc/DecLoad` —— 请求进行中读 `/v1/accounts` 断言 `inflight=1`、结束后断言
    `=0`；换号（401→活号）后断言**两个号的 inflight 都归零**。**这条测试当场抓出真 bug**：
    原先只有换号分支 `dec_load`，正常路径一个都没释放（Go 用 `defer`），`inflight` 永久 +1
    ——P5b 门禁把 `inflight` 归一成 `__INFLIGHT__`（差异 18）所以一直没暴露；
  * 499 取消信号 —— 退避期间断开连接后**不再打上游**（与 Go 采收的 `meta.p5c_cancel` 比对：
    带取消 1 次命中 / 不带取消 3 次）。
* **P5c 关闭的历史跳过**：P5a/P5b 期间非流式 200 步标着 `body_compare: "outbound-only"`
  （下游留给 P5c），本轮把 **8 处标记全部摘掉**，这些步的响应体现在也逐字节比
  （聚合体里的 `created` / `chatcmpl-<hex>` 归一成标记，其余字节照比）。
* **反向验证**：`.work/rust-rewrite/_mutation-check-codebuddy.ps1`（P5a 六条突变）。
  P5b 的突变清单见第七节；P5c 的反向验证见第八节。

### P5b 门禁怎么采的（关键：**生产装配**，不是零值 Server）

P5a 最初那版黄金是**零值 Server**采的，于是「生产缺省 `reasoning_effort` 注入」这类行为
不出现在基准里（生产缺省是 `enabled:true` / `deepseek-v4-pro` / `max`）。P5b 把两个采集器
都改成走**生产装配** `NewServerForRegion`：真凭据层（`RealCredential`）+ 真账号池
（`NewAccountPool`）+ 真 failover 配置；登录态是 `CODEBUDDY_AUTH_DIR` 指向的临时目录里的
**合成假文件**（含明文与 `$wbEncrypted` 两种，secret 由 `WORKBUDDY_AT_REST_SECRET` 注入），
上游是**进程内假上游**（自写 `RoundTripper`，不出网、不占端口）。真实
`workbuddy-desktop*.info` 一个字节都没读。

## 四、凭据层（P5b 已落地）

```rust
pub trait CredSource {                              // src/cred.rs
    fn auth_dirs(&self) -> Vec<String>;             // Go findAuthDirs()
    fn summary(&self) -> Option<CredSummary>;       // Go Credential.Summary()
    fn headers(&self) -> Result<HeaderMap, String>; // Go Credential.Headers(ctx)
    fn headers_for(&self, path: &str) -> Result<HeaderMap, String>; // Go Credential.HeadersFor(ctx, path)
}
```

* **生产** = `RealCredential`（`src/credential.rs`）：区域过滤 / at-rest 解密 / token 刷新 /
  形态保持回写 / `Summary`。`AppState::production_with_data_dir` 就是 Go `NewServerForRegion`
  的等价物（装配失败给同款文案，**不许跨区域回退**）。
* **`headers_for` 是一把锁罩全程**（`credential.rs` 顶部注释 + 差异 23）：切文件 → 读 →
  必要时刷新（**含网络段**）→ 建头，全在同一把 `Mutex` 内，与 Go `credential.go:552-586`
  逐条同款。刷新走「一次性 OS 线程 + 线程内 async 客户端」——因为
  `reqwest::blocking::Client` 在 async handler 内首调**必 panic**（实测证据见
  `tests/p5b_refresh.rs` 的注释与 `Cargo.toml` 的 `reqwest` 行）。
* 测试缝 `StubCredential`（`--cred-stub <json>`）：仍保留，供活体对照喂固定头。

## 五、已知差异（逐条，数字编号供引用）

1. ~~**下游 200 响应仍是最小透传**~~ **【P5c 已关闭】** 流式过 `sseFieldCleaner`（逐字节同 Go：
   不可改行原样、改过的帧按 `json.Marshal` 形态重写、残行走 flush），非流式走 `collectStream`
   聚合。P5a/P5b 里那 8 步 `outbound-only` 的跳过标记已全部摘掉，响应体现在逐字节比。
2. **TLS 指纹**：Rust 无 uTLS 对等物（沿用 P0 起的既有登记口径）。
3. **头序/头名大小写/默认头**：hyper 发出的头名是小写（Go 会规范化成 `X-User-Id`）；reqwest 不会
   自动补 Go `net/http` 那些默认头。**值**逐字节一致（门禁比的是值，头名大小写不敏感）。
4. ~~**脱敏未实现（且生产默认开着）**~~ **【P5c 已关闭】** `--desensitize` 已放行并接线到
   `AppState.desensitize`；黄金里 `p5c-desensitize-cn/-intl` 两场景就是 `desensitize=true` 的
   生产装配，**出站请求体逐字节与 Go 一致**（3 步硬门禁）。
5. ~~**`/v1/stats` 仍是 501**~~ **【P5c 已关闭】** 现为真实应答（按模型累计 + `uptimeSec`），
   `codebuddy-stats(-intl).json` 每笔 `addStat` 立即原子落盘（Go 无节流，见差异 34），
   落盘内容与 Go 键序无关一致。
6. **`inflight` 的计数在流式路径上不递减**：Go 的流式分支读完流即 `return`，`handleChat` 的
   `defer DecLoad` 仍然会跑；Rust 侧用 Drop 守卫（`LoadRelease`）等价，**流式path 随响应流带走**，
   流结束才释放。但**门禁里 `/v1/accounts` 快照与请求的采样时刻有关**（见差异 18），
   故该字段不参与逐字节比对；另有 `tests/p5c_wiring.rs` 的行为级接线测试守着。
7. **`Ref` 内联的 `$defs` 删除时机**：与 Go 一致（`sanitizeToolRefs` 每个工具内联后立即删
   `$defs`），但 Go 在 `inlineSchemaRefs` 里对**非 object/array** 节点直接返回原值——Rust 同。
   无差异，登记在此是为了说明「$ref 相关行为已逐字对齐」。
8. **诊断目录的解析时机**：Go 每次落盘现读 `appdir.DataDir()`；Rust 启动时解析一次
   （`--diag-dir` 可覆盖）。差异只在运行中改环境变量时可见。
9. **探测的并发与超时形状**：并发上限（10）与单条超时（25s）同 Go，但 Go 是一个 `context` 罩住
   整次请求，Rust 用两个 `timeout` 分别罩「发送」与「首个数据块」；上游命中的**先后顺序**两侧不同
   （门禁对探测场景按 body 排序比对）。
10. **`probeOne` 的非 EOF 读错误视为「可用」**：这是 Go 的原逻辑（只把 `EOF` 当失败），Rust 逐字照抄。
11. **`HEAD /health` 的 `content-length` 未与 Go 比对**：黄金里那个数字是占位符本地化**之前**的体长。
12. ~~**路径清洗**~~ **【P5c 已对齐】** 复刻 Go 1.26.5 `ServeMux` 的 `cleanPath` + 307 重定向
    （含 `Location` 的 `hexEscapeNonASCII`、GET 才写字面体 `<a href="…">Temporary Redirect</a>.\n\n`、
    POST/HEAD 无体无 `Content-Type`）。`http_contract::double_slash_path_redirects_like_go_serve_mux`
    + P5c 黄金 `p5c-paths` 场景（4 步，含裸 socket 发的点段）各钉一次。
13. ~~**11133 退避期间的客户端取消（499）**~~ **【P5c 已对齐】** 不引 `hyper_util`、也不自写
    accept 循环：用**文档化的扩展点**拼出同一信号——自定义 `axum::serve::Listener` 把每个连接
    包进 `WatchedIo`（`poll_read` 读到 EOF/错误即点亮 `watch`），再 `impl Connected<IncomingStream<…>>`
    经 `into_make_service_with_connect_info` 把**本连接**的信号塞进请求扩展（`src/conn.rs`）。
    handler 用 `tokio::select!` 等它 ⇒ 退避期间断开立刻回 499 且**不再打上游**。
    实测（`tests/p5c_wiring.rs`）：断开时上游命中 1 次、不断开 3 次，与真 Go 采收值一致
    （黄金 `meta.p5c_cancel`）。**代价**：accept 路径换成自定义 `Listener`（生产与测试同一条，
    测试脚手架也走 `rs_codebuddy::serve`）；HTTP/2 与 upgrade 仍由 `axum::serve` 内部处理，未改。
14. **`datadir.rs` / `credential.rs` 读进程级环境变量**：测试并行时互扰，故门禁用
    `common::EnvGuard` 在**进程锁**内改/查/复位（`env_lock()`）；生产路径不会并发改环境变量。
15. **门禁的路径形态归一**：`/health` 的 `auth_search_dirs` / `auth_file` 与
    `credentials-missing` 的报错文案都带**绝对路径**，黄金里是 `__AUTHDIR__` / `__DATA__`
    占位符；回放时两侧都做「占位符还原 + `\\` 折成 `\`」。只动绝对路径的转义层数，
    不动任何业务字节。
16. **标准库 JSON 语法错的措辞**：Go `encoding/json` 与 `serde_json` 对同一份坏输入的用词不同
    （`unexpected end of JSON input` vs `EOF while parsing a value at line 1 column 1`），
    行/列也只有后者有。门禁只归一**错误文案这一层**（前缀 `bad json: ` /
    `auth 文件格式错误: ` 与状态码照比），其余 `godecode` 已逐字复刻 Go scanner 的文案。
17. **`last_used` 的取值**：墙钟时间（两侧运行时刻必然不同），门禁归一成 `__LAST_USED__`；
    「从未使用 = 空串」这条区分保留。
18. **`inflight` 的取值**：内存态计数，同一快照在两侧的**采样时刻**不同（Go 采集侧的
    `/v1/accounts` 在请求处理中发起、被挑中的号此刻 `inflight=1`；Rust 回放侧请求已结束）。
    门禁归一成 `__INFLIGHT__`，Inc/Dec 的配对与钳位另有
    `pool.rs::tests::inflight_inc_dec_pairs_and_clamps` 守着。
19. **积分请求里的墙钟时间**：`PackageStartTimeRangeEnd` = `time.Now().Format("2006-01-02 15:04:05")`
    （Go 与 Rust 同式），门禁把具体时刻归一成 `__NOW__`，字段名与格式照比。
20. **`reasoning_default_for` 的返回类型**：Go 是 `string`（空串 = 不注入），Rust 是 `Option<String>`
    （`None` = 不注入）——语义同，形态差，仅内部实现。
21. **`$wbEncrypted` 信封字节随构建单元漂移**：`atrest.rs` 用 `json!` + `to_vec` 组装，
    单独构建（BTreeMap 字典序）与 workspace 构建（IndexMap 书写序）字节不同
    （P7d-2 feature 并集实测）。无门禁锁信封字节（nonce 随机本来就不锁），
    消费方按键名解析、不破坏互通；与 rs-bai 同款问题（那边已显式排键），
    这里登记不断言——P7d-2 审查 #2。
22. **未覆盖**：64MB 截断边界；`WorkBuddy` 桌面端真实 exe 的注册表发现（只做了纯函数级对照）；
    DPAPI 路径（at-rest 用的是合成 secret，没碰真机 DPAPI 密文）。

### P5b 修正轮（2026-10-15，审查驳回后）新增登记

22. **三处「结构体声明序 vs 键序」分歧**（功能等价、**字节序不同**；门禁按**键序无关一致**比）：
    Go 侧这三处都是 `json.Marshal(结构体)` ⇒ 按**字段声明序**输出，Rust 侧一律走
    `gjson::to_go`（等价 `map[string]any`）⇒ 按**键字母序**输出。三处分别是
    ① 池状态落盘 `poolState`（Go `{"version":1,"accounts":…}` / Rust `{"accounts":…,"version":1}`）；
    ② failover 落盘 `failoverCfg`（Go `enabled,fallback,default_reasoning` / Rust 字母序）；
    ③ `/v1/accounts` 的 `AccountView` 与 `/quota` 的 `Quota` 响应体（Go 字段声明序 / Rust 键序）。
    这不是品味差异而是**可观测的字节差**（`cat` 那两个 json 文件就能看见），之所以不算行为分歧：
    两侧都是合法 JSON、字段集合与取值逐项相同、解析方（GUI / 桌面端）按键取值。
    复刻它需要在 Rust 侧引入「有序字段表」的序列化器，**本轮不做**（登记待后续决定）。
23. **`headers_for` 目前是单锁罩全程**（= Go 语义；见第四节）。代价是**刷新时的网络 IO 也在锁内**，
    并发请求会排队——这是 Go 本来的行为（`credential.go:544-546` 明确警告拆锁会串号），
    不是本实现的取舍。
24. **刷新与形态保持回写原先无对照**（本轮补齐）：`tests/p5b_refresh.rs` 用合成近过期夹具 +
    假刷新端点覆盖了明文与 `$wbEncrypted` 两种形态。**仍未覆盖**：真实桌面端写的密文
    （用合成 secret，与差异 21 同一条边界）、`/v2/plugin/auth/token/refresh` 的真实上游响应形态。
25. **刷新路径的 HTTP 客户端形态**：Go 用 `&http.Client{Timeout: 20s}`；Rust 用
    「一次性 OS 线程 + 线程内 current-thread runtime + async 客户端」（20s 请求超时 + 25s
    接收超时兜底）。为什么不用 `reqwest::blocking`：它在 async handler 内首调必 panic
    （实测原文见 `tests/p5b_refresh.rs` 注释）。**行为等价**（同步返回、超时口径相同），
    代价是每次刷新多一个线程。
26. **`quota` 的 15s 超时只罩到 `client.Do`/`req.send()` 为止**（两侧同款）：Go 的
    `context.WithTimeout` + `defer cancel()` 也只罩住 `client.Do`，随后的
    `io.ReadAll(LimitReader(resp.Body, 2MB))` 在 context 之外；Rust 是
    `timeout(QUOTA_TIMEOUT, req.send())` + 之后的 `read_capped` 同样不在预算内。
    即「响应头到手后读体挂住」两侧都会超出 15s，**不是分歧**，登记在此是为了说明
    15s 不是端到端预算（要端到端得把读体也纳入）。
27. **`CycleEndTime` 为 date-only（如 `"2026-01-01"`）时的取舍不同**：Go
    `time.ParseInLocation("2006-01-02 15:04:05", …)` 对短形态**报错跳过**；Rust
    `parse_local_datetime` 会把秒位补成 `00:00:00` 解析成功 ⇒ 该包**计入**总剩余/包数
    （未过期时）。真实上游给的是完整 `YYYY-MM-DD HH:MM:SS`，只在构造输入下可见。
28. **时区可移植性**：`parseQuotaReset` 固定按 **UTC+8** 解释（与 Go
    `time.FixedZone("UTC+8", 8*3600)` 同款，见差异 22 的第 2 轮取证）；而
    `format_full_display` / `format_reset_display` / `buildQuota` / `parse_local_datetime`
    用的是 **本机时区**（`gotime::local_offset_secs()`）——Go 这几处用 `time.Local`，
    故在**非 UTC+8 的机器上两侧的日志/展示字符串会不同**。本机（CST）一致；跨时区部署前要改成固定偏移。
29. **`quota` 解析失败的措辞不归一**：Go 透传 `encoding/json` 的原文
    （`官方积分接口响应解析失败: invalid character …`），Rust 走 `serde_json`
    （同一句前缀 + 库自己的措辞）。与差异 16 同类，门禁只归一库措辞那一层。
30. **`credential.rs` 里原先那条「eprintln」注释已改为「println」**：实现是 `println!`
    （Go 的 `log.Printf` 写到 stderr，本 crate 统一用 stdout，与 P5a 起的口径一致）。
31. **`hy4 trip` 日志的时间精度已对齐**：Go `server.go:1441` 用 `d.until.Format("01-02 15:04")`
    （不到秒），而 `MarkLimited` / 换号日志（`pool.go:267`、`server.go:1413`）用
    `"01-02 15:04:05"`。Rust 原先三处共用一个秒精度格式，现拆出
    `format_reset_display_minute` 专供 trip 日志。
32. **刷新路径上三处错误文案/行为的形态差**（与差异 22 一族，本轮**登记不对齐**，逐条写清）：
    ① **非 JSON 的刷新响应**：Go 的 `json.Unmarshal` 失败时结构体是零值 ⇒
    `刷新 token 失败(code=0): `（**尾巴为空**，因为 `len(out.Data)==0` 与解析失败同一分支）；
    Rust 把整份响应体截 200 字当尾巴 ⇒ 同前缀 + **带响应体尾巴**（信息更多、非逐字同源）。
    ② **`data` 字段**：Go 用 `json.RawMessage` ⇒ 报错文案里夹的是**上游原字节**；Rust 把
    `data` 解析成 `Value` 后再序列化 ⇒ 文案里是**按键序重排**的形态。两者只在
    「刷新失败且 `data` 是对象」时可见。
    ③ **`data:null`（如 `{"code":0,"data":null}`）**：Go 侧 `len(out.Data)==4` 曾逃过校验，
    随后把 `"null"` 解进 `map[string]any` 得到 **nil map**，`credential.go:448` 的
    `newAuth["lastRefreshTime"] = …` 对 nil map 赋值 **panic**（进程级致命）。
    **【已修复】Go 于 `462d377`（`fix(codebuddy): 刷新响应 data:null 不再 panic`）把判定补成
    `… || len(out.Data) == 0 || string(out.Data) == "null"`**，与该分支已有的「无数据」形态
    合并为同一条错误 ⇒ 静态报 `刷新 token 失败(code=0): null`。**Rust 已对齐该修复后的行为**
    （`credential.rs` 用 `data_present` 区分「缺键」与「显式 null」：缺键尾巴为空、显式 null
    尾巴是 `null`；两条分支都在回写之前返回，不改登录文件）。
    行为级证据：`tests/p5b_refresh.rs::refresh_data_null_and_missing_data_report_like_go`
    —— 显式 `null` 与「缺 `data` 键」两例的文案逐字同 Go，且各只打一发刷新请求、报错路径不改写文件。

### P5c 新增登记（2026-10-16）

33. **【Go 侧缺陷：已由 Go 修复，Rust 已对齐】** 非流式 + 有池时的 14018 判定**曾吃掉上游体头部**：
    修复前 `server.go` 写的是 `br := bufio.NewReader(resp.Body); br.Peek(64*1024)`，但
    `bufio.NewReader` 的默认缓冲是 **4096** ⇒ `Peek` 返回 `ErrBufferFull`、**最多消费 4096 字节**；
    而 `br` 之后**没有回填** `resp.Body`，那 4096 字节被丢弃，随后的 `collectStream(resp.Body)`
    读到的是**从第 4097 字节开始**的流 ⇒ 下游 `"content":null` + usage 全 0。
    **修复点（`3d4627f`，用户授权）**：`break` 前补
    `resp.Body = struct{ io.Reader; io.Closer }{br, resp.Body}`（缓冲与未读部分一起被读到）；
    `handleStats`/`loadStats` 无关联动见差异 35。
    **Rust 侧已对齐**：`src/chat.rs` 的 14018 判定改成 `ChunkReader::peek(PEEK_WINDOW)`——
    peek 到的字节**留在 `pending` 里一字不丢**，后续聚合照读（此前的 `take(4096)` + 丢弃那套
    是复刻缺陷的，已删）。门禁锁在 `p5c-nonstream-peek-large #0`（6.3KB 上游体 → 下游必须是
    真实 content 6000 字节，而非 `content:null`）与 `chat::tests::peek_keeps_every_byte_for_the_aggregation`
    （peek 之后下游字节逐字节等于上游；**改回丢弃式实现本用例判红**，实测 `left: []`）。
    黄金一侧的具体形状见下面「P5c-b」那节。
34. **`codebuddy-stats.json` 每笔都落盘（**没有节流**）**：简报与本 README 早前写的「节流落盘」
    在本仓 Go 里**不存在**（`addStat` → `saveStats` 直落，`internal/proxy/stats.go` 的 ≥2s 节流
    属主代理）。本实现如实对齐 Go 现状，**不额外加节流**（同 rs-bai/P1 的口径）。
35. **【Go 侧缺陷：已随 `3d4627f` 一并修好，Rust 已对齐；无残留分歧】** 统计文件内容为 JSON `null`
    （或顶层数组/字符串/坏 JSON/文件缺失）时的两段行为：
    * **装载段（`loadStats` + `handleStats`）：Go 已修、Rust 已对齐。**
      修复前 Go 会把 `s.stats` 换成 **nil map**（`null`）或保持 nil，写 nil map / 解 nil 指针
      都会 panic；`3d4627f` 的修法是「解析失败保持初始空表、`null` 时 `m == nil` 不赋值、
      条目级 `{"m":null}` 装载时 `delete` 剔除、`handleStats` 遍历时跳过 nil 条目」。
      Rust 侧逐条对齐：非法形态一律**保持空表**（不 panic）、条目级 `null` **跳过不插入**
      （此前按零值条目插进去，是真实分歧，本轮改正）。
    * **写出段（第一发 `addStat`）：Go 同样不再 panic —— 已随 `3d4627f` 一并修好**（更正 2026-10-16，P5c-c）。
      生产构造路径（`NewServerForRegion`，`server.go:131` 的 `stats: map[string]*modelStat{}`）下，
      上述任一形态都让 `s.stats` 保持那张**非 nil 空表**、`addStat` 照常累计并落盘；
      Rust 侧同（「非法形态 → 空表」），两侧 11 例逐字节一致。
    * **⚠ P5c-b 曾把写出段记成「Go 仍 panic」（5/11 例），那条判据是错的，根因是探针构造**：
      当时的采集器写的是 `s := &Server{statsPath: p}`——**零值构造**，`stats` 字段没初始化 = nil map；
      而 `loadStats` 在「`null` / 解析失败 / 读失败」三条路径上**都不赋值**，于是 nil 被保留、一写就 panic。
      生产装配里 `stats` 一开始就是空表，同样的路径保留的是空表 ⇒ 不 panic。
      **实测（P5c-c）**：把两种构造并列跑同一份修复后的 Go 源码——
      production 变体 **11/11 无 panic**；零值构造变体 **5/11 panic**（`null` / `top-array` /
      `top-string` / `bad-json` / `missing-file`），且与 P5c-b 的旧基准**逐字段一致**
      ⇒ 那 5 例纯粹是探针构造造成的假象，**不是 Go 的残留缺陷，也不是跑了旧二进制**
      （`3d4627f` 提交于 12:50，P5c-b 采集器 13:50 才跑，工作树里就是修复后的源码）。
    * **期望值来源**：真 Go 采收 `.work/rust-rewrite/_p5cc-stats-go.json`（探针
      `internal/codebuddy/zz_p5cc_stats_probe_test.go`，跑完已删、原样留档 `.work/rust-rewrite/`），
      其中的 `production` 变体；P5c-b 的旧基准 `.work/rust-rewrite/_p5cb-stats-go.json`（零值构造）
      保留作物证，两份经 `_p5cc-cmp.mjs` 逐字段比过：**11 例全部 0 处差异**（旧基准可被零值构造原样复现）。
      门禁 `tests/p5c_golden.rs::stats_malformed_shapes_match_go`（11 例全部精确比对，`go_panics` 表全 `false`）。
      另：`big-int-2p53` / `int64-max-minus-1` 两例的**写出侧**受差异 41 末尾那条影响
      （统计写出走 f64，>2^53 落的字节与 Go 的 int64 编码不同），期望值按 Rust 现状锁定并在用例注释里写明。
36. **脱敏的两处微差（构造输入下才可见）**：
    ① 大小写折叠：Go 的 `(?i)` 是 **Unicode 简单折叠**（全角 `ｄｏｓ` 也能命中 `DoS`），
    Rust 用 `eq_ignore_ascii_case`（**ASCII 折叠**）；
    ② 词表排序：Go `sort.Slice` **不保证稳定**，Rust `sort_by` 稳定——只有「等长且同前缀」的两个词
    才会看出差别，而词表里不存在这种对（已逐对核过）。
    真实提示词里两者都不可能触发（词表与文本都是 ASCII）。
37. **`bytes.TrimSpace` 的 Unicode 子集**：SSE 逐行清洗用 `bytes.TrimSpace`（Unicode 空白），
    Rust 用 ASCII 空白子集——只有行首尾出现 U+00A0 这类**非 ASCII 空白**时才有别。
    `appendCleanedSSELine` 里 `data:` 之后紧跟的内容不受影响。
38. **Go 侧 `desensitize.go` 的三处死代码**（Rust 未迁，如实登记）：
    `runtimeBlockReplacements`、`runtimeTailMarkers`/`runtimeTailSummary`、`codexCoreSummary`
    **在 Go 里没有任何调用点**——`compactHarnessMessage` 的两个分支都返回内联的短句。
    这一点是门禁当场抓出来的：Rust 最初照常量写了长版（带 "Inspect the repository…"），
    `p5c-desensitize-cn` 的出站体立刻与 Go 不合。
39. **超长单行（>16MB）的 `collectStream` 行为**：Go 的 `bufio.Scanner` 遇 `ErrTooLong` 直接
    **停止扫描**（后续帧全丢、聚合提前结束）；Rust 把缓冲截到上限后**继续按行处理**。
    真实上游不会给单行 >16MB 的 SSE（内容分片），登记备查。
40. **`/v1/stats` 的 `uptimeSec` 时点**：Go 用 `time.Since(s.startedAt)`（装配时刻起算），
    Rust 用 `Instant`（同为装配时刻起算），两侧都归一成 `0` 比对（差异 17 一族）。

### P5c 审查后补修（2026-10-16）新增登记

41. **`usageScanner` 的 `usage` 取数走 f64，不等于 Go 的 int64 字面量规则**（本轮**登记、不对齐**）：
    Go 把整帧 `json.Unmarshal` 进 `int64` 字段 ⇒ 每个字面量按 `strconv.ParseInt(s,10,64)` 解，
    带小数点或指数的字面量（`1e2` / `1.0` / `1.5`）与超出 int64 的字面量**一律报错 ⇒ 该帧不入账**
    （真 Go 采收：`.work/cli/jsonint/main.go`）。Rust 这条路径的帧解析走 `godecode`，
    它把**所有**数字折成 f64（对齐 Go 的 `map[string]any` 语义，见本表第 1 条与 `gjson` 头注释）
    ⇒ 到 `src/sse.rs::parse_usage` 时**原始字面量已不存在**，只能按「f64 且无小数部分」判断：
    `1e2` / `1.0` 会被接受、>2^53 丢精度、溢出饱和。真上游只发普通整数，两侧取值相同。
    对齐代价（二选一，都不该在本轮动）：改 `godecode` 的数字模型——会牵动 chat 请求体那条
    已被黄金逐字节锁住的路径；或另写一个 int64 专用扫描器（复刻 `encoding/json` 的字段匹配）。
    **同族的另一半已对齐**：`src/stats.rs::load_from_disk` 那条路径上 serde_json 保留了
    「整数形 / 浮点形」之分，`as_i64()` 恰好等价 `ParseInt` + 溢出检查，`null` 按 Go 的
    no-op 处理；单测 `stats::tests::int64_literals_follow_go_parse_int`（正反 19 例 + 精确值）。
    **顺带发现（同族、本轮未改）**：统计的**写出**（`/v1/stats` 与 `codebuddy-stats.json`）
    走 `gjson` 的 f64 规则，而 Go 那边是**结构体 int64 编码**（`handleStats` 直接写
    `map[string]*modelStat`）⇒ |值| > 2^53 时两侧字节不同（修法要给统计的 i64 字段单独走整数写出）。
42. **`collectStream` 的 EOF 残行**（本轮**已对齐**，不再算差异，登记为改动记录）：
    `bufio.Scanner` 在流结束（EOF 或读错误）时会把末条**没有换行**的残行也交给聚合
    （`bufio/scan.go` 的 `ScanLines`：`if atEOF { return len(data), dropCR(data) }`），
    只有 `[DONE]` 触发的提前 `break` 才让它作废。Rust 原先只消费带 `\n` 的完整行 ⇒ 残行被丢
    （丢内容 / `finish_reason` / `usage`）。现已对齐；基准是真 Go `collectStream` 采收的
    `pure.collect_eof` 5 例（末帧无换行 / 对照带换行 / `[DONE]` 之后的残行必须作废 /
    残行本身是 `[DONE]` / 残行是坏 JSON），另有 `collect.rs` 单测守「残行跨 chunk 拼接」。

## 六、P5c 交接清单（**已全部完成**，保留作对照）

1. ✅ `desensitize.go` 全角色脱敏 + `--desensitize` 放行（`src/desensitize.rs`，硬门禁 3 步）。
2. ✅ `sseFieldCleaner` + `usageScanner`（`src/sse.rs`）。
3. ✅ `collectStream` 非流式聚合（`src/collect.rs`）；原先那条 `MAX_OK_BODY = 64MB` 整份缓冲
   **已撤掉**，改成与 Go 同一套「Peek 消费 + 边扫边聚」。
4. ✅ `codebuddy-stats.json` 按模型累计 + `/v1/stats`（`src/stats.rs`；落盘口径按 Go 现状，见差异 34）。
5. ✅ 11128/11129 日志（P5a 已接；本轮用 `_cmp11128.mjs` 逐字比对过文案、gating 与截断长度）。
6. ✅ 差异 12（路径清洗 307）与差异 13（499 取消信号）**两条都对齐**，不再是差异。
7. ✅ `AppState::production` / `production_with_data_dir` 的 `--desensitize` 放行与接线。

**仍未迁**（刻意不迁，非欠账）：差异 2（TLS 指纹，Rust 无对等物）；`.work/` 里那批一次性脚本
（采集/合并/比对工具，不进库）。

## 七、采集器与临时文件登记

* `internal/codebuddy/zz_codebuddy_a_collect_test.go`（P5a 合同场景）与
  `zz_codebuddy_p5b_collect_test.go`（P5b 池/凭据/额度/failover 场景）——
  **两个一次性采集器，跑完已删除**，原样留档
  `.work/rust-rewrite/collect-codebuddy-a2_test.go` 与 `collect-codebuddy-p5b_test.go`。
  删除后核验：`git status --short internal/` 空、`go test ./internal/codebuddy -count=1` 与
  `go vet ./internal/codebuddy` 干净（Go 包回到未污染状态）。
* 再生成链：两个采集器 → `.work/rust-rewrite/_p5b-merge.mjs`（打 `scope` 并合并）→ 入库黄金。
  详见 `tests/golden/README.md`。
* `.work/rust-rewrite/` 下另存：`golden-codebuddy-go.json`、`p5a-scenarios-go.json`、
  `p5b-scenarios-go.json`、`_p5b-merge.mjs`、`_live-compare-codebuddy.ps1`、
  `_mutation-check-codebuddy.ps1`（+ `.log`）、`_live-stub-intl.json`、`_diffcodebuddy.mjs`。

### P5c 追加（2026-10-16）

* `internal/codebuddy/zz_codebuddy_p5c_collect_test.go`——**P5c 一次性采集器，跑完已删除**，
  原样留档 `.work/rust-rewrite/zz_codebuddy_p5c_collect_test.go`。两个测试函数：
  `TestZZCollectCodebuddyP5C`（7 场景；**脱敏场景用 `NewServerForRegion(region, true)`** = 生产
  `--desensitize` 口径）与 `TestZZCollectCodebuddyP5CCancel`（11133 退避期间的断连实验：
  带取消 1 次命中 / 不带 3 次）。
  跑法（`CODEBUDDY_P5C_OUT` / `CODEBUDDY_P5C_CANCEL_OUT` 两个 env 指输出文件）与纪律写在
  文件头；删除后核验：`git status --short internal/` 空、`go test ./internal/codebuddy -count=1`
  0.295s ok、`go vet ./internal/codebuddy` 退出码 0（`internal/codebuddy` 回到 22 文件原状）。
* 合并：`.work/rust-rewrite/_p5c-merge.mjs`（**文本级拼接**，只插 `meta.p5c`/`meta.p5c_cancel`
  与替换 `scenarios` 数组，P5a/P5b 的字节一字不动——`git diff` 只有 2 个 hunk、全为新增）。
* 关掉历史跳过：`.work/rust-rewrite/_unskip-downstream.mjs`（摘掉 8 处 `body_compare:
  "outbound-only"`）。
* 日志文案比对：`.work/rust-rewrite/_cmp11128.mjs`（11128/11129 逐字比，格式动词归一）。
* 测试执行辅助：`.work/rust-rewrite/_cbtest.ps1`（见「怎么跑」里的火绒说明）。
* 本 crate 未改任何 Go 源码、未动版本串 8 处 / `CHANGELOG.md` / 根 `README.md` / `*.exe` / tag。

## 八、P5c 反向验证（门禁不是空转）

脚本：`.work/rust-rewrite/_p5c-mutations.ps1`（逐条突变 → 重编 → 拷出二进制 → 跑门禁 → 期望判红 → 还原；
**必须先拷出**是因为火绒会秒删刚链接的 exe，见「怎么跑」那节的说明）。

| # | 突变 | 结果 |
|---|---|---|
| M1 | 脱敏：不删黑名单模板句的括号注解（去掉 `strip_fingerprints`） | `p5c_golden` **判红** |
| M2 | 14018 判定窗口按注释写成 64KB（真实现只有 4096） | `p5c_golden` **判红**（由 `p5c-nonstream-peek-large` 抓住——上游体 ≤4096 字节时这条差异不可见） |
| M3 | SSE 清洗：不剔空数组 `tool_calls` | `p5c_golden` **判红** |
| M4 | 聚合：tool_calls 的 `arguments` 分片不累加（覆盖式） | `p5c_golden` **判红** |
| M5 | 统计：`saveStats` 不落盘 | `p5c_golden` **判红** |
| M6 | 路径清洗：重定向体少一个尾巴换行 | `p5c_golden` **判红** |
| M8 | 499：连接级断连信号永不点亮（等价改前的行为） | **未被捕获**（见下，如实登记） |

**M8 为什么没被捕获（不粉饰）**：hyper 在客户端断开时会**直接丢弃在飞的请求 future**，于是
「不再打上游」这条**外部可观测**行为在没有 499 分支时也成立（`tests/p5c_wiring.rs` 断言的就是
这一条：断开后上游命中数 = 1）。`conn.rs` 带来的是**代码路径**上的对齐（显式走
`writeErr(499, "client closed request")`、与 Go 逐字同文案、且 Drop 守卫照常释放 inflight），
而 499 响应本身在两侧都**不可能被客户端观测**（发出去时客户端已经断了）。
⇒ 这属「不可由外部观测的等价性」，不是门禁空转的证据缺口。

另有两条**先有测试、后修实现**的收获（不是突变，是真实缺陷）：
* `tests/p5c_wiring.rs` 的 `Inc/DecLoad` 接线测试当场抓出「正常路径完全不释放 inflight」——
  Go 用 `defer DecLoad`，Rust 原先只在换号分支里 `dec_load`；已用 Drop 守卫 `LoadRelease` 修正。
* `p5c-desensitize-cn` 当场抓出 `codexCoreSummary` 用错版本（长短句之差）——见差异 38。

### P5c 审查后补修的反向验证（2026-10-16）

逐条「改回旧行为 → 跑门禁 → 必须判红 → 还原」：

| 变更 | 改回旧行为后 | 证据（`.work/cbtest/`） |
|---|---|---|
| `collect.rs` 的 EOF 残行（差异 42） | `pure.collect_eof` 第 1 例判红（`collectStream 与 Go 不一致`）+ `collect::tests::eof_residual_line_is_consumed_across_chunks` 判红 | `_p5c-r6-eof-revert.txt` |
| 同上，去掉 `[DONE]` 守卫（残行一律处理） | `pure.collect_eof` 第 3 例判红（`[DONE]` 之后的 `AFTER` 被错误聚合） | `_p5c-r6-eof-overprocess.txt` |
| `stats.rs` 的 int64 字面量规则（差异 41） | `stats::tests::int64_literals_follow_go_parse_int` 在 `1.0` 一例判红 | `_p5c-r6-int64-revert.txt` |

**M2 突变日志补齐**：`_p5c-mutations.ps1` 重跑（`mut-M2-peek-window.log` 已落 `.work/cbtest/`）。
顺手加固了 `_cbtest.ps1` 与 `_p5c-mutations.ps1`：**构建失败、或链接产物已被火绒删掉时直接报错**，
不再回退到按 mtime 挑出的旧二进制——旧版会把「编译失败」静默变成「跑旧产物」，本轮实测踩到过
（一次 lib 编译错被伪装成 `55 passed`）。

## 九、P5c-b：Go 修了「非流式 Peek 丢头」之后的 Rust 重对齐（2026-10-16）

Go 侧两条真缺陷由主智能体在 `3d4627f`（用户授权）修掉：非流式 + 有池时的 14018 Peek 丢头、
`codebuddy-stats.json` 的 nil 形态。此前 Rust 是**如实复刻缺陷**，本轮改为对齐修复后的 Go。

| # | 改了什么 | 位置 |
|---|---|---|
| 1 | **删掉 4096 丢头复刻**：`BUFIO_DEFAULT_BUF`（4096）常量删掉，换成 `PEEK_WINDOW = 64*1024`（Go 的 `Peek(64*1024)`）；14018 判定改调 `ChunkReader::peek`，**peek 到的字节留在 `pending` 里**、后续聚合照读——对齐 Go `resp.Body = struct{io.Reader; io.Closer}{br, resp.Body}` | `src/chat.rs`（`PEEK_WINDOW`、`handle_chat` 的 200 分支、`ChunkReader::peek`/`read_source`） |
| 2 | **`ChunkReader` 分成 `next_chunk`（drain，先发 `pending`）与 `read_source`（直读上游）**：`peek` 必须走 `read_source`——走 `next_chunk` 会把同一个 `pending` 反复读回来，**空转死循环**（开发中实测踩到，`pending` 恒为 40 字节、CPU 打满）。`next_chunk` 里留了显式警告注释 | `src/chat.rs` |
| 3 | **条目级 `null` 由「按零值条目插入」改为「跳过不插入」**：对齐 Go `loadStats` 的 `delete(m, k)`——`{"m":null}` 装载后 `/v1/stats` 两侧都不显示该条目（此前 Rust 会多出一条全 0） | `src/stats.rs::load_from_disk` |
| 4 | **i64 字段解码补 u64 回退**：serde_json 把 `> i64::MAX` 的整数字面量保留成 `u64`，而 Go 的 `ParseInt(…, 10, 64)` 在**无符号词法**下接受 2^64-1 以内的字面量 ⇒ 补 `as_u64()` 回退 | `src/stats.rs::load_from_disk` |
| 5 | **黄金重采（按路径并入）**：10 处 `resp_body` 实质变化 + 1 处整条头部消失（另有 6 处只随采集变的 `created`/`id`，按纪律**不写回**） | `tests/golden/golden-codebuddy-go.json`（311,546 B，与 `.work` 副本同哈希 `1BACB424…`） |
| 6 | 新增门禁与单测：`stats_malformed_shapes_match_go`（11 例畸形/边界形态）、`chat::tests::peek_keeps_every_byte_for_the_aggregation` | `tests/p5c_golden.rs`、`src/chat.rs` |

### 重采后的新旧差异（逐条，真 Go 采收）

上游输入一字未动，只重跑采集器（A2 / P5b / P5c 三个一次性采集器，跑完已删、原样留档
`.work/rust-rewrite/`）：

| 场景 / 步骤 | 旧（复刻缺陷） | 新（修复后的 Go，Rust 已对齐） |
|---|---|---|
| `chat-request-edges` #0~#3 | `content:null` + usage 全 0 | `content:"hi"` + `usage{7,2,9}` |
| `chat-request-intl` #0/#1 | 同上 | 同上 |
| `chat-badjson` #16（数字下溢） | 同上 | 同上 |
| `chat-14018-200` #0（换号后） | 同上 | 同上 |
| `p5c-desensitize-cn` #1（非流式，脱敏硬门禁那一步） | 同上 | 同上（**脱敏硬门禁的 3 步计数不变**） |
| `p5c-nonstream-peek-large` #0 | `content:null`（丢头 4096 的形状）+ `content-length: 287` | **真实 content 6000 字节**（6288B 响应体）+ usage{5,6,11}，`content-length` 头部**整条消失**（体超过 Go 的缓冲阈值后不再补该头） |
| `p5c-nonstream-collect` #1~#4、`p5c-stats` #2/#3 | — | **无实质变化**（只有 `created` / `chatcmpl-<hex>` 两个易变字段，门禁本就归一，故**不写回黄金**） |
| 其余 29 个场景 | — | 完全未动（逐路径 diff 零差异） |

**反向验证（门禁非空转）**：把 `peek` 改回「读进 `out` 且清空 `pending`」的旧实现 ⇒
`chat::tests::peek_keeps_every_byte_for_the_aggregation` 立即判红（`left: []`——下游一个字节都收不到）。
`p5c-nonstream-peek-large` 是这条修复的**形状门禁**：上游 6.3KB 的 SSE，丢头实现必然聚不出 content。
耗时提示：`lib` / `p5c_golden` 的旧二进制副本不能复用——**改 `src/` 后必须重链**（`_cbtest.ps1` 会因
`cargo` 非零退出直接报错，不会退回旧产物）。

**本轮未做**：`sse.rs::parse_usage` 的字面量对齐（差异 41 前半，同前一轮的登记）；
统计**写出**侧 `> 2^53` 的 f64 变形（差异 41 末尾，本轮只在 `stats_malformed_shapes_match_go`
的期望值里如实锁定）。
