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

use epics_libcom_rs::net::port_unavailable;
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
/// Unix: `SO_REUSEADDR` on TCP, as std's `TcpListener` sets it, so a port
/// left in TIME_WAIT reads free. Windows: `SO_EXCLUSIVEADDRUSE`, because a
/// plain Winsock wildcard bind succeeds beside a socket holding the number
/// on one address, and is refused only when the holder set the same option.
fn port_is_held(ty: Type, port: u16) -> bool {
    let sock = Socket::new(Domain::IPV4, ty, None).expect("open a probe socket");
    #[cfg(unix)]
    if ty == Type::STREAM {
        sock.set_reuse_address(true)
            .expect("SO_REUSEADDR on the probe");
    }
    #[cfg(windows)]
    exclusive_address_use(&sock);
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    matches!(sock.bind(&addr.into()), Err(e) if port_unavailable(&e))
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
