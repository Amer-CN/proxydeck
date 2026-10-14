# rs-bai —— B.AI 插件的 Rust 试点（绞杀者路线第一步）

本 crate 是「不整仓推倒，先拿无红线的边缘插件试点」的第一个产物：把 Go 版
`internal/bai`（8891）的对外合同照搬到 Rust，**先在测试端口独立跑，不注册进
`app/plugins.go`，不改 Go 构建，不动任何现行服务端口**。

- Go 侧参照：`internal/bai/server.go`（请求改写 / 反向代理）、`internal/bai/models.go`（矩阵与密钥）
- 基线提交：`3a5ee06`
- 本试点状态：4 个端点 + 请求改写 + CORS + 重试；`/model/matrix`、`/v1/stats` 未迁（见下）

## 跑起来

```powershell
cargo build --release
.\target\release\rs-bai.exe --plugin-bai --port 18788          # 测试端口，勿用 8891
curl.exe -s --noproxy "*" http://127.0.0.1:18788/health
```

参数与 Go 侧同名同形（`--plugin-bai` / `--port` / `--host`），另加一个 `--upstream <url>`
（默认 `https://api.b.ai`，仅供黄金回放/本地捕获对照改指向）。

## 与 Go 侧对齐的部分

| 项 | Go 侧 | 本 crate |
|---|---|---|
| 上游 base | `https://api.b.ai` | 同（`--upstream` 可改） |
| 路由 | `/health`、`/model/matrix`、`/v1/stats` 本地；其余透传 | 同（后两条见「未迁」） |
| `adaptQuirks` 四条改写 | developer→system、`max_tokens>8192→8192`、`reasoning_effort:"max"→"high"`、超 1M token 截断最早轮次 | 逐条复刻，见 `src/adapt.rs` |
| `estTokens` 估算口径 | 4 字符≈1 token；string content / 数组 text / data URI 图 / 外链图 512 / tool_calls.arguments；每条 +4 | 同 |
| CORS | `*` / `GET, POST, OPTIONS` / `Content-Type, Authorization`；OPTIONS→204 | 同 |
| 代理自探测 | 先看 `HTTPS_PROXY`/`https_proxy`，否则试连 7897/7890/7892/7896/10809/1080 | 同（`src/transport.rs`） |
| 请求体上限 | `bufferBody` 缓冲到 32MB（`server.go:503`） | 同，`MAX_BODY_BYTES` 显式设 `DefaultBodyLimit`（axum 默认只有 2MB） |
| 超时口径 | 只给连接超时：`http.DefaultTransport` 的 `DialContext` 30s；**无含响应体的总超时**（`http.Server` 未设 `ReadTimeout`/`WriteTimeout`/`IdleTimeout`） | 同：`connect_timeout` 30s + `pool_idle_timeout` 90s，**不设 `.timeout()`** |
| 重试 | 传输错误或 5xx 最多 3 次、间隔 300ms；4xx 不重试 | 同 |
| 流式及时下推 | `FlushInterval = 50ms` | 流式响应体直通（chunked），逐帧下推 |

## 已知差异（试点期如实登记）

1. **TLS 指纹**：Rust 无 uTLS 对等物，ClientHello 是 rustls 默认形态，与 Go `net/http`
   不同。B.AI 无签名/指纹红线，本轮不处理；团结 8788 才会真正撞上这一条。
2. **出站头**：`Host` 由 URL 决定（同 Go 的 `Director` 改 `req.Host`）；
   `User-Agent`/`Accept-Encoding` 由各自 HTTP 客户端生成，必然与 Go 不同；
   Go `httputil.ReverseProxy` 会补 `X-Forwarded-For`/`X-Forwarded-Host`，本试点未补
   （对上游语义无影响，如要对齐在这一处加）。
3. **`/model/matrix`、`/v1/stats` 未迁**：Go 侧这两条由本进程应答（矩阵要读
   `tuanjie-water-channels.json` 里的 B.AI Bearer，密钥是 DPAPI 密文；统计要落
   `bai-stats.json`）。本试点对这两条**本地回 503 `not_implemented_in_pilot`**，
   而不是漏给上游——漏出去会变成对 `api.b.ai` 的无效请求（Go 侧这两条明确不回上游）。
4. **统计**：Go 的 `statsWith` 旁路拷贝响应体抠 `usage` 入账，本试点未实现（属 `/v1/stats` 一块）。
5. **请求体上限与超时（2026-10-14 第 2 轮修正，原为差异，现已对齐）**：
   ① axum 的 `Bytes` 抽取器默认只收 **2MB**，而 Go `bufferBody` 是 **32MB**
   （`server.go:503`）——未修前 2MB~32MB 的请求在本进程就被 413 挡掉，Go 却能转发，
   且「超 1M token 截断」在 HTTP 路径上不可达。现由 `MAX_BODY_BYTES` 显式对齐 32MB，
   回归用例 `tests/http_contract.rs::large_body_reaches_upstream`（3.6MB 请求体经真 HTTP
   路径到达上游且逐字节不改写）把关。
   ② 原先给 reqwest 设了 `timeout(600s)`，那是**含响应体的总超时**，会把长流式响应掐断；
   Go 对上游**没有总超时**（只有 30s 连接超时）。现按 Go 语义去掉，见上表「超时口径」。
   ③ `HTTPS_PROXY` 检测命中时**只报存在性、不打印变量值**（对齐 `server.go:202` 的
   Go 日志；代理地址可能含凭据）。

## 验证

```powershell
cargo test                       # 单测 + 黄金对照 + HTTP 合同（15 个，其中 1 个默认 ignored）
cargo test --test golden_adapt   # 单独跑黄金门禁
cargo test --test http_contract large_body    # 单独跑 >2MB 体量回归
cargo test --test http_contract slow_stream -- --ignored   # 长流不断流验证（约 10 分钟）
```

黄金门禁口径：`.work/cli/go-adapter`（Go 侧 `adaptQuirks`/`truncateToContext`/`estTokens` 的
逐字副本，`.work/rust-rewrite/provenance.mjs` 校验与 `internal/bai/server.go` 一字不差）对
`.work/rust-rewrite/fixtures.json` 产出 `golden-adapt-go.json`；`tests/golden_adapt.rs` 用同一组
fixture 跑本 crate 的改写，逐字节比对。缺黄金文件时测试**失败**而不是跳过。

`tests/http_contract.rs` 里两条体量/时长相关的用例值得单独说：
`large_body_reaches_upstream`（3.6MB 请求体，秒级）默认就跑——它守着"32MB 上限别被改回 2MB"；
`slow_stream_is_not_cut_by_total_timeout`（读完 600s 后仍在收帧，约 10 分钟）默认 `#[ignore]`，
守着"别再加回总超时"，改动 `src/transport.rs` 的出站客户端后建议手动跑一次。
注意后者**测试客户端自己不设总超时**：给客户端设时限会让"本 crate 有没有时限"被测不出来。
