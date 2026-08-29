# B.AI 模型矩阵 · 设计

日期：2026-08-29 · 状态：已与用户定稿 · 影响面：`internal/bai/`、`app/ui.html`、`CLAUDE.md`

## 背景与实测现状

B.AI 甲板（COMMAND 键副页，8891 → `https://api.b.ai` 透明转发）的「模型矩阵」区块
一直存在，但**从来没有真正出过内容**：

- `app/ui.html` 的 `loadPluginModels()` 对所有插件甲板统一拉
  `http://127.0.0.1:<port>/v1/models`，**不带 Authorization**。
- B.AI 是纯透传（`internal/bai/server.go:87`），没有 key 就原样把 401 回给 GUI。
- 于是甲板显示 `0 MODELS · 后端暂无可调用模型（预算超限？）`——文案还是猜的。
- 更糟：这个无 key 请求被 3 秒轮询驱动（`_pluginPoll`），实测已在上游刷了 22 小时的 401。

带上 key 实测（2026-08-29）：上游 `/v1/models` 回 **44 个模型**，字段只有
`id / object / created / owned_by / supported_endpoint_types`，`created` 全部是
1626777600（占位），**没有任何价格或免费标记**。`/model/info`、`/v2/models/info`、
`/key/info`、`/user/info`、`/tags` 一律 403——网关只放行推理路径。

## 目标

1. B.AI 甲板显示全部模型，按厂商归类，与 COMMAND 甲板同一套 chips 视觉。
2. 拉杆点火时刷新一次，让官方增删模型能自动反映到矩阵。
3. 消除 GUI 对 api.b.ai 的周期性空打（零轮询铁律）。

## 非目标（明确砍掉）

- **不做「免费模型」独立分类。** 上游不提供免费口径，任何名单都得手工背债，官方
  一改就要跟着改。用户 2026-08-29 明确否决：「不如一开始就不要这个功能」。
- 不动 `/v1/models` 透传语义；不给其他三个插件甲板改分类逻辑；不做模型价格/上下文元数据。

## 决策记录

**端点形态：新增 GUI 专用 `GET /model/matrix`，`/v1/models` 一个字节都不动。**
被否方案：改造 `/v1/models` 做「有 key 透传、无 key 走缓存」——前端零改动，但会让
「不带 key 也能列出模型」这种假象混进代理契约，以后排障会骗人。新端点与团结已有的
`/model/info`、`?full=true` 是同族做法（GUI 专用旁路端点）。

**key 来源：`tuanjie-water-channels.json` 中 `id:"bai"` 条目。** 该文件已在用（注水检测
的 bai 渠道 Bearer 就取这里）、已 .gitignore、注水检测页已有密钥输入框——不加任何新
配置 UI。被否方案：`import internal/tuanjie` 复用其导出的 `LoadChannelKeys()`——它依赖
包内私有的 `exeDirOverride` 测试缝，bai 借不到，单测无法把密钥路径指向临时文件；为此
导出测试钩子又是在插件之间制造耦合。改为 bai 包内自带 15 行读取器 + 可注入路径变量。

## 架构

### 后端 `internal/bai/models.go`（新文件）

```go
// 上游 data 数组原样透传，不重塑字段（前端 ids 取值逻辑因此一行都不用改）
type matrixResp struct {
    OK        bool            `json:"ok"`
    Source    string          `json:"source"`     // live = 本次真打了上游；cache = 吃缓存
    FetchedAt int64           `json:"fetched_at"` // unix 秒；0 = 从未成功
    Count     int             `json:"count"`
    Data      json.RawMessage `json:"data"`
    Err       string          `json:"err,omitempty"` // need_key | upstream_error
}
```

- 缓存 `matrixState{loaded, data, count, fetchedAt, errKind, errAt}` 挂在 `Server` 上，
  `sync.Mutex` 全程持有 = single-flight（并发请求排队，第一个拉完后面的直接吃缓存，
  不需要额外 inflight 标志）。空判用 `loaded` 而不是 `count > 0`——上游合法返回空数组时
  不该每轮询都重打一次。
- `refreshLocked()`：读 key → GET 上游 `/v1/models` → 校验并缓存 data 原文。上游走已有的
  `detectUpstreamTransport()` + `retryRoundTripper`（境外直连不稳，复用同一套重试）。
- **无 TTL**。矩阵只在三种情况下打上游：缓存从未填充、`?refresh=1`、且未被负缓存拦住。
  刷新时机由拉杆决定（用户口径），不由时间决定——加 TTL 反而会在长跑进程里偷偷打官网。
- 负缓存：失败（含 `need_key`）后 60 秒内不再重试（仿 `internal/tuanjie/quota.go:107` 的
  `quotaErrTTL`），防止 3 秒轮询在失败态打穿上游。
- 失败保旧值：上游挂了但已有旧缓存 → 回旧数据 + `source:"cache"`，不清空、不报错。
- 端点失败也回 HTTP 200 + `ok:false` + `err` 码，不把「没配密钥」这种可解释状态变成状态码。

### 端点 `GET /model/matrix`

挂在 `mux` 上，与 `/health` 同级。Go 的 `http.ServeMux` 按最具体模式匹配，注册顺序不影响
路由归属（`/model/matrix` 不会被后登记的 `"/"` 透传吞掉），只有重复注册同一模式才会 panic。
CORS 由既有 `corsWith` 覆盖。`?refresh=1` 跳过负缓存强制刷新。

### 实现期又发现两处对境外的 3 秒空打（同批修掉）

审计整条 bai 轮询链，除了 `/v1/models` 还有两处会经 `mux.Handle("/", proxy)` 漏到 api.b.ai：

- `loadPluginMeta()` 每 3 秒拉 `/model/info`，bai 本地无此路由 → 原样打到境外（回 403）。
- `loadPluginStats()` 每 3 秒拉 `/v1/stats`，同样漏到境外；而 B.AI 甲板本就没有 `baBars`
  消耗条元素，这段结果从未被使用。

两处都对 bai 直接跳过。零轮询铁律的完整口径是「轮询只打本地，任何路径都不许顺路漏到境外」。

### 闩锁改造

`V.fetchOnRun` 原先由 tj / cb 分支各自「读 + 清」，而 B.AI 没有积分分支 → **漏清**。一旦把
矩阵刷新挂到这个闩锁上，漏清就等于每 3 秒强制刷一次上游。改为在 `refreshPluginView` 顶部
一次性消费成局部 `justIgnited`，矩阵与积分共用；`↻ 刷新` 对 bai 也置位同一闩锁（与拉杆点火
同权），不新造第二条刷新通路。

### 前端 `app/ui.html`

- `loadPluginModels(V, refresh)`：bai 换 URL 到 `/model/matrix`，其余甲板不变。后端把上游
  `data` 数组**原样透传**（不重塑字段），所以前端 `ids = d.data.map(m => m.id)` 一行都不用改。
  `owned_by` 实测不可靠（glm 系标 `unknown`、`claude-sonnet-4.5` 标 `claude` 而其余 claude
  标 `mixai`），**不作为分组依据**，分组仍以 id 前缀为准。
- `pluginVendorOf(id)` 只改**兜底返回值**：全不命中时取分隔符前的前导段并去掉尾部数字。
  不往正则里加 `qwen|mimo`——那样每来一个新厂商就得改一次表。实测 44 个模型全部分进
  10 个干净厂商组（openai 12 / claude 10 / gemini 5 / glm 4 / deepseek 3 / qwen 3 /
  kimi 2 / minimax 2 / mimo 2 / hy3 1），且其他甲板的 id 形状（`b3o`、
  `glm-5.3_哈希`、`codely/GLM-5.3`）分组结果一字不变。`PLUGIN_VENDOR_COLORS` 补
  `qwen`、`mimo` 两色（沿用二者在 `VENDOR_COLORS` 里自己的色系）。
- `baStModelsSub`：`实时拉取` → 显示 `缓存于 HH:MM`（数据本来就不实时，写"实时"是撒谎）；
  `baModelCount` 维持 `N MODELS`。
- 刷新时序：沿用甲板既有的三段式——`V.fetchOnRun` 闩锁（只有拉杆点火成功那一次置真）→
  首次 `loadPluginModels` 带 `?refresh=1`；`↻ 刷新` 按钮带 `?refresh=1`；3 秒轮询不带参数，
  只读 8891 本地缓存。停堆时 `fetchOnRun` 复位（已有逻辑）。

## 错误与边界

| 情形 | 表现 |
|---|---|
| 未配置 bai key | `err:"need_key"`，甲板文案「未配置 B.AI 密钥（注水检测页填入）」 |
| key 无效 / 上游 401 | `err:"upstream_error"` + 60s 负缓存；有旧缓存则继续画旧缓存 |
| 上游连不上 / Cloudflare 5xx | 同上（重试已由 `retryRoundTripper` 兜底） |
| 插件未运行 | 沿用现有 `OFFLINE` / 「启动插件后自动拉取」分支 |
| 上游新增模型 | 下次拉杆自动出现，无需改码 |
| 上游删除模型 | 下次拉杆自动消失，无需改码 |

## 清理（与本功能直接相关的两项）

1. 删 `app/ui.html:2468` 的死灯 `baiFree`——HTML 里声明了，全项目无任何 `PTApp.dot('baiFree')`
   调用，从未点亮过；这次明确不做免费分类，留着更误导。保留 `baiProxy`（「需代理」）灯。
2. 改 `CLAUDE.md:94` 「b.ai —— 已移除（2026-08-19，用户明确要求）/ 用户再提起就提醒别接」
   ——那是 v2.5.1 时期的结论，B.AI 插件在 v3.5.0 已回归且正在跑。该条会误导后续任何
   接手的人（包括我自己）。

## 测试

`internal/bai/models_test.go` —— 测试缝是三个包级变量：`upstreamModelsURL`（指向 httptest
假上游）、`channelsConfigPath`（指向临时密钥文件）、`matrixTransport`（换成不带系统代理的
client，否则 `127.0.0.1` 会被环境变量代理劫走）。假上游用请求计数器证明「有没有漏到上游」。

1. `TestMatrixServesUpstreamList` —— 清单原样落 `data`，条数与顺序一致，`source:"live"`。
2. `TestMatrixCachedUntilRefresh` —— 6 轮轮询只打上游 1 次；`?refresh=1` 才打第 2 次。
3. `TestMatrixSingleFlight` —— 冷启动 8 个并发请求，上游计数为 1。
4. `TestMatrixNeedKey` —— 配置里无 bai 条目 → `err:"need_key"` 且上游计数为 0。
5. `TestMatrixNegativeCacheOnFailure` —— 上游 502 → `err:"upstream_error"`；随后 5 轮轮询
   不再新增请求（60 秒负缓存）。
6. `TestMatrixKeepsStaleOnFailure` —— 已有清单后上游挂 → `ok:true` + `source:"cache"` +
   条数不丢。
7. `TestBaiChannelKeyReadsSharedConfig` —— 只取 `id:"bai"` 条目；文件缺失 / JSON 坏掉回空串
   且不 panic、不外泄文件内容。

前端分类靠端到端看甲板验证（见下），不为 JS 引测试框架。

## 验证（2026-08-29 实测）

1. `go build ./...` + `go vet ./internal/bai/` + `go test ./internal/bai/ -count=1` → 全绿。
2. 端到端用**独立文件名** `ProxyDeck-worktree.exe` 起在 8891（经用户同意停掉当时的旧 B.AI 插件），
   `ProxyDeck.exe` 全程未替换、未改版本号与 CHANGELOG；验证完已删临时 exe 并用原 exe 复位 8891。
3. 已验证：甲板 42 个模型 / 9 个厂商组，组名无版本号尾巴（`hy3`→HY3、`qwen3.8-27b`→QWEN）；
   `baStModelsSub` 显示「缓存于 HH:MM」；点「↻ 刷新」→ 恰好一条 `/model/matrix?refresh=1`
   且插件日志仅多一行「模型矩阵已刷新」；3 秒轮询若干轮只发无 refresh 的请求，日志零新增；
   重新载入页面只画缓存（时间戳仍是上次的 15:12，不打上游）；
   `ok:false` 两条分支上屏分别渲染为 `NO KEY` +「未配置 B.AI 密钥（注水检测页填入）」、
   `ERR` +「上游拉取失败 · 60 秒内不重试…」。
4. 未验证：**拉杆物理手势本身**——内置浏览器当时 `innerWidth=0`（后台页），指针拖动无法可靠模拟，
   改用同一条闩锁路径上的「↻ 刷新」按钮取证；拉杆 → `V.fetchOnRun=true` 是改动外的既有两行代码。
   正式上杆时请按第 3 条口径复核一次。
