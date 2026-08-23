@echo off
chcp 65001 >nul
taskkill /F /IM ProxyDeck.exe >nul 2>&1
taskkill /F /IM command-code-proxy.exe >nul 2>&1
echo [OK] 已发送停止命令（控制台与后台实例都会被结束）。
ping -n 3 127.0.0.1 >nul
exit /b 0
