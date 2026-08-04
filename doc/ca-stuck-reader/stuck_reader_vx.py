#!/usr/bin/env python3
"""Does a CA client that stops reading ever get reaped by the RTEMS server?

Item 4's measurement. The code analysis says no: a zero-window peer runs
rtems-libbsd's PERSIST timer, whose drop needs `ticks - t_rcvtime >=
tcp_maxpersistidle` (tcp_timer.c:540-541) while `t_rcvtime` is refreshed by
every inbound segment (tcp_input.c:1596) -- and the window probes such a peer
keeps ACKing ARE inbound segments. So nothing should reap it, and the server's
`write_frame_locked` should stay parked in `write` under the send lock.

Two sockets against the live guest on 127.0.0.1:5064:

  A: handshake, create a channel, pipeline N READ_NOTIFY requests, then never
     read. Its 2 KB receive buffer fills, the window shuts, and the server's
     write for this client parks.
  B: an ordinary client, opened and exercised repeatedly while A is parked. If
     B keeps getting answers, A wedges only its own CAS thread; if B stalls,
     one stuck reader takes the whole IOC down.

Read-only with respect to the guest: no puts, and both sockets are closed on
every exit path so the server reaps them when the run ends.
"""
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", int(__import__("os").environ.get("CAPORT", "5064"))
PV = b"RTEMS:MEM_FREE"

CA_VERSION, CA_READ_NOTIFY, CA_CREATE_CHAN = 0, 15, 18
CA_CLIENT_NAME, CA_HOST_NAME = 20, 21
CA_CREATE_CH_FAIL = 26
MINOR = 13

DURATION = int(sys.argv[1]) if len(sys.argv) > 1 else 300
SAMPLE = 30


def hdr(cmd, payload=b"", dtype=0, count=0, p1=0, p2=0):
    pad = (-len(payload)) % 8
    payload += b"\0" * pad
    return struct.pack(">HHHHII", cmd, len(payload), dtype, count, p1, p2) + payload


def handshake(sock, name=b"stuckreader"):
    sock.sendall(hdr(CA_VERSION, dtype=0, count=MINOR))
    sock.sendall(hdr(CA_CLIENT_NAME, name))
    sock.sendall(hdr(CA_HOST_NAME, b"rig"))


def read_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError("peer closed")
        buf += chunk
    return buf


def next_msg(sock):
    h = read_exact(sock, 16)
    cmd, psize, dtype, count, p1, p2 = struct.unpack(">HHHHII", h)
    body = read_exact(sock, psize) if psize else b""
    return cmd, dtype, count, p1, p2, body


def create_channel(sock, cid):
    sock.sendall(hdr(CA_CREATE_CHAN, PV, p1=cid, p2=MINOR))
    deadline = time.time() + 10
    while time.time() < deadline:
        cmd, dtype, count, p1, p2, _ = next_msg(sock)
        if cmd == CA_CREATE_CHAN and p1 == cid:
            return p2, dtype, count
        if cmd == CA_CREATE_CH_FAIL:
            raise RuntimeError(f"server refused the channel {PV!r}")
    raise TimeoutError("no CREATE_CHAN reply")


def open_client(rcvbuf=None, name=b"stuckreader"):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if rcvbuf:
        # Must precede connect: the window scale is negotiated in the SYN.
        s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
    s.settimeout(15)
    s.connect((HOST, PORT))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    handshake(s, name)
    return s


def peer_state(sock):
    """Still connected, or did the server drop us?"""
    try:
        sock.getpeername()
    except OSError as e:
        return f"DROPPED ({e.errno})"
    # A zero-length send surfaces a pending RST without moving data.
    try:
        sock.send(b"")
        return "connected"
    except OSError as e:
        return f"DROPPED ({e.errno})"


def main():
    a = open_client(rcvbuf=2048, name=b"stuck-A")
    try:
        sid, dtype, count = create_channel(a, cid=0x5AAA)
        print(f"A: channel up, sid={sid} native dtype={dtype} count={count}", flush=True)

        # Pipeline far more replies than a 2 KB window can hold, then stop
        # reading. From here the server's write for A must park.
        n = 50000
        req = b"".join(
            hdr(CA_READ_NOTIFY, dtype=dtype, count=count, p1=sid, p2=i)
            for i in range(n)
        )
        a.setblocking(False)
        sent = 0
        t0 = time.time()
        while sent < len(req) and time.time() - t0 < 30:
            try:
                sent += a.send(req[sent:])
            except BlockingIOError:
                time.sleep(0.05)
        print(f"A: issued {sent // 16} READ_NOTIFY requests, now reading nothing", flush=True)

        start = time.time()
        while time.time() - start < DURATION:
            time.sleep(SAMPLE)
            elapsed = int(time.time() - start)
            state = peer_state(a)

            # Is the IOC still serving anybody else?
            t_b = time.time()
            try:
                b = open_client(name=b"probe-B")
                sid_b, dt_b, ct_b = create_channel(b, cid=0x5BBB)
                b.sendall(hdr(CA_READ_NOTIFY, dtype=dt_b, count=ct_b, p1=sid_b, p2=1))
                deadline = time.time() + 15
                got = None
                while time.time() < deadline:
                    cmd, _, _, _, ioid, body = next_msg(b)
                    if cmd == CA_READ_NOTIFY and ioid == 1:
                        got = body
                        break
                b.close()
                verdict = "SERVED" if got is not None else "NO REPLY"
            except Exception as e:  # noqa: BLE001 - the verdict is the point
                verdict = f"FAILED ({type(e).__name__}: {e})"
            rtt = time.time() - t_b

            print(f"t+{elapsed:4d}s  A={state:24s}  B={verdict} in {rtt:.2f}s", flush=True)

        print(f"\nfinal: A={peer_state(a)} after {DURATION}s of reading nothing", flush=True)
    finally:
        a.close()
        print("A closed; the server may now reap it.", flush=True)


if __name__ == "__main__":
    main()
