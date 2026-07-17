use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};

use tokio::net::UdpSocket;

use crate::protocol::*;

/// Per-client connected UDP socket, matching C EPICS repeaterClient.
/// Using a connected socket lets the OS detect dead clients via
/// ECONNREFUSED on send().
struct RepeaterClient {
    sock: StdUdpSocket,
    addr: SocketAddr,
}

impl RepeaterClient {
    fn new(addr: SocketAddr) -> io::Result<Self> {
        let sock = StdUdpSocket::bind("0.0.0.0:0")?;
        sock.connect(addr)?;
        sock.set_nonblocking(true)?;
        Ok(Self { sock, addr })
    }

    fn send_confirm(&self) -> bool {
        let mut confirm = CaHeader::new(CA_PROTO_REPEATER_CONFIRM);
        if let SocketAddr::V4(v4) = self.addr {
            confirm.available = u32::from_be_bytes(v4.ip().octets());
        }
        self.sock.send(&confirm.to_bytes()).is_ok()
    }

    fn send_message(&self, data: &[u8]) -> bool {
        // distinguish error kinds. The previous version
        // returned `false` for everything (including transient
        // WouldBlock on a saturated kernel UDP buffer), causing the
        // outer loop to drop the client. Now: keep alive on
        // transient/unknown errors; only treat ECONNREFUSED /
        // EHOSTUNREACH as "client gone".
        match self.sock.send(data) {
            Ok(_) => true,
            Err(e) => !matches!(
                e.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::HostUnreachable
            ),
        }
    }

    /// Check if client is still alive by trying to bind to its address.
    /// If bind succeeds, the client has released the port (dead).
    ///
    /// C's bind test binds INADDR_ANY:port (`makeSocket(port, false)`,
    /// `repeater.cpp:94-124`), which works there because C's clients bind
    /// their UDP socket to INADDR_ANY (`udpiiu.cpp:241-249`) — so the
    /// wildcard test collides (EADDRINUSE) with the wildcard client. This
    /// port's clients bind SPECIFIC NIC addresses (the AsyncUdpV4 per-NIC
    /// bundle, never a wildcard socket), and on Windows a wildcard bind
    /// test does NOT collide with a socket bound to a specific address —
    /// so an INADDR_ANY probe would report every live client as departed
    /// and reap it. Bind the test to the client's OWN registered address
    /// (`self.addr`, the datagram source the repeater stored) so it
    /// collides with the client's specific-address socket on every
    /// platform, preserving C's liveness semantics ("is this client's port
    /// still held?") against the port's specific-address sockets.
    fn verify(&self) -> bool {
        let bind_addr = match self.addr {
            SocketAddr::V4(v4) => v4,
            _ => return false,
        };
        match StdUdpSocket::bind(bind_addr) {
            Ok(_) => false,                                         // addr free → client gone
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => true, // addr in use → alive
            Err(_) => true,                                         // other error → assume alive
        }
    }
}

/// Run the CA repeater daemon. Equivalent to
/// `run_repeater_with_debug(0)`.
pub async fn run_repeater() -> io::Result<()> {
    run_repeater_with_debug(0).await
}

/// Run the CA repeater daemon with an explicit debug level.
///
/// Mirrors epics-base PR #831 (commit `e2717521` "Added -d option
/// to caRepeater, sets debug level"):
/// - level 0: silent (default)
/// - level 1: print "New client", "Verified N active clients",
///   "Client refused message", "Deleted client" — high-level
///   client lifecycle.
/// - level 2: also print per-beacon "Sent to port N" and per-client
///   "Client on port N is alive" verification.
///
/// Output goes to stderr to match the C repeater's `fprintf(stderr, …)`.
/// `ca-repeater-rs -v` keeps stderr connected so the messages reach
/// the operator; without `-v` stderr is `dup2`'d to `/dev/null` and
/// the messages are discarded (also matches C behaviour).
///
/// Binds to UDP 5065, accepts client registrations, and fans out beacons.
pub async fn run_repeater_with_debug(debug: u8) -> io::Result<()> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // libcom commit 51191e6: Linux defaults IP_MULTICAST_ALL=1, which would
    // give the repeater multicast traffic for groups it never joined.
    // No-op on non-Linux.
    #[cfg(target_os = "linux")]
    {
        let _ = sock.set_multicast_all_v4(false);
    }
    sock.set_nonblocking(true)?;
    // libca `repeater.cpp:511` resolves the bind port through
    // `envGetInetPortConfigParam(&EPICS_CA_REPEATER_PORT, …)`. Mirror
    // that so sites that override the port via env (e.g. to coexist
    // with a parallel C caRepeater on the default 5065) reach our
    // daemon. The default remains 5065 when the env var is unset.
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, repeater_port());
    // Singleton-per-host bind. libca `repeater.cpp` makeSocket() binds
    // with NO reuse option set, so a second repeater process gets
    // EADDRINUSE and exits (`repeater.cpp:513-521`); ca-repeater-rs
    // treats that error as "another repeater is already running". The
    // bind must therefore be exclusive — do not enable SO_REUSEPORT
    // here: `epicsSocketEnableAddressUseForDatagramFanout` (the
    // SO_REUSEPORT fanout helper) is never called for the repeater, and
    // enabling it would let two repeaters join the kernel UDP fanout
    // group and split client registration / beacon delivery between
    // them. CA server UDP sockets keep fanout (server/udp.rs) so
    // multiple IOCs share the CA port; the repeater daemon port does not.
    sock.bind(&bind_addr.into())?;
    // Only after a successful exclusive bind does C enable SO_REUSEADDR
    // (`epicsSocketEnableAddressReuseDuringTimeWaitState`) so THIS daemon
    // can rebind across a restart. POSIX-only — WINSOCK SO_REUSEADDR has
    // different (port-hijack) semantics, so the C helper is a no-op on
    // Windows.
    #[cfg(not(windows))]
    let _ = sock.set_reuse_address(true);

    // ca commit 97bf917: join every multicast (224.0.0.0/4) beacon address
    // from EPICS_CAS_BEACON_ADDR_LIST (or EPICS_CA_ADDR_LIST as fallback) so
    // multicast-configured sites actually receive the beacons they fan out.
    // Errors are logged but non-fatal — broadcast/unicast beacons still work.
    join_beacon_multicast_groups(&sock);

    let std_sock: StdUdpSocket = sock.into();
    let socket = UdpSocket::from_std(std_sock)?;
    // pvxs `udp_collector.cpp` parity: opt the kernel into
    // SO_RXQ_OVFL so a sustained beacon-fanout backlog surfaces as
    // a debug log instead of silent loss. No-op on non-Linux.
    if let Err(e) = epics_base_rs::net::enable_so_rxq_ovfl_for_socket(&socket) {
        tracing::trace!(
            target: "epics_ca_rs::repeater",
            error = %e,
            "SO_RXQ_OVFL enable failed (non-fatal)"
        );
    }

    if debug > 0 {
        eprintln!("CA Repeater: Attached and initialized");
    }

    let mut clients: HashMap<u16, RepeaterClient> = HashMap::new();
    let mut buf = [0u8; 4096];
    let mut prev_drops: u32 = 0;

    loop {
        // C `repeater.cpp:589-605`: a recv error never exits the repeater —
        // `ECONNREFUSED` (Linux ICMP bug) and `ECONNRESET` (Windows KB263823)
        // are silently skipped, anything else is logged, and the loop always
        // continues. A repeater client that vanished between our fan-out send
        // and this recv otherwise killed the whole repeater.
        let (len, src, drops) =
            match epics_base_rs::net::recv_from_with_drop_count_socket(&socket, &mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    if !matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset
                    ) {
                        tracing::warn!(
                            target: "epics_ca_rs::repeater",
                            "CA Repeater: unexpected UDP recv err: {e}"
                        );
                    }
                    continue;
                }
            };
        if drops != 0 && drops != prev_drops {
            tracing::debug!(
                target: "epics_ca_rs::repeater",
                prev = prev_drops,
                drops,
                "CA repeater UDP socket buffer overflow"
            );
        }
        prev_drops = drops;

        // C CA clients send a zero-length UDP packet for repeater
        // registration (backward compat with pre-3.12 repeaters).
        //
        // C `register_new_client` (`repeater.cpp:374-424`)
        // applies the same locality gate to BOTH the zero-length
        // legacy form and `CA_PROTO_REPEATER_REGISTER`. Pre-fix
        // Rust registered any zero-length datagram regardless of
        // source.
        //
        // C accepts loopback OR any source IP that belongs
        // to a local interface (the bind-test compatibility quirk
        // for clients alternating between loopback and the first
        // non-loopback interface). Use the same `is_local_source`
        // helper as `CA_PROTO_REPEATER_REGISTER` so a site-local
        // legacy client registering from e.g. `192.168.x.y` is
        // accepted, matching C.
        if len == 0 {
            if !is_local_source(src) {
                tracing::warn!(
                    src = %src,
                    "caRepeater: zero-length registration from non-local source rejected"
                );
                metrics::counter!("ca_repeater_register_non_loopback_rejects_total").increment(1);
                continue;
            }
            register_client_debug(&mut clients, src, debug);
            continue;
        }

        // Intentional divergence from C: `repeater.cpp:613-637` only
        // special-cases `size >= sizeof(caHdr)` (16) and `size == 0`, so
        // a 1–15-byte sub-header datagram falls through to `fanOut` and
        // is forwarded verbatim. We drop it instead — a runt datagram
        // carries no decodable CA header, every registered (loopback)
        // client would discard it on receipt anyway, and no legitimate
        // sender emits one; dropping here avoids waking every client
        // with an undecodable packet.
        if len < CaHeader::SIZE {
            continue;
        }

        let Ok(hdr) = CaHeader::from_bytes(&buf[..len]) else {
            continue;
        };

        let action = decode_datagram(&buf[..len], &hdr, src);
        if action.register {
            // C `register_new_client` (repeater.cpp:374-424) rejects
            // REPEATER_REGISTER from non-AF_INET peers and, for
            // non-loopback sources, requires `bind()` to the source
            // address to succeed (proving the IP belongs to a local
            // interface). The intent: the repeater and its clients
            // must be on the same host — beacon fan-out from a
            // remote peer would silently expose PV existence to
            // unauthorised observers via the registered-clients
            // list.
            //
            // Rust simplifies the C check to loopback-only. The C
            // bind-test is a 3.13-era compatibility quirk that
            // allowed the first non-loopback interface as well
            // (clients alternated between addresses during the
            // transition); modern libca always uses 127.0.0.1
            // (`repeater.cpp:466-478` `caRepeaterRegistrationMessage`
            // sets the destination to loopback explicitly).
            // accept loopback OR any source IP that
            // belongs to a local interface (C bind-test
            // compatibility, `repeater.cpp::register_new_client`
            // accepts a non-loopback source if `bind()` to that
            // address succeeds locally).
            if !is_local_source(src) {
                tracing::warn!(
                    src = %src,
                    "caRepeater: REPEATER_REGISTER from non-local source rejected"
                );
                metrics::counter!("ca_repeater_register_non_loopback_rejects_total").increment(1);
                continue;
            }
            register_client_debug(&mut clients, src, debug);
        }
        if let Some(data) = action.fanout {
            fan_out(&mut clients, src, &data, debug);
        }
    }
}

/// Outcome of decoding a single incoming repeater datagram. Mirrors
/// C `ca_repeater()` (`repeater.cpp:613-637`):
///   * `register = true` when the leading header is REPEATER_REGISTER,
///     and the registration is performed before fan-out.
///   * `fanout = Some(bytes)` when there is anything to broadcast to
///     other registered clients — i.e. either a non-REGISTER datagram,
///     or the remainder of a REGISTER + payload datagram after stripping
///     the 16-byte REGISTER header.
///
/// The chained REGISTER + payload case is rare in practice (clients
/// almost never piggy-back other messages on a registration), but
/// byte-exact parity matters: a beacon-tunnel datagram that prepends
/// REGISTER would otherwise be silently dropped by us while C still
/// fans it out to peers.
///
/// Note: after stripping REGISTER, C does NOT re-inspect the remainder
/// for RSRV_IS_UP — the source-IP rewrite only fires when the *outer*
/// header is RSRV_IS_UP. So the remainder fan-out path here does not
/// rewrite `m_available` either, to avoid diverging in the other
/// direction.
struct DatagramAction {
    register: bool,
    fanout: Option<Vec<u8>>,
}

/// C `repeater.cpp::register_new_client` accepts a registration
/// when the source IP is loopback OR when `bind()` to that address
/// succeeds locally (the 3.13-era compatibility quirk that allows
/// clients which alternate between loopback and the first non-
/// loopback interface). Pre-fix Rust simplified to loopback-only.
/// We replicate the C bind-test by trying to bind a fresh UDP socket
/// to `(src.ip(), 0)`; if the kernel accepts the bind, the address
/// belongs to a local interface.
fn is_local_source(src: SocketAddr) -> bool {
    if src.ip().is_loopback() {
        return true;
    }
    match src {
        SocketAddr::V4(v4) => StdUdpSocket::bind(SocketAddrV4::new(*v4.ip(), 0)).is_ok(),
        // C `register_new_client` rejects non-AF_INET (IPv6) explicitly.
        SocketAddr::V6(_) => false,
    }
}

fn decode_datagram(buf: &[u8], hdr: &CaHeader, src: SocketAddr) -> DatagramAction {
    if hdr.cmmd == CA_PROTO_REPEATER_REGISTER {
        // Remainder after the stripped REGISTER header.
        if buf.len() <= CaHeader::SIZE {
            return DatagramAction {
                register: true,
                fanout: None,
            };
        }
        // Per C: no source-IP rewrite on the remainder.
        DatagramAction {
            register: true,
            fanout: Some(buf[CaHeader::SIZE..].to_vec()),
        }
    } else {
        let mut data = buf.to_vec();
        // Per C `repeater.cpp:626-630`: rewrite m_available on
        // RSRV_IS_UP only when the caller didn't already fill it in.
        if hdr.cmmd == CA_PROTO_RSRV_IS_UP && hdr.available == 0 {
            if let SocketAddr::V4(v4) = src {
                let avail_offset = 12; // available field at bytes 12..16
                data[avail_offset..avail_offset + 4].copy_from_slice(&v4.ip().octets());
            }
        }
        DatagramAction {
            register: false,
            fanout: Some(data),
        }
    }
}

/// Fan a datagram out to every registered repeater client other than
/// the sender. Mirrors C `repeater.cpp::fanOut`: per-client `sendMessage`,
/// and on send failure the client is verified, removed if dead.
fn fan_out(clients: &mut HashMap<u16, RepeaterClient>, src: SocketAddr, data: &[u8], debug: u8) {
    let mut dead = Vec::new();
    for (port, client) in clients.iter() {
        // Don't reflect back to sender. C `fanOut` (repeater.cpp:340-349)
        // skips the originating client via `identicalAddress`, which
        // compares the FULL address (family + port + IP), not just the
        // port. Matching on port alone wrongly suppresses a beacon to a
        // local client whose ephemeral registration port happens to
        // equal the beacon's source (server) port.
        if client.addr == src {
            continue;
        }
        if !client.send_message(data) {
            if debug >= 1 {
                eprintln!("Client on port {port} refused message");
            }
            if !client.verify() {
                dead.push(*port);
            } else if debug >= 2 {
                eprintln!("Client on port {port} is alive");
            }
        } else if debug >= 2 {
            eprintln!("Sent to port {port}");
        }
    }
    for p in dead {
        if debug >= 1 {
            eprintln!("Deleted client on port {p}");
        }
        clients.remove(&p);
    }
    if debug >= 1 {
        eprintln!("Verified {} active clients", clients.len());
    }
}

/// Parse `EPICS_CAS_BEACON_ADDR_LIST` (or fall back to `EPICS_CA_ADDR_LIST`)
/// and join every multicast group (224.0.0.0/4) on `INADDR_ANY`. Any address
/// that isn't multicast is silently skipped. Logs warnings for join failures
/// but never aborts: the repeater keeps running for unicast/broadcast beacons.
fn join_beacon_multicast_groups(sock: &socket2::Socket) {
    let list = epics_base_rs::runtime::env_table::EPICS_CAS_BEACON_ADDR_LIST
        .get()
        .or_else(|| epics_base_rs::runtime::env_table::EPICS_CA_ADDR_LIST.get());
    let Some(list) = list else {
        return;
    };
    for token in list.split_whitespace() {
        // Strip optional :port suffix; we only care about the address.
        let host = token.rsplit_once(':').map(|(h, _)| h).unwrap_or(token);
        let Ok(addr) = host.parse::<Ipv4Addr>() else {
            continue;
        };
        if !addr.is_multicast() {
            continue;
        }
        if let Err(e) = sock.join_multicast_v4(&addr, &Ipv4Addr::UNSPECIFIED) {
            tracing::warn!(group = %addr, error = %e,
                "ca-repeater: IP_ADD_MEMBERSHIP failed");
        }
    }
}

/// Soft cap on simultaneously registered repeater clients. The
/// in-process repeater is loopback-only, so the practical attacker
/// is a local process opening many UDP sockets on different source
/// ports. 1024 is comfortably above any realistic CA-client farm
/// on a single host (one or two per Phoebus + ~hundred CSS + a
/// handful of caget/caput) but small enough to bound memory if
/// abused. C `caRepeater.c` has no cap; we choose to be stricter.
const MAX_REPEATER_CLIENTS: usize = 1024;

fn register_client_debug(clients: &mut HashMap<u16, RepeaterClient>, src: SocketAddr, debug: u8) {
    let port = src.port();
    let was_registered = clients.contains_key(&port);
    register_client(clients, src, debug);
    if !was_registered && debug >= 1 && clients.contains_key(&port) {
        eprintln!("New client on port {port}");
        eprintln!("Verified {} active clients", clients.len());
    }
}

/// C `verifyClients` (`repeater.cpp:317-335`) — bind-test EVERY registered
/// client and reap the ones whose port is now free.
///
/// This is unconditional: it does NOT wait for a send to fail. C's own
/// comment at the call site (`repeater.cpp:475-479`) gives the reason —
/// "an ICMP error return does not get through to send(), which returns no
/// error code" on some platforms — so send-failure reaping alone leaks stale
/// clients there. Returns the number of clients reaped.
fn verify_clients(clients: &mut HashMap<u16, RepeaterClient>) -> usize {
    let dead: Vec<u16> = clients
        .iter()
        .filter(|(_, c)| !c.verify())
        .map(|(p, _)| *p)
        .collect();
    let reaped = dead.len();
    for p in dead {
        clients.remove(&p);
    }
    reaped
}

/// C `register_new_client` (`repeater.cpp:366-487`), in C's order:
/// find-or-create → sendConfirm (removing the client on failure, whether it
/// is new or not) → noop fan-out to every OTHER client (on EVERY
/// registration, so a repeat registration still exercises their sockets) →
/// `verifyClients` when this registration created a client.
fn register_client(clients: &mut HashMap<u16, RepeaterClient>, src: SocketAddr, debug: u8) {
    let port = src.port();

    let mut new_client = false;
    if !clients.contains_key(&port) {
        // Rust-only soft cap (C has none): sweep first so a full table of
        // departed clients still admits a live one.
        if clients.len() >= MAX_REPEATER_CLIENTS {
            verify_clients(clients);
            if clients.len() >= MAX_REPEATER_CLIENTS {
                // All slots are alive — refuse the new client. The peer
                // sees no CONFIRM and retries (every 1 s, per
                // `repeaterSubscribeTimer`), so it gets in as soon as a
                // slot frees.
                return;
            }
        }
        // Per-client connected socket, as C's `repeaterClient::connect`.
        let client = match RepeaterClient::new(src) {
            Ok(c) => c,
            Err(_) => return,
        };
        clients.insert(port, client);
        new_client = true;
    }

    // C `repeater.cpp:453-467`: a client whose CONFIRM cannot be sent is
    // removed — including an ALREADY-REGISTERED one, which the pre-fix Rust
    // kept (it returned early after `send_confirm()`, ignoring the result).
    let confirmed = clients.get(&port).is_some_and(|c| c.send_confirm());
    if !confirmed {
        clients.remove(&port);
        if debug >= 1 {
            eprintln!("Deleted repeater client on port {port}, error sending ack");
        }
    }

    // C `repeater.cpp:469-474`: "send a noop message to all other clients so
    // that we don't accumulate sockets when there are no beacons". Sent on
    // every registration message, not only on the one that created a client
    // — and clients now re-register once a second until confirmed (R6-22),
    // so this is the sweep that runs in a beacon-less network.
    let noop = CaHeader::new(CA_PROTO_VERSION);
    fan_out(clients, src, &noop.to_bytes(), debug);

    // C `repeater.cpp:476-486`: the bind-test sweep, run whenever a
    // registration created a client, and deliberately AFTER the confirm above
    // so the new client is never reaped before it is acknowledged.
    if new_client {
        let reaped = verify_clients(clients);
        if debug >= 1 && reaped > 0 {
            eprintln!("Reaped {reaped} departed client(s) on new registration");
        }
    }
}

/// Try to register with an existing repeater. If none is running, spawn one
/// as a background process using the current executable's `ca-repeater` binary,
/// then register again.
/// `repeater_port` is the client's single resolution of
/// `EPICS_CA_REPEATER_PORT` (C `udpiiu::repeaterPort`, `udpiiu.cpp:168`) —
/// both the pre-spawn and the post-spawn attempt reuse it.
pub async fn ensure_repeater(repeater_port: u16) {
    if try_register(repeater_port).await.is_ok() {
        return;
    }

    // No repeater running — spawn one
    spawn_repeater();

    // Give it a moment to start, then register
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(50)).await;
    let _ = try_register(repeater_port).await;
}

/// Send a REPEATER_REGISTER to localhost:5065 and wait for CONFIRM.
async fn try_register(repeater_port: u16) -> Result<(), ()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|_| ())?;
    // SO_RXQ_OVFL opt-in for diagnostic parity with the long-running
    // repeater; the brief CONFIRM wait below ignores the counter
    // (just one packet expected) but enables it so any future reuse
    // of this socket inherits the same diagnostic surface. No-op
    // on non-Linux.
    let _ = epics_base_rs::net::enable_so_rxq_ovfl_for_socket(&socket);

    let local_ip = match socket.local_addr().ok() {
        Some(SocketAddr::V4(v4)) => *v4.ip(),
        _ => Ipv4Addr::LOCALHOST,
    };

    let mut hdr = CaHeader::new(CA_PROTO_REPEATER_REGISTER);
    hdr.available = u32::from_be_bytes(local_ip.octets());

    // Client REGISTER target: the port the caller resolved once (C
    // `udpiiu::repeaterPort`, `udpiiu.cpp:168`), so the register attempt
    // and the daemon bind agree and a misconfigured value is diagnosed
    // once per process rather than once per attempt.
    let repeater_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, repeater_port);
    socket
        .send_to(&hdr.to_bytes(), repeater_addr)
        .await
        .map_err(|_| ())?;

    // Wait for confirm with short timeout
    let mut buf = [0u8; 64];
    let result = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let (len, _, _drops) =
                epics_base_rs::net::recv_from_with_drop_count_socket(&socket, &mut buf)
                    .await
                    .map_err(|_| ())?;
            if len >= CaHeader::SIZE {
                if let Ok(resp) = CaHeader::from_bytes(&buf[..len]) {
                    if resp.cmmd == CA_PROTO_REPEATER_CONFIRM {
                        return Ok::<(), ()>(());
                    }
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        _ => Err(()),
    }
}

/// Spawn the repeater as a detached background process.
/// Falls back to an in-process repeater thread if the binary is not found.
fn spawn_repeater() {
    let exe = std::env::current_exe().unwrap_or_default();
    // The sibling binary carries the platform executable suffix: on Windows
    // it is `ca-repeater-rs.exe`. Joining the bare stem made `bin.exists()`
    // always false there, so every client fell through to the in-process
    // repeater fallback — which re-resolves `EPICS_CA_REPEATER_PORT` (and
    // re-prints its diagnostics into the client's own stderr) instead of
    // spawning the shared daemon C `caStartRepeaterIfNotInstalled` starts.
    let repeater_bin = exe
        .parent()
        .map(|p| p.join(format!("ca-repeater-rs{}", std::env::consts::EXE_SUFFIX)));

    // Try external binary first
    if let Some(ref bin) = repeater_bin {
        if bin.exists() {
            use std::process::{Command, Stdio};
            // Windows has no CLOEXEC: `CreateProcess` inherits every
            // inheritable handle we hold, and the stdout/stderr pipe a
            // capturing parent (`Command::output`, a shell `$(caput …)`,
            // nextest) handed us stays inheritable. The repeater is a
            // detached daemon that outlives us, so if it inherits that
            // pipe the capturing parent never sees EOF and blocks until
            // the daemon exits — i.e. forever. Unix avoids this because
            // `Stdio::null()` redirects the child's fds 0/1/2 and the pipe
            // only ever lived on fd 1. Clear the inherit flag on our own
            // standard handles first so the daemon cannot hold them — the
            // Windows analogue of the CLOEXEC unix already gets.
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
                use windows_sys::Win32::System::Console::{
                    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                };
                for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                    let h = GetStdHandle(id);
                    // A closed/redirected slot is null; an unset one is
                    // INVALID_HANDLE_VALUE. Skip both — only real handles
                    // carry the inheritable pipe we must detach.
                    if !h.is_null() && h != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                        SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
                    }
                }
            }
            if Command::new(bin)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .is_ok()
            {
                return;
            }
        }
    }

    // Fallback: run repeater in-process on a background thread.
    // This ensures beacon reception works even without the external binary.
    std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("repeater runtime");
        let _ = rt.block_on(run_repeater());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(cmmd: u16, available: u32) -> Vec<u8> {
        let mut h = CaHeader::new(cmmd);
        h.available = available;
        h.to_bytes().to_vec()
    }

    fn src_v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
    }

    #[test]
    fn beacon_rewrites_zero_m_available_with_source_ip() {
        // RSRV_IS_UP with m_available=0 → repeater fills in the
        // sender's IP (C `repeater.cpp:626-630`).
        let buf = header_bytes(CA_PROTO_RSRV_IS_UP, 0);
        let hdr = CaHeader::from_bytes(&buf).unwrap();
        let src = src_v4(10, 0, 0, 5, 4321);
        let act = decode_datagram(&buf, &hdr, src);
        assert!(!act.register);
        let data = act.fanout.expect("beacon must be fanned out");
        // m_available is at bytes 12..16.
        assert_eq!(&data[12..16], &[10, 0, 0, 5]);
    }

    #[test]
    fn beacon_with_nonzero_m_available_is_unchanged() {
        // RSRV_IS_UP with m_available already set → leave it.
        let buf = header_bytes(CA_PROTO_RSRV_IS_UP, 0x0a00_0006);
        let hdr = CaHeader::from_bytes(&buf).unwrap();
        let src = src_v4(192, 168, 1, 99, 5555);
        let act = decode_datagram(&buf, &hdr, src);
        assert!(!act.register);
        let data = act.fanout.expect("beacon must be fanned out");
        assert_eq!(&data[12..16], &0x0a00_0006u32.to_be_bytes());
    }

    #[test]
    fn non_rsrv_non_register_message_is_not_rewritten() {
        // Previous code rewrote m_available on ANY non-REGISTER
        // command — C only rewrites RSRV_IS_UP. Verify a different
        // command (e.g. VERSION) flows through untouched.
        let buf = header_bytes(CA_PROTO_VERSION, 0);
        let hdr = CaHeader::from_bytes(&buf).unwrap();
        let src = src_v4(10, 0, 0, 5, 4321);
        let act = decode_datagram(&buf, &hdr, src);
        assert!(!act.register);
        let data = act.fanout.expect("fan out");
        // Bytes 12..16 stay zero.
        assert_eq!(&data[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn bare_register_returns_register_only_no_fanout() {
        let buf = header_bytes(CA_PROTO_REPEATER_REGISTER, 0);
        let hdr = CaHeader::from_bytes(&buf).unwrap();
        let src = src_v4(127, 0, 0, 1, 9000);
        let act = decode_datagram(&buf, &hdr, src);
        assert!(act.register);
        assert!(
            act.fanout.is_none(),
            "bare REGISTER must not fan out anything"
        );
    }

    #[test]
    fn chained_register_plus_payload_strips_then_fans_out_remainder() {
        // C parity: REGISTER + RSRV_IS_UP in one datagram. Repeater
        // registers the sender, strips the 16-byte REGISTER header,
        // and fans out the remainder to other clients. The remainder's
        // m_available is NOT rewritten (C `repeater.cpp:613-637` only
        // checks the outer header for the rewrite — once stripped, the
        // remainder fan-out path is the literal fanOut call).
        let mut buf = header_bytes(CA_PROTO_REPEATER_REGISTER, 0);
        let remainder = header_bytes(CA_PROTO_RSRV_IS_UP, 0);
        buf.extend_from_slice(&remainder);

        let hdr = CaHeader::from_bytes(&buf).unwrap();
        let src = src_v4(10, 0, 0, 5, 5060);
        let act = decode_datagram(&buf, &hdr, src);
        assert!(act.register, "REGISTER must register the sender");
        let data = act.fanout.expect("chained payload must fan out");
        assert_eq!(data.len(), CaHeader::SIZE);
        // Verify the fanned-out bytes are the literal RSRV_IS_UP
        // header without source-IP rewrite (parity quirk: the rewrite
        // only fires when the *outer* command is RSRV_IS_UP).
        assert_eq!(&data, &remainder);
        // And the m_available stays zero — C does not rewrite it after
        // the strip.
        assert_eq!(&data[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn fan_out_skips_on_full_address_not_port_alone() {
        // C `fanOut` (repeater.cpp:340-349) skips the originating
        // client by FULL address (`identicalAddress` = family + port +
        // IP). A client registered on loopback:P must still receive a
        // beacon whose SOURCE is a server at a different IP but the same
        // port P — port-only skip wrongly suppressed it.
        let recv = StdUdpSocket::bind("127.0.0.1:0").expect("bind recv");
        recv.set_read_timeout(Some(std::time::Duration::from_millis(750)))
            .unwrap();
        let local = recv.local_addr().unwrap();
        let port = local.port();

        let mut clients: HashMap<u16, RepeaterClient> = HashMap::new();
        clients.insert(port, RepeaterClient::new(local).expect("client sock"));

        let data = header_bytes(CA_PROTO_RSRV_IS_UP, 0x0a00_0005);

        // (1) Beacon from a DIFFERENT IP but the SAME port → the client
        // must NOT be skipped; it receives the fanned-out datagram.
        let server_src = src_v4(10, 0, 0, 5, port);
        fan_out(&mut clients, server_src, &data, 0);
        let mut buf = [0u8; 64];
        let n = recv
            .recv(&mut buf)
            .expect("client with a coinciding port must still receive the beacon");
        assert_eq!(
            &buf[..n],
            &data[..],
            "fanned-out bytes must match the input"
        );

        // (2) Datagram whose FULL address equals the client → skipped
        // (no reflect-to-self), so the receive times out.
        fan_out(&mut clients, local, &data, 0);
        let err = recv.recv(&mut buf).expect_err(
            "a datagram from the client's own full address must be skipped, not reflected",
        );
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "expected a read timeout for the self-skip case, got {err:?}"
        );
    }

    /// R6-27: C runs `verifyClients()` — a bind test on EVERY registered
    /// client — whenever a registration creates a client
    /// (`repeater.cpp:476-486`), regardless of whether any send failed. The
    /// pre-fix Rust reaped only on send failure, and `send_message` treats
    /// just ECONNREFUSED / EHOSTUNREACH as gone; on a platform that never
    /// surfaces the ICMP error (C names HP-UX and Solaris) the departed
    /// client stayed registered until the 1024-entry cap.
    ///
    /// Here the departed client's socket is CLOSED but its address is still
    /// in the table — `send_message` to it succeeds (a connected UDP send to
    /// a free loopback port does not fail synchronously on the first datagram
    /// here), so only the bind-test sweep can reap it.
    #[test]
    fn new_registration_bind_test_sweeps_departed_clients() {
        // A departed client: bind to learn a port, then drop the socket so the
        // port is free — exactly what `verify()`'s bind test detects.
        let departed_port = {
            let s = StdUdpSocket::bind("127.0.0.1:0").expect("bind departed");
            s.local_addr().unwrap().port()
        };
        let departed = src_v4(127, 0, 0, 1, departed_port);

        // A live client, still holding its port.
        let live_sock = StdUdpSocket::bind("127.0.0.1:0").expect("bind live");
        let live = live_sock.local_addr().unwrap();

        let mut clients: HashMap<u16, RepeaterClient> = HashMap::new();
        clients.insert(
            departed_port,
            RepeaterClient::new(departed).expect("departed client sock"),
        );
        clients.insert(live.port(), RepeaterClient::new(live).expect("live sock"));
        assert_eq!(clients.len(), 2);

        // A brand-new client registers. C: newClient ⇒ verifyClients().
        let newcomer_sock = StdUdpSocket::bind("127.0.0.1:0").expect("bind newcomer");
        let newcomer = newcomer_sock.local_addr().unwrap();
        register_client(&mut clients, newcomer, 0);

        assert!(
            !clients.contains_key(&departed_port),
            "a client whose port is free must be reaped by the bind-test sweep \
             on the next new registration, even though no send failed"
        );
        assert!(
            clients.contains_key(&live.port()),
            "the live client must survive the sweep"
        );
        assert!(
            clients.contains_key(&newcomer.port()),
            "the registering client must be present and confirmed"
        );
    }

    /// A REPEAT registration from an already-registered client re-sends the
    /// CONFIRM and still fans the noop out to the others
    /// (`repeater.cpp:469-474`) — the pre-fix Rust returned early after the
    /// confirm, so with clients re-registering every second (R6-22) the
    /// "don't accumulate sockets when there are no beacons" sweep never ran.
    #[test]
    fn repeat_registration_still_fans_the_noop_to_other_clients() {
        let other = StdUdpSocket::bind("127.0.0.1:0").expect("bind other");
        other
            .set_read_timeout(Some(std::time::Duration::from_millis(750)))
            .unwrap();
        let other_addr = other.local_addr().unwrap();

        let repeat_sock = StdUdpSocket::bind("127.0.0.1:0").expect("bind repeat");
        let repeat_addr = repeat_sock.local_addr().unwrap();

        let mut clients: HashMap<u16, RepeaterClient> = HashMap::new();
        clients.insert(
            other_addr.port(),
            RepeaterClient::new(other_addr).expect("other sock"),
        );
        clients.insert(
            repeat_addr.port(),
            RepeaterClient::new(repeat_addr).expect("repeat sock"),
        );

        register_client(&mut clients, repeat_addr, 0);

        let mut buf = [0u8; 64];
        let n = other
            .recv(&mut buf)
            .expect("a repeat registration must still noop-fan-out to other clients");
        let hdr = CaHeader::from_bytes(&buf[..n]).expect("noop parses");
        assert_eq!(hdr.cmmd, CA_PROTO_VERSION, "the noop is a VERSION frame");
    }
}
