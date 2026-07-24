#!/usr/bin/env python3
"""Undo mutation 3's pinned queue depth: back to the shipping default 256, so
the low-threshold site run measures the same configuration runs 1 and 2 did."""
import os
IOC=os.path.expanduser("~/epics-rs/crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs")
s=open(IOC).read()
s=s.replace('            ("EPICS_CA_NAMESERVER_QUEUE_DEPTH", "8"),\n','')
open(IOC,"w").write(s)
print("ns depth back to the shipping default")
