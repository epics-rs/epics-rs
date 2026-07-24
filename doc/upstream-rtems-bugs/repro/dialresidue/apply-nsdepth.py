#!/usr/bin/env python3
"""MEASUREMENT MUTATION 3: revert the per-attempt dial (back to the shipping
pooled shape) and compile in a small EPICS_CA_NAMESERVER_QUEUE_DEPTH, so the
144 B/site's ceiling can be *observed* inside one boot instead of extrapolated
(at the measured ~1 frame per 120 s the default 256 would take ~8.5 hours)."""
import os, shutil
HOME=os.path.expanduser("~")
T=os.path.join(HOME,"epics-rs/crates/epics-ca-rs/src/client/transport.rs")
bak=T+".pooled-orig"
if os.path.exists(bak):
    shutil.copyfile(bak,T); print("reverted the per-attempt dial mutation")
IOC=os.path.join(HOME,"epics-rs/crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs")
s=open(IOC).read()
a='            ("EPICS_CA_CONN_TMO", "5"),\n'
assert a in s
if "EPICS_CA_NAMESERVER_QUEUE_DEPTH" not in s:
    s=s.replace(a,a+'            ("EPICS_CA_NAMESERVER_QUEUE_DEPTH", "8"),\n')
    open(IOC,"w").write(s)
print("ns depth pinned to 8")
