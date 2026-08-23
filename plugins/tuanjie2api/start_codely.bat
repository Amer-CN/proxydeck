@echo off
:: codely2api 启动脚本 - 把团结Cowork的模型变成本地API
:: 端口 8788，模型: codely-core, codely-flash, GLM-5.2 等
set PYTHONHOME=
set PYTHONPATH=

cd /d "E:\codebuddy2api"
call .venv\Scripts\activate.bat

python codely2api.py

pause
