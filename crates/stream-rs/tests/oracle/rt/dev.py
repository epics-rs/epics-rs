# A device for run-rt.sh: argv is the port, then request=>reply pairs
# (Python escapes) separated by ';;'. Whenever what it received since the last answer ends
# with a request, it sends the reply; the reply to request '@' is sent on
# connecting. Connections are served one at a time.
import socket, sys
port = int(sys.argv[1])
table = {}
for a in sys.argv[2].split(';;'):
    k, v = a.split('=>', 1)
    table[k.encode().decode('unicode_escape').encode('latin1')] = v.encode().decode('unicode_escape').encode('latin1')
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', port)); s.listen(1)
while True:
    c, _ = s.accept()
    if table.get(b'@'): c.sendall(table[b'@'])
    buf = b''
    while True:
        d = c.recv(1000)
        if not d: break
        buf += d
        for k, v in table.items():
            if buf.endswith(k):
                buf = b''
                if v: c.sendall(v)
    c.close()
