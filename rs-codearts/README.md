# rs-codearts —— 华为云码道（CodeArts Doer）插件

把**码道桌面端**的模型服务复刻成本地 OpenAI 兼容接口，接进 ProxyDeck：端口 **8793**，
插件 id `codearts`，启动参数 `--plugin-codearts`。

> 逆向接入，仅供学习研究。额度是账号级共享：插件跑量会消耗你码道账号当天的积分。
> 本项目不做任何风控规避（TLS 指纹、设备指纹一律不改），只求与官方客户端同源。

## 端点

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 本地应答；`ready` / `account` / `expiresAt` / `models` |
| GET | `/v1/models` | 上游 `/v1/model/builtin`，10 分钟缓存 |
| POST | `/v1/chat/completions` | 流式透传；`stream:false` 本地聚合 |
| GET | `/v1/quota` | 上游 `/snap-manager/v1/statistics/plugin`（积分） |

客户端配置：`base_url = http://127.0.0.1:8793/v1`，`api_key` 随便填（本机回环无鉴权）。

## 凭据从哪来

**优先级 1：本机码道登录态（推荐，零配置）**

```text
%APPDATA%\codearts\Local State                       ← DPAPI 保护的主密钥
%APPDATA%\codearts\secretStorage\<sha256>.enc        ← base64(v10 + AES-256-GCM)
```

`import.rs` 只读解密，**不写回、不续期**。桌面端每小时自己刷新一次，插件按文件读取跟随；
过期时间为 `session.expiresAt`（毫秒）。

**优先级 2：账号文件** `%APPDATA%\ProxyDeck\codearts-accounts.json`

```jsonc
{ "channel": "credit",
  "accounts": [
    { "name": "hid_xxx",
      "accessKey": "HSTA...", "secretKey": "...", "securitytoken": "...",
      "expires_at": "2026-10-27T00:00:00Z" }
  ] }
```

也接受直接粘贴码道 `secretStorage` 解出来的 session JSON（含 `session` / `loginContext`）。
`--no-local` 可关闭本机自动导入（只用文件）。

## 实现要点（改代码前必读）

1. **签名是华为云 SDK-HMAC-SHA256，且有两处与文档不同**（`signer.rs`，金标准单测锁定）：
   - CanonicalURI 每段严格 percent-encode 后**强制补尾斜杠**；
   - CanonicalHeaders 每行自带 `\n`，拼 CanonicalRequest 时再补一个 `\n`（`SignedHeaders`
     前多一个空行）；
   - 只保留 `A-Za-z0-9-._~`，其余 `%XX` 大写；GET 按空 body 签名。
2. **上游一律 `stream:true`**：`glm-5.3` 只支持流式（`stream:false` 回 HTTP 400 空 body）。
   客户端要非流式时本地聚合（`sse.rs`）。
3. **业务错误可能以 HTTP 200 返回**：`{"error_code":"InferHub.xxx.404"}`。首事件预检在
   发响应头前转成真错误；状态码取 `error_code` 末段，取不到兜底 400。
4. **`model` / `model-id` / `model-name` 必须逐字相等**（大小写敏感），客户端尾随空格要 trim。
5. **`x-ot-session-id` 按 AK 派生并复用**：上游每账号并发上限 3，每请求随机会瞬间打满
   （`TM.00001041` → 429）。
6. 凭据类错误（`APIG.0301/0303`、`STS5.*`、`invalid_grant`）统一 502 并提示重新登录，
   不当作普通 400 让客户端无限重试。
7. `maas_type` 只有在账号文件写 `"channel":"benefit"` 时才发。2026-10 实测福利通道已整体
   下线（`4000 invalid channel`），默认 `credit`。

## 实测记录（2026-10-26，本机）

- 本机码道版本：Electron 版，`product.json` 的 `auth.clientId = codearts`（不是旧 VS Code
  版的 `codearts-agent`），`inferhubBaseUrl = https://snap-access.cn-north-4.myhuaweicloud.com/api/v2/`。
- `GET /v1/model/builtin` → 9 个模型：`GLM-5.2`、`Qwen3-VL-235B`、`kimi-k2.6-vl`、
  `openpangu-2.0-flash`、`openpangu-2.0-pro`、`glm-5.3`、`glm-5.2-sft-harmony`、
  `deepseek-v4.1-flash`、`kimi-k3`。
- `GET /snap-manager/v1/statistics/plugin` → `usageTotalPackageCredit` 7500 / 已用 120.11。
- 真实对话：流式 `chat.completion.chunk`、非流式聚合均 200，`reasoning_content` 正常。

## 验证命令

```powershell
# 单测（签名向量 / SSE 聚合 / 账号解析）
cd rs-codearts
$env:CARGO_TARGET_DIR='E:\cargo-target\rs-codearts'
cargo test

# 起测试端口（不要碰生产端口；生产由 GUI 点火）
E:\cargo-target\rs-codearts\debug\rs-codearts.exe --plugin-codearts --port 18793
curl http://127.0.0.1:18793/health
```

## 未做（后续）

- DPoP 自动续期（`refresh_token` 单次轮转，插件续期会让桌面端掉登录；本机导入模式下没必要）。
- 福利领取/签到（上游接口已下线）。
- 多账号轮转（账号文件支持多条，但 `resolve_account` 目前只取第一条可用）。
