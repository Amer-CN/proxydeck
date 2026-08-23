@echo off
chcp 65001 >nul
setlocal
set "STARTUP=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup"
set "VBS=%STARTUP%\command-code-proxy-autostart.vbs"
set "EXE=%~dp0ProxyDeck.exe"

if not exist "%EXE%" (
  echo [错误] 未找到 %EXE% —— 请从发布包获取程序。
  ping -n 5 127.0.0.1 >nul
  exit /b 1
)
if not exist "%STARTUP%" (
  echo [错误] 未找到启动文件夹: %STARTUP%
  ping -n 4 127.0.0.1 >nul
  exit /b 1
)

(
echo Set sh = CreateObject("WScript.Shell"^)
echo sh.Run """%EXE%"" -headless -port 55990", 0, False
) > "%VBS%"

if exist "%VBS%" (
  echo [OK] 开机自启已设置：登录 Windows 后自动以无窗口模式点火。
  echo      取消方法：运行「取消开机自启.bat」，或在控制台里点「开机自启」开关。
) else (
  echo [失败] 无法写入自启脚本（被杀软拦截？）。
)
ping -n 4 127.0.0.1 >nul
