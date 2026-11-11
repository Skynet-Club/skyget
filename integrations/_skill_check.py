import json
import sys

path, key, want = sys.argv[1:4]
d = json.load(open(path, encoding="utf-8"))
v = d.get(key)
if isinstance(v, dict):
    v = v.get("paths") or v.get("customDirectories") or []
if not isinstance(v, list):
    v = []
sys.exit(0 if want in v else 1)
