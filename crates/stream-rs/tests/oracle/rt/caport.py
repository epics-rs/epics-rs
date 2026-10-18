# Prints a CA server port for an oracle IOC that needs RSRV (the camonitor
# rigs). RSRV binds the port for TCP and for UDP; TCP falls back to another
# port but a failed UDP bind leaves it no server, and it cantProceeds
# "CAS: No TCP server started" (caservertask.c:630, :752). So the port is
# free for both, and below the kernel's ephemeral range, which no socket
# bound to port 0 lands in before the IOC binds it.
import random, socket
lo = int(open('/proc/sys/net/ipv4/ip_local_port_range').read().split()[0])
while True:
    port = random.randrange(1024, lo)
    try:
        for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
            with socket.socket(socket.AF_INET, kind) as s:
                s.bind(('', port))
    except OSError:
        continue
    print(port)
    break
