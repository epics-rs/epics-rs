//! The verdict a spawned IOC's console carries about the port it was given —
//! the retry trigger for a test that names that port, and nothing else.

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
    /// for, so the candidate is spent and the caller retries.
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
    if console.contains(PORT_WAS_TAKEN) {
        SoftIocVerdict::PortTaken
    } else if console.contains(IOC_IS_UP) {
        SoftIocVerdict::Up
    } else {
        SoftIocVerdict::Silent
    }
}

/// An epics-rs IOC's report that `port` belonged to somebody else — the retry
/// trigger for a test that names the port of `softioc-rs` or
/// `realtime-ca-ioc`.
///
/// Both bind after `iocRun`, so the report follows it and a test judges the
/// console only once its attempt is over. softioc-rs prints C's
/// [`PORT_WAS_TAKEN`] for the TCP listener and serves on a dynamic one as C
/// does, or names the address its search responder could not bind and exits
/// 1; realtime-ca-ioc names the port on either bind arm and exits 1. The
/// errno text is required on the lines that carry one: realtime-ca-ioc's TCP
/// arm also covers a `local_addr` failure, which is no steal.
pub fn rs_ioc_port_was_taken(console: &str, port: u16) -> bool {
    let udp = format!(":{port}: Address already in use");
    let rt_tcp = format!("realtime-ca-ioc: cannot start the CA TCP server on port {port}: ");
    let rt_udp =
        format!("realtime-ca-ioc: cannot start the CA UDP search responder on port {port}: ");
    console.lines().any(|l| {
        l.contains(PORT_WAS_TAKEN)
            || (l.contains("CA server UDP search responder bind ") && l.contains(&udp))
            || ((l.starts_with(&rt_tcp) || l.starts_with(&rt_udp))
                && l.contains("Address already in use"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// realtime-ca-ioc's stderr, verbatim, when `local_addr` fails after a good
    /// bind: measured on Linux 6.14 / glibc 2.39 (thread-backend build,
    /// port 32961) with `getsockname` forced to EINVAL on the listening socket,
    /// which is RTEMS's failure there. The RTEMS console itself is unmeasured.
    /// The port was bound, so it is no steal, and a retry on it would hide the
    /// failure as a slow pass.
    const LOCAL_ADDR_FAILED: &str = "\
Starting iocInit
iocRun: All initialization complete
realtime-ca-ioc: cannot start the CA TCP server on port 32961: bind() succeeded; \
local_addr() on the listener failed: Invalid argument (os error 22)
";

    #[test]
    fn a_local_addr_failure_is_not_a_stolen_port() {
        assert!(!rs_ioc_port_was_taken(LOCAL_ADDR_FAILED, 32961));
        assert!(rs_ioc_port_was_taken(
            "realtime-ca-ioc: cannot start the CA TCP server on port 32961: \
             Address already in use (os error 98)\n",
            32961
        ));
    }

    /// A report names the port it is about: softioc-rs's measured UDP-held
    /// line and realtime-ca-ioc's UDP arm are no steal of another number,
    /// including one the named port's digits start with.
    #[test]
    fn a_report_about_another_port_is_not_a_stolen_port() {
        const UDP_HELD: &str = "iocRun: All initialization complete\n\
            \x1b[31;1mERROR\x1b[0m: I/O error: CA server UDP search responder bind \
            127.0.0.1:32961: Address already in use (os error 98)\n";
        const RT_UDP: &str = "realtime-ca-ioc: cannot start the CA UDP search responder on port \
            32961: Address already in use (os error 98)\n";
        for console in [UDP_HELD, RT_UDP] {
            assert!(rs_ioc_port_was_taken(console, 32961), "{console:?}");
        }
        assert!(!rs_ioc_port_was_taken(UDP_HELD, 3296));
        assert!(!rs_ioc_port_was_taken(RT_UDP, 32962));
        assert!(!rs_ioc_port_was_taken(
            "Starting iocInit\niocRun: All initialization complete\n",
            32961
        ));
    }
}
