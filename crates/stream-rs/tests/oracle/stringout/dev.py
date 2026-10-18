# The device of run-posts.sh: it prints the free port it takes, answers
# "Q<x>\r\n" with the next of argv's "v0,v1,..." (the same list for every
# x, counted per x) and leaves every other request unanswered.
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', 0)); s.listen(1)
print(s.getsockname()[1], flush=True)
replies = sys.argv[1].split(',')
count = {}
while True:
    c, _ = s.accept()
    buf = b''
    while True:
        d = c.recv(1000)
        if not d: break
        buf += d
        while b'\r\n' in buf:
            line, buf = buf.split(b'\r\n', 1)
            if line.startswith(b'Q'):
                n = count.get(line, 0); count[line] = n + 1
                c.sendall(replies[min(n, len(replies) - 1)].encode() + b'\r\n')
    c.close()
