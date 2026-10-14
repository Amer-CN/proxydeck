# rs-vibex —— ProxyDeck VibeX 插件（8790）的 Rust 等价实现

绞杀者路线 **P4**：等价替换 `internal/vibex`（10 个生产文件、3,183 行，REST ＋ WS 私有协议直译）。
只做测试端口实例，**不注册进 `plugins.go`、不碰 8790、不碰任何现行服务端口**。

## 一、合同清单（对齐表）

采集/实现/验证三步都按本表逐条核。列 `Go` 是唯一权威（只读），`Rust` 是实现处，`门禁` 是锁住它的测试。

### 1. 配置与落盘（`internal/vibex/config.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C1 | 缺省值：`base_url=https://vibex.runninghub.cn/vc`、`app_name=vibex2api`、`app_type=web`、`fresh_session=true`、`turn_timeout=900`、`ready_timeout=90`、`queue_limit=4`、`heartbeat_s=15`、`connect_mode=auto`、`chat_instruction` 空 | `config.go:21-41` | `config.rs` `defaults()` | `golden:config-defaults`、`golden:config-normalize` |
| C2 | **`base_url` 裸域补 `/vc`**：TrimRight `/` 后解析，path 为空或 `/` 才补；自带 path 的自定义地址原样 | `config.go:118-121` | `config.rs` `normalize()` | `golden:config-normalize` |
| C3 | `normalize` 其余：空串回缺省、`<=0` 的四个数字回缺省、空 `connect_mode`→`auto` | `config.go:112-143` | 同上 | 同上 |
| C4 | `tenant_id` 为空 → `randHex(16)`（32 位小写 hex）并**立即落盘**；失败不阻断 | `config.go:104-107` | `config.rs` `load_config()` | `golden:config-tenant` |
| C5 | 配置文件读：旧明文原样读、`PDAPI1\n`+base64 密文解密；解析失败用缺省 | `config.go:100-102` + `appdir.ReadFile` | `crypto.rs` `read_file()` | `golden:dpapi-roundtrip` |
| C6 | 落盘：`MarshalIndent(c,"","  ")` + DPAPI 加密 + ACL 收紧 + tmp→rename；**加密失败不改动原文件** | `config.go:147-156` + `appdir.WriteFile` | `crypto.rs` `write_file()` | `golden:config-persist` |
| C7 | `persistToken`：TrimSpace、去空、去重追加、**同 sub 只留最后一条**（从后往前扫再反转）、同步内存 `cfg.Tokens` | `config.go:161-209` | `config.rs` `persist_token()` | `golden:persist-token-dedupe` |
| C8 | token 来源优先级：`RH_ACCESSTOKEN` > `--vibex-token` > `cfg.Tokens`；TrimSpace 去重保序 | `config.go:213-227` | `config.rs` `token_sources()` | `golden:token-sources` |
| C9 | `upstreamUA` 常量逐字 | `config.go:39-40` | `config.rs` `UPSTREAM_UA` | `golden:headers` |

### 2. JWT（`jwt.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C10 | `decodeJWT`：TrimSpace→`len(parts)<2 → nil`→TrimRight `=`→RawURL（无填充）base64→JSON→`sub`；`nickName`/`user_name`/`username`/`mobile` 首个非空，全空回退 `sub` | `jwt.go:27-57` | `jwt.rs` | `golden:jwt` |
| C11 | `exp` 只认数值（`float64` 截断为整数秒）；非数值 → 零值（= 无 exp） | `jwt.go:60-71` | `jwt.rs` `unix_time()` | `golden:jwt` |

### 3. token 池（`pool.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C12 | `usable`：`Dead` → false；`CooldownUntil.After(now)` → false；`HasExp && now.After(Exp)` → false | `pool.go:22-33` | `pool.rs` | `golden:pool-usable` |
| C13 | `addrOf`：TrimSpace 后取**首个 `;` 之前**那段（cookie 属性尾巴去重） | `pool.go:69-75` | `pool.rs` | `golden:pool-dedupe` |
| C14 | `addWithState` 两档去重：同串 → `existing=true` 返回池里那条；**同 sub 不同串 → 旧串让位**；sub 为空的老条目按需现解 JWT 回填 | `pool.go:97-125` | `pool.rs` | `golden:pool-same-sub`、`golden:pool-load-dedupe` |
| C15 | `acquire` 取第一个可用；`markDead` / `markCooldown`（now+cooldown）/ `incCalls` | `pool.go:128-168` | `pool.rs` | `golden:token-rotation` |
| C16 | `snapshot` 脱敏字段：`index`/`dead`/`calls`/`available`，命中冷却再有 `cooldown_until`，有 exp 再有 `exp`/`expired`/`days_left`；时间 `RFC3339`；**绝不含 token 本体** | `pool.go:171-194` | `pool.rs` | `golden:health-with-token` |
| C17 | `daysLeft`：`0 < d < 24h` → 1；`d <= 0` → −1；否则 `ceil(d/24h)` | `pool.go:197-206` | `pool.rs` | `golden:days-left` |
| C18 | `maxDaysLeft`：所有带 exp 的 token 里最大剩余天数；无 exp/无 token → `ok=false`（键省略） | `pool.go:209-224` | `pool.rs` | `golden:health-with-token` |

### 4. 容错解包（`util.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C19 | `str`：nil→""；string→原样；float64→`trimFloat`；bool→"true"/"false"；其它→`fmt.Sprint` | `util.go:13-29` | `util.rs` | `golden:str-conversions` |
| C20 | `trimFloat`：`f == int64(f)` → 整数串（`403.0`→`"403"`），否则 `fmt.Sprint(f)` | `util.go:32-37` | `util.rs` | 同上 |
| C21 | `truthy`：nil→false；bool；float64≠0；string≠""；其它→true | `util.go:40-53` | `util.rs` | `golden:truthy` |
| C22 | `appsFrom`：顶层数组 / `.apps` / `.items` / `.data`，且 `data` 里可再套一层 `apps`/`items` | `util.go:114-135` | `util.rs` | `golden:apps-from` |
| C23 | `unwrapObj`：顶层若含 `.data`/`.app` map 则下沉一层 | `util.go:102-111` | `util.rs` | `golden:unwrap-obj` |
| C24 | `liveStatus`：`live` 是 map 且**无 `error` 键**且 `live.status` 是 string → 它；否则 `status_cached` | `util.go:138-151` | `util.rs` | `golden:live-status` |
| C25 | `portsOK`：`host_ports` 的 `9000` 与 `7000` 都 `> 0`（数值型才算） | `util.go:154-175` | `util.rs` | `golden:ports-ok` |
| C26 | `initRootMeta`：9 个键（`app_id`/`name`/`app_type`/`image`/`host_ip`/`host_ports`/`created_at`/`pocketbase_url` + `flow`(仅当是 string)/`enabled_capabilities`(仅当是数组)），**缺失键仍以 null 出现** | `util.go:178-191` | `util.rs` | `golden:init-root-meta` |
| C27 | `shapeOf`：`array[N]` / `map{k[],k{},k}`（键排序，`[]` 带 `=+N`）/ `non-container` | `util.go:63-84` | `util.rs` | 单测 |

### 5. 提示词打平与事件折叠（`prompt.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C28 | `extractText`：string 原样；数组逐块（string 段；`type=="image_url"` 或含 `image_url` 键 → `[图片内容不支持, 已忽略]`；否则 `text`；再否则 `content`），**过滤空段后 `\n` 连接**；map→`text`；其它→`str(v)` | `prompt.go:18-54` | `prompt.rs` | `golden:extract-text` |
| C29 | `buildPrompt`：role 小写（空→user）；`system`/`developer`→系统段；`assistant`→`助手`；`tool`→`工具输出`；其它→`用户`；instruction 空→`defaultInstruction` | `prompt.go:60-111` | `prompt.rs` | `golden:build-prompt` |
| C30 | 特例分支：**无系统文本 且 `len(messages)==1` 且唯一那条 role 为 `user`** → 原文直传；仅当 `chat_instruction` 非空才追加 `\n\n【任务】\n{instruction}` | `prompt.go:89-97` | `prompt.rs` | 同上 |
| C31 | 常规分支：`【系统设定】\n…`、`【对话记录】\n【label】\n文本`（行间 `\n\n`）、`【任务】\n…`，块间 `\n\n` | `prompt.go:98-110` | `prompt.rs` | 同上 |
| C32 | `claudeEvents`：`stream_event`→`content_block_delta`→`text_delta`→`text` / `thinking_delta`→`thinking`；`assistant`→`message.content` 各块 `text`→`assistant_text`（`isApiErrorMessage` 或 `balance_insufficient` 为真时**额外**发 `error_text`）、`tool_use`→`tool`（仅 `expose_tools`）；`result`→`result`（`Text=result`，`Usage=usage`）；其它→空 | `prompt.go:114-164` | `prompt.rs` | `golden:claude-events` |
| C33 | `oaiUsage`：`input_tokens`/`output_tokens` 取 int，`total = in + out` | `prompt.go:174-177` | `prompt.rs` | `golden:usage` |
| C34 | `foldSync`：`text`/`error_text`/`tool`/`note` 优先拼接；为空才回退 `assistant_text`；`result`→usage；`done`→flags；`error`→**首个** lastErr | `prompt.go:193-221` | `prompt.rs` | `golden:chat-sync` |

### 6. 协议客户端（`client.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C35 | `VibexError{Message,Status,Code}`；`tokenHint`（含 `Rh-Accesstoken`）/`authFailHint` 文案逐字；`restTimeout=120s` | `client.go:26-43` | `client.rs` | `golden:error-texts` |
| C36 | REST 鉴权头 8 项：`Content-Type`/`Authorization: Bearer`/`RH-TOKEN`/`X-Tenant-Id`/`X-Vibex-Locale: zh-CN`/`Origin=base_url`/`Referer=base_url+"/"`/`User-Agent`；JWT 有 `sub` 再补 `userId`/`username` | `client.go:67-83` | `client.rs` `headers_for()` | `golden:upstream-rest` |
| C37 | `validateToken`：`GET {base}/api/llm-providers`，读体上限 8 KiB，先 `unauthErr`，再 `>=400 → "探活失败 HTTP %d: %s"`（clip 200） | `client.go:88-115` | `client.rs` | `golden:accounts-add` |
| C38 | `api` 换号：`401` → `markDead` 换下一个；`402`/`429` → `markCooldown` 换下一个；其它错误**立即返回**；全试完回最后一个错误，无则 `TOKEN_EXHAUSTED` | `client.go:119-157` | `client.rs` | `golden:token-rotation` |
| C39 | `do`：body `json.Marshal`；读体上限 8 MiB；**先 `unauthErr`**，后 `402/429 → BALANCE_OR_LIMIT`，`>=400 → "接口 %s 返回 HTTP %d: %s"`(502) | `client.go:160-210` | `client.rs` | `golden:upstream-rest` |
| C40 | `unauthErr` 判定顺序：body `code` 是 `"401"`/`"403"`/`"412"`（字符串比较！）→ `TOKEN_INVALID`；`"402"`→`BALANCE_INSUFFICIENT`；`"429"`→`RATE_LIMITED`；否则 `msg`（缺则 `message`）大写后含 `TOKEN` → `TOKEN_INVALID`；最后 HTTP 401/403/412 → `TOKEN_INVALID`。**数值 code 走不到**（`str(403)`="403" 但类型是 float64→`trimFloat` 后确实得 "403"，见单测） | `client.go:215-238` | `client.rs` | `golden:unauth-err` |
| C41 | `providers` 容错解包（顶层数组 / `.providers` / `.items` / `.data` + 二层 `providers`/`items`/`data`/`list`）；未解出→**记结构日志后返回 `(nil,nil)`** | `client.go:241-269` | `client.rs` | `golden:models-upstream-shapes` |
| C42 | `appCreate` 请求体逐字段：`name`/`pinned_host_ip:null`/`llm_provider_id`/`app_type`/`flow:null`/`enabled_capabilities:null`；`code != "0"` 且含 `FREE_PROJECT_LIMIT` → 409 | `client.go:290-309` | `client.rs` | `golden:upstream-rest` |
| C43 | `appStart`：POST `start`（体 `{}`），然后每 2s `GET /api/apps/{id}` 到 `running && portsOK`，上限 240s；超时但已 running 仍放行 | `client.go:312-334` | `client.rs` | `golden:app-start` |
| C44 | `setLLMProvider`：PATCH `{provider_id, llm_test:false}`；`ensureLLMSettings`：POST `{}`，**失败只记日志不抛** | `client.go:337-349` | `client.rs` | `golden:upstream-rest` |
| C45 | `wsURL`：scheme `ws`/`wss`（`https`/`wss` → `wss`），host 取 base_url 的 host（**不带 base path**），路径 `/app-ws/{url.PathEscape(appID)}/ws` | `client.go:386-399` | `client.rs` | `golden:ws-url` |
| C46 | WS 握手请求逐行：`GET {path} HTTP/1.1`、`Host`、`Upgrade: websocket`、`Connection: Upgrade`、`Sec-WebSocket-Key`、`Sec-WebSocket-Version: 13` + 自定义头；校验 `Sec-WebSocket-Accept = base64(sha1(key+GUID))`；非 101 → `wsHandshakeError{Status,Body}`（读体 512 B） | `client.go:402-468` | `client.rs` | `golden:ws-handshake` |
| C47 | 帧层：客户端帧**必带掩码**；读侧 `ping`→回 `pong`、`pong`→忽略、`close`→回 close + `errWSClosed`、文本/二进制/续帧拼接；单条消息上限 `64 MiB` | `client.go:353-363`、`:490-611` | `client.rs` | `golden:ws-frames` |
| C48 | `clip`（TrimSpace + 前 n 字节）/`firstNonEmpty`/`sleepCtx` | `client.go:616-644` | `client.rs` | 单测 |

### 7. 编排层（`engine.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C49 | `nextFirstConnect`：仅 `connect_mode` 大小写不敏感等于 `auto` 时首连 `init_root`、之后 `init` | `engine.go:41-50` | `engine.rs` | `golden:ws-sequence` |
| C50 | `resolveModel`：请求名精确 id → `display_name`/`label`/`name` → `cfg.Model`（同两条）→ **首个 `billing_tier=="lite"`** → 首个；providers 空 → 502 | `engine.go:53-101` | `engine.rs` | `golden:resolve-model` |
| C51 | `ensureApp`：`appsList` 找 `name==AppName` 或 `app_id==AppName` → 无则创建 → `app_id` 空报 502 → `appStart` → 把 started 的键**合并**进 target（**原位覆盖**）→ `curProv != pid` 才 PATCH；`curProv`/`lastApp` 记忆 | `engine.go:111-159` | `engine.rs` | `golden:upstream-rest` |
| C52 | `sessionTable`：TTL 30 分钟（过期即删）；`begin`：id 空→新会话；`ent.sent >= total` → **当新会话全量重发**；否则 `start=ent.sent`；`commit` 成功才写 | `engine.go:163-214` | `engine.rs` | `golden:session-affinity` |
| C53 | **哨兵时序（已知坑）**：错误先写 `state`，哨兵必须是**最后一个动作**，且必须 `select { case out<-{}: case <-ctx.Done(): }` —— **消费方已退出时不得阻塞**（v3.19.0 审查驳回的「流式中断哨兵泄漏」就是漏了 `ctx.Done()` 分支） | `engine.go:251-277` | `engine.rs` `turn()` | `contract:sentinel_does_not_leak` |
| C54 | `turn`：`recover()` → `state.setErr("内部错误: %v")`；`turnMu` 单 app 串行；`pump` 出错 → `state.setErr`；成功才 `sessions.commit` | `engine.go:253-277` | `engine.rs` | `contract:turn_panic_is_reported` |
| C55 | `pump` 换号：`wsHandshakeError` 的 `401/403/412`→`markDead`、`402/429`→`markCooldown`；其它错误立即返回 | `engine.go:280-318` | `engine.rs` | `golden:token-rotation-ws` |
| C56 | `pumpWithToken` WS 头 3 项：`Cookie: Rh-Accesstoken=<tok>`、`Origin=base_url`、`User-Agent`；握手超时 30s；**ping 每 20s**（`{"type":"ping"}`），收尾先停 ping 再关连接 | `engine.go:324-365` | `engine.rs` | `golden:ws-sequence`、`golden:ws-handshake` |
| C57 | 时序：`init`/`init_root`（`cwd=/workspace/app`、`replay:false`，`init_root` 多 `meta`）→ 等 `ready`（readyTimeout，5s 片读；`error` → `"连接失败: …"`）→ `ensure-llm-settings` → `new_session`（fresh 时）→ `prompt{text,permission_mode:"dontAsk"}` → 事件泵 | `engine.go:367-416` | `engine.rs` | `golden:ws-sequence` |
| C58 | 事件泵：`claude_event`→折叠投递；`done`→取 6 个 flags（`context_overflow`/`session_poison`/`balance_insufficient`/`rate_limit_failover`/`failover_model`/`primary_model`）返回；`error`→记 lastErr **继续泵**；`ready`/`pong`/其它忽略 | `engine.go:418-473` | `engine.rs` | `golden:chat-stream` |
| C59 | 超时（`turn_timeout`）→ 发 `cancel` + `note:"已达到单轮超时上限, 已发送 cancel"` + `done{timeout:true}`；读错 → `note:"连接关闭"` + `done{ws_closed:true}`；`ctx` 取消 → 直接返回 | `engine.go:424-445` | `engine.rs` | `golden:turn-timeout`、`golden:ws-closed` |
| C60 | `readWS` 把 `net.Error.Timeout()` 归一到「片内无消息」；`emit` 尊重 ctx；`minDuration` | `engine.go:477-516` | `engine.rs` | 单测 |

### 8. HTTP 层（`server.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C61 | CORS：`Access-Control-Allow-Origin: *`、`Allow-Methods: GET, POST, OPTIONS`、`Allow-Headers: Content-Type, Authorization, X-Vibex-Session`；`OPTIONS` → **204 空体** | `server.go:79-90` | `lib.rs` | `golden:routes` |
| C62 | 路由 5 条：`/health`、`/accounts`、`/v1/models`、`/v1/stats`、`/v1/chat/completions`（Go `ServeMux` 精确匹配 + 子树；尾斜杠与未知路径由 mux 兜底 404） | `server.go:93-101` | `lib.rs` | `golden:routes` |
| C63 | `/health` **常驻 200**（无 token 也 200，`token_configured:false`）；字段 `status/service=vibex-go/version=1.0.0/token_configured/tokens/base_url/model_default/app_name/uptimeSec` + 有条件 `days_left_max`/`days_left`；`model_default` 空串 → `null` | `server.go:124-144` | `lib.rs` | `golden:health` |
| C64 | `/accounts` GET：`ok/action:"list"/tokens/pool_size/token_configured`（+`days_left`）；POST 非 JSON 体 → 400 `请求体解析失败`；`action` 空 = `add`；未知 action → **200 `ok:false`**；非 GET/POST → 405 `只接受 GET/POST` | `server.go:158-191` | `lib.rs` | `golden:accounts` |
| C65 | `addAccount`：JWT 非法 → 400 `INVALID_TOKEN`（文案含 `Rh-Accesstoken`）；探活失败 → 400 `INVALID_TOKEN` + `invalidTokenMsg`；已在池 → 200 `existing:true`；成功 → 200 + `persisted` + `exp`/`days_left`/`expired`；落盘失败**不回滚内存态**、回 `persisted:false` + `warning`；建 state 时登记 JWT `sub`（`ca7f09f` 起），手动粘贴路径也按同号收敛（见二之二） | `server.go:195-239` | `lib.rs` | `golden:accounts-add` |
| C66 | `action=auto` 三段：①直接探测（探活失败/已在池 → fall through）②拉起一次性 profile 浏览器 ③150s 长轮询（每 2s；客户端断即停）；超时文案逐字（含 `150 秒`） | `server.go:246-311` | `lib.rs` | `golden:accounts-auto` |
| C67 | `/v1/models`：池空 → 401 `TOKEN_NOT_CONFIGURED` + `tokenHint`；`?refresh=1` 旁路；**5 分钟 TTL 内存缓存**；缓存命中不打上游；**错误不写缓存**；providers 空 → 502 `provider 列表为空`；条目 `{id,object:"model",created:0,owned_by:"vibex",vibex:{display_name,billing_tier,description}}`，`id` 为空跳过 | `server.go:336-387` | `lib.rs` | `golden:models` |
| C68 | `/v1/chat/completions` 校验顺序：读体上限 32 MiB → TrimSpace 空 → 400；非 JSON 对象 → 400；`messages` 非数组或空 → 400；**然后**才判池空 401 | `server.go:390-410` | `lib.rs` | `golden:chat-validate` |
| C69 | `modelName = firstNonEmpty(requested, providerID(provider), "vibex")`；排队信号量**非阻塞取槽**，满 → 429 `QUEUE_FULL` | `server.go:412-427` | `lib.rs` | `golden:queue-full` |
| C70 | 会话亲和：`X-Vibex-Session` 头优先，其次 body `session_id`（string）；`startIdx > len(messages)` → 0 | `server.go:429-440` | `lib.rs` | `golden:session-affinity` |
| C71 | `syncOut`：聚合；`content==""` 时按 `balance_insufficient`(402) → `lastErr`(502) → `EMPTY_TURN`(502)；成功体 `{id:"chatcmpl-"+randHex(12),object:"chat.completion",created,model,choices[{index,message{role,content},finish_reason:"stop"}],usage}`；**只认 result 事件真 usage 入账** | `server.go:466-501` | `lib.rs` | `golden:chat-sync` |
| C72 | `streamOut` 帧序：3 个响应头 + 200 + **立即 flush** → 首块 `{"role":"assistant"}` →（增量）→ 心跳 `: hb\n\n`（`heartbeat_s`）→ 兜底 `assistant_text` → 无产出且有错 → `{"error":{...}}` + `[DONE]` → `balance_insufficient` 追加 `"\n[上游余额不足]"` → 尾块 `finish_reason:"stop"` + `usage` → `data: [DONE]\n\n`；写失败即 `cancel()` + 排空通道（**不泄漏生产方**） | `server.go:503-650` | `lib.rs` | `golden:chat-stream`、`contract:sentinel_does_not_leak` |
| C73 | `writeJSON`：`Content-Type: application/json; charset=utf-8`，`json.Marshal`（键排序 + HTML 转义）；错误体 `{error:{message,type:"vibex_error",code}}` | `server.go:652-682` | `lib.rs` + `gjson.rs` | 全部黄金 |

### 9. 统计（`stats.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C74 | `modelStat{calls,inputTokens,outputTokens,totalTokens}`；`/v1/stats` → `{models,uptimeSec}`，坏文件当空表仍 200 | `stats.go:14-39` | `stats.rs` | `golden:stats` |
| C75 | `recordUsage`：`nil` → 不入账；`in`/`out` 取 int，`total=in+out`；`addStat` **三者全 0 跳过**（不估算）；`total>0 ? total : in+out` | `stats.go:43-77` | `stats.rs` | `golden:chat-sync`（usage 入账）、`golden:stats` |
| C76 | 落盘：`json.Marshal(stats)`（键排序）、tmp→rename、权限 0644、文件路径 `appdir.Path("vibex-stats.json")` | `stats.go:95-108` | `stats.rs` | `golden:stats-persist` |

### 10. token 自动探测（`cdp_browser.go`）

| ID | 合同 | Go | Rust | 门禁 |
|---|---|---|---|---|
| C77 | cookie 名 `Rh-Accesstoken`（`EqualFold` 比名）；值须 `isJWTLooking`（`eyJ` 前缀 + 长度 ≥ 40 + `decodeJWT` 成功） | `cdp_browser.go:136-157` | `cdp.rs` | `golden:cdp` |
| C78 | 探测顺序：`/json` 取第一个 `type=="page"` 的 `webSocketDebuggerUrl` → CDP `Network.getCookies{urls:[vibex, runninghub.cn, vibex/]}`（id=1）→ 无则 `Runtime.evaluate` 读 localStorage（id=2）；均无 → 报错文案逐字 | `cdp_browser.go:94-132` | `cdp.rs` | `golden:cdp` |
| C79 | 浏览器名：`Browser` 含 `Edg` → edge、含 `Chrome` → chrome、否则 browser | `cdp_browser.go:183-201` | `cdp.rs` | `golden:cdp` |
| C80 | 拉起参数逐项：`--remote-debugging-port`、`--user-data-dir`、`--no-first-run`、`--no-default-browser-check`、`--no-experiments`、`https://vibex.runninghub.cn/` | `cdp_browser.go:209-229` | `cdp.rs` | `golden:cdp-launch` |
| C81 | profile 目录名 `proxydeck-vibex-probe-<UnixNano>`；清理 `%TEMP%` 下同名前缀且 mtime > 24h 的目录；空闲端口从 9222 起到 9229 | `cdp_browser.go:233-277` | `cdp.rs` | 单测 |

## 二、已知差异清单（登记，不静默跟随）

平台级差异沿用 P0~P3 口径（TLS 指纹、hyper/reqwest 的默认头与头序、`X-Forwarded-For`），
本阶段另有：

1. **TLS 栈**：Go `crypto/tls`（无 uTLS）vs Rust `tokio-native-tls`（Windows = schannel）。
   `wss://` 的真正上游只有真机才能到，本阶段全部回放都在 `ws://127.0.0.1`。
2. **出站头由各自客户端生成**：Go `net/http` 会补 `Accept-Encoding: gzip`、`User-Agent` 覆盖等；
   Rust `reqwest` 的头集/顺序不同。本 crate 显式设置契约要求的 8 个头，其余不保证逐字。
3. **`X-Forwarded-For` / `Host`**：Go 侧走 `ReverseProxy` 才有，本插件走直连 `http.Client`，两侧都没有。
4. **浮点最短表示的取整方向**：极大值（`|v| >= 1e17`）上 Go 的 Ryu 与 Rust 的 `Display`
   可能差最后一位（样本：`1.23456789012345678901e20` → Go `123456789012345680000`、
   Rust `123456789012345670000`，回读成相邻的两个 f64）。门禁的放行判据是**相差 ≤ 1 ULP
   且量级 ≥ 1e17**，其余一律判红。vibex 实际出站里没有这个量级的数（用量/端口/天数都是小整数），
   受影响面为 0。
5. **不迁**：`app/plugin_modes.go` 的 `--plugin-vibex` 分发（属 P5 GUI 阶段）；
   `main.go` 的 flag 集合。（本 crate 自带 `--plugin-vibex` 以便将来接入，但**不注册进 GUI**。）
6. **非 ASCII 上游请求头值——已收口，无 allowance**（2026-10-15 补修，此前登记有误）。
   `username` 取的是 JWT 里的昵称，中文昵称时该值是 UTF-8；Go 的 `net/http` 原样发出，
   Rust 侧 `reqwest`/`hyper` **同样原样发出**（用真 Go `net/http` 服务端验过：
   `GO-SERVER-SAW username="小明" userid="u-1"`，探针留档 `.work/rust-rewrite/hdr-echo/main.go`）。
   **原表述「axum/hyper 假上游的服务端解析会丢掉 obs-text」是错的，根因在测试脚手架**：
   `tests/common/mod.rs::pick_headers` 用 `HeaderValue::to_str().ok()` 做**有损读取**——
   `to_str()` 只接受可见 ASCII，非 ASCII 值必然 `Err`，被读成空串；门禁再把它当
   「假上游观察不到」放行，于是**同一句话在 87 处被"通过"**（假 allowance）。
   已改为按字节无损读取（`v.as_bytes()` + `String::from_utf8_lossy`），并**删掉放行分支**：
   现在 `username: 小明` 与其余头一样**逐字节比对**。实测依据：改完只动 `pick_headers`
   时门禁立刻判红 `[accounts-add #3 合法 token 入池] 非 ASCII 头 username …（实得 "小明"）`
   —— 证明假上游**保留**了 obs-text，是脚手架读丢了；收口后 allowance 归零
   （汇总行改为「无 allowance 放行」）。
7. **`/v1/chat/completions` 的请求体上限是「拒绝」而非「截断」**：Go `io.LimitReader`
   读满 32 MiB 就把剩下的丢掉、拿截断后的内容去解析；本 crate 用 axum 的
   `DefaultBodyLimit::max(32 MiB)`，超过即 413。两者在 32 MiB 以下完全一致
   （`>>2 MiB` 的回归用例锁住了「2 MiB 默认上限不会再回来」）。
8. **错误文案里的系统错误原文**（`请求失败 %s %s: %v`）由各自网络栈产生，措辞必然不同。
9. **上游连接断开的检测时机**：Go 把 `r.Context()` 贯穿到上游请求，下游断开会立刻取消；
   本 crate 的取消由**写响应失败**触发（`stream_out` 的 abort 路径 → `Cancel` → 生产方 unwind），
   不额外接 axum 的连接生命周期钩子。

## 二之二、D1：手动粘贴入池的同号收敛（**Go 已于 `ca7f09f` 修复，本 crate 已对齐**）

**曾经的 Go 侧缺陷（D1）**：`internal/vibex/server.go` 的 `addAccount` 建 state 时漏了 `Sub`：

```go
st := &tokenState{Value: token, Exp: info.Exp, HasExp: info.hasExp()}   // 修复前
```

而 `addWithState` 的同号替换分支是 `if st.Sub != "" { … }` → 该分支被跳过、新串直接**追加**，
于是同号在内存池里会留下两条（旧串 + 新串）。`NewPool`（重启加载）、`handleAccountAuto`
（自动探测，走 `tokenStateOf` 会设 `Sub`）与落盘路径 `persistToken` 三条路径本就收敛
——**四路只有手动粘贴这一条不收敛**。P4 阶段 Rust 侧逐字复刻了它（`lib.rs::add_account`
显式 `sub: String::new()` 并注明出处），门禁也把旧行为锁死（`accounts-add` #7 `pool_size:2`）。

**修复**：用户授权后 Go 侧已改 `server.go:206` 为
`&tokenState{Value: token, Exp: info.Exp, HasExp: info.hasExp(), Sub: info.Sub}`，提交 `ca7f09f`
（`go build ./...` / `go vet ./internal/vibex` / `go test ./internal/vibex -count=1` 全绿，
既有测试无需改动）。四路现一致收敛。

**Rust 侧对齐**：`lib.rs::add_account` 的 `sub: String::new()` → `sub: info.sub.clone()`，
「如实复刻 D1」的注释与特判一并删除；黄金基准用**修复后的 Go** 重采（变化清单见第七节）。
门禁现在锁的是收敛后的形态：`accounts-add` #7 `pool_size:1`、#8 `tokens[]` 一条；
`pool-converge` 同号入池后 1 条、再加不同号 2 条。`pool.rs` 里没有别处依赖「手动粘贴路径
sub 为空」这个前提（`TokenState.sub` 只被 `pool.rs::add_with_state` 的同号替换分支读取；
`client.rs` 的鉴权头是从 token 串现解 JWT，不读 state）。

## 三、黄金门禁设计

与 P0~P3 同口径：

* **基准来源**：Go 侧**同包**临时采集器 `internal/vibex/zz_vibex_golden_collect_test.go`
  （跑完立即删除，留档 `.work/rust-rewrite/collect-vibex_test.go`）。采集时上游是**进程内假 VibeX**
  （REST + WS 手写握手），**不碰 8790、不碰任何现行端口、不出网**。
* **基准形态**：`golden-vibex-go.json`**单档全量入库**（256 KB < 1 MB；2026-10-15 D1 修复后重采，
  见第七节），**没有需要跳过的超大场景**；重采链与归一化规则见 `rs-vibex/tests/golden/README.md`。
* **加载顺序**：先 `rs-vibex/tests/golden/`，缺则回落 `.work/rust-rewrite/`。
* **归一化**：`created` / `uptimeSec` / `days_left*` / 随机 `id` / 随机端口由断言侧归一，
  **其余逐字节**。
* **零轮询纪律**：上游命中计数进黄金（`upstream_hits`），缓存/旁路口径用**精确等值**锁死。
* **allowance 计数**：已登记的脚手架/实现差异（差异 4）**逐条计数并打印**，
  不静默放行；判据是「相差 ≤ 1 ULP 且量级达标」这种可机械检验的条件。
  **差异 6（非 ASCII 头）的 allowance 已归零**：它本来就是脚手架的有损读取造成的假边界
  （见差异 6），修掉读取后门禁对这些头**逐字节比对**，不再有放行分支。

## 四、验收命令

```powershell
cd rs-vibex; cargo test                    # 37 单测 + 3 黄金 + 3 合同
cargo test -- --nocapture                  # 看汇总行（纯函数条数 / 场景数 / allowance 计数）
cd ..; go test ./internal/vibex -count=1
go vet ./internal/vibex
git status --short
```

> ⚠ **构建目录**：本机 `F:` 盘已满（`0 B` 可用，主要是 P0~P3 三个 crate 的 `target/` 缓存
> 约 7.5 GB），故本轮 `cargo` 全程用外置 target 目录：
> `$env:CARGO_TARGET_DIR="E:\cargo-target\rs-vibex"`（那个目录不入库）。
> 磁盘腾出来后去掉这个环境变量即可回到默认行为。

## 五、P4 执行结果（2026-10-15，executor 实测）

### 五条验收

| # | 验收 | 实测 |
|---|---|---|
| 1 | `/health`、`/v1/models`（5 分钟缓存）与路由边界与 Go 同形 | 黄金门禁 `routes-no-token`（22 步）+ `models-shapes`（8 步）+ `models-cache`（10 步）逐字节；缓存命中/旁路用**上游命中计数精确等值**锁死 |
| 2 | 两侧出站（REST 与 WS 帧）逐字节/逐帧一致 | 黄金 `http` 19 场景 60 步：每条上游请求的 method/path/契约头/体逐字节一致；WS 帧序（`init_root`→`new_session`→`prompt`，续轮变 `init`）与握手头（`Cookie`/`Origin`/`User-Agent`）逐字节一致 |
| 3 | 非流式/流式各至少一次成功；chunk/`finish_reason`/`[DONE]` 与 Go 逐事件一致；`/v1/stats` 真 usage 入账 | `chat-stream` 场景 6 步：SSE 五帧（role → 你好 → 世界 → `finish_reason:stop` + usage → `[DONE]`）与非流式体逐字节；`/v1/stats` 响应与 Go 相同（usage 入账 12/7/19） |
| 4 | token 池收敛行为与 Go 一致；配置权限/落盘形态一致 | `pool-converge` 场景：内存 `tokens[]` 与 Go 逐字节（**D1 修复后的收敛形态**：同号 1 条 → 加不同号 2 条；旧口径「含 D1 那条不收敛」已随 `ca7f09f` 作废，见第七节）；落盘经 DPAPI 解回的 `tokens` 数组、`tenant_id` 长度 32、磁盘上 `PDAPI1\n` 前缀逐项一致 |
| 5 | `cargo test` 全绿；`go test`/`go vet` 干净；无发版动作 | 见下方「验收命令与真实输出」 |

### 覆盖之外的额外门禁

* `tests/http_contract.rs` 三条：路由边界（404 文案逐字 / 尾斜杠 / OPTIONS 204 / 405 / 6 种 400）、
  **C53 哨兵不泄漏**（客户端断流后下一轮不被堵）、**32 MiB 口径**（2.5 MiB 请求体逐字节进 WS `prompt` 帧）。
* `tests/golden_vibex.rs` 的 CDP 段 4 个场景（cookie 命中 / localStorage 兜底 / 都没有 / 探活 401），
  含 CDP 调用序、拉起次数、profile 目录名前缀、探测成功后的落盘形态。

### 门禁规模与可复现性

* **规模**：纯函数 **152 项**；HTTP/WS **19 个场景 / 60 步**（每步 = 下游响应 + 上游观测 + WS 帧序 +
  握手头 + 上游命中计数）；CDP **4 个场景**。**allowance 0 处**（原 87 处是脚手架有损读取造成的
  假边界，已随差异 6 收口：非 ASCII 头值与其余头一样逐字节比对）。
* **可复现性**：同一采集器**独立跑两次**（`.work/rust-rewrite/_diffvibex.mjs` 逐字段比对），
  除 WS 握手 `Origin` 里的**随机端口**外，每一个测量值都被原样复现
  （其余差异全是 `created`/`uptimeSec`/`days_left`/随机 id 这类易变字段）⇒ 基准可重放、可作门禁。
* **门禁非空转（反向验证）**：两处突变都让门禁变红，还原后复绿——
  ① `addr_of` 不再剥 cookie 属性尾巴 → `[C13] addrOf` 判红；
  ② `/v1/models` 的 5 分钟缓存永不命中 → `models-cache` 场景在上游请求条数上判红。
  ⚠ 还原后必须**显式把文件 mtime 顶到当前时间**，否则 cargo 认为没变、继续用改坏的二进制
  （P3 已登记的同一个坑，本轮复现一次）。


### 采集器与临时文件

* 采集器 `internal/vibex/zz_vibex_golden_collect_test.go`——**跑完已删除**，
  留档 `.work/rust-rewrite/collect-vibex_test.go`。删除后 `git status --short internal/` 空、
  `go test ./internal/vibex -count=1` ok、`go vet ./internal/vibex` 退出码 0。
* 探针 `hdr-echo`（真 Go `net/http` 服务端，验 Rust 客户端是否发出非 ASCII 头）——
  源码留档 `.work/rust-rewrite/hdr-echo/main.go`，产物已删。
* 探针 `tests/zz_probe.rs`——**已删除的一次性探针**，按 P2/P3 惯例登记留痕
  （便于事后核对 `git status` 干净是「删干净了」而不是「从没建过」）。
  内容不在了，但**构建痕迹可查**，据此复原了它的身份：目标名
  `zz_probe-a90363bcf71f1e78`（`E:\cargo-target\rs-vibex\debug\deps\`，exe 11,528,704 B /
  mtime 2026-10-15 03:17:52 / SHA-256 前 16 位 `090009CF19ED9AB7`；`.d` 依赖文件只列
  `tests\zz_probe.rs`）；`--list` 显示它只有一个用例 **`probe_non_ascii_header`**，
  二进制里的字符串为 `http://127.0.0.1:18799/x`、`username`/`userid`、`小明`、
  `服务端应收到请求`、`send 结果:`/`错误:`/`服务器看到:`。
  ⇒ 它探的就是**假上游（axum 服务端）能不能收到非 ASCII 头**这个问题；
  它当时读回来的到底是空串还是 `小明`、以及它在第 4 轮之前有没有被读过，**无从复原、不臆断**。
  第 4 轮已用可复现的方式把这个问题问清楚：`pick_headers` 改成按字节无损读取后，
  门禁实得 `"小明"`（见差异 6 与本轮第 1 条）。

## 六、P4 审查后补修（第 4 轮，2026-10-15）

审查判 **通过**（3 条中低危 + 若干低危；无伪造、无越界）。本轮只修审查列出的必修与登记缺口：
**`src/**` 与生产语义零改动**，动过的只有测试脚手架、黄金文案与文档。

### 逐条（改了哪一行 → 怎么验证）

| # | 位置 | 改动 | 验证 |
|---|---|---|---|
| 1 | `tests/common/mod.rs`（`pick_headers`） | `HeaderValue::to_str().ok()` → `v.as_bytes()` + `String::from_utf8_lossy`：按字节**无损**读取 | 见下「第 1 条的三步取证」 |
| 1 | `tests/golden_vibex.rs`（原 allowance 分支） | **删掉非 ASCII 放行分支**，这类头与其余头一样 `assert_eq!` 逐字节比对；`ALLOWANCES` 计数器与汇总行的「87 处」字样一并删除 | allowance **87 → 0**；`-- --nocapture` 汇总行现为「无 allowance 放行（非 ASCII 上游头值也逐字节比对）」 |
| 1 | `README.md` 差异 6、`tests/golden/README.md` 末节 | 更正根因表述：**不是**「axum/hyper 服务端丢 obs-text」，而是脚手架有损读取 | 见下第 3 步 |
| 2 | `README.md`「采集器与临时文件」 | 补登记已删的一次性探针 `tests/zz_probe.rs`（含从构建痕迹复原的身份） | 见该条 |
| 3 | `README.md` 本节 + `.work/rust-rewrite/_vibex-deviation-notes-r4.md`（新） | 补披露 `_live-compare-vibex.ps1` / `_mutation-check-vibex.ps1` **未交付**的改道原因与替代证据链 | 见该文件 |
| 4 | `.work/rust-rewrite/collect-vibex_test.go`（原 `:1197,1200`）+ 两份 `golden-vibex-go.json` 里同两句 desc | 「GET /accounts（应只剩 1 条）」→「（D1 不收敛：实为 2 条）」；「（2 条）」→「（D1 + 不同号：实为 3 条）」。**只改 desc 字面，测量值一个字节未动** | 去掉所有 desc 行后逐字节比对**差异 0**、desc 行数 109→109、JSON 解析 OK；两份副本 SHA-256 同为 `481F1CC1622F1246EC5CA2C2555C10B96283F1458B823290B9A6D1A857AA2D4A`。**⚠ 这两句（以及那一代基准）已随 D1 修复在第七节再次改写/重采，现口径见第七节** |
| 5 | `tests/golden_vibex.rs`（浮点段两处注释） | 「README 差异 3」→「**差异 4**」（浮点最短表示是差异 4；差异 3 是 `X-Forwarded-For`/`Host`） | 全 crate `Select-String "差异 3"` 复扫 |
| 6 | `.work/vibex-profile-probe/README.md` | 给目录里两个 exe 补一行来历登记（**不删**） | 见该文件 |

### 第 1 条的三步取证（allowance 87 → 0，`username: 小明` 逐字节锁定）

1. **改前基线**：`cargo test --test golden_vibex -- --nocapture` 汇总行实测
   `已登记 allowance 87 处`（与审查数字一致）。
2. **只改 `pick_headers` 一处**（放行分支先不动）→ 门禁立刻判红：
   `[accounts-add #3 合法 token 入池] 非 ASCII 头 username 在 axum 假上游侧应观察不到（实得 "小明"）`
   ⇒ **假上游（axum/hyper 服务端）保留了 obs-text 原始字节**，是脚手架把它读丢了；
   原「服务端解析丢 obs-text」的表述被这条实测推翻。
3. **删掉放行分支**（改为逐字节比对）→ 全绿，汇总行 allowance 归零。再**反向验证**
   它真在比对而不是空转：把入库黄金里 `http[1] accounts-add #3` 的
   `"username": "小明"` 做**字节级**替换成 `"小朋"`（`http` 段内首个命中，offset 39093）→
   门禁判红在 `[accounts-add #3 合法 token 入池] 上游请求头 username（GET /vc/api/llm-providers）`；
   还原后哈希回到 `CDFF7229…`、复跑全绿。**备份副本**：
   `.work/rust-rewrite/_golden-backup-r4.json`。

> 踩到的坑（登记，避免下次重犯）：第一次反向验证改的是**全文第一处** `"username": "小明"`，
> 那一处在 `cdp` 段、`http` 段根本比不到，于是门禁照常全绿——**险些误判成「改了也没红」**。
> 正确做法是先定位 `"http": [` 的字节偏移、只改该段之后的命中；`cdp` 段的同名字段不参与这条断言。

### 本轮的反向验证清单（都非空转）

* 非 ASCII 头：见上第 3 步（黄金改 `小明`→`小朋` 判红，还原复绿）。
* 黄金文案改动：去 desc 行后**零差异**（证明测量值没被顺手动过）。
* 第 4 步两份副本同源：SHA-256 逐字节相同。

## 七、P4b：D1 修复后的重对齐（2026-10-15）

起因：用户授权 Go 侧修掉 D1（提交 `ca7f09f`），于是「逐字复刻旧行为」不再成立——基准与实现都必须
对齐**修复后的 Go**。本轮只做对齐，不加功能。

### 1. 黄金基准重采（用修复后的 Go）

* 采集器 `.work/rust-rewrite/collect-vibex_test.go` → 临时放回 `internal/vibex/zz_vibex_golden_collect_test.go`
  → 连跑两次（`_repro-vibex-b1.json` / `_repro-vibex-b2.json`）→ **立即删除** → 重新留档 `.work/`。
  删除后 `git status --short internal/` 空、`go test ./internal/vibex -count=1` ok、`go vet ./internal/vibex` 退出码 0。
* **可复现性**：两份新采集经 `.work/rust-rewrite/_diffvibex-r4b.mjs` 归一化（端口 / `uptimeSec` /
  `created` / `chatcmpl` 随机 id / `days_left*` / `collected_at`；脚本另有一条防御性的 `date` 替换，
  基准里没有 `Date` 头故不生效）后**差异 0 处**。
* **新旧比对**（同一脚本，旧 = 上一代基准）：归一化后 **8 处差异**，逐条如下；除这 8 处外，上一代基准的
  每一个测量值（`pure`/`cdp`/`floats` 三段、19 个场景的 `upstream_seen`/`ws_frames`/`ws_handshake`/
  `upstream_hits`/响应头，以及 `pool-converge` 的落盘 `extra`）都被原样复现。

| # | 路径（旧 → 新） | 旧（D1 未修） | 新（D1 已修） |
|---|---|---|---|
| 1 | `http[1] accounts-add #7` 同号新串入池 · 响应体 | `pool_size:2` | `pool_size:1` |
| 2 | `http[1] accounts-add #8` 换旧后的 `/accounts` · 响应体 | `pool_size:2`，`tokens[]` 2 条（旧串 exp `2027-01-15` + 新串 exp `2027-01-16`） | `pool_size:1`，`tokens[]` **1 条**（只剩新串 exp `2027-01-16`） |
| 3 | `http[18] pool-converge #0` 同号新串入池 · 响应体 | `pool_size:2` | `pool_size:1` |
| 4 | `http[18] pool-converge #1` · desc | `GET /accounts（D1 不收敛：实为 2 条）` | `GET /accounts（同号已收敛：实为 1 条）` |
| 5 | `http[18] pool-converge #1` · 响应体 | `pool_size:2`，`tokens[]` 2 条 | `pool_size:1`，`tokens[]` 1 条 |
| 6 | `http[18] pool-converge #2` 不同号入池 · 响应体 | `pool_size:3` | `pool_size:2` |
| 7 | `http[18] pool-converge #3` · desc | `GET /accounts（D1 + 不同号：实为 3 条）` | `GET /accounts（+ 不同号：实为 2 条）` |
| 8 | `http[18] pool-converge #3` · 响应体 | `pool_size:3`，`tokens[]` 3 条 | `pool_size:2`，`tokens[]` 2 条 |

* 第 4/7 条是**基准文案**（desc 随事实改写，数据本来就是对的）；其余 6 条是内存池的测量值。
* `http[18].extra`（落盘 DPAPI 解回：`tokens:[B,C]`、`token_count:2`、`plaintext_contains_token_b:true`、
  `tenant_id`、`on_disk_encrypted:true`）**新旧一致**——落盘路径本就按 sub 收敛，不受 D1 影响。
* 新基准两份副本同源：`rs-vibex/tests/golden/golden-vibex-go.json` 与
  `.work/rust-rewrite/golden-vibex-go.json` SHA-256 同为
  `B420C20CA7E75FB515DE33D2DDFE90A9844FB16FCCF8DDC682C1A58AD4E44B47`（261,886 B）。
  上一代（`481F1CC1…`，262,285 B）原样留档 `.work/rust-rewrite/_golden-vibex-backup-r4b.json`。

### 2. 实现对齐（`src/**` 只此一处）

| 位置 | 改动 |
|---|---|
| `src/lib.rs::add_account` | `sub: String::new()` → `sub: info.sub.clone()`；「如实复刻 D1 缺陷」的注释整段改写为「与修复后的 Go 一致（`ca7f09f`）」 |

`src/pool.rs` 未动：`sub` 只在 `add_with_state` 的同号替换分支里被读，没有别处依赖「手动粘贴路径 sub 为空」。

### 3. 反向验证（门禁非空转）

拿**旧实现**（`sub: String::new()`）对新基准跑门禁 → 立刻判红（这就是本轮修复前的实跑输出）：

```
thread 'http_scenarios_match_go_golden' panicked at tests\golden_vibex.rs:509:9:
assertion `left == right` failed: [accounts-add #7 同号新串入池（以新换旧）] 响应体
  left:  …"pool_size":2…
  right: …"pool_size":1…
test result: FAILED. 2 passed; 1 failed
```

改回 `info.sub.clone()` 后全绿（下节输出）。

### 4. 验收命令与真实输出

```powershell
cd rs-vibex; $env:CARGO_TARGET_DIR="E:\cargo-target\rs-vibex"; cargo test
#   unittests src\lib.rs   : 37 passed; 0 failed
#   unittests src\main.rs  :  0 passed
#   tests\golden_vibex.rs  :  3 passed; 0 failed
#   tests\http_contract.rs :  3 passed; 0 failed
#   0 warning / 0 error
cargo test -- --nocapture
#   纯函数黄金对比：152 项与 Go 一致；… 差异 1 项（浮点最短表示，已登记，见差异 4）
#   HTTP/WS 场景黄金对比：19 个场景逐字节一致；显式跳过 0 个；无 allowance 放行（非 ASCII 上游头值也逐字节比对）
#   CDP 场景黄金对比：4 个场景全部一致
cd ..; go test ./internal/vibex -count=1     # ok  github.com/Amer-CN/proxydeck/internal/vibex  6.286s
go vet ./internal/vibex                      # 退出码 0
git status --short internal/                 # 空（临时采集器已删净）
```

