"""Task 4: endurance. The bytes measurement stopped at 600 cycles; this drives
the same stock-image server-side CA connect/disconnect cycle well past that and
reads EVERY rt-malloc counter at each milestone, to test (a) whether the 5.000
blocks / 160 B per cycle slope holds at scale and (b) whether any other counter
(free blocks, largest free block, lifetime alloc/free, searches, resizes, failed
allocations) drifts non-linearly.

Same cycle, discipline and image family as evleak.py: concurrency 1, 40 ms gap,
60 warm-up before the baseline. Milestones at 0/500/1000/2000/3000 cycles past
baseline give four independent slope segments for the linearity check.
"""
import socket, struct, time, os

HOST, PORT = "127.0.0.1", 5164
FIFO = os.path.expanduser("~/rtems-cside/ciocin")
GAP = 0.04
WARMUP = 60
MILESTONES = [500, 500, 1000, 1000]   # cumulative 500,1000,2000,3000

def hdr(cmd, payload=b"", dtype=0, dcount=0, p1=0, p2=0):
    payload = payload + b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, dcount, p1, p2) + payload

HELLO = hdr(0, dtype=0, dcount=13) + hdr(20, b"probe\0") + hdr(21, b"probe\0")

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

def cycle():
    s = socket.create_connection((HOST, PORT), timeout=10)
    s.settimeout(10)
    s.sendall(HELLO)
    s.recv(16)
    s.close()
    time.sleep(GAP)

def run(n, tag):
    t0 = time.time()
    for i in range(n):
        cycle()
    print("%s: %d cycles in %.1f s" % (tag, n, time.time() - t0), flush=True)

def reading(tag):
    time.sleep(5)
    w("#=== %s ===\n" % tag);      time.sleep(2)
    w("rt malloc\n");              time.sleep(7)
    w("epicsThreadShowAll 1\n");   time.sleep(6)
    w("#=== END %s ===\n" % tag);  time.sleep(2)
    print("reading %s" % tag, flush=True)

print("== evendure: stock cioc-fd64.exe, endurance ==", flush=True)
run(WARMUP, "warmup")
reading("E0-baseline")
total = 0
for m in MILESTONES:
    run(m, "batch+%d" % m)
    total += m
    reading("E-at-%d" % total)
print("== evendure done: %d cycles past baseline ==" % total, flush=True)
