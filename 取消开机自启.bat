@echo off
set "VBS=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\command-code-proxy-autostart.vbs"
del "%VBS%" >nul 2>&1
if exist "%VBS%" (
  echo [FAIL] Could not remove autostart file.
) else (
  echo [OK] Autostart removed.
)
ping -n 3 127.0.0.1 >nul
