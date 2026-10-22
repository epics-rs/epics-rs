//! One rule for every test that has to NAME the port its subject binds.
//!
//! Most tests do not: the process that binds can ask the kernel for `0` and
//! report back what it got, and then no number was ever guessed. A few
//! cannot, and they are the ones this crate is for — the property under test
//! *is* that a named number is honoured (an st.cmd line, an
//! `EPICS_CAS_SERVER_PORT` boot assignment), or the child is C's `softIoc`,
//! which takes its port from the environment and says nothing about what it
//! bound.
//!
//! For those, a candidate port is a hint and never a reservation: the probe
//! that found it has closed its socket by the time the subject binds, so
//! between the two the number belongs to whoever asks the kernel next. Under
//! a parallel suite that happens often enough to turn a test red — measured
//! as a 0.055 s failure of a boot test while a second full-workspace run was
//! on the same box.
//!
//! # The rule
//!
//! *A test that must name a port treats "the subject did not get the port it
//! was told to use" as a retry with a fresh candidate, never as a failure.*
//!
//! The assertion the test is making is untouched by that: the boot line still
//! has to be honoured, the st.cmd line still has to bind. What changes is the
//! verdict on losing a race the test never meant to enter.
//!
//! # The dangerous half
//!
//! A retry is only safe when the evidence is specific to *somebody else owns
//! this number*. Retrying on a general failure — "the client could not
//! connect", "no value came back" — silently converts a real regression into
//! [`ATTEMPTS`] slow attempts and then a confusing panic, which is worse than
//! the flake it replaces. So [`on_a_named_port`]'s closure returns `None`
//! only on evidence naming the port, and every caller here names its own:
//!
//! * the asyn port object is absent after `drvAsynIPServerPortConfigure`,
//!   which the handler unregisters *only* on a failed bind;
//! * `realtime-ca-ioc` prints `cannot start the CA TCP server on port <p>:
//!   Address already in use` and exits 1;
//! * C `softIoc` prints `cas WARNING: Configured TCP port was unavailable.`
//!   and then `CAS: No TCP server started` (measured against R7.0.10 —
//!   `rsrv_init` reaches `cantProceed` and the process suspends rather than
//!   exiting, so "the child is gone" is *not* the discriminator there).
//!
//! Anything else the subject does is a failure and must reach the test as
//! one.
//!
//! [`verdict`] reads the two IOC consoles for that evidence.

#[cfg(target_os = "linux")]
pub mod port_holders;
pub mod verdict;

use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};

use epics_libcom_rs::net::port_held;
use socket2::{Domain, Socket, Type};

/// Build `shim/getsockname_einval.c` into `dir` and return the `.so`: under
/// `LD_PRELOAD` it fails `getsockname` with EINVAL on a listening socket, as
/// RTEMS does on the CA listener, so a host run reaches the console
/// [`verdict::LOCAL_ADDR_FAILED`] records. Panics when `cc` cannot build it.
pub fn getsockname_einval_shim(dir: &std::path::Path) -> std::path::PathBuf {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/shim/getsockname_einval.c");
    let so = dir.join("getsockname_einval.so");
    let out = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&so)
        .arg(src)
        .arg("-ldl")
        .output()
        .expect("run cc");
    assert!(
        out.status.success(),
        "cc could not build {src}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    so
}

/// How many candidates a test may burn before the run is called broken.
///
/// A steal is a coincidence of timing; sixteen of them in a row is a box
/// where something is systematically taking the numbers, and reporting that
/// is more useful than trying forever.
pub const ATTEMPTS: usize = 16;

/// Run `attempt` on candidate ports until one of them survives to the bind,
/// never on the same number twice ([`Candidates`]).
///
/// `attempt` returns `Some(value)` when the subject came up on the number it
/// was given, and `None` **only** on evidence that this particular number was
/// taken by somebody else — see the crate docs. Panics with the whole
/// candidate list when [`ATTEMPTS`] have been lost, because at that point the
/// number is not the problem.
pub fn on_a_named_port<T>(attempt: impl FnMut(u16) -> Option<T>) -> T {
    on_a_named_port_from(free_for_tcp_and_udp, attempt)
}

/// [`on_a_named_port`] with the candidate drawn by `pick`, for a subject
/// that needs more of the number than the same value free on TCP and UDP
/// (see [`free_tcp_with_udp_successor`]).
pub fn on_a_named_port_from<T>(
    pick: impl FnMut() -> u16,
    mut attempt: impl FnMut(u16) -> Option<T>,
) -> T {
    let mut candidates = Candidates::new(pick);
    for port in candidates.by_ref() {
        if let Some(won) = attempt(port) {
            return won;
        }
    }
    candidates.exhausted()
}

/// How many times [`Candidates`] re-draws a number the search already tried
/// before it calls the pick broken.
const REPICKS: usize = 64;

/// The candidates of one search, each a number no earlier attempt of the
/// search was given. A probe closes its socket before it returns, so the
/// kernel can hand the same number back to the next probe; a repeat is
/// drawn again here rather than retried.
pub struct Candidates<P> {
    pick: P,
    tried: Vec<u16>,
}

impl<P: FnMut() -> u16> Candidates<P> {
    pub fn new(pick: P) -> Self {
        Self {
            pick,
            tried: Vec::with_capacity(ATTEMPTS),
        }
    }

    /// Panic with every candidate given out: the search lost them all.
    pub fn exhausted(&self) -> ! {
        panic!(
            "every one of {ATTEMPTS} candidate ports was taken before the subject \
             could bind it: {:?}. That is no longer a lost race — something \
             on this host is taking the numbers, or the subject is refusing every \
             port for a reason its retry evidence is misreporting as a steal.",
            self.tried
        );
    }
}

impl<P: FnMut() -> u16> Iterator for Candidates<P> {
    type Item = u16;

    /// The next untried candidate, `None` once [`ATTEMPTS`] have been given
    /// out. Panics when `pick` returns only tried numbers 64 times
    /// in a row.
    fn next(&mut self) -> Option<u16> {
        if self.tried.len() == ATTEMPTS {
            return None;
        }
        for _ in 0..REPICKS {
            let port = (self.pick)();
            if !self.tried.contains(&port) {
                self.tried.push(port);
                return Some(port);
            }
        }
        panic!(
            "the candidate source returned only already-tried ports {REPICKS} times \
             in a row; tried: {:?}",
            self.tried
        );
    }
}

/// A port number that was free for both TCP and UDP a moment ago.
///
/// Both, because a CA server binds its TCP listener and its UDP search
/// socket on the same number, so probing one leaves the other free to
/// collide. A hint, never a reservation — both probe sockets are closed
/// before this returns, and they must be, or the subject could not bind.
///
/// Probed on the wildcard: a subject binds every NIC, and a number free on
/// 127.0.0.1 can already be held on one of them.
pub fn free_for_tcp_and_udp() -> u16 {
    for _ in 0..64 {
        let tcp = TcpListener::bind("0.0.0.0:0").expect("bind an ephemeral TCP port");
        let port = tcp.local_addr().expect("read the ephemeral port").port();
        if UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
    panic!("no port was free for both TCP and UDP after 64 attempts");
}

/// A port number free on TCP whose successor is free on UDP: a PVA server
/// started with only `EPICS_PVAS_SERVER_PORT` searches on `port + 1`
/// whatever `EPICS_PVAS_BROADCAST_PORT` says. A hint, as
/// [`free_for_tcp_and_udp`] is.
pub fn free_tcp_with_udp_successor() -> u16 {
    for _ in 0..64 {
        let tcp = TcpListener::bind("0.0.0.0:0").expect("bind an ephemeral TCP port");
        let port = tcp.local_addr().expect("read the ephemeral port").port();
        if port < u16::MAX && UdpSocket::bind(("0.0.0.0", port + 1)).is_ok() {
            return port;
        }
    }
    panic!("no TCP port with a free UDP successor after 64 attempts");
}

/// Whether another socket holds `port` on TCP now: the kernel's own answer,
/// which is the evidence a test needs before it calls a missing bind a lost
/// race and not a regression. On every interface, as [`udp_port_is_held`].
pub fn tcp_port_is_held(port: u16) -> bool {
    port_is_held(Type::STREAM, port)
}

/// [`tcp_port_is_held`] for UDP, on every interface as a PVA search socket
/// binds it.
pub fn udp_port_is_held(port: u16) -> bool {
    port_is_held(Type::DGRAM, port)
}

/// A wildcard bind of `port` that only another socket on the number refuses.
///
/// Linux: `SO_REUSEADDR` on TCP, as std's `TcpListener` sets it, so a port
/// left in TIME_WAIT reads free; Linux still refuses it beside a listener on
/// any address. Darwin and the BSDs: none, because there `SO_REUSEADDR` lets
/// a wildcard bind share the number with a socket on one address
/// (`in_pcbbind`). Windows: `SO_EXCLUSIVEADDRUSE`, because a plain Winsock
/// wildcard bind succeeds beside such a socket. Off Linux a port with
/// connections left in TIME_WAIT may therefore read held.
fn port_is_held(ty: Type, port: u16) -> bool {
    let sock = Socket::new(Domain::IPV4, ty, None).expect("open a probe socket");
    #[cfg(target_os = "linux")]
    if ty == Type::STREAM {
        sock.set_reuse_address(true)
            .expect("SO_REUSEADDR on the probe");
    }
    #[cfg(windows)]
    exclusive_address_use(&sock);
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    matches!(sock.bind(&addr.into()), Err(e) if port_held(&e))
}

/// A port a test holds from first bind to last, for a subject that binds or
/// dials the number itself. Probed, released and rebound, the number is free
/// for any socket on the box in between — under load an outgoing connect's
/// source port — and the subject then falls back, fails, or reaches a
/// stranger. The hold never listens and is never handed out by a plain
/// `bind(0)` or by `connect()`; what else may bind beside it is the
/// constructor's choice.
pub struct HeldPort {
    /// The hold, then any socket that keeps it in place (its peer).
    holds: Vec<Socket>,
    pub port: u16,
}

/// Darwin and the BSDs deliver to an unconnected hold what Linux keeps from
/// it: a SYN for the number to a bound socket that is not listening, which
/// drops it (`tcp_input`: `TCPS_CLOSED`) so a connect times out instead of
/// being refused, and UDP unicast to the oldest wildcard bind
/// (`in_pcblookup_hash` keeps the last match of a head-inserted chain).
/// Both lookups skip a connected socket.
const BSD_PCB_LOOKUP: bool = cfg!(all(unix, not(target_os = "linux")));

impl HeldPort {
    /// A TCP number for a listener under test, which binds beside the hold:
    /// on unix both sides set `SO_REUSEADDR` and the hold is not listening;
    /// on Windows a specific bind over an unoptioned wildcard succeeds.
    pub fn tcp() -> Self {
        let hold = Socket::new(Domain::IPV4, Type::STREAM, None).expect("open a TCP hold");
        #[cfg(unix)]
        hold.set_reuse_address(true)
            .expect("SO_REUSEADDR on the hold");
        Self::bind(hold)
    }

    /// A UDP number for a socket under test that sets the datagram fanout
    /// options (`SO_REUSEADDR`, and `SO_REUSEPORT` on unix). On the wildcard,
    /// so a plain bind on any address is refused, which would otherwise fail
    /// the subject's wildcard bind. Linux: `SO_REUSEADDR` alone, outside the
    /// subject's `SO_REUSEPORT` group, so unicast goes to the later-bound
    /// subject and none is hashed to the hold. Darwin and the BSDs share a
    /// UDP address only when both sides set `SO_REUSEPORT`, and the hold is
    /// connected ([`BSD_PCB_LOOKUP`]); a plain bind on another address then
    /// binds there, and cannot fail the subject's: their `SO_REUSEADDR`
    /// bind looks only at unconnected sockets on its own address.
    pub fn udp() -> Self {
        let hold = Socket::new(Domain::IPV4, Type::DGRAM, None).expect("open a UDP hold");
        hold.set_reuse_address(true)
            .expect("SO_REUSEADDR on the hold");
        #[cfg(all(unix, not(target_os = "linux")))]
        hold.set_reuse_port(true).expect("SO_REUSEPORT on the hold");
        let held = Self::bind(hold);
        if BSD_PCB_LOOKUP {
            // The discard port: a peer nothing sends from.
            let discard = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
            held.holds[0]
                .connect(&discard.into())
                .expect("connect the hold");
        }
        held
    }

    /// A UDP number for asyn's `udp&`, which sets `SO_REUSEPORT` alone
    /// before its bind (`SO_REUSEADDR` where the platform has no
    /// `SO_REUSEPORT`), so the hold has to be a member of that group. It is
    /// unconnected and so takes a share of the group's unicast: only for a
    /// test that binds the number and sends nothing to it.
    pub fn reuse_port_udp() -> Self {
        let hold = Socket::new(Domain::IPV4, Type::DGRAM, None).expect("open a UDP hold");
        #[cfg(unix)]
        hold.set_reuse_port(true).expect("SO_REUSEPORT on the hold");
        #[cfg(not(unix))]
        hold.set_reuse_address(true)
            .expect("SO_REUSEADDR on the hold");
        Self::bind(hold)
    }

    /// A TCP number nothing answers on: a connect is refused, and no plain
    /// bind takes it (unix: the hold sets no reuse option; Windows:
    /// `SO_EXCLUSIVEADDRUSE`, which refuses every other bind). Darwin and
    /// the BSDs: connected to a loopback peer it keeps
    /// ([`BSD_PCB_LOOKUP`]), so there an `SO_REUSEADDR` bind, whose
    /// lookup sees only unconnected sockets, is not refused.
    pub fn closed_tcp() -> Self {
        let held = Self::closed(Type::STREAM);
        if BSD_PCB_LOOKUP {
            held.connected()
        } else {
            held
        }
    }

    /// [`HeldPort::closed_tcp`] for UDP: a datagram sent to it is absorbed
    /// and never answered, and no other socket binds it on loopback or the
    /// wildcard. Darwin and the BSDs: an `SO_REUSEADDR` bind there conflicts
    /// only with a socket on its own address, so the hold is a plain socket
    /// on loopback and, beside it, one on the wildcard with `SO_REUSEADDR`
    /// alone, which refuses every bind short of both sides setting
    /// `SO_REUSEPORT`.
    pub fn closed_udp() -> Self {
        if !BSD_PCB_LOOKUP {
            return Self::closed(Type::DGRAM);
        }
        let lo = Socket::new(Domain::IPV4, Type::DGRAM, None).expect("open a closed hold");
        let mut held = Self::bind_on(lo, Ipv4Addr::LOCALHOST);
        let any = Socket::new(Domain::IPV4, Type::DGRAM, None).expect("open a closed hold");
        any.set_reuse_address(true)
            .expect("SO_REUSEADDR on the hold");
        any.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, held.port)).into())
            .expect("bind the wildcard hold");
        held.holds.push(any);
        held
    }

    fn closed(ty: Type) -> Self {
        let hold = Socket::new(Domain::IPV4, ty, None).expect("open a closed hold");
        #[cfg(windows)]
        exclusive_address_use(&hold);
        Self::bind(hold)
    }

    fn bind(hold: Socket) -> Self {
        Self::bind_on(hold, Ipv4Addr::UNSPECIFIED)
    }

    fn bind_on(hold: Socket, ip: Ipv4Addr) -> Self {
        let at = SocketAddr::from((ip, 0));
        hold.bind(&at.into()).expect("bind the hold");
        let port = hold
            .local_addr()
            .expect("read the held port")
            .as_socket()
            .expect("an inet address")
            .port();
        Self {
            holds: vec![hold],
            port,
        }
    }

    /// The TCP hold connected to a loopback peer, which is kept beside it.
    fn connected(mut self) -> Self {
        let peer = TcpListener::bind("127.0.0.1:0").expect("bind the hold's peer");
        let at = peer.local_addr().expect("read the peer's port");
        self.holds[0].connect(&at.into()).expect("connect the hold");
        let (accepted, _) = peer.accept().expect("accept the hold");
        self.holds.push(accepted.into());
        self
    }
}

#[cfg(windows)]
fn exclusive_address_use(sock: &Socket) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{SO_EXCLUSIVEADDRUSE, SOL_SOCKET, setsockopt};
    let on: i32 = 1;
    // SAFETY: a live socket handle, and `on` outlives the call.
    let rc = unsafe {
        setsockopt(
            sock.as_raw_socket() as _,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&on as *const i32).cast(),
            size_of::<i32>() as i32,
        )
    };
    assert_eq!(
        rc,
        0,
        "SO_EXCLUSIVEADDRUSE on the probe: {}",
        std::io::Error::last_os_error()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The held/free boundary: a bound socket reads held, the number it
    /// leaves behind reads free.
    #[test]
    fn a_bound_port_reads_held_and_a_released_one_free() {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = UdpSocket::bind("0.0.0.0:0").unwrap();
        let (t, u) = (
            tcp.local_addr().unwrap().port(),
            udp.local_addr().unwrap().port(),
        );
        assert!(tcp_port_is_held(t) && udp_port_is_held(u));
        drop((tcp, udp));
        assert!(!tcp_port_is_held(t) && !udp_port_is_held(u));
    }

    fn plain_bind(ty: Type, ip: [u8; 4], port: u16) -> std::io::Result<Socket> {
        let s = Socket::new(Domain::IPV4, ty, None).unwrap();
        s.bind(&SocketAddr::from((ip, port)).into()).map(|()| s)
    }

    /// The hold keeps a plain bind off the number and lets the subject's on.
    #[test]
    fn a_held_port_refuses_a_plain_bind_and_takes_the_subjects() {
        let tcp = HeldPort::tcp();
        assert!(plain_bind(Type::STREAM, [0, 0, 0, 0], tcp.port).is_err());
        let listener = TcpListener::bind(("127.0.0.1", tcp.port)).unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), tcp.port);

        let udp = HeldPort::udp();
        assert!(plain_bind(Type::DGRAM, [0, 0, 0, 0], udp.port).is_err());
        let search = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
        search.set_reuse_address(true).unwrap();
        #[cfg(unix)]
        search.set_reuse_port(true).unwrap();
        search
            .bind(&SocketAddr::from(([0, 0, 0, 0], udp.port)).into())
            .unwrap();
    }

    /// A plain bind on another loopback address cannot fail the subject's
    /// wildcard fanout bind: Linux would, so the hold refuses it there.
    #[test]
    fn a_plain_bind_on_another_address_leaves_the_subject_its_bind() {
        let held = HeldPort::udp();
        let stranger = plain_bind(Type::DGRAM, [127, 0, 0, 2], held.port);
        if cfg!(target_os = "linux") {
            assert!(stranger.is_err());
        }
        let subject = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
        subject.set_reuse_address(true).unwrap();
        #[cfg(unix)]
        subject.set_reuse_port(true).unwrap();
        subject
            .bind(&SocketAddr::from(([0, 0, 0, 0], held.port)).into())
            .unwrap();
    }

    /// Every datagram to the number reaches the subject's fanout socket, none
    /// the hold: one client socket per datagram, so each is hashed afresh.
    #[test]
    fn a_udp_hold_takes_none_of_the_subjects_unicast() {
        let held = HeldPort::udp();
        let subject = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
        subject.set_reuse_address(true).unwrap();
        #[cfg(unix)]
        subject.set_reuse_port(true).unwrap();
        subject
            .bind(&SocketAddr::from(([0, 0, 0, 0], held.port)).into())
            .unwrap();
        let subject = UdpSocket::from(subject);
        subject
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        for _ in 0..64 {
            UdpSocket::bind("127.0.0.1:0")
                .unwrap()
                .send_to(b"x", ("127.0.0.1", held.port))
                .unwrap();
            subject
                .recv(&mut [0; 1])
                .expect("the subject got the datagram");
        }
    }

    /// The `udp&` hold lets a second `SO_REUSEPORT`-only bind on, and still
    /// refuses a plain one.
    #[test]
    fn a_reuse_port_hold_takes_a_reuse_port_bind_only() {
        let held = HeldPort::reuse_port_udp();
        let member = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
        #[cfg(unix)]
        member.set_reuse_port(true).unwrap();
        #[cfg(not(unix))]
        member.set_reuse_address(true).unwrap();
        member
            .bind(&SocketAddr::from(([127, 0, 0, 1], held.port)).into())
            .unwrap();
        assert!(plain_bind(Type::DGRAM, [0, 0, 0, 0], held.port).is_err());
    }

    /// A closed hold refuses a connect and every plain bind; a listener's
    /// `SO_REUSEADDR` one too, except beside Darwin's connected hold.
    #[test]
    fn a_closed_hold_answers_nothing_and_shares_with_nobody() {
        let tcp = HeldPort::closed_tcp();
        let refused = std::net::TcpStream::connect(("127.0.0.1", tcp.port)).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
        assert_eq!(
            TcpListener::bind(("127.0.0.1", tcp.port)).is_err(),
            !BSD_PCB_LOOKUP
        );
        assert!(plain_bind(Type::STREAM, [0, 0, 0, 0], tcp.port).is_err());
        assert!(plain_bind(Type::STREAM, [127, 0, 0, 1], tcp.port).is_err());

        let udp = HeldPort::closed_udp();
        assert!(UdpSocket::bind(("127.0.0.1", udp.port)).is_err());
        for ip in [[127, 0, 0, 1], [0, 0, 0, 0]] {
            let fanout = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
            fanout.set_reuse_address(true).unwrap();
            #[cfg(unix)]
            fanout.set_reuse_port(true).unwrap();
            assert!(
                fanout
                    .bind(&SocketAddr::from((ip, udp.port)).into())
                    .is_err(),
                "{ip:?}"
            );
        }
    }

    /// The winning attempt's value comes back, and no further candidate is
    /// burned after it.
    #[test]
    fn a_win_ends_the_search() {
        let mut seen = Vec::new();
        let port = on_a_named_port(|p| {
            seen.push(p);
            (seen.len() == 2).then_some(p)
        });
        assert_eq!(seen.len(), 2, "the search stops at the first win");
        assert_eq!(port, seen[1]);
    }

    /// Every candidate is a fresh number, so a retry is not a retry of the
    /// same losing bet.
    #[test]
    fn each_attempt_gets_its_own_candidate() {
        let mut seen = Vec::new();
        on_a_named_port(|p| {
            seen.push(p);
            (seen.len() == 4).then_some(())
        });
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), seen.len(), "candidates repeated: {seen:?}");
    }

    /// A pick that hands back a number already tried — a closed probe's port
    /// the kernel gave out again — is drawn again, never retried.
    #[test]
    fn a_repeated_pick_never_reaches_attempt_twice() {
        let mut picks = [5, 5, 6, 5, 6, 7, 7, 8].into_iter();
        let mut seen = Vec::new();
        on_a_named_port_from(
            || picks.next().unwrap(),
            |p| {
                seen.push(p);
                (seen.len() == 4).then_some(())
            },
        );
        assert_eq!(seen, [5, 6, 7, 8]);
    }

    /// A pick that only ever repeats is reported, not looped on.
    #[test]
    #[should_panic(expected = "returned only already-tried ports")]
    fn a_pick_stuck_on_one_number_is_reached_loudly() {
        on_a_named_port_from(|| 5, |_| -> Option<()> { None });
    }

    /// A subject that never wins is a failure, not an infinite loop — and the
    /// panic names what was tried.
    #[test]
    #[should_panic(expected = "was taken before the subject could bind it")]
    fn a_cap_is_reached_loudly() {
        on_a_named_port(|_| -> Option<()> { None });
    }
}
