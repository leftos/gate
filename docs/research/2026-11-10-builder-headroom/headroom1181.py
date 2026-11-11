"""GATE-1181: builder headroom during full/busy fallback bursts. Read-only over the gate journal."""

import glob
import gzip
import json
import os
import re
import statistics
import sys
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone

ROOT = os.path.join(os.environ["LOCALAPPDATA"], "gate", "v1")
WIN_SLOTS = 10
MAC_SLOTS = 6


def ts(s):
    s = s.rstrip("Z")
    if "." in s:
        a, b = s.split(".")
        s = a + "." + b[:6]
    return datetime.fromisoformat(s).replace(tzinfo=timezone.utc)


def load():
    files = sorted(glob.glob(os.path.join(ROOT, "archive", "*.jsonl.gz")))
    files += [os.path.join(ROOT, "events.1.jsonl"), os.path.join(ROOT, "events.jsonl")]
    seen = set()
    out = []
    for f in files:
        op = gzip.open if f.endswith(".gz") else open
        with op(f, "rt", encoding="utf-8") as fh:
            for line in fh:
                if '"release"' not in line:
                    continue
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if e.get("event") != "release":
                    continue
                k = (e.get("pid"), e.get("processStart"), e.get("time"))
                if k in seen:
                    continue
                seen.add(k)
                out.append(e)
    return out


def repo_of(cwd):
    m = re.match(r"(?i)[A-Z]:\\dev\\([^\\]+)", cwd or "")
    if not m:
        return "?"
    r = m.group(1)
    return r[:-3] if r.endswith(".wt") else r


def builder_of(e):
    h = e.get("host") or ""
    if "mac" in h:
        return "MAC"
    if "buildbox" in h:
        return "WIN"
    return "OTHER" if h else None


def family(words):
    w = (words or "").split()
    if not w:
        return "?"
    if w[0] in ("dotnet", "cargo", "pnpm", "npm", "uv", "git") and len(w) > 1:
        return " ".join(w[:2])
    if w[0] in ("pwsh", "powershell"):
        for x in w[1:]:
            if x.endswith(".ps1"):
                return "pwsh " + os.path.basename(x)
    return os.path.basename(w[0])


def main():
    evs = load()
    routed = []
    fallbacks = []
    for e in evs:
        t = ts(e["time"])
        e["_end"] = t
        e["_repo"] = repo_of(e.get("cwd"))
        if e.get("remote"):
            b = builder_of(e)
            held = e.get("heldSeconds") or 0
            e["_start"] = t - timedelta(seconds=held)
            e["_b"] = b
            e["_fam"] = family(e.get("sentWords") or e.get("command"))
            cmd = (e.get("phases") or {}).get("command")
            e["_cmd"] = cmd
            adj = e.get("adjustedSeconds")
            e["_free"] = (adj / cmd) if (adj is not None and cmd and cmd > 1) else None
            routed.append(e)
        fb = e.get("fellBack") or []
        if "full" in fb or "busy" in fb:
            e["_start"] = ts(e["processStart"]) if e.get("processStart") else t
            e["_why"] = "full" if "full" in fb else "busy"
            e["_fam"] = family(e.get("sentWords") or e.get("command"))
            fallbacks.append(e)
    print(f"releases={len(evs)} routed={len(routed)} fallbacks={len(fallbacks)}")
    first = min(e["_end"] for e in evs)
    print("journal from", first.isoformat())

    # fallbacks per day
    perday = defaultdict(lambda: [0, 0.0, 0, 0.0])
    for f in fallbacks:
        d = f["_end"].date().isoformat()
        i = 0 if f["_why"] == "full" else 2
        perday[d][i] += 1
        perday[d][i + 1] += f.get("wallSeconds") or 0
    print("\nday | full n | full s | busy n | busy s")
    for d in sorted(perday):
        if d >= "2026-11-01":
            p = perday[d]
            print(f"{d} | {p[0]} | {p[1]:.0f} | {p[2]} | {p[3]:.0f}")

    win = [e for e in routed if e["_b"] == "WIN"]
    mac = [e for e in routed if e["_b"] == "MAC"]

    import bisect

    index = {}

    def conc(runs, t):
        key = id(runs)
        if key not in index or index[key][0] is not runs:
            index[key] = (runs, sorted(e["_start"] for e in runs), sorted(e["_end"] for e in runs))
        _, s, en = index[key]
        return range(bisect.bisect_right(s, t) - bisect.bisect_right(en, t))

    # bursts on 11-09 and 11-10
    fb2 = sorted((f for f in fallbacks if f["_end"].date().isoformat() in ("2026-11-09", "2026-11-10")), key=lambda f: f["_start"])
    bursts = []
    for f in fb2:
        if bursts and f["_start"] <= bursts[-1]["end"] + timedelta(minutes=10):
            bursts[-1]["items"].append(f)
            bursts[-1]["end"] = max(bursts[-1]["end"], f["_end"])
        else:
            bursts.append({"start": f["_start"], "end": f["_end"], "items": [f]})

    # baseline usual command seconds per (builder, repo, sentWords), excluding burst windows
    def in_any_burst(e):
        return any(e["_start"] < b["end"] and e["_end"] > b["start"] for b in bursts)

    base = defaultdict(list)
    for e in routed:
        if e["_cmd"] and e.get("verdict") == "passed" and not in_any_burst(e):
            base[(e["_b"], e["_repo"], e.get("sentWords"))].append(e["_cmd"])
    basefam = defaultdict(list)
    for e in routed:
        if e["_cmd"] and e.get("verdict") == "passed" and not in_any_burst(e):
            basefam[(e["_b"], e["_repo"], e["_fam"])].append(e["_cmd"])

    def usual(e):
        s = base.get((e["_b"], e["_repo"], e.get("sentWords")), [])
        if len(s) >= 3:
            return statistics.median(s), "exact", len(s)
        return None, None, 0

    def sample_series(runs, a, b, step=15):
        t = a
        vals = []
        while t <= b:
            vals.append(len(conc(runs, t)))
            t += timedelta(seconds=step)
        return vals

    for i, b in enumerate(bursts, 1):
        a, z = b["start"], b["end"]
        print(f"\n=== burst {i}: {a:%Y-%m-%d %H:%M:%S}Z to {z:%H:%M:%S}Z ({(z - a).total_seconds():.0f} s)")
        for f in b["items"]:
            print(f"  fallback {f['_why']}: {f['_start']:%H:%M:%S}-{f['_end']:%H:%M:%S} {f['_repo']} [{f['_fam']}] wall {f.get('wallSeconds') or 0:.0f} s, waited {(f['_end'] - f['_start']).total_seconds() - (f.get('heldSeconds') or 0):.0f} s, balanced={[(x.get('free'), 'mac' if 'mac' in x.get('builder', '') else 'win') for x in (f.get('balanced') or [])]}")
        wv = sample_series(win, a, z)
        mv = sample_series(mac, a, z)
        print(f"  WIN held: max {max(wv)} mean {statistics.mean(wv):.1f} share-of-time-at-10 {sum(1 for v in wv if v >= WIN_SLOTS) / len(wv):.0%}")
        print(f"  MAC held: max {max(mv)} mean {statistics.mean(mv):.1f} of {MAC_SLOTS}")
        # who held WIN slots: slot-seconds by repo/family within the window
        ss = Counter()
        sfam = Counter()
        for e in win:
            ov = (min(e["_end"], z) - max(e["_start"], a)).total_seconds()
            if ov > 0:
                ss[e["_repo"]] += ov
                sfam[(e["_repo"], e["_fam"], e.get("kind"))] += ov
        dur = (z - a).total_seconds()
        print("  WIN slot-holders (avg slots held):", ", ".join(f"{r} {s / dur:.1f}" for r, s in ss.most_common(8)))
        print("  top families:", "; ".join(f"{r} {fm} ({k}) {s / dur:.1f}" for (r, fm, k), s in sfam.most_common(8)))
        # free share and slowdown of WIN runs overlapping window
        frees, freesl, ratios, rows = [], [], [], []
        for e in win:
            if e["_start"] < z and e["_end"] > a and e["_cmd"]:
                if e["_free"] is not None:
                    (frees if e.get("kind") == "heavy" else freesl).append((e["_free"], e["_cmd"]))
                u, how, n = usual(e)
                if u and e.get("verdict") == "passed" and u >= 5:
                    r = e["_cmd"] / u
                    ratios.append(r)
                    rows.append((r, e["_repo"], e["_fam"], e["_cmd"], u, n, e["_free"]))
        def wavg(xs):
            w = sum(c for _, c in xs)
            return sum(f * c for f, c in xs) / w if w else None
        hf = wavg(frees)
        print(f"  WIN heavy free share (cmd-time weighted): {hf if hf is None else round(hf, 2)} over {len(frees)} runs; min {min((f for f, _ in frees), default=None)}")
        lf = wavg(freesl)
        print(f"  WIN light free share: {lf if lf is None else round(lf, 2)} over {len(freesl)} runs")
        if ratios:
            print(f"  slowdown vs usual (runs >=5 s usual, n={len(ratios)}): median {statistics.median(ratios):.2f}, max {max(ratios):.2f}, share >1.5x {sum(1 for r in ratios if r > 1.5) / len(ratios):.0%}")
            for r in sorted(rows, reverse=True)[:5]:
                print(f"    {r[0]:.2f}x {r[1]} [{r[2]}] {r[3]:.0f} s vs usual {r[4]:.0f} s (n={r[5]}), free {r[6] if r[6] is None else round(r[6], 2)}")

    # global: free share and slowdown by WIN concurrency (avg concurrency over the run's life)
    print("\n=== WIN heavy runs 2026-11-01..11-10: by slots held at the run's midpoint")
    byc = defaultdict(lambda: [[], []])
    winrecent = [e for e in win if e["_end"] >= datetime(2026, 11, 1, tzinfo=timezone.utc)]
    winrecent.sort(key=lambda e: e["_start"])
    for e in winrecent:
        if e.get("kind") != "heavy" or not e["_cmd"] or e["_cmd"] < 5:
            continue
        mid = e["_start"] + (e["_end"] - e["_start"]) / 2
        c = len(conc(winrecent, mid))
        bucket = min(c, 10)
        if e["_free"] is not None:
            byc[bucket][0].append(e["_free"])
        u, _, _ = usual(e)
        if u and u >= 5 and e.get("verdict") == "passed":
            byc[bucket][1].append(e["_cmd"] / u)
    print("held | n free | median free | p10 free | n ratio | median ratio | p90 ratio")
    for c in sorted(byc):
        fr, ra = byc[c]
        fr.sort()
        ra.sort()
        p10 = fr[len(fr) // 10] if fr else None
        p90 = ra[(len(ra) * 9) // 10] if ra else None
        print(f"{c} | {len(fr)} | {statistics.median(fr) if fr else '-':.2} | {p10 if p10 is None else round(p10, 2)} | {len(ra)} | {statistics.median(ra) if ra else 0:.2f} | {p90 if p90 is None else round(p90, 2)}")

    # time at 10 per day, and mac utilization per day
    print("\n=== per day: share of time WIN at >=9 / 10 held; MAC mean held (sampled every 30 s, 06:00-24:00 UTC)")
    for d in range(1, 11):
        a = datetime(2026, 11, d, 6, tzinfo=timezone.utc)
        z = a + timedelta(hours=18)
        ws = [e for e in win if e["_start"] < z and e["_end"] > a]
        ms = [e for e in mac if e["_start"] < z and e["_end"] > a]
        wv = sample_series(ws, a, z, 30)
        mv = sample_series(ms, a, z, 30)
        print(f"11-{d:02d} WIN >=9 {sum(1 for v in wv if v >= 9) / len(wv):.1%} =10 {sum(1 for v in wv if v >= 10) / len(wv):.1%} mean {statistics.mean(wv):.2f} | MAC mean {statistics.mean(mv):.2f} max {max(mv)} =6 {sum(1 for v in mv if v >= 6) / len(mv):.1%}")


if __name__ == "__main__":
    sys.exit(main())
