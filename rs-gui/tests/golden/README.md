# tests/golden —— P7a 桥接层的黄金基准

## `bridge-go-reference.json`

**来源**：`.work/rust-rewrite/_p7a_collect_test.go`（原样留档；执行时临时拷进 `app/` 跑完即删）
跑**真实 Go 宿主**采出来的参考值。采集方式不是转抄闭包体，而是：

1. 给 `webview.WebView` 接口写一个假实现（只实现 `Bind` / `Unbind` / `Window`）；
2. 让生产代码 `app.bindAll(fake)` 把真实的 26 个绑定闭包原样交出来（`WebView::Window()` 返回 nil，
   于是所有需要窗口的分支走「窗口句柄不可用」——与 Rust 门禁的 `NoWindowHost` 同一条路径）；
3. 用 `reflect.Call` 按位置传实参调用，逐条记录返回串；
4. 顺带记录场景目录里的落盘文件（相对路径 → 内容）。

复采命令：

```powershell
Copy-Item .work/rust-rewrite/_p7a_collect_test.go app/zz_p7a_collect_test.go -Force
$env:P7A_OUT='F:\AIXM\proxydeck\.work\p7a\go-bridge-reference.json'
go test ./app -run TestP7ACollect -count=1 -v
Remove-Item app/zz_p7a_collect_test.go -Force
Copy-Item .work/p7a/go-bridge-reference.json rs-gui/tests/golden/bridge-go-reference.json -Force
```

**隔离**（绝不碰用户真实状态）：`PROXYDECK_DATA_DIR` 与 `APPDATA` 都指临时目录；
`PATH` 清空 + `HTTPS_PROXY` 指向死端口 → 更新检查的后台 `gh`/HTTP 拉取立刻失败，全程离线；
真插件端口只被**探活**（`ccLiveSnapshot`），不启停任何服务；`--fax` 与 GUI 分支一概不进。

## 结构

| 字段 | 含义 |
|---|---|
| `app_version` / `core_version` | Go 侧版本常量（Rust 侧 `APP_VERSION`/`CORE_VERSION` 必须同值） |
| `bindings26` | 简报点名的 26 个绑定名；采集器断言它 = 假 WebView 实收名集合 − 4 个 `ccPlugin*` |
| `bindings_all` | `bindAll` 实际注册的全部名字（30 个 = 26 + 4 插件绑定） |
| `normalization` | 归一化规则（两侧同一套哨兵） |
| `comparison_exemptions` | 差分里**明示**的豁免项（见下） |
| `scenarios.<id>.steps[]` | 命令序列：`cmd` / `args` / `out`（归一化后的返回串） |
| `scenarios.<id>.files{}` | 场景结束时的落盘文件（相对路径 → 内容；`$APPDATA/...` 前缀表示落在 APPDATA 下） |
| `scenarios.<id>.note` | 该场景的门禁口径说明 |

## 归一化哨兵（两侧同规则）

| 规则 | 哨兵 |
|---|---|
| exe 路径 | `$EXE` |
| `PROXYDECK_DATA_DIR` 临时目录 | `$DATA` |
| `APPDATA` 临时目录 | `$APPDATA` |
| `127.0.0.1:<测试端口>` | `127.0.0.1:$PORT` |
| `"port":"<n>"` | `"port":"$PORT"` |
| `-port <n>`（自启 VBS 命令行） | `-port $PORT` |
| `"pid":<n>` | `"pid":0` |
| `"uptime":<n>` | `"uptime":0` |

## 豁免项（**明示**，不是静默放过）

1. `ccKeyMapSet` 的 JSON 解析错误文案——Go `encoding/json` 与 serde_json 的报错文本不可能逐字相同，
   该步的 `msg` 归一为 `$JSONERR`（其余字段照比）。
2. `appdir.log`——带时间戳，且内容取决于 `icacls` 是否在 PATH 上（采集器为离线清空了 PATH），不作黄金。
3. `PDAPI1` 密文文件的字节——DPAPI 每次产出不同；改比「格式是否为密文」+「能否解回同一明文」。
4. `ccLiveSnapshot` 的取值——随本机插件端口的实时状态变化，只比 `ok` 与键集。

## `pdapi1-go-apikey-sample.txt`

Go 宿主（`appdir.WriteFile` + `ccSaveKey("TESTKEY-ALIGN")`）真实写出的 `api-key.txt` 密文原件，
**但不是现行黄金 `S1_fresh` 里那一份**：DPAPI 每次调用换随机会话密钥，同一明文的两次产出必然不同
（已核对：同为 316 字节、第 89 字节起分叉）。该样本来自另一次采集运行， decrypt 出的明文同样是
`TESTKEY-ALIGN`（门禁断言即此）。`src/dpapi.rs` 的单测用它证明
**Go 写的密文，Rust 能解开**（跨实现兼容的实证，而不是各写各的往返测试）。
