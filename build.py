#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""CommandCode Proxy Deck 构建脚本（单模式，直接编译本仓库）。

用法:
    python build.py             构建到 ProxyDeck.exe
    python build.py public      同上（保留参数兼容旧调用，行为一致）

仓库自包含：所有 Go 源码与 UI 均在 app/、internal/、cmd/ 下，无注入。
"""
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))


def fail(msg):
    print("[构建失败]", msg, file=sys.stderr)
    sys.exit(1)


def build():
    # go build（WebView2 CGO 需要 MinGW）。
    # 先构建到临时名，再原子替换——正在运行的 exe 会被 Windows 锁定，
    # 直接 -o 目标名会静默失败（go 把 a.out.exe 拷贝到目标时报错）。
    env = dict(os.environ)
    ldflags = "-H windowsgui -s -w"
    final = os.path.join(ROOT, "ProxyDeck.exe")
    staging = os.path.join(ROOT, final + ".new")
    cmd = ["go", "build", "-trimpath", "-ldflags=" + ldflags, "-o", staging, "./app"]
    print("[构建]", " ".join(cmd), "(cwd=%s)" % ROOT)
    r = subprocess.run(cmd, cwd=ROOT, env=env)
    if r.returncode != 0:
        fail("go build 失败（exit %d）" % r.returncode)
    # 原子替换（目标被锁时 os.replace 失败并明确报错，不再静默）
    try:
        os.replace(staging, final)
    except PermissionError as e:
        os.remove(staging)
        fail("无法替换 %s（文件被运行中的进程锁定，请先关闭程序）: %s" % (final, e))

    size = os.path.getsize(final)
    print("[完成] → %s (%.1f MB)" % (final, size / 1048576))


if __name__ == "__main__":
    # 兼容旧调用：`python build.py full` / `python build.py public` 现在都是同一行为
    build()
