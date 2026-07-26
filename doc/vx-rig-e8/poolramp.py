# E8: hold N concurrent CA clients against the VxWorks guest at a series of
# plateaus, so POOLPROBE / PRIOPROBE / STACKUSE can be read at each one.
#
# WHY PLATEAUS AND NOT A STRAIGHT RAMP.  The image's own reporter prints
# FDPROBE/POOLPROBE every 10 s and the task + stack census every 6th pass
# (~60 s, realtime-ca-ioc.rs:776).  A monotone ramp therefore gives at most one
# census at a connection count nobody chose.  Each plateau here dwells longer
# than the census period, so every plateau is crossed by at least one full
# STACKUSE block at a known, held connection count.
#
# The Chan handshake is ~/vx-bringup/ftp/caramp.py's, unchanged: VERSION ->
# SEARCH -> CREATE_CHAN -> READ_NOTIFY with a decoded data reply, so "served"
# means the client got a value, not merely that accept() happened.
#
# Two independent derivations of the served count, as the RTEMS run had:
#   D1 client-side: handshakes completed and still open.
#   D2 server-side: RTEMS:CA_CONN_CNT read over CA from monitor connections
#      opened FIRST and held.
# The monitor connections are themselves clients and are counted separately, so
# D1 + NMON is what D2 is compared against.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
PLATEAUS = [1, 2, 4, 8, 16, 24, 32, 40]
DWELL = float(sys.argv[1]) if len(sys.argv) > 1 else 75.0
CEILING = int(sys.argv[2]) if len(sys.argv) > 2 else 120

T0 = time.time()

soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
resource.setrlimit(resource.RLIMIT_NOFILE, (min(hard, 65536), hard))


def log(msg):
    print("[%7.1fs] %s" % (time.time() - T0, msg), flush=True)


log("client RLIMIT_NOFILE %s -> %s (hard %s)"
    % (soft, resource.getrlimit(resource.RLIMIT_NOFILE)[0], hard))


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
    """One held CA connection with one channel, handshake completed."""

    def __init__(self, pv, cid, timeout=15):
        self.pv = pv
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sid = None
        self.dtype = None
        self.dcount = None
        self.first = None
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
            for (cmd, _ps, dt, dc, p1, p2, pl) in ms:
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


# ---- monitor connections first, so they survive the wall ----
MONPV = ("RTEMS:CA_CONN_CNT", "RTEMS:CA_REFUSED_CNT", "RTEMS:FD_CNT",
         "RTEMS:FD_MAX", "RTEMS:MEM_USED")
mon = {}
for pv in MONPV:
    try:
        mon[pv] = Chan(pv, 900 + len(mon))
        log("monitor %-22s initial=%r" % (pv, mon[pv].first))
    except Exception as e:
        log("monitor %-22s UNAVAILABLE: %s" % (pv, classify(e)))
NMON = len(mon)
log("held monitor connections = %d (they count toward the server's total)" % NMON)

_ioid = [2000]


def sample(tag):
    vals = {}
    for pv, c in mon.items():
        _ioid[0] += 1
        try:
            vals[pv] = c.read(ioid=_ioid[0])
        except Exception as e:
            vals[pv] = "ERR:%s" % classify(e)
    log("SAMPLE %-16s CONN_CNT=%s REFUSED=%s FD_CNT=%s FD_MAX=%s MEM_USED=%s"
        % (tag, vals.get("RTEMS:CA_CONN_CNT"), vals.get("RTEMS:CA_REFUSED_CNT"),
           vals.get("RTEMS:FD_CNT"), vals.get("RTEMS:FD_MAX"),
           vals.get("RTEMS:MEM_USED")))
    return vals


held = []
failures = []


def grow_to(n):
    """Open ramp connections until len(held) == n. Returns True if reached."""
    while len(held) < n:
        i = len(held)
        try:
            held.append(Chan("RTEMS:AO", 1 + i))
        except Exception as e:
            cls = classify(e)
            failures.append((i + 1, cls, time.time() - T0))
            log("WALL attempt=%d (ramp #%d) FAILED: %s  held=%d total=%d"
                % (i + 1, i + 1, cls, len(held), len(held) + NMON))
            return False
    return True


sample("boot-idle")
log("=== plateaus %s, dwell %.0fs each, then ramp to %d ==="
    % (PLATEAUS, DWELL, CEILING))

reached_wall = False
for target in PLATEAUS:
    if not grow_to(target):
        reached_wall = True
        break
    log("PLATEAU n=%d ramp=%d total=%d -- dwelling %.0fs for a census pass"
        % (target, len(held), len(held) + NMON, DWELL))
    sample("plateau=%d-enter" % target)
    time.sleep(DWELL)
    sample("plateau=%d-exit" % target)

if not reached_wall:
    log("=== plateaus done, ramping to the wall (ceiling %d) ===" % CEILING)
    consec = 0
    while len(held) < CEILING:
        before = len(held)
        if grow_to(before + 1):
            consec = 0
            if len(held) % 5 == 0:
                sample("ramp=%d" % len(held))
        else:
            consec += 1
            if consec >= 4:
                log("4 consecutive failures -- wall reached")
                reached_wall = True
                break

log("=== WALL / TOP OF RAMP ===")
log("D1 client-side served = %d ramp + %d monitor = %d"
    % (len(held), NMON, len(held) + NMON))
final = sample("top")
log("D2 server-side RTEMS:CA_CONN_CNT = %s" % final.get("RTEMS:CA_CONN_CNT"))
log("   server-side RTEMS:CA_REFUSED_CNT = %s" % final.get("RTEMS:CA_REFUSED_CNT"))
if failures:
    seen = {}
    for _i, c, _t in failures:
        seen[c] = seen.get(c, 0) + 1
    log("failure classes:")
    for c, n in sorted(seen.items(), key=lambda kv: -kv[1]):
        log("   %4d x %s" % (n, c))
    log("first failure at attempt %d, t=%.1fs" % (failures[0][0], failures[0][2]))
else:
    log("NO WALL REACHED within ceiling %d" % CEILING)

alive = 0
spot = held[:5] + held[-5:]
for c in spot:
    _ioid[0] += 1
    try:
        c.read(ioid=_ioid[0])
        alive += 1
    except Exception:
        pass
log("spot-check: %d/%d sampled held connections still answer a fresh READ_NOTIFY"
    % (alive, len(spot)))

log("holding the top for %.0fs so the census can be read there" % DWELL)
time.sleep(DWELL)
sample("top-held")

log("=== releasing every ramp connection ===")
for c in held:
    try:
        c.sock.close()
    except Exception:
        pass
log("released %d; holding %.0fs so the census shows what the pool retains" % (len(held), DWELL))
time.sleep(DWELL)
sample("released")
for c in mon.values():
    try:
        c.sock.close()
    except Exception:
        pass
log("poolramp: done")
