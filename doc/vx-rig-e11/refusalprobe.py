# E11 rig: drive the CA server past its admission wall and count the refusals.
#
# E8's wallprobe2.py answered "where is the wall".  This one answers "is the
# refusal reported faithfully", which needs two things wallprobe2 did not do:
#
#   * a run of refusals whose length is NOT a power of two, and whose ordinals
#     are consecutive.  The pre-fix server announced refusal #n on errlog only
#     when n was a power of two, and both E8 runs happened to stop at 4 and 8
#     refusals, so the last ordinal was announced by coincidence and only the
#     interior ones (#3, #5, #6, #7) were missing.  REFUSALS=8 forces the whole
#     1..8 run through one console.
#   * the refusal frame kept verbatim per attempt, so the diagnostic string can
#     be read off the wire rather than off the server's own console.
#
# Ports are the E11 block: CA on 55064.  Never uses another panel's block.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 55064
CEILING = int(sys.argv[1]) if len(sys.argv) > 1 else 200
REFUSALS = int(sys.argv[2]) if len(sys.argv) > 2 else 8
TAG = sys.argv[3] if len(sys.argv) > 3 else "e11"
MAXSEC = float(sys.argv[4]) if len(sys.argv) > 4 else 900.0
RATE = 8

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


class Refused(RuntimeError):
    """A CA_PROTO_ERROR refusal, with the whole frame kept."""

    def __init__(self, frame, status, text):
        super().__init__("CA_PROTO_ERROR:%s" % text)
        self.frame = frame
        self.status = status
        self.text = text


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
                    frame = hdr(11, len(pl), dt, dc, _p1, p2) + pl
                    raise Refused(frame, p2, pl[16:].split(b"\0")[0].decode("latin1"))
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
    if isinstance(exc, Refused):
        return "REFUSED_BY_SERVER(status=%d text=%r)" % (exc.status, exc.text)
    if isinstance(exc, OSError) and exc.errno is not None:
        return "CONNECT_FAIL(errno=%d %s)" % (exc.errno, E.errorcode.get(exc.errno, "?"))
    s = str(exc)
    if "EOF-after-accept" in s:
        return "ACCEPTED_THEN_EOF"
    if "timeout" in s:
        return "HANDSHAKE_TIMEOUT"
    return "OTHER(%s: %s)" % (type(exc).__name__, s)


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
_ioid = [4000]


def sample(tag):
    v = {}
    for pv, c in mon.items():
        _ioid[0] += 1
        try:
            v[pv] = c.read(ioid=_ioid[0])
        except Exception as e:
            v[pv] = "ERR:%s" % classify(e)
    log("SAMPLE %-14s CONN_CNT=%s REFUSED=%s FD_CNT=%s FD_MAX=%s MEM_USED=%s"
        % (tag, v.get("RTEMS:CA_CONN_CNT"), v.get("RTEMS:CA_REFUSED_CNT"),
           v.get("RTEMS:FD_CNT"), v.get("RTEMS:FD_MAX"), v.get("RTEMS:MEM_USED")))
    return v


log("monitors=%d ceiling=%d refusals_wanted=%d deadline=%.0fs" % (NMON, CEILING, REFUSALS, MAXSEC))
sample("idle")

# Phase 1: climb until the server refuses once.  That first refusal is the wall.
held, refusals = [], []
deadline_hit = False
while len(held) < CEILING:
    if time.time() - T0 > MAXSEC:
        deadline_hit = True
        log("INTERNAL DEADLINE %.0fs at held=%d -- stopping the climb" % (MAXSEC, len(held)))
        break
    t = time.time()
    try:
        held.append(Chan("RTEMS:AO", 1 + len(held)))
        n = len(held)
        if n % RATE == 0:
            log("UP held=%d total=%d last_conn=%.2fs" % (n, n + NMON, time.time() - t))
    except Exception as e:
        refusals.append(e)
        log("WALL attempt=%d held=%d elapsed_conn=%.2fs %s"
            % (len(held) + 1, len(held), time.time() - t, classify(e)))
        break

if not refusals:
    log("NO WALL REACHED -- nothing to say about refusal reporting")
else:
    # Phase 2: keep the wall loaded and ask again, REFUSALS-1 more times, so the
    # server emits one consecutive run of refusal ordinals.
    while len(refusals) < REFUSALS:
        if time.time() - T0 > MAXSEC:
            deadline_hit = True
            log("INTERNAL DEADLINE during the refusal burst at %d refusals" % len(refusals))
            break
        t = time.time()
        try:
            c = Chan("RTEMS:AO", 5000 + len(refusals))
            held.append(c)
            log("UNEXPECTED ADMIT during the refusal burst at refusal=%d (capacity freed)"
                % (len(refusals) + 1))
        except Exception as e:
            refusals.append(e)
            log("REFUSAL %d/%d elapsed_conn=%.2fs %s"
                % (len(refusals), REFUSALS, time.time() - t, classify(e)))

log("=== TOP ===")
log("D1 client-side served = %d ramp + %d monitor = %d" % (len(held), NMON, len(held) + NMON))
log("refusals observed client-side = %d" % len(refusals))
for i, e in enumerate(refusals, 1):
    if isinstance(e, Refused):
        log("refusal %d frame status=%d len=%d text=%r" % (i, e.status, len(e.frame), e.text))
        log("refusal %d hex=%s" % (i, e.frame.hex()))
    else:
        log("refusal %d NOT A PROTOCOL REFUSAL: %s" % (i, classify(e)))
sample("top")

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

for c in held:
    try:
        c.sock.close()
    except Exception:
        pass
log("released %d ramp connections" % len(held))
for c in mon.values():
    try:
        c.sock.close()
    except Exception:
        pass
log("refusalprobe done deadline_hit=%s" % deadline_hit)
