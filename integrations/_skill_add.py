import json
import sys

path, key, want = sys.argv[1:4]
d = json.load(open(path, encoding="utf-8"))
v = d.get(key)
if isinstance(v, dict):
    v = v.get("paths") or v.get("customDirectories") or []
if not isinstance(v, list):
    v = []
if want not in v:
    v.append(want)
d[key] = v
with open(path, "w", encoding="utf-8") as f:
    json.dump(d, f, ensure_ascii=False, indent=2)
    f.write("\n")
