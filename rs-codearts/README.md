# rs-codearts —— 华为云码道（CodeArts Doer）插件

把**码道桌面端**的模型服务复刻成本地 OpenAI 兼容接口，接进 ProxyDeck：端口 **8793**，
插件 id `codearts`，启动参数 `--plugin-codearts`。

> 逆向接入，仅供学习研究。额度是账号级共享：插件跑量会消耗你码道账号当天的积分。
> 本项目不做任何风控规避（TLS 指纹、设备指纹一律不改），只求与官方客户端同源。

## 端点

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 本地应答；`ready` / `account` / `expiresAt` / `models` / `pool` |
| GET | `/v1/models` | 上游 `/v1/model/builtin`，10 分钟缓存 |
| POST | `/v1/chat/completions` | 流式透传；`stream:false` 本地聚合 |
| GET | `/v1/quota` | 上游 `/snap-manager/v1/statistics/plugin`（积分） |

客户端配置：`base_url = http://127.0.0.1:8793/v1`，`api_key` 随便填（本机回环无鉴权）。

## 账号池与续期（WorkBuddy 式：桌面切号即攒池）

> 码道桌面端同一时间只登一个号（`secretStorage` 里只有一个 session），
> 和 WorkBuddy（桌面存多号）不一样。所以多号池是这样攒出来的：
> **桌面端登录 A → 插件自动存一份快照；退出、登录 B → 再存一份**，
> 之后两个号都在池里自动轮转，桌面端那边可以随便切，不影响。

- **自动归档**：读到本机登录态且文件里没有同身份条目（也没被删过）就存一份快照；
  本进程内每个身份只写一次，不会反复写盘。
- **多账号轮转**：文件快照 + 本机实时登录态组成候选池（同一身份合并为一行，
  三方凭据取过期最晚的）。凭据失效（`APIG.0301/0303`、`STS5.*`、`invalid_grant`）
  → 标死换下一个；429 / 5xx / `TM.00001041`（并发 3）→ 冷却 30 秒换下一个；
  客户端参数错误直接透出。本机登录态在“码道桌面端运行中”被拒时只冷却 120 秒
  （桌面端随后会自己刷新），不永久标死。
- **手动管理**：GUI 甲板「码道」→「账号池」可看全部账号（状态/过期/可续期），
  一键「导入本机登录态」、按行「刷新 / 停用 / 删除」。对应接口：
  `GET /accounts`、`POST /accounts/import-local`、`POST /accounts/refresh`、
  `POST /accounts/enable`、`POST /accounts/remove`、
  `POST /accounts/import`（兼容 session 整包 / 单 session / 普通账号 JSON）。
- **DPoP 自动续期**：账号过期且带 `refresh_token + DPoP 私钥 + PKCE verifier` 时，
  按官方 Electron 版同一套流程续期：
  `POST https://sts.cn-north-4.myhuaweicloud.com/v1/oauth2/tokens`，
  form `client_id=codearts & code_verifier & grant_type=refresh_token & refresh_token`，
  头 `DPoP: ES256 JWS`（header 带 `jwk`，payload `htm/htu/iat/jti`）。
- **续期结果落盘**：同时写续期 overlay 和账号文件；**不写码道自己的 `secretStorage`**，
  避免和桌面端抢状态。
- **本机登录态默认不主动续期**（`--allow-local-refresh` 可开）：`refresh_token`
  单次轮转，和桌面端同时续期会把桌面端踢下线；桌面端每小时自己刷新，插件跟随即可。

## 凭据从哪来

**优先级 1：本机码道登录态（推荐，零配置）**

```text
%APPDATA%\codearts\Local State                       ← DPAPI 保护的主密钥
%APPDATA%\codearts\secretStorage\<sha256>.enc        ← base64(v10 + AES-256-GCM)
```

`import.rs` 只读解密，**不写回、不续期**。桌面端每小时自己刷新一次，插件按文件读取跟随；
过期时间为 `session.expiresAt`（毫秒）。

**优先级 2：账号文件** `%APPDATA%\ProxyDeck\codearts-accounts.json`

```jsonc
{ "channel": "credit",
  "accounts": [
    { "name": "hid_xxx",
      "accessKey": "HSTA...", "secretKey": "...", "securitytoken": "...",
      "expires_at": "2026-10-27T00:00:00Z" }
  ] }
```

也接受直接粘贴码道 `secretStorage` 解出来的 session JSON（含 `session` / `loginContext`）。
`--no-local` 可关闭本机自动导入（只用文件）。

## 实现要点（改代码前必读）

1. **签名是华为云 SDK-HMAC-SHA256，且有两处与文档不同**（`signer.rs`，金标准单测锁定）：
   - CanonicalURI 每段严格 percent-encode 后**强制补尾斜杠**；
   - CanonicalHeaders 每行自带 `\n`，拼 CanonicalRequest 时再补一个 `\n`（`SignedHeaders`
     前多一个空行）；
   - 只保留 `A-Za-z0-9-._~`，其余 `%XX` 大写；GET 按空 body 签名。
2. **上游一律 `stream:true`**：`glm-5.3` 只支持流式（`stream:false` 回 HTTP 400 空 body）。
   客户端要非流式时本地聚合（`sse.rs`）。
3. **业务错误可能以 HTTP 200 返回**：`{"error_code":"InferHub.xxx.404"}`。首事件预检在
   发响应头前转成真错误；状态码取 `error_code` 末段，取不到兜底 400。
4. **`model` / `model-id` / `model-name` 必须逐字相等**（大小写敏感），客户端尾随空格要 trim。
5. **`x-ot-session-id` 按 AK 派生并复用**：上游每账号并发上限 3，每请求随机会瞬间打满
   （`TM.00001041` → 429）。
6. 凭据类错误（`APIG.0301/0303`、`STS5.*`、`invalid_grant`）统一 502 并提示重新登录，
   不当作普通 400 让客户端无限重试。
7. `maas_type: benefit` 有两种触发：账号文件写 `"channel":"benefit"`（整号福利通道，默认
   `credit`），或该账号的**福利模型目录**确认了本模型（`benefit=true`）。2026-10/11 实测
   福利网关整体下线（`4000 invalid channel`），未确认时不发——内置模型误发会被 APIG 路由到
   不存在的福利 API（`APIG.0101.404`）。
8. **请求体规整（`protocol.rs`）**：客户端可能发空 `role`（DSH 等 agent 框架会塞内部条目），
   上游直接回 `InferHub.001001044.400 Message role cannot empty`。对齐参考实现
   HITZY2002 `chatMessagesToOpenAI`：空 role → `user`（有 `tool_calls` → `assistant`、
   有 `tool_call_id` → `tool`）；`role` 去空白转小写；`developer` → `system`；
   缺 `content` 且无 `tool_calls` 的补空串。
9. **`APIG.0602` 必须算凭据错误**（2026-11-05 线上事故）：华为 STS 临时令牌
   （`session.securityToken`）寿命比 `session.expiresAt` 短，到点后上游回
   `400 {"code":"APIG.0602","message":"the security token has expired"}`。
   旧版只认 `APIG.0301/0303/STS5.*`，于是既不给本机登录态续期、也不换号，
   把上游 400 **原样透给客户端**——DSH 侧连续重试 38 次、约 36 分钟（13:58→14:34），
   直到码道桌面端 14:40:31 自己刷新登录态才恢复。现在：
   - `APIG.0602` / 正文 `security token has expired` → 凭据错误 → 换号；
   - 续期被 4xx 拒（`the refresh token has been used`）的账号直接标死，不再当替补；
   - 续期后仍过期的账号跳过不发请求，最终回 502
     「码道登录态失效，请重新打开码道桌面端登录（账号 … 登录态已过期且无法续期）」；
   - `/health` 的 `account`/`expiresAt` 改报**真正会拿来发请求的账号**
     （此前按文件顺序挑第一个未判死账号，长期显示早已过期的存档快照）。

## 实测记录（2026-10-26，本机）

- 本机码道版本：Electron 版，`product.json` 的 `auth.clientId = codearts`（不是旧 VS Code
  版的 `codearts-agent`），`inferhubBaseUrl = https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/`。
- `GET /v1/model/builtin` → 9 个模型：`GLM-5.2`、`Qwen3-VL-235B`、`kimi-k2.6-vl`、
  `openpangu-2.0-flash`、`openpangu-2.0-pro`、`glm-5.3`、`glm-5.2-sft-harmony`、
  `deepseek-v4.1-flash`、`kimi-k3`。
- `GET /snap-manager/v1/statistics/plugin` → `usageTotalPackageCredit` 7500 / 已用 120.11。
- 真实对话：流式 `chat.completion.chunk`、非流式聚合均 200，`reasoning_content` 正常。

## 2026-11-13：活动积分 / 福利 / 看门狗（照社区实现补齐）

来源：`HITZY2002/codearts2api` 的 `internal/upstream/checkin.go` 与
`Darkstarrd-dev/tinylab` 的 `internal/jethub/codearts_credits.go`（两份互相印证）。

| 块 | 做法 | 位置 |
|---|---|---|
| 活动领取 | `GET /v1/ops/delivery?channel=IDE` → `POST /v1/ops/claim`（`{campaignId, channel:"IDE", idempotentKey}`）→ `POST /v1/ops/confirm`（claim 响应 `data.id` 非空才做）。**按 `type` 选活动**，`campaignId` 数字/字符串都兼容；所有 `claimable` 活动都领（`USER_LOGIN` / `NEW_USER_REGISTER` / `INVITE_USER` / `STUDENT_CERTIFIED` …） | `activity.rs` + `upstream.rs` |
| 账本 | `codearts-activity.json`：签到按自然日（UTC+8）、其他活动每账号一次；失败不影响推理健康 | `activity.rs::Ledger` |
| 福利模型 | `opengw.developer.huaweicloud.com` 的 `/api/v1/benefit/claim` + `/api/v1/gateway/config`；只有**确认的**福利模型才带 `maas_type: benefit`（内置同名模型不能带，否则 APIG.0101） | `upstream.rs` + `lib.rs::is_benefit_model` |
| 看门狗 | 60s 一拍：启动/每 30 分钟跑活动；30 分钟内到期的号提前续期；10 分钟无活动打一次积分接口保活 | `lib.rs::background_loop` |
| 并发闸门 | 单账号默认 3（上游实测上限 3），`CODEARTS_MAX_CONCURRENT` 覆盖 | `lib.rs::permit` |
| 模型可用性学习 | `002002009 not registered` / `benefit not found` → 记 30 分钟不可用、从 `/v1/models` 过滤；小写别名聊天时归一成精确 ID | `lib.rs::mark_model_unavailable` / `canonical_model` |
| 本机登录态 | `secretStorage` 有多个 `.enc` 时逐个解密、取 `expiresAt` 最晚（2026-11-13 实测：桌面端新写的是 184 B 空壳，真会话在另一份里） | `import.rs::read_local_session` |

端点：`GET /activity`（账本）、`GET /activity?campaigns=1`（只读预览活动列表）、
`POST /activity/claim`（立即领取）、`GET /models/benefit`（福利网关诊断）。
开关：`CODEARTS_WATCHDOG=0`、`CODEARTS_ACTIVITY_AUTO=0`。

**2026-11-13 实测**（本机有效登录态）：
- 活动列表返回 4 条：`USER_LOGIN`（100/天）、`NEW_USER_REGISTER`（4000，已领）、
  `INVITE_USER`（1000，当前 `status` 为空＝未获邀请资格）、`STUDENT_CERTIFIED`（4000，未认证）；
  无可领活动时 `/activity/claim` 返回「无可领取活动」，不报错。
- 福利网关当前回 `4000 invalid channel, group not found`（通道下线口径，与 2026-10-26 结论一致）；
  失败按 30 分钟缓存、按内置目录路由，不影响正常聊天。
- `glm-5.3-flash` 等内置模型不再被误加 `maas_type`（修复了实测到的 `APIG.0101.404`）。

### OAuth 登录（/login/*，2026-11-13）

不依赖码道桌面端，直接走华为云 OAuth2（PKCE + DPoP，照抄
`HITZY2002/codearts2api` 的 `BuildAuthorizeURL` / `ExchangeCode`）：

| 端点 | 作用 |
|---|---|
| `POST /login/start` | 在 `127.0.0.1:<随机端口>` 起回调服务，返回授权链接与 `id` |
| `GET /login/status?id=` | 查询结果（`done` / `error` / `account`） |
| `POST /login/complete` | 远程部署时手动提交回调 URL 里的 `code`（`{id, code}` 或 `{id, url}`） |
| `GET /login/list` | 列出所有登录会话 |

流程：`portal/authorize?theme=2&locale=zh-cn&uri_scheme=codearts-agent&client_id=codearts-agent
&port=…&code_challenge=…&code_challenge_method=SHA-256&ticket_id=…&plugin-name=snap_AIIDE
&plugin-version=5.2.0` → 浏览器登录 → portal 回调两次（先 `secret+redirect` 307，再 `code`）
→ `POST sts…/v1/oauth2/tokens`（`grant_type=authorization_code` + `DPoP`）→ 存账号
（含 `refresh_token`、`code_verifier`、`dpop_private_jwk`、`client_id=codearts-agent`）。

DPoP 私钥**必须**与 refresh_token 一起保存，换密钥刷新会被 STS 拒。
实测（2026-11-13）：`/login/start` 返回的授权页 HTTP 200（804 B），回调端口监听正常；
完整登录需人工在浏览器完成（未在本轮验证里走完）。

### 协议适配（2026-11-13）

| 端点 | 做法 |
|---|---|
| `POST /v1/messages`（Anthropic） | `system`/文本/图片/`tool_use`/`tool_result`/`tools`/`tool_choice` → chat；回包翻成 `message`（文本 + `tool_use`，`stop_reason` 映射）；流式翻成 `message_start` / `content_block_start|delta|stop` / `message_delta` / `message_stop` |
| `POST /v1/responses`（OpenAI Responses） | `instructions`/`input`（字符串或 item 数组：`message`/`function_call`/`function_call_output`）→ chat；回包翻成 `response` 对象（`output_text` + `function_call`）；流式翻成 `response.created` / `output_item.added` / `content_part.added` / `output_text.delta` / `output_text.done` / `completed` |

工具调用在流式里按 `tool_use` block / `function_call` 尽力透传；不认识的块丢弃。
实测（2026-11-13，真实上游 `glm-5.3`）：非流式两个端点均 200 且中文正确；
流式 Anthropic 收到 `content_block_delta`（"温暖明亮。"）、Responses 收到文本增量。

## 验证命令

```powershell
# 单测（签名向量 / SSE 聚合 / 账号解析）
cd rs-codearts
$env:CARGO_TARGET_DIR='E:\cargo-target\workspace'
cargo test

# 起测试端口（不要碰生产端口；生产由 GUI 点火）
# target 是一次性缓存（2026-10-30 已清场回收），先 build 再跑，别直接抄 exe 路径
cargo build
& "$env:CARGO_TARGET_DIR\debug\rs-codearts.exe" --plugin-codearts --port 18793
curl http://127.0.0.1:18793/health
```

## 未做（后续）

- TLS 指纹复刻（见文首「未做（有意的）」）。
- 福利网关领取：接口已实现并接到看门狗/模型发现里，但上游 2026-11 实测仍回
  `4000 invalid channel, group not found`（通道下线）；恢复后无需改代码。
（DPoP 自动续期、多账号轮转、活动签到/邀请领取、OAuth 登录与多协议适配均已落地，见上文。）
