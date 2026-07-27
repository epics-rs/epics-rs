# rampprobe-e12.py — the same ramp as rampprobe-e11.py, with the one thing the
# E11 log could not answer split out: is the 7.2 s the connect, or the CA
# handshake after it?
#
# The E11 arms report a single `conn=` covering `socket.create_connection` plus
# the whole VERSION / CREATE_CHAN / READ_NOTIFY exchange, and above held=82 it
# sits flat at 7.2-7.3 s across three arms at three different guest RAMs.  A
# flat plateau is a fixed constant, not congestion, and 7 s is exactly the
# 1+2+4 SYN retransmission ladder — but that reading only holds if the delay is
# in the CONNECT.  So this times four things separately:
#
#   connect_s    TCP connect only
#   ver_s        to the server's VERSION reply (the accept loop has handed off)
#   chan_s       to CREATE_CHAN_RESP (the per-client thread is dispatching)
#   read_s       to the first READ_NOTIFY reply (the database is answering)
#
# connect_s large + ver_s small  => the accept side is not running: the listen
#   backlog overflows and SYNs are dropped, i.e. CAS-TCP is not getting the CPU.
# connect_s small + ver_s large  => the client was accepted promptly and the
#   delay is behind the accept, in the per-client worker.
#
# Ports and discipline as rampprobe-e11.py: 25064, this panel's block.
import errno as E
import resource
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25064
CEILING = int(sys.argv[1]) if len(sys.argv) > 1 else 200
TAG = sys.argv[2] if len(sys.argv) > 2 else "e12"
MAXSEC = float(sys.argv[3]) if len(sys.argv) > 3 else 900.0
PACE = float(sys.argv[4]) if len(sys.argv) > 4 else 1.0

T0 = time.time()
resource.setrlimit(resource.RLIMIT_NOFILE, (65536, resource.getrlimit(resource.RLIMIT_NOFILE)[1]))


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
    def __init__(self, pv, cid, timeout=25):
        t = time.time()
        self.sock = socket.create_connection((HOST, PORT), timeout=timeout)
        self.t_connect = time.time() - t
        self.t_ver = self.t_chan = self.t_read = None
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""
        self.sid = self.dtype = self.dcount = self.first = None
        nm = pad(pv)
        self.sock.sendall(hdr(0, 0, 0, 13, 0, 0))
        self.sock.sendall(hdr(6, len(nm), 10, 13, cid, cid) + nm)
        sent_create = False
        deadline = time.time() + timeout
        while time.time() < deadline:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("EOF-after-accept")
            self.buf += chunk
            ms, self.buf = msgs(self.buf)
            for (cmd, _ps, dt, dc, _p1, p2, pl) in ms:
                if cmd == 11:
                    raise Refused(p2, pl[16:].split(b"\0")[0].decode("latin1"))
                if cmd == 0 and self.t_ver is None:
                    self.t_ver = time.time() - t
                if cmd == 6 and not sent_create:
                    if self.t_ver is None:
                        self.t_ver = time.time() - t
                    self.sock.sendall(hdr(18, len(nm), 0, 0, cid, 13) + nm)
                    sent_create = True
                elif cmd == 18:
                    self.t_chan = time.time() - t
                    self.sid, self.dtype, self.dcount = p2, dt, dc
                    self.sock.sendall(hdr(15, 0, dt, dc, self.sid, cid))
                elif cmd == 15:
                    self.t_read = time.time() - t
                    self.first = pl[:8]
                    return
        raise RuntimeError("handshake-timeout")


def classify(exc):
    if isinstance(exc, Refused):
        return "REFUSED_BY_SERVER(status=%d text=%r)" % (exc.status, exc.text)
    if isinstance(exc, OSError) and exc.errno is not None:
        return "CONNECT_FAIL(errno=%d %s)" % (exc.errno, E.errorcode.get(exc.errno, "?"))
    s = str(exc)
    if "EOF-after-accept" in s:
        return "ACCEPTED_THEN_EOF"
    if "timeout" in s:
        return "HANDSHAKE_TIMEOUT"
    return "OTHER(%s: %s)" % (type(exc).__name__, s)


log("ceiling=%d pace=%.2fs deadline=%.0fs port=%d SPLIT-TIMING" % (CEILING, PACE, MAXSEC, PORT))
held = []
while len(held) < CEILING:
    if time.time() - T0 > MAXSEC:
        log("INTERNAL DEADLINE %.0fs at held=%d" % (MAXSEC, len(held)))
        break
    t = time.time()
    try:
        c = Chan("RTEMS:AO", 1 + len(held))
        held.append(c)
        log("ATTEMPT %3d OK held=%3d total=%.2fs connect=%.2fs ver=%s chan=%s read=%s"
            % (len(held), len(held), time.time() - t, c.t_connect,
               "%.2f" % c.t_ver if c.t_ver is not None else "-",
               "%.2f" % c.t_chan if c.t_chan is not None else "-",
               "%.2f" % c.t_read if c.t_read is not None else "-"))
    except Exception as e:
        log("WALL attempt=%d held=%d elapsed=%.2fs %s"
            % (len(held) + 1, len(held), time.time() - t, classify(e)))
        break
    slack = PACE - (time.time() - t)
    if slack > 0:
        time.sleep(slack)

log("=== TOP held=%d ===" % len(held))
for c in held:
    try:
        c.sock.close()
    except Exception:
        pass
log("released %d" % len(held))
