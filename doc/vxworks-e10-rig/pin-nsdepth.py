#!/usr/bin/env python3
"""E10 item 2: compile EPICS_CA_NAMESERVER_QUEUE_DEPTH=8 into the CA image, so
the 144 B fire_searches class reaches its ceiling inside one boot instead of in
the ~6.7 hours the shipping default 256 would take at the measured ~1 block per
94 s."""
import os

p = os.path.expanduser("~/vx-rig-e10/tree/crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs")
s = open(p).read()
a = '            ("EPICS_CA_CONN_TMO", "5"),\n'
assert a in s, "anchor missing"
if "EPICS_CA_NAMESERVER_QUEUE_DEPTH" not in s:
    s = s.replace(a, a + '            ("EPICS_CA_NAMESERVER_QUEUE_DEPTH", "8"),\n')
    open(p, "w").write(s)
    print("pinned ns depth to 8")
else:
    print("already pinned")
