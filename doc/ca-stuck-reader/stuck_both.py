#!/usr/bin/env python3
"""Does the unbounded outbox actually grow while the drain is parked?

Run 2 failed because a 1.1 Hz scalar subscription only puts ~66 KB in flight
over 20 minutes, which QEMU's SLIRP hostfwd absorbs outright -- the guest's
window never shut, so the drain never parked and there was nothing to queue.

Run 1 did not have that problem: 50,000 pipelined READ_NOTIFY produce ~2 MB of
replies, far past SLIRP's ~64 KB of per-socket buffering, so the guest's window
must close. This run combines the two on ONE connection:

  A: subscribe (EVENT_ADD) and wait for a real monitor event, proving the
     producer is live. THEN flood 50,000 READ_NOTIFY to park the drain. Then
     read nothing.
  B: sample RTEMS:MEM_FREE every 30 s.

With the drain parked, the monitor producer task keeps pushing into the
per-connection outbox (`mpsc::unbounded_channel`, server/outbox.rs:73). If that
queue is genuinely unbounded, free memory must fall steadily at roughly the
event rate times the frame size (~55 B/s here, i.e. ~1.6 KB per sample) --
well above the 216 B resolution run 2 demonstrated. Flat memory instead means
something bounds it.

OUTCOME (added after the run, see ../ca-stuck-reader-measurement.md): flat. Two
216 B steps in 1200 s, and the second half of the run moved 0 B. What bounds it
is structural: this guest runs server/blocking.rs, whose CAS-event thread writes
the socket DIRECTLY under send_lock (blocking.rs:1219) rather than pushing into
the outbox, so a parked read thread back-pressures the event thread into the
bounded EvQue ring, which coalesces -- C's own client->lock structure. The
unbounded outbox is the HOSTED driver's; hosted_growth.py measures that one at
9.34 kB/s with the drain provably parked.
"""
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 5064
PV = b"RTEMS:MEM_FREE"

CA_VERSION, CA_EVENT_ADD, CA_READ_NOTIFY = 0, 1, 15
CA_CREATE_CHAN, CA_CLIENT_NAME, CA_HOST_NAME = 18, 20, 21
CA_CREATE_CH_FAIL = 26
MINOR = 13
DBE_VALUE_ALARM = 1 | 4

HOLD = int(sys.argv[1]) if len(sys.argv) > 1 else 1200
SAMPLE = 30
FLOOD = 50000


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
    s.settimeout(20)
    s.connect((HOST, PORT))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.sendall(hdr(CA_VERSION, dtype=0, count=MINOR))
    s.sendall(hdr(CA_CLIENT_NAME, name))
    s.sendall(hdr(CA_HOST_NAME, b"rig"))
    s.sendall(hdr(CA_CREATE_CHAN, PV, p1=cid, p2=MINOR))
    deadline = time.time() + 10
    while time.time() < deadline:
        cmd, dtype, count, p1, p2, _ = next_msg(s)
        if cmd == CA_CREATE_CHAN and p1 == cid:
            return s, p2, dtype, count
        if cmd == CA_CREATE_CH_FAIL:
            raise RuntimeError(f"server refused {PV!r}")
    raise TimeoutError("no CREATE_CHAN reply")


def mem_free(s, sid, dtype, count, ioid):
    s.sendall(hdr(CA_READ_NOTIFY, dtype=dtype, count=count, p1=sid, p2=ioid))
    deadline = time.time() + 15
    while time.time() < deadline:
        cmd, dt, _, _, got, body = next_msg(s)
        if cmd == CA_READ_NOTIFY and got == ioid:
            return struct.unpack(">d", body[:8])[0] if dt == 6 else None
    return None


def alive(s):
    try:
        s.send(b"")
        return "connected"
    except OSError as e:
        return f"DROPPED ({e.errno})"


def main():
    a, sid_a, dt_a, ct_a = open_client(0x8001, rcvbuf=2048, name=b"stuck-both")
    b, sid_b, dt_b, ct_b = open_client(0x8002, name=b"watch")
    try:
        # 1. Subscribe and prove the producer is live BEFORE parking anything.
        body = struct.pack(">fffHH", 0.0, 0.0, 0.0, DBE_VALUE_ALARM, 0)
        a.sendall(hdr(CA_EVENT_ADD, body, dtype=dt_a, count=ct_a, p1=sid_a, p2=0x33))
        seen, t0 = 0, time.time()
        a.settimeout(5)
        while seen < 3 and time.time() - t0 < 30:
            try:
                cmd, *_ = next_msg(a)
                if cmd == CA_EVENT_ADD:
                    seen += 1
            except (socket.timeout, TimeoutError):
                break
        print(f"A: subscription live, {seen} events read before parking", flush=True)
        if seen < 2:
            print("A: producer did not tick -- ABORT, this run cannot answer "
                  "the question.", flush=True)
            return

        base = mem_free(b, sid_b, dt_b, ct_b, 1)
        print(f"baseline MEM_FREE = {base}", flush=True)

        # 2. Park the drain with a flood far larger than SLIRP can absorb.
        req = b"".join(
            hdr(CA_READ_NOTIFY, dtype=dt_a, count=ct_a, p1=sid_a, p2=i)
            for i in range(FLOOD)
        )
        a.setblocking(False)
        sent, t0 = 0, time.time()
        while sent < len(req) and time.time() - t0 < 40:
            try:
                sent += a.send(req[sent:])
            except BlockingIOError:
                time.sleep(0.05)
        print(f"A: flooded {sent // 16} READ_NOTIFY (~{sent * 40 // 16 // 1024} KB of "
              f"replies), reading nothing from here on", flush=True)

        # 3. Watch memory while the monitor keeps producing into a parked drain.
        start, ioid = time.time(), 2
        series = []
        while time.time() - start < HOLD:
            time.sleep(SAMPLE)
            cur = mem_free(b, sid_b, dt_b, ct_b, ioid)
            ioid += 1
            if cur is not None and base is not None:
                series.append((int(time.time() - start), cur - base))
            print(f"t+{int(time.time()-start):4d}s  A={alive(a):20s}  "
                  f"MEM_FREE={cur}  delta={cur - base if cur and base else None}",
                  flush=True)

        # 4. Verdict from the shape, not from one sample.
        if len(series) >= 4:
            first_half = series[: len(series) // 2]
            second_half = series[len(series) // 2:]
            d1 = first_half[-1][1] - first_half[0][1]
            d2 = second_half[-1][1] - second_half[0][1]
            print(f"\nfirst half moved {d1} B, second half moved {d2} B", flush=True)
            distinct = len({d for _, d in series})
            print(f"{distinct} distinct delta values across {len(series)} samples",
                  flush=True)

        a.close()
        time.sleep(15)
        rec = mem_free(b, sid_b, dt_b, ct_b, 99999)
        print(f"after closing A, MEM_FREE={rec} (baseline {base})", flush=True)
    finally:
        try:
            a.close()
        except OSError:
            pass
        b.close()


if __name__ == "__main__":
    main()
