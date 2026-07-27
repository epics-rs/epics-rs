# rtemsalloc-e10.py — consume the armv7-rtems malloc heap WITHOUT filling the
# network stack, so the heap wall can be reached below the CA client pool cap.
#
# Why this shape.  The plain client ramp cannot reach the heap wall: it stops at
# `CAS_CLIENT_POOL_CAPACITY` = 141 with ~9 MB free.  The obvious alternative —
# flood unread monitors — does not reach it either: that arm ended on
# `[zone: mbuf_cluster] kern.ipc.nmbclusters limit reached`, a libbsd pool the
# malloc heap does not contain, and it starved every reader on the box.  So this
# consumes heap through server-side per-channel and per-subscription state while
# DRAINING every socket, on passive PVs that fire once per subscription and then
# go quiet.  Nothing queues, nothing starves, and the only thing that grows is
# the heap.
#
# The point is item (2): `malloc_free_space()` is `Free.largest`, which the IOC
# publishes as `RTEMS:MEM_BLK`, and C base gates CA admission on exactly that
# value (`osiSufficentSpaceInPool`, RTEMS-posix/osdPoolStatus.c, RTEMS >= 5).
# Along the client ramp `largest` stays within 0.03 % of `total`, so it tracks —
# but a client costs 1.59 MB of CONTIGUOUS stack, and hundreds of thousands of
# small monitor allocations are what would make `largest` and `total` part
# company.  This arm creates that state deliberately and reads both.
import select
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164
NCLIENT = int(sys.argv[1]) if len(sys.argv) > 1 else 4
NCHAN = int(sys.argv[2]) if len(sys.argv) > 2 else 500
NSUB = int(sys.argv[3]) if len(sys.argv) > 3 else 20
TAG = sys.argv[4] if len(sys.argv) > 4 else "alloc"
MAXSEC = float(sys.argv[5]) if len(sys.argv) > 5 else 900.0

# Passive, unscanned: one event per subscription, then silence.
PV = "RTEMS:AO"
MONPV = ("RTEMS:MEM_FREE", "RTEMS:MEM_USED", "RTEMS:MEM_MAX", "RTEMS:MEM_BLK",
         "RTEMS:CA_CONN_CNT", "RTEMS:CA_REFUSED_CNT")
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


class Sess:
    def __init__(self, idx):
        self.idx = idx
        self.s = socket.create_connection((HOST, PORT), timeout=30)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.ch = {}          # cid -> (sid, dtype, dcount)
        self.named = {}       # pv  -> cid
        self.errors = []
        self.s.sendall(hdr(0, 0, 0, 13, 0, 0))

    def drain(self, budget=0.25, until=None):
        """Discard whatever the server has for us.  Never let it back up.

        Polls for the WHOLE budget: a quiet 20 ms in the middle of a burst is
        not the end of the burst, and treating it as one is what made the first
        version of this script read zero channels.
        """
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
            pkt = b"".join(hdr(18, len(nm), 0, 0, cid0 + k + j, 13) + nm
                           for j in range(min(batch, n - k)))
            self.s.sendall(pkt)
            self.drain(0.10)
        self.drain(30.0, until=lambda s: len(s.ch) >= want)
        return len(self.ch)

    def subscribe_all(self, n, batch=50):
        """`n` subscriptions per channel, DBE_VALUE only, native type."""
        mask = struct.pack(">fffHH", 0.0, 0.0, 0.0, 1, 0)
        sent = 0
        for cid, (sid, dt, dc) in list(self.ch.items()):
            pkt = b"".join(hdr(1, 16, dt, dc, sid, cid * 64 + k) + mask
                           for k in range(n))
            self.s.sendall(pkt)
            sent += n
            if sent % (batch * n or 1) == 0:
                self.drain(0.02)
        self.drain(0.4)
        return sent


smp = Sess(9000)
smp_ch = {}
for pv in MONPV:
    cid = 800 + len(smp_ch)
    nm = pad(pv)
    smp.s.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
    smp_ch[pv] = cid
smp.drain(30.0, until=lambda s: len(s.ch) >= len(MONPV))
_ioid = [5000]


def sample(tag):
    v = {}
    for pv, cid in smp_ch.items():
        if cid not in smp.ch:
            v[pv] = "NOCHAN"
            continue
        sid, dt, dc = smp.ch[cid]
        _ioid[0] += 1
        ioid = _ioid[0]
        try:
            smp.s.sendall(hdr(15, 0, dt, dc, sid, ioid))
            end = time.time() + 20
            val = None
            while time.time() < end and val is None:
                smp.s.settimeout(max(0.05, end - time.time()))
                c = smp.s.recv(65536)
                if not c:
                    raise RuntimeError("EOF")
                smp.buf += c
                ms, smp.buf = msgs(smp.buf)
                for (cmd, _ps, t, _dc, _p1, p2, pl) in ms:
                    if cmd == 18:
                        smp.ch[_p1] = (p2, t, _dc)
                    elif cmd == 15 and p2 == ioid:
                        val = decode(t, pl)
            v[pv] = val if val is not None else "TIMEOUT"
        except Exception as e:
            v[pv] = "ERR:%r" % e
    f, b = v.get("RTEMS:MEM_FREE"), v.get("RTEMS:MEM_BLK")
    gap = (f - b) if isinstance(f, float) and isinstance(b, float) else "?"
    log("SAMPLE %-16s MEM_FREE=%s MEM_USED=%s MEM_BLK=%s FREE-BLK=%s CONN=%s REFUSED=%s"
        % (tag, v.get("RTEMS:MEM_FREE"), v.get("RTEMS:MEM_USED"),
           v.get("RTEMS:MEM_BLK"), gap, v.get("RTEMS:CA_CONN_CNT"),
           v.get("RTEMS:CA_REFUSED_CNT")))
    return v


log("clients=%d chan/client=%d subs/chan=%d pv=%s" % (NCLIENT, NCHAN, NSUB, PV))
base = sample("idle")

loads, made = [], 0
for i in range(NCLIENT):
    if time.time() - T0 > MAXSEC:
        log("DEADLINE during channel phase at client %d" % i)
        break
    try:
        s = Sess(i)
    except Exception as e:
        log("client %d connect FAILED: %r" % (i, e))
        break
    try:
        n = s.create_many(PV, 1000 + i * (NCHAN + 8), NCHAN)
    except Exception as e:
        log("client %d create FAILED after %d: %r" % (i, len(s.ch), e))
        loads.append(s)
        made += len(s.ch)
        break
    loads.append(s)
    made += n
    if s.errors:
        log("client %d first error: %r" % (i, s.errors[0]))
    v = sample("chan-%d" % made)
    if n < NCHAN:
        log("client %d got %d/%d channels; channel phase stops" % (i, n, NCHAN))
        break

log("=== channels created: %d over %d clients ===" % (made, len(loads)))
after_chan = sample("chan-top")
try:
    d = (float(after_chan["RTEMS:MEM_USED"]) - float(base["RTEMS:MEM_USED"]))
    log("per-CHANNEL heap = %.1f B  (%d channels, %d client conns)"
        % ((d - len(loads) * 1588000) / made, made, len(loads)))
except Exception as e:
    log("per-channel unavailable: %r" % e)

# Subscriptions, in waves over all clients, sampled each wave.
wave, stop, subs = 0, None, 0
while time.time() - T0 < MAXSEC and stop is None:
    wave += 1
    for s in loads:
        try:
            subs += s.subscribe_all(NSUB)
        except Exception as e:
            stop = "client %d subscribe failed at wave %d after %d subs: %r" % (
                s.idx, wave, subs, e)
            break
    v = sample("sub-%d" % subs)
    f = v.get("RTEMS:MEM_FREE")
    if isinstance(f, float) and f < 4_000_000:
        stop = "free heap under 4 MB at %d subscriptions" % subs
    if not isinstance(f, float):
        stop = "sampler lost at %d subscriptions" % subs
    errs = [e for s in loads for e in s.errors]
    if errs:
        log("server errors seen: %d, first=%r" % (len(errs), errs[0]))

log("=== stopped: %s (subs=%d) ===" % (stop, subs))
top = sample("sub-top")
try:
    d = float(top["RTEMS:MEM_USED"]) - float(after_chan["RTEMS:MEM_USED"])
    log("per-SUBSCRIPTION heap = %.1f B  (%d subscriptions)" % (d / subs, subs))
except Exception as e:
    log("per-subscription unavailable: %r" % e)

# With the heap squeezed, does a NEW CA client still get in, and does the server
# refuse politely or fail?  This is the pair to the pool-cap wall.
for k in range(6):
    t = time.time()
    try:
        n = Sess(20000 + k)
        nm = pad(PV)
        n.s.sendall(hdr(18, len(nm), 0, 0, 77, 13) + nm)
        n.drain(8.0)
        log("squeezed-client %d: channels=%d errors=%r conn=%.2fs"
            % (k, len(n.ch), n.errors[:1], time.time() - t))
        loads.append(n)
    except Exception as e:
        log("squeezed-client %d FAILED after %.2fs: %r" % (k, time.time() - t, e))
        break
    sample("squeeze-%d" % k)

for s in loads:
    try:
        s.s.close()
    except Exception:
        pass
log("released %d connections; settling 25 s" % len(loads))
time.sleep(25)
sample("post-release")
log("rtemsalloc done")
