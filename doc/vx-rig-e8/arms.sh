#!/bin/bash
# Six more Medium/Medium 1024M wall runs, three with the FramePool applied and
# three without, alternating so a slow drift on the box cannot land entirely in
# one arm.  Per run we record: served count, first-failure verbatim, the
# POOLPROBE SETS/WORKERS/REFUSED triple, and whether the RTP was deleted by a
# page fault.
set -u
cd "$HOME/vx-rig-e8" || exit 1
OUT=$HOME/vx-rig-e8/arms.tsv
: > "$OUT"
for i in 3 4 5; do
    for arm in pool nopool; do
        if [ "$arm" = pool ]; then python3 rigpool.py > /dev/null; else python3 rigpool.py --restore > /dev/null; fi
        T="${arm}M${i}"
        ./stackclass.sh Medium 1024M "$T" Medium wall > "run-$T.log" 2>&1
        SERVED=$(rg -o -N "client-side served = .*" "phaseramp-$T.log" | tail -1)
        FIRST=$(rg -o -N "first failure verbatim: [A-Z_]+\(([A-Za-z]+|CA_PROTO_ERROR)" "phaseramp-$T.log" | tail -1)
        PROBE=$(rg -o -N "POOLPROBE seq=1 .*" "console-$T.log" | tail -1)
        FAULT=$(rg -c "has had a failure and has been deleted" "console-$T.log" 2>/dev/null || echo 0)
        MAXSET=$(rg -o -N "SETS=[0-9]+" "console-$T.log" | sort -t= -k2 -n | tail -1)
        printf '%s\t%s\t%s\t%s\tfaultlines=%s\tmax=%s\n' "$T" "$SERVED" "$FIRST" "$PROBE" "$FAULT" "$MAXSET" >> "$OUT"
    done
done
# leave the tree in the un-pooled state; the pooled state is a probe, not a commit
python3 rigpool.py --restore > /dev/null
echo "ARMS-DONE" >> "$OUT"
