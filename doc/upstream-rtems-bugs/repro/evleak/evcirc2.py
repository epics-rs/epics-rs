"""Task 2, focused: a CLEAN libca mode1 (context + one remote circuit) heap slope
on a fresh boot, paced so concurrent TCP TIME_WAIT sockets stay well under
libbsd kern.ipc.maxsockets (a 200-cycle burst hits that wall fatally).

Read against the already-clean mode0 (context-only) slope of 2.0 blocks /
64 bytes per cycle measured over 2x200 cycles: per-circuit = mode1 - mode0.

Pacing: chunks of 8 with an 8 s drain, and a 70 s TIME_WAIT flush between the
two 40-cycle batches, so no 60 s window holds more than ~40 client sockets.
"""
import os, time

FIFO = os.path.expanduser("~/rtems-cside/ciocin")
PV = "TEST:AI"

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

def caloop_batch(total, mode, chunk=8, drain=8.0):
    done = 0
    while done < total:
        n = min(chunk, total - done)
        w("caloop %d %s %d\n" % (n, PV, mode))
        time.sleep(max(3.0, n * 0.06) + drain)
        done += n

def reading(tag):
    time.sleep(4)
    w("#=== %s ===\n" % tag);   time.sleep(2)
    w("rt malloc\n");           time.sleep(7)
    w("#=== END %s ===\n" % tag); time.sleep(2)
    print("reading %s" % tag, flush=True)

print("== evcirc2: clean paced mode1 slope ==", flush=True)
w('epicsEnvSet("EPICS_CA_NAME_SERVERS","10.0.2.2:5064")\n'); time.sleep(1)
w('epicsEnvSet("EPICS_CA_ADDR_LIST","")\n');                 time.sleep(1)
w('epicsEnvSet("EPICS_CA_AUTO_ADDR_LIST","NO")\n');          time.sleep(1)

caloop_batch(15, 1)              # warmup
reading("C0-mode1-baseline")
caloop_batch(40, 1)
reading("C1-mode1-batchA-40")
print("TIME_WAIT flush 70s", flush=True); time.sleep(70)
caloop_batch(40, 1)
reading("C2-mode1-batchB-40")

print("== evcirc2 done ==", flush=True)
