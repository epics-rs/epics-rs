# rtemsrecover-e10.py — after the blast, does the IOC come back?
#
# This is the discriminator the mbuf arm needs.  `rtemsmbuf-e10.py` establishes
# that N non-reading clients blasting inbound requests stop the IOC serving new
# clients, and that at N>=32 the guest also prints
# `[zone: mbuf_cluster] kern.ipc.nmbclusters limit reached`.  It does NOT
# establish that the mbuf ceiling is the cause: the same outage appears at N=8
# with zero such lines.
#
# So the question is not "did it print the message" but "is this an outage that
# ends".  An IOC that recovers is starved (the priority-band defect); an IOC
# that never recovers with its load gone is a new defect and a resource that
# has to be budgeted.  This blasts, releases, and then polls a fresh CA client
# every POLL seconds until it answers or the deadline passes.
import select
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164
NCLIENT = int(sys.argv[1]) if len(sys.argv) > 1 else 32
BLAST_SECS = float(sys.argv[2]) if len(sys.argv) > 2 else 25.0
DEADLINE = float(sys.argv[3]) if len(sys.argv) > 3 else 600.0
POLL = float(sys.argv[4]) if len(sys.argv) > 4 else 20.0
TAG = sys.argv[5] if len(sys.argv) > 5 else "rec"

PV = "RTEMS:AO"
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


class Sess:
    def __init__(self, timeout=15):
        self.s = socket.create_connection((HOST, PORT), timeout=timeout)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.ch = {}
        self.s.sendall(hdr(0, 0, 0, 13, 0, 0))

    def drain(self, budget, until=None):
        end = time.time() + budget
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
            self.buf += c
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, p1, p2, pl) in ms:
                if cmd == 18:
                    self.ch[p1] = (p2, dt, dc)

    def create(self, pv, cid, budget):
        nm = pad(pv)
        self.s.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
        self.drain(budget, until=lambda s: cid in s.ch)
        return cid in self.ch


def probe(budget=12.0):
    t = time.time()
    try:
        c = Sess(timeout=budget)
        ok = c.create(PV, 61, budget=budget)
        c.s.close()
        return ok, time.time() - t
    except Exception as e:
        return "%s" % type(e).__name__, time.time() - t


ok, dt = probe()
log("baseline probe: %s in %.1fs" % (ok, dt))

loads = []
for i in range(NCLIENT):
    try:
        c = Sess(timeout=25)
        if not c.create(PV, 10, budget=20.0):
            log("client %d got no channel; ramp stops" % i)
            break
        loads.append(c)
    except Exception as e:
        log("client %d FAILED: %r" % (i, e))
        break
log("connected %d blasting clients" % len(loads))

for c in loads:
    c.s.setblocking(False)
pushed = 0
end = time.time() + BLAST_SECS
while time.time() < end:
    for c in loads:
        sid, dt_, dc = c.ch[10]
        pkt = b"".join(hdr(15, 0, dt_, dc, sid, 7000 + k) for k in range(256))
        try:
            pushed += c.s.send(pkt)
        except OSError:
            continue
log("blast done: %d B pushed" % pushed)

# Hard close: SO_LINGER 0 so the host sends RST and whatever the host still has
# queued for the guest is discarded rather than trickling in for minutes.
for c in loads:
    try:
        c.s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER,
                       struct.pack("ii", 1, 0))
        c.s.close()
    except OSError:
        pass
log("load released with RST; polling for recovery every %.0fs up to %.0fs"
    % (POLL, DEADLINE))

t_release = time.time()
recovered = None
while time.time() - t_release < DEADLINE:
    ok, dt = probe()
    log("probe at +%.0fs: %s in %.1fs" % (time.time() - t_release, ok, dt))
    if ok is True:
        recovered = time.time() - t_release
        break
    time.sleep(POLL)

if recovered is None:
    log("NO RECOVERY within %.0fs of the load being released" % DEADLINE)
else:
    log("RECOVERED %.0fs after the load was released" % recovered)
log("rtemsrecover done")
