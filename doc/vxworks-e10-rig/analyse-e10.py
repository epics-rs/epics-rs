#!/usr/bin/env python3
"""analyse-e10.py — read one E10 console log into the numbers the doc quotes.

    analyse-e10.py <console.log> [seq_lo] [seq_hi]

Prints, for the chosen steady window: the live-bytes and live-block endpoints,
their deltas, the attempt span, a least-squares B/attempt slope, the accounting
health counters, and the per-size-class / per-site deltas between the two detail
passes that bracket the window.  Every number is read from the log; nothing is
inferred.
"""

import re
import sys
from collections import OrderedDict

LIVE = re.compile(
    r"HEAPLIVE seq=(\d+) live_bytes=(-?\d+) live_blocks=(-?\d+) alloc=(\d+) "
    r"free=(\d+) untracked_free=(\d+) blk_ovf=(\d+) site_ovf=(\d+) size_ovf=(\d+)"
)
# The CA probe line carries queued/dialing, the PVA one does not.
DIAL = re.compile(
    r"(?:C6|STAGE5) seq=(\d+) dialpool workers=(\d+) attempts=(\d+).*?MEM_USED=(-?\d+)"
)
SIZE = re.compile(r"HEAPSIZE seq=(\d+) size=(\d+) live=(-?\d+) bytes=(-?\d+) allocs=(\d+)")
SITE = re.compile(
    r"HEAPSITE seq=(\d+) pc=0x([0-9a-f]+) calls=(\d+) bytes=(\d+) live=(-?\d+) "
    r"livebytes=(-?\d+)"
)


def parse(path):
    live, dial, sizes, sites = {}, {}, {}, {}
    with open(path, errors="replace") as f:
        for line in f:
            m = LIVE.search(line)
            if m:
                s = int(m.group(1))
                live[s] = dict(
                    bytes=int(m.group(2)),
                    blocks=int(m.group(3)),
                    alloc=int(m.group(4)),
                    free=int(m.group(5)),
                    untracked_free=int(m.group(6)),
                    blk_ovf=int(m.group(7)),
                    site_ovf=int(m.group(8)),
                    size_ovf=int(m.group(9)),
                )
                continue
            m = DIAL.search(line)
            if m:
                dial[int(m.group(1))] = dict(
                    workers=int(m.group(2)),
                    attempts=int(m.group(3)),
                    mem_used=int(m.group(4)),
                )
                continue
            m = SIZE.search(line)
            if m:
                s = int(m.group(1))
                sizes.setdefault(s, {})[int(m.group(2))] = (
                    int(m.group(3)),
                    int(m.group(4)),
                )
                continue
            m = SITE.search(line)
            if m:
                s = int(m.group(1))
                sites.setdefault(s, {})[int(m.group(2), 16)] = (
                    int(m.group(3)),
                    int(m.group(5)),
                    int(m.group(6)),
                )
    return live, dial, sizes, sites


def lsq(points):
    n = len(points)
    if n < 2:
        return float("nan")
    sx = sum(p[0] for p in points)
    sy = sum(p[1] for p in points)
    sxx = sum(p[0] * p[0] for p in points)
    sxy = sum(p[0] * p[1] for p in points)
    d = n * sxx - sx * sx
    return float("nan") if d == 0 else (n * sxy - sx * sy) / d


def main():
    path = sys.argv[1]
    live, dial, sizes, sites = parse(path)
    common = sorted(set(live) & set(dial))
    if not common:
        print(f"{path}: no paired HEAPLIVE/dialpool samples")
        return
    lo = int(sys.argv[2]) if len(sys.argv) > 2 else common[0]
    hi = int(sys.argv[3]) if len(sys.argv) > 3 else common[-1]
    win = [s for s in common if lo <= s <= hi]
    a, b = win[0], win[-1]

    print(f"== {path}")
    print(f"samples paired: {len(common)}  window seq {a}..{b} ({len(win)} samples)")
    for tag, s in (("start", a), ("end", b)):
        print(
            f"  {tag:5s} seq={s:4d} attempts={dial[s]['attempts']:4d} "
            f"workers={dial[s]['workers']:4d} live_bytes={live[s]['bytes']:8d} "
            f"live_blocks={live[s]['blocks']:6d} MEM_USED={dial[s]['mem_used']}"
        )
    d_att = dial[b]["attempts"] - dial[a]["attempts"]
    d_byt = live[b]["bytes"] - live[a]["bytes"]
    d_blk = live[b]["blocks"] - live[a]["blocks"]
    d_mem = dial[b]["mem_used"] - dial[a]["mem_used"]
    print(
        f"  delta over {d_att} attempts: live_bytes {d_byt:+d}  live_blocks {d_blk:+d}"
        f"  MEM_USED {d_mem:+d}"
    )
    slope = lsq([(dial[s]["attempts"], live[s]["bytes"]) for s in win])
    print(f"  lsq slope: {slope:.3f} B/attempt")
    if d_att:
        print(f"  endpoint  : {d_byt / d_att:.3f} B/attempt")
    last = live[b]
    print(
        f"  accounting: alloc={last['alloc']} free={last['free']} "
        f"untracked_free={last['untracked_free']} blk_ovf={last['blk_ovf']} "
        f"site_ovf={last['site_ovf']} size_ovf={last['size_ovf']}"
    )
    print(f"  workers: min={min(dial[s]['workers'] for s in win)} "
          f"max={max(dial[s]['workers'] for s in win)}")

    det = sorted(s for s in sizes if lo <= s <= hi)
    if len(det) >= 2:
        p, q = det[0], det[-1]
        print(f"\n  per-size-class delta, detail seq {p} -> {q} "
              f"(attempts {dial.get(p, {}).get('attempts')} -> "
              f"{dial.get(q, {}).get('attempts')}):")
        keys = set(sizes[p]) | set(sizes[q])
        rows = []
        for k in keys:
            c0, b0 = sizes[p].get(k, (0, 0))
            c1, b1 = sizes[q].get(k, (0, 0))
            if c1 - c0 or b1 - b0:
                rows.append((b1 - b0, k, c1 - c0))
        rows.sort(reverse=True)
        tot_b = tot_c = 0
        for db_, k, dc in rows:
            print(f"    size {k:8d}  dcount {dc:+5d}  dbytes {db_:+8d}")
            tot_b += db_
            tot_c += dc
        print(f"    {'TOTAL':>13s}  dcount {tot_c:+5d}  dbytes {tot_b:+8d}")

    det = sorted(s for s in sites if lo <= s <= hi)
    if len(det) >= 2:
        p, q = det[0], det[-1]
        print(f"\n  per-site delta, detail seq {p} -> {q} (top 15 by dbytes):")
        keys = set(sites[p]) | set(sites[q])
        rows = []
        for k in keys:
            _, l0, b0 = sites[p].get(k, (0, 0, 0))
            _, l1, b1 = sites[q].get(k, (0, 0, 0))
            if l1 - l0 or b1 - b0:
                rows.append((b1 - b0, k, l1 - l0))
        rows.sort(key=lambda r: -abs(r[0]))
        for db_, k, dl in rows[:15]:
            print(f"    pc 0x{k:x}  dlive {dl:+5d}  dbytes {db_:+8d}")
        print(f"    sites with any change: {len(rows)}")


if __name__ == "__main__":
    main()
