#!/usr/bin/env python3
"""Does a stuck MONITOR client grow the server's outbox without bound?

Run 1 (stuck_reader.py) showed a stuck reader is never reaped and wedges only
its own thread. It used pipelined READ_NOTIFY, which cannot grow the outbox:
once the drain parks, the read loop stops dispatching, so no new replies are
produced.

Monitors are different. `spawn_monitor_*` tasks push into the per-connection
outbox independently of the read loop, and that outbox is
`mpsc::unbounded_channel()` (server/outbox.rs:73), justified at :68 by "the
sole draining owner pulls the queue empty" -- which stops being true exactly
when the drain parks in `write`. So a camonitor client that stops reading
should make the server's free memory fall without limit.

  A: EVENT_ADD subscription, tiny receive buffer, then reads nothing.
  B: reads RTEMS:MEM_FREE every SAMPLE seconds and prints it.

Phase 0 first checks the subscription actually ticks; without a producer the
experiment would prove nothing and say so.

OUTCOME (added after the run, see ../ca-stuck-reader-measurement.md): this did
NOT establish the hypothesis. At 1.10 events/s the 1200 s run only ever put
~66 KB in flight, which QEMU's SLIRP hostfwd and the host socket buffers absorb
outright, so A's small SO_RCVBUF never closed the guest's window and the drain
never parked. Free memory moved 216 B and then held. The script is kept as the
record of what was run; it needs a waveform-rate producer to be conclusive.
"""
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 5064
SUB_PV = b"RTEMS:MEM_FREE"
WATCH_PV = b"RTEMS:MEM_FREE"

CA_VERSION, CA_EVENT_ADD, CA_READ_NOTIFY = 0, 1, 15
CA_CREATE_CHAN, CA_CLIENT_NAME, CA_HOST_NAME = 18, 20, 21
CA_CREATE_CH_FAIL = 26
MINOR = 13
DBE_VALUE_ALARM = 1 | 4

HOLD = int(sys.argv[1]) if len(sys.argv) > 1 else 600
SAMPLE = 30


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


def open_client(pv, cid, rcvbuf=None, name=b"mon"):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    if rcvbuf:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
    s.settimeout(15)
    s.connect((HOST, PORT))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.sendall(hdr(CA_VERSION, dtype=0, count=MINOR))
    s.sendall(hdr(CA_CLIENT_NAME, name))
    s.sendall(hdr(CA_HOST_NAME, b"rig"))
    s.sendall(hdr(CA_CREATE_CHAN, pv, p1=cid, p2=MINOR))
    deadline = time.time() + 10
    while time.time() < deadline:
        cmd, dtype, count, p1, p2, _ = next_msg(s)
        if cmd == CA_CREATE_CHAN and p1 == cid:
            return s, p2, dtype, count
        if cmd == CA_CREATE_CH_FAIL:
            raise RuntimeError(f"server refused {pv!r}")
    raise TimeoutError("no CREATE_CHAN reply")


def subscribe(s, sid, dtype, count, subid):
    body = struct.pack(">fffHH", 0.0, 0.0, 0.0, DBE_VALUE_ALARM, 0)
    s.sendall(hdr(CA_EVENT_ADD, body, dtype=dtype, count=count, p1=sid, p2=subid))


def mem_free(s, sid, dtype, count, ioid):
    s.sendall(hdr(CA_READ_NOTIFY, dtype=dtype, count=count, p1=sid, p2=ioid))
    deadline = time.time() + 15
    while time.time() < deadline:
        cmd, dt, _, _, got, body = next_msg(s)
        if cmd == CA_READ_NOTIFY and got == ioid:
            # dtype 6 == DBR_DOUBLE
            return struct.unpack(">d", body[:8])[0] if dt == 6 else None
    return None


def alive(s):
    try:
        s.send(b"")
        return "connected"
    except OSError as e:
        return f"DROPPED ({e.errno})"


def main():
    # Phase 0: does the subscription actually tick? Read normally for 20 s.
    probe, sid, dt, ct = open_client(SUB_PV, 0x7001, name=b"probe-tick")
    subscribe(probe, sid, dt, ct, 0x11)
    ticks, t0 = 0, time.time()
    probe.settimeout(3)
    while time.time() - t0 < 20:
        try:
            cmd, *_ = next_msg(probe)
            if cmd == CA_EVENT_ADD:
                ticks += 1
        except (socket.timeout, TimeoutError):
            pass
    probe.close()
    rate = ticks / 20.0
    print(f"phase 0: subscription delivered {ticks} events in 20s ({rate:.2f}/s)", flush=True)
    if ticks <= 1:
        print("phase 0: NO PRODUCER -- only the initial value arrived. A stuck "
              "monitor cannot grow the outbox on this PV, so the growth "
              "question is NOT ANSWERED by this run.", flush=True)
        return

    a, sid_a, dt_a, ct_a = open_client(SUB_PV, 0x7002, rcvbuf=2048, name=b"stuck-mon")
    b, sid_b, dt_b, ct_b = open_client(WATCH_PV, 0x7003, name=b"watch")
    try:
        base = mem_free(b, sid_b, dt_b, ct_b, 1)
        print(f"baseline MEM_FREE = {base}", flush=True)

        subscribe(a, sid_a, dt_a, ct_a, 0x22)
        print(f"A: subscribed at {rate:.2f} events/s, now reading nothing", flush=True)

        start, ioid = time.time(), 2
        while time.time() - start < HOLD:
            time.sleep(SAMPLE)
            cur = mem_free(b, sid_b, dt_b, ct_b, ioid)
            ioid += 1
            delta = (cur - base) if (cur is not None and base is not None) else None
            print(f"t+{int(time.time()-start):4d}s  A={alive(a):20s}  "
                  f"MEM_FREE={cur}  delta={delta}", flush=True)

        print(f"\nfinal: A={alive(a)}", flush=True)
        a.close()
        time.sleep(10)
        rec = mem_free(b, sid_b, dt_b, ct_b, 9999)
        print(f"after closing A, MEM_FREE={rec} (baseline {base})", flush=True)
    finally:
        try:
            a.close()
        except OSError:
            pass
        b.close()


if __name__ == "__main__":
    main()
