"""Final pass: 4 connections, each with 25 monitors on the 10 Hz record (so the
per-connection CAS-event thread earns CPU) plus a throttled read loop (so
CAS-client does).  Takes `rt stackuse` and `rt top` in the SAME state, so the
task-ID -> thread-name join is exact within one boot."""
import socket, struct, threading, time, os
HOST, PORT = "127.0.0.1", 5164
FIFO = os.path.expanduser("~/rtems-cside/ciocin")
N, NSUB = 4, 25
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
    socks.append(s); chans.append((sid_ai, dt_ai, dc_ai, sid_hb, dt_hb, dc_hb, i))
    print("  conn %d up" % i, flush=True)

stop = threading.Event()
def work(s, c):
    sid_ai, dt_ai, dc_ai, sid_hb, dt_hb, dc_hb, i = c
    body = struct.pack(">fffHH", 0.0, 0.0, 0.0, 7, 0)
    for k in range(NSUB):
        s.sendall(hdr(1, body, dtype=dt_hb, dcount=dc_hb, p1=sid_hb,
                      p2=100000 * i + k))
    ioid = 9000
    s.settimeout(0.05)
    t_last = 0.0
    while not stop.is_set():
        try:
            s.recv(262144)
        except socket.timeout:
            pass
        except OSError:
            return
        now = time.time()
        if now - t_last > 0.02:
            t_last = now
            ioid += 1
            try:
                s.sendall(hdr(15, dtype=dt_ai, dcount=dc_ai, p1=sid_ai, p2=ioid))
            except OSError:
                return

for s, c in zip(socks, chans):
    threading.Thread(target=work, args=(s, c), daemon=True).start()
print("4 connections x %d monitors @10Hz + ~50 reads/s" % NSUB, flush=True)

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

time.sleep(25)
w("rt stackuse\n"); time.sleep(12)
w("rt top\n"); time.sleep(14)
w("\n"); time.sleep(6)
w("epicsThreadShowAll 1\n"); time.sleep(8)
stop.set(); time.sleep(2)
for s in socks:
    try: s.close()
    except OSError: pass
print("done", flush=True)
