"""Measure the heap cost of one CA client connect/disconnect cycle on the stock
C IOC, to put a number on the RTEMS-posix epicsEventDestroy leak.

Design constraints (all deliberate):
  * concurrency is EXACTLY 1 for the whole run -- one socket open at a time and
    a GAP pause after each close, so rsrv's freelists and buffer pools can never
    raise their concurrent high-water mark mid-run and fake a slope.
  * WARMUP cycles run BEFORE the baseline reading, so the bounded caching that
    freelists do on the first cycles is already paid for when the baseline is
    taken.
  * two batches (A then B) with a reading after each, so the per-cycle slope is
    computed twice independently; the two agreeing is the linearity check.

Readings are taken through the console fifo with stock commands only:
  `rt malloc`  -- base's `rt` bridge to the RTEMS shell's malloc statistics
  `epicsThreadShowAll 1` -- base's own thread census
and bracketed by `#=== tag ===` comment markers that iocsh echoes into cioc.log.

Connect method is hold6.py's, unchanged.
"""
import socket, struct, time, os, sys

HOST, PORT = "127.0.0.1", 5164
FIFO = os.path.expanduser("~/rtems-cside/ciocin")

GAP     = 0.04   # inter-cycle pause: lets the server finish teardown, keeping
                 # server-side concurrency at 1 as well as client-side
SETTLE  = 6.0    # quiescence before a reading
WARMUP  = 60
BATCH_A = 250
BATCH_B = 600

def hdr(cmd, payload=b"", dtype=0, dcount=0, p1=0, p2=0):
    payload = payload + b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, dcount, p1, p2) + payload

HELLO = hdr(0, dtype=0, dcount=13) + hdr(20, b"probe\0") + hdr(21, b"probe\0")

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

def connect():
    s = socket.create_connection((HOST, PORT), timeout=10)
    s.settimeout(10)
    s.sendall(HELLO)
    s.recv(16)      # CA_PROTO_VERSION reply: proves the server-side client
                    # task is up and has run, not just that TCP accepted
    return s

def cycle():
    s = connect()
    s.close()
    time.sleep(GAP)

def run(n, tag):
    t0 = time.time()
    for i in range(n):
        cycle()
        if (i + 1) % 100 == 0:
            print("  %s %d/%d" % (tag, i + 1, n), flush=True)
    print("%s: %d cycles in %.1f s" % (tag, n, time.time() - t0), flush=True)

def reading(tag, settle=SETTLE):
    time.sleep(settle)
    w("#=== %s ===\n" % tag);      time.sleep(2)
    w("rt malloc\n");              time.sleep(7)
    w("epicsThreadShowAll 1\n");   time.sleep(7)
    w("#=== END %s ===\n" % tag);  time.sleep(2)
    print("reading %s taken" % tag, flush=True)

print("== evleak: stock cioc-fd64.exe, port %d ==" % PORT, flush=True)

# 0. first reading. NOTE: a 20-cycle smoke test preceded this, so it is not a
#    virgin-heap reading; it is not used as the baseline (B0 is).
reading("T0-initial-after-20-smoke-cycles")

# 1. requirement 5: what does ONE held connection actually create on the server?
s = connect()
time.sleep(4)
w("#=== HELD-1-connection ===\n"); time.sleep(2)
w("epicsThreadShowAll 1\n");       time.sleep(7)
w("rt malloc\n");                  time.sleep(7)
w("#=== END HELD-1-connection ===\n"); time.sleep(2)
s.close()
print("held-1 census taken", flush=True)

reading("T1-idle-after-held-1")

# 2. warm-up, THEN baseline
run(WARMUP, "warmup")
reading("B0-baseline-after-%d-warmup" % WARMUP)

# 3. batch A
run(BATCH_A, "batchA")
reading("B1-after-batchA-%d" % BATCH_A)

# 4. batch B
run(BATCH_B, "batchB")
reading("B2-after-batchB-%d" % BATCH_B)

print("== evleak done: warmup=%d A=%d B=%d ==" % (WARMUP, BATCH_A, BATCH_B), flush=True)
