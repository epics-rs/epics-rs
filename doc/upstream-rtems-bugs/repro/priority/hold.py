"""Hold N CA connections against the C IOC on 5164 so that the per-connection
CAS-client / CAS-event threads exist, then drive iocsh through the fifo and
quote the listings.  Connect method identical to ceiling.py (raw CA sockets,
version handshake, one CREATE_CHAN per connection)."""
import socket, struct, time, sys, os

HOST, PORT = "127.0.0.1", 5164
PV = b"CIOC:AO\0"
N = int(sys.argv[1]) if len(sys.argv) > 1 else 4
FIFO = os.path.expanduser("~/rtems-cside/ciocin")


def hdr(cmd, payload=b"", dtype=0, dcount=0, p1=0, p2=0):
    payload = payload + b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, dcount, p1, p2) + payload


HELLO = hdr(0, dtype=0, dcount=13) + hdr(20, b"probe\0") + hdr(21, b"probe\0")


def served(s, cid):
    s.settimeout(5)
    s.sendall(hdr(18, PV, p1=cid, p2=13))
    buf = b""
    t0 = time.time()
    while time.time() - t0 < 5:
        try:
            d = s.recv(4096)
        except socket.timeout:
            return "TIMEOUT"
        if not d:
            return "CLOSED"
        buf += d
        while len(buf) >= 16:
            cmd, psz, dt, dc, p1, p2 = struct.unpack(">HHHHII", buf[:16])
            if len(buf) < 16 + psz:
                break
            buf = buf[16 + psz:]
            if cmd == 18:
                return "OK"
            if cmd == 26:
                return "FAIL"
    return "TIMEOUT"


def ioc(cmd, wait=4):
    with open(FIFO, "w") as f:
        f.write(cmd + "\n")
    time.sleep(wait)


socks = []
for i in range(1, N + 1):
    s = socket.create_connection((HOST, PORT), timeout=8)
    s.settimeout(8)
    s.sendall(HELLO)
    s.recv(16)
    socks.append(s)
    r = served(s, 3000 + i)
    print("  conn %d: %s" % (i, r), flush=True)

time.sleep(3)
print("holding %d connections; driving iocsh" % len(socks), flush=True)
ioc("casr 1", 5)
ioc("epicsThreadShowAll 1", 8)
ioc("rt stackuse", 10)
# top is interactive: ENTER exits it
with open(FIFO, "w") as f:
    f.write("rt top\n")
time.sleep(8)
with open(FIFO, "w") as f:
    f.write("\n")
time.sleep(6)
ioc("epicsThreadShowAll 1", 8)
print("done, closing", flush=True)
for s in socks:
    try:
        s.close()
    except OSError:
        pass
