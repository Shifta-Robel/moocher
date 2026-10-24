#!/usr/bin/env python3
# Offline `cargo audit`: tests/audit.py ADVISORY_DB_DIR [Cargo.lock]
# ADVISORY_DB_DIR: an unpacked https://github.com/rustsec/advisory-db (crates/<name>/RUSTSEC-*.md).
import re, sys, tomllib, pathlib
db, lock = pathlib.Path(sys.argv[1]), (sys.argv[2] if len(sys.argv) > 2 else "Cargo.lock")
pkgs = {}
for p in tomllib.load(open(lock, "rb"))["package"]:
    pkgs.setdefault(p["name"], set()).add(p["version"])
def ver(s):
    s = s.split("+")[0]; main, _, pre = s.partition("-")
    parts = [int(x) for x in main.split(".")] + [0, 0, 0]
    return (parts[0], parts[1], parts[2], 0 if pre else 1, pre)
def ok(v, req):  # req like ">= 1.2.3, < 1.3" ; all must hold
    for c in req.split(","):
        m = re.match(r"\s*(>=|<=|>|<|=|\^)?\s*(\S+)", c)
        op, r = m.group(1) or "^", m.group(2)
        a, b = ver(v), ver(r)
        if op == "^":
            parts = r.split(".")
            if not (a >= b): return False
            if int(parts[0]) > 0: up = (int(parts[0]) + 1, 0, 0, 0, "")
            elif len(parts) > 1 and int(parts[1]) > 0: up = (0, int(parts[1]) + 1, 0, 0, "")
            else: up = (0, 0, int(parts[2] if len(parts) > 2 else 0) + 1, 0, "")
            if not a < up: return False
        elif not {">=": a >= b, "<=": a <= b, ">": a > b, "<": a < b, "=": a == b}[op]: return False
    return True
found = []
for f in db.glob("crates/*/RUSTSEC-*.md"):
    t = f.read_text()
    m = re.search(r"```toml\n(.*?)```", t, re.S)
    if not m: continue
    a = tomllib.loads(m.group(1))
    adv, vs = a["advisory"], a.get("versions", {})
    name = adv["package"]
    if name not in pkgs or adv.get("withdrawn"): continue
    for v in pkgs[name]:
        if any(ok(v, r) for r in vs.get("patched", []) + vs.get("unaffected", [])): continue
        title = re.search(r"^# (.*)$", t, re.M)
        found.append((adv["id"], name, v, adv.get("informational", "vulnerability"), title.group(1) if title else "", vs.get("patched", [])))
for x in sorted(found): print(*x, sep=" | ")
print(f"{len(found)} findings over {sum(len(v) for v in pkgs.values())} crate versions")
