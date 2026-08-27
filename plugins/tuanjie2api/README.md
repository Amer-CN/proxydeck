# codely2api

把 **团结 Cowork (Codely CLI)** 的登录态，转成本地 **OpenAI 兼容 API**。

## 原理

```
团结Cowork桌面端（已登录）
    ↓ oauth_creds.json (access_token)
codely2api (127.0.0.1:8788)
    ↓ 1. 用 access_token 调 codely.tuanjie.cn/api/api-token/cli-api-key 换取 cli_api_key
    ↓ 2. 用 cli_api_key 作为 Bearer + x-litellm-session-id 转发请求
团结 LiteLLM 后端 (codely-litellm.tuanjie.cn)
    ↓ 返回 OpenAI 兼容格式
你的 AI 工具 (ADM Agent / Cherry Studio / 任何 OpenAI 兼容客户端)
```

## 前置条件

1. 本机已安装并登录 **团结 Cowork 桌面端**（会自动生成 `~/.codely-cli/oauth_creds.json`）
2. Python 3.8+
3. 依赖：`fastapi`、`uvicorn`、`httpx`

## 安装

```bash
git clone <本仓库> codely2api
cd codely2api

python -m venv .venv

# Windows
.venv\Scripts\pip install fastapi "uvicorn[standard]" httpx

# macOS / Linux
.venv/bin/pip install fastapi "uvicorn[standard]" httpx
```

## 启动

```bash
# Windows
python codely2api.py

# 或用启动脚本
start_codely.bat
```

看到监听 `http://127.0.0.1:8788` 就说明起来了。

## 验证

```bash
curl http://127.0.0.1:8788/health
curl http://127.0.0.1:8788/v1/models
```

## 可用模型

| 模型名 | 后端实际模型 | 上下文长度 |
|--------|-------------|-----------|
| `codely-core` | glm-5-fp8-128k | 1,048,576 |
| `codely-flash` | - | - |
| `codely-air` | - | 1,024,000 |
| `codely-vl` | - | 262,144 (视觉) |
| `GLM-5.2` | glm-5-fp8-128k | 1,048,576 |

> 注意：后端实际模型由团结 LiteLLM 路由决定，可能与名称不一致。

## 接入方式

### 任何 OpenAI 兼容客户端

| 项目 | 填什么 |
|------|--------|
| API 地址 (Base URL) | `http://127.0.0.1:8788/v1` |
| API Key | 随便填，如 `sk-1234` |
| 模型名 | `codely-core` 或 `GLM-5.2` |

### curl 示例

```bash
curl http://127.0.0.1:8788/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"codely-core","messages":[{"role":"user","content":"你好"}]}'
```

## 认证流程详解

### 第一步：读取登录态

团结 Cowork 桌面端登录后，会在 `~/.codely-cli/oauth_creds.json` 保存凭据：

```json
{
  "access_token": "eyJhbGci...",
  "cli_api_key": "sk-xxxxx",
  "user_id": 12345678
}
```

### 第二步：刷新 cli_api_key

用 `access_token` 调用团结后端 API 获取有效的 `cli_api_key`：

```
GET https://codely.tuanjie.cn/api/api-token/cli-api-key?teamId={org_id}
Authorization: Bearer {access_token}
```

返回：
```json
{"cli_api_key": "sk-xxxxxxxxxxxxxxxxxxxxxxxx（示例已脱敏）", "user_id": 12345678}
```

### 第三步：转发请求到 LiteLLM

用获取的 `cli_api_key` 作为 Bearer token，加上必要的 header，转发到 LiteLLM 后端：

```
POST https://codely-litellm.tuanjie.cn/v1/chat/completions
Authorization: Bearer sk-xxxxxxxxxxxxxxxxxxxxxxxx（示例已脱敏）
Content-Type: application/json
User-Agent: codely-cli/1.0.0-release.43 (win32; x64)
x-litellm-session-id: {random-uuid}
```

### 关键发现过程

1. **直接用 access_token 调 LiteLLM** → 401 Unauthorized
2. **直接用 cli_api_key 调 LiteLLM** → 401 Unauthorized
3. **逆向 codely.exe 二进制** → 发现 `ensureCliApiKey()` 方法和 `CODELY_OAUTH_DYNAMIC_TOKEN` 环境变量
4. **找到 cli-api-key 端点** → `https://codely.tuanjie.cn/api/api-token/cli-api-key`
5. **用 access_token 换取新的 cli_api_key 成功** → `sk-xxxxxxxxxxxxxxxxxxxxxxxx（示例已脱敏）`
6. **用新 cli_api_key 调 LiteLLM** → 400 "欢迎使用Codely"
7. **用 mitmproxy 抓包 codely.exe 的实际请求** → 发现需要 `x-litellm-session-id` header 和特定 `User-Agent`
8. **补上 header 后成功** → 200 OK，模型正常回复

## 文件说明

| 文件 | 作用 |
|------|------|
| `codely2api.py` | 主服务，FastAPI 应用 |
| `start_codely.bat` | Windows 启动脚本 |
| `requirements.txt` | Python 依赖 |

## 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `CODELY_AUTH_DIR` | oauth_creds.json 所在目录 | `~/.codely-cli` |

## 免责声明

本项目仅用于个人学习与研究。与团结引擎、Codely、腾讯无官方关联。请仅在你合法拥有订阅的前提下使用，并自行承担风险。

## License

MIT
