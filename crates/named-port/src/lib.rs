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

use std::net::{TcpListener, UdpSocket};

/// How many candidates a test may burn before the run is called broken.
///
/// A steal is a coincidence of timing; sixteen of them in a row is a box
/// where something is systematically taking the numbers, and reporting that
/// is more useful than trying forever.
pub const ATTEMPTS: usize = 16;

/// Run `attempt` on candidate ports until one of them survives to the bind.
///
/// `attempt` returns `Some(value)` when the subject came up on the number it
/// was given, and `None` **only** on evidence that this particular number was
/// taken by somebody else — see the crate docs. Panics with the whole
/// candidate list when [`ATTEMPTS`] have been lost, because at that point the
/// number is not the problem.
pub fn on_a_named_port<T>(mut attempt: impl FnMut(u16) -> Option<T>) -> T {
    let mut tried = Vec::with_capacity(ATTEMPTS);
    for _ in 0..ATTEMPTS {
        let port = free_for_tcp_and_udp();
        tried.push(port);
        if let Some(won) = attempt(port) {
            return won;
        }
    }
    panic!(
        "every one of {ATTEMPTS} candidate ports was taken before the subject \
         could bind it: {tried:?}. That is no longer a lost race — something \
         on this host is taking the numbers, or the subject is refusing every \
         port for a reason its retry evidence is misreporting as a steal."
    );
}

/// A port number that was free for both TCP and UDP a moment ago.
///
/// Both, because a CA server binds its TCP listener and its UDP search
/// socket on the same number, so probing one leaves the other free to
/// collide. A hint, never a reservation — both probe sockets are closed
/// before this returns, and they must be, or the subject could not bind.
pub fn free_for_tcp_and_udp() -> u16 {
    for _ in 0..64 {
        let tcp = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral TCP port");
        let port = tcp.local_addr().expect("read the ephemeral port").port();
        if UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no port was free for both TCP and UDP after 64 attempts");
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A subject that never wins is a failure, not an infinite loop — and the
    /// panic names what was tried.
    #[test]
    #[should_panic(expected = "was taken before the subject could bind it")]
    fn a_cap_is_reached_loudly() {
        on_a_named_port(|_| -> Option<()> { None });
    }
}
