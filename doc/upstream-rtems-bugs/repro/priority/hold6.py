"""Make the listener threads themselves earn CPU so `rt top` prints their own
RPRI rather than leaving it to be derived: accept churn drives CAS-TCP
(0x0b010012) and a UDP name-search flood drives CAS-UDP (0x0b010013).  Two
steady connections are held throughout so CAS-client/CAS-event still exist."""
import socket, struct, threading, time, os
HOST, PORT = "127.0.0.1", 5164
FIFO = os.path.expanduser("~/rtems-cside/ciocin")
def hdr(cmd, payload=b"", dtype=0, dcount=0, p1=0, p2=0):
    payload = payload + b"\0" * ((-len(payload)) % 8)
    return struct.pack(">HHHHII", cmd, len(payload), dtype, dcount, p1, p2) + payload
HELLO = hdr(0, dtype=0, dcount=13) + hdr(20, b"probe\0") + hdr(21, b"probe\0")

held = []
for i in range(2):
    s = socket.create_connection((HOST, PORT), timeout=10)
    s.settimeout(10)
    s.sendall(HELLO); s.recv(16)
    s.sendall(hdr(18, b"CIOC:AO\0", p1=8000 + i, p2=13)); s.recv(4096)
    held.append(s)
print("2 steady connections held", flush=True)

stop = threading.Event()
def accept_churn():
    n = 0
    while not stop.is_set():
        try:
            s = socket.create_connection((HOST, PORT), timeout=5)
            s.settimeout(5)
            s.sendall(HELLO)
            s.recv(16)
            s.close()
            n += 1
        except OSError:
            pass
        time.sleep(0.05)
    print("  accept churn: %d connect/close cycles" % n, flush=True)

def udp_search():
    u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    pkt = hdr(0, dtype=0, dcount=13) + hdr(6, b"CIOC:AO\0", dtype=5, dcount=13,
                                           p1=4242, p2=4242)
    while not stop.is_set():
        try:
            u.sendto(pkt, (HOST, PORT))
            u.settimeout(0.02)
            try:
                u.recv(1024)
            except socket.timeout:
                pass
        except OSError:
            pass
        time.sleep(0.02)

threading.Thread(target=accept_churn, daemon=True).start()
threading.Thread(target=udp_search, daemon=True).start()

def w(text):
    with open(FIFO, "w") as f:
        f.write(text)

time.sleep(25)
w("rt top\n"); time.sleep(14)
w("\n"); time.sleep(6)
stop.set(); time.sleep(3)
for s in held:
    try: s.close()
    except OSError: pass
print("done", flush=True)
