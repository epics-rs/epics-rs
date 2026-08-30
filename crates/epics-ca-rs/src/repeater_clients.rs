//! `repeater.cpp`'s client table, its datagram decisions and the diagnostic
//! facilities both halves of the repeater emit through — everything the C file
//! does that is not the socket event loop.
//!
//! Split out of `repeater` because that module needs a reactor and this does
//! not. `repeater`'s `run_repeater` parks on a `tokio::net::UdpSocket`, so the
//! whole module is declared behind a gate; the client table underneath it
//! keeps its own `std::net::UdpSocket` per client and the datagram decisions
//! are byte rewriting over `CaHeader`. Leaving them in the gated module meant a
//! configuration that has no reactor also had no test of the beacon rewrite,
//! the register/fan-out split or the client sweep — ten unit tests compiled
//! away for a reason that applies to none of them. The gate is right about its
//! own module and wrong about this code, so this code moved out rather than
//! the gate getting an exception.
//!
//! Both halves emit through `Diag`, and neither needs a reactor to do it, so it
//! lives here too.
//!
//! # A moved item's gate is a property of its own dependencies
//!
//! Never of the module it came out from. A coarse gate is a statement about
//! the loosest thing under it, so inheriting it onto extracted code asserts a
//! requirement that code does not have — and the result is invisible, because
//! the configurations that should have the item simply do not, with no error
//! anywhere. Read what the moved code actually needs, then gate on that and
//! nothing else.
//!
//! Here that reading was: `socket2` and `tokio`, both declared in `Cargo.toml`
//! under `cfg(not(any(target_os = "rtems", target_os = "vxworks")))`, and no
//! reactor. So this module carries the TARGET gate, `not(epics_embedded_target)`
//! — on RTEMS and VxWorks neither crate exists, so there is nothing to build
//! and no caller left to build it for — and it must never acquire a BACKEND
//! gate. `tokio_backend` is what removes `repeater`; a host building
//! `--all-features` has every dependency this module needs and no reactor for
//! `repeater`, which is precisely the configuration whose repeater behaviour
//! these tests are the only check on.
//!
//! Taking `repeater`'s gate wholesale would have looked correct — the file
//! compiled, the tests passed where they were run — and would have deleted
//! that check in exactly the configuration it exists for.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};

use crate::protocol::*;

/// C `__FILE__` for `repeater.cpp` as the base build compiles it — the
/// literal the `fprintf(stderr, "%s: …", __FILE__, …)` sites at `:145`,
/// `:155` and `:514` put on the terminal. Verified in the built artifact:
/// `strings lib/linux-x86_64/libca.so` carries `../repeater.cpp`.
pub(crate) const C_FILE: &str = "../repeater.cpp";

/// The three diagnostic facilities `repeater.cpp` uses.
///
/// A stock C repeater prints none of the nine `debugPrintf` lines, at
/// either revision this port has been read against. At R7.0.10 — the pin
/// — `repeater.cpp` never defines `DEBUG`, so `debugPrintf`
/// (`iocinf.h:28-32`) expands to nothing and all nine compile out; five
/// of them are additionally inside `#ifdef DEBUG`. The runtime facility
/// arrives after the tag, in `e271752158dd` ("Added -d option to
/// caRepeater, sets debug level"), which adds `#define DEBUG`, a
/// file-static `int debug`, `ca_repeater(int setDebug)` and an
/// `if (debug)` / `if (debug > 1)` around most sites — and even there
/// `caRepeater.cpp` `dup2`s stdout to `/dev/null` unless `-d` or `-v`
/// was given. The facility is closed at level 0 both ways.
///
/// The port had it open. A threshold of `0` meant two things at once —
/// "C wrote no `if (debug)` here" and "the facility is compiled in" —
/// and `0 >= 0` holds, so four lines printed unasked. That is not a
/// cosmetic difference: [`crate::repeater::ensure_repeater`]
/// runs the repeater in-process on a background thread when the
/// `caRepeater` binary is absent, so those lines came out of a CA client
/// tool's own stdout, its data channel. [`DebugGate`] removes the dual
/// meaning by having no variant for `0`; the facility opens only at a
/// level the operator asked for, and `-d` keeps its meaning.
///
/// The three facilities are not interchangeable — `debugPrintf` is
/// stdout, `fprintf(stderr, …)` is stderr, and `errlogPrintf` is the
/// errlog queue with its listeners.
#[derive(Clone, Copy)]
pub(crate) struct Diag {
    /// C's file-static `int debug` and the `setDebug` argument that
    /// assigns it, both from `e271752158dd`; at the R7.0.10 pin
    /// `ca_repeater` takes no argument and there is no such variable.
    debug: u8,
}

/// The `if ( debug… )` a C `debugPrintf` site sits inside, and the only
/// thing that can open the facility.
///
/// There is deliberately no variant for `0`. No `debugPrintf` in
/// `repeater.cpp` reaches a terminal at debug 0 — see [`Diag`] — so a
/// `0` threshold describes no C site that exists, and it was the value
/// that put this port's diagnostics on a CA client's stdout.
#[derive(Clone, Copy)]
pub(crate) enum DebugGate {
    /// C `if ( debug )`, and the four sites C leaves unguarded: both are
    /// silent until `-d 1`.
    Debug = 1,
    /// C `if ( debug > 1 )` — the per-datagram and per-client chatter.
    Verbose = 2,
}

impl Diag {
    pub(crate) fn new(debug: u8) -> Self {
        Self { debug }
    }

    /// C `debugPrintf` — `::printf`, i.e. **stdout** — once the facility
    /// is open. `gate` is the `if (debug…)` the C site sits inside;
    /// the sites C leaves unguarded take [`DebugGate::Debug`], because
    /// unguarded in C still means "not without `-d`".
    pub(crate) fn printf(self, gate: DebugGate, args: fmt::Arguments<'_>) {
        if self.debug >= gate as u8 {
            println!("{args}");
        }
    }

    /// C `fprintf ( stderr, … )`. None of `repeater.cpp`'s seven stderr
    /// sites is gated on `debug`, so this takes no threshold.
    pub(crate) fn stderr(self, args: fmt::Arguments<'_>) {
        eprintln!("{args}");
    }

    /// C `errlogPrintf` — the errlog facility, which is a message queue
    /// with listeners, not a stream. Collapsing it onto stderr would lose
    /// the IOC log client and every `errlogAddListener` consumer.
    pub(crate) fn errlog(self, args: fmt::Arguments<'_>) {
        epics_base_rs::runtime::log::errlog_printf(&format!("{args}"));
    }
}

/// C `epicsSocketConvertErrorToString` / `epicsSocketConvertErrnoToString`
/// (`epicsSocketConvertErrnoToString.cpp:25-38`), which are `strerror` into
/// a 64-byte buffer. `io::Error`'s Display is the same sentence with
/// ` (os error N)` appended, so the suffix comes off rather than the text
/// being rebuilt from a table this port would have to keep in step.
pub(crate) fn sock_err_string(e: &io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(errno) => text
            .strip_suffix(&format!(" (os error {errno})"))
            .unwrap_or(&text)
            .to_string(),
        None => text,
    }
}

/// Per-client connected UDP socket, matching C EPICS repeaterClient.
/// Using a connected socket lets the OS detect dead clients via
/// ECONNREFUSED on send().
pub(crate) struct RepeaterClient {
    sock: StdUdpSocket,
    addr: SocketAddr,
    /// C reads the file-static `debug` from the client's constructor,
    /// `sendMessage`, `verify` and its destructor. `Drop` cannot be handed
    /// a parameter, so the client carries the (one-byte, `Copy`) emitter
    /// instead of the module hiding it in a global.
    diag: Diag,
}

impl RepeaterClient {
    /// C `repeaterClient::repeaterClient` + `repeaterClient::connect`
    /// (`repeater.cpp:137-161`). The two fallible steps are C's two, and
    /// each has its own stderr diagnostic: a socket that cannot be made,
    /// and a socket that cannot be connected to the client. Collapsing
    /// them into one `io::Result` the caller answered with `Err(_) =>
    /// return` is why an operator saw a client silently fail to register.
    pub(crate) fn new(addr: SocketAddr, diag: Diag) -> Option<Self> {
        // The constructor announces the client BEFORE `connect` runs, so
        // the line appears even for a client whose socket then fails.
        diag.printf(
            DebugGate::Debug,
            format_args!("New client on port {}", addr.port()),
        );
        let sock = match StdUdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(e) => {
                diag.stderr(format_args!(
                    "{C_FILE}: no client sock because \"{}\"",
                    sock_err_string(&e)
                ));
                return None;
            }
        };
        if let Err(e) = sock.connect(addr) {
            diag.stderr(format_args!(
                "{C_FILE}: unable to connect client sock because \"{}\"",
                sock_err_string(&e)
            ));
            return None;
        }
        // No C counterpart: C's client sockets are blocking, and this port
        // needs the send side non-blocking. Nothing to report.
        sock.set_nonblocking(true).ok()?;
        Some(Self { sock, addr, diag })
    }

    /// C `repeaterClient::sendConfirm` (`repeater.cpp:163-187`): a refused
    /// confirm is the ordinary "client went away" answer and stays silent;
    /// any other send error is reported. The port's `.is_ok()` reported
    /// neither.
    fn send_confirm(&self) -> bool {
        let mut confirm = CaHeader::new(CA_PROTO_REPEATER_CONFIRM);
        if let SocketAddr::V4(v4) = self.addr {
            confirm.available = u32::from_be_bytes(v4.ip().octets());
        }
        match self.sock.send(&confirm.to_bytes()) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => false,
            Err(e) => {
                self.diag.printf(
                    DebugGate::Debug,
                    format_args!(
                        "CA Repeater: confirm req err was \"{}\"",
                        sock_err_string(&e)
                    ),
                );
                false
            }
        }
    }

    fn send_message(&self, data: &[u8]) -> bool {
        // distinguish error kinds. The previous version
        // returned `false` for everything (including transient
        // WouldBlock on a saturated kernel UDP buffer), causing the
        // outer loop to drop the client. Now: keep alive on
        // transient/unknown errors; only treat ECONNREFUSED /
        // EHOSTUNREACH as "client gone".
        //
        // The diagnostics are C `repeaterClient::sendMessage`'s own
        // (`repeater.cpp:189-217`) and belong here, not in the caller:
        // `fanOut` prints nothing, and only this function knows the errno
        // that decides between the two messages.
        match self.sock.send(data) {
            Ok(_) => {
                self.diag.printf(
                    DebugGate::Verbose,
                    format_args!("Sent to port {}", self.addr.port()),
                );
                true
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                self.diag.printf(
                    DebugGate::Debug,
                    format_args!("Client on port {} refused message", self.addr.port()),
                );
                false
            }
            Err(e) => {
                // C `repeater.cpp:213` — a `debugPrintf` C leaves
                // outside any `if (debug)`, which still means "not
                // without `-d`". The RETURN value keeps this port's rule
                // (a transient error must not reap a live client); only
                // the diagnostic is C's.
                self.diag.printf(
                    DebugGate::Debug,
                    format_args!("CA Repeater: UDP send err was \"{}\"", sock_err_string(&e)),
                );
                !matches!(e.kind(), io::ErrorKind::HostUnreachable)
            }
        }
    }

    /// Check if client is still alive by trying to bind to its address.
    /// If bind succeeds, the client has released the port (dead).
    ///
    /// C's bind test binds INADDR_ANY:port (`makeSocket(port, false)`,
    /// `repeater.cpp:91-126`), which works there because C's clients bind
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
            Ok(_) => false, // addr free → client gone
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                // C `repeaterClient::verify` (`repeater.cpp:282-304`).
                self.diag.printf(
                    DebugGate::Verbose,
                    format_args!("Client on port {} is alive", self.addr.port()),
                );
                true
            }
            Err(e) => {
                // C `repeater.cpp:296-302`: a bind test that fails for any
                // reason OTHER than EADDRINUSE is neither "alive" nor a
                // clean departure, and C says so on stderr before
                // returning false. This port answers `true` instead —
                // deliberately, so a transient bind failure cannot reap a
                // live client — but the operator still gets C's line.
                self.diag.stderr(format_args!(
                    "CA Repeater: Bind test error \"{}\"",
                    sock_err_string(&e)
                ));
                true
            }
        }
    }
}

impl Drop for RepeaterClient {
    /// C `repeaterClient::~repeaterClient` (`repeater.cpp:219-228`) closes
    /// the socket and announces the departure. Putting it in `Drop` is what
    /// makes the line cover EVERY removal path — `fanOut`'s send-failure
    /// reap, `verifyClients`' bind-test sweep, the confirm-failure removal
    /// and the table teardown — instead of only the one the port used to
    /// print from.
    fn drop(&mut self) {
        self.diag.printf(
            DebugGate::Debug,
            format_args!("Deleted client on port {}", self.addr.port()),
        );
    }
}

/// Outcome of decoding a single incoming repeater datagram. Mirrors
/// C `ca_repeater()` (`repeater.cpp:601-625`):
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
pub(crate) struct DatagramAction {
    pub(crate) register: bool,
    pub(crate) fanout: Option<Vec<u8>>,
}

pub(crate) fn decode_datagram(buf: &[u8], hdr: &CaHeader, src: SocketAddr) -> DatagramAction {
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
        // Per C `repeater.cpp:614-618`: rewrite m_available on
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
pub(crate) fn fan_out(clients: &mut HashMap<u16, RepeaterClient>, src: SocketAddr, data: &[u8]) {
    let mut dead = Vec::new();
    for (port, client) in clients.iter() {
        // Don't reflect back to sender. C `fanOut` (repeater.cpp:330-341)
        // skips the originating client via `identicalAddress`, which
        // compares the FULL address (family + port + IP), not just the
        // port. Matching on port alone wrongly suppresses a beacon to a
        // local client whose ephemeral registration port happens to
        // equal the beacon's source (server) port.
        if client.addr == src {
            continue;
        }
        if !client.send_message(data) && !client.verify() {
            dead.push(*port);
        }
    }
    // C `fanOut` itself prints nothing: the send diagnostics belong to
    // `repeaterClient::sendMessage`, the liveness one to
    // `repeaterClient::verify`, and the departure one to the destructor —
    // which here is `Drop`, run by `HashMap::remove`. `Verified %u active
    // clients` is `verifyClients`' line (`repeater.cpp:331-333`) and was
    // never emitted from `fanOut` in C.
    for p in dead {
        clients.remove(&p);
    }
}

/// Parse `EPICS_CAS_BEACON_ADDR_LIST` (or fall back to `EPICS_CA_ADDR_LIST`)
/// and join every multicast group (224.0.0.0/4) on `INADDR_ANY`. Any address
/// that isn't multicast is silently skipped. A join failure is reported and
/// never aborts: the repeater keeps running for unicast/broadcast beacons.
///
/// `default_port` is the repeater port C passes to
/// `addAddrToChannelAccessAddressList` (`repeater.cpp:533-534`) and which
/// therefore appears in the failure line, since C renders the address with
/// `ipAddrToDottedIP` — `a.b.c.d:port` (`osiSock.c:166-169`).
pub(crate) fn join_beacon_multicast_groups(sock: &socket2::Socket, default_port: u16, diag: Diag) {
    let list = epics_base_rs::runtime::env_table::EPICS_CAS_BEACON_ADDR_LIST
        .get()
        .or_else(|| epics_base_rs::runtime::env_table::EPICS_CA_ADDR_LIST.get());
    let Some(list) = list else {
        return;
    };
    for token in list.split_whitespace() {
        // Split the optional :port suffix; the address decides whether we
        // join, the port only spells the diagnostic the way C does.
        let (host, port) = match token.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().unwrap_or(default_port)),
            None => (token, default_port),
        };
        let Ok(addr) = host.parse::<Ipv4Addr>() else {
            continue;
        };
        if !addr.is_multicast() {
            continue;
        }
        if let Err(e) = sock.join_multicast_v4(&addr, &Ipv4Addr::UNSPECIFIED) {
            // C `repeater.cpp:563` — `errlogPrintf`, not a stream: the
            // line has to reach `errlogAddListener` consumers (the IOC log
            // client among them), which a `tracing::warn!` with different
            // wording never did.
            diag.errlog(format_args!(
                "caR: Socket mcast join to {addr}:{port} failed: {}",
                sock_err_string(&e)
            ));
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
pub(crate) const MAX_REPEATER_CLIENTS: usize = 1024;

/// C `verifyClients` (`repeater.cpp:310-325`) — bind-test EVERY registered
/// client and reap the ones whose port is now free.
///
/// This is unconditional: it does NOT wait for a send to fail. C's own
/// comment at the call site (`repeater.cpp:463-474`) gives the reason —
/// "an ICMP error return does not get through to send(), which returns no
/// error code" on some platforms — so send-failure reaping alone leaks stale
/// clients there.
///
/// C closes with `debugPrintf("Verified %u active clients\n",
/// theClients.count())` — the SURVIVOR count, and this is the only function
/// in `repeater.cpp` that prints it. The port used to print it from `fanOut`
/// and from the registration wrapper, and to print an invented "Reaped N
/// departed client(s)" here instead. That line is post-pin: it and its
/// `if (debug)` arrive in `e271752158dd`, so R7.0.10's `verifyClients`
/// (`:310-325`) prints nothing at all.
pub(crate) fn verify_clients(clients: &mut HashMap<u16, RepeaterClient>, diag: Diag) {
    let dead: Vec<u16> = clients
        .iter()
        .filter(|(_, c)| !c.verify())
        .map(|(p, _)| *p)
        .collect();
    for p in dead {
        clients.remove(&p);
    }
    diag.printf(
        DebugGate::Debug,
        format_args!("Verified {} active clients", clients.len()),
    );
}

/// C `register_new_client` (`repeater.cpp:358-477`), in C's order:
/// find-or-create → sendConfirm (removing the client on failure, whether it
/// is new or not) → noop fan-out to every OTHER client (on EVERY
/// registration, so a repeat registration still exercises their sockets) →
/// `verifyClients` when this registration created a client.
pub(crate) fn register_client(
    clients: &mut HashMap<u16, RepeaterClient>,
    src: SocketAddr,
    diag: Diag,
) {
    let port = src.port();

    let mut new_client = false;
    if !clients.contains_key(&port) {
        // Rust-only soft cap (C has none): sweep first so a full table of
        // departed clients still admits a live one.
        if clients.len() >= MAX_REPEATER_CLIENTS {
            verify_clients(clients, diag);
            if clients.len() >= MAX_REPEATER_CLIENTS {
                // All slots are alive — refuse the new client. The peer
                // sees no CONFIRM and retries (every 1 s, per
                // `repeaterSubscribeTimer`), so it gets in as soon as a
                // slot frees.
                return;
            }
        }
        // Per-client connected socket, as C's `repeaterClient::connect`.
        let Some(client) = RepeaterClient::new(src, diag) else {
            return;
        };
        clients.insert(port, client);
        new_client = true;
    }

    // C `repeater.cpp:443-452`: a client whose CONFIRM cannot be sent is
    // removed — including an ALREADY-REGISTERED one, which the pre-fix Rust
    // kept (it returned early after `send_confirm()`, ignoring the result).
    let confirmed = clients.get(&port).is_some_and(|c| c.send_confirm());
    if !confirmed {
        clients.remove(&port);
        diag.printf(
            DebugGate::Debug,
            format_args!("Deleted repeater client on port {port}, error sending ack"),
        );
    }

    // C `repeater.cpp:454-461`: "send a noop message to all other clients so
    // that we don't accumulate sockets when there are no beacons". Sent on
    // every registration message, not only on the one that created a client
    // — and clients now re-register once a second until confirmed (R6-22),
    // so this is the sweep that runs in a beacon-less network.
    let noop = CaHeader::new(CA_PROTO_VERSION);
    fan_out(clients, src, &noop.to_bytes());

    // C `repeater.cpp:463-476`: the bind-test sweep, run whenever a
    // registration created a client, and deliberately AFTER the confirm above
    // so the new client is never reaped before it is acknowledged. It carries
    // its own `Verified %u active clients` line.
    if new_client {
        verify_clients(clients, diag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddrV4;

    fn header_bytes(cmmd: u16, available: u32) -> Vec<u8> {
        let mut h = CaHeader::new(cmmd);
        h.available = available;
        h.to_bytes().to_vec()
    }

    fn src_v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), port))
    }

    /// Debug level 0. The four unguarded `debugPrintf` sites still print at
    /// this level — as they do in C — but none of them fires in the tests
    /// below, which exercise the client table rather than the daemon.
    fn quiet() -> Diag {
        Diag::new(0)
    }

    #[test]
    fn beacon_rewrites_zero_m_available_with_source_ip() {
        // RSRV_IS_UP with m_available=0 → repeater fills in the
        // sender's IP (C `repeater.cpp:614-618`).
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
        // m_available is NOT rewritten (C `repeater.cpp:601-625` only
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
        // C `fanOut` (repeater.cpp:330-341) skips the originating
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
        clients.insert(
            port,
            RepeaterClient::new(local, quiet()).expect("client sock"),
        );

        let data = header_bytes(CA_PROTO_RSRV_IS_UP, 0x0a00_0005);

        // (1) Beacon from a DIFFERENT IP but the SAME port → the client
        // must NOT be skipped; it receives the fanned-out datagram.
        let server_src = src_v4(10, 0, 0, 5, port);
        fan_out(&mut clients, server_src, &data);
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
        fan_out(&mut clients, local, &data);
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
    /// (`repeater.cpp:463-476`), regardless of whether any send failed. The
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
            RepeaterClient::new(departed, quiet()).expect("departed client sock"),
        );
        clients.insert(
            live.port(),
            RepeaterClient::new(live, quiet()).expect("live sock"),
        );
        assert_eq!(clients.len(), 2);

        // A brand-new client registers. C: newClient ⇒ verifyClients().
        let newcomer_sock = StdUdpSocket::bind("127.0.0.1:0").expect("bind newcomer");
        let newcomer = newcomer_sock.local_addr().unwrap();
        register_client(&mut clients, newcomer, quiet());

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
    /// (`repeater.cpp:454-461`) — the pre-fix Rust returned early after the
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
            RepeaterClient::new(other_addr, quiet()).expect("other sock"),
        );
        clients.insert(
            repeat_addr.port(),
            RepeaterClient::new(repeat_addr, quiet()).expect("repeat sock"),
        );

        register_client(&mut clients, repeat_addr, quiet());

        let mut buf = [0u8; 64];
        let n = other
            .recv(&mut buf)
            .expect("a repeat registration must still noop-fan-out to other clients");
        let hdr = CaHeader::from_bytes(&buf[..n]).expect("noop parses");
        assert_eq!(hdr.cmmd, CA_PROTO_VERSION, "the noop is a VERSION frame");
    }

    /// Every errno the seven restored `repeater.cpp` stderr/`debugPrintf`
    /// error sites can carry, rendered by [`sock_err_string`] and compared
    /// against the `strerror` that C's `epicsSocketConvertErrorToString`
    /// copies into its buffer (`epicsSocketConvertErrnoToString.cpp:25-31`).
    ///
    /// The syscalls at `:148 :158 :187 :216 :307 :391 :526` cannot be forced
    /// without resource pressure, but the RENDERING half is a pure function
    /// and this pins it: `io::Error`'s Display appends ` (os error N)` to the
    /// same sentence, and a port that shipped that suffix would print bytes C
    /// never prints.
    #[cfg(unix)]
    #[test]
    fn sock_err_string_renders_what_c_strerror_renders() {
        use std::ffi::CStr;

        // Grouped by the site that produces them.
        let errnos = [
            // `:148` / `:391` / `:526` — socket create and bind.
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::EAFNOSUPPORT,
            // `:158` — connect.
            libc::EACCES,
            libc::ENETUNREACH,
            // `:187` — the CONFIRM send.
            libc::EMSGSIZE,
            libc::EPERM,
            // `:216` — the fan-out send.
            libc::EHOSTUNREACH,
            libc::ENETDOWN,
            // `:307` — the bind test.
            libc::EADDRNOTAVAIL,
            libc::EADDRINUSE,
        ];

        for errno in errnos {
            // SAFETY: `strerror` returns a pointer to a static/thread buffer
            // valid until the next call; it is copied before anything else
            // can call it. This is the exact function C reaches through
            // `epicsSocketConvertErrorToString`.
            let c_text = unsafe { CStr::from_ptr(libc::strerror(errno)) }
                .to_string_lossy()
                .into_owned();
            let ours = sock_err_string(&io::Error::from_raw_os_error(errno));
            assert_eq!(
                ours, c_text,
                "errno {errno} must render as C's strerror does"
            );
            assert!(
                !ours.contains("(os error"),
                "errno {errno} kept Rust's suffix: {ours:?}"
            );
            // C copies into `char sockErrBuf[64]` and truncates
            // (`strncpy` + explicit NUL at `[63]`). No errno these sites can
            // produce reaches that bound, so the port needs no truncation —
            // this is the assertion that would tell us if that changed.
            assert!(
                ours.len() < 64,
                "errno {errno} renders {} bytes, past C's 64-byte sockErrBuf: {ours:?}",
                ours.len()
            );
        }

        // A non-OS error has no suffix to strip and must pass through, as C's
        // `strerror` has nothing to say about it either.
        let other = io::Error::other("stream did not contain valid UTF-8");
        assert_eq!(
            sock_err_string(&other),
            "stream did not contain valid UTF-8"
        );
    }

    /// C `repeater.cpp:563` reports a failed `IP_ADD_MEMBERSHIP` through
    /// `errlogPrintf` — the message queue with its listeners, not a stream —
    /// and renders the group with `ipAddrToDottedIP`, i.e. `a.b.c.d:port`
    /// (`osiSock.c:166-169`). The port used a `tracing::warn!` carrying
    /// different words, which no `errlogAddListener` consumer (the IOC log
    /// client among them) ever saw.
    ///
    /// Forced without resource pressure: a second `IP_ADD_MEMBERSHIP` for a
    /// membership the socket already holds fails deterministically. *Which*
    /// failure is the platform's — POSIX answers `EADDRINUSE`, Winsock
    /// answers `WSAEINVAL` — so only C's half of the line is compared on
    /// every host, and the errno is named where it is knowable.
    #[test]
    fn failed_mcast_join_reaches_the_errlog_with_cs_bytes() {
        use std::sync::{Arc, Mutex};

        let group = "224.0.2.3";
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", group) };

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let listener = epics_base_rs::runtime::log::errlog_add_listener(move |line| {
            if line.starts_with("caR: ") {
                sink.lock().expect("errlog sink").push(line.to_string());
            }
        });

        let sock = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .expect("probe socket");
        let port = 5065u16;

        // First join: the membership is taken, and C says nothing.
        join_beacon_multicast_groups(&sock, port, Diag::new(0));
        epics_base_rs::runtime::log::errlog_flush();
        assert!(
            seen.lock().expect("errlog sink").is_empty(),
            "a join that succeeds must be silent; got {:?}",
            seen.lock().expect("errlog sink")
        );

        // Second join of the same membership: EADDRINUSE.
        join_beacon_multicast_groups(&sock, port, Diag::new(0));
        epics_base_rs::runtime::log::errlog_flush();
        epics_base_rs::runtime::log::errlog_remove_listener(listener);

        let lines = seen.lock().expect("errlog sink").clone();
        let prefix = format!("caR: Socket mcast join to {group}:{port} failed: ");
        assert_eq!(
            lines.len(),
            1,
            "one line for one failed join, got {lines:?}"
        );
        let reason = lines[0]
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("the failed join must carry C's bytes, got {:?}", lines[0]));
        assert!(!reason.is_empty(), "C interpolates a reason, never nothing");

        // `libc::EADDRINUSE` is the CRT constant on Windows, not the Winsock
        // one, and `from_raw_os_error` there takes Win32 codes — naming it
        // would compare this sentence against an unrelated code's.
        #[cfg(unix)]
        assert_eq!(
            reason,
            sock_err_string(&io::Error::from_raw_os_error(libc::EADDRINUSE))
        );
    }
}
