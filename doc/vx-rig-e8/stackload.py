# E8 follow-up: stack high-water under a workload that is NOT one scalar.
#
# §10.2 of doc/vxworks-ca-worker-pool-on-target-measurement.md could only
# report a 13,240 B CAS-client high-water, because every driver in that round
# did READ_NOTIFY against a single `ao`.  That is the shallowest CA request
# there is, so it cannot decide StackSizeClass.  This drives the four shapes
# the decision actually depends on, against the RTEMS:E8:* records:
#
#   large array get     READ_NOTIFY, 32,768 DOUBLE -> a 262,144 B reply, which
#                       crosses the CA extended-header boundary (payload >
#                       0xFFFF) and so exercises the 24-byte header path.
#   4x larger get       131,072 DOUBLE -> 1,048,576 B, an octave up, so the
#                       payload-size SENSITIVITY of the high-water is a
#                       measurement instead of an assumption.  One array size
#                       cannot distinguish "stack scales with payload" from
#                       "payload is on the heap".
#   large array put     WRITE_NOTIFY with the same payload inbound.
#   subArray get        a windowed reply built through ArrayKind::SubArray.
#   deep FLNK chain     puts to RTEMS:E8:H.  MEASURED: this processes H and
#                       L1..L15 and bails at L16 on MAX_LINK_DEPTH = 16, so it
#                       drives the recursion to the engine's cap -- which is
#                       what makes the reported high-water depth-inclusive.
#   wide fan-out        puts to RTEMS:E8:FAN, 8 targets, to tell breadth from
#                       depth.
#   monitors            EVENT_ADD on the arrays and on the chain tail, so the
#                       CAS-event thread serialises big arrays too -- that
#                       thread is Medium and has its own high-water.
#
# Then it holds, so a census pass lands while the workload is at its deepest,
# and the STACKUSE lines are read from the console rather than from here.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 35064
CONNS = int(sys.argv[1]) if len(sys.argv) > 1 else 6
HOLD = float(sys.argv[2]) if len(sys.argv) > 2 else 120.0
TAG = sys.argv[3] if len(sys.argv) > 3 else "stackload"
ROUNDS = int(sys.argv[4]) if len(sys.argv) > 4 else 25
# Whether to also EVENT_ADD the 1 MiB WFBIG array.  MEASURED: with it on, 4
# connections drove MEM_USED 43,278,336 -> 211,804,160 B and the RTP died with
# `memory allocation of 1048576 bytes failed` -> signal 6, before a second
# census pass.  Off isolates the get/put reply path (which is what the
# CAS-client high-water is about) from the monitor event queues (which is what
# exhausted the heap), so the two can be reported as separate numbers.
MONBIG = int(sys.argv[5]) if len(sys.argv) > 5 else 1

T0 = time.time()
resource.setrlimit(resource.RLIMIT_NOFILE, (65536, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))

DBR_DOUBLE = 6
DBR_TIME_DOUBLE = 20
DBR_CTRL_DOUBLE = 34
WF_N = 32768
WFBIG_N = 131072
WF2_N = 8192
SA_N = 4096


def log(m):
    print("[%7.1fs] %s %s" % (time.time() - T0, TAG, m), flush=True)


def hdr(cmd, dtype, count, p1, p2, payload=b""):
    """Short header, or the 24-byte extended form when the payload or the
    element count will not fit the 16-bit fields.  CA_V49; the short fields
    carry the 0xFFFF/0 markers and the real values trail."""
    n = len(payload)
    if n >= 0xFFFF or count >= 0xFFFF:
        return (struct.pack(">HHHHII", cmd, 0xFFFF, dtype, 0, p1, p2)
                + struct.pack(">II", n, count) + payload)
    return struct.pack(">HHHHII", cmd, n, dtype, count, p1, p2) + payload


def msgs(buf):
    """Decode as many whole messages as `buf` holds, short or extended."""
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
    def __init__(self, timeout=30):
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sock.sendall(hdr(0, 0, 13, 0, 0))
        self.cid = 0
        self.timeout = timeout

    def pump(self, want, key=None, budget=None):
        """Read until a message with command `want` (and matching key) arrives."""
        deadline = time.time() + (budget or self.timeout)
        while time.time() < deadline:
            ms, self.buf = msgs(self.buf)
            for m in ms:
                if m[0] == 11:
                    raise RuntimeError("CA_PROTO_ERROR:%s"
                                       % m[5][16:].split(b"\0")[0].decode("latin1"))
                if m[0] == want and (key is None or m[4] == key):
                    return m
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
        # (sid, dtype, native count, cid)
        return (m[4], m[1], m[2], cid)

    def get(self, ch, count, dtype=DBR_DOUBLE):
        """READ_NOTIFY.  `dtype` is explicit because the plain DBR_DOUBLE reply
        is the SMALLEST of the reply shapes: DBR_CTRL_DOUBLE (34) prepends the
        full control block (limits, units, precision) and DBR_TIME_DOUBLE (20)
        a stamp.  A high-water measured only against DBR_DOUBLE cannot be
        called a worst case."""
        sid, _dt, _n, cid = ch
        ioid = 0x4000 + cid + (dtype << 8)
        self.sock.sendall(hdr(15, dtype, count, sid, ioid))
        m = self.pump(15, key=ioid)
        return len(m[5])

    def put(self, ch, values, notify=True):
        sid, dt, _n, cid = ch
        payload = struct.pack(">%dd" % len(values), *values)
        if len(payload) % 8:
            payload += b"\0" * (8 - len(payload) % 8)
        ioid = 0x5000 + cid
        self.sock.sendall(hdr(19 if notify else 4, DBR_DOUBLE, len(values), sid, ioid, payload))
        if notify:
            self.pump(19, key=ioid)

    def monitor(self, ch, count, dtype=DBR_DOUBLE):
        sid, _dt, _n, cid = ch
        subid = 0x6000 + cid + (dtype << 8)
        body = struct.pack(">fffHH", 0.0, 0.0, 0.0, 1 | 4, 0)
        self.sock.sendall(hdr(1, dtype, count, sid, subid, body))
        self.pump(1, key=subid)
        return subid

    def drain(self):
        """Consume whatever monitors have pushed, without blocking."""
        self.sock.setblocking(False)
        got = 0
        try:
            while True:
                chunk = self.sock.recv(1 << 20)
                if not chunk:
                    break
                self.buf += chunk
                got += len(chunk)
        except BlockingIOError:
            pass
        except OSError:
            pass
        finally:
            self.sock.setblocking(True)
            self.sock.settimeout(self.timeout)
        ms, self.buf = msgs(self.buf)
        return got, len(ms)


def classify(exc):
    if isinstance(exc, OSError) and exc.errno is not None:
        return "CONNECT_FAIL(errno=%d %s)" % (exc.errno, E.errorcode.get(exc.errno, "?"))
    s = str(exc)
    if "CA_PROTO_ERROR" in s:
        return "REFUSED_BY_SERVER(%s)" % s
    if "timeout" in s or isinstance(exc, TimeoutError):
        return "TIMEOUT"
    return "OTHER(%s: %s)" % (type(exc).__name__, s)


log("connections=%d rounds=%d hold=%.0fs" % (CONNS, ROUNDS, HOLD))

conns = []
for i in range(CONNS):
    try:
        c = Conn()
        ch = {
            "WF": c.channel("RTEMS:E8:WF"),
            "WFBIG": c.channel("RTEMS:E8:WFBIG"),
            "WF2": c.channel("RTEMS:E8:WF2"),
            "SA": c.channel("RTEMS:E8:SA"),
            "CMP": c.channel("RTEMS:E8:CMP"),
            "H": c.channel("RTEMS:E8:H"),
            "L32": c.channel("RTEMS:E8:L32"),
            "FAN": c.channel("RTEMS:E8:FAN"),
        }
        conns.append((c, ch))
        log("conn %d up, %d channels" % (i + 1, len(ch)))
    except Exception as e:
        log("conn %d FAILED: %s" % (i + 1, classify(e)))
        break

if not conns:
    log("no connections; nothing measured")
    raise SystemExit(1)

# Monitors first, so every later put pushes through CAS-event as well.
for i, (c, ch) in enumerate(conns):
    # (record, count, dbr).  The CTRL monitor on WF puts the metadata reply
    # shape on the CAS-event thread too, which is the thread whose own class is
    # being judged.
    mons = [("WF", WF_N, DBR_DOUBLE), ("WF", WF_N, DBR_CTRL_DOUBLE),
            ("WF2", WF2_N, DBR_DOUBLE), ("SA", SA_N, DBR_DOUBLE),
            ("L32", 1, DBR_DOUBLE), ("CMP", 1, DBR_DOUBLE)]
    if MONBIG:
        mons.insert(2, ("WFBIG", WFBIG_N, DBR_DOUBLE))
    for k, n, dbr in mons:
        try:
            c.monitor(ch[k], n, dbr)
        except Exception as e:
            log("conn %d monitor %s dbr=%d FAILED: %s" % (i + 1, k, dbr, classify(e)))
log("monitors established on %d connections (monbig=%d)" % (len(conns), MONBIG))

# Per-op accounting, not just the first outcome. Round 1 reported
# `put_chain TIMEOUT` with no number attached, which cannot be told from a
# notify that never arrives; `worst` turns that into a latency.
stats = {}


def note(name, ok, dt, detail):
    s = stats.setdefault(name, {"ok": 0, "fail": 0, "worst": 0.0, "first": None,
                                "firstfail": None})
    s["ok" if ok else "fail"] += 1
    s["worst"] = max(s["worst"], dt)
    if ok and s["first"] is None:
        s["first"] = detail
    if not ok and s["firstfail"] is None:
        s["firstfail"] = detail


for r in range(ROUNDS):
    for i, (c, ch) in enumerate(conns):
        for name, fn in (
            ("get_WF", lambda: c.get(ch["WF"], WF_N)),
            ("get_WFBIG", lambda: c.get(ch["WFBIG"], WFBIG_N)),
            ("get_WF2", lambda: c.get(ch["WF2"], WF2_N)),
            ("get_SA", lambda: c.get(ch["SA"], SA_N)),
            # The metadata reply shapes, on the biggest arrays: DBR_CTRL_DOUBLE
            # carries the whole control block ahead of the data,
            # DBR_TIME_DOUBLE a stamp.  Without these the high-water is only a
            # DBR_DOUBLE high-water.
            ("get_WF_ctrl", lambda: c.get(ch["WF"], WF_N, DBR_CTRL_DOUBLE)),
            ("get_WF_time", lambda: c.get(ch["WF"], WF_N, DBR_TIME_DOUBLE)),
            ("get_BIG_ctrl", lambda: c.get(ch["WFBIG"], WFBIG_N, DBR_CTRL_DOUBLE)),
            ("get_SA_ctrl", lambda: c.get(ch["SA"], SA_N, DBR_CTRL_DOUBLE)),
            ("put_WF", lambda: c.put(ch["WF"], [float(r)] * WF_N)),
            ("put_WFBIG", lambda: c.put(ch["WFBIG"], [float(r)] * WFBIG_N)),
            ("put_chain", lambda: c.put(ch["H"], [float(r)])),
            ("put_fan", lambda: c.put(ch["FAN"], [float(r)])),
        ):
            t = time.time()
            try:
                got = fn()
                note(name, True, time.time() - t,
                     "ok%s" % ("" if got is None else " bytes=%d" % got))
            except Exception as e:
                note(name, False, time.time() - t, classify(e))
            # Which (round, connection) a stall lands on is the only thing that
            # distinguishes a load-dependent outlier from a first-touch one.
            el = time.time() - t
            if el > 5.0:
                log("SLOW %s round=%d conn=%d t0=%.3f elapsed=%.3f"
                    % (name, r + 1, i + 1, t - T0, el))
        c.drain()
    if r == 0:
        for k in sorted(stats):
            log("round1 %-10s %s" % (k, stats[k]["first"] or stats[k]["firstfail"]))
log("%d rounds done over %d connections" % (ROUNDS, len(conns)))
for k in sorted(stats):
    s = stats[k]
    log("OP %-10s ok=%-4d fail=%-4d worst=%7.3fs first=%s firstfail=%s"
        % (k, s["ok"], s["fail"], s["worst"], s["first"], s["firstfail"]))

log("holding %.0fs so a census pass lands at the deepest point" % HOLD)
# The census fires every 6th reporter pass and the reporter is starved to
# ~33 s/pass under this load, so the hold must outlast several of those or only
# one sample lands.  Failures are COUNTED, not swallowed: a bare `except: pass`
# here hid an IOC that had died and made the hold look healthy.
end = time.time() + HOLD
hold_ok = hold_fail = 0
hold_first_fail = None
while time.time() < end:
    for c, ch in conns:
        try:
            c.get(ch["WF"], WF_N)
            c.get(ch["WFBIG"], WFBIG_N)
            c.get(ch["WF"], WF_N, DBR_CTRL_DOUBLE)
            c.get(ch["WFBIG"], WFBIG_N, DBR_CTRL_DOUBLE)
            c.put(ch["H"], [1.0])
            c.drain()
            hold_ok += 1
        except Exception as e:
            hold_fail += 1
            if hold_first_fail is None:
                hold_first_fail = classify(e)
                log("HOLD first failure at %.1fs into the hold: %s"
                    % (HOLD - (end - time.time()), hold_first_fail))
    time.sleep(0.5)
log("HOLD ok=%d fail=%d firstfail=%s" % (hold_ok, hold_fail, hold_first_fail))

for c, _ch in conns:
    try:
        c.sock.close()
    except Exception:
        pass
log("stackload done")
