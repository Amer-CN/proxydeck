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
- **升版本必须同步的位置**（漏一处就有地方显示旧版；2026-09-02 按实操勘误计数）：
  1. `app/main.go` `appVersion`
  2. `app/ui.html` `id="etchVer"`（顶栏蚀刻）
  3. `app/ui.html` `.pt-nameplate`（铭牌，含 `aria-label` 同行两处）
  4. `app/ui.html` `id="buildNo"`
  5. `app/ui.html` `id="verChip"`
  6. `app/ui.html` `var UI_CUR`（更新检查比对用）
  7. `app/ui.html` **`var CHANGELOG_DATA`**（甲板内「更新日志」浮层的**独立副本**，不读 `CHANGELOG.md`！
     只改仓库文件不改这里，浮层就还是旧版——v3.6.2 / v3.6.3 两次都栽在这里）
  外加 `CHANGELOG.md` 顶条 + GitHub Release（正文＝CHANGELOG 顶条逐字，标题＝`vX.Y.Z · 主题 · 主题`，
  **必须附 `ProxyDeck.exe` 附件**——那是一键更新的下载源，漏传 = 用户点更新 404；
  2026-09-04 发 v3.8.5 时曾漏、事后补传）。
  改完用旧版本串 grep app/ 复核一遍（应只剩 CHANGELOG_DATA 历史条目）。

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
- 白名单 6 个模型 ID，**大小写敏感**：`GLM-5.3`、`KIMI-K3`（全大写）、
  `codely-core/basic/flash/air/vl`
- codely-basic/flash/air 同后端 `deepseek-v4-flash-0731`（真 v4 flash），仅推理深度不同
  （实测同题：basic 3.8K / air 6.8K / flash 9.4K reasoning tokens）
- 费率（官方 /model/info）：codely 系 1.6/3.2；GLM-5.3 3.2/11.2；KIMI-K3 16/80（贵，慎用）
- GLM-5.3：≥825K 上下文，effort 参数时灵时不灵（上游 bug）；**上游过载时偶发
  429 和"200 空响应"**（空响应重试已实现在 tuanjie/server.go 的 ensureNonEmpty，5eeca56）
- 已知上游故障：间歇 400 "Invalid model name passed in model=None"（多实例映射失步，
  插件已内置 3 次重试；高发期可能连撞，等待即可）
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
  internal/tuanjie/seedcheck.go 双层监控：①本机 bundle/gemini.js 特征扫描
  （种子 hex + X-Codely-Signature 头名）；②npm registry 在线核对——每小时查
  latest 版本号，**版本变了才下载新包验特征（约 16MB 一次性），没变零下载**。
  任一层特征消失 → /health `seed_alert=true` + GUI 警示条；字段
  seed_alert/seed_signal/seed_latest/seed_online；网络故障只记状态不误报。
  2026-09-04 实测：rc.58 种子与签名算法逐字段一致、registry latest=rc.58。
  旁证链：种子真轮换 → 全线 401 → 连续 3 次触发 judgment_alert（client 自动换 key
  不掩盖告警）；⚠ team_model_access_denied 的 401 同样推高计数（潜在误报源）；
  探针/水印旁路的 401 不进告警链；告警状态只反映在 /health 与 GUI，不写日志文件。
- **凭证纪律**：tuanjie-accounts.json.bak-* 内含有效 JWT，gitignore 已补通配
  （2026-09-04，ebaf3c9）；账号备份文件一律不准进 git。

#### 可选缓解（不保证，风险自担）
- 请求形态裁剪（去 stream_options / 隐藏 tools / 消息合并）理论上降低次一级差异，
  但损失 ZCode 的 Agent 工具调用能力或引发协议问题，默认不做；
  真·规避（换号、深度伪装）不在本项目职责内，也不建议投入。

### WorkBuddy（8787）——子智能体可用
- 支持 tool_calls（实测返回过标准工具调用），可当 Agent/子智能体模型
- glm-5.2 / kimi-k3 等可用
- **模型元数据纪律（2026-09-02 定）**：`/model/info` 的 modelMeta 只准填实测值
  （reasoning 实测矩阵：deepseek 系与 auto 拒 off，其余 11 个 off~max 全接；
  maxInput 仅 glm-5.2 / deepseek-v4-pro / deepseek-v4-flash 三条实测 1M）。
  查无实据的上下文/最大输出一律缺省不填——GUI 悬停会如实显示「未核实」，
  禁止照抄官方宣传数字回填（曾因统一写 low/medium/high 误导 Agent 填出 400）
- **hy4-preview 限流兜底（v3.8.1）**：撞配额 429 自动切 fallback（缺省
  deepseek-v4-pro）并按上游重置时点倒计时，到期自动恢复；GUI 甲板有开关
  （/v1/failover GET/POST，配置持久化 codebuddy-failover.json）。实测教训：
  上游限流是滑动窗口+按日配额双形态

### B.AI / b.ai（8891）——已重新接入（v3.5.0），但保留当年实测结论
- **现状**：`internal/bai`，COMMAND 键副页，Go 栈透明转发 `https://api.b.ai`；
  模型矩阵走 GUI 专用旁路 `/model/matrix`（详见 `docs/bai-model-matrix-design.md`）
- **当年实测结论（2026-08-19 曾据此整个拆除，commit 9fd9bf9；结论本身未被推翻）**：
  其 "deepseek-v4-flash" 实测为 2025 上半年的 DeepSeek 推理系模型换皮（知识截止 2025H1，
  不知道 2025-12 的 V4），且 CF WAF 滚动封锁长请求（1010）——**这一路不适合当主力 Agent 后端**
- 上游 `/v1/models` 实测只给 `id/object/created/owned_by/supported_endpoint_types`，
  `created` 恒为占位值、无任何价格或免费字段；`/model/info`、`/key/info` 等元数据路径一律 403
  （网关只放行推理路径）。**别指望从接口自动判定模型是否免费**

## 已踩的坑（改代码前必读）

1. **WebView2 不吃系统代理**：Node/Electron fetch 连境外 API 会挂，Go 栈最稳
2. **Go ReverseProxy 必须显式改 req.Host**，否则 Cloudflare 403（Host 不认识）
3. **GUI 跨域**：ui.html 从 localhost:随机端口 fetch 127.0.0.1:端口 属跨域，
   各插件服务的响应必须带 CORS 头（见各包的 corsWith）
4. **Git Bash 后台进程会被回收**：长驻服务用工具的 run_in_background 或 cmd start
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
    回落 codely-vl 零 ok 事件即此因（请求实际成功）

## 当前状态（2026-09-04）

- 版本 v3.8.5（GitHub Release v3.8.4 / v3.8.5 均已发布，2026-09-04）；
  一键更新已上线（GUI 内直接下载替换重启，无需去网页）
- v3.8.5 改动：种子卫兵（seedcheck.go，官方 CLI 签名种子轮换双层自动告警，
  详见「团结风控识别特征」节）；gitignore 补账号备份通配防凭据泄露（ebaf3c9）
- 远程 main 已同步（2026-09-04 推送至 fb9792c，含标签 v3.8.4/v3.8.5）；
  仓库 github.com/Amer-CN/proxydeck
- 服务通常在跑：55990（headless 主代理）/ 8788（团结）/ 8787（WorkBuddy）/
  8786（Comate）/ 8785（Qoder）/ 8891（B.AI）
- 更新检查通道：本机 gh CLI（认证 5000/h）优先 → 匿名 HTTP 兜底（60/h 按
  出口 IP 计，共享网络易撞墙，撞后负缓存 10 分钟）
- ZCode 侧模型配置：tuanjie provider（8788）配了 GLM-5.3/codely 系；
  子智能体 executor 用团结模型、code-reviewer 用 WorkBuddy（依赖服务在跑；
  WorkBuddy 的 hy4-preview 撞配额时代理自动切 deepseek-v4-pro）

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
