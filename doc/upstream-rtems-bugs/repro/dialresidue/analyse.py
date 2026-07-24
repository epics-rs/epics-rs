#!/usr/bin/env python3
"""Analyse a HEAPATTR console log: per-size and per-site growth against the
dial-attempt counter, plus a least-squares slope over the steady window."""
import re, sys, collections

log = sys.argv[1]
WARM = int(sys.argv[2]) if len(sys.argv) > 2 else 30   # skip attempts below this

summary = {}      # seq -> dict
sizes = {}        # seq -> {size: live}
sites = {}        # seq -> {siteid: (live, bytes, total, pcs)}

for line in open(log, errors="replace"):
    if not line.startswith("HEAPATTR"):
        continue
    m = re.match(r"HEAPATTR seq=(\d+) attempts=(\d+) (.*)", line)
    if m:
        seq = int(m.group(1))
        d = {"attempts": int(m.group(2))}
        for k, v in re.findall(r"(\w+)=(\d+)", m.group(3)):
            d[k] = int(v)
        summary[seq] = d
        continue
    m = re.match(r"HEAPATTR seq=(\d+) sizes(.*)", line)
    if m:
        seq = int(m.group(1))
        h = sizes.setdefault(seq, {})
        for sz, cnt in re.findall(r"(\d+):(\d+)", m.group(2)):
            h[int(sz)] = int(cnt)
        continue
    m = re.match(r"HEAPATTR seq=(\d+) site=(\d+) live=(\d+) bytes=(\d+) total=(\d+) pc (.*)",
                 line)
    if m:
        seq = int(m.group(1))
        sites.setdefault(seq, {})[int(m.group(2))] = (
            int(m.group(3)), int(m.group(4)), int(m.group(5)),
            tuple(m.group(6).split()))


def lsq(xs, ys):
    n = len(xs)
    if n < 2:
        return float("nan")
    mx = sum(xs) / n
    my = sum(ys) / n
    den = sum((x - mx) ** 2 for x in xs)
    if den == 0:
        return float("nan")
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den


seqs = sorted(summary)
print(f"reports={len(seqs)} first_seq={seqs[0]} last_seq={seqs[-1]} "
      f"attempts {summary[seqs[0]]['attempts']} -> {summary[seqs[-1]]['attempts']}")
last = summary[seqs[-1]]
print("final:", " ".join(f"{k}={v}" for k, v in last.items()))

steady = [s for s in seqs if summary[s]["attempts"] >= WARM]
if len(steady) >= 2:
    xs = [summary[s]["attempts"] for s in steady]
    a0, a1 = xs[0], xs[-1]
    for key in ("live_bytes", "live_blocks"):
        ys = [summary[s][key] for s in steady]
        print(f"steady window attempts {a0}..{a1} ({len(steady)} reports): "
              f"{key} {ys[0]} -> {ys[-1]}  delta={ys[-1]-ys[0]}  "
              f"lsq slope={lsq(xs,ys):.3f} per attempt")

# ---- per-size growth ----------------------------------------------------
ssz = [s for s in sorted(sizes) if summary.get(s, {}).get("attempts", 0) >= WARM]
if len(ssz) >= 2:
    first, lastq = ssz[0], ssz[-1]
    da = summary[lastq]["attempts"] - summary[first]["attempts"]
    print(f"\n== per-size live-count growth, attempts "
          f"{summary[first]['attempts']}..{summary[lastq]['attempts']} (da={da}) ==")
    rows = []
    keys = set(sizes[first]) | set(sizes[lastq])
    for sz in keys:
        d = sizes[lastq].get(sz, 0) - sizes[first].get(sz, 0)
        if d:
            xs = [summary[s]["attempts"] for s in ssz]
            ys = [sizes[s].get(sz, 0) for s in ssz]
            rows.append((d * sz, sz, sizes[first].get(sz, 0),
                         sizes[lastq].get(sz, 0), d, lsq(xs, ys)))
    rows.sort(key=lambda r: -abs(r[0]))
    print(f"{'dBytes':>9} {'size':>8} {'first':>7} {'last':>7} {'dCount':>7} "
          f"{'blocks/attempt':>15} {'B/attempt':>10}")
    tot = 0
    for dB, sz, f, l, d, sl in rows[:30]:
        tot += dB
        print(f"{dB:>9} {sz:>8} {f:>7} {l:>7} {d:>7} {sl:>15.4f} {sl*sz:>10.2f}")
    print(f"{'TOTAL':>9} {sum(r[0] for r in rows):>8} "
          f"({sum(r[0] for r in rows)/da:.2f} B/attempt over all growing sizes)")

# ---- per-site growth ----------------------------------------------------
ssi = [s for s in sorted(sites) if summary.get(s, {}).get("attempts", 0) >= WARM]
if len(ssi) >= 2:
    first, lastq = ssi[0], ssi[-1]
    da = summary[lastq]["attempts"] - summary[first]["attempts"]
    print(f"\n== per-site live growth, attempts "
          f"{summary[first]['attempts']}..{summary[lastq]['attempts']} (da={da}) ==")
    rows = []
    for sid in set(sites[first]) | set(sites[lastq]):
        f = sites[first].get(sid, (0, 0, 0, ()))
        l = sites[lastq].get(sid, (0, 0, 0, ()))
        db = l[1] - f[1]
        if db:
            xs = [summary[s]["attempts"] for s in ssi]
            ys = [sites[s].get(sid, (0, 0, 0, ()))[1] for s in ssi]
            rows.append((db, sid, f[0], l[0], f[1], l[1], lsq(xs, ys),
                         l[3] or f[3]))
    rows.sort(key=lambda r: -abs(r[0]))
    for db, sid, fl, ll, fb, lb, sl, pcs in rows[:25]:
        print(f"site={sid:<6} live {fl}->{ll}  bytes {fb}->{lb}  d={db:+}  "
              f"slope={sl:.3f} B/attempt")
        print(f"        pc {' '.join(pcs)}")
