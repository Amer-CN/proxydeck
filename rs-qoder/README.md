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
# 国际版（同一端口形态，按区域换整套默认值）
.\target\debug\rs-qoder.exe --plugin-qoder --port 18786 --region INTL
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--plugin-qoder` | 关 | 与 Go 侧同名占位开关（GUI 不会 spawn 本 crate） |
| `--port` | `8785` | 与 Go 侧同值；对照一律显式给测试端口 |
| `--host` | `127.0.0.1` | 回环 |
| `--region` | `CN` | `CN` / `INTL`。**只提供「整套默认值」**（登录态目录 / worker 路径 / 额度接口），显式覆盖参数优先 |
| `--worker` | 按区域自动探测 | 覆盖 worker 路径（`QODER_WORKER` → `D:\Program Files\...` → `%ProgramFiles%\...`） |
| `--auth-dir` | 按区域 | 登录态目录覆盖（对照用） |
| `--credits-file` | `%USERPROFILE%\.qoder-cli-credits.json` | 账本路径覆盖（对照用） |
| `--usage-url` | 按区域 | 实时额度接口覆盖（对照用） |

## 双区（CN / INTL）

Qoder 客户端有 CN 版与国际版两个构建。实测两侧 worker 运行时**字符串计数完全一致**
（`qoder.com.cn` / `qoder.sh` / `agent_chat_generation` / `stream-json` / `jobToken` /
`machine-id` / `--output-format` / `session_id` 等 15 个探针同值），差异仅 4 字节长度 +
包名，说明是**同一份代码的两个构建**，协议一致 ⇒ **不拆端口、不做账号池**，同一端口内按
`--region` 切三处默认值 + 模型目录文件：

| 项 | CN（默认） | INTL |
|---|---|---|
| 登录态目录 | `%APPDATA%\com.qodercn.app.stable` | `%APPDATA%\com.qoder.app.stable` |
| worker 相对路径 | `Qoder CN\...\@qoder-ai\qoder-cn-agent-sdk\dist\_worker` | `Qoder\...\@qoder-ai\qoder-agent-sdk\dist\_worker` |
| 额度接口 | `https://openapi.qoder.com.cn/api/v2/quota/usage` | `https://openapi.qoder.sh/api/v2/quota/usage` |
| 模型目录 | `Qoder CN\resources\dynamic-text\qoder-cn.json` | `Qoder\resources\dynamic-text\qoder.json` |

区域配置是 `lib.rs` 的 `Region` / `RegionCfg`（`REGION_CN` / `REGION_INTL` 两张常量表，
**在 rs-qoder 内自建，不跨 crate 引用** `rs-codebuddy/src/region.rs`）。两区登录态目录内部
结构完全一致（都有 `Local State` + `auth.v1.dat`），解密链复用同一份 `auth.rs`。
模型目录的区域选择在 `catalog.rs`（`file_name` / `install_dir` / `find_root`），**不在**
`RegionCfg` 里（它只装 worker 侧的三处默认值）。

**优先级**：`--region` 只给默认值，`--auth-dir` / `--worker` / `--usage-url` 显式给出时一律以
覆盖值为准。**不带 `--region` 时三处默认值与改前逐字节一致**（CN）。

实测（2026-10-20，测试端口 18785）：
- 默认（不带 `--region`）→ 读 `com.qodercn.app.stable`、用 `Qoder CN` 的 worker，
  `/v1/chat/completions` 返回 `OK-CN-DEFAULT`；
- `--region INTL` → 读 `com.qoder.app.stable`、用国际版 worker，`/health?refresh=1` 走 `.sh`
  域名正常返回 `realtime`，`/v1/chat/completions` 返回 `OK-INTL`。

> ⚠ **CN worker 路径已于 2026-10-20 按实测修正**：CN 客户端装在独立的
> `D:\Program Files\Qoder CN\`，**不在** `Qoder\Qoder CN\` 之下。原常量拼出的路径不存在，
> 会导致默认实例 `worker=<未找到>`、chat 回 503 `no_worker`。旧布局
> （`Qoder\Qoder CN\...`）作为**兜底候选**保留在 `WORKER_REL_CN_LEGACY`，
> 老安装仍能命中。本机 `D:\Program Files\Qoder` 是**国际版**。


## 端点（与 Go `http.ServeMux` 注册的四条同形）

| 路径 | 行为 | 方法 |
|---|---|---|
| `/health` | 无登录态 `{"status":"no_license"}`；有则 `{"credits":{...},"service":"qoder","status":"ok","uptimeSec":N}`，`credits` 含 `spent` / `spentThisSession` / `quotaExceeded`，`refresh=1` 时另有 `realtime` | **不看方法**（POST 也 200） |
| `/credits/set` | `POST {"spent":0}` 清零账本；非 POST → 405 + `Allow: POST` | 判方法 |
| `/v1/models` | 模型目录 **13 条**（Qoder 客户端 `resources\dynamic-text` 的目录；读不到回落内置静态表） | **不看方法** |
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
6. **`no_worker` 503 分支**：本机真装了 Qoder CN（`D:\Program Files\Qoder CN\...`，
   2026-10-20 实测；旧文写的 `Qoder\Qoder CN\...` 布局不存在），Go 的 `findWorker()`
   一定命中 ⇒ **Go 侧采不到**。Rust 用测试缝
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
11. **`--usage-url` 的「显式覆盖」判定**：该参数保留了 `default_value`（CN 常量，字段类型与
    改前一致，既有合同测试据此断言默认值），所以「值 == CN 默认值」被当作「未显式覆盖」，
    由 `resolve_usage_url` 按区域取。极端情形 `--region INTL --usage-url <CN 的默认域名>`
    会被当成未覆盖、实际走 `.sh`；真要指回 CN 域名时请带别的写法（如显式端口）。
    `--auth-dir` / `--worker` / `--credits-file` 没有这个歧义（`Option`，给了就是给了）。
12. **区域只换三处默认值 + 模型目录文件**：两区 worker 协议实测一致，故 `--region` **不改**
    端口、cwd 解析、积分账本、请求/响应形状；随区域变的只有登录态目录名、worker 相对路径、
    额度接口域名，以及 `/v1/models` 读的那份目录文件（`qoder-cn.json` / `qoder.json`；
    两区文件实测逐字节相同，所以解析出的条目也一致）。
13. **`/v1/models` 的目录是动态的（有意偏离 Go 黄金）**：Go 版是写死目录；Rust 版
    在启动时读 Qoder 客户端的 `resources\dynamic-text\{qoder-cn,qoder}.json`，取
    `model.<key>.label`（**排除**四个质量档位 `lite`/`efficient`/`performance`/`ultimate`），
    `en` 打底、`zh` 只补缺失的 key（保证 id 恒为 ASCII），按 worker key 字节序输出（13 条）。
    文件缺失 / 坏 JSON / 解析空 ⇒ 回落内置静态表（同一份 13 条），`/v1/models` 永不 500。
    * **`lite` 是档位不是模型（实证）**：直传 `lite` 上游回
      `ModelNotFoundError: Model "lite" is not available in the current QoderCN catalog`（404）；
      它和 `efficient`/`performance`/`ultimate` 构成「轻量/经济/性能/极致」四档阶梯。
    * `QODER_STATIC_MODELS=1` 强制静态表（黄金门禁与可复现对照用；显式构造的 `AppState`
      本来就固定走静态表——它是黄金回放的测试缝）。
    * `q37fmodel`(Qwen3.7-Flash) / `gm51model`(GLM-5.2) 已从目录里消失，**不再进
      `/v1/models`**，但上游仍能跑通 ⇒ 保留在 `QODER_LEGACY_MODELS`，旧调用方继续可用。
    * 黄金门禁为此拆成两条**更窄**的断言：响应体「信封」（data 数组以外的键序/分隔/换行）
      仍与 Go 逐字节一致；目录本身与**手写**的新期望体逐字节一致。`resolve` 的黄金比对
      只对四个新增模型（`Cantus`/`cmodel`/`Kimi-K3`/`kmodel_latest`）列了显式新期望，
      其余输入（含旧代号与全部拒绝样本）仍逐字比 Go。
14. **上游排队状态透出（Go 侧没有这一层）**：worker 的 `type=system` /
    `subtype=model_queue_status` 以前落在 `_ => {}` 被静默丢弃；现在解析
    `modelKey`/`queueCount`/`waitTime`/`retryAfterSeconds`（兼容字段平铺、`message` 是
    对象、`message` 是（多层）转义 JSON 字符串三种形态；实测原文
    `{"isQueued":true,"modelKey":"dfmodel","queueCount":4186,"queueType":"p3",
    "retryAfterSeconds":30,"waitTime":254}`）：
    * 每条排队状态写日志 `排队 N 人，预计等 M 秒（model=xxx）`（GUI 日志面板可见）；
    * 单个请求从**第一次**收到排队状态起计时，累计超过 `QUEUE_PATIENCE`（120s）⇒
      HTTP 429 + `Retry-After: <retryAfterSeconds>` +
      `{"error":{"message":"Qoder 上游排队：N 人，预计 M 秒","type":"queue_timeout"}}`；
      未超阈值继续等（短队列正常完成，流式/非流式都实跑验过）。计时用墙钟而不是累加
      `waitTime`（后者随每轮询重复下发，累加会重复计数）。
    * 流式响应只在**提交（首个内容 chunk）之前**回 429；提交后再来的排队状态只写日志
      （不能改回头）。上游没给 `retryAfterSeconds` 时用 30 秒兜底（上游自己的重试节奏；
      回 0 会让客户端立刻重试、加剧排队）。
    * 排队超时放弃时 `abort` worker 任务，并靠 `kill_on_drop` 杀掉排队中的 node 子进程
      （实测无残留）。
15. **worker 权限旁路（2026-10-22，有意偏离 Go）**：`worker_args()` 追加
    `--permission-mode bypassPermissions`。
    * 起因（DSH 实测）：`--print`（headless）模式默认「遇工具先问」，而反代场景没人能回答
      那个确认提示 —— 模型只能回用户「我没有权限」。实测原话：
      `Error: Allow reading F:\AIXM\proxydeck\Cargo.toml?`。
    * 与官方 App 的 bypassPermissions 同语义（`can_use_tool` 分支注释早就写明这个意图，
      只是真正生效要靠这个 CLI 参数）。
    * 黄金门禁：不改基准文件，比对前从实得 argv 里剥掉这对参数（`ARGV_DEVIATION`），
      并**单独断言它确实在**；其余参数与顺序仍逐字比 Go。
16. **工作目录提取兼容带引号路径（2026-10-22，有意偏离 Go）**：`cwd::trim_left_cutset`
    现在会跳过开引号（`"` / `` ` `` / `'`）。
    * 起因（DSH 实测）：DSH 的系统提示是 `Current working directory: "F:\AIXM\proxydeck".`，
      **路径带引号**。Go 版不跳开引号 → 扫描在第一个 `"` 上 break → 候选为空 →
      worker 退回临时目录 `E:\Temp` → 模型读项目文件被判「工作目录之外」而弹授权。
    * 修好后 `worker cwd=` 日志显示真实项目目录；实测同一请求从「权限提示」变成
      正确读出 `Cargo.toml` 的 9 个 workspace 成员（含行号）。

## 未覆盖

- 与**真 worker** 的端到端对话：CN 侧本阶段一律用假 worker 驱动协议（会真出网、消耗账号积分）；
  INTL 侧已做**一次**活体验证（`--region INTL` 实例 `/v1/chat/completions` 返回 `OK-INTL`，
  见上文「双区」），但未做长时/多模型对照。
- 600s 超时与 >8MB 单行的逐字节对照（见差异 5）。
- `no_worker` 的 Go 侧实测（见差异 6）。
- Go 侧 `handleChatStream` 里「首 chunk 落盘后 worker 才超时」的时间轴细节（语义已对齐、
  未做长时对照）。

## 测试与门禁

```powershell
cd rs-qoder; cargo test          # 50 单测 + 6 黄金 + 7 合同
# 黄金门禁里的 SKIP 行与计数用 println! 打，libtest 默认**只在失败时**显示通过用例的 stdout：
cargo test -- --nocapture        # 要看「跳过了什么、跳过几条」就带这个（见 tests/golden/README.md）
```

黄金基准：`tests/golden/golden-qoder-go.json`（真 Go 采收，≈109 KB，完整入库，无跳过）。
黄金里内嵌的**采集机路径字面量**（临时目录、仓库根）由 `meta.machine_paths` 记着，门禁换成
本机等价物再比——换机不重采也不假红（口径见 `tests/golden/README.md`「采集机字面量」）。
再采集链与归一化口径见 `tests/golden/README.md`。
反向验证（改坏实现看门禁是否变红）：`.work/rust-rewrite/_mutation-check-qoder.ps1`。
活体对照（Go 18785 vs Rust 18786）：`.work/rust-rewrite/_live-compare-qoder.ps1`。
