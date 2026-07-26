# E10 rig ftpd — the box has no privileged ports, so netDrv reaches this
# through the qemu guestfwd bridges (see boot-e10.sh).  Ports are E10's block
# alone: control 2131, passive 60010-60015.
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
h.passive_ports = range(60010, 60016)
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
FTPServer(("127.0.0.1", 2131), h).serve_forever()
