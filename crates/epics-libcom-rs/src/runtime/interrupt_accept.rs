//! `interruptAccept` — the process-global gate that says record processing and
//! I/O Intr callbacks are live.
//!
//! # Why this exists
//!
//! C's asyn param library defers every `callParamCallbacks` until the IOC has
//! finished wiring records to their driver. `paramList::callCallbacks` opens
//! with `if (!interruptAccept) return asynSuccess;` and returns **before**
//! `flags.clear()` (`asynPortDriver.cpp:838,871`), so every callback fired
//! while the IOC is still building is a no-op that *preserves* the accumulated
//! changed-flags. A per-port thread then does the callbacks exactly once, the
//! moment `interruptAccept` goes true (`callbackThread::run`,
//! `asynPortDriver.cpp:923-937`), delivering every seeded read-only value
//! (`Manufacturer`, `MaxSizeX`, …) to the `_RBV` records that only just
//! registered their interrupts.
//!
//! Without this gate the port's `call_param_callbacks` clears the changed-flags
//! whenever it happens to run first — a driver's own acquisition/array task can
//! fire it before `iocInit` wires the records — and a read-only parameter set
//! once at construction is then lost for the life of the process: its `_RBV`
//! record sits at the `.db` default forever. This is the owner of the flag that
//! `asyn-rs`'s `PortDriverBase::call_param_callbacks` consults and that the
//! scan facility drives.
//!
//! # The default, and its single owner
//!
//! C initialises the flag `FALSE` (`dbAccess.c:67`); so does this port. The gate
//! is thus closed at process start by construction, and the scan facility is its
//! single owner thereafter: `scan_run` sets it true (C `scanRun`,
//! `dbScan.c:218`), `scan_pause`/`scan_stop` set it false (`dbScan.c:241,165`).
//! No other code writes it, so the seeds a port sets at construction are
//! guaranteed to survive to the `scan_run` boot flush without depending on any
//! bring-up path having lowered a gate first.
//!
//! C's non-IOC translation unit compiles `static int interruptAccept = 1`
//! instead (`asynPortDriver.cpp:23-25`) so a bare `asynPortDriver` used outside
//! an IOC still delivers. This port does not take that convenience default: it
//! serves IOCs, where the gate is opened by `scan_run` at `iocRun`. A unit test
//! that exercises `call_param_callbacks` without an `iocInit` must therefore
//! open the gate itself with [`set_interrupts_accepted`], the precondition that
//! always holds by the time records process in a running IOC.
//!
//! # The one-shot flush
//!
//! [`set_interrupts_accepted`] invokes the callbacks registered through
//! [`on_interrupts_accepted`] on each `false → true` edge — never when the flag
//! is written `true` while already `true`, so a pause→resume does not re-flush.
//! `asyn-rs` registers one such callback that sweeps every port's changed
//! params once; it is this crate's stand-in for C's per-port `callbackThread`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// C `volatile int interruptAccept`. Defaults `false` (`dbAccess.c:67`); the
/// scan facility is its single owner thereafter — see the module docs.
static ACCEPTED: AtomicBool = AtomicBool::new(false);

/// Callbacks fired on the `false → true` edge — the crate-level analogue of the
/// per-port `callbackThread` C creates in every `asynPortDriver` constructor.
/// `Arc` so the list can be snapshotted and the lock dropped before any
/// callback runs (a callback that registers another must not deadlock), the
/// same discipline as `init_hook_announce`.
type OnAccept = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;
static ON_ACCEPT: Mutex<Vec<OnAccept>> = Mutex::new(Vec::new());

/// Whether record processing / I/O Intr callbacks are live — C `interruptAccept`.
///
/// The single reader is `asyn-rs`'s `PortDriverBase::call_param_callbacks`,
/// which returns without consuming its changed-flags while this is `false`.
pub fn interrupts_accepted() -> bool {
    ACCEPTED.load(Ordering::Acquire)
}

/// Set the gate, and on a `false → true` edge run every [`on_interrupts_accepted`]
/// callback once.
///
/// C reaches the true edge inside `scanRun` (`dbScan.c:218`) and the false edge
/// inside `scanPause`/`scanStop` (`dbScan.c:241,165`); the IOC's scan facility
/// is the single owner that calls this. The edge guard means a redundant
/// `true`-while-`true` (a pause→resume that never lowered the flag, or two scan
/// starts) fires nothing, matching C's once-per-process `callbackThread`.
pub fn set_interrupts_accepted(accepted: bool) {
    let was = ACCEPTED.swap(accepted, Ordering::AcqRel);
    if accepted && !was {
        let snapshot: Vec<OnAccept> = ON_ACCEPT.lock().unwrap().clone();
        for cb in snapshot {
            cb();
        }
    }
}

/// Register a callback for the next `false → true` edge of the gate.
///
/// Registration does **not** fire the callback, even if the flag is already
/// `true`: the caller (`asyn-rs`'s boot-flush arm) registers this once, and a
/// port that appears after the flag is already up is that caller's concern, not
/// this edge's. Process-global, matching C's single per-port thread list.
pub fn on_interrupts_accepted<F: Fn() + Send + Sync + 'static>(cb: F) {
    ON_ACCEPT.lock().unwrap().push(std::sync::Arc::new(cb));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    // These tests mutate process-global state; nextest runs each in its own
    // process, so they do not race. Under `cargo test` they would share the
    // flag, which is why each restores it.

    #[test]
    fn edge_fires_the_callback_exactly_once_per_false_to_true() {
        set_interrupts_accepted(false);
        static HITS: AtomicUsize = AtomicUsize::new(0);
        on_interrupts_accepted(|| {
            HITS.fetch_add(1, Ordering::SeqCst);
        });

        // false -> true: one fire.
        set_interrupts_accepted(true);
        assert_eq!(HITS.load(Ordering::SeqCst), 1);
        assert!(interrupts_accepted());

        // true -> true: no fire (C's callbackThread is once-per-process).
        set_interrupts_accepted(true);
        assert_eq!(HITS.load(Ordering::SeqCst), 1);

        // false -> true again: fires again (a resume delivers what changed).
        set_interrupts_accepted(false);
        set_interrupts_accepted(true);
        assert_eq!(HITS.load(Ordering::SeqCst), 2);

        set_interrupts_accepted(true);
    }

    #[test]
    fn registration_alone_does_not_fire() {
        set_interrupts_accepted(true);
        static HITS: AtomicUsize = AtomicUsize::new(0);
        on_interrupts_accepted(|| {
            HITS.fetch_add(1, Ordering::SeqCst);
        });
        // Already true, no edge — must not have fired.
        assert_eq!(HITS.load(Ordering::SeqCst), 0);
    }
}
