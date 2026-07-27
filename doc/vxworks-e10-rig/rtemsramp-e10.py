# rtemsramp-e10.py — the armv7-rtems thread/heap ladder: add one CA client at a
# time and read the whole heap surface after each, until the wall.
#
# The VxWorks answer does NOT transfer.  There the binding resource was RESERVED
# ADDRESS SPACE (thread = declared stack + ~1 MiB), no RTP query tracked it, and
# the wall moved linearly with guest RAM.  RTEMS is a single address space with
# one protected malloc heap, so the candidate constraint here is the heap
# itself, and the heap is queryable from inside the IOC — which is why every
# sample below is a real reading rather than an inference.
#
# What each client costs by declaration: `CAS-client` is `StackSizeClass::Big`
# and `CAS-event` is `Medium`, and on a 32-bit target `bytes()` is
# `f * 0x10000 * 4`, so 1,048,576 + 524,288 = 1,572,864 B of declared stack per
# client.  If the wall is heap-bound and linear in declared stack, MEM_USED must
# climb by about that per client and the wall must land where MEM_FREE runs out.
#
# The four heap PVs are the IOC's own `epics_rtems_boot_mem_usage`, i.e.
# `_Protected_heap_Get_information(RTEMS_Malloc_Heap)`:
#
#   RTEMS:MEM_FREE  Free.total
#   RTEMS:MEM_USED  Used.total
#   RTEMS:MEM_MAX   free + used
#   RTEMS:MEM_BLK   Free.largest  ==  malloc_free_space()
#
# That last identity is the point of item (2): RTEMS's `malloc_free_space()`
# (cpukit/libcsupport/src/mallocfreespace.c) is exactly
# `_Protected_heap_Get_free_information(RTEMS_Malloc_Heap).largest`, and C base
# gates CA admission on it — `osiSufficentSpaceInPool()` in
# libcom/src/osi/os/RTEMS-posix/osdPoolStatus.c is
# `malloc_free_space() > 50000 + contiguousBlockSize` for RTEMS >= 5.  So
# MEM_BLK read once per client IS the C gate's input sampled along the ramp,
# and whether it tracks the wall or sits pinned is directly readable here.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164
CEILING = int(sys.argv[1]) if len(sys.argv) > 1 else 400
TAG = sys.argv[2] if len(sys.argv) > 2 else "ramp"
MAXSEC = float(sys.argv[3]) if len(sys.argv) > 3 else 1200.0
PACE = float(sys.argv[4]) if len(sys.argv) > 4 else 0.25

T0 = time.time()
resource.setrlimit(resource.RLIMIT_NOFILE, (65536, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))

MONPV = ("RTEMS:MEM_FREE", "RTEMS:MEM_USED", "RTEMS:MEM_MAX", "RTEMS:MEM_BLK",
         "RTEMS:FD_CNT", "RTEMS:FD_MAX", "RTEMS:CA_CONN_CNT",
         "RTEMS:CA_REFUSED_CNT")
DECLARED_PER_CLIENT = 1048576 + 524288


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
    def __init__(self, status, text):
        super().__init__("CA_PROTO_ERROR:%s" % text)
        self.status = status
        self.text = text


class Chan:
    def __init__(self, pv, cid, timeout=20):
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sid = self.dtype = self.dcount = self.first = None
        nm = pad(pv)
        self.sock.sendall(hdr(0, 0, 0, 13, 0, 0))
        self.sock.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.sock.settimeout(max(0.05, deadline - time.time()))
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("EOF-after-accept")
            self.buf += chunk
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, p1, p2, pl) in ms:
                if cmd == 11:
                    raise Refused(p2, pl[16:].split(b"\0")[0].decode("latin1"))
                if cmd == 26:
                    raise RuntimeError("CREATE_CH_FAIL")
                if cmd == 18 and p1 == cid:
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
        if dt == 0:
            return pl.split(b"\0")[0].decode("latin1")
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
    if "CREATE_CH_FAIL" in s:
        return "CREATE_CH_FAIL"
    if "timeout" in s:
        return "HANDSHAKE_TIMEOUT"
    return "OTHER(%s: %s)" % (type(exc).__name__, s)


mon = {}
for pv in MONPV:
    try:
        mon[pv] = Chan(pv, 900 + len(mon))
    except Exception as e:
        log("monitor %-22s UNAVAILABLE: %s" % (pv, classify(e)))
NMON = len(mon)
_ioid = [4000]


def sample(n):
    v = {}
    for pv, c in mon.items():
        _ioid[0] += 1
        try:
            v[pv] = c.read(ioid=_ioid[0])
        except Exception as e:
            v[pv] = "ERR:%s" % classify(e)
    log("SAMPLE held=%-4d MEM_FREE=%s MEM_USED=%s MEM_MAX=%s MEM_BLK=%s "
        "FD_CNT=%s FD_MAX=%s CONN=%s REFUSED=%s"
        % (n, v.get("RTEMS:MEM_FREE"), v.get("RTEMS:MEM_USED"),
           v.get("RTEMS:MEM_MAX"), v.get("RTEMS:MEM_BLK"),
           v.get("RTEMS:FD_CNT"), v.get("RTEMS:FD_MAX"),
           v.get("RTEMS:CA_CONN_CNT"), v.get("RTEMS:CA_REFUSED_CNT")))
    return v


log("monitors=%d/%d ceiling=%d pace=%.2fs deadline=%.0fs declared/client=%d B"
    % (NMON, len(MONPV), CEILING, PACE, MAXSEC, DECLARED_PER_CLIENT))
base = sample(0)

held = []
walls = []
while len(held) < CEILING:
    if time.time() - T0 > MAXSEC:
        log("INTERNAL DEADLINE %.0fs at held=%d" % (MAXSEC, len(held)))
        break
    t = time.time()
    try:
        held.append(Chan("RTEMS:AO", 1 + len(held)))
    except Exception as e:
        walls.append(e)
        log("WALL attempt=%d held=%d elapsed=%.2fs %s"
            % (len(held) + 1, len(held), time.time() - t, classify(e)))
        break
    log("ATTEMPT %3d OK held=%3d conn=%.2fs" % (len(held), len(held), time.time() - t))
    sample(len(held))
    slack = PACE - (time.time() - t)
    if slack > 0:
        time.sleep(slack)

log("=== TOP held=%d ===" % len(held))
sample(len(held))
try:
    b0 = float(base.get("RTEMS:MEM_USED"))
    top = float(sample(len(held)).get("RTEMS:MEM_USED"))
    if held:
        log("per-client MEM_USED delta = %.0f B over %d clients (declared %d B, ratio %.3f)"
            % ((top - b0) / len(held), len(held), DECLARED_PER_CLIENT,
               (top - b0) / len(held) / DECLARED_PER_CLIENT))
except Exception as e:
    log("per-client delta unavailable: %r" % e)

for c in held:
    try:
        c.sock.close()
    except Exception:
        pass
log("released %d; settling 20 s then a post-release sample" % len(held))
time.sleep(20)
sample(-1)
log("rtemsramp done")
