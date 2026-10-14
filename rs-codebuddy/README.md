# rs-codebuddy —— CodeBuddy/WorkBuddy 插件（8787 CN + 8789 INTL）的 Rust 重写

> **P5a：合同与请求侧**；**P5b：账号池 / 凭据 / 额度 / failover**（绞杀者路线第 5 阶段的两小轮）。
> 等价替换 `internal/codebuddy`，先在测试端口独立跑通，**不注册进 `app/plugins.go`**、
> 不动 Go 构建、不碰任何现行服务端口（尤其 8787/8789 上跑着的 Go 版）。
> 脱敏 / SSE cleaner / 统计属 **P5c**（交接清单见第六节）。

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

## 二、怎么跑

```powershell
cd rs-codebuddy
$env:CARGO_TARGET_DIR="E:\cargo-target\rs-codebuddy"   # F: 盘紧张时的外置目标目录
cargo test                                            # 42 单测 + 3 P5a 黄金 + 8 合同 + 4 P5b 门禁
cargo test -- --nocapture                             # 看「跳过/allowance」汇总行

# 起一个测试实例（**别用 8787/8789**）
cargo run -- --port 18786 --region INTL
```

参数：`--plugin-codebuddy`（占位，与 Go 侧同名）、`--port`、`--host`、`--region CN|INTL`、
`--upstream-base`（对照/回放用）、`--cred-stub <json>`（测试缝，见第四节）、`--auth-dir`、
`--diag-dir`、`--pool-dir`（后三个都是对照用覆盖；默认走 `CODEBUDDY_AUTH_DIR` /
`appdir::data_dir()`）。
`--desensitize` **传了就拒绝启动**（属 P5c，不静默忽略）。

## 三、门禁与证据

* **P5a 黄金门禁** `tests/golden_codebuddy.rs`：纯函数 42 条 + `scope=P5a` 的 12 个场景 / 56 步
  逐字节对照（比 **状态码 + 响应头白名单 + 响应体 + 上游看到的请求体与头**）。
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
* **反向验证**：`.work/rust-rewrite/_mutation-check-codebuddy.ps1`（P5a 六条突变）。
  P5b 的突变清单见第七节。

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

1. **下游 200 响应仍是最小透传**（**P5c 前最大的不等价点**）：流式按上游原字节转发（Go 会过
   `sseFieldCleaner` 剔掉空字符串键）、非流式把上游体读入后原样转发（Go 会用 `collectStream`
   聚合成 `chat.completion`）。→ P5c。门禁对这些步打 `SKIP(downstream-p5c)` 并计数。
2. **TLS 指纹**：Rust 无 uTLS 对等物（沿用 P0 起的既有登记口径）。
3. **头序/头名大小写/默认头**：hyper 发出的头名是小写（Go 会规范化成 `X-User-Id`）；reqwest 不会
   自动补 Go `net/http` 那些默认头。**值**逐字节一致（门禁比的是值，头名大小写不敏感）。
4. **脱敏未实现（且生产默认开着）**：`app/plugins.go` 对 CN 与 INTL **都**传 `--desensitize`，
   所以当前 Rust 实例**不能当生产用**——`--desensitize` 会拒绝启动（不静默忽略），
   而黄金里的 chat 场景是**未脱敏形态**。`DesensitizeBody` 与指纹清洗表属 P5c。
5. **`/v1/stats` 仍是 501**（P5c）：`codebuddy-stats.json` 的按模型累计与 usage 入账未迁。
6. **`inflight` 的计数在流式路径上不递减**：Go 的流式分支读完流即 `return`，`handleChat` 的
   `defer DecLoad` 仍然会跑；Rust 侧同样是 `defer` 语义（请求结束时 Dec）。但**门禁里
   `/v1/accounts` 快照与请求的采样时刻有关**（见差异 18），故该字段不参与逐字节比对。
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
12. **路径清洗**（P5a 审查低危项，已升级为**已知差异**）：`//v1/models` 这类双斜杠请求
    Go `ServeMux` 会清洗并回 **307**，axum 不清洗、回 **404**。对照用例
    `http_contract::double_slash_path_is_a_known_deviation` 把这条钉死（单斜杠路径两侧都 200）。
    客户端实际只会送单斜杠，故影响面为零；要不要复刻 `ServeMux` 的清洗规则留给 P5c 决定。
13. **11133 退避期间的客户端取消（499）**：分支已实现、文案与 Go 逐字一致
    （`writeErr(499, "client closed request")`），但**触发条件暂缺**——Go 的 `r.Context()` 是
    请求级 context（连接断开即 Done，退避等待期间也能中断）；axum/hyper 侧要拿到同一信号需要
    `hyper_util` 的 graceful-shutdown 观看器或自写 `Body` 包装。当前行为：客户端断开后仍会
    多试 1~2 次。→ P5c。
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
21. **未覆盖**：64MB 截断边界；`WorkBuddy` 桌面端真实 exe 的注册表发现（只做了纯函数级对照）；
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
    ③ **`data:null`（如 `{"code":0,"data":null}`）**：Go 侧 `len(out.Data)==4` 不进错误分支，
    随后把 `"null"` 解进 `map[string]any` 得到 **nil map**，`credential.go:448` 的
    `newAuth["lastRefreshTime"] = …` 对 nil map 赋值 **panic**（进程级致命）；
    Rust 见 `data.is_null()` 直接报 `刷新 token 失败(code=0): `、释放锁、继续服务。
    **这是 Go 侧的缺陷（与 P4 的 D1 同类：Rust 无对应形态可"干净"复刻）**——按纪律 Go 源码只读、
    缺陷只登记；若要修 Go（赋值前补 `if newAuth == nil { newAuth = map[string]any{} }`），
    须先经用户授权。

## 六、P5c 必须接手的清单

1. `desensitize.go` 全角色脱敏（`fingerprintRules`、黑名单模板句的括号注解删除）与 `--desensitize` 放行。
2. `sseFieldCleaner`（剔空字符串键）+ `usageScanner` 的 `data:` 行 usage 提取。
3. `collectStream`（非流式聚合：content 拼接、tool_calls 槽位按 index 合并、`finish_reason`
   缺省规则、`chatcmpl-<6 字节 hex>` id、`finish_reason_debug` 字段）。
   注意：Rust 侧为「14018 判定窗口与转发字节同源」把非流式上游体整份读入
   （`chat.rs::MAX_OK_BODY = 64MB`），换成真聚合时可撤掉这条缓冲。
4. `codebuddy-stats.json` 的按模型累计 + 节流落盘 + `/v1/stats`（现仍 501）。
5. 11128/11129 的日志诊断（两处日志已接；`summarizeToolsForDiag` 已在 `src/diag.rs`）。
6. 差异 12（路径清洗 307）与差异 13（499 的取消信号）是否复刻。
7. `AppState::production` 的 `--desensitize` 放行（现在一律拒绝启动）。

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
* 本 crate 未改任何 Go 源码、未动版本串 8 处 / `CHANGELOG.md` / 根 `README.md` / `*.exe` / tag。
