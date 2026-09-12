# CommandCode Proxy Deck — 项目交接文档

> 本文件给 AI 助手（Claude Code / ZCode 等）和新协作者提供项目全貌。
> 记录了架构、已踩过的坑、协作规矩、当前状态。改动项目前先读完本文件。

## 项目一句话

Windows 独立 GUI 工具（单 exe，Go + WebView2 + 内嵌 HTML）：把 CommandCode /
团结 / WorkBuddy(腾讯CodeBuddy) / Comate(百度) / Qoder(阿里) / B.AI 的订阅转成
本地 OpenAI 兼容接口，机械风控制台统一管理。完全开箱即用，无任何激活/授权机制。

## 架构速览

```
ProxyDeck.exe        ← 唯一主程序，双击即用
├─ app/                         GUI 主体（Go + go:embed ui.html）
│   ├─ main.go                  入口：flag 分发（GUI/headless/--plugin-*）
│   ├─ bridge.go                WebView2 JS 桥（ccGetState/ccStart/...绑定）
│   ├─ plugins.go               插件托管：pluginDefs 注册 + 启动/停止/健康检查
│   ├─ plugin_modes.go          --plugin-* 子模式（进程内直跑插件服务）
│   └─ ui.html                  全部 UI（内嵌；四模式键甲板 + Qoder/B.AI/Comate/媒体/注水副页）
├─ internal/
│   ├─ proxy + server + api     主代理核心
│   ├─ tuanjie/                 团结插件后端（8788）
│   ├─ codebuddy/               WorkBuddy 插件后端（8787）
│   ├─ comate/                  Comate 插件后端（8786）
│   ├─ qoder/                   Qoder 插件后端（8785）
│   └─ bai/                     B.AI 插件后端（8891）
└─ build.py                     构建脚本（单模式，python build.py）
```

- 端口约定：主代理 55990 / WorkBuddy 8787 / 团结 8788 / Comate 8786 / Qoder 8785 / B.AI 8891
- 插件 = spawn 本 exe 的 `--plugin-<id> --port <n>` 子进程，独立常驻，关 GUI 不中断
- **升版本必须同步的位置**（漏一处就有地方显示旧版；2026-09-02 按实操勘误计数，
  2026-09-10 补第 8 项）：
  1. `app/main.go` `appVersion`
  2. `app/ui.html` `id="etchVer"`（顶栏蚀刻）
  3. `app/ui.html` `.pt-nameplate`（铭牌，含 `aria-label` 同行两处）
  4. `app/ui.html` `id="buildNo"`
  5. `app/ui.html` `id="verChip"`
  6. `app/ui.html` `var UI_CUR`（更新检查比对用）
  7. `app/ui.html` **`var CHANGELOG_DATA`**（甲板内「更新日志」浮层的**独立副本**，不读 `CHANGELOG.md`！
     只改仓库文件不改这里，浮层就还是旧版——v3.6.2 / v3.6.3 两次都栽在这里）
  8. `README.md` 版本徽章（shields.io 的 `Version-vX.Y.Z`）——**不在 `app/` 下，历次发版
     最容易漏**：v3.10.1~v3.10.3 连发三版全漏、徽章停在 v3.10.0，根因就是清单里从来没
     列过它（2026-09-10 发现并修）
  外加 `CHANGELOG.md` 顶条 + GitHub Release（正文＝CHANGELOG 顶条逐字，标题＝`vX.Y.Z · 主题 · 主题`，
  **必须附 `ProxyDeck.exe` 附件**——那是一键更新的下载源，漏传 = 用户点更新 404；
  2026-09-04 发 v3.8.5 时曾漏、事后补传）。
  改完用旧版本串 grep `app/ README.md` 复核一遍（应只剩 CHANGELOG_DATA 历史条目）。
  `CHANGELOG_DATA` 的 `date` 用**本机发布日**——跨零点发版最容易差一天（v3.10.5 于
  09-11 01:19 发布却写成 09-10）。历史条目**不回改**（会牵动 tag 与附件对应关系），
  下次发版顺手校正即可。

## 版本号三段位规则（2026-08-31 定，用户裁决）

`主版本.次版本.修订号`（如 3.7.0），判据只看**对用户的影响面**，不看代码量：

| 段位 | 什么时候动 | 例 |
|---|---|---|
| **修订号**（x.x.+1） | 修 bug、小文案、小参数，**不改任何交互习惯** | 灯误亮修好、字号微调 |
| **次版本**（x.+1.0） | 有**新功能或界面变化**，用户要重新认识某个界面 | 新平台甲板、新浮窗、双页改造 |
| **主版本**（+1.0.0） | **整体形态变化**：导航大改、多套界面重构、或一次含 ≥3 个次版本级功能 | 传真机+灯系统+双页这批升 3.7 即临界案例 |

铁律：
- **一次只动一段**，升次版修订归零，升主版次版归零。
- 禁止连跳（3.6.5→3.7.0 可以，→3.8.0 不行——中间没发过版）。
- 同号重发 = 违规（v3.6.5 叠 v3.6.5 是事故，历史已发生，不再重演）。
- **段位按"本版内容最高档"现算，不沿用旧口径**：v3.10.8（2026-09-12）装进了
  WorkBuddy 国际版（新功能 + 甲板内新增切换，本应 3.11.0）却按修订号发——起因是
  「下一版待办」那条注写在只有修订级修复（headless 去密钥）的时候，后来把新功能
  塞进同一版时没人重估段位。已发布的**不动**（同号重发违规、强推 tag 更不许），
  v3.11.0（2026-09-12）已按次版本段位补齐（3.10.8→3.11.0 合法，未补 3.10.9）。
  → **发版前固定一问：本版有没有新功能或界面变化？有 → 次版本。**

## CHANGELOG 行文规范（2026-08-31 定，用户裁决）

用户两句话定死：「大白话、减少字数、一目了然；**内容太多就多分几个小版本**」。

- 每版**一行一个功能点**，每行 **≤40 字**，动词开头，砍技术细节（细节留 git 提交）。
- 反例（v3.7.0 旧行文，一行 120 字塞五件事）：「注水检测独立浮窗（--fax 子进程窗口）…设备即窗口…客户区铺满…」
- 正例：「**注水检测独立浮窗**：关主窗仍存活，单实例不重复弹出」。
- 每版**上限 5 行**；写不下 = 功能太杂 = 拆成多个次版本分别发。
- 分节沿用 🚀 新功能 / ✨ 体验优化 / 🐛 Bug 修复，每节 ≤3 行。

## 运行服务管理规矩（最重要！）

**用户用这些端口上的模型驱动 AI 子智能体工作。任何停服操作前必须先告知用户、
等用户确认已换模型，才能动手。** 曾两次因未打招呼停服导致用户模型掉线。

| 操作 | 是否断服务 |
|---|---|
| python build.py（os.replace 被运行中 exe 锁定必败） | 换装改走腾位法：ren 旧 exe 腾位 → 新 exe 落位 → 只重启 GUI，**后端不断** |
| taskkill ProxyDeck 进程 | **断**，先打招呼 |
| 改代码后要生效 | **断**（要重启对应服务） |
| 改 ui.html / git 提交 / push / 查日志 | 不断 |

不停服务的构建技巧：`go build -o 新文件名.exe ./app`（不锁运行中的 exe）。

## 模型情报（实测结论，血泪换来的）

### 团结（8788，tuanjie）——主力
- 可用模型 ID（2026-09-06 实测 /v1/models，大小写敏感）：`KIMI-K3`（全大写）、
  `GLM-5.3-FLASH`、`codely-core/basic/flash/air/vl`；**GLM-5.3 直连名已 401**
  （team_model_access_denied，上游白名单只剩 alias-only-proxy-models / KIMI-K3 / GLM-5.3-FLASH），
  **codely-core 即 GLM-5.3 的承载入口**——上游把 GLM-5.3 收进 core 别名，
  选 core 实际用的就是 GLM-5.3（用户 2026-09-06 确认，直连 401 同日复核）
- codely-basic/flash/air 同后端 `deepseek-v4-flash-0731`（真 v4 flash），仅推理深度不同
  （实测同题：basic 3.8K / air 6.8K / flash 9.4K reasoning tokens）
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
  - codely 别名现行映射：codely-core→glm-5-fp8-128k、codely-vl→glm-5.3-flash、
    codely-flash/basic/air→deepseek-v4-flash（ga-260731/0731 双部署负载均衡）
  - 官方团队白名单实测收紧为 alias-only-proxy-models / KIMI-K3 / GLM-5.3-FLASH：
    GLM-5.3 直连已 401（team_model_access_denied），桌面客户端模型目录里的 GLM-5.3
    选项不可用（目录与权限不匹配，非代理问题）
  - rc.55→rc.58 逆向四项无变化（签名种子与两层 HMAC / telemetry 不发 x-b3 /
    metadata 四字段 / UA 构造）；UA 无版本门槛（rc.54/55/58 直连全 200）；
    反竞品 system 检测仍活跃；GLM-5.3 TPM 滑窗参数留静默窗口实测

#### ⚠ 团结风控识别特征（2026-08-25 实测；2026-09-04 解包实证重大修正）
- **状态**：2026-08 官方被封过号（Unity 登录锁死、access token 失效、网页无法登录）。
- **🔴 2026-09-04 推翻「入口域名最硬差异」结论**：解包 rc.58 bundle 实证，官方 CLI 的
  chat/模型请求**同样直连 `codely-litellm.tuanjie.cn`**（gemini.js 内 chat base 常量 +
  签名函数专门只对该域名及其子域签名）；`codely.tuanjie.cn` 只用于登录鉴权、dashboard、
  埋点、技能下载。08-25「抓包 894 包全见 codely.tuanjie.cn → 直连 litellm = 一眼反代」
  的旧结论**作废**（成因未定：当时 CLI 版本更老或抓包时段无 chat 流量）——
  **入口域名这个轴上我们与官方一致，无需也无处改路径**。
- **现行风控认知**：封号轴心 = session + 签名凭证链（官方客服原话，见下方红线节）；
  UA 无版本门槛（rc.54/55/58 直连全 200）、TLS 指纹与官方同源，均不在判定主轴。
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

#### 可选缓解（不保证，风险自担）
- 请求形态裁剪（去 stream_options / 隐藏 tools / 消息合并）理论上降低次一级差异，
  但损失 ZCode 的 Agent 工具调用能力或引发协议问题，默认不做；
  真·规避（换号、深度伪装）不在本项目职责内，也不建议投入。

### WorkBuddy（8787）——子智能体可用
- 支持 tool_calls（实测返回过标准工具调用），可当 Agent/子智能体模型
- glm-5.2 / kimi-k3 等可用
- **模型列表机制（2026-09-10 定案）**：`/v1/models` = **硬编码候选池
  （`modelCandidates`）+ 每小时在线探测过滤**（TTL 1h）。探测只能把池内不可用的
  筛掉，**发现不了上游新增**；腾讯侧无任何 `/models` 类端点（`/models`、
  `/v1/models`、`/v2/models` 全 404）——所以「自动更新」只等于可用性刷新，
  **上游新增模型必须手工入池**，且要动三处：`modelCandidates` + `modelMeta`
  （`server.go`）+ ui.html 的 `CB_REF_FALLBACK`（v3.10.3 加 `deepseek-v4.1-flash`
  即走此流程）。8787 由**编译进 exe 的 Go 后端**提供，`plugins/codebuddy2api/`
  是早期 Python 实现、已不在链路上（仅 `credential.go` 一处注释提到它）
- **模型元数据纪律（2026-09-02 定）**：`/model/info` 的 modelMeta 只准填实测值
  （reasoning 实测矩阵：deepseek 系与 auto 拒 off，其余 11 个 off~max 全接；
  maxInput 仅 glm-5.2 / deepseek-v4-pro / deepseek-v4-flash 三条实测 1M）。
  查无实据的上下文/最大输出一律缺省不填——GUI 悬停会如实显示「未核实」，
  禁止照抄官方宣传数字回填（曾因统一写 low/medium/high 误导 Agent 填出 400）
- **hy4-preview 限流兜底（v3.8.1）**：撞配额 429 自动切 fallback（缺省
  deepseek-v4-pro）并按上游重置时点倒计时，到期自动恢复；GUI 甲板有开关
  （/v1/failover GET/POST，配置持久化 codebuddy-failover.json；v3.8.6 起
  POST 响应同构带 hy4_limit、甲板 hy4 行常驻三态状态字「未限流/已限流跑
  fallback/关」、警告条随 fallback 配置显示不再写死）。实测教训：
  上游限流是滑动窗口+按日配额双形态

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
- **传输**：内部托管 zulu serve（8792），`/v1/chat/completions` 扁平化后打
  `/api/v1/conversations/init`。zulu 是 **agent 引擎**（`comate-engine` = @comate/kernel：
  沙箱命令执行、MCP、sqlite-vec 代码库索引、LSP、git），`--mode` **默认 Agent**
  （可选 Ask/Plan/Cloud/Bot；Ask/Plan 只读）
- **⚠ 工具无法由客户端注入**：会话请求体字段全集只有 query / cwd / ptoken / license /
  model / conversation / mode / enableCodebaseSearch / activate{Skills,Commands,
  Subagents,Rules} / attach{Images,Files}——**没有 `tools`**。所以「让 ZCode/DSH 自己的
  工具被 comate 模型调用」在代理层无解；可用形态是「给自足任务 → 上游工具在 cwd 里干完 →
  只回文本」（Qoder 同形态：客户端侧同样看不到 tool_calls）
- **工作目录跟随（v3.10.5）**：插件从客户端提示词提取 `Working directory:`/`工作目录`
  标记（`internal/clientcwd` 共享包，与 Qoder 同源），`COMATE_CWD` 可钉死、
  `COMATE_MODE=Ask` 只读。**此前 cwd 写死 `os.TempDir()`，引擎工具在空目录里空转，
  表现与纯聊天模型无法区分——2026-09-10 曾据此误判「comate 只能当聊天用」，实为插件缺陷**
- **query 走命令行参数**：Windows 上限实测约 32767 字符（32700 过 / 32800 ENAMETOOLONG）；
  超长以 SSE `task_failed` 返回，代理侧表现为「上游 SSE 中断」而非 500
- **工具过程不可见**：上游有 tool-call-action / tool-params-delta / tool-output-delta 等事件，
  但本代理只转发 `delta-batch`/`element-add` 的文本，`thinking-delta` 亦被丢弃
  （首字前那 30~60s 空白即此；若要改善，把它透成 `reasoning_content`）
- **上游偶发返回占位残渣**：2026-09-11 在 8786 实测两次里有一次 content 就是字面量
  `<final>...</final>`（18s 返回，非报错），同请求重测即正常；代理如实转发、**comate
  侧无空响应重试**（团结有 ensureNonEmpty，此处没有）。遇到空/残渣答案先重测再排查

## 已踩的坑（改代码前必读）

1. **WebView2 不吃系统代理**：Node/Electron fetch 连境外 API 会挂，Go 栈最稳
2. **Go ReverseProxy 必须显式改 req.Host**，否则 Cloudflare 403（Host 不认识）
3. **GUI 跨域**：ui.html 从 localhost:随机端口 fetch 127.0.0.1:端口 属跨域，
   各插件服务的响应必须带 CORS 头（见各包的 corsWith）
4. **Git Bash 后台进程会被回收**：长驻服务用工具的 run_in_background
   （`cmd start /b` 会被拒「拒绝访问」；旧 cmd start 方式拉的 8788 没存活过，
   2026-09-06 实证——`cmd //c "start ..."` 退出码 1）。
   **⚠ run_in_background 拉的服务与会话同生共死**：会话挂起/结束后进程树被
   回收——2026-09-07 凌晨实证，5 插件随会话全灭、服务静默下线数小时。
   **跨会话常驻只能 GUI 点火**（插件是 GUI 的分离子进程，关 GUI 不死；
   会话后台任务只用于「本次会话内」的临时服务或发版换装）
5. **`env -u` 启动 windowsgui 程序会假死**：测环境变量相关逻辑用 cmd 脚本
6. **exe 被运行中的自己锁定**：直接覆盖 / os.replace 必败。实测**腾位法免全停**
   （2026-09-02 用户实证裁决）：`ren ProxyDeck.exe ProxyDeck.old.exe`（运行中的 exe
   允许改名）→ 新 exe 落位原路径 → 只需关掉并重启 GUI；插件是独立常驻子进程
   （关 GUI 不死），**后端完全不用停**。旧的 .old 文件等进程自然退出后删
7. **流式响应转发**：FlushInterval 50ms 保 SSE 及时；stream_options.include_usage 补 usage
8. **协作流程**：见 ~/.zcode/AGENTS.md（子智能体 executor/code-reviewer 四步流程），
   简报写 .work/current-task.md
9. **python build.py 静默失败**：exe 被运行中进程锁定时 os.replace 报错走 stderr、
   输出缓冲乱序会把错误吞掉——务必字节校验产物（grep 版本串）再部署
10. **GitHub 匿名 API 限额按出口 IP 计**（60/h）：共享 NAT/Clash 出口必撞墙；
    更新检查类功能优先走 gh CLI 认证通道
11. **升版日志必须从 git 提交清单倒推**（git log 上版..HEAD），不能凭工作记忆——
    v3.8.1 曾漏记同批的 hy4 兜底整块功能（用户指出后补）
12. **池路径流式成功不写 activity 事件**：流式分支读完流即 return，非流式也只在
    响应含 usage 时才记 ok（server.go 池路径）；单账号路径 200 一律 defer 记。
    排查「回落明明成功但实时动态没有 ok」先想到这条——2026-09-04 媒体改路由
    回落 codely-vl 零 ok 事件即此因（请求实际成功）。例外：429 兜底介入过的
    请求，恢复事件由兜底路径自发上报（v3.8.6），不依赖 ok 事件
13. **无 BOM 的 UTF-8 .ps1 中文注释会被 PowerShell 5.1 按 GBK 解析**：乱码可能
    拼出引号/花括号直接炸语法（2026-09-04 换装脚本踩过：报「意外的标记 "}"」
    且行号错位难排查）。写 .ps1 要么纯 ASCII，要么存成 UTF-8 with BOM。
    **变体（2026-09-05 ZCode stop hook 踩过）**：`[Console]::In.ReadToEnd()` 读
    管道也按 GBK 解码——ZCode 喂 UTF-8，中文全变乱码、正则永不命中（每轮误拦）。
    修法：stdin 用 `[Console]::OpenStandardInput()` 按字节读再 UTF8.GetString，
    输出错误同样按 UTF8 字节写 stderr。
14. **Git Bash 内联中文 curl -d 会送出乱码**：Windows 终端编码把请求体里的
    中文打碎，上游模型收到 mojibake 后回复"编码问题"（2026-09-06 实测假阳性，
    Qoder/WorkBuddy 都中过招）。测试请求体一律写 UTF-8 文件 +
    `--data-binary @file`
15. **注水探针 max_tokens 自适应名单须含 codely 系**：probes.go probeCall 名单
    （deepseek/glm-5/kimi/o1/o3）2026-09-06 漏 codely 别名组——codely-core 上游即
    思考型 GLM-5.3，只拿 8 token 预算时 content 恒空、金丝雀「算术」必误判
    （d207471 已补 `codely`）。**探针基线（tuanjie-baselines.json）是启动时加载进
    内存**：改/删条目必须重启 8788 才生效；探针预算变更后旧基线不可比，须删
    `tuanjie|codely-core` 条目重启重采（2026-09-06 实证）
16. **故障时间线陷阱**：判定"持续故障"前先拉逐分钟成败统计——两次测试之间
    的空白期 ≠ 故障持续期（2026-09-06 曾把 6.5 分钟的上游间歇故障误判成
    "连接钉死 85 分钟"，靠逐分钟数据翻案撤回）
17. **版本串清单以文首「升版本必须同步的位置」为唯一权威（现 8 处）**：本坑旧文写「6 处」、
    漏列 `CHANGELOG_DATA`；「当前状态」节旧文写「7 处」、又漏列 `README.md` 徽章——
    同一件事在文件里出现三个数字，正是这类计数漂移的成因，别再在别处复述数字。
    漏改任一项则应用自报旧版并误弹「发现新版本」框（弹窗当前值取 ui.html UI_CUR，
    与 appVersion 不同源；v3.10.0 实测踩坑）
18. **opencode Zen 会话头 + 测试假阴性**：出站 opencode.ai 必带
    `x-opencode-session`（进程级稳定 UUID，opencode_session.go），缺头按出口
    分片随机 400 MissingSessionID；ZCode「连接测试」按钮发裸请求必报错=假阴性，
    以真实对话为准。完整定案见 CODELY.md 2026-09-07 条
19. **同一工作树禁止多窗口并发接手同一任务**：2026-09-10 实测——三个会话
    （收到的是同一句「接手这个会话的任务」）同时改 `internal/codebuddy/server.go`
    与 `sse_cleaner_test.go`，互写工作树、各自把对方的改动当成越界，其中一个
    未打招呼就 `taskkill` 了 8787 后端配合换装（正是本文件明令要先问的那种操作）。
    接手前先 `git status` + 看 `~/.zcode/cli/hooks/activity-*.log`（PostToolUse
    钩子逐条记录 Edit/Write/Bash 与时间戳，可反查是谁在几点改了哪个文件）确认
    没有别的会话在动同一批文件；commit / push / Release 这类不可逆且对外的动作
    尤其必须串行，且推送前先 `git ls-remote <remote> refs/heads/main` 复核远端
    有没有被别人推进过
20. **编译输入 = 工作区当前内容**：`python build.py` / `go build ./app` 编的是磁盘上
    的源码，**并行会话的未提交改动会被静默编进 exe**——2026-09-11 发 v3.10.5 时中招：
    另一个会话 01:09 改的 `internal/tuanjie/reqshape.go`（未提交、无 CHANGELOG）已混入
    产物，靠比对 mtime 才发现，最后改用 `git worktree add --detach <目标提交>` 在干净树
    上重编并 amend。**发版编译前必须确认 `git status` 没有他人改动**，或直接走 worktree
    ——否则 exe 与 tag 指向的源码对不上，且会把未审查代码推给全部用户
21. **不要整行打印进程命令行**：argv 里可能带密钥——GUI 启动 headless 子进程时曾把
    CommandCode key 拼在 `-api-key` 后（`bridge.go`；2026-09-11 已改为只走
    `api-key.txt`，仅落盘失败才退回 argv），而排查时一句
    `Get-CimInstance … CommandLine` 就会把 key 带进会话记录/日志（已实际发生过一次，
    事后需轮换 key）。打印前一律加
    `-replace '-api-key \S+','-api-key <redacted>'`；密钥类文件（`api-key.txt`）
    保持 gitignore

## 自定义服务商（tuanjie-providers.json；v3.9.0 引入时叫「外部账号」，v3.10.0 改名重构）

- **三协议**：账号自带 `protocol` 字段——chat（/chat/completions 缺省）/
  responses（/responses，Zen muse-spark-1.3-contributor-free 只支持此协议）/
  anthropic（/v1/messages，原生 x-api-key + anthropic-version 认证）；入站统一
  chat，出站按协议转换后翻回 chat，客户端零改动
- 旧配置无 protocol 字段走 isZenResponsesModel 白名单兜底（Zen 老条目不断流）；
  **edit 更新时 protocol 留空=保留原值，磁盘空值绝不能回填 chat**
- **opencode 会话头**（v3.10.0，opencode_session.go）：出站到 opencode.ai 的三条
  转发路径自动带 `x-opencode-session`（进程级稳定 UUID）；缺头时 Zen 免费模型
  按出口分片随机 400 MissingSessionID。完整定案见 CODELY.md 2026-09-07 条
- **在线编辑**：卡片「改」按钮免删号重加（v3.10.0 添加/编辑拆分独立状态）；
  api_key 留空=保持原 key
- 计费徽章语义（35cf8bf 改口径）：绿点=**/models 连通**（转发真正依赖的端点）；
  计费端点的 404/超时只进状态文案（「用量未知」提示兜住），不再判不可达——
  此前 Zen 冷启动计费超时被误标红牌（2026-09-07 实测）；/models 不通才判真死
- **媒体改路由链**（tuanjie-media.json）：vision 现为 codely-vl（v3.10.0 下拉
  补 GLM-5.3-FLASH / KIMI-K3 / muse-spark 可选）/ image、video 仍走 Agnes；
  识图回落链 vision→vision_fallback→codely-vl 兜尾（当前 fallback 已清空）。
  **只对团结 8788 生效**——其他插件无此逻辑；含图请求整轮改写（含全部
  上下文）交给识图模型，图片入历史后每轮都会触发（codely-core→muse-spark
  610 次实测），与客户端自己的识图子智能体不冲突（子智能体用识图模型直通，
  主对话轮次被兜底）
- **Agnes 生图生视频合同（2026-09-06 实测，免费账号）**：生图 POST
  /v1/images/generations 约 7-8s 返回图片 URL。生视频两型号合同不同——
  `agnes-video-v2.0` **不需要 mode**；`agnes-video-2.5-flash`（默认配置）
  **必填 mode**，合法值 `text`/`keyframe`/`reference`（文生视频用
  `"mode":"text"`，文档 wiki.agnes-ai.com/en/docs/agnes-video-v25）。流程：
  POST /v1/videos → 轮询 GET /v1/videos/{id}（代理自动补 model_name）→
  约 60s completed → metadata.url 即 MP4。429/5xx 已如实透传（f722e40
  修复前 forwardExternal 的失败状态会被调用方吞成 200 空响应）
- **日志重定向坑**：手动拉插件别用 PowerShell `Start-Process -RedirectStandard*`
  （重定向不生效，日志写进虚空）；用 cmd 批处理 `>>` 追加（.work/spawn_*.cmd，
  保留历史日志，GUI 日志面板才有内容）

## 当前状态（2026-09-12，c16d086 未发版收尾）

- **c16d086 待发版**：账号池区补拨杆 keydown 重拉（用户明确要求补①；键盘路径走引擎
  gearSelect 不派发 click，故与 click 并列监听，引擎代码未动）。仅 1 文件
  （app/ui.html +12/-2），下版（v3.13.2）固化发版。
- **v3.13.1 已发布**：账号池长条行固化——把 v3.13.0 后的三处热修复正式收口：
  ① 添加提示改 pt-notice（原 pt-silk 装饰小字被截断）② 卡片兼容旧后端快照
  （字段缺失按启用）+ 删登录文件行 ③ 账号池区改用团结主列表 acc-row 长条行。
  纯 UI 返工 → 修订号（v3.13.0 → v3.13.1）。
  提交链 `16af821`/`7008a3b`/`a98640c`（三处修复）→ `f6d9a28`（发版）→
  `e43968d`（exe，干净树 @ f6d9a28）
- **v3.13.0**：账号矩阵三批——① 积分按账号切换（`/quota?uid=`、缓存按区+号分键、
  GUI 下拉选号、零轮询纪律）② 凭据失效治理（401 标死换活号、rescan 复位、红徽章）
  ③ 矩阵对齐团结（停用/移除/恢复落 `codebuddy-pool(-intl).json`、进行中/调用统计、
  添加提示、积分 401 显示凭据失效）。独立审查通过（-race 40/40）。
  提交链 `82c3e3d`（后端）→ `31a60ab`（ui）→ `2184a10`（发版）→ `bd24737`
 （exe，干净树 @ 2184a10）
- **v3.12.1**：账号池区卡片化返工（复用团结 mx-card）。**v3.12.0**：WorkBuddy 双区
  账号池 + 注水检测两轮（两批分别来自账号池会话与并行注水会话，代为入库归属见提交）。
  v3.11.0：脱敏扩全角色。更早细节见 `CHANGELOG.md`
- 远程 main 与本地同步；仓库 github.com/Amer-CN/proxydeck（remote 名 `myrepo`，
  不是 origin；`push_api.py` 里的 `REPO` 写的是改名前旧名 `command-code-proxy-tools`，
  GitHub 会重定向，能用但名字陈旧）
- 服务现状（2026-09-12 22:4x 复核）：GUI（PID 32448，22:35 起）已是 **v3.13.1 新包**
  （长条版，单 GUI 无残留）。六后端在线：8787/8789（32640/27472，22:19 起——GUI 重启
  时连带重启）/8788/8785/8786/8891。v3.13.1 是纯 UI 返工，**后端无需再换包**。
  ⚠ **拉杆不会换包**：`pluginStart` 探活到端口健康就「接管复用，不杀不重启」（见下条），
  想让后端换包必须**熄火再点火**。
  ⚠ 账号池 **Dead/冷却/调用计数是进程内存态**（重启清零），只有 Enabled/Removed 落盘；
  上游 reset 时点不落盘，重启丢一次无害。`codebuddy-pool(-intl).json` 现已 gitignore。
  55990 主代理未起（按需点火）。**GUI 不持久化插件启动状态**（`plugins.go` 无写盘），
  **重启 GUI 不会自动点火、需逐个拉杆**——跨会话常驻只能 GUI 点火（坑 4）
- 服务治理：**GUI 管得住外部拉起的后端**——靠的不是
  进程归属，而是 `plugins.go` 的 `pluginStart` 先探活端口、健康就「接管复用，不杀不
  重启」（`pluginList` 的 `healthy` 也无条件查端口，`stop` 按端口杀）。所以旧文
  「外部拉起的实例 GUI 插件开关未必管得住它」是误解：2026-09-10 曾据此给出「拉杆会
  撞端口、得先杀掉」的错误处置建议，实际拉杆只会接管
- **换装纪律**：`ProxyDeck.exe` 换装走腾位法（rename 运行中的 exe → 新包顶替原
  路径），**后端不断、只有 GUI 需重启**；一键更新的替换目标取自
  `os.Executable()`（`bridge.go`）而非硬编码路径，所以腾位换装后**先重启一次
  GUI 再谈别的**，避免在 `.old` 名字上继续套娃
- 发版坑：版本串清单以文首「升版本必须同步的位置」为唯一权威（**8 处**，含
  2026-09-10 补的 `README.md` 徽章），本文件不再复述处数；`CHANGELOG_DATA` 是内嵌 JS，
  改完用 node 实解一次验语法（改坏会白屏）
- `CHANGELOG.md` 顶条与 ui.html `CHANGELOG_DATA` 首条必须**逐字同源**：前者进
  GitHub Release 正文，后者进 GUI 更新日志浮层；两处不同源用户会看到两套日志
- `.work/` 现状：构建中间产物已清（2026-09-10 清掉 `pd3103*.exe`、`ProxyDeck.new/stripped.exe`），
  exe 回滚副本按 `ProxyDeck.prev*.exe` 命名累积（`ProxyDeck.prev.exe`、
  `ProxyDeck.prev-85693aa.exe`、2026-09-11 加的 `ProxyDeck.prev-dc548a8.exe` 等）；
  另有已完成任务的简报（`current-task.md`、`task-*.md`）、探测语料与临时脚本等，
  全部 gitignore，可随时清；**清场需用户确认**（并发会话可能仍在用同名简报）
  简报 `current-task.md` 由下一任务重写；**并行会话改用 `task-<关键词>.md`**（已核
  `enforce-flow.ps1` 第 49 行对 `current-task.md` 与 `task-*.md` 同等识别为简报；
  2026-09-10 实测用 `task-*.md` 连改 6 个文件全程未被闸门拦截）
- **已清理（用户 2026-09-12 明确要求删）**：`.probe/`（24MB 探测草稿）、
  `docs/glm53flash-litellm-probe-report.md`、`docs/reasoning-effort-verification.md`
  均已删除（代码零引用；此前 09-10 不入库裁决的执行）。
- 实测记录（2026-09-06 两轮全量）：结论已并入上文模型情报与坑 14/15/16；
  唯一未覆盖：B.AI 6-8M 字符级 WAF 极端量级（判定不值得复现）
- 下一版待办：① 种子卫兵双轨道扫描**代码已改完待发版**（2026-09-13，改动只在
  internal/tuanjie/seedcheck.go + 新增 desktop_path_{windows,other}.go，单测已过、
  测试端口 18788 实测生效）；**尚未换装到运行中的 8788**（换装要断服务，需用户确认）。
  换装走腾位法（ren 旧 exe → 新 exe 落位 → 只重启 GUI）。新构建在仓库根
  `ProxyDeck-seedguard.exe`（v3.11.0 口径，12.5MB，与现版同参数）。
  ② hy4 键盘可达性（拨杆同位置同问题，用户未要求补，先记账）。
  键盘重拉已在 c16d086 落地。新坑/结论随时按惯例入册
- 更新检查通道：本机 gh CLI（认证 5000/h）优先 → 匿名 HTTP 兜底（60/h 按
  出口 IP 计，共享网络易撞墙，撞后负缓存 10 分钟）
- ZCode 侧模型配置：tuanjie provider（8788）配了 codely 系（core 即 GLM-5.3
  承载入口）；子智能体 executor 用团结模型、code-reviewer 用 WorkBuddy
  （依赖服务在跑；WorkBuddy 的 hy4-preview 撞配额时代理自动切 deepseek-v4-pro）

## 常用命令

```bash
python build.py                      # 构建正式 exe（需全停服务）
go build -o new.exe ./app            # 不停服务构建
go vet ./...                         # 静态检查
node --check <js>                    # ui.html 抽取 script 后语法校验
./ProxyDeck.exe -headless # 主代理后台模式
./ProxyDeck.exe --plugin-tuanjie --port 8788   # 单独拉团结
curl http://127.0.0.1:8788/health    # 健康检查
curl http://127.0.0.1:8787/v1/failover   # WorkBuddy hy4 兜底状态/配置
git push myrepo HEAD:main            # 推送（remote 名是 myrepo 不是 origin）
```

## 🔴 红线：团结凭证链（2026-08-27 官方客服实证）

官方客服原话：「我们是通过 session 的签名 sp 来封的，cliapi 这块不动，就不会」。
即封号判定依据 = **session（x-litellm-session-id）+ 签名（X-Codely-Signature）凭证链**。
只要请求携带这套 CLI 凭证链，官方不管流量从哪个客户端壳发出。

**我们的实现必须逐字节保持与官方 CLI（@unity-china/codely-cli rc.58）一致，任何"顺手优化/重构"都禁止触碰以下项：**

1. `codelySigningSeedHex = "406f00f74768ba0cb0cd30f097ec6c2bdacb89c61a38b7dd140838bbd0e98018"`（client.go）
2. 签名密钥派生：`HMAC(seed,"codely-signing-v1") → HMAC(k1, cli_api_key)`（codelySigningKey）
3. 签名消息体：`["v1", path, timestamp].join("
")`，输出 `v1.<ts>.<base64url>`（SignLitellm）
4. x-litellm-session-id 头 = 请求体 litellm_session_id = prompt_cache_key（每请求 randomUUID）
5. cli_api_key 换取路径 `codely.tuanjie.cn/api/api-token/cli-api-key`（不能改成其他换取源）
6. cliUserAgent = `codely-cli/1.0.0-rc.58 (win32; x64)`（官方 HTTP UA 真值：Dre/QEe 构造器
   defaultHeaders，`codely-cli/${版本} (${process.platform}; ${process.arch})`。
   版本号动态读本机 npm 安装的官方 CLI（client.go:54-87），读不到用兜底常量——
   **官方发新版必须同步兜底常量并重验种子**（种子卫兵会自动盯，f97f787 为 rc.55→rc.58 先例）。
   注意：`Codely-CLI - OSS/...` 是官方 telemetry 埋点字段（getRealUserAgent），
   不是 HTTP 请求 UA——2026-08-27 曾误用并已修正）
7. **不带** X-DashScope-CacheControl/X-DashScope-UserAgent（官方仅 isDashScopeProvider
   路径发：authType=QWEN_OAUTH 或 baseUrl 指向 dashscope.aliyuncs.com；团结 LiteLLM
   路径官方不发，我们发了就是多出来的识别信号，2026-08-27 已删）
8. 请求体重排（reqshape.go）：官方 buildCreateParams 字段序 + 默认字段，非流式删 stream

**改动以上任何一项 = 掉出安全线 = 有被官方按凭证链封号的风险。**
如需变更（官方发新版 CLI、签名算法迭代），必须先逆向新版源码实证、再改，改后必须用
真实流量 200 验证并提交。历史教训：56574 账号 401 被封发生在签名/UA 未对齐时期，
对齐后新账号流量稳定 200 未被扫。

**2026-09-13 三轨道复核（桌面端 2.0.9 更新触发的例行核验，结论：无需改动）**：
对三条线各自逆向 + 实测，八项红线**逐字一致**，未发现任何需适配项——
- **桌面端 2.0.9 内置 CLI**（`release.57`，203MB 编译产物）：种子 hex / 两层 HMAC /
  `["v1",path,ts].join("\n")` / `v1.<ts>.<base64url>` / 头名 / UA 模板
  （`` `codely-cli/${ver} (win32; x64)` ``）/ 头集合 `{UA, x-litellm-session-id}` /
  metadata 四字段 / `########` 分隔符 / `stream_options:{include_usage:true}` /
  `parallel_tool_calls:true` / 换 key 端点 —— 全一致
- **npm rc.58**（本机全局，反代 UA 来源）与 **npm rc.60**（registry latest，9/9 发布）：
  同上全一致；rc.60 对 rc.58 的 diff 只有 OAuth 内部路由改名与一处 `buildToolParams`
  空数组提前返回（官方收紧，非新增信号）
- **实测**：本地 8788 真实流量 200；用官方算法独立复算签名直连上游 200；
  **UA 无版本门槛**（release.57 / rc.58 / rc.60 三个版本号直连全 200）
- 两个「看着像雷、实测无事」的点：①新版换 key 多带 `?teamId=<当前组织ID>`，
  同账号 A/B 实测**拿到同一把 key**（反代不带此参数无影响）；②`oauth_creds.json`
  的 `cli_api_key` 改为 AES 加密存储（`iv:密文`），但反代只用 `access_token` 现换
  key、不读该字段，无影响
- **次一级差异（非凭证链，未处理）**：客户端发 `tools: []` 时反代补
  `parallel_tool_calls` 而 rc.60 官方不发（实际客户端不发空数组，影响可忽略）
