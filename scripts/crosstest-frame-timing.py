#!/usr/bin/env python3
"""W3 cross-test: DXVKit Plus frame timing vs Swift Glance FrameTiming, file by file.

Inputs: two JSON-lines files keyed by "file" (relative path) — Swift's (the throwaway probe built
from glance Sources/GlanceCore/FrameTiming.swift) and ours (`--example frame_times`). Compares
the rate (as a reduced fraction), the rule that gave it, the frame count and every frame start (exact fractions; Swift's
grid when it has no table). Prints one JSON line per file and a summary line; exit 1 on any
difference. Paths in the output are relative (basenames only from the test folder).

usage: crosstest-frame-timing.py swift.jsonl ours.jsonl
"""
import json
import sys
from fractions import Fraction as F


def load(path):
    return {d["file"]: d for d in map(json.loads, open(path))}


def main():
    swift, ours = load(sys.argv[1]), load(sys.argv[2])
    same = differ = skipped = 0
    for f in sorted(set(swift) | set(ours)):
        s, o = swift.get(f, {}), ours.get(f, {})
        row = {"file": f}
        if "error" in s or "error" in o or not s or not o:
            row.update(compared=False, swift_error=s.get("error", "missing" if not s else None), ours_error=o.get("error", "missing" if not o else None))
            skipped += 1
            print(json.dumps(row, sort_keys=True))
            continue
        rs, ro = F(*s["rate"]), F(*o["rate"])
        ts = [F(v, t) for v, t in s["times"]] or [n / rs for n in range(s["frame_count"])]
        to = [F(v, t) for v, t in o["times"]] or [n / ro for n in range(o["frame_count"])]
        diffs = [n for n, (a, b) in enumerate(zip(ts, to)) if a != b]
        camel = "".join(w.title() if i else w for i, w in enumerate(o["source"].split("_")))
        ok = rs == ro and s["source"] == camel and s["frame_count"] == o["frame_count"] and not diffs and len(ts) == len(to)
        row.update(compared=True, equal=ok, rate=[rs.numerator, rs.denominator], rate_ours=[ro.numerator, ro.denominator],
                   source=s["source"], source_ours=o["source"], frames=s["frame_count"], frames_ours=o["frame_count"],
                   samples=o.get("samples"), table=s["table"], table_ours=o["table"], start_diffs=len(diffs), first_diff=diffs[:3])
        same += ok
        differ += not ok
        print(json.dumps(row, sort_keys=True))
    print(json.dumps({"summary": True, "equal": same, "different": differ, "not_compared": skipped}))
    sys.exit(1 if differ else 0)


if __name__ == "__main__":
    main()
