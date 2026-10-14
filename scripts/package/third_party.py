"""Lists the third-party crates compiled into the given workspace packages, with their licenses.

    cargo metadata --format-version 1 --locked | python3 scripts/package/third_party.py <version> <package>...

Used by scripts/package.sh (scripts/package.ps1 does the same in PowerShell). Follows normal and
build dependencies, not dev dependencies.
"""
import json
import sys

version, wanted = sys.argv[1], set(sys.argv[2:])
# Windows PowerShell may put a byte order mark in front of what it pipes on.
meta = json.loads(sys.stdin.buffer.read().decode("utf-8-sig"))
packages = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
members = set(meta["workspace_members"])
todo = [i for i in members if packages[i]["name"] in wanted]
seen = set(todo)
while todo:
    for dep in nodes[todo.pop()]["deps"]:
        kinds = [k["kind"] for k in dep["dep_kinds"]]
        if kinds and all(k == "dev" for k in kinds):
            continue
        if dep["pkg"] not in seen:
            seen.add(dep["pkg"])
            todo.append(dep["pkg"])

print(f"Third-party software in bf2-rust-engine {version}")
print()
print("These Rust crates are compiled into the programs in this package. Each is used under the")
print("license named here; the full texts are in each crate's source at its repository or on")
print("https://crates.io.")
print()
for p in sorted((packages[i] for i in seen - members), key=lambda p: (p["name"], p["version"])):
    license = p.get("license") or ("see " + p["license_file"] if p.get("license_file") else "unknown")
    repo = "  " + p["repository"] if p.get("repository") else ""
    print(f"{p['name']} {p['version']}: {license}{repo}")
