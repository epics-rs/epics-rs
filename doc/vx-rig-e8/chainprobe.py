# E8 follow-up: pin the FLNK-chain depth boundary and time WRITE_NOTIFY on it.
#
# Two questions stackload.py left open.
#
#   (a) How deep does the chain actually run?  stackload.py sampled L1..L5, L8,
#       L16, L24, L32 and saw L8 propagate but L16 not, which brackets the cap
#       to (9, 15].  `MAX_LINK_DEPTH = 16` in
#       crates/epics-base-rs/src/server/database/processing.rs predicts H at
#       depth 0 .. L15 at depth 15 all process, and L16 bail.  This reads every
#       link L1..L18 so the boundary is a measurement, not an inference.  It
#       matters for StackSizeClass: the CAS-client high-water is only
#       depth-inclusive if the recursion really reached its cap.
#
#   (b) stackload.py reported `put_chain TIMEOUT` while `put_fan` succeeded on
#       the same WRITE_NOTIFY mechanism.  This times a WRITE_NOTIFY and a plain
#       WRITE to the same record separately, so a slow chain can be told from a
#       notify that never comes.
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
TAG = sys.argv[1] if len(sys.argv) > 1 else "chain"
BASE = float(sys.argv[2]) if len(sys.argv) > 2 else 200.0
SETTLE = float(sys.argv[3]) if len(sys.argv) > 3 else 3.0

T0 = time.time()
DBR_DOUBLE = 6


def log(m):
    print("[%7.1fs] %s %s" % (time.time() - T0, TAG, m), flush=True)


def hdr(cmd, dtype, count, p1, p2, payload=b""):
    n = len(payload)
    if n >= 0xFFFF or count >= 0xFFFF:
        return (struct.pack(">HHHHII", cmd, 0xFFFF, dtype, 0, p1, p2)
                + struct.pack(">II", n, count) + payload)
    return struct.pack(">HHHHII", cmd, n, dtype, count, p1, p2) + payload


def msgs(buf):
    out = []
    while len(buf) >= 16:
        cmd, psize, dt, dc, p1, p2 = struct.unpack(">HHHHII", buf[:16])
        if psize == 0xFFFF and dc == 0:
            if len(buf) < 24:
                break
            psize, dc = struct.unpack(">II", buf[16:24])
            head = 24
        else:
            head = 16
        if len(buf) < head + psize:
            break
        out.append((cmd, dt, dc, p1, p2, buf[head:head + psize]))
        buf = buf[head + psize:]
    return out, buf


def pad(name):
    b = name.encode() + b"\0"
    return b.ljust((len(b) + 7) // 8 * 8, b"\0")


class Conn:
    def __init__(self, timeout=45):
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sock.sendall(hdr(0, 0, 13, 0, 0))
        self.cid = 0
        self.timeout = timeout

    def pump(self, want, key=None, budget=None):
        deadline = time.time() + (budget or self.timeout)
        while time.time() < deadline:
            ms, self.buf = msgs(self.buf)
            for m in ms:
                if m[0] == 11:
                    raise RuntimeError("CA_PROTO_ERROR:%s"
                                       % m[5][16:].split(b"\0")[0].decode("latin1"))
                if m[0] == want and (key is None or m[4] == key):
                    return m
            self.sock.settimeout(max(0.05, deadline - time.time()))
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise RuntimeError("EOF")
            self.buf += chunk
        raise RuntimeError("timeout waiting for command %d" % want)

    def channel(self, pv):
        self.cid += 1
        cid = self.cid
        nm = pad(pv)
        self.sock.sendall(hdr(6, 10, 13, cid, cid, nm))
        self.pump(6)
        self.sock.sendall(hdr(18, 0, 0, cid, 13, nm))
        m = self.pump(18)
        return (m[4], m[1], m[2], cid)

    def get1(self, ch):
        sid, _dt, _n, cid = ch
        ioid = 0x4000 + cid
        self.sock.sendall(hdr(15, DBR_DOUBLE, 1, sid, ioid))
        m = self.pump(15, key=ioid)
        return struct.unpack(">d", m[5][:8])[0]

    def put1(self, ch, v, notify, budget):
        sid, _dt, _n, cid = ch
        ioid = 0x5000 + cid
        t = time.time()
        self.sock.sendall(hdr(19 if notify else 4, DBR_DOUBLE, 1, sid,
                              ioid, struct.pack(">d", v)))
        if notify:
            self.pump(19, key=ioid, budget=budget)
        return time.time() - t


c = Conn()
LINKS = ["RTEMS:E8:L%d" % i for i in range(1, 19)]
ch = {"H": c.channel("RTEMS:E8:H"), "FAN": c.channel("RTEMS:E8:FAN")}
for pv in LINKS:
    try:
        ch[pv] = c.channel(pv)
    except Exception as e:
        log("channel %-14s UNAVAILABLE: %s" % (pv, e))
for i in range(1, 9):
    pv = "RTEMS:E8:F%d" % i
    try:
        ch[pv] = c.channel(pv)
    except Exception as e:
        log("channel %-14s UNAVAILABLE: %s" % (pv, e))
log("channels=%d" % len(ch))

# (b) notify vs plain, same record, timed separately.
for notify in (True, False):
    v = BASE + (0.0 if notify else 1000.0)
    try:
        dt = c.put1(ch["H"], v, notify, budget=25.0)
        log("put H=%.1f notify=%s  elapsed=%.3fs  OK" % (v, notify, dt))
    except Exception as e:
        log("put H=%.1f notify=%s  FAILED after %.3fs: %s"
            % (v, notify, time.time() - T0, e))
    time.sleep(SETTLE)

try:
    dt = c.put1(ch["FAN"], BASE + 7.0, True, budget=25.0)
    log("put FAN=%.1f notify=True elapsed=%.3fs OK" % (BASE + 7.0, dt))
except Exception as e:
    log("put FAN notify=True FAILED: %s" % e)
time.sleep(SETTLE)

# (a) the boundary, every link.
try:
    log("H          = %.1f" % c.get1(ch["H"]))
except Exception as e:
    log("H read FAILED: %s" % e)
for i, pv in enumerate(LINKS, start=1):
    if pv not in ch:
        continue
    try:
        log("L%-2d depth=%-2d = %.1f" % (i, i, c.get1(ch[pv])))
    except Exception as e:
        log("L%-2d read FAILED: %s" % (i, e))
for i in range(1, 9):
    pv = "RTEMS:E8:F%d" % i
    if pv not in ch:
        continue
    try:
        log("F%-2d        = %.1f" % (i, c.get1(ch[pv])))
    except Exception as e:
        log("F%-2d read FAILED: %s" % (i, e))

c.sock.close()
log("chainprobe done")
