# rtemscmds-e10.py — which CA request shapes does the blocking driver actually
# serve?  The stack high-water is only a bound on the paths that RAN, so the
# load driver's sweep has to be checked one command at a time: a request the
# server answers with "command not yet supported" never reached an encoder and
# contributes nothing to the reading.
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 25164


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


def one(label, send, pv="RTEMS:AO", wait=2.0):
    s = socket.create_connection((HOST, PORT), timeout=10)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.sendall(hdr(0, 0, 0, 13, 0, 0))
    nm = pad(pv)
    s.sendall(hdr(18, len(nm), 0, 0, 1, 13) + nm)
    buf = b""
    sid = dt = dc = None
    deadline = time.time() + 8
    while time.time() < deadline and sid is None:
        s.settimeout(max(0.1, deadline - time.time()))
        try:
            c = s.recv(65536)
        except socket.timeout:
            break
        if not c:
            break
        buf += c
        ms, buf = msgs(buf)
        for (cmd, _ps, t, n, p1, p2, _pl) in ms:
            if cmd == 18 and p1 == 1:
                sid, dt, dc = p2, t, n
    if sid is None:
        print("%-34s NO CHANNEL" % label, flush=True)
        s.close()
        return
    send(s, sid, dt, dc)
    got = []
    deadline = time.time() + wait
    while time.time() < deadline:
        s.settimeout(max(0.05, deadline - time.time()))
        try:
            c = s.recv(65536)
        except socket.timeout:
            break
        if not c:
            got.append("EOF")
            break
        buf += c
        ms, buf = msgs(buf)
        for (cmd, ps, t, n, _p1, p2, pl) in ms:
            if cmd == 11:
                got.append("ERROR(status=%d %r)" % (p2, pl[16:].split(b"\0")[0].decode("latin1")))
            else:
                got.append("cmd=%d psize=%d dtype=%d dcount=%d" % (cmd, ps, t, n))
    print("%-34s -> %s" % (label, got if got else "SILENCE"), flush=True)
    s.close()


FAM = [("native", 0), ("STS", 7), ("TIME", 14), ("GR", 21), ("CTRL", 28)]
for name, off in FAM:
    one("READ_NOTIFY dbr_%s" % name,
        lambda s, sid, dt, dc, o=off: s.sendall(hdr(15, 0, dt + o, dc, sid, 77)))
for name, off in FAM:
    one("legacy READ(7) dbr_%s" % name,
        lambda s, sid, dt, dc, o=off: s.sendall(hdr(7, 0, dt + o, dc, sid, 78)))
for name, off in FAM:
    one("EVENT_ADD dbr_%s" % name,
        lambda s, sid, dt, dc, o=off: s.sendall(
            hdr(1, 16, dt + o, dc, sid, 79) + struct.pack(">fffHH", 0, 0, 0, 7, 0)))
one("WRITE(4) double",
    lambda s, sid, dt, dc: s.sendall(hdr(4, 8, 6, 1, sid, 80) + struct.pack(">d", 2.5)))
one("WRITE_NOTIFY(19) double",
    lambda s, sid, dt, dc: s.sendall(hdr(19, 8, 6, 1, sid, 81) + struct.pack(">d", 3.5)))
one("SEARCH(6) over TCP",
    lambda s, sid, dt, dc: s.sendall(hdr(6, len(pad("RTEMS:LO")), 5, 13, 82, 82) + pad("RTEMS:LO")))
one("ECHO(23)", lambda s, sid, dt, dc: s.sendall(hdr(23, 0, 0, 0, 0, 0)))
one("EVENTS_OFF(8)", lambda s, sid, dt, dc: s.sendall(hdr(8, 0, 0, 0, 0, 0)))
one("EVENTS_ON(9)", lambda s, sid, dt, dc: s.sendall(hdr(9, 0, 0, 0, 0, 0)))
one("READ_NOTIFY dcount=4096",
    lambda s, sid, dt, dc: s.sendall(hdr(15, 0, dt, 4096, sid, 83)))
one("READ_NOTIFY dbr=199",
    lambda s, sid, dt, dc: s.sendall(hdr(15, 0, 199, 1, sid, 84)))
one("READ_NOTIFY unknown sid",
    lambda s, sid, dt, dc: s.sendall(hdr(15, 0, 6, 1, 0x7fffffff, 85)))
one("CREATE_CHAN missing pv",
    lambda s, sid, dt, dc: s.sendall(
        hdr(18, len(pad("RTEMS:NO:SUCH")), 0, 0, 4242, 13) + pad("RTEMS:NO:SUCH")))
one("CLEAR_CHANNEL(12)",
    lambda s, sid, dt, dc: s.sendall(hdr(12, 0, 0, 0, sid, 1)))
print("done", flush=True)
sys.exit(0)
