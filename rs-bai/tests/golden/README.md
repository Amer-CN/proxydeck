# rs-bai/tests/golden —— 黄金基准

这些文件是 **Rust 侧对齐 Go 的存证**：每条都来自 Go 侧真实代码路径的实测输出，
不是手写的期望值。放在仓库里（而不是 gitignore 的 `.work/`）是为了让 `cargo test`
不依赖 `.work/`——那是临时目录，清掉会让整批门禁转红。

## 入库的只有小体积基准

| 文件 | 体积 | 是什么 | 谁读它 |
|---|---|---|---|
| `golden-adapt-go.small.json` | 5.2KB | **常规用例**（F1~F6、F9~F12 共 10 条）的 Go 侧出站请求体原文 | `tests/golden_adapt.rs` |
| `fixtures.small.json` | 3.8KB | 上表对应的 10 条脱敏改写用例（无 Authorization、无真实账号），人工对照材料 | 人工（`golden-adapt-go.small.json` 的输入） |
| `golden-p1-go.json` | 58KB | `/model/matrix`（含 data:null、data compact+HTML 转义、配置解析容错）与 `/v1/stats` 的 17 场景 / 42 步逐字节响应（含响应头） | `tests/p1_golden.rs` |
| `golden-compact-go.json` | 3.3KB | Go `encoding/json` 把 `json.RawMessage` 编进响应时的 compact + HTML 转义输出（含 `1e999` 逐字节搬运、非法数字整体失败等边角） | `src/matrix.rs` 的 `compact_json_go` 单测 |

## 全量基准不入库（工作树体积 65MB）

全量的 `fixtures.json`（27.7MB）与 `golden-adapt-go.json`（37.8MB）**只在
`.work/rust-rewrite/` 下**（该目录 gitignore）。裁掉的是 **F7/F8 两条超大用例**：
入参体 20MB / 7.5MB，守的是「超 1M token 截断最早轮次」这一条改写分支——
属少数派场景，而代价是每次克隆/换机都多落 65MB 工作树。

入库的那份是**小号基准**（`*.small.json`）：干净检出上常规用例照跑。判定规则写在
`.work/rust-rewrite/mksmall.mjs` 里（`in` 体 > 1 MiB 即视为超大用例，当前命中 F7、F8），
不是按 id 硬编码。

**超大用例缺基准时的行为（`tests/golden_adapt.rs`）**：全量基准在（本机 `.work/` 里有）
就跑、覆盖度 12/12；不在就**逐条打印**
`SKIP(oversized): baseline not vendored — run .work/rust-rewrite/mkfixtures.mjs to regenerate`
并在汇总行打印跳过条数（覆盖度 10/12）。**绝不静默跳过**，也绝不因此把测试判失败。
libtest 默认捕获通过用例的 stdout，要看这几行得跑
`cargo test --test golden_adapt -- --nocapture`。

## 加载顺序

两个黄金测试都按 **`rs-bai/tests/golden/` 优先、缺则回落 `.work/rust-rewrite/`** 取文件
（`rs_bai::golden_path()`）：入库副本在，`cargo test` 就与 `.work/` 无关；若有人在 `.work/`
里重新采集（想比对新版），临时移走入库副本即可让回落生效。
**必需的那几份**（上表四个）两处都没有时测试**失败**，不静默跳过——黄金门禁是这条重写路线的前提；
只有全量基准里的超大用例那一档允许缺失，且缺失时必须显式打 SKIP。

## 再生成路径

全量基准的生成链（在仓库根跑）：

```powershell
node .work/rust-rewrite/mkfixtures.mjs          # 1) 全量用例集 → .work/rust-rewrite/fixtures.json
go run ./.work/cli/go-adapter                   # 2) Go 侧逐字副本跑一遍 → .work/rust-rewrite/golden-adapt-go.json
node .work/rust-rewrite/mksmall.mjs             # 3) 裁小号基准 → rs-bai/tests/golden/*.small.json（入库）
node .work/rust-rewrite/provenance.mjs          # 可选：校验 go-adapter 的副本与 internal/bai/server.go 逐字一致
```

- `mkfixtures.mjs` 里 `oversizedMessages()` / `oversizedWithToolTail()` 两个函数就是 F7/F8
  的超大体量来源（1.5M 字符 × 轮数）；想调体量改这两个函数的参数即可，改完重跑上面三步。
- `go-adapter`（`.work/cli/go-adapter/main.go`）是 `internal/bai/server.go` 里
  `adaptQuirks` / `truncateToContext` / `estTokens` / `baiContextLimit` 的**逐字副本**，
  只把它从 HTTP 中间件形态改成 `[]byte → []byte`；`provenance.mjs` 会拿源文件逐行核对，
  副本漂移即退出码非 0、黄金对照作废。
- 其余两个基准（`golden-p1-go.json` / `golden-compact-go.json`）的采集方式记在
  `rs-bai/README.md` 与任务简报里：前者由 `internal/bai` 包内一次性采集器（真 `Server.Start()`
  + 假上游 + 临时渠道配置）采集，后者由 `.work/cli/recompact` 用真 `encoding/json` 采。
