"""Control experiment for the '+1 block per cancelled monitor' row of the
prediction table in c-base-rtems-posix-event-leak.md.

Same rig and same discipline as evleak.py (concurrency exactly 1, warm-up before
baseline, two batches), but each cycle additionally creates a channel on the
10 Hz record and subscribes a monitor, then closes with that monitor still
active.  destroyAllChannels -> db_cancel_event may then take the db_sync_event
path (dbEvent.c:632), which creates and destroys one MORE epicsEvent
(dbEvent.c:572/:591) -- i.e. a sixth leaked block.

That path is CONDITIONAL: db_cancel_event only calls db_sync_event when a
callback for the subscription is pending or in progress concurrently with
event_task (dbEvent.c:612-617).  So the expected slope is NOT an integer -- it
is between 5 and 6 blocks/cycle, and where it lands is a measure of how often
the race is won, not a constant of the code.  It is reported as such.
"""
import socket, struct, time, os

HOST, PORT = "127.0.0.1", 5164
FIFO = os.path.expanduser("~/rtems-cside/ciocin")
PV   = b"CIOC:HB100\0"      # SCAN = .1 second, so a callback is often in flight

GAP     = 0.04
HOLD    = 0.15   # keep the monitor up across at least one 10 Hz update
SETTLE  = 6.0
WARMUP  = 60
BATCH_A = 200
BATCH_B = 400

def hdr(cmd, payload=b"", dtype=0, dcount=0, p1=0, p2=0):
    payload = payload + b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, dcount, p1, p2) + payload

HELLO = hdr(0, dtype=0, dcount=13) + hdr(20, b"probe\0") + hdr(21, b"probe\0")

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

def find_create_chan_reply(buf):
    """walk 16-byte CA headers, return (dtype, dcount, sid) of the CREATE_CHAN reply"""
    i = 0
    while i + 16 <= len(buf):
        cmd, plen, dtype, dcount, p1, p2 = struct.unpack(">HHHHII", buf[i:i+16])
        if cmd == 18:
            return dtype, dcount, p2      # p2 = server id
        i += 16 + plen
    return None

def cycle():
    s = socket.create_connection((HOST, PORT), timeout=10)
    s.settimeout(10)
    s.sendall(HELLO); s.recv(16)
    s.sendall(hdr(18, PV, p1=7777, p2=13))
    buf = s.recv(4096)
    got = find_create_chan_reply(buf)
    if got:
        dtype, dcount, sid = got
        # CA_PROTO_EVENT_ADD, 16-byte payload: low/high/to floats + mask + pad
        s.sendall(hdr(1, struct.pack(">fffHH", 0.0, 0.0, 0.0, 1, 0),
                      dtype=dtype, dcount=dcount, p1=sid, p2=7777))
        t_end = time.time() + HOLD
        while time.time() < t_end:
            try:
                s.settimeout(max(0.01, t_end - time.time()))
                s.recv(4096)
            except (socket.timeout, OSError):
                break
    s.close()
    time.sleep(GAP)
    return got is not None

def run(n, tag):
    t0 = time.time(); ok = 0
    for i in range(n):
        if cycle():
            ok += 1
    print("%s: %d cycles in %.1f s, %d with a live subscription"
          % (tag, n, time.time() - t0, ok), flush=True)

def reading(tag):
    time.sleep(SETTLE)
    w("#=== %s ===\n" % tag);      time.sleep(2)
    w("rt malloc\n");              time.sleep(7)
    w("epicsThreadShowAll 1\n");   time.sleep(7)
    w("#=== END %s ===\n" % tag);  time.sleep(2)
    print("reading %s taken" % tag, flush=True)

print("== evmon: monitor-subscribing cycles ==", flush=True)
run(WARMUP, "mon-warmup")
reading("M0-baseline-after-%d-warmup" % WARMUP)
run(BATCH_A, "mon-batchA")
reading("M1-after-batchA-%d" % BATCH_A)
run(BATCH_B, "mon-batchB")
reading("M2-after-batchB-%d" % BATCH_B)
print("== evmon done ==", flush=True)
