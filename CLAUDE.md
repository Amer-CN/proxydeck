# CommandCode Proxy Deck — 项目交接文档

> 本文件给 AI 助手（Claude Code / ZCode 等）和新协作者提供项目全貌。
> 记录了架构、已踩过的坑、协作规矩、当前状态。改动项目前先读完本文件。

## 项目一句话

Windows 独立 GUI 工具（单 exe，**纯 Rust + WebView2 + 内嵌 HTML**；v4.0.0 起是 Rust，
2026-10-18 起 Go 实现已全部移除）：把 CommandCode /
团结 / WorkBuddy(腾讯CodeBuddy) / Comate(百度) / Qoder(阿里) / B.AI 的订阅转成
本地 OpenAI 兼容接口，机械风控制台统一管理。完全开箱即用，无任何激活/授权机制。

## 架构速览

> **2026-10-18：Go 参考实现已整体移除**（42,274 行 / 151 文件）。仓库里只剩 Rust。
> `app/` 目录保留两个原件：`ui.html`（UI 唯一源，Rust 侧逐字节嵌入）与
> `icon.ico`（`winres_contract` 要求与 `rs-gui/icon.ico` 逐字节相同）。
> 历史基准（采收自 Go 的 golden JSON）仍在各 crate 的 `tests/golden/` 里，照常守门。

```
ProxyDeck.exe                 ← 唯一主程序（Rust 构建），双击即用
├─ rs-gui/                    宿主：WebView2 + tao/wry 窗口、JS 桥、插件托管、更新、托盘
├─ rs-proxy/                  COMMAND 代理（55990，三条入站协议）
├─ rs-tuanjie/                团结插件（8788）
├─ rs-codebuddy/              WorkBuddy 插件（8787 + 国际版 8789）
├─ rs-comate/                 Comate 插件（8786；云直连默认，zulu serve 8792 兜底）
├─ rs-qoder/                  Qoder 插件（8785）
├─ rs-vibex/                  VibeX 插件（8790）
├─ rs-bai/                    B.AI 插件（8891）
├─ rs-codearts/               华为云码道插件（8793，本机登录态自动导入）
├─ rs-opencode/               OpenCode 插件（8784；不自带能力，驱动本机 `opencode serve`）
├─ app/                       只剩两个原件：ui.html（UI 唯一源）+ icon.ico
└─ Cargo.toml                 工作区（逐条列全的成员清单）
```

- 端口约定：COMMAND 代理 55990 / WorkBuddy 8787（国际版 8789）/ 团结 8788 / Comate 8786 / Qoder 8785 / B.AI 8891 / VibeX 8790 / 码道 8793 / OpenCode 8784
- 插件 = spawn 本 exe 的 `--plugin-<id> --port <n>` 子进程，独立常驻，关 GUI 不中断
- **升版本必须同步的位置**（**Rust 时代清单，2026-10-18 重写**；Go 时代的
  `app/main.go appVersion` 已不参与构建，只剩历史参考）：
  1. `app/ui.html` `id="etchVer"`（顶栏蚀刻）
  2. `app/ui.html` `.pt-nameplate`（铭牌，含 `aria-label` 同行两处）
  3. `app/ui.html` `id="buildNo"`
  4. `app/ui.html` `id="verChip"`
  5. `app/ui.html` `var UI_CUR`（更新检查比对用）
  6. `app/ui.html` **`var CHANGELOG_DATA` 首条**（浮层是 `CHANGELOG.md` 的独立副本；
     ⚠ **只插一条新记录，绝不动历史条目**——2026-10-18 事故：脚本对整文件做
     `replace(旧版本, 新版本)`，把 12 条历史条目全改成了同一个版本号）
  7. `rs-gui/src/lib.rs` `APP_VERSION`
  8. `rs-gui/version.rc` 四处：`APP_VERSION` + `FILEVERSION` + `PRODUCTVERSION` + 字符串 `"x.y.z.0"`
  9. `rs-gui/ui.html` ← `app/ui.html` 的**逐字节副本**（`Copy-Item` 复制，**不许手改**）
  10. `rs-gui/src/uihtml.rs` `UI_HTML_SHA256` + `UI_HTML_LEN`（改完 UI 必重算）
  11. `README.md` 版本徽章（shields.io 的 `Version-vX.Y.Z`）
  外加 `CHANGELOG.md` 顶条 + GitHub Release（正文＝CHANGELOG 顶条逐字，标题＝`vX.Y.Z · 主题`，
  **必须附 `ProxyDeck.exe` 附件**——那是一键更新的下载源，漏传 = 用户点更新 404；
  附件名必须逐字叫 `ProxyDeck.exe`，改成 `ProxyDeck-vX.Y.Z.exe` 后端找直链会无视）。
  `CHANGELOG_DATA` 的 `date` 用**本机发布日**——跨零点发版最容易差一天。
  历史条目**不回改**（会牵动 tag 与附件对应关系）。
  **现存三道自动门禁**（改坏会直接红，别再靠人眼）：
  `rs-gui/tests/ui_html.rs` 的 ① 逐字节副本 ② 版本号唯一+严格降序 ③ v4 段不许跳版
  + 浮层与 `CHANGELOG.md` 同集合；以及 `build.rs` 的构建期字节比对。

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

## 攒版发布（2026-09-13 用户裁决，并行会话必须遵守）

用户原话：「我暂时还不想发版，因为并行会话还有几个在更新别的东西，比如增加其他功能、
优化修复等。所以我想把这些全部搞完之后，再重新发新版本，不要一会儿又发版，一会儿又发版。」

**发版节奏由用户统一掌握，不再改完一个功能就发一版。**

并行会话纪律（每个会话各自遵守，不需要互相协调）：
- **只提交代码，不发版**：改完 → 跑测试 → `git commit` → 停。**不升版本号、不动
  `CHANGELOG.md`、不打 tag、不做 GitHub Release、不用新包覆盖 `ProxyDeck.exe`**
  （升版本清单见上文，全部属于发版动作）。
- **不自行换装运行中的服务**：换装要断服务，须用户确认（见「运行服务管理规矩」）。
  测试构建直接跑 `cargo build` 的产物、或换测试端口（`--port 18788`），不碰 8788。
- **改完在提交信息里写清「属哪一版待办」**，方便发版时汇总。
- 用户说「发版」时，才由那个会话统一执行：重估段位（按最高档现算）→ 升版本号
  → 补 CHANGELOG 顶条 → 构建 → 打 tag → 发 Release（附 exe 附件）。

> 发版积压：v3 时代的逐条流水已清理（对应内容要么已发布，要么并入「当前状态 → 已知遗留」）。

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
| 覆盖运行中的 `ProxyDeck.exe`（文件被锁，直接覆盖必败） | 换装走腾位法：ren 旧 exe 腾位 → 新 exe 落位 → **只重启 GUI**，后端不断 |
| taskkill ProxyDeck 进程 | **断**，先打招呼 |
| 改代码后要生效 | **断**（要重启对应服务） |
| 改 ui.html / git 提交 / push / 查日志 | 不断 |

不停服务的构建技巧：`cargo build` 的产物不锁运行中的 exe，先 `Copy-Item` 到新名字；真正的换装仍走腾位法。

## 模型情报（要点；细目见 `docs/model-intel.md`）

> 完整实测数据、时间线与单次事故记录已搬到 `docs/model-intel.md`（2026-11-21）。
> **动任何平台的请求形态 / 模型池 / 账号策略前，先读对应小节。**

- **团结（8788）**：模型 id **大小写敏感**（`KIMI-K3` 全大写）；codely 系三档同源、只是推理深度
  不同，当前承载 `DeepSeek-V4.1-Flash`；KIMI-K3 最贵慎用；上游 `/model/info` 已 403（费率是历史
  缓存，别指望实时刷新）；**UA 无版本门槛**。封号轴 = **session + 签名凭证链**（见文末红线）；
  TLS 指纹已实测**不是**轴（官方两条轨道都是纯 Node 默认 TLS）。
  **两条版本轨道都要盯**（npm `rc.x` 与桌面端 `release.x`）——只盯 npm 会漏掉桌面端先轮换种子。
- **WorkBuddy（8787 / 国际版 8789）**：模型列表 = 硬编码候选池 + 每小时可用性探测（**发现不了上游
  新增**；加模型要动三处：`modelCandidates`、`modelMeta`、ui.html 的 `CB_REF_FALLBACK`）。
  `/model/info` 只填实测值，查无实据留空（GUI 悬停显示「未核实」）。
  **错误码三分**：`11128` = 请求形态/内容指纹（同一号可自愈，v4.9.9 起改写身份句重发）→
  `11140` = **账号级限制**（改头/换号/重登都无效，只能申诉）→ `11158` = 该号不接该模型（换号）。
  **⚠ 判据陷阱**：**所有** 11128 的 `displayTips` 都写 "channel verification"，**不能拿它分流**。
  身份头/UA 已对齐官方当前版本，请求体按官方形态 gzip；TLS 指纹复刻仍未做（要换 BoringSSL/uTLS）。
- **B.AI（8891）**：模型名与实测能力不符（换皮实证），CF WAF 还会封长请求——**不适合当主力后端**。
- **Comate（8786）**：默认走**云直连**（工具调用 + 真实 usage + 2s 级），zulu 路线留作对照与兜底。
  8786 挂了先查三件事：`PLATFORM=saas` 是否注入（大写等于没设）、流式是 `element-add/patch`
  还是老 `text-delta`、登录态是不是 IDE 那个号。工具名按**短名 `__` 后缀唯一匹配**，认不出的
  原样透传（细则与五个真机坑见 docs）。

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
8. **协作流程**：见 `~/.dsh/AGENTS.md`（全局规则）+ 本项目 `CLAUDE.md`。
   `dsh-collab-mode` 插件已于 2026-11-14 卸载，不要再找它的规则段或
   executor/code-reviewer 角色工具；~~`~/.zcode/AGENTS.md`~~ 2026-09-13 退役。
   简报仍写 `.work/current-task.md`（gitignore，不进库）。
9. ~~**python build.py 静默失败**~~（**Go 时代**，Rust 已无此脚本）：教训保留——
   构建工具静默失败会让人把旧产物当新产物部署，**发版后必须字节校验产物**（现在由
   `--version` 输出 + 与 Release digest 比对完成）。
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
17. **版本串清单以文首「升版本必须同步的位置」为唯一权威**：本坑旧文曾写「6 处」/「7 处」/
    「8 处」三个互相矛盾的数字——正是计数漂移的成因，**别在任何地方复述处数**，需要时直接看清单。
    漏改任一项则应用自报旧版并误弹「发现新版本」框（弹窗当前值取 ui.html UI_CUR，
    与 appVersion 不同源；v3.10.0 实测踩坑）
18. ~~**opencode Zen 会话头 + 测试假阴性**~~（**已作废 2026-10-18**：Zen 上游已
    `ModelDeprecated`，自定义服务商与 `opencode_session` 均随 v4.2.0 删除）：出站 opencode.ai 必带
    `x-opencode-session`（进程级稳定 UUID，opencode_session.go），缺头按出口
    分片随机 400 MissingSessionID；ZCode「连接测试」按钮发裸请求必报错=假阴性，
    以真实对话为准。完整定案见 CODELY.md 2026-09-07 条
19. **同一工作树禁止多窗口并发接手同一任务**：2026-09-10 实测——三个会话
    （收到的是同一句「接手这个会话的任务」）同时改 `internal/codebuddy/server.go`
    与 `sse_cleaner_test.go`，互写工作树、各自把对方的改动当成越界，其中一个
    未打招呼就 `taskkill` 了 8787 后端配合换装（正是本文件明令要先问的那种操作）。
    接手前先 `git status` 确认没有别的会话在动同一批文件（ZCode 时代还可查
    `~/.zcode/cli/hooks/activity-*.log` 反查谁在几点改了哪个文件；该工具已于
    2026-09-13 退役，DSH 侧暂无对等日志）；commit / push / Release 这类不可逆且对外的动作
    尤其必须串行，且推送前先 `git ls-remote <remote> refs/heads/main` 复核远端
    有没有被别人推进过
20. **编译输入 = 工作区当前内容**：`cargo build` 编的是磁盘上的源码，**并行会话的未提交改动会被静默编进 exe**——2026-09-11 发 v3.10.5 时中招：
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

22. **双 exe 并行必致版本误诊，认进程一律用全路径**：测试版与正式版同名不同文件
    并跑时，看到的每个界面都可能是过期货（2026-09-17 实证：鼓轮"设置/VIBEX"错位
    投诉查了两轮，根因是旧测试版窗口没关）。已裁决单轨（测试版使命结束即删）。
    附带两条：① `Get-Process` 的 ProcessName 截断（`ProxyDeck.test-ext` 显示成
    `ProxyDeck`），认进程一律用 `Win32_Process.ExecutablePath` 全路径；
    ② 删不掉的 exe 先查谁跑在它身上（如 9/16 五个老后端跑在 `ProxyDeck.old.exe`
    上）——是后端宿主就等自然消亡，不要硬删；也不要拿改名/删除试探结论当证据。
    （v3.20.5 起更新流自动换血，此条只剩手动腾位场景。）

23. **`Copy-Item` 会保留源文件 mtime → cargo 跳过重建，人会看到"假失败"**（2026-10-18
    两次踩中）：用 `Copy-Item a.rs a.rs.bak` 备份、改文件、再 `Copy-Item a.rs.bak a.rs`
    还原时，**还原后的文件带着备份时的旧 mtime**；cargo 认为"没变过"，复用上一次编译的
    object/rlib —— 于是代码明明改对了，测试却报旧行为（当天表现为一条本该通过的用例连续
    失败，加调试打印才发现运行的是回退版）。
    两条规避：①**每次还原后 `(Get-Item <file>).LastWriteTime = Get-Date`**；
    ②更可靠的是 `git checkout -- <file>` 还原（mtime 会更新），或改完直接重编一个全新
    `CARGO_TARGET_DIR`。**排查"代码对但行为旧"时，先怀疑这个，别怀疑逻辑。**

24. **手动拉插件别用 `Start-Process -RedirectStandard*`**：重定向不生效、日志写进虚空；
    用 cmd 批处理 `>>` 追加（`.work/spawn_*.cmd`，保留历史日志，GUI 日志面板才有内容）。

## 当前状态（2026-11-21，v4.9.10 已发布）

> **逐版历史只在 `CHANGELOG.md`**（唯一权威）。本节只保留「现在什么状态、下一步注意什么」，
> 不再复制版本流水账——2026-10-18 洁癖收尾时删掉了约 200 行重复条目。

- **产品本体**：Rust（`rs-gui` + 九个插件 crate + `rs-proxy`），单 exe 约 23.8MB。
- **已删除的功能**（别再找代码）：生图/生视频 + Agnes（v4.1.0）、自定义服务商与三协议
  （v4.2.0）、**Go 参考实现整体移除（2026-10-18，155 文件 / 42,274 行）**。
  历史实现细节见 git 历史与 `CHANGELOG.md`（规则层不再保留退役代码的流水）。
- **本版主线（2026-11-21，v4.9.10）**：WorkBuddy 出站请求体按官方形态 gzip（CODEBUDDY_GZIP=0 可关，解不开自动退回明文）；v4.9.9 的 11128 改写自愈与 v4.9.8 的 UA 跟进同批。
- **11159 / /v2/report 仍未做**：11159 只出现在 Anthropic Messages 的 thinking 签名场景，而本插件没有 /v1/messages 路由（请求体也不带 thinking 块），**结构上不可达**；/v2/report 是 INTL 专属行为上报，报文形状没有真机抓包，瞎发高风险，暂缓。
- **上一版主线（2026-11-15，v4.9.0）**：Comate 改走云直连（工具调用、真实 usage、
  2s 级响应），zulu 路线留作兜底并适配 1.8.1 新协议；另修 build-rs.ps1 的
  `--version` 自检在无控制台宿主下恒为空的假失败。细节只在 CHANGELOG/提交里。
- **服务现状**（2026-11-21 02:10 实测）：GUI（PID 36496，**2026-11-16 16:40 启动**）内存里仍
  跑旧版；磁盘上的 `ProxyDeck.exe` 已是 **v4.9.10**（`--version` 可核）——**重启 GUI 才换血**，
  重启前 `ProxyDeck.exe.old-202611210114566825162` 还被它映射，删不掉。
  点火的后端只有 8786 Comate / 8793 码道；8787/8789 WorkBuddy、8788 团结、
  8785/8784/8790/8891/55990 都未启动。后端是独立常驻子进程，关 GUI 不死；
  **重启 GUI 不会自动点火**，需逐个拉杆。
- **WorkBuddy 模块裁决（2026-11-21 用户裁决，结论已定）**：**保留**该模块——不删、不转维护模式、\n  不发布「改用 magpie」公告。理由：两个区仍有可用号（实测 200），瓶颈是**账号供给**而非代码；\n  magpie 的强项已在 v4.9.8→v4.9.10 抄进产品（UA/11128 自愈/gzip）。对标结论：单点更强 ≠ 应删模块。\n- **WorkBuddy 双区状态（2026-11-21 实测）**：CN 健康号 `05f441b6`（来自 CodeBuddy CLI
  登录写入的 `Tencent-Cloud.coding-copilot.info`，ProxyDeck 池扫描自动收）与
  INTL 号 `c5c2e094`（token 有效期 2027-10）**都实测 200、带真实 usage 与 quota**；
  旧的 `e3f27d19`/`516235a0` 已 11140 标死。**CLI 登录会往桌面端同目录写文件，
  等于顺手给池子加号**——这是运维上要知道的副作用。
- **账号池内存态**：Dead/冷却/调用计数只在内存（重启清零），只有 Enabled/Removed 落盘；
  上游 reset 时点不落盘，重启丢一次无害。`codebuddy-pool(-intl).json` 已 gitignore。
  码道侧不同：续期结果写 overlay（`codearts-refreshed.json`）并同步回写账号文件，
  保证续期后 identity 变化不丢账号。
- **已知遗留**（2026-10-18 收尾后现状）：
  ① 错误响应格式不统一：**池路径 + 单账号路径**的上游错误体都已改为抽 `error.message`
     （人话，不再整串转义；抽出的文本与退回原文都按字符截 600）；判定告警仍吃**原始 body**。
     404/405 保持 Go `http.NotFound`/`http.Error` 逐字节原形（金标锁死，**不要统一**）。
  ② 插件端口无鉴权（仅绑 127.0.0.1 缓解）——**用户裁决不做**：加鉴权会破坏 GUI/客户端
     零配置直连，回环绑定是已接受的缓解面。
  ③ Comate usage：**云直连（默认）返回真实 token**（input+cached/output，带
     `cached_tokens`）；只有回退的 zulu 路线仍是 0（上游 SSE 不回 usage，留 0 比编数字诚实）。
  ④ 账号区无障碍标注——已补：甲板 `.acc-row` 与矩阵弹窗 `.mx-card` 都加
     `role="listitem"` + `aria-label`（账号+状态+调用次数），行内按钮补 `aria-label`，
     两处容器 `role="list"`。纯属性，未动布局/样式/交互。
  ⑤ hy4/各拨杆键盘可达性——**早就是已实现状态**：`.pt-gear` 带 `role="slider"`、
     `tabindex="0"`、方向键经引擎 `gearSelect`，`aria-valuenow` 随档位同步（本次只验证）。
- **`p8b5` 金标**：当前跑 3 步 HTTP 用例 + 探针/基线（provider 与媒体场景按版本有意跳过，
  跳过清单见测试文件与 `rs-tuanjie/README.md` 差异表 #46/#47）。
- 远程 main 与本地同步；仓库 github.com/Amer-CN/proxydeck（remote 名 `myrepo`，
  不是 origin；`push_api.py` 里的 `REPO` 写的是改名前旧名 `command-code-proxy-tools`，
  GitHub 会重定向，能用但名字陈旧）
- 服务治理：**GUI 管得住外部拉起的后端**——靠的不是
  进程归属，而是 `rs-gui/src/plugins.rs` 的 `PluginHost::start` 先探活端口、健康就
  「接管复用，不杀不重启」（`list` 的 `healthy` 也无条件查端口，`stop` 按端口杀）。所以旧文
  「外部拉起的实例 GUI 插件开关未必管得住它」是误解：2026-09-10 曾据此给出「拉杆会
  撞端口、得先杀掉」的错误处置建议，实际拉杆只会接管
- **换装纪律**：`ProxyDeck.exe` 换装走腾位法（rename 运行中的 exe → 新包顶替原
  路径），**后端不断、只有 GUI 需重启**；一键更新的替换目标取自
  `std::env::current_exe()`（`rs-gui/src/bridge.rs`）而非硬编码路径。v3.20.3 起腾位名唯一
  （`ProxyDeck.exe.old-<纳秒>`，不再撞残留锁）；v3.20.5 起新 GUI 凭换血标记
  把旧后端自动换血（改名不改上报路径，靠名字认不出，只能靠标记）。
- **⚠ 发版脚本禁止对 ui.html 做「全局版本串替换」**（2026-10-18 事故）：
  用 `t.replace(旧版本, 新版本)` 改整个 ui.html，会把 `CHANGELOG_DATA` 里的**历史条目
  一起改名**——连发几版后更新日志浮层出现一排同样的版本号（当时连出 12 个 `v4.2.5`）。
  正确做法：只替换那 6 个固定位置的版本串（`etchVer` / `.pt-nameplate`（含 aria-label 同行两处）/
  `buildNo` / `verChip` / `UI_CUR` / `CHANGELOG_DATA` **首条**），历史条目一个字都不动。
  已加门禁 `rs-gui/tests/ui_html.rs::changelog_versions_are_unique_and_strictly_descending`
  （版本号必须唯一且严格降序），改坏会直接红。
- `CHANGELOG.md` 顶条与 ui.html `CHANGELOG_DATA` 首条必须**逐字同源**：前者进
  GitHub Release 正文，后者进 GUI 更新日志浮层；两处不同源用户会看到两套日志
- `.work/` 现状（2026-11-15 洁癖清场后，gitignore 不入库）：`refs/` 保留 Comate 参考实现
  clone（Comate2API、magpie-comate-auth ×2）；本轮抓包/探针目录（`comate-update-*`、
  `comate-cloud-*`）与 release 记事已删（结论已进 README/CHANGELOG）；码道与其它会话的
  历史文件未动。
- **构建缓存纪律（2026-10-30 用户裁决：磁盘优先）**：Rust 的 target 曾散落成
  `E:\cargo-target\rs-*` 9 份，同一张依赖图各编一遍，12 天攒到 67GB。2026-10-30
  整目录清场（实测释放 67.14GB，E: 11.8MB → 62.5GiB）后改为：
  ① **所有 crate 共用 `E:\cargo-target\workspace`**（本机 `.cargo/config.toml`
  已钉，gitignored；`build-rs.ps1` 默认同路径）——不要再给单个 crate 设独立 target；
  ② `[profile.dev]`/`[profile.test]` 关 `incremental`，第三方依赖 `debug=0`，
  自家 crate 保留 `debug=1`（Cargo.toml 有注释；需要完整调试时用环境变量覆盖）；
  ③ 发版 `build-rs.ps1 -Final` 只清 release 产物（保留 debug 缓存），
  `-CleanTarget` 清整个共享 target；
  ④ `tools/clean-cargo-target.ps1` 在 target 超过阈值（默认 10GiB）且无 cargo
  进程时做整目录回收（新配置全量测试后共享 target 约 3.1GB，incremental 为空）；
  `target/` 已在 `.gitignore`，仓库内误留的 `rs-*/target/` 也顺手清。
- **已清理（用户 2026-09-12 明确要求删）**：`.probe/`（24MB 探测草稿）、
  `docs/glm53flash-litellm-probe-report.md`、`docs/reasoning-effort-verification.md`
  均已删除（代码零引用；此前 09-10 不入库裁决的执行）。
- 实测记录（2026-09-06 两轮全量 + 2026-09-13 三轮全量）：结论已并入 `docs/model-intel.md`、
  坑 14/15/16 与「已知遗留」；唯一未覆盖：B.AI 6-8M 字符级 WAF 极端量级（判定
  不值得复现）
- 攒版节奏继续有效（2026-09-13 用户裁决）：各会话只提交不发版，用户说「发版」时
  由该会话统一升版本号（段位现算）、worktree 干净树构建并附 exe。新坑/结论随时
  按惯例入册
- 更新检查通道：本机 gh CLI（认证 5000/h）优先 → 匿名 HTTP 兜底（60/h 按
  出口 IP 计，共享网络易撞墙，撞后负缓存 10 分钟）
- 客户端侧模型配置：tuanjie provider（8788）用 codely 系（core 即 GLM-5.3 承载入口）。
  2026-09-13 起主用 DSH（ZCode 已退役）；`dsh-collab-mode` 插件 2026-11-14 已卸载，
  模型席位以 DSH 自身会话/模型设置为准（Comate 走 8786 云直连）。

## 常用命令

```powershell
# 构建（发版走干净树：git worktree add --detach <tag>，别在脏树上编）
# 打包用 build-rs.ps1 -Final：产物拷到仓库根并校验后，脚本清 release 产物
cargo build --release -p rs-gui --locked
pwsh -File build-rs.ps1 -Final -ConfirmOverwrite

# 测试（所有 crate 共用 E:\cargo-target\workspace；本机 .cargo/config.toml 已钉）
cd rs-tuanjie; cargo test
# 需要整目录回收时（先看体积，超阈值才清；无 cargo 进程才动手）：
pwsh -File tools/clean-cargo-target.ps1 -DryRun
pwsh -File tools/clean-cargo-target.ps1

# 界面合同门禁（3 条：逐字节副本 / 版本号唯一降序 / v4 不跳版 + 与 CHANGELOG 同集合）
cd rs-gui; cargo test --test ui_html

# ui.html 内联 JS 语法校验（改完 UI 必做，改坏会白屏）
node --check <抽取出的 js>

# 换装：腾位法（后端不断，只重启 GUI）
#   ren ProxyDeck.exe ProxyDeck.exe.old-<时间戳> → 新包落位 → 重启 GUI

./ProxyDeck.exe --plugin-tuanjie --port 8788   # 单拉团结（测试用 18788，别碰 8788）
curl http://127.0.0.1:8788/health              # 健康检查
git push myrepo HEAD:main                      # 推送（remote 名 myrepo 不是 origin）
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





