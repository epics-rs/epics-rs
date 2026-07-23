"""Task 3 attribution: name WHICH epicsEvent blocks leak per rsrv client cycle,
by recording every epicsEventCreate/Destroy caller PC through the ciocEvTrace
--wrap tracer during a KNOWN small number of external CA client cycles, then
resolving each PC with arm-rtems6-addr2line off-box.

Same external-client cycle as evleak.py (hold6 connect + close), same port 5164,
concurrency 1. Tracing is switched on only around the counted cycles so no boot
or background create/destroy is recorded.
"""
import socket, struct, time, os, sys

HOST, PORT = "127.0.0.1", 5164
FIFO = os.path.expanduser("~/rtems-cside/ciocin")
N = int(sys.argv[1]) if len(sys.argv) > 1 else 3
GAP = 0.10

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

print("== evattr: %d counted cycles, tracer on ==" % N, flush=True)
w("#=== ATTR reset ===\n"); time.sleep(1)
w("evtrace reset\n");       time.sleep(1)
w("evtrace on\n");          time.sleep(1)
for i in range(N):
    cycle()
time.sleep(3)
w("evtrace off\n");         time.sleep(1)
w("#=== ATTR count ===\n"); time.sleep(1)
w("evtrace count\n");       time.sleep(2)
w("#=== ATTR dump ===\n");  time.sleep(1)
w("evtrace dump\n");        time.sleep(6)
w("#=== ATTR end ===\n");   time.sleep(2)
print("== evattr done ==", flush=True)
