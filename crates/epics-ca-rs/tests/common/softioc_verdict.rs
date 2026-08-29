//! The verdict a spawned C `softIoc`'s console carries about the port it was
//! given — the retry trigger for `common::spawn_softioc`, and nothing else.
//!
//! Its own file rather than a section of `common/mod.rs` so the test that
//! pins it against C's real console text can include this and NOT the
//! spawner: `tools/ioc-spawn-gate` derives "this binary spawns an IOC" by
//! following the `mod` graph from the test root, and a pure string predicate
//! that arrives carrying `Command::new("softIoc")` would have to be declared
//! in a `.config/nextest.toml` test-group it has no business being in.

/// C's report that the IOC is up: `iocRun` prints it after `iocBuild` has
/// bound the server sockets and `rsrv_run` has started their threads
/// (iocInit.c:272-273 at R7.0.10). Waiting for it replaces an 800 ms sleep,
/// which was a bet on how much CPU the box would give `softIoc` while the
/// rest of the suite ran.
pub(crate) const IOC_IS_UP: &str = "iocRun: All initialization complete";

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
pub(crate) const PORT_WAS_TAKEN: &str = "cas WARNING: Configured TCP port was unavailable.";

/// What a spawned `softIoc`'s stderr says about the port it was given.
///
/// A separate function from [`softioc_on`] so the verdict can be pinned
/// against C's real console text: the alternative is a test that waits
/// [`budget::FACT_BUDGET`] for a subject built to never speak.
#[derive(Debug, PartialEq, Eq)]
pub enum SoftIocVerdict {
    /// [`IOC_IS_UP`] — serving on the number it was given.
    Up,
    /// [`PORT_WAS_TAKEN`] — RSRV bound a different TCP port than the one asked
    /// for, so the candidate is spent and the caller retries.
    PortTaken,
    /// Neither line. **Not** a retry: a different port fixes none of the ways
    /// an IOC can be broken, and retrying here would turn a regression into
    /// [`named_port::ATTEMPTS`] slow attempts and a misleading panic.
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
