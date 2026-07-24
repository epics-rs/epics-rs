"""Fourth pass: establish all 4 connections FIRST (a saturating read loop on
one connection starves the CAS-TCP accept thread, which is itself a priority
observation), then apply a throttled load so CAS-client/CAS-event accumulate
CPU and rank into `rt top`'s CPU-ordered rows."""
import socket, struct, threading, time, os
HOST, PORT = "127.0.0.1", 5164
FIFO = os.path.expanduser("~/rtems-cside/ciocin")
N = 4
def hdr(cmd, payload=b"", dtype=0, dcount=0, p1=0, p2=0):
    payload = payload + b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, dcount, p1, p2) + payload
HELLO = hdr(0, dtype=0, dcount=13) + hdr(20, b"probe\0") + hdr(21, b"probe\0")

def create(s, pv, cid):
    s.sendall(hdr(18, pv.encode() + b"\0", p1=cid, p2=13))
    buf = b""
    t0 = time.time()
    while time.time() - t0 < 8:
        buf += s.recv(4096)
        while len(buf) >= 16:
            cmd, psz, dt, dc, p1, p2 = struct.unpack(">HHHHII", buf[:16])
            if len(buf) < 16 + psz:
                break
            buf = buf[16 + psz:]
            if cmd == 18:
                return dt, dc, p2
    raise RuntimeError("no create-chan reply for " + pv)

socks, chans = [], []
for i in range(1, N + 1):
    s = socket.create_connection((HOST, PORT), timeout=10)
    s.settimeout(10)
    s.sendall(HELLO); s.recv(16)
    dt_hb, dc_hb, sid_hb = create(s, "CIOC:HB100", 5000 + i)
    dt_ai, dc_ai, sid_ai = create(s, "CIOC:AI1", 6000 + i)
    body = struct.pack(">fffHH", 0.0, 0.0, 0.0, 7, 0)   # DBE_VALUE|LOG|ALARM
    s.sendall(hdr(1, body, dtype=dt_hb, dcount=dc_hb, p1=sid_hb, p2=7000 + i))
    socks.append(s); chans.append((sid_ai, dt_ai, dc_ai))
    print("  conn %d up (monitor on CIOC:HB100 @10Hz)" % i, flush=True)

stop = threading.Event()
def churn(s, sid, dt, dc, base):
    ioid = base
    while not stop.is_set():
        try:
            ioid += 1
            s.sendall(hdr(15, dtype=dt, dcount=dc, p1=sid, p2=ioid))
            s.recv(65536)
        except OSError:
            return
        time.sleep(0.02)          # throttled: ~50 reads/s/connection

for i, s in enumerate(socks):
    sid, dt, dc = chans[i]
    threading.Thread(target=churn, args=(s, sid, dt, dc, 9000 + 1000 * i),
                     daemon=True).start()
print("4 busy connections; ~50 reads/s + 10 Hz monitor each", flush=True)

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

time.sleep(25)
w("rt top\n"); time.sleep(14)
w("\n"); time.sleep(6)
w("epicsThreadShowAll 1\n"); time.sleep(8)
w("casr 1\n"); time.sleep(5)
stop.set(); time.sleep(2)
for s in socks:
    try: s.close()
    except OSError: pass
print("done", flush=True)
