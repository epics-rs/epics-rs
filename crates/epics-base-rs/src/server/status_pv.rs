//! Status PVs — the few numbers an operator can read off a target that has no
//! shell.
//!
//! On RTEMS there is no iocsh, no `dbpr` and no `procServ` console to type
//! into — the serial console is write-only — so the only way to ask the IOC how
//! it is doing is to `caget` it. This module registers a handful of plain PVs
//! and keeps them current, so the answer exists.
//!
//! # A pusher, not a read hook
//!
//! The obvious shape — a [`ReadHook`](crate::server::pv::ReadHook) computing
//! the value when a GET arrives — is the wrong one, and not for a performance
//! reason. **The read hook is honoured on the one-shot GET only.** `read_snapshot`
//! consults it; monitor fan-out and the *initial* monitor event serve the stored
//! snapshot instead, which `snapshot_ignores_read_hook` in `server::pv` pins as
//! deliberate. A `camonitor` on a hook-backed PV would therefore show one value
//! at connect and never update again — and a client watching from elsewhere on
//! the network is exactly the posture these PVs exist for.
//!
//! So a thread pushes: [`ProcessVariable::set`] stores the value *and* notifies
//! subscribers, so `caget` and `camonitor` agree by construction and there is no
//! GET/monitor split to explain.
//!
//! # One second, and not less
//!
//! [`PUSH_INTERVAL`] is one second and is a constant rather than a parameter,
//! because both bounds on it are properties of the target rather than of the
//! caller:
//!
//! * a sub-second `thread::sleep` is the never-wakes case on the target's stock
//!   libc (the `timespec` layout defect `epics_rtems_boot`'s layout guard
//!   refuses to boot without), so anything finer is not merely imprecise;
//! * the target's clock is quantised to whole seconds anyway, so nothing finer
//!   is measurable.
//!
//! A caller cannot pass a good value here that this file does not already know,
//! and can pass a fatal one.
//!
//! # No record layer
//!
//! [`PvDatabase::add_pv`] registers a `PvEntry::Simple` — no `.db` file, no
//! record type, no device support, no scan task. A status PV is a number and a
//! name; anything more would be machinery to maintain for no reader.
//!
//! # What is deliberately absent: free heap
//!
//! Free heap is the one value that shows the ceiling *approaching* rather than
//! after the fact, and it is still not here. C computes it with
//! `malloc_get_statistics` (`os/RTEMS-score/osdPoolStatus.c`), for which there
//! is no binding anywhere in `epics-rtems-boot` — it is the only status value
//! that would need new FFI. Measured on the bring-up box at the real ceiling:
//! 142 concurrent CA connections, connection 143 refused by the libbsd socket
//! zone with `ENFILE`, and free heap at that moment had room for roughly 9.7
//! more. Descriptors and memory bite within ten connections of each other, so a
//! connection count is very nearly as predictive of the ceiling as free heap is
//! — which buys the warning without the shim. Add `MEM:FREE` when something
//! else needs `malloc_get_statistics`, not for this.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::error::CaResult;
use crate::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use crate::runtime::task::{StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread};
use crate::server::database::PvDatabase;
use crate::server::pv::ProcessVariable;
use crate::types::EpicsValue;

/// How often every status PV is republished. See the module docs — one second
/// is a floor set by the target's libc, not a tuning choice.
pub const PUSH_INTERVAL: Duration = Duration::from_secs(1);

/// One status value: the PV name, and how to read it.
///
/// The reader is a closure so this module needs to know nothing about what it
/// is reporting — the CA server's connection count, an uptime clock and a PVA
/// server's are all the same shape from here, and none of them makes
/// `epics-base-rs` depend on the crate it came from.
pub struct StatusPv {
    name: String,
    read: Box<dyn Fn() -> f64 + Send>,
}

impl StatusPv {
    /// `name` is the PV name as a client will `caget` it — fully qualified,
    /// including whatever prefix the IOC uses.
    pub fn new(name: impl Into<String>, read: impl Fn() -> f64 + Send + 'static) -> Self {
        Self {
            name: name.into(),
            read: Box::new(read),
        }
    }

    /// The PV name this value is published under.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A running status-PV pusher. Dropping it retires the thread.
///
/// A running status-PV pusher.
///
/// # Dropping it does not stop it, deliberately
///
/// There is no `Drop`, and that is the point. The one caller that matters is an
/// entry point whose process never exits, and tying the facility's life to a
/// binding's scope there would mean a status PV freezes at its last value
/// because someone wrote `let _ =` — leaving three PVs that answer `caget`
/// with a stale number. A frozen status PV is worse than an absent one: it
/// reads exactly like a healthy IOC that happens to have no clients. So the
/// pusher retires only when [`stop`](Self::stop) says so, and the handle is
/// safe to drop.
///
/// The thread is not joined either: it retires within one [`PUSH_INTERVAL`] of
/// being asked, and joining would make every caller wait out a tick. The same
/// choice, for the same reason, that `ConnRegistry::stop` makes in the PVA
/// driver.
pub struct StatusPvs {
    shutdown: Arc<AtomicBool>,
    names: Vec<String>,
}

impl StatusPvs {
    /// Ask the pusher to retire. Idempotent; returns immediately.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// The PV names this pusher publishes, in the order they were given.
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

/// One value paired with the PV it publishes to.
type Published = (Box<dyn Fn() -> f64 + Send>, Arc<ProcessVariable>);

/// Register every PV in `pvs` on `db` and start publishing them.
///
/// Registration happens **on the calling thread**, before the pusher exists, so
/// a name clash with an already-loaded record comes back as an error the caller
/// can print and act on rather than dying on a detached thread nobody is
/// reading. That also means the caller must be somewhere `block_on_sync` works:
/// a plain `std::thread` (the RTEMS entry point) or a multi-thread runtime
/// worker, not a current-thread runtime.
pub fn serve_status_pvs(db: Arc<PvDatabase>, pvs: Vec<StatusPv>) -> CaResult<StatusPvs> {
    let mut published: Vec<Published> = Vec::with_capacity(pvs.len());
    let mut names = Vec::with_capacity(pvs.len());
    for pv in pvs {
        let handle = block_on_sync(register(&db, &pv.name))
            .map_err(|e| crate::error::CaError::Protocol(format!("status PVs: {e}")))??;
        names.push(pv.name);
        published.push((pv.read, handle));
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let worker_shutdown = shutdown.clone();
    // Low: this is the least urgent thread in the IOC by construction — it
    // reports, it does not serve — so it must never be the reason a scan or a
    // CA reply waits. Small: its whole frame is a `Vec` walk and one `set` per
    // PV.
    spawn_dedicated_thread(
        "status-pv".to_string(),
        ThreadPriority::Low,
        StackSizeClass::Small,
        move || push_loop(&published, &worker_shutdown),
    )?;

    Ok(StatusPvs { shutdown, names })
}

/// Add one PV and hand back the live handle to push to.
async fn register(db: &PvDatabase, name: &str) -> CaResult<Arc<ProcessVariable>> {
    db.add_pv(name, EpicsValue::Double(0.0)).await?;
    db.find_pv(name).await.ok_or_else(|| {
        crate::error::CaError::ChannelNotFound(format!(
            "status PV {name} vanished between add_pv and find_pv"
        ))
    })
}

/// The pusher thread's whole body.
fn push_loop(published: &[Published], shutdown: &AtomicBool) {
    while !shutdown.load(Ordering::Acquire) {
        if !push_once(published) {
            // Nothing published and nothing will: the thread this runs on
            // cannot drive an async `set` at all, so every later tick would
            // fail identically. Spinning silently is the failure mode this
            // whole module exists to remove, so say it once and retire.
            errlog_sev_printf(
                ErrlogSevEnum::Major,
                "status PVs: this thread cannot drive a value publish (current-thread \
                 runtime); the status PVs are registered but will never update",
            );
            return;
        }
        thread::sleep(PUSH_INTERVAL);
    }
}

/// Publish one sample of every value. Returns `false` only when the calling
/// thread cannot drive the publish at all.
///
/// Every value is *read* before any is published, so one tick reports one
/// instant rather than a spread over however long the publishes take.
fn push_once(published: &[Published]) -> bool {
    let samples: Vec<(f64, &Arc<ProcessVariable>)> =
        published.iter().map(|(read, pv)| (read(), pv)).collect();
    block_on_sync(async {
        for (value, pv) in samples {
            pv.set(EpicsValue::Double(value)).await;
        }
    })
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DbFieldType;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    fn database() -> Arc<PvDatabase> {
        Arc::new(PvDatabase::new())
    }

    /// The property the whole design turns on: a pushed value reaches a
    /// *monitor*, not only a GET. A read hook would satisfy the GET half and
    /// fail this one.
    #[test]
    fn a_pushed_value_reaches_a_subscriber() {
        let db = database();
        let source = Arc::new(AtomicU64::new(7));
        let reader = source.clone();
        let pvs = serve_status_pvs(
            db.clone(),
            vec![StatusPv::new("T:COUNT", move || {
                reader.load(Ordering::Relaxed) as f64
            })],
        )
        .expect("registration");
        pvs.stop();

        let pv = block_on_sync(db.find_pv("T:COUNT"))
            .expect("plain thread")
            .expect("the PV was registered");
        const DBE_VALUE: u16 = 1;
        let mut rx = block_on_sync(pv.add_subscriber(1, DbFieldType::Double, DBE_VALUE))
            .expect("plain thread")
            .expect("subscriber added");

        source.store(42, Ordering::Relaxed);
        assert!(push_once(&[(
            Box::new(move || source.load(Ordering::Relaxed) as f64),
            pv.clone(),
        )]));

        assert!(
            rx.try_recv().is_ok(),
            "a pushed value must post to subscribers — this is the property a \
             read hook would not have"
        );
        assert_eq!(
            format!("{}", pv.get()),
            "42",
            "and the stored value must be the pushed sample, not the initial 0"
        );
    }

    /// Registration failures belong to the caller, not to a detached thread.
    #[test]
    fn a_name_clash_is_reported_to_the_caller() {
        let db = database();
        let first = serve_status_pvs(db.clone(), vec![StatusPv::new("T:DUP", || 1.0)])
            .expect("the first registration succeeds");
        first.stop();

        let second = serve_status_pvs(db, vec![StatusPv::new("T:DUP", || 2.0)]);
        assert!(
            second.is_err(),
            "a status PV that collides with an existing name must fail the call, \
             not fail silently on the pusher thread"
        );
    }

    /// Every value in one tick is read before any of them is published, so
    /// one tick reports one instant rather than a spread over however long the
    /// publishes take.
    ///
    /// Encoded by having the second value's reader look at what the *first*
    /// PV currently holds: read-all-then-publish-all means it still holds its
    /// registered 0 at that moment, and an interleaved loop means it already
    /// holds this tick's sample.
    #[test]
    fn every_value_is_read_before_any_is_published() {
        let db = database();
        let handle = serve_status_pvs(
            db.clone(),
            vec![
                StatusPv::new("T:FIRST", || 1.0),
                StatusPv::new("T:SECOND", || 2.0),
            ],
        )
        .expect("registration");
        handle.stop();
        assert_eq!(handle.names(), &["T:FIRST", "T:SECOND"]);

        let first = block_on_sync(db.find_pv("T:FIRST"))
            .expect("plain thread")
            .expect("registered");
        let second = block_on_sync(db.find_pv("T:SECOND"))
            .expect("plain thread")
            .expect("registered");

        let seen = Arc::new(Mutex::new(String::new()));
        let observer = {
            let seen = seen.clone();
            let first = first.clone();
            move || {
                *seen.lock().expect("observation") = format!("{}", first.get());
                2.0
            }
        };
        let published: Vec<Published> = vec![
            (Box::new(|| 1.0), first.clone()),
            (Box::new(observer), second),
        ];
        assert!(push_once(&published));

        assert_eq!(
            *seen.lock().expect("observation"),
            "0",
            "the second value was read AFTER the first was published — the tick \
             spans two instants instead of one"
        );
        assert_eq!(
            format!("{}", first.get()),
            "1",
            "precondition: the first value really was published by this tick"
        );
    }

    /// Dropping the handle must NOT stop the pusher.
    ///
    /// A status PV frozen at a stale value reads exactly like a healthy IOC
    /// with nothing happening, so tying the facility's life to a binding's
    /// scope is the silent-degradation shape this module exists to remove.
    /// Waits for a *second* tick, which can only have happened after the drop.
    #[test]
    fn dropping_the_handle_does_not_stop_the_pusher() {
        let db = database();
        let ticks = Arc::new(AtomicU64::new(0));
        let counter = ticks.clone();
        let handle = serve_status_pvs(
            db,
            vec![StatusPv::new("T:TICKS", move || {
                counter.fetch_add(1, Ordering::Relaxed) as f64
            })],
        )
        .expect("registration");
        drop(handle);

        let deadline = std::time::Instant::now() + PUSH_INTERVAL * 4;
        while ticks.load(Ordering::Relaxed) < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ticks.load(Ordering::Relaxed) >= 2,
            "the pusher stopped when its handle was dropped; the status PVs are \
             now frozen at whatever they last held"
        );
    }

    /// The interval is a floor set by the target, so it must not become a
    /// parameter and must not drift below a second.
    #[test]
    fn the_push_interval_is_never_sub_second() {
        assert!(
            PUSH_INTERVAL >= Duration::from_secs(1),
            "a sub-second thread::sleep is the never-wakes case on the target's \
             stock libc, and the target clock is quantised to whole seconds"
        );
    }

    /// A pusher that cannot publish says so and stops, rather than spinning
    /// through ticks that all fail the same way.
    #[tokio::test]
    async fn a_pusher_that_cannot_publish_retires() {
        // A current-thread runtime is exactly the context `block_on_sync`
        // refuses, so `push_once` reports failure here.
        let db = Arc::new(PvDatabase::new());
        db.add_pv("T:NOPE", EpicsValue::Double(0.0))
            .await
            .expect("add");
        let pv = db.find_pv("T:NOPE").await.expect("registered");
        assert!(
            !push_once(&[(Box::new(|| 1.0), pv)]),
            "on a current-thread runtime the publish cannot be driven"
        );

        let shutdown = AtomicBool::new(false);
        // Must return rather than loop: a `true` shutdown flag is not what ends
        // it here, the publish failure is.
        push_loop(&[], &shutdown);
        assert!(
            !shutdown.load(Ordering::Acquire),
            "the loop retired on its own, without the flag"
        );
    }
}
