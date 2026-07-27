# E8 task 3: isolate the put_chain WRITE_NOTIFY stall.
#
# stackload.py reported it as a load-dependent outlier -- one `put_chain
# TIMEOUT 30.75s` in 40 ops, one in 30, one in 20 -- but alarmprobe.py hit it
# on a single connection with no other load, so it is first-touch, not load.
# The question left is WHICH first touch:
#
#   fanfirst   NOTIFY FAN, NOTIFY H, NOTIFY H
#              -> if FAN is fine and the first H stalls, it is the CHAIN, not
#                 "the first WRITE_NOTIFY on a fresh RTP".
#   plainfirst plain WRITE H, NOTIFY H, NOTIFY H
#              -> if the plain write warms it and the notify then returns, the
#                 trigger is the first PROCESSING of the chain, not the notify.
#   hfirst     NOTIFY H, NOTIFY H, NOTIFY FAN
#              -> the alarmprobe ordering, with a 90s budget so a late reply is
#                 told from no reply at all.
#
# Each scenario needs its own cold boot: the state it probes exists once per
# RTP start.
#
# usage: python3 notifyprobe.py SCENARIO [BASE]
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
SCENARIO = sys.argv[1] if len(sys.argv) > 1 else "hfirst"
BASE = float(sys.argv[2]) if len(sys.argv) > 2 else 400.0

T0 = time.time()
DBR_STRING = 0
DBR_DOUBLE = 6


def log(m):
    print("[%7.1fs] %s %s" % (time.time() - T0, SCENARIO, m), flush=True)


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
    def __init__(self, timeout=120):
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
        raise RuntimeError("no reply within %.1fs" % (budget or self.timeout))

    def channel(self, pv):
        self.cid += 1
        cid = self.cid
        nm = pad(pv)
        self.sock.sendall(hdr(6, 10, 13, cid, cid, nm))
        self.pump(6)
        self.sock.sendall(hdr(18, 0, 0, cid, 13, nm))
        m = self.pump(18)
        return (m[4], m[1], m[2], cid)

    def getd(self, ch):
        sid, _dt, _n, cid = ch
        ioid = 0x4000 + cid
        self.sock.sendall(hdr(15, DBR_DOUBLE, 1, sid, ioid))
        m = self.pump(15, key=ioid)
        return struct.unpack(">d", m[5][:8])[0]

    def put(self, ch, v, notify, budget):
        sid, _dt, _n, cid = ch
        # A distinct ioid per attempt: a late reply to attempt 1 must not be
        # mistaken for the reply to attempt 2.
        self.cid += 1
        ioid = 0x5000 + self.cid
        t = time.time()
        self.sock.sendall(hdr(19 if notify else 4, DBR_DOUBLE, 1, sid, ioid,
                              struct.pack(">d", v)))
        if notify:
            self.pump(19, key=ioid, budget=budget)
        return time.time() - t


def step(c, ch, rec, v, notify, budget):
    kind = "NOTIFY" if notify else "PLAIN "
    try:
        dt = c.put(ch[rec], v, notify, budget)
        log("%s %-4s=%.1f budget=%4.0fs elapsed=%7.3fs OK" % (kind, rec, v, budget, dt))
    except Exception as e:
        log("%s %-4s=%.1f budget=%4.0fs elapsed=%7.3fs FAILED: %s"
            % (kind, rec, v, budget, time.time() - T0, e))


def solo(pv, v, budget, why):
    """One WRITE_NOTIFY on its OWN connection.

    `plainfirst` and `fanfirst` both showed the first WRITE_NOTIFY to
    RTEMS:E8:H never replying and the second replying in ~30ms, so the
    remaining question is what the "first" is keyed on.  A fresh connection per
    step keeps per-record and per-connection apart: if the state that unsticks
    it is per-record, a brand-new connection still gets a fast reply.
    """
    cc = Conn()
    try:
        c2 = cc.channel(pv)
    except Exception as e:
        log("SOLO   %-14s channel FAILED: %s" % (pv, e))
        cc.sock.close()
        return
    t = time.time()
    try:
        cc.put(c2, v, True, budget)
        log("SOLO   %-14s=%.1f budget=%3.0fs elapsed=%7.3fs OK    (%s)"
            % (pv, v, budget, time.time() - t, why))
    except Exception as e:
        log("SOLO   %-14s=%.1f budget=%3.0fs elapsed=%7.3fs LOST  (%s) %s"
            % (pv, v, budget, time.time() - t, why, e))
    cc.sock.close()


if SCENARIO == "discrim":
    # Each on its own connection, cold RTP, in increasing order of what the
    # chain does:
    solo("RTEMS:AO", BASE + 1, 45.0, "ao, no FLNK, VAL preset")
    solo("RTEMS:E8:L15", BASE + 2, 45.0, "calc, FLNK->L16, entry depth 0 so no bail")
    solo("RTEMS:E8:H", BASE + 3, 45.0, "ao + 16-deep chain that BAILS")
    solo("RTEMS:E8:H", BASE + 4, 20.0, "same record, new connection")
    log("notifyprobe done")
    raise SystemExit(0)

c = Conn()
ch = {"H": c.channel("RTEMS:E8:H"), "FAN": c.channel("RTEMS:E8:FAN")}
log("channels up")

if SCENARIO == "fanfirst":
    step(c, ch, "FAN", BASE + 1, True, 60.0)
    step(c, ch, "H", BASE + 2, True, 90.0)
    step(c, ch, "H", BASE + 3, True, 20.0)
elif SCENARIO == "plainfirst":
    step(c, ch, "H", BASE + 1, False, 0.0)
    time.sleep(3.0)
    step(c, ch, "H", BASE + 2, True, 90.0)
    step(c, ch, "H", BASE + 3, True, 20.0)
else:
    step(c, ch, "H", BASE + 1, True, 90.0)
    step(c, ch, "H", BASE + 2, True, 20.0)
    step(c, ch, "FAN", BASE + 3, True, 20.0)

for rec in ("H", "FAN"):
    try:
        log("%-4s = %.1f" % (rec, c.getd(ch[rec])))
    except Exception as e:
        log("%-4s read FAILED: %s" % (rec, e))

c.sock.close()
log("notifyprobe done")
