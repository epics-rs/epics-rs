# E8 addendum: ramp straight to the wall, no plateaus.
#
# Two uses, both on the same image poolramp.py ran:
#   (1) a SECOND ramp on a guest whose pool already holds its high-water mark.
#       The pool never shrinks, so a second concurrent ramp that reaches the
#       same count with SETS/WORKERS unchanged is the bounded-REUSE statement --
#       the concurrent counterpart of the RTEMS run's 30-cycle serial churn.
#   (2) the same probe on a guest booted with more memory, to discriminate
#       whether the EAGAIN wall is guest RAM or an RTP-internal limit.
#
# Holds the top for HOLD seconds so at least two POOLPROBE passes land there,
# then releases and holds again.  Deliberately does NOT dwell mid-ramp: the
# census is not what this probe is for.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
CEILING = int(sys.argv[1]) if len(sys.argv) > 1 else 120
HOLD = float(sys.argv[2]) if len(sys.argv) > 2 else 30.0
TAG = sys.argv[3] if len(sys.argv) > 3 else "wall"

T0 = time.time()
resource.setrlimit(resource.RLIMIT_NOFILE, (65536, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))


def log(m):
    print("[%7.1fs] %s %s" % (time.time() - T0, TAG, m), flush=True)


def hdr(cmd, psize, dtype, dcount, p1, p2):
    return struct.pack(">HHHHII", cmd, psize, dtype, dcount, p1, p2)


def pad(name):
    b = name.encode() + b"\0"
    return b.ljust((len(b) + 7) // 8 * 8, b"\0")


def msgs(buf):
    out = []
    while len(buf) >= 16:
        cmd, psize, dt, dc, p1, p2 = struct.unpack(">HHHHII", buf[:16])
        if len(buf) < 16 + psize:
            break
        out.append((cmd, psize, dt, dc, p1, p2, buf[16:16 + psize]))
        buf = buf[16 + psize:]
    return out, buf


class Chan:
    def __init__(self, pv, cid, timeout=15):
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sid = self.dtype = self.dcount = self.first = None
        nm = pad(pv)
        self.sock.sendall(hdr(0, 0, 0, 13, 0, 0))
        self.sock.sendall(hdr(6, len(nm), 10, 13, cid, cid) + nm)
        sent_create = False
        deadline = time.time() + timeout
        while time.time() < deadline:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("EOF-after-accept")
            self.buf += chunk
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, _p1, p2, pl) in ms:
                if cmd == 11:
                    raise RuntimeError("CA_PROTO_ERROR:%s|hex=%s"
                                       % (pl[16:].split(b"\0")[0].decode("latin1"),
                                          (hdr(11, len(pl), dt, dc, _p1, p2) + pl).hex()))
                if cmd == 6 and not sent_create:
                    self.sock.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
                    sent_create = True
                elif cmd == 18:
                    self.sid, self.dtype, self.dcount = p2, dt, dc
                    self.sock.sendall(hdr(15, 0, dt, dc, self.sid, cid))
                elif cmd == 15:
                    self.first = self.decode(dt, pl)
                    return
        raise RuntimeError("handshake-timeout")

    @staticmethod
    def decode(dt, pl):
        if dt == 6 and len(pl) >= 8:
            return struct.unpack(">d", pl[:8])[0]
        if dt == 5 and len(pl) >= 4:
            return struct.unpack(">i", pl[:4])[0]
        return pl[:16]

    def read(self, ioid, timeout=15):
        self.sock.settimeout(timeout)
        self.sock.sendall(hdr(15, 0, self.dtype, self.dcount, self.sid, ioid))
        deadline = time.time() + timeout
        while time.time() < deadline:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("EOF")
            self.buf += chunk
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, _dc, _p1, p2, pl) in ms:
                if cmd == 15 and p2 == ioid:
                    return self.decode(dt, pl)
        raise RuntimeError("read-timeout")


def classify(exc):
    if isinstance(exc, OSError) and exc.errno is not None:
        return "CONNECT_FAIL(errno=%d %s)" % (exc.errno, E.errorcode.get(exc.errno, "?"))
    s = str(exc)
    if "CA_PROTO_ERROR" in s:
        return "REFUSED_BY_SERVER(%s)" % s
    if "EOF-after-accept" in s:
        return "ACCEPTED_THEN_EOF"
    if "timeout" in s:
        return "HANDSHAKE_TIMEOUT"
    return "OTHER(%s: %s)" % (type(exc).__name__, s)


mon = {}
for pv in ("RTEMS:CA_CONN_CNT", "RTEMS:CA_REFUSED_CNT", "RTEMS:FD_CNT", "RTEMS:MEM_USED"):
    try:
        mon[pv] = Chan(pv, 900 + len(mon))
    except Exception as e:
        log("monitor %s UNAVAILABLE: %s" % (pv, classify(e)))
NMON = len(mon)
_ioid = [3000]


def sample(tag):
    v = {}
    for pv, c in mon.items():
        _ioid[0] += 1
        try:
            v[pv] = c.read(ioid=_ioid[0])
        except Exception as e:
            v[pv] = "ERR:%s" % classify(e)
    log("SAMPLE %-12s CONN_CNT=%s REFUSED=%s FD_CNT=%s MEM_USED=%s"
        % (tag, v.get("RTEMS:CA_CONN_CNT"), v.get("RTEMS:CA_REFUSED_CNT"),
           v.get("RTEMS:FD_CNT"), v.get("RTEMS:MEM_USED")))
    return v


log("monitors=%d" % NMON)
sample("idle")
held, failures, consec = [], [], 0
while len(held) < CEILING and consec < 4:
    try:
        held.append(Chan("RTEMS:AO", 1 + len(held)))
        consec = 0
    except Exception as e:
        consec += 1
        cls = classify(e)
        failures.append((len(held) + 1, cls))
        log("FAIL attempt=%d held=%d total=%d %s"
            % (len(held) + 1, len(held), len(held) + NMON, cls))

log("TOP ramp=%d monitors=%d total=%d" % (len(held), NMON, len(held) + NMON))
if failures:
    log("first failure at ramp attempt %d" % failures[0][0])
    log("first failure verbatim: %s" % failures[0][1])
log("holding %.0fs" % HOLD)
time.sleep(HOLD)
sample("top")
for c in held:
    try:
        c.sock.close()
    except Exception:
        pass
log("released %d, holding %.0fs" % (len(held), HOLD))
time.sleep(HOLD)
sample("released")
for c in mon.values():
    try:
        c.sock.close()
    except Exception:
        pass
log("wallprobe done")
