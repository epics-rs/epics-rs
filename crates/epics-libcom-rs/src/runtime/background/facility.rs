//! What keeps a facility thread alive, and what is said when one dies.
//!
//! Three single-purpose worker threads carry every deferred and timed
//! operation this runtime offers: the delayed-callback timer
//! ([`super::delayed_timer`]) behind `sleep`, `interval`, scan periods and
//! `callbackRequestDelayed`; the scanOnce worker ([`super::scan_once`]) behind
//! FLNK and `scanOnce`; and the callback bands
//! ([`super::callback_executor`]). Each owns a `Mutex` + `Condvar` queue and
//! runs callbacks its callers supplied. Two defects follow from that shape,
//! and they were the same defect in all three files:
//!
//! * `.lock().unwrap()` propagates a poisoned mutex. Poison means "a thread
//!   panicked while holding this", not "the data is broken" — these queues are
//!   plain collections whose invariants survive a panic — so propagating turns
//!   one caller's panic into the loss of the whole facility.
//! * A caller-supplied callback runs **on the facility thread**. An unwinding
//!   callback therefore unwinds the loop and the thread ends. Nothing
//!   announces it: submitters keep enqueueing, the IOC keeps answering CA and
//!   PVA, and every timed operation has simply stopped. An operator sees a
//!   healthy IOC whose records no longer process.
//!
//! C reaches neither state: there is no unwinding, and `callbackTask`
//! (`callback.c:210-235`) cannot be ended by the callback it invoked.
//!
//! On a target built with `panic = "abort"` the two `catch_unwind`s here never
//! catch and the process aborts instead. That is a different outcome but not a
//! silent one, which is the property being defended.

use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::runtime::log::{ErrlogSevEnum, errlog_sev_printf};

thread_local! {
    /// Set for the whole life of a facility worker loop — see
    /// [`on_facility_thread`].
    ///
    /// Private to this module on purpose: [`run_facility_loop`] is the only
    /// thing that can set it, and every facility worker thread runs its loop
    /// through that function (`no_facility_propagates_a_poisoned_lock` pins
    /// that for all three), so "this thread is a facility worker" cannot be
    /// asserted by anything that is not one, and cannot be *missed* by
    /// anything that is.
    static ON_FACILITY_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Is the calling thread a background-facility worker — a callback band
/// ([`super::callback_executor`]), the delayed-callback timer
/// ([`super::delayed_timer`]) or the scanOnce worker ([`super::scan_once`])?
///
/// **The invariant this exists for.** Each facility has a bounded worker set
/// (one thread per band by default, C `callbackThreadsDefault`, and exactly one
/// for the timer and for scanOnce), and every unit of work it carries is
/// enqueued *for that worker*. A worker that parks waiting for async progress
/// is therefore waiting for something only it could have run: the deferred
/// callbacks, the FLNK tails, the monitor tails and — on RTEMS, where
/// [`crate::runtime::task::spawn`] routes here — every other spawned future on
/// that band. Parking the delayed timer is worse still: it stops every `sleep`,
/// `interval` and scan period in the process, so a future waiting on a timeout
/// waits forever.
///
/// [`crate::runtime::task::block_on_sync`] is the single gate that consults
/// this, and refuses rather than deadlocking.
pub(crate) fn on_facility_thread() -> bool {
    ON_FACILITY_THREAD.with(Cell::get)
}

/// Marks the calling thread a facility worker until it is dropped.
struct FacilityThreadMark(bool);

impl FacilityThreadMark {
    fn set() -> Self {
        FacilityThreadMark(ON_FACILITY_THREAD.with(|f| f.replace(true)))
    }
}

impl Drop for FacilityThreadMark {
    fn drop(&mut self) {
        ON_FACILITY_THREAD.with(|f| f.set(self.0));
    }
}

/// Latch for the poison announcement below: set on the first recovery.
///
/// Announced once per process, not once per recovery — poisoning is sticky, so
/// every later lock in that facility would repeat it forever on a console that
/// on RTEMS is a serial line.
static POISON_ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// True exactly once per process — for the first poisoned lock recovered.
fn poison_should_be_announced() -> bool {
    !POISON_ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// Take a lock whose data outlives a panic — the only lock accessor these
/// facilities use.
///
/// Every queue behind these mutexes is a `VecDeque`/`BinaryHeap` of pending
/// work plus a `bool`; none has an invariant that a panic elsewhere could have
/// broken. Recovering is therefore not a shortcut around poisoning, it is the
/// correct reading of it.
///
/// Recovering is not silent, or a degraded IOC would read exactly like a
/// healthy one. The panic that *caused* the poisoning is announced where it
/// happened ([`run_isolated`], [`run_facility_loop`]) for the two paths that
/// run on a facility thread; a panic under the lock on a *submitter's* thread
/// — inside `schedule`/`request`, on whatever thread called it — has no such
/// site, and this is what reports it.
pub fn recover<T>(facility: &str, result: std::sync::LockResult<T>) -> T {
    match result {
        Ok(guard) => guard,
        Err(poisoned) => {
            if poison_should_be_announced() {
                errlog_sev_printf(
                    ErrlogSevEnum::Minor,
                    &format!(
                        "{facility}: recovered a lock poisoned by a panic elsewhere; \
                         the queued work is intact and the facility continues. Later \
                         recoveries are not repeated."
                    ),
                );
            }
            poisoned.into_inner()
        }
    }
}

/// Run one caller-supplied callback on a facility thread so that a panic
/// inside it costs that callback and nothing else.
///
/// Returns `false` when the callback unwound.
pub fn run_isolated(facility: &str, cb: impl FnOnce()) -> bool {
    match catch_unwind(AssertUnwindSafe(cb)) {
        Ok(()) => true,
        Err(payload) => {
            errlog_sev_printf(
                ErrlogSevEnum::Major,
                &format!(
                    "{facility}: a callback panicked ({}); the callback was abandoned, \
                     the facility continues",
                    panic_text(&*payload)
                ),
            );
            false
        }
    }
}

/// Run a facility's worker loop, announcing the facility's loss if the loop
/// itself unwinds.
///
/// `on_lost` runs first and marks the facility stopped, so submitters take the
/// same drop-late-requests path they take at shutdown instead of queueing onto
/// a ring no thread will ever drain. Then the loss is stated — at `Fatal`,
/// because from this point the facility is gone for the life of the process
/// while everything around it still looks well.
///
/// This is also where the thread marks itself a facility worker
/// ([`on_facility_thread`]): every worker loop already comes through here, so
/// no facility — present or future — can acquire a worker thread that has not
/// been marked. The mark covers the loop's dynamic extent and is restored on
/// the way out (including on an unwind), so a caller that runs a loop inline on
/// a borrowed thread does not leave that thread marked afterwards.
pub(super) fn run_facility_loop(facility: &str, body: impl FnOnce(), on_lost: impl FnOnce()) {
    let _marked = FacilityThreadMark::set();
    if let Err(payload) = catch_unwind(AssertUnwindSafe(body)) {
        on_lost();
        errlog_sev_printf(
            ErrlogSevEnum::Fatal,
            &format!(
                "{facility}: the worker thread has STOPPED ({}). Every operation this \
                 facility carries has stopped with it; the IOC keeps serving requests.",
                panic_text(&*payload)
            ),
        );
    }
}

/// The panic message, when the payload is one of the two shapes `panic!`
/// produces.
fn panic_text(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "non-string panic payload"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_poisoned_lock_still_yields_its_data() {
        let m = std::sync::Mutex::new(7u32);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _g = m.lock().expect("first lock");
            panic!("poison it");
        }));
        assert!(m.lock().is_err(), "the mutex must actually be poisoned");
        assert_eq!(
            *recover("test-facility", m.lock()),
            7,
            "the data survived the panic"
        );
    }

    #[test]
    fn an_unwinding_callback_is_contained_and_reported() {
        assert!(!run_isolated("test-facility", || panic!(
            "callback blew up"
        )));
        assert!(run_isolated("test-facility", || {}));
    }

    #[test]
    fn a_lost_loop_marks_the_facility_before_it_announces() {
        let marked = AtomicBool::new(false);
        run_facility_loop(
            "test-facility",
            || panic!("loop blew up"),
            || marked.store(true, Ordering::SeqCst),
        );
        assert!(
            marked.load(Ordering::SeqCst),
            "a lost facility must be marked stopped, or submitters queue into a \
             ring with no worker"
        );
    }

    #[test]
    fn a_loop_that_returns_normally_is_not_announced() {
        let marked = AtomicBool::new(false);
        run_facility_loop(
            "test-facility",
            || {},
            || marked.store(true, Ordering::SeqCst),
        );
        assert!(
            !marked.load(Ordering::SeqCst),
            "an orderly shutdown is not a loss"
        );
    }

    /// The treatment is uniform across the three facilities, or it is not a
    /// treatment. A `.lock().unwrap()` reintroduced in any of them is one
    /// caller's panic away from taking that facility down.
    #[test]
    fn no_facility_propagates_a_poisoned_lock() {
        let files = [
            (
                "delayed_timer.rs",
                include_str!("delayed_timer.rs"),
                "delayed-callback timer",
            ),
            (
                "scan_once.rs",
                include_str!("scan_once.rs"),
                "scanOnce worker",
            ),
            (
                "callback_executor.rs",
                include_str!("callback_executor.rs"),
                "callback band",
            ),
        ];

        // Split so this file's own needles do not match when it is read.
        let poison = concat!(".lock()", ".unwrap()");
        let wait_poison = concat!(".wait(", "st).unwrap()");

        let mut offences = Vec::new();
        for (label, src, facility) in files {
            let prod = match src.find("\n#[cfg(test)]") {
                Some(i) => &src[..i],
                None => src,
            };
            for (n, line) in prod.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(poison) || line.contains(wait_poison) {
                    offences.push(format!("{label}:{}", n + 1));
                }
            }
            assert!(
                prod.contains("run_facility_loop("),
                "{label} does not guard its worker loop: its loss would be silent, \
                 and its thread would not be marked a facility worker — so \
                 `block_on_sync` would hand it a blocking bridge that parks the \
                 facility"
            );
            assert!(
                prod.contains("run_isolated("),
                "{label} runs a caller's callback unprotected on its own thread"
            );
            assert!(
                prod.contains(facility),
                "{label} must name itself ({facility:?}) in what it reports"
            );
        }

        assert!(
            offences.is_empty(),
            "these lock sites propagate poisoning instead of recovering: {offences:?}"
        );
    }

    /// A recovered poisoning is stated once and then not again — a degraded
    /// facility must not read as a healthy one, and must not fill a serial
    /// console either.
    #[test]
    #[serial_test::serial]
    fn the_first_poison_recovery_is_the_one_announced() {
        POISON_ANNOUNCED.store(false, Ordering::SeqCst);
        assert!(poison_should_be_announced(), "the first is announced");
        assert!(!poison_should_be_announced(), "the second is not");
        assert!(!poison_should_be_announced(), "nor any later one");
    }

    #[test]
    fn the_panic_message_survives_into_the_report() {
        let payload = catch_unwind(|| panic!("a formatted {}", "message")).expect_err("panics");
        assert_eq!(panic_text(&*payload), "a formatted message");
        let payload = catch_unwind(|| panic!("a literal")).expect_err("panics");
        assert_eq!(panic_text(&*payload), "a literal");
    }
}
