# rs-qoder —— ProxyDeck Qoder 插件（8785）的 Rust 重写

> 绞杀者路线 **P3**：等价替换 `internal/qoder`（阿里 Qoder），先在测试端口独立跑通。
> **不注册进 `app/plugins.go`**、不动 Go 构建、不碰任何现行服务端口（尤其 8785）。

## 形态

与 Go 侧同构：**每请求 spawn 一个官方 agent SDK worker 进程**
（`qoder-worker-runtime.obf.mjs`，stdin/stdout 各一行一个 JSON 的 stream-json 协议，
jobToken 回调鉴权）。**全部上游流量出自官方 worker 进程**——代理只做协议翻译，
不自己构造任何对上游的请求（Go `server.go:3-6` 的「零自造请求」原则）。

```powershell
cd rs-qoder
cargo build
# 测试端口（绝不与 Go 版 8785 抢端口）
.\target\debug\rs-qoder.exe --plugin-qoder --port 18785
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--plugin-qoder` | 关 | 与 Go 侧同名占位开关（GUI 不会 spawn 本 crate） |
| `--port` | `8785` | 与 Go 侧同值；对照一律显式给测试端口 |
| `--host` | `127.0.0.1` | 回环 |
| `--worker` | 自动探测 | 覆盖 worker 路径（`QODER_WORKER` → `D:\Program Files\...` → `%ProgramFiles%\...`） |
| `--auth-dir` | `%APPDATA%\com.qodercn.app.stable` | 登录态目录覆盖（对照用） |
| `--credits-file` | `%USERPROFILE%\.qoder-cli-credits.json` | 账本路径覆盖（对照用） |
| `--usage-url` | `https://openapi.qoder.com.cn/api/v2/quota/usage` | 实时额度接口覆盖（对照用） |

## 端点（与 Go `http.ServeMux` 注册的四条同形）

| 路径 | 行为 | 方法 |
|---|---|---|
| `/health` | 无登录态 `{"status":"no_license"}`；有则 `{"credits":{...},"service":"qoder","status":"ok","uptimeSec":N}`，`credits` 含 `spent` / `spentThisSession` / `quotaExceeded`，`refresh=1` 时另有 `realtime` | **不看方法**（POST 也 200） |
| `/credits/set` | `POST {"spent":0}` 清零账本；非 POST → 405 + `Allow: POST` | 判方法 |
| `/v1/models` | 静态目录 13 条（首条 Auto） | **不看方法** |
| `/v1/chat/completions` | 扁平化 `messages` → worker prompt → 翻回 OpenAI（流式/非流式） | 仅 POST |
| 其余 | `ServeMux` 语义：`cleanPath != path` → 307，否则 404 纯文本 | — |

所有响应都带 CORS 三头；`OPTIONS` 一律 204（Go `corsWith` 的短路）。

## 对齐表（每条都有真 Go 采收的基准或活体对照）

| 合同 | 怎么对齐的 |
|---|---|
| worker 命令行 12 个参数（含 `--model`、**不带 `--bare`**） | 假 worker 把 `argv` 记进日志，两侧逐字节比（黄金 `chat[].worker_log`） |
| 子进程环境：父环境去 `NODE_OPTIONS` + 末尾追加三个 `QODER_*` | 同上（假 worker 记 `node_options` 与三个值） |
| auth payload 文件内容 70 字节 | 同上（假 worker 把文件内容原样记回） |
| stdin 四类消息（`initialize` / `user` / `token` 应答 / `can_use_tool` 应答）逐字节 | 同上 |
| `initialize` 的键序与默认值（`initializeTimeoutMs:120000`、`modelPolicyProvider:false`、`supportsCatalogReadyInitialize:true`） | 同上（按 Go `json.Marshal` 的 map 键排序写出） |
| `fetch_job_token` 的 `{token, expires_at}`（`expires_at` 为 Unix 毫秒） | 同上；`expires_at` 来自登录态 `expiresAt` 的 RFC3339 解析 |
| cwd 解析（`Working directory:`/`工作目录` 标记 → 必须真存在且是目录） | 黄金 `chat` 5 条 cwd 场景的 `worker_log.cwd` 逐字比 |
| 流式：`text`→`delta.content`、`thinking`→`delta.reasoning_content`、收尾 `stop`+`[DONE]` | 黄金 `chunk` 10 例 + `chat` 流式场景的下游帧逐字节 |
| 非流式：`content = result.result`（空则回落拼接文本） | 黄金 `complete` 3 例 + `chat` 场景 |
| OpenAI 错误体（`{"error":{"message","type"}}` + 末尾换行）与各类文案（no_license/invalid_model/400） | 黄金 `chat[14..25]` 与 `models`/`routes` |
| 400 的 Go 原文错误串（`请求体解析失败: <Go 的 scanner 文案>`） | `godecode.rs`（复制自 `rs-comate`，`comate.chatRequest`→`qoder.chatRequest`）+ 黄金 7 条 |
| 积分账本 `~/.qoder-cli-credits.json`（键序、浮点形状、缺失/损坏→0、重复键后者生效） | 黄金 `credits_ledger` 15 例 + `/credits/set` 13 步（含账本落盘形态） |
| `/credits/set` 的请求体解析（Go `json.Decoder`：**只看第一个 JSON 值**，尾随数据不解析；数字溢出按 `ErrRange` 文案报错） | `godecode.rs::decode_spent_field`（Go 兼容写法）+ 黄金 `set-trailing-data` / `set-trailing-nonzero` / `set-number-overflow` 三步逐字节 |
| 假 worker 源码（整条 argv/stdin 证据链的「测量仪器」） | `tests/http_contract.rs::fake_worker_source_hash_matches_golden`：对入库 `fake-worker.js` 现算 SHA-256 与黄金 `worker_source_sha256` 比，改仪器必红 |
| `/health` 的 `credits` 三段与 `realtime` 视图（含 `isQuotaExceeded` 顶掉 `quotaExceeded`） | 黄金 `health` 9 组（含 refresh 四步 + 假额度上游） |
| 「绝不自动轮询官方接口」：只有 `refresh=1` 才拉 | 黄金 + 门禁断言「不带 refresh 时假上游命中数 = 0」 |
| 路由边界（404/307/405/OPTIONS/HEAD/大小写与路径清洗） | 黄金 `routes` 16 组 + `models` 6 种方法 |
| 登录态解密链（`Local State` 的 DPAPI key → `auth.v1.dat` 的 v10/AES-256-GCM） | 黄金门的合成夹具**跨语言**读同一份；另有**活体**对照：两侧读本机**真实** `%APPDATA%\com.qodercn.app.stable` 都回 `status:ok` |
| 请求体上限（Go `io.LimitReader(r.Body, 32<<20)`，读满即停、不 413） | `tests/http_contract.rs::large_body_reaches_the_worker`（3.6MB 体逐字节进 worker） |

**复用资产（按简报「复制后各自独立」处理，未抽公共 crate）**：
`gjson.rs`（+ `J::Float`：Go 浮点写出规则）、`godecode.rs`（改包名/错误串前缀）、
`cwd.rs`（改环境变量名）、`crypto.rs`（裁剪自 `rs-bai`：只留 DPAPI `unprotect` + Go 标准 base64；
被忽略的字符也只吃 `\r`/`\n`，与 `base64.StdEncoding` 同口径）。

## 已知差异（登记，不静默跟随）

1. **环境变量读取时机**：`QODER_WORKER` / `QODER_CWD` / `APPDATA` / `USERPROFILE` 在 Rust 是
   **启动时读一次**（Go 每请求现读）。运行中改这些环境变量，Go 立刻跟随、Rust 不跟随。
   黄金门禁为此按场景构造 `cwd_env`（`chat` 场景分段复用实例），逐条锁住 cwd 结果。
2. **额度接口的出站头由各自客户端生成**：Go `net/http` vs `reqwest`（`Accept-Encoding`、
   HTTP/1.1 vs 协商 HTTP/2、TLS 指纹）。显式设置的那四个头（`Authorization` / `Accept` /
   `Cosy-ClientType` / `User-Agent: Qoder`）逐字一致，已在门禁里断言。
3. **日志文案不逐字对齐**：Go `log.Printf` 带时间戳前缀，Rust 用 `println!`；时长格式化、
   标点都不保证逐字。日志不进 HTTP 合同，门禁不覆盖。
4. **`worker 超时（10m0s）` 文案**：按 Go `time.Duration.String()` 的形状复刻（整秒场景）。
   子秒级时长的 `%v` 形状未复刻（本 crate 只用得到 600s 这一个值）。
5. **单行 >8MB 的 worker 输出**：Go 侧实测会**卡到 600s 上下文超时**——子进程写满 stdout
   管道后阻塞、父进程 `cmd.Wait()` 一直等（采一条要 10 分钟，故**未入黄金基准**）。
   Rust 报同一条 `bufio.Scanner: token too long` 文案、等待也套在同一个 600s 预算里、到点杀进程；
   两侧都归到「worker 超时」分支；**未做逐字节对照**。
   ⚠ **内存形状不同（此前文案写成「读满 8MB 即停」，与实现不符，已更正）**：Go 的
   `bufio.Scanner` 封顶在 8MB——超过即弃、不再读；Rust 的 `read_until` 是先把**整行**读进
   `Vec<u8>`（长到换行 / EOF / 600s 截止为止），读完才判 `> 8MB`。所以超长单行的峰值内存
   是「该行实际长度」，不是 8MB。
6. **`no_worker` 503 分支**：本机真装了 Qoder CN（`D:\Program Files\Qoder\Qoder CN\...`），
   Go 的 `findWorker()` 一定命中 ⇒ **Go 侧采不到**。Rust 用测试缝
   （`AppState.worker_path = None`）锁住状态码与文案（`tests/http_contract.rs`）。
7. **流式响应的 `transfer-encoding`**：Go 的 SSE 不带 `content-length`（分块由 Go 隐式完成），
   hyper 会对长度未知的 body 显式补 `transfer-encoding: chunked`。门禁显式放行这一种形态，
   其它多出来的头一律判红。
8. **`sse_error` 保留 SSE 头**：流式路径**首个 chunk 落盘前**失败时，Go 先设了
   `Cache-Control`/`Connection` 再走 `writeError`（只覆盖 `Content-Type`），所以 502 上仍带这两个头。
   Rust 照此实现（黄金 `chat #7` 的头集合锁住了）。
9. **合成登录态夹具的可用性**：`tests/golden/auth-fixture/` 是本机现制的 DPAPI 密文
   （**当前用户作用域**），换机解不开。门禁遇到解不开会打
   `SKIP(dpapi-fixture): ...` 并计入汇总（**不静默跳过**）；重采即得新夹具。
10. **`--port` 默认值等于生产端口 8785**：沿用 `rs-bai`（8891）/`rs-comate`（8786）的同款约定，
    本阶段一致；对照一律显式给测试端口。

## 未覆盖

- 与**真 worker** 的端到端对话（会真出网、消耗账号积分）——本阶段一律用假 worker 驱动协议；
  真实上游行为未验证，**接线到 GUI/切流都不在本阶段**。
- 600s 超时与 >8MB 单行的逐字节对照（见差异 5）。
- `no_worker` 的 Go 侧实测（见差异 6）。
- Go 侧 `handleChatStream` 里「首 chunk 落盘后 worker 才超时」的时间轴细节（语义已对齐、
  未做长时对照）。

## 测试与门禁

```powershell
cd rs-qoder; cargo test          # 29 单测 + 6 黄金 + 7 合同
# 黄金门禁里的 SKIP 行与计数用 println! 打，libtest 默认**只在失败时**显示通过用例的 stdout：
cargo test -- --nocapture        # 要看「跳过了什么、跳过几条」就带这个（见 tests/golden/README.md）
```

黄金基准：`tests/golden/golden-qoder-go.json`（真 Go 采收，≈109 KB，完整入库，无跳过）。
黄金里内嵌的**采集机路径字面量**（临时目录、仓库根）由 `meta.machine_paths` 记着，门禁换成
本机等价物再比——换机不重采也不假红（口径见 `tests/golden/README.md`「采集机字面量」）。
再采集链与归一化口径见 `tests/golden/README.md`。
反向验证（改坏实现看门禁是否变红）：`.work/rust-rewrite/_mutation-check-qoder.ps1`。
活体对照（Go 18785 vs Rust 18786）：`.work/rust-rewrite/_live-compare-qoder.ps1`。
