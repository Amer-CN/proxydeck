# rs-qoder 黄金基准（入库）

本目录的四份材料都是**真 Go 侧 `internal/qoder` 采收**（或由它现制）的，Rust 实现按它们
逐字节对齐（门禁：`tests/golden_qoder.rs`）。

| 文件 | 内容 | 体积 | 备注 |
|---|---|---|---|
| `golden-qoder-go.json` | 纯函数（flatten/resolve/chunk/complete/quota/rfc3339/float/usage_view/账本）+ HTTP 合同（health 9 组、models 6 方法、routes 16 组、credits/set 13 步、chat 31 组含假 worker 观测） | ≈109 KB | **完整入库，无跳过** |
| `fake-worker.js` | 假 Qoder worker（协议对端：应答 stream-json，并把自己观察到的 argv/cwd/env/payload/stdin 记进日志） | 7.4 KB | 两侧用**同一份源码**；源码 SHA-256 由门禁现算校验（`worker_source_sha256`） |
| `auth-fixture/Local State` | 合成登录态的 DPAPI 加密 key（本机现制） | 389 B | **当前用户作用域** |
| `auth-fixture/auth.v1.dat` | 合成 `v10` AES-256-GCM 密文（token 是写死的假串） | 127 B | 换机解不开 ⇒ 门禁显式 SKIP |

合计 ≈116 KB（< 1 MB），所以门禁没有任何体积档位/跳过——`.work/` 被清掉也照样全绿。
`tests/common/mod.rs::golden_path()` 的加载顺序是「先本目录、缺则回落 `.work/rust-rewrite/`」。

## 三条显式跳过（不静默）

1. **DPAPI 夹具解不开**（换机/换用户）：需要登录态的场景（health ok/refresh 四步、credits/set 13 步、
   chat 31 组）会打
   `SKIP(dpapi-fixture): 合成登录态夹具解不开（DPAPI 当前用户作用域，换机即失效）—— ...`
   并在汇总行写清跳过了多少组。**重采即得新夹具**（见下）。
2. **`oversize-line`（单行 >8MB）压根不在基准里**：Go 侧实测会卡到 600s 上下文超时
   （子进程写满 stdout 管道后阻塞，父进程 `cmd.Wait()` 一直等），采一条要 10 分钟。
   语义在 `rs-qoder/README.md` 差异 5 里登记。
3. **采集机路径对不上**（换机且没重采）：`chat` 里绑采集机 `QODER_CWD` 的那几组会在本机
   打 `SKIP(machine-path): chat #N 的 QODER_CWD=... 在本机不是目录——该组未跑` 并计入
   汇总行的「显式跳过 N 组」。详见下节。

### ⚠ 跳过行/计数的可见性（`rs-bai` 同款口径）

这些 `SKIP(...)` 行与汇总行都是 `println!` 打的，而 **libtest 默认捕获通过用例的 stdout**：
**裸 `cargo test` 看不到它们**（只出 `test result: ok`），要看就带 `-- --nocapture`
（用例失败时也会随失败输出显示）：

```powershell
cd rs-qoder; cargo test -- --nocapture | Select-String 'SKIP|黄金对比'
```

「跳过」在代码里是显式分支（不静默吞掉），但「一眼可见」需要带 `--nocapture` —— 这是
libtest 的行为，不是门禁在偷偷放过。

## 采集机字面量（`meta.machine_paths`）

黄金里有两处**采集机专属**的路径字面量，它们不是合同的一部分：

* `E:\Temp` —— 采集机的 `os.TempDir()`，出现在「没给 `QODER_CWD`」那些 chat 场景的
  `worker_log[].cwd` 里（两侧的默认 cwd 都是各自的临时目录）；
* `F:\AIXM\proxydeck` —— 采集机的仓库根，是 `chat[30]` 那组的 `cwd_env`（`QODER_CWD`）。

采集器把这两个值记进 `meta.machine_paths`，门禁（`tests/common/mod.rs::MachinePaths`）在比对
期望值时做**整串相等的等价替换**：临时目录 → 本机 `std::env::temp_dir()`、仓库根 → 本机
`rs-qoder/` 的上级目录。所以**换一台机器不重采也不会假红**，而且换机后测的仍是同一条语义
（「默认 cwd = 临时目录」「`QODER_CWD` 指到哪就往哪」）。

换机后如果连**等价物都不存在**（例如第 1 节那条绑 `C:\Windows` 的场景在非 Windows 上跑），
该组会打 `SKIP(machine-path): ...` 并计入汇总，**不会**报成差异。要恢复满覆盖就重采基准
（重采会把新机器的字面量写进 `meta.machine_paths`）。

## 怎么再采

采集需要**临时往 `internal/qoder/` 放一个同包的 `_test.go`**（只有同包才能指到
`flattenMessages` / `chunkJSON` / `completeJSON` / `resolveModel` / `isQuotaError` /
`loadAuth` / `readCreditsSpent` / `usageView` 这些未导出的缝）。

采集器**原样留档在** `.work/rust-rewrite/collect-qoder_test.go`（本轮跑完已从 `internal/` 删除）。
重采时把它拷回包内、改回 `_test.go` 后缀即可：

```powershell
Copy-Item .work\rust-rewrite\collect-qoder_test.go internal\qoder\zz_qoder_golden_collect_test.go
$env:QODER_GOLDEN_OUT   = "F:\AIXM\proxydeck\.work\rust-rewrite\golden-qoder-go.json"
$env:QODER_FIXTURE_OUT  = "F:\AIXM\proxydeck\.work\rust-rewrite\qoder-auth-fixture"
go test ./internal/qoder -run TestZZCollectQoderGolden -count=1 -v

# 采完回填入库（先看内容，再覆盖）
Copy-Item .work\rust-rewrite\golden-qoder-go.json    rs-qoder\tests\golden\ -Force
Copy-Item .work\rust-rewrite\qoder-auth-fixture\*    rs-qoder\tests\golden\auth-fixture\ -Force

# 采完立即删掉临时采集器（**这一步不许省**）
Remove-Item internal\qoder\zz_qoder_golden_collect_test.go
git status --short internal/      # 必须空
go test ./internal/qoder -count=1 # 必须 ok
go vet ./internal/qoder           # 必须退出码 0
```

（若只改假 worker / 只改 Rust 侧断言，不动 Go 采集器，则 `fake-worker.js` 的入库副本要
与 `.work/rust-rewrite/fake-worker.js` 手动保持一致——门禁读的是入库那一份。
**改了假 worker 就得连带重采**：门禁会现算它的 SHA-256 与 `worker_source_sha256` 比，
仪器变了不重采必红。）

采集器把**采集机路径**写进 `meta.machine_paths`（`os.TempDir()` 与仓库根），重采即自动刷新；
`/credits/set` 的场景表在 `collectCreditsSet()` 里（P3 补修时加了尾随数据两条与数字溢出一条）。

## 采集器的四条纪律（踩过的坑都在这）

1. **绝不碰 8785**：服务起在 `127.0.0.1:0` 现取的端口上，全部请求打本地。
2. **绝不碰真实登录态 / 真实账本 / 真 worker**：`APPDATA` 与 `USERPROFILE` 都指向临时目录；
   登录态是采集器现制的**合成**密文（`CryptProtectData` + AES-256-GCM，token 写死）；
   worker 由 `QODER_WORKER` 指向同目录的假 worker。
3. **额度接口走 `http.DefaultTransport` 拦截器**合成——`fetchUsage` 用的就是它；
   而**打本地网关的跳必须用直连 client**（`Transport: &http.Transport{}` + 不跟随重定向），
   否则请求会被自己的拦截器截走（P2 那份废基准就是这么来的）。
4. **DPAPI 输出必须当次拷走再释放**：`CryptProtectData`/`CryptUnprotectData` 的
   `DATA_BLOB` 是系统内存，`defer LocalFree` 会在调用方读之前放掉它 ⇒ 实测报
   `CryptUnprotectData: The data is invalid.`。采集器里是「拷贝 → 再 `LocalFree`」。

## 门禁的归一化口径

Rust 侧回放时按采集器**同一套规则**归一化后再比（`tests/common/mod.rs`）：

* 忽略 `date` 与 `content-length` 的字面值（后者只校验「等于本次响应体长度」；HEAD 除外）；
* `"created":<数字>` → `<CREATED>`、`"id":"chatcmpl-qoder-<数字>"` → `<ID>`、
  `"uptimeSec":<数字>` → `<UPTIME>`、账本 `"updatedAt":<数字>` → `<AT>`；
* 假 worker 日志里的 `argv[0]`（脚本路径）与 `env_payload_path`（临时文件名）分别
  归一化成 `<WORKER>` / `<PAYLOAD>`——其余字段**逐字节比**；
* 流式响应的 `transfer-encoding` 显式放行（见 README 差异 7）。

## 可复现性核对（实测）

采集器本身是确定性的（假 worker、合成夹具、固定 nonce/key），两次采集**逐字段**比对的实测结果是
**27 处差异，全部是易变字段**，一处实质差异都没有：

```powershell
Copy-Item .work\rust-rewrite\golden-qoder-go.json .work\rust-rewrite\_repro-qoder-a1.json -Force
# 再跑一遍采集（同上），然后逐字段比对（递归 walk + 逐键比）
```

| 差异类别 | 条数 | 说明 |
|---|---|---|
| `chat[*].body_out` 里的 `"created"` 与 `"id":"chatcmpl-qoder-<随机>"` | 19 | 门禁本来就会归一化这两个值 |
| `resp_headers.content-length` | 3 | 内嵌随机 id 的位数，1~2 字节之差（门禁只校验「等于本次体长」） |
| 账本 `updatedAt`（时钟毫秒） | 6 | 门禁归一化成 `<AT>` |
| `meta.collected_at` | 1 | 采集时刻 |

**其余全部逐字一致**：假 worker 的 argv/cwd/env/payload/stdin 记录、所有状态码与头集合、
非易变响应体、纯函数段（flatten/resolve/chunk/complete/quota/rfc3339/float/usage_view）。

**第三次采集（P3 补修，加了 3 条 `/credits/set` 与 `meta.machine_paths`）与上一代逐字段比**：
归一化易变字段（`created`/`id`/`uptimeSec`/`updatedAt`/`content-length`/`date`/`collected_at`）后
**只剩 6 处差异，全部是本次有意新增**（3 条 credits/set + 2 个 meta 字段 + 数组长度 1 处），
上一代的每一个测量值都被原样复现：

```powershell
node .work/rust-rewrite/_diffqoder-p3.mjs .work/rust-rewrite/_repro-qoder-a1.json .work/rust-rewrite/golden-qoder-go.json
# 总差异 6 条；其中实质差异（不含易变字段）6 条 —— 逐条即为上面那 6 处
```

⚠ 一个容易踩的点：`CryptProtectData` 的输出**每次都不一样**（内含随机 salt），
所以 `auth-fixture/Local State` 两次采集的密文不同——但两者解出来的 AES key 相同
（采集器写死 `key[i] = i*7%251`），解密后的 payload 与 Golden 里的期望值完全一致，
门禁实测照样全绿。入库时「夹具与基准同批采」只是图个整齐，**不是必须**。

