"""Second pass: hold 4 CA connections and drive `rt top` with A (all tasks)
and + (more lines) so the idle CAS-* rows are not truncated away."""
import socket, struct, time, sys, os
HOST, PORT = "127.0.0.1", 5164
PV = b"CIOC:AO\0"
N = 4
FIFO = os.path.expanduser("~/rtems-cside/ciocin")
def hdr(cmd, payload=b"", dtype=0, dcount=0, p1=0, p2=0):
    payload = payload + b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, dcount, p1, p2) + payload
HELLO = hdr(0, dtype=0, dcount=13) + hdr(20, b"probe\0") + hdr(21, b"probe\0")
socks = []
for i in range(1, N + 1):
    s = socket.create_connection((HOST, PORT), timeout=8)
    s.settimeout(8)
    s.sendall(HELLO); s.recv(16)
    s.sendall(hdr(18, PV, p1=4000 + i, p2=13)); s.recv(4096)
    socks.append(s)
print("held %d" % len(socks), flush=True)
time.sleep(3)
def w(text):
    with open(FIFO, "w") as f:
        f.write(text)
w("rt top\n"); time.sleep(6)
w("A"); time.sleep(4)          # toggle "all tasks"
w("+" * 25); time.sleep(4)     # more display lines
w("\n"); time.sleep(6)         # ENTER exits top
time.sleep(2)
for s in socks:
    try: s.close()
    except OSError: pass
print("done", flush=True)
