# -*- coding: utf-8 -*-
"""Push local HEAD to GitHub via the GitHub Database API (driven by gh CLI).

Use when github.com:443 is blocked (TUN off) but api.github.com works:
    python push_api.py            # update main
    python push_api.py --tag vX   # also create/update tag ref
"""
import base64
import json
import subprocess
import sys
import tempfile

REPO = "Amer-CN/command-code-proxy-tools"
HEAD = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
MSG = subprocess.check_output(["git", "log", "-1", "--pretty=%B", HEAD], text=True)


def api(method, path, payload=None):
    import time
    last_err = None
    for attempt in range(4):
        cmd = ["gh", "api", "-X", method, f"repos/{REPO}{path}"]
        tmp = None
        if payload is not None:
            tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False, encoding="utf-8")
            json.dump(payload, tmp)
            tmp.close()
            cmd += ["--input", tmp.name]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if tmp:
            import os
            os.unlink(tmp.name)
        if r.returncode == 0:
            out = r.stdout.strip()
            return json.loads(out) if out else {}
        last_err = r.stderr[:200]
        print(f"  retry {attempt+1} after: {last_err}")
        time.sleep(3 * (attempt + 1))
    print(f"gh api {method} {path} failed: {last_err}")
    sys.exit(1)


def git(*args):
    return subprocess.check_output(["git"] + list(args), text=True)


remote = api("GET", "/git/refs/heads/main")["object"]["sha"]
print(f"remote main: {remote[:12]}  local HEAD: {HEAD[:12]}")

# 1. Upload every blob (small repo, simplest & robust; binary-safe)
ls = git("ls-tree", "-r", HEAD)
entries = []
for line in ls.splitlines():
    meta, path = line.split("\t")
    mode, typ, sha = meta.split()
    raw = subprocess.check_output(["git", "cat-file", "blob", sha])  # bytes (binary-safe)
    data = base64.b64encode(raw).decode()
    r = api("POST", "/git/blobs", {"content": data, "encoding": "base64"})
    entries.append({"path": path, "mode": mode, "type": "blob", "sha": r["sha"]})
print(f"uploaded {len(entries)} blobs")

# 2. Full tree
tree = api("POST", "/git/trees", {"tree": entries})["sha"]
print("tree ->", tree[:12])

# 3. Commit with remote main as parent
commit = api("POST", "/git/commits", {
    "message": MSG.strip(), "tree": tree, "parents": [remote],
})["sha"]
print("commit ->", commit[:12])

# 4. Update main
api("PATCH", "/git/refs/heads/main", {"sha": commit, "force": True})
print("main updated ->", commit[:12])

# 5. Optional tag
if "--tag" in sys.argv:
    tag = sys.argv[sys.argv.index("--tag") + 1]
    try:
        r = api("POST", "/git/refs", {"ref": f"refs/tags/{tag}", "sha": commit})
        print("tag", tag, "->", r["ref"])
    except SystemExit:
        api("PATCH", f"/git/refs/tags/{tag}", {"sha": commit, "force": True})
        print("tag", tag, "force-updated")
print("DONE")

