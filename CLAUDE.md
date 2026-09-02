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
│   └─ ui.html                  全部 UI（内嵌，5 个视图 tab）
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
  外加 `CHANGELOG.md` 顶条 + GitHub Release（正文＝CHANGELOG 顶条逐字，标题＝`vX.Y.Z · 主题 · 主题`）。
  全在 `grep -rn "v3\.6\.[0-9]" app/` 一枪能扫到的范围内，改完照这条 grep 复核一遍。

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
| python build.py（替换运行中的 exe） | **断**，需全停，先打招呼 |
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

#### ⚠ 团结风控识别特征（2026-08-25 实测，血泪教训）
- **状态**：2026-08 官方被封过号（Unity 登录锁死、access token 失效、网页无法登录）。
- **✅ 抓包实证的核心差异（2026-08-25 直连抓包 894 包）**：
  - **官方 CLI 的连接目标是 `codely.tuanjie.cn`**（35 条 TLS 连接 SNI 全是它），
    由官方后端转发到 litellm；
  - **我们的反代直连 `codely-litellm.tuanjie.cn`**（litellm 网关本体）。
  - 这是最硬的判定信号：litellm 网关很可能只接受官方后端转发来的流量，
    外部直连 litellm = 一眼反代。
- **次一级的差异**（本地 8788 诊断日志对比）：
  - 官方 CLI：极简请求体（`model/max_tokens/messages`、2 条消息、无 stream/tools）；
  - ZCode/agent：带 `stream_options / tools / tool_choice / stream`、超大消息。
- **协议层**：官方 CLI 消息角色含 `gemini/gemini_reasoning`（会话存档实证），
  ZCode 是 OpenAI 角色（assistant/reasoning_content）——疑似差异，未完全实锤。
- **排除项**：TLS 指纹（官方 CLI 走 Clash 时与我们同指纹）；
  "走 Gemini 协议" 的早期判断因 SDK 方法名误判，已推翻（详见 git 历史）。
- **修复边界**：服务端判定黑盒、规则随时变，**无 100% 保证**；修改连接路径
  （改连 codely.tuanjie.cn）需要换认证方式，风险高收益不确定，不建议投入。

#### 可选缓解（不保证，风险自担）
- 转发层裁剪请求：去掉 `stream_options`（官方 CLI 不带）、隐藏 `tools/tool_choice`
  字段、消息合并压缩——能降低请求形态差异，但会损失 ZCode 的 Agent 能力
  （工具调用）或引发别的协议问题，需权衡。
- 真·规避（换号、伪装更多）不在本项目职责内，也不建议投入。

### WorkBuddy（8787）——子智能体可用
- 支持 tool_calls（实测返回过标准工具调用），可当 Agent/子智能体模型
- glm-5.2 / kimi-k3 等可用
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
6. **exe 被运行中的自己锁定**：构建替换前需全停；或构建到新文件名
7. **流式响应转发**：FlushInterval 50ms 保 SSE 及时；stream_options.include_usage 补 usage
8. **协作流程**：见 ~/.zcode/AGENTS.md（子智能体 executor/code-reviewer 四步流程），
   简报写 .work/current-task.md
9. **python build.py 静默失败**：exe 被运行中进程锁定时 os.replace 报错走 stderr、
   输出缓冲乱序会把错误吞掉——务必字节校验产物（grep 版本串）再部署
10. **GitHub 匿名 API 限额按出口 IP 计**（60/h）：共享 NAT/Clash 出口必撞墙；
    更新检查类功能优先走 gh CLI 认证通道
11. **升版日志必须从 git 提交清单倒推**（git log 上版..HEAD），不能凭工作记忆——
    v3.8.1 曾漏记同批的 hy4 兜底整块功能（用户指出后补）

## 当前状态（2026-09-02）

- 版本 v3.8.2 已发布（GitHub Release + exe 附件）；一键更新已上线
  （GUI 内直接下载替换重启，无需去网页）
- 远程 main 与本地同步；仓库 github.com/Amer-CN/proxydeck
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

**我们的实现必须逐字节保持与官方 CLI（@unity-china/codely-cli rc.54）一致，任何"顺手优化/重构"都禁止触碰以下项：**

1. `codelySigningSeedHex = "406f00f74768ba0cb0cd30f097ec6c2bdacb89c61a38b7dd140838bbd0e98018"`（client.go）
2. 签名密钥派生：`HMAC(seed,"codely-signing-v1") → HMAC(k1, cli_api_key)`（codelySigningKey）
3. 签名消息体：`["v1", path, timestamp].join("
")`，输出 `v1.<ts>.<base64url>`（SignLitellm）
4. x-litellm-session-id 头 = 请求体 litellm_session_id = prompt_cache_key（每请求 randomUUID）
5. cli_api_key 换取路径 `codely.tuanjie.cn/api/api-token/cli-api-key`（不能改成其他换取源）
6. cliUserAgent = `codely-cli/1.0.0-rc.54 (win32; x64)`（官方 HTTP UA 真值：Dre/QEe 构造器
   defaultHeaders，`codely-cli/${版本} (${process.platform}; ${process.arch})`。
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
