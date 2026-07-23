"""Task 2: measure the libca per-virtual-circuit heap cost on the RTEMS target
DIRECTLY, instead of deriving 6x32=192 from the block size.

Isolation: the target's libca CLIENT connects to a HOST softIoc (EPICS_CA_NAME_
SERVERS=10.0.2.2:5064, TCP-direct). The SERVER side of every circuit therefore
allocates in the host process, not in the target heap that `rt malloc` reads.
A loopback (same-IOC) client cannot be used: CA short-circuits a locally-served
PV to in-memory dbCa and builds no virtual circuit at all ("dbContext:
preemptive callback required for direct in memory interfacing").

Differential:
  mode 0 = ca_context_create + ca_context_destroy, NO channel  -> context cost
  mode 1 = same + one channel connected to the remote PV        -> context + circuit
per-circuit = mode1 slope - mode0 slope, on used blocks and on bytes, each read
from `rt malloc`. Two batches per mode = independent linearity check.

Pacing: each batch is issued as CHUNK-sized caloop calls with a drain pause
between them, so libbsd's socket pool is not driven to kern.ipc.maxsockets and
every circuit connects. Concurrency is 1 within each caloop (sequential).
Readings are bracketed by `#=== tag ===` markers echoed into cioc.log.
"""
import os, sys, time

FIFO = os.path.expanduser("~/rtems-cside/ciocin")
WARM = 30
BATCH = 200
CHUNK = 25
DRAIN = 3.0
PV = "TEST:AI"

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

def caloop_batch(total, mode):
    done = 0
    while done < total:
        n = min(CHUNK, total - done)
        w("caloop %d %s %d\n" % (n, PV, mode))
        time.sleep(max(4.0, n * 0.06) + DRAIN)
        done += n

def reading(tag):
    time.sleep(4)
    w("#=== %s ===\n" % tag);   time.sleep(2)
    w("rt malloc\n");           time.sleep(7)
    w("#=== END %s ===\n" % tag); time.sleep(2)
    print("reading %s" % tag, flush=True)

print("== evcirc: libca per-circuit differential, PV=%s ==" % PV, flush=True)
w('epicsEnvSet("EPICS_CA_NAME_SERVERS","10.0.2.2:5064")\n'); time.sleep(1)
w('epicsEnvSet("EPICS_CA_ADDR_LIST","")\n');                 time.sleep(1)
w('epicsEnvSet("EPICS_CA_AUTO_ADDR_LIST","NO")\n');          time.sleep(1)

# ---- phase A: context-only (mode 0) ----
caloop_batch(WARM, 0)
reading("A0-mode0-baseline")
caloop_batch(BATCH, 0)
reading("A1-mode0-batchA")
caloop_batch(BATCH, 0)
reading("A2-mode0-batchB")

# ---- phase B: context + one remote circuit (mode 1) ----
caloop_batch(WARM, 1)
reading("B0-mode1-baseline")
caloop_batch(BATCH, 1)
reading("B1-mode1-batchA")
caloop_batch(BATCH, 1)
reading("B2-mode1-batchB")

print("== evcirc done: WARM=%d BATCH=%d CHUNK=%d ==" % (WARM, BATCH, CHUNK), flush=True)
