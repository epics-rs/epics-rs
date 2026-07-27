#!/usr/bin/env python3
"""E10 rig: set the PVA client's RECONNECT_INTERVAL to <secs>. 5 is the rig
cadence the four committed round-1 runs used; 10 is the shipping default."""
import os
import re
import sys

secs = sys.argv[1]
p = os.path.expanduser(
    "~/vx-rig-e10/tree/crates/epics-pva-rs/src/client_native/search_engine.rs"
)
s = open(p).read()
new = (
    "    const RECONNECT_INTERVAL: Duration = Duration::from_secs(%s);"
    " // E10 RIG\n" % secs
)
s2 = re.sub(
    r"    const RECONNECT_INTERVAL: Duration = Duration::from_secs\(\d+\);[^\n]*\n",
    new,
    s,
    count=1,
)
assert s2 != s, "anchor missing or already identical"
open(p, "w").write(s2)
print(new.rstrip())
