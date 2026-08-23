@echo off
chcp 65001 >nul
cd /d "%~dp0"
set "EXE=ProxyDeck.exe"

rem --- 已在运行？ ---
curl -s -f -o nul http://127.0.0.1:55990/health >nul 2>&1
if %errorlevel%==0 (
  echo [信息] 代理已在运行: http://127.0.0.1:55990
  ping -n 3 127.0.0.1 >nul
  exit /b 0
)

if not exist "%EXE%" (
  echo [错误] 未找到 %EXE% —— 请从发布包获取程序。
  ping -n 5 127.0.0.1 >nul
  exit /b 1
)

start "" "%CD%\%EXE%" -headless -port 55990

echo 正在点火... 等待服务就绪（最多约 15 秒）
set /a tries=0
:waitloop
ping -n 2 127.0.0.1 >nul
curl -s -f -o nul http://127.0.0.1:55990/health >nul 2>&1
if %errorlevel%==0 goto up
set /a tries+=1
if %tries% lss 14 goto waitloop

echo [失败] 服务无响应 —— 端口 55990 可能被其他程序占用。
ping -n 4 127.0.0.1 >nul
exit /b 1

:up
echo [OK] 已以后台模式启动。Agent 里填 Base URL: http://127.0.0.1:55990/v1
echo      （需要界面时，直接双击 %EXE% 即可，控制台会自动接入监测）
ping -n 4 127.0.0.1 >nul
exit /b 0
