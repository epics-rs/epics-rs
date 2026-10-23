//! The verdict a spawned IOC's console carries about the port it was given —
//! the retry trigger for a test that names that port, and nothing else.

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::OnceLock;

/// C's report that the IOC is up: `iocRun` prints it after `iocBuild` has
/// bound the server sockets and `rsrv_run` has started their threads
/// (iocInit.c:272-273 at R7.0.10). Waiting for it replaces an 800 ms sleep,
/// which was a bet on how much CPU the box would give `softIoc` while the
/// rest of the suite ran.
pub const IOC_IS_UP: &str = "iocRun: All initialization complete";

/// C's report that the port it was told to use belonged to somebody else.
///
/// RSRV compares the number `rsrv_grab_tcp` came back with against the one it
/// asked for and says so when they differ (caservertask.c:583-591 at
/// R7.0.10). This is the ONLY safe retry trigger for a C IOC: `softIoc` never
/// tells a test what it bound, so "the client could not connect" would make a
/// broken IOC and a stolen port indistinguishable, and retrying on that would
/// turn a real regression into a slow, confusing pass-shaped failure. Note
/// that C does not exit here — with the UDP port taken as well it reaches
/// `cantProceed` in `rsrv_init` and *suspends*, so "the child is gone" is not
/// the discriminator either.
pub const PORT_WAS_TAKEN: &str = "cas WARNING: Configured TCP port was unavailable.";

/// C's report when the UDP port alone was taken. `tryBind` returns on
/// EADDRINUSE without a word (caservertask.c:131-145), so this `cantProceed`
/// line (:751-752) is all the console says. Any other bind error prints a
/// `CAS: ... bind` line and suspends `_main_` there, so it never precedes
/// this one.
pub const NO_TCP_SERVER: &str = "CAS: No TCP server started";

/// What a spawned `softIoc`'s stderr says about the port it was given.
///
/// A separate function from the spawner so the verdict can be pinned
/// against C's real console text: the alternative is a test that waits
/// `fact_budget::FACT_BUDGET` for a subject built to never speak.
#[derive(Debug, PartialEq, Eq)]
pub enum SoftIocVerdict {
    /// [`IOC_IS_UP`] — serving on the number it was given.
    Up,
    /// [`PORT_WAS_TAKEN`] — RSRV bound a different TCP port than the one asked
    /// for — or [`NO_TCP_SERVER`] — so the candidate is spent and the
    /// caller retries.
    PortTaken,
    /// Neither line. **Not** a retry: a different port fixes none of the ways
    /// an IOC can be broken, and retrying here would turn a regression into
    /// [`crate::ATTEMPTS`] slow attempts and a misleading panic.
    Silent,
}

/// [`PORT_WAS_TAKEN`] outranks [`IOC_IS_UP`], because C prints BOTH: RSRV
/// warns about the fallback and then carries on to `iocRun` on a dynamically
/// assigned TCP port (`caservertask.c:582-590` @R7.0.10). An IOC serving on
/// a number the test did not name is not the subject the test asked for.
pub fn softioc_verdict(console: &str) -> SoftIocVerdict {
    if console.contains(PORT_WAS_TAKEN) || console.contains(NO_TCP_SERVER) {
        SoftIocVerdict::PortTaken
    } else if console.contains(IOC_IS_UP) {
        SoftIocVerdict::Up
    } else {
        SoftIocVerdict::Silent
    }
}

/// What an epics-rs binary prints for EADDRINUSE: std's `io::Error` text,
/// `{strerror} (os error {errno})`. Taken from a real collision, because
/// both halves are the platform's — Linux says `... (os error 98)`, macOS
/// `(os error 48)`, Windows another sentence and 10048 — and a verdict that
/// spells one of them reads every other platform's steal as a regression.
pub fn addr_in_use() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| {
        let held = UdpSocket::bind("127.0.0.1:0").expect("bind an ephemeral UDP port");
        let e = UdpSocket::bind(held.local_addr().expect("read the ephemeral port"))
            .expect_err("a second bind of a held UDP port");
        assert_eq!(e.kind(), ErrorKind::AddrInUse, "{e}");
        e.to_string()
    })
}

/// Every text an epics-rs binary prints for a bind `epics_libcom_rs::net::port_held`
/// reads as held: [`addr_in_use`], and on Windows std's text for WSAEACCES,
/// which an `SO_REUSEADDR` bind over a held address gets.
pub fn held_texts() -> &'static [String] {
    static TEXTS: OnceLock<Vec<String>> = OnceLock::new();
    TEXTS.get_or_init(|| {
        let in_use = addr_in_use().to_owned();
        #[cfg(windows)]
        let texts = vec![
            in_use,
            std::io::Error::from_raw_os_error(windows_sys::Win32::Networking::WinSock::WSAEACCES)
                .to_string(),
        ];
        #[cfg(not(windows))]
        let texts = vec![in_use];
        texts
    })
}

/// An epics-rs IOC's report that `port` belonged to somebody else — the retry
/// trigger for a test that names the port of `softioc-rs` or
/// `realtime-ca-ioc`.
///
/// A test judges the console once its attempt is over, including one that
/// ended before `iocRun`: softioc-rs binds in iocInit as C `rsrv_init` does.
/// It prints C's [`PORT_WAS_TAKEN`] for the TCP listener and serves on a
/// dynamic one as C does, or names the address its search responder could
/// not bind and exits. realtime-ca-ioc takes the same TCP fallback and keeps
/// running, so its console must be judged while it runs too; it names the
/// port and exits 1 when a bind it cannot fall back from fails. On the
/// lines that carry an error the cause must be one of [`held_texts`] and
/// nothing else: realtime-ca-ioc's TCP arm also covers a `local_addr`
/// failure, which is no steal.
pub fn rs_ioc_port_was_taken(console: &str, port: u16) -> bool {
    let rt_tcp = format!("realtime-ca-ioc: cannot start the CA TCP server on port {port}: ");
    let rt_udp =
        format!("realtime-ca-ioc: cannot start the CA UDP search responder on port {port}: ");
    console.lines().any(|l| {
        l.contains(PORT_WAS_TAKEN)
            || (l.contains("CA server UDP search responder bind ")
                && held_texts()
                    .iter()
                    .any(|t| l.ends_with(&format!(":{port}: {t}"))))
            || [&rt_tcp, &rt_udp].iter().any(|arm| {
                l.strip_prefix(arm.as_str())
                    .is_some_and(|cause| held_texts().iter().any(|t| t == cause))
            })
    })
}

/// `qsrv-rs`'s report that the port it was given belonged to somebody else.
///
/// A taken TCP port is not fatal there: the server warns
/// `Server unable to bind port {tcp}, falling back to …` (runtime.rs:929)
/// and serves on another, which a client dialling `tcp` never finds. A taken
/// UDP search port ends the process with `qsrv-rs: …` and the bind's own
/// error, which names no number; it counts only when that error is one of
/// [`held_texts`] and `udp_held`, the kernel's answer for `tcp + 1` after
/// the child has exited. Both follow `iocRun`, so a test judges the console
/// after its attempt.
pub fn qsrv_rs_port_was_taken(console: &str, tcp: u16, udp_held: bool) -> bool {
    let fell_back = format!("Server unable to bind port {tcp},");
    console.lines().any(|l| {
        l.contains(&fell_back)
            || (udp_held
                && l.starts_with("qsrv-rs: ")
                && held_texts().iter().any(|t| l.ends_with(t.as_str())))
    })
}

/// realtime-ca-ioc's stderr, verbatim, when `local_addr` fails after a good
/// bind: measured on Linux 6.14 / glibc 2.39 (thread-backend build, port
/// [`LOCAL_ADDR_FAILED_PORT`]) under [`crate::getsockname_einval_shim`],
/// which is RTEMS's failure there. The RTEMS console itself is unmeasured.
/// The port was bound, so it is no steal, and a retry on it would hide the
/// failure as a slow pass.
pub const LOCAL_ADDR_FAILED: &str = "\
Starting iocInit
iocRun: All initialization complete
realtime-ca-ioc: cannot start the CA TCP server on port 32961: bind() succeeded; \
local_addr() on the listener failed: Invalid argument (os error 22)
";

/// The port [`LOCAL_ADDR_FAILED`] was measured on.
pub const LOCAL_ADDR_FAILED_PORT: u16 = 32961;

#[cfg(test)]
mod tests {
    use super::*;

    /// qsrv-rs's stderr, verbatim, with `tcp + 1` held on UDP by one socket
    /// the measuring process owned: Linux 6.14 / glibc 2.39 at 3afdb55dc.
    #[cfg(target_os = "linux")]
    const QSRV_UDP_HELD: &str = "Starting iocInit\n\
        iocRun: All initialization complete\n\
        qsrv-rs: invalid value: invalid value: I/O error: Address already in use (os error 98)\n";

    #[test]
    fn qsrv_rs_names_a_stolen_port_only_by_its_own_two_lines() {
        let fell_back = " WARN epics_pva_rs::server_native::runtime: Server unable to bind \
                         port 38017, falling back to 127.0.0.1:43637\n";
        assert!(qsrv_rs_port_was_taken(fell_back, 38017, false));
        assert!(!qsrv_rs_port_was_taken(fell_back, 3801, false));
        let udp_line = format!("qsrv-rs: invalid value: I/O error: {}\n", addr_in_use());
        assert!(qsrv_rs_port_was_taken(&udp_line, 1, true));
        assert!(!qsrv_rs_port_was_taken(&udp_line, 1, false));
        assert!(!qsrv_rs_port_was_taken(
            "Starting iocInit\niocRun: All initialization complete\n",
            38017,
            true
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn qsrv_rs_measured_udp_held_line_is_a_stolen_port() {
        assert!(qsrv_rs_port_was_taken(QSRV_UDP_HELD, 42041, true));
    }

    /// A fatal bind with another cause is no steal, even with `tcp + 1` held:
    /// the `AsyncUdpV4` line is EADDRNOTAVAIL, printed when no interface was
    /// selected at all.
    #[test]
    fn qsrv_rs_fatal_bind_of_another_cause_is_not_a_stolen_port() {
        for console in [
            "qsrv-rs: invalid value: invalid value: I/O error: AsyncUdpV4: failed to bind \
             any interface\n",
            "qsrv-rs: invalid value: invalid value: I/O error: Cannot assign requested \
             address (os error 99)\n",
        ] {
            assert!(!qsrv_rs_port_was_taken(console, 1, true), "{console:?}");
        }
    }

    /// The text is the kernel's own: whatever the platform calls EADDRINUSE,
    /// [`addr_in_use`] is what std prints for it.
    #[test]
    fn addr_in_use_is_what_std_prints_for_a_held_port() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let e = std::net::TcpListener::bind(held.local_addr().unwrap()).unwrap_err();
        assert_eq!(e.to_string(), addr_in_use());
    }

    #[test]
    fn a_local_addr_failure_is_not_a_stolen_port() {
        assert!(!rs_ioc_port_was_taken(
            LOCAL_ADDR_FAILED,
            LOCAL_ADDR_FAILED_PORT
        ));
    }

    /// The CA binaries' stderr lines, verbatim, each with the named port held
    /// by one socket the measuring process owned (UDP for the search arms, a
    /// TCP listener for the TCP arm): Linux 6.14 / glibc 2.39 at 3afdb55dc,
    /// realtime-ca-ioc on the thread backend.
    #[cfg(target_os = "linux")]
    const SOFTIOC_RS_UDP_HELD: &str = "iocRun: All initialization complete\n\
        \x1b[31;1mERROR\x1b[0m: I/O error: CA server UDP search responder bind \
        127.0.0.1:32961: Address already in use (os error 98)\n";
    #[cfg(target_os = "linux")]
    const RT_TCP_HELD: &str = "realtime-ca-ioc: cannot start the CA TCP server on port 32961: \
        Address already in use (os error 98)\n";
    #[cfg(target_os = "linux")]
    const RT_UDP_HELD: &str = "realtime-ca-ioc: cannot start the CA UDP search responder on port \
        32961: Address already in use (os error 98)\n";

    /// A report names the port it is about: each measured line is a steal of
    /// its own number and of no other, including one the named port's digits
    /// start with.
    #[cfg(target_os = "linux")]
    #[test]
    fn each_ca_binarys_measured_line_names_its_own_stolen_port() {
        for console in [SOFTIOC_RS_UDP_HELD, RT_TCP_HELD, RT_UDP_HELD] {
            assert!(rs_ioc_port_was_taken(console, 32961), "{console:?}");
            assert!(!rs_ioc_port_was_taken(console, 3296), "{console:?}");
            assert!(!rs_ioc_port_was_taken(console, 32962), "{console:?}");
        }
        assert!(!rs_ioc_port_was_taken(
            "Starting iocInit\niocRun: All initialization complete\n",
            32961
        ));
    }

    /// Every text the platform reads as held is a steal on each arm, and of
    /// its own number only — on Windows that includes WSAEACCES.
    #[test]
    fn every_held_text_is_a_stolen_port_on_each_arm() {
        for t in held_texts() {
            for console in [
                format!("CA server UDP search responder bind 127.0.0.1:32961: {t}\n"),
                format!("realtime-ca-ioc: cannot start the CA TCP server on port 32961: {t}\n"),
                format!(
                    "realtime-ca-ioc: cannot start the CA UDP search responder on port 32961: {t}\n"
                ),
            ] {
                assert!(rs_ioc_port_was_taken(&console, 32961), "{console:?}");
                assert!(!rs_ioc_port_was_taken(&console, 3296), "{console:?}");
            }
            assert!(qsrv_rs_port_was_taken(
                &format!("qsrv-rs: I/O error: {t}\n"),
                1,
                true
            ));
        }
    }

    /// softioc-rs's TCP arm is C's line and serves on a dynamic port.
    #[test]
    fn softioc_rs_tcp_fallback_is_a_stolen_port() {
        let console = format!("iocRun: All initialization complete\n{PORT_WAS_TAKEN}\n");
        assert!(rs_ioc_port_was_taken(&console, 32961));
    }

    /// The arms judge the cause, not the presence of the words: a bind error
    /// of another kind on the same port, or EADDRINUSE quoted mid-line before
    /// another cause, is no steal.
    #[test]
    fn a_ca_bind_failure_of_another_cause_is_not_a_stolen_port() {
        let in_use = addr_in_use();
        for console in [
            "realtime-ca-ioc: cannot start the CA UDP search responder on port 32961: \
             Cannot assign requested address (os error 99)\n"
                .to_string(),
            format!(
                "realtime-ca-ioc: cannot start the CA TCP server on port 32961: \
                 {in_use}; local_addr() on the listener failed: Invalid argument\n"
            ),
            "\x1b[31;1mERROR\x1b[0m: I/O error: CA server UDP search responder bind \
             127.0.0.1:32961: Permission denied (os error 13)\n"
                .to_string(),
        ] {
            assert!(!rs_ioc_port_was_taken(&console, 32961), "{console:?}");
        }
    }
}
