"""GATE-1181: how many Windows-builder remote slots desktop-locked runs held, and CPU figures without them."""

import bisect
import importlib.util
import os
import statistics
import sys
from collections import defaultdict
from datetime import datetime, timedelta, timezone
from fnmatch import fnmatchcase

spec = importlib.util.spec_from_file_location("h", os.path.join(os.path.dirname(__file__), "headroom1181.py"))
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)

LOCKED = {
    "godot-mcp": ["pwsh -NoProfile -File run.ps1 itest", "pwsh -NoProfile -File run.ps1 itest -*"],
    "rustling-tulip": [
        "cargo run --manifest-path Cargo.toml -p rustling-tulip-native --example shot*",
        "pwsh -NoProfile -File rt.ps1 native-drive-check*",
        "pwsh -NoProfile -File ./rt.ps1 native-drive-check*",
    ],
    "claude": ["pwsh -NoProfile -File tools?build-box?gl-probe.ps1 -Mode GodotAngle"],
    "herdr-gpui": ["cargo test --locked -p herdr-gpui --all-features credential_manager"],
}

BURSTS = [
    ("11-09 18:31", "2026-11-09T18:31:08", "2026-11-09T18:32:56"),
    ("11-10 03:24", "2026-11-10T03:24:07", "2026-11-10T03:27:12"),
    ("11-10 04:23", "2026-11-10T04:23:01", "2026-11-10T04:28:30"),
    ("11-10 05:12", "2026-11-10T05:12:41", "2026-11-10T05:15:44"),
    ("11-10 05:43", "2026-11-10T05:43:51", "2026-11-10T05:46:56"),
    ("11-10 07:42", "2026-11-10T07:42:42", "2026-11-10T07:45:50"),
    ("11-10 08:08", "2026-11-10T08:08:01", "2026-11-10T08:11:04"),
    ("11-10 08:26", "2026-11-10T08:26:17", "2026-11-10T08:37:04"),
    ("11-10 08:49", "2026-11-10T08:49:32", "2026-11-10T09:15:54"),
    ("11-10 16:02", "2026-11-10T16:02:32", "2026-11-10T16:05:55"),
    ("11-10 16:27", "2026-11-10T16:27:28", "2026-11-10T17:20:36"),
    ("11-10 19:09", "2026-11-10T19:09:47", "2026-11-10T19:10:57"),
    ("11-10 20:19", "2026-11-10T20:19:26", "2026-11-10T20:22:27"),
    ("11-10 20:38", "2026-11-10T20:38:39", "2026-11-10T20:43:39"),
    ("11-10 21:03", "2026-11-10T21:03:55", "2026-11-10T21:12:55"),
]


def is_locked(e):
    pats = LOCKED.get(e["_repo"], [])
    w = e.get("sentWords") or ""
    return any(fnmatchcase(w, p) for p in pats)


class Count:
    def __init__(self, runs):
        self.s = sorted(e["_start"] for e in runs)
        self.e = sorted(e["_end"] for e in runs)

    def at(self, t):
        return bisect.bisect_right(self.s, t) - bisect.bisect_right(self.e, t)


def main():
    evs = h.load()
    win, mac = [], []
    for e in evs:
        if not e.get("remote"):
            continue
        t = h.ts(e["time"])
        e["_end"] = t
        e["_start"] = t - timedelta(seconds=e.get("heldSeconds") or 0)
        e["_repo"] = h.repo_of(e.get("cwd"))
        cmd = (e.get("phases") or {}).get("command")
        adj = e.get("adjustedSeconds")
        e["_cmd"] = cmd
        e["_free"] = (adj / cmd) if (adj is not None and cmd and cmd > 1) else None
        b = h.builder_of(e)
        (win if b == "WIN" else mac if b == "MAC" else []).append(e)
    locked = [e for e in win if is_locked(e)]
    other = [e for e in win if not is_locked(e)]
    cl, co, cm = Count(locked), Count(other), Count(mac)
    print(f"WIN routed runs {len(win)}, desktop-locked {len(locked)}")

    print("\nburst | WIN held mean | locked-desktop runs held mean (max) | of which waiting (n-1) mean | other runs held mean | effective active mean | other heavy free share")
    for name, a, z in BURSTS:
        a = datetime.fromisoformat(a).replace(tzinfo=timezone.utc)
        z = datetime.fromisoformat(z).replace(tzinfo=timezone.utc)
        t = a
        L, O = [], []
        while t <= z:
            L.append(cl.at(t))
            O.append(co.at(t))
            t += timedelta(seconds=10)
        wait = [max(0, n - 1) for n in L]
        frees = [(e["_free"], e["_cmd"]) for e in other if e["_start"] < z and e["_end"] > a and e["_free"] is not None and e.get("kind") == "heavy"]
        w = sum(c for _, c in frees)
        fs = sum(f * c for f, c in frees) / w if w else None
        print(f"{name} | {statistics.mean(l + o for l, o in zip(L, O)):.1f} | {statistics.mean(L):.1f} ({max(L)}) | {statistics.mean(wait):.1f} | {statistics.mean(O):.1f} | {statistics.mean(o + min(1, l) for l, o in zip(L, O)):.1f} | {fs if fs is None else round(fs, 2)}")

    # CPU curve on non-desktop heavy runs, bucketed by effective active runs (others + min(1, locked)) at the midpoint
    base = defaultdict(list)
    for e in other:
        if e["_cmd"] and e.get("verdict") == "passed":
            base[(e["_repo"], e.get("sentWords"))].append(e["_cmd"])
    med = {k: statistics.median(v) for k, v in base.items() if len(v) >= 3}
    since = datetime(2026, 11, 1, tzinfo=timezone.utc)
    byc = defaultdict(lambda: [[], []])
    for e in other:
        if e["_end"] < since or e.get("kind") != "heavy" or not e["_cmd"] or e["_cmd"] < 5:
            continue
        mid = e["_start"] + (e["_end"] - e["_start"]) / 2
        n = co.at(mid) + min(1, cl.at(mid))
        if e["_free"] is not None:
            byc[min(n, 10)][0].append(e["_free"])
        u = med.get((e["_repo"], e.get("sentWords")))
        if u and u >= 5 and e.get("verdict") == "passed":
            byc[min(n, 10)][1].append(e["_cmd"] / u)
    print("\nnon-desktop heavy WIN runs since 11-01, by active runs at midpoint (others + desktop holder)")
    print("active | n | median free | p25 free | median slowdown vs own median | throughput (active / slowdown)")
    for c in sorted(byc):
        fr, ra = sorted(byc[c][0]), sorted(byc[c][1])
        mr = statistics.median(ra) if ra else None
        print(f"{c} | {len(fr)} | {statistics.median(fr):.2f} | {fr[len(fr) // 4]:.2f} | {mr if mr is None else round(mr, 2)} | {c / mr if mr else 0:.1f}")

    # whole-day held breakdown 11-10 06-24
    a = datetime(2026, 11, 10, 6, tzinfo=timezone.utc)
    z = datetime(2026, 11, 10, 23, 59, tzinfo=timezone.utc)
    t = a
    L, O, M = [], [], []
    while t <= z:
        L.append(cl.at(t))
        O.append(co.at(t))
        M.append(cm.at(t))
        t += timedelta(seconds=30)
    full = [i for i, (l, o) in enumerate(zip(L, O)) if l + o >= 10]
    print(f"\n11-10 06-24 UTC: samples at 10 held {len(full)} of {len(L)}; in them locked-desktop held mean {statistics.mean(L[i] for i in full) if full else 0:.1f}, waiting mean {statistics.mean(max(0, L[i] - 1) for i in full) if full else 0:.1f}, Mac held mean {statistics.mean(M[i] for i in full) if full else 0:.1f}")
    print(f"11-10 06-24 UTC: Mac held mean {statistics.mean(M):.2f}, share of time Mac had >=3 free {sum(1 for m in M if m <= 3) / len(M):.0%}")
    macfree = [e["_free"] for e in mac if e["_end"] >= since and e["_free"] is not None and e.get("kind") == "heavy"]
    if macfree:
        print(f"Mac heavy free share since 11-01: median {statistics.median(macfree):.2f} over {len(macfree)} runs")


if __name__ == "__main__":
    sys.exit(main())
