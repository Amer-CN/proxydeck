# P8a 黄金基准（入库副本）

P8a 门禁（`../golden_p8a.rs`）优先读本目录，缺则回落 `.work/p8a/`（与 rs-bai 同规：**干净检出
也能跑**，不依赖 gitignore 的 `.work/`）。

| 文件 | 内容 | 来源 |
|---|---|---|
| `golden-p8a-go.json` | 签名向量 253 条（含 `signing_key_hex`）/ 重排用例 21 条 / UA 常量与版本正则行为表 / 头集合 / 端点与超时 | Go 侧一次性采集器现采 |
| `go-outbound-wire.txt` | Go `net/http` **线上实抓**的原始请求字节（头序、大小写、自动补头） | 同一采集器现采 |
| `tls/go-clienthello-sni.bin` | Go utls `HelloChrome_Auto` + ALPN 改写后的真实 ClientHello（1796B） | `utlsHandshake(ctx, raw, "codely-litellm.tuanjie.cn:443")` |
| `tls/rustls-clienthello-sni.bin` | 候选 (a) rustls 默认栈的 ClientHello（257B） | `.work/p8a/rustls-probe` |
| `tls/wreq-clienthello-chrome.bin` | 候选 (b) `wreq` + Chrome 模拟（BoringSSL）的 ClientHello（1762B） | `.work/p8a/wreq-probe` |
| `tls/parse-clienthello.mjs` | 两侧共用解析器（扩展序/密套件/key_share/ALPN/JA3/JA4） | — |

解析任意一份：`node tests/golden/tls/parse-clienthello.mjs tests/golden/tls/*.bin`

## 再生成链（P8a）

1. **签名 / 重排 / UA / 头集合 / 线上字节**：在 `internal/tuanjie/` 下临时落一个同包 `_test.go`
   采集器（口径见简报「P8a 本轮实际要动/新建的文件清单」），跑

   ```powershell
   $env:P8A_GOLDEN_OUT="<repo>\.work\p8a\golden-p8a-go.json"
   $env:P8A_WIRE_OUT="<repo>\.work\p8a\go-outbound-wire.txt"
   go test ./internal/tuanjie -run TestZZCollectP8aGolden -count=1 -v
   ```

   **跑完立即删除该临时文件**——删除后 `git status --short internal/` 必须为空、
   `go test ./internal/tuanjie -count=1` 与 `go vet ./internal/tuanjie` 保持原状
   （Go 源码在 P8 全程只读）。采集器**不读真实凭据**：会话/密钥全是合成值。

2. 把 `.work/p8a/golden-p8a-go.json` 与 `go-outbound-wire.txt` 复制回本目录（两份逐字节相同）。

3. **ClientHello 证据**（只在需要复核 uTLS 路线时重采）：Go 侧见 `internal/tuanjie` 的
   `TestZZCollectHelloSNI` 形态采集器（临时文件，跑完即删）；Rust 侧两个探针在
   `.work/p8a/{rustls-probe,wreq-probe}`，`wreq` 探针需要 `cmake` + `NASM` + `libclang`。

## 基线采集时的机器事实（写下来免得后人误判）

- 采集时本机 npm 全局 `@unity-china/codely-cli` 版本 = `1.0.0-rc.60`（于是 UA 解析值 =
  `codely-cli/1.0.0-rc.60 (win32; x64)`）。**UA 版本号随本机安装走**，换机后这条会变——门禁用
  「模板 + 兜底常量 + 版本正则行为表」锁死，不锁死具体版本号。
- `metadata.cwd` 在基准里是**采集机 Go 测试二进制的目录**（`E:\Temp\go-build…`）。Rust 侧由
  `reshape_chat_body_with_cwd(.., cwd)` 注入同一值做逐字节比对；生产路径取 exe 目录。
