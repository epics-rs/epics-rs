# E11 heap decomposition driver: climb the CA admission ramp one connection per
# second and stop at the first refusal, abort, or ceiling.
#
# The protocol layer (CA_PROTO_VERSION / CREATE_CHAN / READ_NOTIFY framing, the
# Refused carrier, classify()) is taken from the refusal-fidelity panel's
# doc/vx-rig-e11/refusalprobe.py on branch
# caucus/58EWEJWV91/refusal-fidelity-494e4108-1 — read with `git show`, never
# merged.  Two things are deliberately different here, because this rig answers
# a different question:
#
#   * PORT is 25064, this panel's block, not 55064.
#   * the ramp is paced at PACE seconds per connection instead of running flat
#     out at ~0.03 s.  The IOC under measurement dumps its whole live-block
#     table once a second; a flat-out ramp puts 30 connections inside one dump
#     interval and the per-connection decomposition is then unrecoverable.
#     Every attempt is timestamped so the pacing can be divided back out.
#
# It also prints an ATTEMPT line for every single connection rather than every
# 8th: the failure under measurement is an abort of the whole IOC, so the last
# line before the console goes quiet is the datum.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25064
CEILING = int(sys.argv[1]) if len(sys.argv) > 1 else 200
TAG = sys.argv[2] if len(sys.argv) > 2 else "e11"
MAXSEC = float(sys.argv[3]) if len(sys.argv) > 3 else 900.0
PACE = float(sys.argv[4]) if len(sys.argv) > 4 else 1.0

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


log("monitors=%d ceiling=%d pace=%.2fs deadline=%.0fs port=%d"
    % (NMON, CEILING, PACE, MAXSEC, PORT))
sample("idle")

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
        log("ATTEMPT %3d OK held=%d total=%d conn=%.2fs"
            % (len(held), len(held), len(held) + NMON, time.time() - t))
    except Exception as e:
        refusals.append(e)
        log("WALL attempt=%d held=%d elapsed_conn=%.2fs %s"
            % (len(held) + 1, len(held), time.time() - t, classify(e)))
        break
    slack = PACE - (time.time() - t)
    if slack > 0:
        time.sleep(slack)

log("=== TOP ===")
log("client-side served = %d ramp + %d monitor = %d" % (len(held), NMON, len(held) + NMON))
for i, e in enumerate(refusals, 1):
    if isinstance(e, Refused):
        log("refusal %d frame status=%d len=%d text=%r" % (i, e.status, len(e.frame), e.text))
        log("refusal %d hex=%s" % (i, e.frame.hex()))
    else:
        log("refusal %d NOT A PROTOCOL REFUSAL: %s" % (i, classify(e)))
sample("top")

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
log("rampprobe done deadline_hit=%s" % deadline_hit)
