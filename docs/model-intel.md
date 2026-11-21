# 模型情报（实测细目）

> 这份文件是 `CLAUDE.md`「模型情报」一节的**完整细目**，2026-11-21 洁癖收尾时整段搬出：
> 规则文件超过平台 64 KiB 预算会被截断，连文末「团结凭证链红线」都会丢。
> 规则层只留要点与边界；**实测数据、时间线、单次事故的过程记录都在这里**。
> 动任何平台的请求形态、模型池或账号策略之前，先读完对应小节。

## 目录

- 团结（8788）—— 模型 ID / 费率 / K3 兜底 / 400 归因 / 风控识别特征 / 种子卫兵 / 两条版本轨道
- WorkBuddy（8787 / 国际版 8789）—— 模型列表机制 / 元数据纪律 / hy4 兜底 / `$ref` 内联 / 11128·11140 / 身份头 / 封号轴 / TLS 指纹
- B.AI（8891）—— 换皮实证与 WAF 量级
- Comate（8786）—— 两条传输 / zulu 无法注入工具 / 1.8.1 适配三根因 / 云直连落地与五个真机坑

---

## 模型情报（实测结论，血泪换来的）

### 团结（8788，tuanjie）——主力
- 可用模型 ID（2026-09-06 实测 /v1/models，大小写敏感）：`KIMI-K3`（全大写）、
  `GLM-5.3-FLASH`、`codely-core/basic/flash/air/vl`；**2026-09-15 上游新增原生
  `DeepSeek-V4.1-Flash`（`?full=true` 共 8 个，裸 `GLM-5.3` 不在内）**；
  **2026-10-20 实测 10 个：新增 `MiniMax-M3.1-Flash`，`GLM-5.3` 也在内**；
  上游当时给 codely 别名配 `display_name` 后端名（core→GLM-5.3、basic/flash/air→
  DeepSeek-V4-Flash；**2026-10-20 复核已变 V4.1-Flash，见下条**），
  **矩阵与按账号列表只认 id，不认 display_name**
  （v3.17.0，已实测 78363 免费号 5 个；**2026-10-20 实测 56574 号 10 个**）。
  **白名单按团队不同（2026-10-20 复核）**：78363 免费号只剩 `alias-only-proxy-models`
  （直连 GLM-5.3 / DeepSeek-V4.1-Flash / MiniMax-M3.1-Flash 全 401）；池内 56574 号 6 个——
  `alias-only-proxy-models / KIMI-K3 / GLM-5.3-FLASH / DeepSeek-V4.1-Flash / GLM-5.3 /
  MiniMax-M3.1-Flash`，三个原生名直调均 200。
  **codely-core 曾是 GLM-5.3 的承载入口**（2026-09-06 用户确认，当时直连 401）；
  2026-10-20 起池内账号可直连 GLM-5.3，core 不再是唯一入口
- codely-basic/flash/air 三档同源、仅推理深度不同（2026-09-06 实测同题：
  basic 3.8K / air 6.8K / flash 9.4K reasoning tokens）。
  **2026-10-20 复核（旧值 `deepseek-v4-flash-0731` 已作废）**：上游 `display_name`
  已改成 `DeepSeek-V4.1-Flash`（`owned_by: deepseek`）；原生 `DeepSeek-V4.1-Flash`
  条目在模型最多的账号（56574）下 `display_name` 是 `(1017) DeepSeek-V4.1-Flash`。
  两者**不是同一部署**：别名组响应 id 形如 `02<毫秒时间戳><随机hex>` 且带
  `service_tier`，原生是 32 位 hex、无 `service_tier`；同题 temperature=0 推理内容
  与部分答案不同（哈希表题：basic「按键哈希映射存储并快速查找的数据结构。」vs
  原生「键值映射实现快速存取的数据结构。」），但 tokenizer 一致（同 prompt 的
  `prompt_tokens` 逐题相同）。`/model/info` 仍 403，部署名读不到——**(1017) 只是
  上游标签，不能据此断言 basic 用的是哪个构建**
- 费率（官方 /model/info）：codely 系 1.6/3.2；GLM-5.3 3.2/11.2；KIMI-K3 16/80（贵，慎用）。
  **⚠ 2026-09-06 晚实测 `/model/info` 上游已拦（nginx 403 HTML，带签名也一样），
  代理正确回 502——费率数字是历史缓存，不再能实时刷新**
- **KIMI-K3 429 兜底（pacing，GUI「K3 限流护航」拨杆 /kimi-pacing）**：model 含
  "KIMI" 才启用；池路径两层——即时换号（≤3 次，换前 1s 退避）+ 全池撞墙按
  Retry-After 排队重发（单请求 30 分钟总预算）；单账号路径只有等待层。v3.8.6 起
  换号/排队/恢复事件全程进实时动态（流式成功本无 ok 事件，恢复事件由兜底自发上报）。
  2026-09-06 全链路实测通过：真实 429 → 换号×3 → 全池排队 60s×2 → 恢复 200
  （含 usage），与设计一致
- GLM-5.3：≥825K 上下文，effort 参数时灵时不灵（上游 bug）；**上游过载时偶发
  429 和"200 空响应"**（空响应重试已实现在 tuanjie/server.go 的 ensureNonEmpty，5eeca56）
- **400 "Invalid model name passed in model=None" 归因已推翻（2026-09-10）**：该报错的
  真正成因是**上游收到非法 UTF-8 的请求体**（详见坑 14），**不是**上游实例映射失步。
  证据：①同一份 JSON，内容换成裸 GBK 字节 `C4 E3 BA C3` 必 400、换回真 UTF-8 即 200，
  与 body 长度无关（同 6 字节的 `abcdef` 通过）；②全量日志里 64 次该报错**无一例外落在
  单条消息的探测请求**上，12k+ 真实 agent 请求零命中；③PostToolUse 审计日志反查到当时
  的探测命令正是 `curl -d '...中文...'`。旧「按模型区分 / 实例级失步」结论建立在同一批
  被 GBK 打碎的探测数据上，一并作废。代理现已本地拦截并回可读 400（v3.10.4），
  且不再对该确定性错误做无谓重试
- **2026-09-04 实测三条**：
  - codely 别名**当时**映射：codely-core→glm-5-fp8-128k、codely-vl→glm-5.3-flash、
    codely-flash/basic/air→deepseek-v4-flash（ga-260731/0731 双部署负载均衡）
    ——**2026-10-20 复核：basic/flash/air 已换 DeepSeek-V4.1-Flash，见上文**
  - 官方团队白名单实测收紧为 alias-only-proxy-models / KIMI-K3 / GLM-5.3-FLASH：
    GLM-5.3 直连已 401（team_model_access_denied），桌面客户端模型目录里的 GLM-5.3
    选项不可用（目录与权限不匹配，非代理问题）
  - rc.55→rc.58 逆向四项无变化（签名种子与两层 HMAC / telemetry 不发 x-b3 /
    metadata 四字段 / UA 构造）；UA 无版本门槛（rc.54/55/58 直连全 200）；
    反竞品 system 检测仍活跃；GLM-5.3 TPM 滑窗参数留静默窗口实测

### ⚠ 团结风控识别特征（2026-08-25 实测；2026-09-04 解包实证重大修正）
- **状态**：2026-08 官方被封过号（Unity 登录锁死、access token 失效、网页无法登录）。
- **🔴 2026-09-04 推翻「入口域名最硬差异」结论**：解包 rc.58 bundle 实证，官方 CLI 的
  chat/模型请求**同样直连 `codely-litellm.tuanjie.cn`**（gemini.js 内 chat base 常量 +
  签名函数专门只对该域名及其子域签名）；`codely.tuanjie.cn` 只用于登录鉴权、dashboard、
  埋点、技能下载。08-25「抓包 894 包全见 codely.tuanjie.cn → 直连 litellm = 一眼反代」
  的旧结论**作废**（成因未定：当时 CLI 版本更老或抓包时段无 chat 流量）——
  **入口域名这个轴上我们与官方一致，无需也无处改路径**。
- **现行风控认知**：封号轴心 = session + 签名凭证链（官方客服原话，见下方红线节）；
  UA 无版本门槛（rc.54/55/58 直连全 200）。**TLS 指纹一条已推翻旧认知（2026-10-16 实证）**：
  官方两条轨道（npm rc.60 的 `gemini.js`、桌面端 release.52 的 `codely.exe`）都是**纯 Node
  默认 TLS**（裸 fetch，无自定义 ciphers/ALPN/agent；桌面端是 Node SEA 单文件、不是
  Electron）；Chrome 口音只存在于**我们自己的 Go 反代**（`utls_transport.go` 的
  `HelloChrome_Auto`），没有任何官方客户端说这种口音——它是长期被上游容忍的自选行为，
  不是官方同源。若上游将来按口音执法，正确动作是 Go+Rust 一起搬到 Node 口音。
- **次一级差异**（本地 8788 诊断日志对比，仍成立）：官方 CLI 请求体极简
  （model/max_tokens/messages、2 条消息、无 stream/tools）；ZCode/agent 带
  stream_options/tools/tool_choice、超大消息。协议角色差异（gemini 系角色 vs OpenAI
  角色）疑似存在未实锤。
- **种子卫兵（v3.8.5 上线）**：封号轴 = 签名 → 盯官方种子即盯封号轴。
  internal/tuanjie/seedcheck.go 双层监控：①本机发行件特征扫描（种子 hex +
  X-Codely-Signature 头名）；②npm registry 在线核对——每小时查 latest 版本号，
  **版本变了才下载新包验特征（约 16MB 一次性），没变零下载**。
  任一层特征消失 → /health `seed_alert=true` + GUI 警示条；字段
  seed_alert/seed_signal/seed_latest/seed_online；网络故障只记状态不误报。
  2026-09-04 实测：rc.58 种子与签名算法逐字段一致、registry latest=rc.58。
  旁证链：种子真轮换 → 全线 401 → 连续 3 次触发 judgment_alert（client 自动换 key
  不掩盖告警）；⚠ team_model_access_denied 的 401 同样推高计数（潜在误报源）；
  探针/水印旁路的 401 不进告警链；告警状态只反映在 /health 与 GUI，不写日志文件。
- **⚠ 两条版本轨道（2026-09-13 补，桌面端 2.0.9 更新时发现）**：官方 CLI 在本机
  有**两条独立版本轨道**，卫兵**两条都盯**（localCliSources 多源扫描，任一份特征
  消失即告警、signal 带来源名）：
  1. **npm 轨道**（rc.x）：`%APPDATA%\npm\node_modules\@unity-china\codely-cli\bundle\gemini.js`
     ——反代 UA 的版本号就取自这里（client.go detectLocalCliVersion）
  2. **桌面端轨道**（release.x）：团结 Cowork 内置 `cli\bin\win32-x64\codely.exe`
     （203MB 编译产物，分块流式扫描，三条轨道共 ~120ms）——**npm 上根本没有
     release 轨道**，只随安装包分发；路径取注册表卸载表项 InstallLocation
     （用户可装任意盘），兜底 `%LOCALAPPDATA%\Programs\Tuanjie Cowork`
  两条轨道版本号不同步（2026-09-13 实测：npm=rc.58/registry latest=rc.60、
  桌面端=release.57），**只盯 npm 会漏掉桌面端先轮换种子的情况**——这正是本项
  补丁的动机（原先只扫 npm，桌面端轨道完全没被监控）。
  注意 `app\resource\core\bin\win32-x64\codely-binary.exe` 是 Node 运行时壳、
  **不含签名特征**，不能列入候选（列了会因「文件在但特征缺失」误告警）。
- **凭证纪律**：tuanjie-accounts.json.bak-* 内含有效 JWT，gitignore 已补通配
  （2026-09-04，ebaf3c9）；账号备份文件一律不准进 git。

### 可选缓解（不保证，风险自担）
- 请求形态裁剪（去 stream_options / 隐藏 tools / 消息合并）理论上降低次一级差异，
  但损失 ZCode 的 Agent 工具调用能力或引发协议问题，默认不做；
  真·规避（换号、深度伪装）不在本项目职责内，也不建议投入。

### WorkBuddy（8787）——子智能体可用
- 支持 tool_calls（实测返回过标准工具调用），可当 Agent/子智能体模型
- glm-5.2 / kimi-k3 等可用
### 参考实现（2026-11-21 起必读）

[dingminhua/dsh-connect-workbuddy](https://github.com/dingminhua/dsh-connect-workbuddy)（**MIT**，
Copyright (c) 2026 LaoDing，2026-11-21 仍在更新）：一个 DSH bundle 插件，把本机登录的 WorkBuddy
（国内 + 国际双 provider）接进 DSH 模型选择器。**它比 magpie 更贴我们的问题**，下面几条硬结论出自它：

- `/v3/config` 是模型目录端点（按 App 自己的客户端身份读，UA 决定给多少）；
- 11128 的「三道门」：消息开头、会话 id 有没有、**首条是不是 system**；
- 改写在 `prepareChatBody` 里按句改词，覆盖 `@` 路径整段；
- CLI 桥（`cliBridge`，默认关）：让本机 CLI 自己跑任务；它也明确「伪装过渠道校验是军备竞赛，会出现 11140」；
- 国际版两个品牌域：桌面端 `workbuddy.ai`、CLI `codebuddy.ai`，都算国际版。

借鉴纪律：它自己是「借鉴设计 + 独立实现」，并把参考项目写进 `THIRD_PARTY_NOTICES.md`；
我们若复用其代码，必须保留 MIT 版权声明。

- **模型列表机制（2026-09-10 定案 → 2026-11-21 推翻）**：旧结论是「上游没有任何模型列表
  端点，`modelCandidates` 硬编码 + 每小时探测，**发现不了上游新增**，加模型要动三处」。
  **2026-11-21 实测发现 `GET /v3/config` 就是模型目录端点**（此前只查了 `/models`、
  `/v1/models`、`/v2/models` 三个 404 路径，没试 `/v3/`）：
  * **UA 决定给多少**：`User-Agent: WorkBuddy/5.9.3` → 39 个模型（27 可对话）；
    `WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0` → **58 个（44 可对话）**；
    `codebuddy2openai/1.0` → 400。`X-IDE-Type/Name/Version`、`X-Private-Data`
    **加不加字节数完全一样**，只有 UA 有效。
  * 每条带 `credits`（倍率 `x0.00`~`x1.62`）、`maxInputTokens`/`maxOutputTokens`、
    `supportsImages`/`supportsToolCall`/`supportsReasoning`、`reasoning.effort`、
    `tags`、`vendor`；另有 `modelPromotions`（促销/夜间折扣）与 `modelTiers`。
  * 国际版同一端点同样可用（28 个模型 / 26 可对话，含 GPT-6.2、Grok-4.7、Gemini-4 系列）。
  * 凡是「按能力位过滤」而不是按名字猜：目录里混着补全/生图/生视频
    （`codewise-*`、`hunyuan-image-*`、`kling-video-*`），接 `/v1/chat/completions` 只会报错。
  * v4.9.11 起 `/v1/models` 走目录（`CODEBUDDY_CATALOG=0` 关）；读不到回退旧候选池。
  * 8787 由**编译进 exe 的 Rust 后端**提供（`rs-codebuddy`），`plugins/codebuddy2api/`
    是早期 Python 实现、已不在链路上
- **模型元数据纪律（2026-09-02 定）**：`/model/info` 的 modelMeta 只准填实测值
  （reasoning 实测矩阵：deepseek 系与 auto 拒 off，其余 11 个 off~max 全接；
  maxInput 仅 glm-5.2 / deepseek-v4-pro / deepseek-v4-flash 三条实测 1M）。
  查无实据的上下文/最大输出一律缺省不填——GUI 悬停会如实显示「未核实」，
  禁止照抄官方宣传数字回填（曾因统一写 low/medium/high 误导 Agent 填出 400）
- **hy4-preview 限流兜底（v3.8.1）**：撞配额 429 自动切 fallback（缺省
  deepseek-v4-pro）并按上游重置时点倒计时，到期自动恢复；GUI 甲板有开关
  （/v1/failover GET/POST，配置持久化 codebuddy-failover.json；v3.8.6 起
  POST 响应同构带 hy4_limit、设置甲板拨杆行带三态状态字「未限流/已限流跑
  fallback/关」、警告条随 fallback 配置显示不再写死；v3.14.0 起点阵屏的
  「hy4 兜底」状态行已删（纯展示无功能，用户裁决）。实测教训：
  上游限流是滑动窗口+按日配额双形态
- **工具 `$ref` 内联（2026-10-14 K3 实证，`sanitizeToolRefs`）**：腾讯不解析工具
  schema 里的 `$ref`/`$defs`（158 个 MCP 工具整批 400/11133，二分到单工具；
  `oneOf`/兄弟键/`$schema` 等极简探针全过——毒的是 `$ref` 指向含复杂构造的
  定义块时的组合形态）。转发前无损内联（兄弟键按 2020-12 合并、循环截断、
  外部引用保留、无 `$ref` 原样），零残留即过。11133 另有自动退避重试
  （上限 2 次）＋失败现场 `diag-11133-*.json` 落数据目录（上限 10 份）兜底

- **11140 与 11128 是两回事（2026-10-24 实测定案）**：11128 是渠道校验（请求形态/内容指纹，
  补会话头/扰动可过）；**11140 = 账号级限制**（`403 request illegal` +
  「账号存在异常行为，已被限制使用」），改头/换号/重登都无效，只能走官方申诉。
  国内/国际都有；国际版社区大量「封号」实为 11128 误判（`workbuddy2api-panel` #197）。
  **已落地分流（v4.6.1）**：11140 + 账号限制文案 → 标死该号并换健康号（有界）；
  全池被限 → 403 + 申诉指引；纯内容策略的 11140 → 原样透传、不标号。
  判据见 `rs-codebuddy/src/chat.rs::is_account_restricted_body`。
- **11128 自愈（v4.9.9，抄 magpie 2026-10-30 实证）**：上游对身份句是**整句精确匹配**，
  插一个逗号就过（`You are an AI agent powered by X.` → `…agent, powered by X.`），
  加词/改大小写都没用。`rs-codebuddy/src/reword.rs` 按需改写：规则表（通用身份句插逗号、
  Claude Code 去 `official`）→ 只动 system/assistant → 命中即**同号重发、号不冷却**
  → 成功后记原文指纹，后续 `pre_reword` 直接按改写形态发（一条 prompt 只吃一次拒绝）。
  规则没命中时退到 `move_system_to_user`（system 挪进首条 user，上游不读 user）。
  **⚠ 判据陷阱（2026-11-21 实测）**：上游**所有** 11128 的 `displayTips` 都写
  `"This call failed channel verification…"`——旧实现拿这句当子类型判据，结果 11128
  一律走「通道校验、原样透传」，改写与按号冷却都是死代码。别再用它分流。
- **官方网关身份头（2026-10-24 取 CN 5.8.3 `app.asar` 的 `applyGatewayHeaders`，形态仍适用）**：官方每条
  chat 带 `X-Conversation-ID/-Request-ID/-Message-ID`、`X-Request-ID`、
  `traceparent`/`b3`/`X-B3-*`/`X-Trace-ID`、`x-codebuddy-request: 1`、
  `X-Agent-Intent: craft` / `X-Agent-Purpose: conversation`、`X-IDE-Type/Name: WorkBuddy`、
  `X-IDE-Version: 0.0.0`、`X-Private-Data: true`、`X-Product: SaaS`。
  `rs-codebuddy` P0 已落地（`chat.rs::IdentityHeaders`）。**UA 自 v4.9.8 起跟官方当前版本**：
  CN `WorkBuddy/5.9.3`、INTL `CLI/2.185.0 CodeBuddy/2.185.0`（2026-11-21 解包 npm
  `@tencent-ai/codebuddy-code` 2.185.0 + 本地抓包实测；旧值分别为 `WorkBuddy/5.8.3` 与
  `CLI/2.168.2`）。⚠ **改 UA 必须同步重采 `tests/golden/golden-codebuddy-go.json`** 的
  期望头与 `region_*` 快照，否则 golden 门禁直接红（v4.9.8 就漏跑过这条）。
- **封号轴是账号/行为层，不是单条请求**：同机同请求头仍可一号 200 一号 11140；
  多账号池、同 IP、自动签到、uid 进 id 都会放大关联风险。**P1 已落地**：
  `X-Conversation-ID` 跨轮稳定（`chat.rs::conversation_id_for`，显式
  conversation_id / prompt_cache_key 优先，否则首条 system+user + uid 哈希；
  跨账号隔离）；设备身份持久化、TLS/HTTP2 指纹、行为节流仍待做。
- **TLS/HTTP2 指纹（2026-10-25 实测，未对齐）**：本地 CONNECT 代理抓 ClientHello 算 JA3。
  ProxyDeck（reqwest default-tls → SChannel）`68caad18…`；官方桌面 Electron/Chromium
  cipher/curve 组为 `4865-4866-4867-49195-49199-…` + `10-4588-29-23-24`（扩展顺序随机化，
  JA3 多变）；官方 CLI Node/OpenSSL `67ae7553…`。三者互不相同——当前上游闸门是 HTTP 层
  （11128），未见 TLS 执法证据；若将来启用，需评估 `rquest`/BoringSSL 的 Chromium 指纹，
  rustls 只能靠近 OpenSSL、仍非 Chromium。测量脚本与完整数据见
  `.work/tls-fingerprint-2026-10-25.md`（未入库）。

**2026-11-05 照社区教科书对齐（照抄，不自己研究）**：GitHub 上 `yreidev/workbuddy2api-panel`
的 `docs/official-client.md`（官方桌面端逐项对照）与 `ardeyouxipianyi/workbuddy2api-hub` 的
`docs/TROUBLESHOOTING-11-128.md`（排障纪律）是目前最全的两份资料。已落地：
① 对话补 OpenAI Node SDK 6.25.0 的 8 个 `X-Stainless-*` + `X-Agent-Type: main` +
`X-Agent-Runtime: local-agent`；② 会话 ID 改官方 uuidv7 形态（conversation 36 位、
request/message 32 hex、`X-Request-ID` 与消息 ID 同值）；③ `11158` 记「该号不接该模型」
黑名单并换号（全池黑 → 400 `model_unavailable`）、`11145` 并入额度耗尽、`11115`/prompt too long
不罚号；④ 风控波保护：15 分钟 2 个号被 11140 → 其余号暂停 30 分钟（`/v1/wave` 可解除，
`CODEBUDDY_WAVE_PAUSE_SECS=0` 关闭）；⑤ 按号代理：auth 文件写 `proxy_url`/`proxyUrl`/`proxy`
（顶层或 `auth` 内），该号对话与刷新 token 都走该出口，每代理独立 client。未做：TLS 指纹复刻
（要换 BoringSSL/uTLS 栈，当前 reqwest+SChannel 做不了），社区证据表明是最后才需要的轴。
验证按号代理时（本地 CONNECT 代理 + 注入 proxy_url）实测：代理侧收到 CONNECT、上游 HTTP/2.0；
期间 **账号 516235a0 被上游 11140 限制**（此后直连与代理都 403，非代理引入），
正好实测到 11140 分流：插件回 403 + 申诉文案并标死该号。

### B.AI / b.ai（8891）——已重新接入（v3.5.0），但保留当年实测结论
- **现状**：`internal/bai`，COMMAND 键副页，Go 栈透明转发 `https://api.b.ai`；
  模型矩阵走 GUI 专用旁路 `/model/matrix`（详见 `docs/bai-model-matrix-design.md`）
- **当年实测结论（2026-08-19 曾据此整个拆除，commit 9fd9bf9；2026-09-06 带真实 key
  复测，换皮部分实证）**：其 "deepseek-v4-flash" 实测为 2025 上半年的 DeepSeek 推理系
  模型换皮（**2026-09-06 自报知识截止 2025-05、不知道 2025-12 的 V4，坐实**；且 6 个
  免费模型指纹题 prompt_tokens 各不相同 95/46/288/43/37/116——不是同一后端套皮）。
  `glm-5.3-flash` 同样换皮（**自报截止约 2025-01~03，对不上自家名**）；`hy3` 自称混元、
  截止 2024-07（名字与自称至少一致）。CF WAF 滚动封锁长请求（1010）：历史封的是 6-8M
  字符级，380K 字符实测 200 通过（未往上推，原结论保留）——**这一路不适合当主力 Agent 后端**
- 上游 `/v1/models` 实测只给 `id/object/created/owned_by/supported_endpoint_types`，
  `created` 恒为占位值、无任何价格或免费字段；`/model/info`、`/key/info` 等元数据路径一律 403
  （网关只放行推理路径）。**别指望从接口自动判定模型是否免费**
- **2026-09-06 晚带真实 key 实测补充（免费模型随便测：qwen3.8-flash/glm-5.3-flash/
  mimo-v2.5/hy3/deepseek-v4-flash/deepseek-v4-flash-vision-exp）**：
  - adaptQuirks 的 developer→system 改写**实证必要**：同请求直连上游 400
    （`developer is not one of ['system',...]`）、过 8891 即 200
  - max_tokens 钳到 8192 现为**预防性而非必需**：直连 99999 上游也 200（上游行为
    已变，钳制无害保留）
  - 全链路通过：live /v1/models 48 个（与缓存矩阵一致）、非流式/流式/长请求 380K
    字符均 200；未覆盖：6-8M 字符级 WAF 极端量级（不值得花 token 复现）

### Comate（8786，百度文心快码）——上游自带 agent 工具，不是纯聊天后端
- **两条传输**（v4.9 起 rs-comate 内置，`COMATE_TRANSPORT` 切换）：
  **云直连（默认，`cloud`）** 打 `https://comate.baidu.com` 三步协议，
  无本地进程、支持客户端工具、有真实 usage；**zulu serve（`zulu`）** 是老的
  `--plugin → 8792 /api/v1/conversations/init` 路线，留作对照与兜底。
  zulu 是 **agent 引擎**（`comate-engine` = @comate/kernel：沙箱命令执行、MCP、
  sqlite-vec 代码库索引、LSP、git），`--mode` 默认 Agent（可选 Ask/Plan/Cloud/Bot）。
- **⚠ 工具无法由客户端注入（此结论只对 zulu 路线成立）**：zulu 会话请求体字段全集只有
  query / cwd / ptoken / license / model / conversation / mode / enableCodebaseSearch /
  activate{Skills,Commands,Subagents,Rules} / attach{Images,Files}——**没有 `tools`**。
  所以「让 ZCode/DSH 自己的工具被 comate 模型调用」在 zulu 路线无解；
  **2026-11-15 起默认改走云直连，`tool_calls` 已实测可往返**（见下节）。
- **工作目录跟随（v3.10.5，仅 zulu 路线）**：插件从客户端提示词提取
  `Working directory:`/`工作目录` 标记（`rs-comate/src/cwd.rs`，与 `rs-qoder/src/cwd.rs`
  同源逻辑），`COMATE_CWD` 可钉死、`COMATE_MODE=Ask` 只读。**此前 cwd 写死临时目录、
  引擎工具在空目录里空转，表现与纯聊天模型无法区分——2026-09-10 曾据此误判
  「comate 只能当聊天用」，实为插件缺陷**。云直连不跑本地引擎，不看 cwd。
- **`zulu run` 的 query 走命令行参数（历史约束）**：Windows 上限实测约 32767 字符
  （32700 过 / 32800 ENAMETOOLONG）。这是 CLI 子命令的限制，**不是本插件的**——
  serve 走 HTTP、云直连走 HTTPS，都不受此限（云直连 query 只受请求体大小约束）。
- **zulu 路线的工具过程不可见（云直连已解决）**：zulu 上游有 tool-call-action /
  tool-params-delta / tool-output-delta 等事件，但代理只转发 `delta-batch`/`element-add`
  的文本（1.8.1 起 `REASON` 也会透成 `reasoning_content`）。要用客户端工具请走默认云直连。
- **上游偶发返回占位残渣（zulu 路线 2026-09-11 实测）**：两次里有一次 content 就是
  字面量 `<final>...</final>`（18s 返回，非报错），同请求重测即正常；代理如实转发、
  **comate 侧无空响应重试**（团结有 ensureNonEmpty，此处没有）。云直连截至 2026-11-15
  未复现；遇到空/残渣先重测再排查。

### Comate 1.108.2 / zulu 1.8.1 更新适配（2026-11-15 实测，已落地 rs-comate）

- **根因一：`PLATFORM=saas` 是硬门槛**。zulu 1.8.1 的 `PLATFORM` 默认 `internal`，
  会去打 `comate.baidu-int.com`（百度内网，公网解析不了）→ 会话创建拿不到
  `conversationId/taskId`（内核里 `taskId<0` 直接抛）→ 统一报「您当前会话执行遇到了异常，
  请检测网络后重试！」。取值只认小写 `saas`/`internal`/`poc`，**大写 `SAAS` 等于没设**。
  `list-model` 与 `serve` 两个子进程都已注入。**这是本次更新后 8786 全挂的唯一根因**。
- **根因二：流式协议换了一套**。旧：`delta-batch` 的 `text-delta`/`thinking-delta`（增量）
  + `{"kind":"completed","status":"ok","content":全文}`。新：`delta-batch` 里的
  `element-add`（首段）/ `element-patch`（**content 是累计全文，不是增量**）
  + `{"type":"task_done","status":"completed"}`（失败时正文位置先给 `EXCEPTION` 元素）。
  旧代理只认 `completed`，所以更新后一律 502「上游会话结束但未收到 completed」。
  `translate::SseAcc` 现在两套通吃；`REASON` 元素透成 `reasoning_content`。
- **根因三：登录态两个账号**。`~/.comate/cli-login.json`（CLI）与
  `%APPDATA%\Comate\User\settings.json` 的 `baidu.comate.license`（IDE）这次不是同一个号；
  用户在用/买会员的是 IDE 那个。解析顺序改为 `COMATE_LICENSE` → IDE settings → cli-login。
  **2026-11-15 用户确认：会员是充在「百度搭子」的，权益落在 IDE 登录的百度账号上**——
  也就是代理默认用的那个；启动日志会打印「来源 ide-settings（账号 xxx）」并警告两个号不同，
  要换号设 `COMATE_LICENSE`。账号名只进本机日志，**不进仓库**。
- **`zulu serve` 是单例**：读 `~/.comate/zulu-serve.pid`，命中就「复用」并立即退出，
  **不看 `--port`/`--license`**（实测 `serve --port 8892` 回 `{"port":8792,"reused":true}`）。
  IDE 自己拉过 serve（如 8779）时我们 spawn 的进程原地退出、8792 永不健康。
  现在 spawn 前若 pid 文件在别的端口就先 `taskkill`；若 8792 上的 serve license 不匹配则
  403 自愈（重启 serve + 重试一次）。代价：IDE 重启后第一次对话会把它自己的 serve 换掉，
  两边都用 IDE license，之后会互相复用。
- **模型目录**（`zulu list-model` 实时，15 个）：新增 `deepseek-v4.1-flash`（ratio 0.3x、
  支持图片/思考、effort low/high/max）、`ernie-5.5-flash-preview`（0.1x）；去掉
  `glm-5.0-fc`、`kimi-k2.5-oneapi`。`FALLBACK_MODELS` 有意未动（被 Go 黄金
  `models-fallback-*` 逐字节锁着，要改得连黄金重采）。
- **云直连路线可行**（GitHub 已有成熟实现，见下）：`https://comate.baidu.com/api/v2/api/models/available`
  + `/api/aidevops/autocomate/rest/autowork/v2/{conversation,task,execute}`，
  认证只需头 `login-name`/`Uuap-login-name` = license（无签名、无设备绑定）。
  本机实测：模型目录 200、非流式/流式对话 200、**工具调用（client tools）真能回 tool_calls**、
  带真实 usage 与 reasoning。zulu serve 这条路**结构上不可能**给客户端注入工具
  （init 请求体字段全集里没有 `tools`）——要做 Agent 工具得走云直连，属独立升级项。
  会员积分：Comate 与「百度搭子」共用权益，Pro 25,000 / Max 100,000 分/月，
  余额在 `/api/user/agent/balance`（要 Cookie，license 头不够）。
- 参考实现（.work/refs/ 已 clone）：`Suiying6023/Comate2API`（Python，零依赖，直连云，
  工具/图片/JSON 输出/凭证池；2026-11-13 更新）、`PengJunchen/magpie-comate-auth`
  （JS，native sessions + client-owned tools + 工具结果续接，VALIDATION.md 5 万字体检）、
  `ikashana/magpie-comate-auth`（会员积分卡 + 15 模型 + Ask(8)/Agent(1) 模式区分）、
  `vibe-coding-labs/ComateReverseEngineering`（逆向分析仓库）。

### 云直连已落地（2026-11-15，rs-comate 新默认传输）

- **代码**：`rs-comate/src/cloud.rs`（协议：HTTP + SSE 帧 + 工具参数按 key 追加归并）、
  `cloud_chat.rs`（OpenAI 侧：请求解析、假 MCP 声明、工具→tool_calls、工具结果续接）、
  `lib.rs` 的 `Transport::{Cloud,Zulu}` 与 `cloud_*` handler。
- **实测（同一台机同一个号）**：非流式 **2.0s**、流式首字 ~1.5s（zulu 是 16~22s）；
  工具调用 `tool_calls` 正常往返（模型调 `UseMcpTool` → 我们翻成客户端函数 →
  结果带回来续同一 conversation/task，`query=""`）；真实 usage（含 cached_tokens）；
  `reasoning_content` 正常。2026-11-15 三条活体链路都验过：纯文本流式/非流式、
  工具调用、工具结果续接。
- **协议要点**（照 Comate2API + magpie 的公开实现对了一遍）：
  `GET /api/v2/api/models/available` → `POST .../rest/autowork/v2/{conversation,task,execute}`；
  认证只靠头 `login-name`/`Uuap-login-name`=license（**无签名、无设备绑定**）。
  工具走 `agentInfo.mcpInfos` 的假 MCP 服务器 `openai_client`；上游回
  `FUNCTION_CALL_START/PARAMS_APPEND/END`，`input` 是**按 key 字符串追加**的碎片。
- **默认值与开关**：`COMATE_TRANSPORT=cloud|zulu`（默认 cloud）、`COMATE_CLOUD_BASE`、
  `COMATE_PLUGIN_VERSION`（默认 `zulucli-1.8.1`，Comate 更新后要跟着改）、`COMATE_LICENSE`。
- **测试**：`tests/cloud_contract.rs` 6 条（进程内假云）锁三步协议、完整 modelId、
  auth 头、tool_calls 往返、续接 query 为空、未知模型 400；单测 +6（帧解析、
  参数归并、真机帧序列、会话绑定）。zulu 老路径的 3 条黄金 + 10 条合同**原样全绿**。
- **工具名解析的坑（2026-11-15 真机）**：模型会把客户端工具**全名**塞进 `server_name`
  （如 `mcp__rea__inspect_kernel_build_config`），`tool_name` 只给短名
  （`inspect_kernel_build_config`）。映射**不能校验 server_name、不能只做全名精确匹配**：
  按短名做 `__` 后缀唯一匹配；认不出来的名字**原样透传**（客户端回 unknown tool，模型自己纠正）。
  旧实现两项都严 → 整批调用被丢 → 只剩 reasoning 然后 `stop`（DSH 里就是「说到一半断流」/空响应）。
  诊断开关 `COMATE_DEBUG_FRAMES=<文件>` 会把上游原始帧与每次结束摘要落盘（未设置时零成本）。
- **坏参数不丢（2026-11-15 第二例）**：模型偶尔产出非法 JSON 参数（真机：命令里的中文引号
  没转义）。旧实现校验失败即丢弃整批 → 又是「只有思考就停」。现在**代理侧一律产出合法 JSON**：
  原样 → 尾逗号/截断修补 → 实在不行退化成 `{}`，由客户端按自己的 schema 校验回给模型。
  **不依赖任何客户端特性**（DSH 的 ToolArgsError 只是其中一种；Cursor/Codex/opencode 同样成立）。
  插件日志每个请求一行 `[comate] finish …`，坏参数另打 `[comate] bad-args …`。
- **只预告不调工具（2026-11-15 第三例）**：弱模型（`ernie-5.5-flash-preview`）在大工具列表下会
  回一句「I'll start by…」就结束（finish 里 `content=78 native_calls=0`，上游正常收尾）。
  代理侧在 `agentPrompt` 里补了硬约束：决定用工具就必须在**同一条回复**里发出调用，
  不许只给计划/预告。另外该模型当时上游过载（同日志两次 502「请求较多」）。
- **缺工具名也不丢（2026-11-15 第四例）**：真机见过模型发了 2 个调用但 `tool_name` 缺失（名字另有归处）。
  现在依次认 `tool_name`/`toolName`/`tool`/`function`/`name`，再退 `server_name`；全空就发
  `unknown_tool` 占位（客户端回“不存在”→模型重发），绝不再整批丢弃。
- **上游过载自动重试（2026-11-15 第五例）**：上游会间歇回 HTTP 200 + EXCEPTION「请求较多/响应异常」；
  DSH 只重试两三次就放弃，用户看到「又断了」。代理现在在首字之前失败时自己重试 3 次
  （重新 prepare + 退避 1s/2s），仍失败才回 502。
- **没做**：图片输入、`max_tokens`/`temperature` 透传、`response_format`、
  跨请求会话复用（工具续接除外）、会员积分展示（余额端点要 Cookie）。

