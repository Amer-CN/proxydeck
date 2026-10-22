# rs-opencode

ProxyDeck 的 OpenCode 插件（测试端口 **8784**）。把用户机器上真实的 `opencode serve`
翻成本地 OpenAI 兼容接口，**不打包 opencode 本体**，Rust 侧只做协议翻译。

第一版范围：纯文本对话（chat）。工具调用 / MCP 桥接、GUI 甲板、一键安装都放到第二版。

## 运行

```powershell
# 1) 用环境变量指定 opencode（测试/便携）
$env:PROXYDECK_OPENCODE_EXE = 'E:\Temp\ocv2\package\bin\opencode.exe'

# 2) 起插件（默认 127.0.0.1:8784）
cargo run -p rs-opencode -- --plugin-opencode --port 8784

# 3) 打开接口
curl http://127.0.0.1:8784/health
curl http://127.0.0.1:8784/v1/models
```

```powershell
# 非流式
curl -H 'content-type: application/json' -d '{"model":"opencode/deepseek-v4.1-flash-free","messages":[{"role":"user","content":"hi"}]}' http://127.0.0.1:8784/v1/chat/completions

# 流式
curl -N -H 'content-type: application/json' -d '{"messages":[{"role":"user","content":"hi"}],"stream":true}' http://127.0.0.1:8784/v1/chat/completions
```

## opencode 定位顺序

1. `PROXYDECK_OPENCODE_EXE`（绝对路径，最高优先级，测试用）
2. `%APPDATA%\ProxyDeck\opencode\` 下**最新版本目录**里的 `opencode.exe`
3. `PATH` 上的 `opencode`
4. 都没有 → `/health` 返回 `{"ok":false,"reason":"opencode_not_installed"}`，
   `/v1/*` 返回 503。**第一版不做一键安装。**

默认**优先接管你已经在运行的 opencode**（TUI 的后台服务）：读
`%LOCALAPPDATA%\state\opencode\service.json` / `~/.local/state/opencode/service.json`，
`/api/info` 探活通过就直接用——同一个登录态、同一个数据库，**退出时不会停掉你的 TUI**。
探不到才自己 spawn 一个 `opencode serve`（端口 20000–29999 挑空闲、密码走
`OPENCODE_SERVER_PASSWORD`、退出时 taskkill），`--no-attach` 可强制自己起。
Windows 上自 spawn 的子进程会挂进 `KILL_ON_JOB_CLOSE` 的 Job Object：父进程被
`taskkill /F` / `TerminateProcess` 强杀时由内核带走，不留孤儿。

**识别 opencode 的落点（宁可多找，绝不误报"没安装"）**：

1. `PROXYDECK_OPENCODE_EXE`（显式指定）
2. 本插件自装目录 `%APPDATA%\ProxyDeck\opencode\<版本>\`
3. `%LOCALAPPDATA%\Programs\opencode\`、`opencode-desktop\`、`OpenCode\`，以及
   `%LOCALAPPDATA%\Programs\*opencode*\`（桌面客户端布局，扫两层）
4. `%USERPROFILE%\.opencode\bin\`（官方 install 脚本落点）、Scoop（`scoop\shims`、
   `scoop\apps\opencode\current`）
5. `%PROGRAMDATA%\chocolatey\bin\`
6. npm 全局：`@opencode\cli\bin\`、旧包名 `@opencode-ai\cli\bin\`、`opencode.cmd` shim
7. `PATH`

只有以上全部落空才报 `opencode_not_installed`；即便如此也不会自动下载，
必须由界面上的「一键安装」按钮显式触发。已安装的用户**永远不会被提示重装**。

## 配置

`%APPDATA%\ProxyDeck\opencode.json`（不存在就用下面的默认值）：

```json
{
  "providers": ["opencode", "opencode-go"],
  "modelDenylist": ["beam-free"],
  "defaultModel": "opencode/deepseek-v4.1-flash-free",
  "timeoutSeconds": 300,
  "cooldownSeconds": 60
}
```

* `providers`：允许路由的 `providerID` 白名单（`/v1/models` 与聊天选模都按它过滤）。
* `modelDenylist`：对 `<providerID>/<modelID>` 做大小写不敏感子串匹配；`beam-free`
  因中国大陆地区限制默认拉黑。
* `defaultModel`：入站未指定模型时的首选；它若也在冷却，照样如实回 429（不换别的模型）。
* 模型 id 形态：`/v1/models` 只给裸名（如 `deepseek-v4.1-flash-free`，`owned_by` 保留
  provider）；入站不管填裸名还是 `opencode/xxx` 全名都能用，裸名按 `providers`
  的顺序还原成第一个实际提供它的 `<provider>/<model>`（重名时排前面的赢）。
* `timeoutSeconds`：单轮总超时，超时杀会话并回 504。
* `cooldownSeconds`：单模型撞 429 后的内存冷却时长；冷却期内再点名该模型 → 429 + `retry-after`。
  **绝不静默换模型**：用户要哪个就是哪个，再差也不偷换。

配置读不出来 / JSON 坏掉只打日志并退默认值，不会让插件起不来。

## 协议翻译

* `messages` 压平成一段 prompt：`[system]` / `[user]` / `[assistant]` /
  `[tool result for <id>]`；`content` 数组只取 `type == "text"`；空内容跳过；
  压平后为空 → 400 `request contains no text content`。
* 每个请求：`POST /api/session` → `POST /api/session/{id}/model` →
  `GET /api/event` → `POST /api/session/{id}/prompt`。
* 事件：`session.text.delta` → `delta.content`；`session.reasoning.delta` →
  `delta.reasoning_content`；`session.step.ended.tokens` → `usage`；
  `session.execution.succeeded/failed/interrupted` 结束本轮。
* `permission.asked` **固定回 `{"decision":"reject"}`**：不收权限 = 不让模型动用户机器。
* 全局 `Semaphore(1)`：同一时间最多 1 个上游会话（免费档吃不了并发）。
* 429 → HTTP 429 `rate_limit_error`；国家限制 → 400 `invalid_request_error`；
  其它失败 → 502；超时 → 504。
* 流式：首帧 `delta.role=assistant`，随后 content / reasoning_content 增量，
  结束 `finish_reason=stop` + `data: [DONE]`。预热 15 秒内若第一帧就是 429/国家限制，
  会在发 SSE 头之前回真实状态码。

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | `Args` + `run(args)`、axum 路由、选模/冷却、SSE 编排 |
| `src/main.rs` | `--plugin-opencode` 入口 |
| `src/discover.rs` | 定位 opencode + 读 attach 的 `service.json` |
| `src/server.rs` | 托管 `opencode serve`（挑端口/密码/就绪/Job Object 兜底/taskkill） |
| `src/upstream.rs` | opencode HTTP API 客户端（Basic auth）+ 错误分类 |
| `src/translate.rs` | 消息压平、SSE 解码、OpenAI 帧构造 |
| `src/models.rs` | `/api/model` 解析、白/黑名单、选型（绝不静默换模型）+ 冷却 |
| `src/config.rs` | `opencode.json` 读取与默认值 |
| `src/datadir.rs` | 数据目录解析（沿用 `rs-bai` 判定链） |
| `tests/http_contract.rs` | 假 opencode 上游 + HTTP 契约用例 |

## 已知不做 / 限制

* MCP 工具桥接已落地（客户端 `tools` 转真 `tool_calls`，默认开，失败自动降级纯文本）。
* 一键安装、GUI 甲板已落地（见 `app/ui.html` 的 OPENCODE 甲板）。
* PATH 回退认 `opencode.exe` / 无扩展名的 `opencode`；npm 生成的 `opencode.cmd`
  启动器会自动解析它指向的真实 exe（解析不出才跳过）。
* 暂不转发 `#variant`（如 `opencode/xxx#high`）：`#` 之后会被忽略。
* `max_tokens` / `temperature` 只读取、不转发（opencode 会话发送接口无此入口）。
* 并发请求串行排队；排队超过 `timeoutSeconds` 回 503。
* 流式一旦发出 200，之后再出错只能在流内发 `{"error":...}` + `[DONE]`，无法改状态码
  （预热窗口只覆盖"首帧就失败"的常见情况）。
* 托管的 `opencode serve` 启动后若自行退出，插件不会自动重启（健康检查会报
  `opencode_unreachable`）。
* 非 Windows 平台没有 Job Object 兜底：父进程被 `SIGKILL` 时仍可能残留子进程
  （Ctrl+C / 正常退出走 `Drop` + `kill -9`）。
