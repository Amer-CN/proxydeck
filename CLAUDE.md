# CommandCode Proxy Deck — 项目交接文档

> 本文件给 AI 助手（Claude Code / ZCode 等）和新协作者提供项目全貌。
> 记录了架构、已踩过的坑、协作规矩、当前状态。改动项目前先读完本文件。

## 项目一句话

Windows 独立 GUI 工具（单 exe，Go + WebView2 + 内嵌 HTML）：把 CommandCode /
团结 / WorkBuddy(腾讯CodeBuddy) / Notion AI / WPS灵犀 的订阅转成本地 OpenAI 兼容接口，
科幻全息风格控制台统一管理。完全开箱即用，无任何激活/授权机制。

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
│   ├─ notion/                  Notion AI 插件后端（8789，仅对话协议）
│   └─ lingxi/                  WPS灵犀插件后端（8790，仅对话协议）
└─ build.py                     构建脚本（单模式，python build.py）
```

- 端口约定：主代理 55990 / WorkBuddy 8787 / 团结 8788 / Notion 8789 / 灵犀 8790
- 插件 = spawn 本 exe 的 `--plugin-<id> --port <n>` 子进程，独立常驻，关 GUI 不中断
- 版本号在 app/main.go（appVersion）和 app/ui.html（多处字符串），改版本要同步

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

#### ⚠ 团结风控识别特征（2026-08-25 实测定论，血泪教训）
- **状态**：2026-08 官方被封过号（Unity 登录锁死、access token 失效、网页无法登录）。
- **已证实的差异**（本地抓取官方 CLI vs ZCode 请求体对比）：
  - 官方 CLI：极简请求体，仅 `model/max_tokens/messages`，2 条消息、单条 ≤1KB、
    **无 stream / 无 stream_options / 无 tools / 无 tool_choice**。
  - ZCode/agent：带 `stream_options / tools / tool_choice / stream`，4~6 条消息、
    单条上万字符、system 6K+——服务端看请求体即可零成本区分。
- **结论**：反代流量在**应用层请求形态**上和官方 CLI 完全不同，一行规则就能筛出；
  TLS 指纹假说已被排除（两边都走 Clash 7897，指纹一致）。
- **修复边界**：服务端判定黑盒、规则随时变，**无 100% 保证**；可做的是降低暴露
  （见下"可选缓解"），不保证长期有效。

#### 可选缓解（不保证，风险自担）
- 转发层裁剪请求：去掉 `stream_options`（官方 CLI 不带）、隐藏 `tools/tool_choice`
  字段、消息合并压缩——能降低请求形态差异，但会损失 ZCode 的 Agent 能力
  （工具调用）或引发别的协议问题，需权衡。
- 真·规避（换号、伪装更多）不在本项目职责内，也不建议投入。

### WorkBuddy（8787）——子智能体可用
- 支持 tool_calls（实测返回过标准工具调用），可当 Agent/子智能体模型
- glm-5.2 / kimi-k3 等可用

### Notion（8789）/ 灵犀（8790）——仅对话，无 Agent 能力
- 协议层无 tools 字段，**只能对话，不能作为子智能体**（UI 顶栏已标警示）
- Notion 限流史：曾因高频测试触发限流，冷却 48h 后恢复；试探一次就会重置倒计时

### b.ai —— 已移除（2026-08-19，用户明确要求）
- 曾做成插件后整个拆除（commit 9fd9bf9）。原因：其 "deepseek-v4-flash" 实测为
  2025 上半年的 DeepSeek 推理系模型换皮（知识截止 2025H1，不知道 2025-12 的 V4），
  且 CF WAF 滚动封锁长请求（1010），不适合 Agent
- 若用户再提起 b.ai：提醒以上结论，别建议重新接入

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

## 当前状态（2026-08-19）

- 版本 v2.5.1 已发布（GitHub Release + exe 附件）
- 远程 main 与本地同步；仓库 github.com/Amer-CN/proxydeck
- 服务通常在跑：55990（headless 主代理）/ 8788（团结）/ 8787（WorkBuddy）
- **待办：8788 待重启激活空响应重试**（代码已提交 5eeca56，等用户给停服窗口；
  重启时顺带换成 v2.5.1 构建——本地 exe 仍是 v2.5.0 版本字符串）
- ZCode 侧模型配置：tuanjie provider（8788）配了 GLM-5.3/codely 系；
  子智能体 executor 用团结模型、code-reviewer 用 WorkBuddy（依赖服务在跑）

## 常用命令

```bash
python build.py                      # 构建正式 exe（需全停服务）
go build -o new.exe ./app            # 不停服务构建
go vet ./...                         # 静态检查
node --check <js>                    # ui.html 抽取 script 后语法校验
./ProxyDeck.exe -headless # 主代理后台模式
./ProxyDeck.exe --plugin-tuanjie --port 8788   # 单独拉团结
curl http://127.0.0.1:8788/health    # 健康检查
git push myrepo HEAD:main            # 推送（remote 名是 myrepo 不是 origin）
```
