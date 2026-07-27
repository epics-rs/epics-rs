# rtemsload-e10.py — drive the armv7-rtems guest hard enough that the
# `CAS-client` and `CAS-event` stack high-water marks the guest reports are the
# high-water of a *working* server, not of an idle one.
#
# The number this exists to produce is the input to the `client_roster`
# StackSizeClass decision: `CAS-client` is Big and `CAS-event` is Medium
# (`runtime/task.rs` `StackSizeClass::bytes()` = f * 0x10000 * usize), i.e.
# 1,048,576 B and 524,288 B on a 32-bit target — half the x86_64-wrs-vxworks
# figures, so the VxWorks reading does not transfer.
#
# RTEMS's stack checker reports the deepest point the pattern fill was ever
# overwritten to, per task, for the whole life of the task.  So the load has to
# reach the deepest code path at least once; it does NOT have to hold it.  Every
# CA request shape the server implements is therefore driven at least once per
# client, on every record in the image's database:
#
#   CREATE_CHAN, READ_NOTIFY (native and DBR_CTRL_*, the largest response
#   payload the type admits), EVENT_ADD (subscription, native + CTRL),
#   WRITE and WRITE_NOTIFY, EVENT_CANCEL, CLEAR_CHANNEL.
#
# CLIENT_NAME/HOST_NAME are sent like a real libca client, because the server
# parses and stores them per client and a rig that skips them measures a
# shorter parse path than production takes.
#
# Ports: 127.0.0.1:25164 -> guest 5064, this panel's own hostfwd; the other two
# guests on the box are on 5064 and 5075 and are never touched.
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164
NCLIENT = int(sys.argv[1]) if len(sys.argv) > 1 else 8
HOLD = float(sys.argv[2]) if len(sys.argv) > 2 else 200.0
TAG = sys.argv[3] if len(sys.argv) > 3 else "load"

T0 = time.time()

# The image's whole database: 3 demo records + the 14-record C6 probe rig +
# the target status PVs the status pusher publishes.
PVS = [
    "RTEMS:AO", "RTEMS:LO", "RTEMS:MSG",
    "RTEMS:CA:DOWN", "RTEMS:CA:DOWN2", "RTEMS:CA:UPLNK", "RTEMS:CA:FAST",
    "RTEMS:CA:C1", "RTEMS:CA:C2", "RTEMS:CA:C3", "RTEMS:CA:C4",
    "RTEMS:CA:C5", "RTEMS:CA:C6", "RTEMS:CA:C7", "RTEMS:CA:C8",
    "RTEMS:CA:OTHER", "RTEMS:CA:TICK",
    "RTEMS:CA_CONN_CNT", "RTEMS:CA_REFUSED_CNT", "RTEMS:FD_CNT",
    "RTEMS:FD_MAX", "RTEMS:MEM_USED", "RTEMS:MEM_FREE",
]
WRITABLE = {"RTEMS:AO": 6, "RTEMS:LO": 5, "RTEMS:MSG": 0, "RTEMS:CA:UPLNK": 6}


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


class Session:
    """One CA circuit: the server gives it one CAS-client and one CAS-event."""

    def __init__(self, idx, timeout=20):
        self.idx = idx
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.chans = {}          # pv -> (cid, sid, dtype, dcount)
        self.next_cid = 1
        self.errors = []
        self.sock.sendall(hdr(0, 0, 0, 13, 0, 0))
        user = pad("e10rig%d" % idx)
        host = pad("gv100-e10")
        self.sock.sendall(hdr(20, len(user), 0, 0, 0, 0) + user)
        self.sock.sendall(hdr(21, len(host), 0, 0, 0, 0) + host)

    def pump(self, seconds=0.0):
        deadline = time.time() + seconds
        while True:
            self.sock.settimeout(max(0.02, deadline - time.time()))
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                break
            except OSError:
                break
            if not chunk:
                raise RuntimeError("EOF")
            self.buf += chunk
            ms, self.buf = msgs(self.buf)
            for m in ms:
                if m[0] == 11:
                    self.errors.append(m[6][16:].split(b"\0")[0].decode("latin1"))
                elif m[0] == 18:
                    self.pending_create = m
            if time.time() >= deadline:
                break
        return None

    def create(self, pv, timeout=20):
        cid = self.next_cid
        self.next_cid += 1
        nm = pad(pv)
        self.sock.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.sock.settimeout(max(0.05, deadline - time.time()))
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                break
            if not chunk:
                raise RuntimeError("EOF")
            self.buf += chunk
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, p1, p2, pl) in ms:
                if cmd == 11:
                    self.errors.append(pl[16:].split(b"\0")[0].decode("latin1"))
                elif cmd == 18 and p1 == cid:
                    self.chans[pv] = (cid, p2, dt, dc)
                    return True
        return False

    def request_all(self):
        """Every request shape the server implements, on every channel.

        DBR family sweep: native, STS(+7), TIME(+14), GR(+21), CTRL(+28) — the
        response encoders differ per family and the stack reading has to see
        the deepest of them, not just the plain scalar path.
        """
        ioid = 5000 + self.idx * 1000
        for pv, (cid, sid, dt, dc) in self.chans.items():
            fam = [dt] + [dt + k for k in (7, 14, 21, 28) if dt < 7]
            for t in fam:
                ioid += 1
                self.sock.sendall(hdr(15, 0, t, dc, sid, ioid))   # READ_NOTIFY
                ioid += 1
                self.sock.sendall(hdr(7, 0, t, dc, sid, ioid))    # legacy READ
            # EVENT_ADD on every family, mask = VALUE|LOG|ALARM.
            for n, t in enumerate(fam, 1):
                mask = struct.pack(">fffHH", 0.0, 0.0, 0.0, 7, 0)
                self.sock.sendall(hdr(1, 16, t, dc, sid, cid * 16 + n) + mask)
        self.pump(0.5)

    def error_paths(self):
        """The refusal/diagnostic encoders, which are their own call depth.

        Every one of these is answered with CA_PROTO_ERROR, whose payload
        carries the original 16-byte request header plus a formatted message —
        a different, and longer, response path than a value reply.
        """
        ioid = 30000 + self.idx * 1000
        # A channel that does not exist: CREATE_CHAN -> NOT_FOUND / ERROR.
        nm = pad("RTEMS:NO:SUCH:RECORD:AT:ALL")
        self.sock.sendall(hdr(18, len(nm), 0, 0, 9999, 13) + nm)
        # SEARCH on the TCP circuit (not UDP): served by this same task.
        for name in ("RTEMS:AO", "RTEMS:NO:SUCH:RECORD:AT:ALL"):
            s = pad(name)
            self.sock.sendall(hdr(6, len(s), 5, 13, 8888, 8888) + s)
        ent = self.chans.get("RTEMS:AO")
        if ent is not None:
            _cid, sid, dt, _dc = ent
            # Element count beyond the record's: ECA_BADCOUNT.
            ioid += 1
            self.sock.sendall(hdr(15, 0, dt, 4096, sid, ioid))
            # An undefined DBR type: ECA_BADTYPE.
            ioid += 1
            self.sock.sendall(hdr(15, 0, 199, 1, sid, ioid))
            # A write whose payload is shorter than the declared type.
            self.sock.sendall(hdr(4, 8, dt, 1, sid, ioid) + b"\0" * 8)
        # A read on a server id that was never handed out.
        ioid += 1
        self.sock.sendall(hdr(15, 0, 6, 1, 0x7fffffff, ioid))
        self.sock.sendall(hdr(23, 0, 0, 0, 0, 0))   # ECHO
        self.sock.sendall(hdr(8, 0, 0, 0, 0, 0))    # EVENTS_OFF
        self.sock.sendall(hdr(9, 0, 0, 0, 0, 0))    # EVENTS_ON
        self.pump(0.5)

    def writes(self, k):
        ioid = 8000 + self.idx * 1000 + k * 50
        for pv, dt in WRITABLE.items():
            ent = self.chans.get(pv)
            if ent is None:
                continue
            _cid, sid, _ndt, _dc = ent
            if dt == 6:
                payload = struct.pack(">d", 1.5 + k)
            elif dt == 5:
                payload = struct.pack(">i", 7 + k)
            else:
                payload = pad("e10-%d" % k)[:40].ljust(40, b"\0")
            ioid += 1
            self.sock.sendall(hdr(4, len(payload), dt, 1, sid, ioid) + payload)
            ioid += 1
            self.sock.sendall(hdr(19, len(payload), dt, 1, sid, ioid) + payload)

    def teardown(self):
        for pv, (cid, sid, _dt, _dc) in self.chans.items():
            for n in (1, 2):
                self.sock.sendall(hdr(2, 0, 0, 0, sid, cid * 16 + n))
            self.sock.sendall(hdr(12, 0, 0, 0, sid, cid))
        self.pump(0.5)
        self.sock.close()


log("clients=%d hold=%.0fs port=%d pvs=%d" % (NCLIENT, HOLD, PORT, len(PVS)))
sessions = []
for i in range(NCLIENT):
    try:
        s = Session(i)
    except Exception as e:
        log("client %d CONNECT FAILED: %r" % (i, e))
        continue
    ok = sum(1 for pv in PVS if s.create(pv))
    log("client %d up: %d/%d channels" % (i, ok, len(PVS)))
    sessions.append(s)

for s in sessions:
    s.request_all()
log("issued 5-family reads (native/STS/TIME/GR/CTRL, notify + legacy) and 5 "
    "subscriptions on every channel of %d clients" % len(sessions))
for s in sessions:
    try:
        s.error_paths()
    except Exception as e:
        log("client %d error-path sweep failed: %r" % (s.idx, e))
log("issued the refusal sweep (bad name, TCP search, bad count, bad type, "
    "short write, unknown sid, echo, events off/on)")

# Connect/disconnect churn, concurrently with the held load: the pool's
# acquire/release path and a fresh task's first-request path are not the same
# code as a warm task's, and either could be the deeper one.
churn = 0
k = 0
while time.time() - T0 < HOLD:
    k += 1
    for s in sessions:
        try:
            s.writes(k)
            s.pump(0.05)
        except Exception as e:
            log("client %d pump/write failed at k=%d: %r" % (s.idx, k, e))
    if k % 10 == 0:
        try:
            t = Session(900 + churn)
            for pv in ("RTEMS:AO", "RTEMS:MSG", "RTEMS:CA:C1"):
                t.create(pv)
            t.request_all()
            t.error_paths()
            t.teardown()
            churn += 1
        except Exception as e:
            log("churn session %d failed: %r" % (churn, e))
    if k % 20 == 0:
        log("write round %d, %d clients live, %d churn sessions" % (k, len(sessions), churn))

log("=== load done, rounds=%d ===" % k)
for s in sessions:
    if s.errors:
        log("client %d server errors: %r" % (s.idx, s.errors[:4]))
for s in sessions:
    try:
        s.teardown()
    except Exception:
        pass
log("released %d sessions" % len(sessions))
