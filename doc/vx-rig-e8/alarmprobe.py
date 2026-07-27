# E8: prove on target that a MAX_LINK_DEPTH refusal is AUDIBLE.
#
# Before this round the bail returned Ok(None): the CA WRITE_NOTIFY that drove
# the chain completed normally, no record carried an alarm, and the only notice
# was an `eprintln!` ("link chain depth limit reached at record ...") that
# reaches no errlog and no IOC log file.  The fix routes both bounds through
# `PvDatabase::refuse_bounded_entry`, which publishes C's own refused-cycle
# shape (`dbAccess.c:544-556`: SCAN_ALARM / INVALID_ALARM + AMSG + post) and an
# errlog line.
#
# This reads L15 and L16 STAT / SEVR / AMSG as strings around a WRITE_NOTIFY
# into the chain head, so the boundary is shown from the wire and not only from
# the console:
#
#   L15 (last processed)   -> no SCAN_ALARM
#   L16 (first refused)    -> SCAN_ALARM / INVALID / "link chain depth limit 16"
#
# usage: python3 alarmprobe.py [TAG] [BASE]
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
TAG = sys.argv[1] if len(sys.argv) > 1 else "alarm"
BASE = float(sys.argv[2]) if len(sys.argv) > 2 else 300.0

T0 = time.time()
DBR_STRING = 0
DBR_DOUBLE = 6


def log(m):
    print("[%7.1fs] %s %s" % (time.time() - T0, TAG, m), flush=True)


def hdr(cmd, dtype, count, p1, p2, payload=b""):
    n = len(payload)
    if n >= 0xFFFF or count >= 0xFFFF:
        return (struct.pack(">HHHHII", cmd, 0xFFFF, dtype, 0, p1, p2)
                + struct.pack(">II", n, count) + payload)
    return struct.pack(">HHHHII", cmd, n, dtype, count, p1, p2) + payload


def msgs(buf):
    out = []
    while len(buf) >= 16:
        cmd, psize, dt, dc, p1, p2 = struct.unpack(">HHHHII", buf[:16])
        if psize == 0xFFFF and dc == 0:
            if len(buf) < 24:
                break
            psize, dc = struct.unpack(">II", buf[16:24])
            head = 24
        else:
            head = 16
        if len(buf) < head + psize:
            break
        out.append((cmd, dt, dc, p1, p2, buf[head:head + psize]))
        buf = buf[head + psize:]
    return out, buf


def pad(name):
    b = name.encode() + b"\0"
    return b.ljust((len(b) + 7) // 8 * 8, b"\0")


class Conn:
    def __init__(self, timeout=45):
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sock.sendall(hdr(0, 0, 13, 0, 0))
        self.cid = 0
        self.timeout = timeout

    def pump(self, want, key=None, budget=None):
        deadline = time.time() + (budget or self.timeout)
        while time.time() < deadline:
            ms, self.buf = msgs(self.buf)
            for m in ms:
                if m[0] == 11:
                    raise RuntimeError("CA_PROTO_ERROR:%s"
                                       % m[5][16:].split(b"\0")[0].decode("latin1"))
                if m[0] == want and (key is None or m[4] == key):
                    return m
            self.sock.settimeout(max(0.05, deadline - time.time()))
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise RuntimeError("EOF")
            self.buf += chunk
        raise RuntimeError("timeout waiting for command %d" % want)

    def channel(self, pv):
        self.cid += 1
        cid = self.cid
        nm = pad(pv)
        self.sock.sendall(hdr(6, 10, 13, cid, cid, nm))
        self.pump(6)
        self.sock.sendall(hdr(18, 0, 0, cid, 13, nm))
        m = self.pump(18)
        return (m[4], m[1], m[2], cid)

    def gets(self, ch):
        sid, _dt, _n, cid = ch
        ioid = 0x4000 + cid
        self.sock.sendall(hdr(15, DBR_STRING, 1, sid, ioid))
        m = self.pump(15, key=ioid)
        return m[5].split(b"\0")[0].decode("latin1")

    def putn(self, ch, v, budget):
        sid, _dt, _n, cid = ch
        ioid = 0x5000 + cid
        t = time.time()
        self.sock.sendall(hdr(19, DBR_DOUBLE, 1, sid, ioid, struct.pack(">d", v)))
        self.pump(19, key=ioid, budget=budget)
        return time.time() - t


FIELDS = ("STAT", "SEVR", "AMSG")
RECS = ("RTEMS:E8:L14", "RTEMS:E8:L15", "RTEMS:E8:L16", "RTEMS:E8:L17")

c = Conn()
ch = {"H": c.channel("RTEMS:E8:H")}
for r in RECS:
    for f in FIELDS:
        pv = "%s.%s" % (r, f)
        try:
            ch[pv] = c.channel(pv)
        except Exception as e:
            log("channel %-22s UNAVAILABLE: %s" % (pv, e))


def dump(when):
    for r in RECS:
        row = []
        for f in FIELDS:
            pv = "%s.%s" % (r, f)
            if pv not in ch:
                row.append("%s=?" % f)
                continue
            try:
                row.append("%s=%r" % (f, c.gets(ch[pv])))
            except Exception as e:
                row.append("%s=ERR(%s)" % (f, e))
        log("%-6s %-14s %s" % (when, r.split(":")[-1], "  ".join(row)))


dump("before")
try:
    dt = c.putn(ch["H"], BASE, budget=40.0)
    log("put H=%.1f WRITE_NOTIFY elapsed=%.3fs OK" % (BASE, dt))
except Exception as e:
    log("put H=%.1f WRITE_NOTIFY FAILED: %s" % (BASE, e))
time.sleep(1.0)
dump("after")

c.sock.close()
log("alarmprobe done")
