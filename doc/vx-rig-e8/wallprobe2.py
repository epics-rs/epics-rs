# E8 addendum, second attempt: ramp to the wall with the ramp itself logged.
#
# wallprobe.py logged only failures, so a ramp that keeps succeeding prints
# nothing; the first 2048M attempt was killed by an external `timeout 400` at
# ~141 connections and left no record of its own progress.  This version:
#   * logs every RATE-th success, so the ramp is visible while it runs;
#   * carries its own deadline (MAXSEC) and always falls through to the
#     summary, so no external timeout can decapitate the reading;
#   * writes the whole transcript unfiltered -- the refusal frame hexdump is
#     part of the evidence, not noise to be grepped away.
#
# Purpose: discriminate the 1024M EAGAIN wall (47 concurrent sets) between
# guest RAM and an RTP-internal limit, and -- if RAM is the answer -- find what
# the wall becomes with the RAM doubled, including whether the pool's own
# CAS_CLIENT_POOL_CAPACITY = 141 is reached and what it does at 142.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
CEILING = int(sys.argv[1]) if len(sys.argv) > 1 else 200
HOLD = float(sys.argv[2]) if len(sys.argv) > 2 else 90.0
TAG = sys.argv[3] if len(sys.argv) > 3 else "wall2"
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


log("monitors=%d ceiling=%d hold=%.0fs deadline=%.0fs" % (NMON, CEILING, HOLD, MAXSEC))
sample("idle")

held, failures, consec = [], [], 0
deadline_hit = False
while len(held) < CEILING and consec < 4:
    if time.time() - T0 > MAXSEC:
        deadline_hit = True
        log("INTERNAL DEADLINE %.0fs reached at held=%d -- stopping the ramp" % (MAXSEC, len(held)))
        break
    t = time.time()
    try:
        held.append(Chan("RTEMS:AO", 1 + len(held)))
        consec = 0
        n = len(held)
        if n % RATE == 0:
            log("UP held=%d total=%d last_conn=%.2fs" % (n, n + NMON, time.time() - t))
        if n % 32 == 0:
            sample("held=%d" % n)
    except Exception as e:
        consec += 1
        cls = classify(e)
        failures.append((len(held) + 1, cls, time.time() - T0))
        log("FAIL attempt=%d held=%d total=%d elapsed_conn=%.2fs %s"
            % (len(held) + 1, len(held), len(held) + NMON, time.time() - t, cls))

log("=== TOP ===")
log("D1 client-side served = %d ramp + %d monitor = %d" % (len(held), NMON, len(held) + NMON))
log("deadline_hit=%s consecutive_failures=%d ceiling_hit=%s"
    % (deadline_hit, consec, len(held) >= CEILING))
top = sample("top")
log("D2 server-side RTEMS:CA_CONN_CNT = %s" % top.get("RTEMS:CA_CONN_CNT"))
if failures:
    log("first failure at ramp attempt %d, t=%.1fs" % (failures[0][0], failures[0][2]))
    log("first failure verbatim: %s" % failures[0][1])
    seen = {}
    for _i, c, _t in failures:
        seen[c] = seen.get(c, 0) + 1
    for c, n in sorted(seen.items(), key=lambda kv: -kv[1]):
        log("failure class %4d x %s" % (n, c))
else:
    log("NO FAILURE OBSERVED (ramp ended by %s)"
        % ("internal deadline" if deadline_hit else "ceiling"))

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

log("holding the top %.0fs so a full census pass lands there" % HOLD)
time.sleep(HOLD)
sample("top-held")
for c in held:
    try:
        c.sock.close()
    except Exception:
        pass
log("released %d ramp connections, holding %.0fs" % (len(held), HOLD))
time.sleep(HOLD)
sample("released")
for c in mon.values():
    try:
        c.sock.close()
    except Exception:
        pass
log("wallprobe2 done")
