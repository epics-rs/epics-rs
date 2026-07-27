# rtemssqueeze-e10.py — squeeze the armv7-rtems malloc heap with server-side
# channel and subscription state, then ramp CA clients into what is left, and
# read `malloc_free_space()` at the wall.
#
# Two things this script exists to work around, both measured:
#
# 1. The heap PVs are PUSHED, not computed on read.  `status_pv.rs` has
#    `PUSH_INTERVAL = 1 s` and the pusher runs at `ThreadPriority::Low`
#    (EPICS 10), eleven levels below `CAS-client`.  Under load that thread does
#    not run: in the first attempt at this arm it published the same
#    `MEM_FREE=229178856` at t=2.6 s and again at t=497.7 s — a 495-second-stale
#    reading taken while the heap had in fact moved.  So every reading here is
#    taken QUIESCED, and only after `UPTIME` proves the pusher has caught up.
#
# 2. Flooding unread monitors does not reach the heap wall.  That arm ended on
#    `[zone: mbuf_cluster] kern.ipc.nmbclusters limit reached` — a libbsd pool
#    that is not the malloc heap — so it measures the network stack, not memory.
#    Here every socket is drained and the PV is passive, so a subscription fires
#    once and then costs only its resident state.
#
# `RTEMS:MEM_BLK` is `_Protected_heap_Get_free_information(...).largest`, which
# IS `malloc_free_space()`, which IS the input to C base's
# `osiSufficentSpaceInPool()` on RTEMS >= 5.  Watching `MEM_FREE - MEM_BLK`
# grow is watching that gate lose contact with the heap it is meant to guard.
import select
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164
NCLIENT = int(sys.argv[1]) if len(sys.argv) > 1 else 4
NCHAN = int(sys.argv[2]) if len(sys.argv) > 2 else 500
NSUB = int(sys.argv[3]) if len(sys.argv) > 3 else 20
WAVES = int(sys.argv[4]) if len(sys.argv) > 4 else 1
TAG = sys.argv[5] if len(sys.argv) > 5 else "sq"
QUIET = float(sys.argv[6]) if len(sys.argv) > 6 else 20.0
RAMP = int(sys.argv[7]) if len(sys.argv) > 7 else 0
# Past this held count the ramp slows to one client per `PUSH_INTERVAL`-plus and
# samples every step, so the approach to the wall is read rather than inferred.
SLOW_FROM = int(sys.argv[8]) if len(sys.argv) > 8 else 10 ** 9

PV = "RTEMS:AO"
MONPV = ("RTEMS:UPTIME", "RTEMS:MEM_FREE", "RTEMS:MEM_USED", "RTEMS:MEM_MAX",
         "RTEMS:MEM_BLK", "RTEMS:CA_CONN_CNT", "RTEMS:CA_REFUSED_CNT")
DECLARED_PER_CLIENT = 1048576 + 524288
T0 = time.time()


def log(m):
    print("[%7.1fs] %s %s" % (time.time() - T0, TAG, m), flush=True)


def hdr(cmd, psize, dtype, dcount, p1, p2):
    return struct.pack(">HHHHII", cmd, psize, dtype, dcount, p1, p2)


def pad(s):
    b = s.encode() + b"\0"
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
    if dt == 0:
        return pl.split(b"\0")[0].decode("latin1")
    return pl[:16]


def uptime_secs(s):
    try:
        h, m, sec = str(s).split(":")
        return int(h) * 3600 + int(m) * 60 + int(sec)
    except Exception:
        return None


class Refused(RuntimeError):
    def __init__(self, status, text):
        super().__init__(text)
        self.status, self.text = status, text


class Sess:
    def __init__(self, idx, timeout=30):
        self.idx = idx
        self.s = socket.create_connection((HOST, PORT), timeout=timeout)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.ch = {}
        self.errors = []
        self.s.sendall(hdr(0, 0, 0, 13, 0, 0))

    def drain(self, budget=0.25, until=None):
        end = time.time() + budget
        got = 0
        while True:
            if until is not None and until(self):
                break
            left = end - time.time()
            if left <= 0:
                break
            r, _, _ = select.select([self.s], [], [], min(0.05, left))
            if not r:
                continue
            c = self.s.recv(262144)
            if not c:
                raise RuntimeError("EOF")
            got += len(c)
            self.buf += c
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, p1, p2, pl) in ms:
                if cmd == 18:
                    self.ch[p1] = (p2, dt, dc)
                elif cmd == 11:
                    self.errors.append((p2, pl[16:].split(b"\0")[0].decode("latin1")))
                elif cmd == 26:
                    self.errors.append((-1, "CREATE_CH_FAIL cid=%d" % p1))
        return got

    def create_many(self, pv, cid0, n, batch=100):
        nm = pad(pv)
        want = len(self.ch) + n
        for k in range(0, n, batch):
            self.s.sendall(b"".join(
                hdr(18, len(nm), 0, 0, cid0 + k + j, 13) + nm
                for j in range(min(batch, n - k))))
            self.drain(0.10)
        self.drain(240.0, until=lambda s: len(s.ch) >= want)
        return len(self.ch)

    def subscribe_all(self, n):
        mask = struct.pack(">fffHH", 0.0, 0.0, 0.0, 1, 0)
        sent = 0
        for cid, (sid, dt, dc) in list(self.ch.items()):
            # The mask MUST travel with the header: `psize=16` promises a
            # payload, and a header sent alone makes the server read the next
            # header as this one's payload — which is exactly what produced
            # `EVENT_ADD invalid mask 6250` (0x186A is the high half of the
            # following subscription id) on the first two runs of this script.
            self.s.sendall(b"".join(
                hdr(1, 16, dt, dc, sid, cid * 4096 + n * 7 + k) + mask
                for k in range(n)))
            sent += n
            self.drain(0.02)
        return sent


smp = Sess(9000)
smp_cid = {}
for pv in MONPV:
    cid = 800 + len(smp_cid)
    nm = pad(pv)
    smp.s.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
    smp_cid[pv] = cid
smp.drain(60.0, until=lambda s: len(s.ch) >= len(MONPV))
log("sampler channels %d/%d" % (len(smp.ch), len(MONPV)))
_ioid = [5000]


def read_one(pv, timeout=25):
    cid = smp_cid[pv]
    if cid not in smp.ch:
        return "NOCHAN"
    sid, dt, dc = smp.ch[cid]
    _ioid[0] += 1
    ioid = _ioid[0]
    smp.s.sendall(hdr(15, 0, dt, dc, sid, ioid))
    end = time.time() + timeout
    while time.time() < end:
        smp.s.settimeout(max(0.05, end - time.time()))
        try:
            c = smp.s.recv(65536)
        except socket.timeout:
            return "TIMEOUT"
        if not c:
            raise RuntimeError("EOF")
        smp.buf += c
        ms, smp.buf = msgs(smp.buf)
        for (cmd, _ps, t, _dc, p1, p2, pl) in ms:
            if cmd == 18:
                smp.ch[p1] = (p2, t, _dc)
            elif cmd == 15 and p2 == ioid:
                return decode(t, pl)
    return "TIMEOUT"


UP0 = [None, None]   # (uptime seconds, wall clock) of the first fresh sample


def sample(tag, need_fresh=True, maxwait=180.0):
    """A reading is only used once UPTIME shows the 1 s pusher has caught up."""
    end = time.time() + maxwait
    while True:
        v = {}
        for pv in MONPV:
            try:
                v[pv] = read_one(pv)
            except Exception as e:
                v[pv] = "ERR:%r" % e
        up = uptime_secs(v.get("RTEMS:UPTIME"))
        fresh, lag = True, 0.0
        if up is not None:
            if UP0[0] is None:
                UP0[0], UP0[1] = up, time.time()
            else:
                lag = (time.time() - UP0[1]) - (up - UP0[0])
                fresh = lag <= 4.0
        else:
            fresh = False
        if fresh or not need_fresh or time.time() > end:
            f, b = v.get("RTEMS:MEM_FREE"), v.get("RTEMS:MEM_BLK")
            gap = (f - b) if isinstance(f, float) and isinstance(b, float) else "?"
            log("SAMPLE %-14s up=%s lag=%.1fs%s MEM_FREE=%s MEM_USED=%s "
                "MEM_BLK=%s FREE-BLK=%s CONN=%s REFUSED=%s"
                % (tag, v.get("RTEMS:UPTIME"), lag, "" if fresh else " STALE",
                   v.get("RTEMS:MEM_FREE"), v.get("RTEMS:MEM_USED"),
                   v.get("RTEMS:MEM_BLK"), gap, v.get("RTEMS:CA_CONN_CNT"),
                   v.get("RTEMS:CA_REFUSED_CNT")))
            v["_fresh"] = fresh
            v["_lag"] = lag
            return v
        time.sleep(2.0)


def num(v, k):
    x = v.get(k)
    return x if isinstance(x, float) else None


log("clients=%d chan/client=%d subs/chan/wave=%d waves=%d quiet=%.0fs ramp=%d"
    % (NCLIENT, NCHAN, NSUB, WAVES, QUIET, RAMP))
base = sample("idle")

loads, made = [], 0
for i in range(NCLIENT):
    try:
        s = Sess(i)
        n = s.create_many(PV, 100000 + i * (NCHAN + 16), NCHAN)
    except Exception as e:
        log("client %d channel phase FAILED: %r" % (i, e))
        break
    loads.append(s)
    made += n
    if n < NCHAN:
        log("client %d got %d/%d channels; channel phase stops" % (i, n, NCHAN))
        break
log("channels=%d over %d connections; quiescing %.0fs" % (made, len(loads), QUIET))
time.sleep(QUIET)
achan = sample("chan-%d" % made)
b0, a0 = num(base, "RTEMS:MEM_USED"), num(achan, "RTEMS:MEM_USED")
if b0 and a0 and made:
    conn_cost = len(loads) * 1_588_000
    log("per-CHANNEL heap = %.1f B  (delta %.0f B minus %d conns x 1,588,000 B)"
        % ((a0 - b0 - conn_cost) / made, a0 - b0, len(loads)))

subs, prev = 0, achan
for w in range(1, WAVES + 1):
    try:
        for s in loads:
            subs += s.subscribe_all(NSUB)
    except Exception as e:
        log("wave %d subscribe FAILED at %d subs: %r" % (w, subs, e))
        break
    log("wave %d issued; total subs=%d; quiescing %.0fs" % (w, subs, QUIET))
    time.sleep(QUIET)
    v = sample("sub-%d" % subs)
    p, c = num(prev, "RTEMS:MEM_USED"), num(v, "RTEMS:MEM_USED")
    if p and c:
        log("wave %d per-SUBSCRIPTION heap = %.1f B (wave delta %.0f B over %d)"
            % (w, (c - p) / (len(loads) * NCHAN * NSUB), c - p,
               len(loads) * NCHAN * NSUB))
    prev = v
    f = num(v, "RTEMS:MEM_FREE")
    if f is not None and f < 12_000_000:
        log("stopping the squeeze: MEM_FREE=%.0f" % f)
        break

errs = [e for s in loads for e in s.errors]
log("server errors during squeeze: %d%s"
    % (len(errs), (" first=%r" % (errs[0],)) if errs else ""))

# The payoff: with the heap squeezed, ramp real CA clients until the wall and
# read `malloc_free_space()` right there.
held, wall = [], None
if RAMP:
    log("=== ramping CA clients into the squeezed heap (ceiling %d) ===" % RAMP)
    while len(held) < RAMP:
        t = time.time()
        c = None
        try:
            c = Sess(30000 + len(held), timeout=30)
            nm = pad(PV)
            c.s.sendall(hdr(18, len(nm), 0, 0, 55, 13) + nm)
            c.drain(20.0, until=lambda s: len(s.ch) >= 1)
            if c.errors:
                st, tx = c.errors[0]
                wall = "REFUSED(status=%s text=%r)" % (st, tx)
                log("WALL attempt=%d held=%d %s" % (len(held) + 1, len(held), wall))
                break
            if not c.ch:
                wall = "ACCEPTED_NO_CHANNEL"
                log("WALL attempt=%d held=%d %s after %.1fs"
                    % (len(held) + 1, len(held), wall, time.time() - t))
                break
        except Exception as e:
            wall = "%s: %s" % (type(e).__name__, e)
            seen = getattr(c, "errors", None)
            log("WALL attempt=%d held=%d %s after %.1fs server_frames=%r"
                % (len(held) + 1, len(held), wall, time.time() - t, seen))
            break
        held.append(c)
        if len(held) >= SLOW_FROM:
            sample("ramp-%d" % len(held), need_fresh=True, maxwait=30.0)
            time.sleep(1.5)
        elif len(held) % 5 == 0:
            log("ramp held=%d (%.1fs)" % (len(held), time.time() - t))
    log("ramp stopped at held=%d; quiescing %.0fs" % (len(held), QUIET))
    time.sleep(QUIET)
    atwall = sample("wall-held-%d" % len(held))
    pf, wf = num(prev, "RTEMS:MEM_FREE"), num(atwall, "RTEMS:MEM_FREE")
    if pf and wf and held:
        log("ramp per-client heap = %.1f B over %d clients (declared %d)"
            % ((pf - wf) / len(held), len(held), DECLARED_PER_CLIENT))
    wb = num(atwall, "RTEMS:MEM_BLK")
    if wb is not None:
        log("AT THE WALL malloc_free_space()=%.0f B; C base gate "
            "osiSufficentSpaceInPool(16384) = %s"
            % (wb, "PASS" if wb > 50000 + 16384 else "REFUSE"))

for s in loads + held:
    try:
        s.s.close()
    except Exception:
        pass
log("released %d connections; settling %.0fs" % (len(loads) + len(held), QUIET))
time.sleep(QUIET)
sample("post-release")
log("rtemssqueeze done: wall=%s" % wall)
