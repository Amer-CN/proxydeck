"""
codely2api - 把团结 Cowork (Codely) 的登录态转成 OpenAI 兼容 API。

流程:
1. 读取 ~/.codely-cli/oauth_creds.json 获取 access_token
2. 用 access_token 调 https://codely.tuanjie.cn/api/api-token/cli-api-key 换取 cli_api_key
3. 用 cli_api_key 作为 Bearer token + x-litellm-session-id 转发请求到 LiteLLM

暴露接口:
- GET  /v1/models
- POST /v1/chat/completions
- GET  /health
"""

import base64
import hashlib
import hmac
import json
import os
import time
import uuid
import httpx
from fastapi import FastAPI, Request, Response
from fastapi.responses import StreamingResponse, JSONResponse
import uvicorn

# ============ Config ============
OAUTH_CREDS_PATH = os.path.expanduser("~/.codely-cli/oauth_creds.json")
CODELY_API_BASE = "https://codely.tuanjie.cn"
LITELLM_API_BASE = "https://codely-litellm.tuanjie.cn"
CLI_API_KEY_FETCH_URL = f"{CODELY_API_BASE}/api/api-token/cli-api-key"
LISTEN_HOST = "127.0.0.1"
LISTEN_PORT = 8788

# 官方 CLI 内置的签名种子（逆向自 1.0.0-release.52），与 cli_api_key 两层 HMAC 派生签名密钥
CODELY_SIGNING_SEED = bytes.fromhex("406f00f74768ba0cb0cd30f097ec6c2bdacb89c61a38b7dd140838bbd0e98018")
CLI_USER_AGENT = "codely-cli/1.0.0-rc.58 (win32; x64)"

# ============ State ============
app = FastAPI(title="codely2api")
_cached_cli_api_key = None
_cached_key_time = 0
_KEY_CACHE_TTL = 3600  # 1 hour


def load_oauth_creds():
    with open(OAUTH_CREDS_PATH, "r", encoding="utf-8") as f:
        return json.load(f)


def fetch_cli_api_key(access_token: str) -> str:
    """Use access_token to fetch a fresh cli_api_key from Codely backend."""
    creds = load_oauth_creds()
    org_id = creds.get("org_id", "")
    # Read org from org.json
    org_path = os.path.expanduser("~/.codely-cli/org.json")
    if os.path.exists(org_path):
        with open(org_path, "r", encoding="utf-8") as f:
            org_data = json.load(f)
        accounts = org_data.get("accounts", {})
        user_id = str(creds.get("user_id", ""))
        if user_id in accounts:
            org_id = accounts[user_id].get("currentOrgId", "")
    
    params = {}
    if org_id:
        params["teamId"] = org_id
    
    resp = httpx.get(
        CLI_API_KEY_FETCH_URL,
        params=params,
        headers={"Authorization": f"Bearer {access_token}", "Accept": "application/json"},
        timeout=15,
    )
    resp.raise_for_status()
    data = resp.json()
    key = data.get("cli_api_key")
    if not key:
        raise RuntimeError(f"No cli_api_key in response: {data}")
    return key


def get_cli_api_key() -> str:
    """Get a valid cli_api_key, with caching."""
    global _cached_cli_api_key, _cached_key_time
    now = time.time()
    if _cached_cli_api_key and (now - _cached_key_time) < _KEY_CACHE_TTL:
        return _cached_cli_api_key
    
    creds = load_oauth_creds()
    access_token = creds.get("access_token")
    if not access_token:
        raise RuntimeError("No access_token in oauth_creds.json")
    
    key = fetch_cli_api_key(access_token)
    _cached_cli_api_key = key
    _cached_key_time = now
    return key


def make_codely_signature(path: str, cli_api_key: str, timestamp_secs: int) -> str:
    """生成 X-Codely-Signature 头值：v1.<秒级时间戳>.<base64url 签名>。

    消息体为 "v1\\n<path>\\n<timestamp>"（只签 path），与官方 CLI 1.0.0-release.52 一致。
    """
    k1 = hmac.new(CODELY_SIGNING_SEED, b"codely-signing-v1", hashlib.sha256).digest()
    signing_key = hmac.new(k1, cli_api_key.encode(), hashlib.sha256).digest()
    msg = f"v1\n{path}\n{timestamp_secs}".encode()
    sig = hmac.new(signing_key, msg, hashlib.sha256).digest()
    return f"v1.{timestamp_secs}.{base64.urlsafe_b64encode(sig).rstrip(b'=').decode()}"


def build_litellm_headers(cli_api_key: str, path: str, extra: dict = None) -> dict:
    """Build headers that mimic codely CLI to satisfy LiteLLM proxy. path 参与签名。"""
    headers = {
        "Authorization": f"Bearer {cli_api_key}",
        "Content-Type": "application/json",
        "Accept": "application/json",
        "User-Agent": CLI_USER_AGENT,
        "x-litellm-session-id": str(uuid.uuid4()),
        "X-Codely-Signature": make_codely_signature(path, cli_api_key, int(time.time())),
    }
    if extra:
        headers.update(extra)
    return headers


# ============ Routes ============

@app.get("/health")
async def health():
    try:
        creds = load_oauth_creds()
        key = get_cli_api_key()
        return {
            "status": "ok",
            "service": "codely2api",
            "user_id": creds.get("user_id"),
            "cli_api_key": key[:10] + "...",
            "backend": LITELLM_API_BASE,
        }
    except Exception as e:
        return {"status": "error", "message": str(e)}


@app.get("/v1/models")
async def list_models():
    key = get_cli_api_key()
    headers = build_litellm_headers(key, "/v1/models")
    async with httpx.AsyncClient() as client:
        resp = await client.get(
            f"{LITELLM_API_BASE}/v1/models",
            headers=headers,
            timeout=30,
        )
    return JSONResponse(content=resp.json(), status_code=resp.status_code)


@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    body = await request.json()
    key = get_cli_api_key()
    headers = build_litellm_headers(key, "/v1/chat/completions")
    
    is_stream = body.get("stream", False)
    
    if is_stream:
        async def stream_generator():
            async with httpx.AsyncClient() as client:
                async with client.stream(
                    "POST",
                    f"{LITELLM_API_BASE}/v1/chat/completions",
                    json=body,
                    headers=headers,
                    timeout=300,
                ) as resp:
                    async for chunk in resp.aiter_bytes():
                        yield chunk
        return StreamingResponse(stream_generator(), media_type="text/event-stream")
    else:
        async with httpx.AsyncClient() as client:
            resp = await client.post(
                f"{LITELLM_API_BASE}/v1/chat/completions",
                json=body,
                headers=headers,
                timeout=300,
            )
        return JSONResponse(content=resp.json(), status_code=resp.status_code)


if __name__ == "__main__":
    # 支持 --host/--port 覆盖默认监听（CommandCodeProxyDeck 托管时指定端口）
    import argparse
    _ap = argparse.ArgumentParser(description="codely2api")
    _ap.add_argument("--host", default=LISTEN_HOST)
    _ap.add_argument("--port", type=int, default=LISTEN_PORT)
    _args = _ap.parse_args()
    LISTEN_HOST, LISTEN_PORT = _args.host, _args.port
    print(f"codely2api starting on http://{LISTEN_HOST}:{LISTEN_PORT}")
    print(f"Backend: {LITELLM_API_BASE}")
    print(f"OAuth creds: {OAUTH_CREDS_PATH}")
    uvicorn.run(app, host=LISTEN_HOST, port=LISTEN_PORT)
