# E8 item 4: does the CA client pool give anything back when clients leave?
#
# The pool is documented as never shrinking, so N concurrent clients cost N*2
# threads for the life of the process.  C rsrv's shape is the reference and it
# is the opposite -- epicsThreadCreate("CAS-client") per accept
# (caservertask.c:109), db_close_events + task exit per disconnect
# (destroy_tcp_client), so C's steady-state retention after a burst is zero.
#
# This measures ours rather than arguing it from the source: sample the
# server's own counters at idle, ramp to a burst, sample at the top, drop every
# ramp connection, wait, then sample again on FRESH monitor connections.  Three
# numbers -- idle / top / after -- say how much of the burst is handed back.
# POOLPROBE on the console tells the same story in sets and workers.
#
# Self-contained on purpose: phaseramp.py runs its ramp at module level, so
# importing it would fire a second ramp underneath this one.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
BURST = int(sys.argv[1]) if len(sys.argv) > 1 else 40
SETTLE = float(sys.argv[2]) if len(sys.argv) > 2 else 45.0
TAG = sys.argv[3] if len(sys.argv) > 3 else "retention"
# Hold the burst before sampling the top.  The status PVs are driven by a scan,
# so a burst that completes in under a second is sampled through pre-burst
# values and the retention figure comes out a meaningless zero -- measured on
# the first run of this probe.
HOLD = float(sys.argv[4]) if len(sys.argv) > 4 else 60.0

T0 = time.time()
resource.setrlimit(resource.RLIMIT_NOFILE, (65536, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))
MONPV = ("RTEMS:CA_CONN_CNT", "RTEMS:CA_REFUSED_CNT", "RTEMS:FD_CNT", "RTEMS:MEM_USED")


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


def decode(dt, pl):
    if dt == 6 and len(pl) >= 8:
        return struct.unpack(">d", pl[:8])[0]
    if dt == 5 and len(pl) >= 4:
        return struct.unpack(">i", pl[:4])[0]
    return pl[:16]


class Chan:
    def __init__(self, pv, cid, timeout=20):
        nm = pad(pv)
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sid = self.dtype = self.dcount = None
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
                    raise RuntimeError("CA_PROTO_ERROR:%s"
                                       % pl[16:].split(b"\0")[0].decode("latin1"))
                if cmd == 6 and not sent_create:
                    self.sock.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
                    sent_create = True
                elif cmd == 18:
                    self.sid, self.dtype, self.dcount = p2, dt, dc
                    self.sock.sendall(hdr(15, 0, dt, dc, self.sid, cid))
                elif cmd == 15:
                    return
        raise RuntimeError("handshake-timeout")

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
                    return decode(dt, pl)
        raise RuntimeError("read-timeout")


def classify(exc):
    if isinstance(exc, OSError) and exc.errno is not None:
        return "CONNECT_FAIL(errno=%d %s)" % (exc.errno, E.errorcode.get(exc.errno, "?"))
    s = str(exc)
    if "CA_PROTO_ERROR" in s:
        return "REFUSED_BY_SERVER(%s)" % s
    if "EOF-after-accept" in s:
        return "ACCEPTED_THEN_EOF"
    if "timeout" in s or isinstance(exc, TimeoutError):
        return "HANDSHAKE_TIMEOUT"
    return "OTHER(%s: %s)" % (type(exc).__name__, s)


_ioid = [7000]


def sample(label):
    """Fresh monitor connections each time, so no reading depends on a socket
    that was itself part of the burst."""
    mon = {}
    for pv in MONPV:
        try:
            mon[pv] = Chan(pv, 900 + len(mon))
        except Exception as e:
            log("monitor %s UNAVAILABLE: %s" % (pv, classify(e)))
    v = {}
    for pv, c in mon.items():
        _ioid[0] += 1
        try:
            v[pv] = c.read(ioid=_ioid[0])
        except Exception as e:
            v[pv] = "ERR:%s" % classify(e)
    log("%-9s CONN_CNT=%s REFUSED=%s FD_CNT=%s MEM_USED=%s"
        % (label, v.get("RTEMS:CA_CONN_CNT"), v.get("RTEMS:CA_REFUSED_CNT"),
           v.get("RTEMS:FD_CNT"), v.get("RTEMS:MEM_USED")))
    for c in mon.values():
        try:
            c.sock.close()
        except Exception:
            pass
    return v


idle = sample("idle")

held = []
for i in range(BURST):
    try:
        held.append(Chan("RTEMS:AO", 1 + i))
    except Exception as e:
        log("burst stopped at %d: %s" % (i + 1, classify(e)))
        break
log("burst held=%d; holding %.0fs so the scan refreshes the status PVs" % (len(held), HOLD))
time.sleep(HOLD)
top = sample("top")

for c in held:
    try:
        c.sock.close()
    except Exception:
        pass
log("dropped all %d ramp connections; settling %.0fs" % (len(held), SETTLE))
time.sleep(SETTLE)
after = sample("after")

mi, mt, ma = (x.get("RTEMS:MEM_USED") for x in (idle, top, after))
if all(isinstance(x, float) for x in (mi, mt, ma)):
    grew, kept = mt - mi, ma - mi
    log("MEM_USED idle=%.0f top=%.0f after=%.0f" % (mi, mt, ma))
    log("burst grew %.0f B; %.0f B still held after all %d clients left (%.1f%% retained)"
        % (grew, kept, len(held), 100.0 * kept / grew if grew else float("nan")))
else:
    log("MEM_USED unavailable in at least one sample; no retention figure")
log("retention done")
