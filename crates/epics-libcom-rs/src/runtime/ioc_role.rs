//! Which role an IOC thread takes on, and therefore which scheduling band it
//! sits at — the one place that choice is made.
//!
//! # Why this exists
//!
//! [`task::enter_ioc_thread`](super::task::enter_ioc_thread) is the single
//! owner of *applying* a band: every IOC thread passes through it, and a
//! source guard in `task.rs` fails the build if one does not. Nothing owned
//! *choosing* the band. The value was an ordinary argument at each thread's
//! creation point, so the serving paths grew named, C-cited, guarded
//! constants (`CAS_TCP_PRIORITY`, `PVA_SERVER_PRIORITY`, `periodic_priority`)
//! while the diagnostic paths — the ones nobody writes a parity note about —
//! took `ThreadPriority::Low` because `Low` is what a thread with no stated
//! opinion gets. C has the same default in the same place
//! (`EPICS_THREAD_OPTS_INIT` is `{epicsThreadPriorityLow, …}`,
//! `epicsThread.h:173`), and it is a default, not a decision.
//!
//! It was measured as a defect, not deduced. On the VxWorks bring-up rig,
//! ramping CA connections to the admission wall froze `UPTIME` for the whole
//! ramp and resumed it two seconds after the load stopped: the values were
//! correct and the publisher never ran, so `CA_REFUSED_CNT` read `0.0` at the
//! exact moment an operator needs it. The bring-up console reporter, which
//! shares no lock with the publisher but shares its band, emitted 4 ticks in
//! 444 s instead of ~44. Client service at `CaServerLow-4..CaServerLow`
//! (16..20) starves anything at 10.
//!
//! # The rule
//!
//! **A thread whose output is how the IOC is observed must be schedulable
//! while the IOC is at its wall — if its per-tick work is bounded.** The
//! operator surface sits strictly above `epicsThreadPriorityCAServerHigh`
//! (40), the ceiling of the band C gives `rsrv` (`caservertask.c:563-575`
//! builds its ladder down from `CAServerLow`), and at or below
//! `epicsThreadPriorityScanHigh` (70), so it can never delay record
//! processing more urgent than the slowest periodic scan.
//!
//! C reaches the same ordering by a different route, which is why the rule is
//! stated against C's constants rather than invented here: its status numbers
//! are *records*, processed by the periodic scan threads at
//! `epicsThreadPriorityScanLow + ind` (`dbScan.c:945`) — 60 and up, i.e. 40
//! levels above the CA server that would otherwise starve them. Ours was
//! inverted. The operator-facing report path C does have as a thread, `iocsh`,
//! sits higher still at 91 (`epicsThread.h:86`), so this table is conservative
//! relative to C in both directions.
//!
//! The bounded-work half of the rule is measured, not assumed, and it is why
//! this table has two answers rather than one. On the same rig, the same ramp
//! and the same guest, with the status pusher raised to `ScanLow`: 136 clients
//! admitted before the pool's capacity refusal, identical to the pre-fix run,
//! and `UPTIME` advancing 1 s/s throughout. With the bring-up console census
//! raised alongside it: 87, twice, the 88th client's handshake exceeding a 15 s
//! timeout. Its per-tick work is roughly a hundred lines onto a byte-serial
//! console, and above the serving threads that work *is* the load — an
//! instrument that changes what it measures. `epicsThreadPriorityIocsh` (91)
//! is not a counter-example: C's unbounded reports are typed by a human, once,
//! not emitted on a timer forever.
//!
//! # What is here and what is not
//!
//! Roles are added here, in view of the ordering test below, not at a thread's
//! creation point — that is the whole mechanism. The serving bands are *not*
//! here: they stay with their servers (`epics_ca_rs::server::blocking`,
//! `epics_pva_rs::server_native::blocking`, `server::scan`) because each is
//! derived from the C line that sets it and each has its own guard pinning it
//! to that derivation. This module is the owner for the roles that had no such
//! home, and it states the ordering against them using C's own constant rather
//! than by reaching across crates for theirs.

use std::io;
use std::thread::JoinHandle;

use super::task::{
    PriorityApplied, StackSizeClass, ThreadPriority, enter_ioc_thread, spawn_dedicated_thread,
};

/// A role an IOC thread takes on, as a name rather than a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IocRole {
    /// The periodic pusher behind the status PVs — `UPTIME`, `MEM_FREE`,
    /// `CA_REFUSED_CNT` and the rest of the devIocStats set
    /// (`server::status_pv`).
    ///
    /// This is the role the rule was written for. Its readings are the only
    /// instrument on a target with no shell, and the readings an operator
    /// most needs are exactly the ones produced while the IOC is loaded.
    StatusPublisher,

    /// The bring-up console reporter on the embedded entry points — the
    /// 10-second line groups and the descriptor/task census behind them.
    ///
    /// Deliberately **below** client service, which is the other half of the
    /// rule rather than an exception to it. This is an instrument, not an
    /// operator surface: it exists only under the `bringup-probes` feature,
    /// no shipped image contains it, and its per-tick work is a census whose
    /// length is the descriptor table's. Raised to the operator surface's band
    /// it cost 36% of the connection ceiling in two identical runs (see the
    /// module docs), which is the instrument rewriting its own measurement.
    /// The operator's diagnostic path on a shell-less target is
    /// [`IocRole::StatusPublisher`], and that one publishes at the wall.
    ConsoleCensus,
}

impl IocRole {
    /// Every role, so the ordering rule can be asserted over all of them
    /// rather than over whichever ones a test remembered.
    pub const ALL: [IocRole; 2] = [IocRole::StatusPublisher, IocRole::ConsoleCensus];

    /// The band this role sits at.
    ///
    /// `ScanLow` (60) is `epicsThreadPriorityScanLow` exactly — the band of
    /// C's *slowest* periodic scan thread, `spawnPeriodic`'s `ind == 0`
    /// (`dbScan.c:945`). It is the lowest band that still outranks every CA
    /// and PVA server thread in this workspace, all of which sit at or below
    /// `CaServerLow` (20). Choosing the bottom of the scan ladder rather than
    /// a point inside it means the operator surface preempts nothing that
    /// scans.
    ///
    /// `Low` (10) is `epicsThreadPriorityLow`, below the whole CA ladder,
    /// which starts at `CaServerLow - 4` (16).
    pub const fn band(self) -> ThreadPriority {
        match self {
            IocRole::StatusPublisher => ThreadPriority::ScanLow,
            IocRole::ConsoleCensus => ThreadPriority::Low,
        }
    }
}

/// The thread prologue for a role: name this thread and take the role's band.
///
/// For a thread whose creation the caller owns for another reason — the
/// embedded entry points build theirs with `thread::Builder` directly so the
/// pool's byte account can be charged inside the closure. Everything else
/// [`spawn_ioc_role`] does is done there.
pub fn enter_ioc_role(role: IocRole) -> PriorityApplied {
    enter_ioc_thread(role.band())
}

/// Start a thread in `role`: the band comes from the role, so the creation
/// point never names one.
///
/// Delegates to [`spawn_dedicated_thread`], which charges the thread to the
/// worker pool's reservation account and runs the prologue.
pub fn spawn_ioc_role<F>(
    role: IocRole,
    name: &str,
    stack: StackSizeClass,
    f: F,
) -> io::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    spawn_dedicated_thread(name.to_string(), role.band(), stack, f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor of the CA server ladder: `CAS_UDP_PRIORITY` in
    /// `epics_ca_rs::server::blocking` is `CaServerLow - 4`, and no serving
    /// thread in this workspace sits lower. Restated rather than imported
    /// because that crate is above this one.
    const CLIENT_SERVICE_FLOOR: u8 = ThreadPriority::CaServerLow.value() - 4;

    /// Every role is placed on one side of client service, deliberately.
    ///
    /// The `match` is exhaustive on purpose: a new role cannot be added to the
    /// table without this test failing to compile, which is the point of
    /// having the table at all. Adding a variant is a decision about whether
    /// its output is what an operator reads or an instrument that must not
    /// disturb what it measures — see the module docs for the measurement
    /// that made those two different answers.
    #[test]
    fn every_role_is_placed_on_a_stated_side_of_client_service() {
        for role in IocRole::ALL {
            let band = role.band().value();
            match role {
                IocRole::StatusPublisher => {
                    assert!(
                        band > ThreadPriority::CaServerHigh.value(),
                        "{role:?} at {band} does not outrank the CA server band \
                         ceiling ({}) — it can be starved by the client load \
                         whose effect it exists to report",
                        ThreadPriority::CaServerHigh.value(),
                    );
                    assert!(
                        band <= ThreadPriority::ScanHigh.value(),
                        "{role:?} at {band} outranks record processing ({}); an \
                         operator surface must not delay a scan",
                        ThreadPriority::ScanHigh.value(),
                    );
                }
                IocRole::ConsoleCensus => assert!(
                    band < CLIENT_SERVICE_FLOOR,
                    "{role:?} at {band} is not below the CA server ladder's \
                     floor ({CLIENT_SERVICE_FLOOR}) — an unbounded periodic \
                     report above client service becomes the load",
                ),
            }
        }
    }

    /// The bands are C's, by value and not by resemblance.
    #[test]
    fn the_bands_are_the_epics_thread_priority_constants() {
        // dbScan.c:945 spawns `scan-%g` at ScanLow + ind, so ind == 0 — the
        // slowest periodic list — is this same number.
        assert_eq!(IocRole::StatusPublisher.band().value(), 60);
        assert_eq!(ThreadPriority::ScanLow.value(), 60);
        assert_eq!(IocRole::ConsoleCensus.band().value(), 10);
        assert_eq!(ThreadPriority::Low.value(), 10);
    }

    /// The table's ordering must survive both embedded targets, whose maps are
    /// not the same function.
    ///
    /// Both land on POSIX `56 + epics` — for RTEMS that is the tail of a
    /// core-priority inversion through `RTEMS_MAXIMUM_PRIORITY`, for VxWorks it
    /// is measured directly — so both are strictly increasing in the EPICS
    /// value and the EPICS-space ordering survives unchanged. VxWorks then
    /// inverts that POSIX value into its own task space as `vx = 199 - epics`,
    /// where lower is more urgent, so the operator surface ends up at a
    /// *smaller* native number than client service: the same ordering a third
    /// time, through a third arithmetic.
    #[test]
    fn the_ordering_survives_both_embedded_priority_maps() {
        use super::super::task::{map_epics_priority_rtems, map_epics_priority_vxworks};

        let service = ThreadPriority::CaServerHigh.value();
        for role in IocRole::ALL {
            let band = role.band().value();
            // Which side of client service this role is on is asserted above;
            // here the only claim is that whichever side it is, both targets
            // agree with the EPICS-space answer.
            let above = band > service;

            for (target, service_os, role_os) in [
                (
                    "RTEMS posix",
                    map_epics_priority_rtems(service),
                    map_epics_priority_rtems(band),
                ),
                (
                    "VxWorks posix",
                    map_epics_priority_vxworks(service),
                    map_epics_priority_vxworks(band),
                ),
                // VxWorks's own POSIX layer inverts into native task
                // priorities, measured as `vx = 199 - epics`; smaller is more
                // urgent there, so negate to compare in the same direction.
                (
                    "VxWorks native",
                    -(199 - service as i32),
                    -(199 - band as i32),
                ),
            ] {
                assert_eq!(
                    role_os > service_os,
                    above,
                    "{role:?}: {target} puts it at {role_os} against client \
                     service at {service_os}, which is the opposite order to \
                     EPICS {band} against {service}",
                );
            }
        }
    }
}
