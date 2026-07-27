# rtemschain-e10.py — the deepest CAS-client path in this image: a CA write
# that lands on the head of the 9-record FLNK chain.
#
# The general load driver writes RTEMS:AO / RTEMS:LO / RTEMS:MSG / RTEMS:CA:UPLNK
# — one record processed per put, plus (for UPLNK) an outbound `ca://` put.  The
# C6 probe database also carries RTEMS:CA:FAST -> C1 -> C2 -> ... -> C8, nine
# records processed *inline* on whichever task performed the put.  A CA client
# writing C1 therefore drives eight further record processings on the CAS-client
# task itself, which is a strictly deeper call chain than any read encoder, and
# the stack high-water is not a bound until that path has run.
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164
ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 300
NCLIENT = int(sys.argv[2]) if len(sys.argv) > 2 else 4
CHAIN = ["RTEMS:CA:FAST", "RTEMS:CA:C1", "RTEMS:CA:C2", "RTEMS:CA:C3",
         "RTEMS:CA:C4", "RTEMS:CA:C5", "RTEMS:CA:C6", "RTEMS:CA:C7",
         "RTEMS:CA:C8", "RTEMS:CA:UPLNK", "RTEMS:CA:TICK"]
T0 = time.time()


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


class C:
    def __init__(self, idx):
        self.idx = idx
        self.s = socket.create_connection((HOST, PORT), timeout=20)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.ch = {}
        self.s.sendall(hdr(0, 0, 0, 13, 0, 0))
        u = pad("chain%d" % idx)
        self.s.sendall(hdr(20, len(u), 0, 0, 0, 0) + u)
        self.s.sendall(hdr(21, len(u), 0, 0, 0, 0) + u)

    def create(self, pv, cid):
        nm = pad(pv)
        self.s.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
        end = time.time() + 15
        while time.time() < end:
            self.s.settimeout(max(0.05, end - time.time()))
            try:
                c = self.s.recv(65536)
            except socket.timeout:
                return False
            if not c:
                return False
            self.buf += c
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, p1, p2, _pl) in ms:
                if cmd == 18 and p1 == cid:
                    self.ch[pv] = (cid, p2, dt, dc)
                    return True
        return False

    def drain(self, sec=0.03):
        end = time.time() + sec
        while True:
            self.s.settimeout(max(0.01, end - time.time()))
            try:
                c = self.s.recv(65536)
            except (socket.timeout, OSError):
                return
            if not c:
                return
            self.buf += c
            _ms, self.buf = msgs(self.buf)
            if time.time() >= end:
                return


cs = []
for i in range(NCLIENT):
    c = C(i)
    ok = 0
    for j, pv in enumerate(CHAIN):
        if c.create(pv, 100 + j):
            ok += 1
    print("[%6.1fs] chain client %d: %d/%d channels" % (time.time() - T0, i, ok, len(CHAIN)), flush=True)
    # Subscribe to the chain tail so the event task also has to encode every
    # value the chain produces: nine monitors per put, on CAS-event.
    for pv, (cid, sid, dt, dc) in c.ch.items():
        c.s.sendall(hdr(1, 16, dt + 28 if dt < 7 else dt, dc, sid, cid * 8 + 1)
                    + struct.pack(">fffHH", 0, 0, 0, 7, 0))
    cs.append(c)

for k in range(ROUNDS):
    for c in cs:
        for pv in ("RTEMS:CA:C1", "RTEMS:CA:FAST", "RTEMS:CA:UPLNK", "RTEMS:CA:TICK"):
            ent = c.ch.get(pv)
            if ent is None:
                continue
            _cid, sid, dt, _dc = ent
            payload = struct.pack(">d", 1.0 + k) if dt == 6 else struct.pack(">i", k)
            c.s.sendall(hdr(4, len(payload), dt, 1, sid, 9000 + k) + payload)
            c.s.sendall(hdr(19, len(payload), dt, 1, sid, 9500 + k) + payload)
        c.drain()
    if k % 50 == 0:
        print("[%6.1fs] chain round %d" % (time.time() - T0, k), flush=True)

print("[%6.1fs] chain done, holding for the next stack report" % (time.time() - T0), flush=True)
time.sleep(130)
for c in cs:
    c.s.close()
print("[%6.1fs] chain released" % (time.time() - T0), flush=True)
