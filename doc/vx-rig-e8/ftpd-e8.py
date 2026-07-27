# E8 rig: the host FTP server netDrv loads the RTP through.
#
# Panel-scoped copy of /tmp/ftpd2.py: port 2141 and passive range 60020-60025
# instead of 2121 / 60000-60005, because three panels share this box and one
# ftpd per panel is what keeps two rtpSp loads from racing on the same data
# connection.  masquerade_address is the SLIRP guestfwd address the guest
# dials, not the bind address.
import logging
import sys

from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import FTPHandler
from pyftpdlib.servers import FTPServer

root = sys.argv[1]
auth = DummyAuthorizer()
auth.add_user("target", "vxTarget", root, perm="elr")
h = FTPHandler
h.authorizer = auth
h.masquerade_address = "10.0.2.100"
h.passive_ports = range(60020, 60026)
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
FTPServer(("127.0.0.1", 2141), h).serve_forever()
