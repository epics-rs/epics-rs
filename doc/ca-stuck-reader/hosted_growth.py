#!/usr/bin/env python3
"""Does the HOSTED driver's unbounded outbox grow while its drain is parked?

The RTEMS run answered this for the blocking driver: flat, because
`blocking.rs:1219` has the CAS-event thread write the socket DIRECTLY under
`send_lock`, so a parked client thread back-pressures the event thread into the
bounded EvQue ring, which coalesces. The hosted driver does not do that:
`monitor.rs:178` is `outbox.push(...)` into `mpsc::unbounded_channel()`
(`outbox.rs:73`), decoupled from the socket, so the ring always drains and the
growth moves into the queue.

This runs the hosted driver on loopback, which removes the confound that beat
runs 1-3 on the RTEMS rig: QEMU's SLIRP terminates TCP, so a small client
SO_RCVBUF did not produce guest-side back-pressure. On loopback there is no
intermediary, and `ss` shows the server's Send-Q directly -- a DIRECT check
that the drain is parked, not an inference from free memory.

  A  small SO_RCVBUF, subscribe, then flood READ_NOTIFY and read nothing.
  P  a putter driving the PV so the subscription keeps producing.
  -> sample the IOC's VmRSS and A's server-side Send-Q every SAMPLE seconds.
"""
import os
import re
import socket
import struct
import subprocess
import sys
import threading
import time

HOST = "127.0.0.1"
PORT = int(os.environ.get("CAPORT", "5099"))
PV = b"DRV"

CA_VERSION, CA_EVENT_ADD, CA_WRITE, CA_READ_NOTIFY = 0, 1, 4, 15
CA_CREATE_CHAN, CA_CLIENT_NAME, CA_HOST_NAME = 18, 20, 21
CA_CREATE_CH_FAIL = 26
MINOR = 13
DBE_VALUE_ALARM = 1 | 4
DBR_DOUBLE = 6

HOLD = int(sys.argv[1]) if len(sys.argv) > 1 else 300
IOC_PID = int(sys.argv[2])
SAMPLE = 10
FLOOD = 500000
PUT_HZ = 100.0

stop_putter = threading.Event()


def hdr(cmd, payload=b"", dtype=0, count=0, p1=0, p2=0):
    payload += b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, count, p1, p2) + payload


def read_exact(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError("peer closed")
        b += c
    return b


def next_msg(s):
    cmd, psize, dtype, count, p1, p2 = struct.unpack(">HHHHII", read_exact(s, 16))
    return cmd, dtype, count, p1, p2, (read_exact(s, psize) if psize else b"")


def open_client(cid, rcvbuf=None, name=b"c"):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if rcvbuf:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
    s.settimeout(15)
    s.connect((HOST, PORT))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.sendall(hdr(CA_VERSION, dtype=0, count=MINOR))
    s.sendall(hdr(CA_CLIENT_NAME, name))
    s.sendall(hdr(CA_HOST_NAME, b"local"))
    s.sendall(hdr(CA_CREATE_CHAN, PV, p1=cid, p2=MINOR))
    deadline = time.time() + 10
    while time.time() < deadline:
        cmd, dtype, count, p1, p2, _ = next_msg(s)
        if cmd == CA_CREATE_CHAN and p1 == cid:
            return s, p2, dtype, count
        if cmd == CA_CREATE_CH_FAIL:
            raise RuntimeError(f"server refused {PV!r}")
    raise TimeoutError("no CREATE_CHAN reply")


def putter():
    """Drive the PV so the subscription has something to post."""
    s, sid, dt, ct = open_client(0x9101, name=b"putter")
    v = 0.0
    period = 1.0 / PUT_HZ
    try:
        while not stop_putter.is_set():
            v += 1.0
            s.sendall(hdr(CA_WRITE, struct.pack(">d", v),
                          dtype=DBR_DOUBLE, count=1, p1=sid, p2=0x9101))
            time.sleep(period)
    except OSError as e:
        print(f"putter died: {e}", flush=True)
    finally:
        s.close()


def rss_kb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(re.search(r"(\d+)", line).group(1))
    return None


def server_queues(local_port):
    """Server-side (Recv-Q, Send-Q) for the connection whose PEER port matches.

    Send-Q pegged is the direct proof the drain is parked in `poll_write`;
    Recv-Q non-empty says the read loop stopped consuming the flood. Neither
    was observable through QEMU's SLIRP, which terminates TCP.
    """
    out = subprocess.run(["ss", "-tn", f"sport = :{PORT}"],
                         capture_output=True, text=True).stdout
    for line in out.splitlines()[1:]:
        f = line.split()
        if len(f) >= 5 and f[4].endswith(f":{local_port}"):
            return int(f[1]), int(f[2])
    return None, None


def main():
    a, sid_a, dt_a, ct_a = open_client(0x9001, rcvbuf=2048, name=b"stuck")
    a_port = a.getsockname()[1]
    print(f"A local port {a_port}, IOC pid {IOC_PID}", flush=True)

    t = threading.Thread(target=putter, daemon=True)
    t.start()
    time.sleep(1)

    try:
        # 1. Prove the subscription actually produces before parking anything.
        body = struct.pack(">fffHH", 0.0, 0.0, 0.0, DBE_VALUE_ALARM, 0)
        a.sendall(hdr(CA_EVENT_ADD, body, dtype=dt_a, count=ct_a, p1=sid_a, p2=0x33))
        seen, t0 = 0, time.time()
        a.settimeout(3)
        while time.time() - t0 < 10:
            try:
                cmd, *_ = next_msg(a)
                if cmd == CA_EVENT_ADD:
                    seen += 1
            except (socket.timeout, TimeoutError):
                break
        rate = seen / max(time.time() - t0, 1e-9)
        print(f"phase 0: {seen} events in {time.time()-t0:.1f}s ({rate:.1f}/s)", flush=True)
        if seen < 5:
            print("phase 0: PRODUCER TOO SLOW -- this run cannot answer the "
                  "question.", flush=True)
            return

        base_rss = rss_kb(IOC_PID)
        print(f"baseline VmRSS = {base_rss} kB", flush=True)

        # 2. Park the drain. Loopback wmem autotunes to several MB, so the flood
        #    has to be far larger than the RTEMS one to fill it.
        req = b"".join(
            hdr(CA_READ_NOTIFY, dtype=dt_a, count=ct_a, p1=sid_a, p2=i)
            for i in range(FLOOD)
        )
        a.setblocking(False)
        sent, t0 = 0, time.time()
        while sent < len(req) and time.time() - t0 < 60:
            try:
                sent += a.send(req[sent:])
            except BlockingIOError:
                time.sleep(0.02)
        print(f"A: flooded {sent // 16} READ_NOTIFY "
              f"(~{sent * 40 // 16 // 1024} KB of replies), reading nothing", flush=True)

        # 3. Watch RSS and the server's Send-Q together.
        start = time.time()
        while time.time() - start < HOLD:
            time.sleep(SAMPLE)
            cur = rss_kb(IOC_PID)
            rq, sq = server_queues(a_port)
            print(f"t+{int(time.time()-start):4d}s  VmRSS={cur} kB  "
                  f"delta={cur - base_rss:+d} kB  server Recv-Q={rq} Send-Q={sq}",
                  flush=True)
    finally:
        stop_putter.set()
        try:
            a.close()
        except OSError:
            pass


if __name__ == "__main__":
    main()
