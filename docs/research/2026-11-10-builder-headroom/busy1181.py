"""GATE-1181: for each busy fallback on 11-09/11-10, was another routed run of the same work tree on the builder at that time?"""

import importlib.util
import os
import sys
from datetime import timedelta

spec = importlib.util.spec_from_file_location("h", os.path.join(os.path.dirname(__file__), "headroom1181.py"))
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)


def main():
    evs = h.load()
    routed, busy = [], []
    for e in evs:
        t = h.ts(e["time"])
        e["_end"] = t
        if e.get("remote"):
            e["_start"] = t - timedelta(seconds=e.get("heldSeconds") or 0)
            routed.append(e)
        fb = e.get("fellBack") or []
        if "busy" in fb and t.date().isoformat() in ("2026-11-09", "2026-11-10"):
            e["_start"] = h.ts(e["processStart"])
            busy.append(e)
    same, other = 0, 0
    for f in sorted(busy, key=lambda x: x["_start"]):
        cw = (f.get("cwd") or "").lower()
        hits = [r for r in routed if (r.get("cwd") or "").lower() == cw and r["_start"] < f["_end"] and r["_end"] > f["_start"] - timedelta(seconds=120)]
        tag = "SAME-TREE" if hits else "no-same-tree"
        if hits:
            same += 1
        else:
            other += 1
        print(f"{f['_start']:%m-%d %H:%M:%S} {h.repo_of(f.get('cwd'))} [{h.family(f.get('sentWords') or f.get('command'))}] wall {f.get('wallSeconds') or 0:.0f} s -> {tag} {len(hits)} {[h.family(r.get('sentWords')) for r in hits][:3]}")
    print(f"busy with a same-tree routed run on a builder within the window: {same}; without: {other}")


if __name__ == "__main__":
    sys.exit(main())
