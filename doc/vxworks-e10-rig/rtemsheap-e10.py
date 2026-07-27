# rtemsheap-e10.py — drive the armv7-rtems guest to its REAL heap wall, and
# watch whether malloc_free_space() still reports it on the way down.
#
# Why this exists: on this BSP guest RAM is not a knob.  `-m 512M` leaves
# MEM_MAX at exactly 260,805,344 B — the same byte count as `-m 256M`, because
# `xilinx_zynq_a9_qemu` fixes its memory size at BSP build time — and anything
# below 256M is refused by qemu before boot ("kernel is too large to fit in
# RAM", declared image footprint 267,370,496 B).  So the VxWorks method (ladder
# the guest RAM, fit a line) has no analogue here, and the only way to reach the
# heap wall is to consume the heap.
#
# The client ramp alone cannot do it: it stops at 141 (the pool capacity) with
# ~9 MB still free.  This pushes past that with the second allocator the server
# has — per-subscription monitor state and whatever it queues for a client that
# has stopped reading.  A slow consumer is ordinary client behaviour, so what
# the IOC does here is what it will do in the field.
#
# Sampling runs on its own connection opened FIRST, so the readings survive
# whatever happens to the load connections.
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164
NCLIENT = int(sys.argv[1]) if len(sys.argv) > 1 else 60
NSUB = int(sys.argv[2]) if len(sys.argv) > 2 else 200
MAXSEC = float(sys.argv[3]) if len(sys.argv) > 3 else 600.0
TAG = sys.argv[4] if len(sys.argv) > 4 else "heap"

PVS = ["RTEMS:AO", "RTEMS:LO", "RTEMS:MSG", "RTEMS:CA:C1", "RTEMS:CA:C2",
       "RTEMS:CA:C3", "RTEMS:CA:C4", "RTEMS:CA:TICK"]
MONPV = ("RTEMS:MEM_FREE", "RTEMS:MEM_USED", "RTEMS:MEM_MAX", "RTEMS:MEM_BLK",
         "RTEMS:FD_CNT", "RTEMS:CA_CONN_CNT", "RTEMS:CA_REFUSED_CNT")
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
    def __init__(self, idx, timeout=20):
        self.idx = idx
        self.s = socket.create_connection((HOST, PORT), timeout=timeout)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.ch = {}
        self.err = []
        self.s.sendall(hdr(0, 0, 0, 13, 0, 0))

    def create(self, pv, cid, timeout=15):
        nm = pad(pv)
        self.s.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
        end = time.time() + timeout
        while time.time() < end:
            self.s.settimeout(max(0.05, end - time.time()))
            try:
                c = self.s.recv(65536)
            except socket.timeout:
                return False
            if not c:
                raise RuntimeError("EOF")
            self.buf += c
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, p1, p2, pl) in ms:
                if cmd == 11:
                    self.err.append(pl[16:].split(b"\0")[0].decode("latin1"))
                elif cmd == 18 and p1 == cid:
                    self.ch[pv] = (cid, p2, dt, dc)
                    return True
        return False

    def read(self, pv, ioid, timeout=15):
        cid, sid, dt, dc = self.ch[pv]
        self.s.settimeout(timeout)
        self.s.sendall(hdr(15, 0, dt, dc, sid, ioid))
        end = time.time() + timeout
        while time.time() < end:
            c = self.s.recv(65536)
            if not c:
                raise RuntimeError("EOF")
            self.buf += c
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, t, _dc, _p1, p2, pl) in ms:
                if cmd == 15 and p2 == ioid:
                    return decode(t, pl)
        raise RuntimeError("read-timeout")

    def subscribe(self, pv, n):
        """`n` subscriptions on one channel, CTRL type: the widest payload the
        type admits, so each queued event costs the most it can."""
        cid, sid, dt, dc = self.ch[pv]
        t = dt + 28 if dt < 7 else dt
        mask = struct.pack(">fffHH", 0.0, 0.0, 0.0, 7, 0)
        for k in range(n):
            self.s.sendall(hdr(1, 16, t, dc, sid, cid * 4096 + k) + mask)


# The sampler, opened first and never written to by the load.
smp = Sess(9000)
_ioid = [7000]
for pv in MONPV:
    smp.create(pv, 800 + len(smp.ch))


def sample(tag):
    v = {}
    for pv in MONPV:
        if pv not in smp.ch:
            continue
        _ioid[0] += 1
        try:
            v[pv] = smp.read(pv, _ioid[0])
        except Exception as e:
            v[pv] = "ERR:%r" % e
    log("SAMPLE %-14s MEM_FREE=%s MEM_BLK=%s MEM_USED=%s FD_CNT=%s CONN=%s REFUSED=%s"
        % (tag, v.get("RTEMS:MEM_FREE"), v.get("RTEMS:MEM_BLK"),
           v.get("RTEMS:MEM_USED"), v.get("RTEMS:FD_CNT"),
           v.get("RTEMS:CA_CONN_CNT"), v.get("RTEMS:CA_REFUSED_CNT")))
    return v


log("clients=%d subs/pv=%d pvs=%d deadline=%.0fs" % (NCLIENT, NSUB, len(PVS), MAXSEC))
sample("idle")

loads = []
for i in range(NCLIENT):
    try:
        s = Sess(i)
    except Exception as e:
        log("client %d CONNECT FAILED: %r" % (i, e))
        break
    ok = 0
    for j, pv in enumerate(PVS):
        try:
            if s.create(pv, 10 + j):
                ok += 1
        except Exception as e:
            log("client %d create %s failed: %r" % (i, pv, e))
            break
    loads.append(s)
    if ok == 0:
        log("client %d got no channels; stopping the ramp" % i)
        break

log("connected %d load clients" % len(loads))
sample("connected")

# Subscriptions, in waves, so the heap is sampled as it is consumed rather than
# once at the end.  Nothing here reads its socket again: the server must hold
# whatever it cannot write.
wave = 0
stop = None
while time.time() - T0 < MAXSEC and stop is None:
    wave += 1
    for s in loads:
        for pv in list(s.ch):
            try:
                s.subscribe(pv, NSUB)
            except Exception as e:
                stop = "client %d subscribe failed at wave %d: %r" % (s.idx, wave, e)
                break
        if stop:
            break
    try:
        v = sample("wave-%d" % wave)
    except Exception as e:
        stop = "SAMPLER LOST at wave %d: %r" % (wave, e)
        break
    f = v.get("RTEMS:MEM_FREE")
    if isinstance(f, float) and f < 2_000_000:
        stop = "free heap under 2 MB at wave %d" % wave
    time.sleep(1.0)

log("=== stopped: %s ===" % stop)
try:
    sample("top")
except Exception as e:
    log("sampler gone at top: %r" % e)

for s in loads:
    try:
        s.s.close()
    except Exception:
        pass
log("released %d load clients; settling 25 s" % len(loads))
time.sleep(25)
try:
    sample("post-release")
except Exception as e:
    log("sampler gone post-release: %r" % e)
# A fresh connection after the release is the liveness test: an IOC that
# refused politely still answers, one that died does not.
try:
    fresh = Sess(9999)
    fresh.create("RTEMS:AO", 1)
    log("post-release fresh client: channels=%d  IOC ALIVE" % len(fresh.ch))
    fresh.s.close()
except Exception as e:
    log("post-release fresh client FAILED: %r  IOC MAY BE DEAD" % e)
log("rtemsheap done")
