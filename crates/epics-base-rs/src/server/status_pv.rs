// RTEMS-EXEC-MODEL-ALLOW(1): block_on_sync's current-thread-runtime refusal needs an ambient tokio runtime; runs and passes in the feature-ON suite.
//! Status PVs — the few numbers an operator can read off a target that has no
//! shell.
//!
//! On RTEMS there is no iocsh, no `dbpr` and no `procServ` console to type
//! into — the serial console is write-only — so the only way to ask the IOC how
//! it is doing is to `caget` it. This module registers a handful of plain PVs
//! and keeps them current, so the answer exists.
//!
//! # The names are devIocStats', not ours
//!
//! Every value here that has an upstream counterpart uses the upstream name,
//! from `iocStats/iocAdmin/Db/ioc.template` and `iocRTOS.template`
//! (<https://github.com/epics-modules/iocStats>): `FD_CNT`, `FD_MAX`,
//! `FD_FREE`, `MEM_FREE`, `MEM_USED`, `MEM_MAX`, `MEM_BLK`, `UPTIME`. Upstream
//! writes them under a `$(IOCNAME)` macro with a colon separator, which is the
//! role [`target_status_pvs`]'s `prefix` argument plays.
//!
//! This is a deliberate refusal to invent a parallel vocabulary. Operators
//! already have OPI screens, archiver configurations and alarm handlers keyed
//! to these names; a status surface that answered to `MEM:FREE` instead of
//! `MEM_FREE` would be a gratuitous deviation on the one surface where matching
//! upstream costs nothing and diverging costs every site its tooling.
//!
//! `UPTIME` is a **string**, because upstream's is a `stringin` carrying
//! `[N day(s), ]HH:MM:SS` (`devIocStats/devIocStatsString.c:397-421`), not a
//! seconds count. [`format_uptime`] reproduces that formatting exactly.
//!
//! Two values have no upstream counterpart and are named in the same shape
//! rather than a different one — see the IOC binaries that publish them:
//! `CA_REFUSED_CNT` (devIocStats counts CA clients and connections but never
//! refusals) and `PVA_CONN_CNT` (devIocStats predates PVA).
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
//! It is also the rate the heap walk is safe at. devIocStats' RTEMS memory OSD
//! carries its author's warning that "gathering heap statistics could be
//! expensive; I wouldn't want to run this too often w/o knowing how it is
//! implemented" (`osdMemUsage.c:23-27`). That is an argument about polling
//! rate, and one second is the answer to it.
//!
//! # No record layer
//!
//! [`PvDatabase::add_pv`] registers a `PvEntry::Simple` — no `.db` file, no
//! record type, no device support, no scan task. A status PV is a number and a
//! name; anything more would be machinery to maintain for no reader.
//!
//! Two consequences of having no record graph, both improvements:
//!
//! * `FD_FREE` is upstream a `calc` record whose `CALC` is `B>0?B-A:C` with
//!   `C = 1000` — a sentinel standing in for "no descriptor support", because a
//!   `calc` has no way to say "unknown". Here it is
//!   [`FdUsage::free`](epics_rtems_boot::stats::FdUsage::free) when there is a
//!   reading and `NaN` when there is not, so "unknown" and "1000 free" are
//!   different answers.
//! * `MEM_MAX` is upstream a separate device-support value that is nonetheless
//!   computed as free + used (`osdMemUsage.c:73`); here it is
//!   [`MemUsage::total`](epics_rtems_boot::stats::MemUsage::total), arithmetic
//!   on the two numbers the kernel actually reported.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::CaResult;
use crate::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use crate::runtime::task::{StackSizeClass, ThreadPriority, block_on_sync, spawn_dedicated_thread};
use crate::server::database::PvDatabase;
use crate::server::pv::ProcessVariable;
use crate::types::{EpicsValue, PvString};

/// How often every status PV is republished. See the module docs — one second
/// is a floor set by the target's libc, not a tuning choice.
pub const PUSH_INTERVAL: Duration = Duration::from_secs(1);

/// One status value: the PV name, and how to read it.
///
/// The reader is a closure returning an [`EpicsValue`], so this module needs to
/// know nothing about what it is reporting — a CA server's connection count, a
/// heap reading and an uptime *string* are all the same shape from here, and
/// none of them makes `epics-base-rs` depend on the crate it came from.
///
/// Returning the value rather than an `f64` is what lets `UPTIME` keep
/// upstream's `stringin` type. The PV's native type is taken from the first
/// sample at registration (see [`serve_status_pvs`]), so a reader that returns
/// a string produces a string PV by construction — there is no way to register
/// a double PV whose pusher then publishes strings.
pub struct StatusPv {
    name: String,
    read: Box<dyn Fn() -> EpicsValue + Send>,
}

impl StatusPv {
    /// `name` is the PV name as a client will `caget` it — fully qualified,
    /// including whatever prefix the IOC uses.
    pub fn new(name: impl Into<String>, read: impl Fn() -> EpicsValue + Send + 'static) -> Self {
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

/// A reading, or `NaN` when there is none.
///
/// `NaN` rather than a sentinel: zero free descriptors and zero free heap are
/// both real readings on this target, and they are the ones an operator most
/// needs to believe. See the module docs on `FD_FREE`.
fn reading(value: Option<f64>) -> EpicsValue {
    EpicsValue::Double(value.unwrap_or(f64::NAN))
}

/// Elapsed time in devIocStats' `UPTIME` format: `[N day(s), ]HH:MM:SS`.
///
/// Reproduces `devIocStats/devIocStatsString.c:397-421` including its two
/// singular/plural forms and the omission of the day part in the first 24
/// hours. Sites parse this string, so the format is a compatibility surface
/// rather than a presentation choice.
pub fn format_uptime(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    let secs = total % 60;
    let mins = (total / 60) % 60;
    let hours = (total / 3600) % 24;
    match total / 86_400 {
        0 => format!("{hours:02}:{mins:02}:{secs:02}"),
        1 => format!("1 day, {hours:02}:{mins:02}:{secs:02}"),
        days => format!("{days} days, {hours:02}:{mins:02}:{secs:02}"),
    }
}

/// The status values every RTEMS IOC publishes, whatever protocol it serves.
///
/// Descriptors, heap and uptime are properties of the *image*, not of the CA or
/// PVA front-end, so both IOC binaries publish exactly this set and add only
/// their own protocol counters. Written once here rather than twice in the two
/// entry points: two copies of a naming rule is how they come to disagree, and
/// a client cannot tell a renamed PV from a dead one.
///
/// `prefix` plays `$(IOCNAME)`'s role in the upstream templates; the separator
/// is the colon upstream uses.
///
/// `started` is the instant the IOC came up, for `UPTIME`.
///
/// On a build with no RTEMS boot shim the descriptor and heap PVs are still
/// registered and publish `NaN` — the honest answer, and the one that keeps the
/// name set identical between a host smoke test and the target.
pub fn target_status_pvs(prefix: &str, started: Instant) -> Vec<StatusPv> {
    use epics_rtems_boot::stats::{fd_usage, mem_usage};

    vec![
        // The ceiling the box hits first. Measured: 142 concurrent CA
        // connections served, the 143rd refused by the libbsd socket zone,
        // which is sized from the descriptor cap this pair reports against.
        StatusPv::new(format!("{prefix}:FD_CNT"), || {
            reading(fd_usage().map(|fd| fd.used as f64))
        }),
        StatusPv::new(format!("{prefix}:FD_MAX"), || {
            reading(fd_usage().map(|fd| fd.max as f64))
        }),
        StatusPv::new(format!("{prefix}:FD_FREE"), || {
            reading(fd_usage().map(|fd| fd.free() as f64))
        }),
        // Each field is read on its own: a target can measure one and not
        // another (VxWorks' mimalloc reports committed bytes but has no
        // free-run metric at all), and the three PVs say so independently
        // rather than all going `NaN` because one of them cannot be had.
        StatusPv::new(format!("{prefix}:MEM_FREE"), || {
            reading(mem_usage().free.map(|v| v as f64))
        }),
        StatusPv::new(format!("{prefix}:MEM_USED"), || {
            reading(mem_usage().used.map(|v| v as f64))
        }),
        StatusPv::new(format!("{prefix}:MEM_MAX"), || {
            reading(mem_usage().total().map(|v| v as f64))
        }),
        // The fragmentation signal, and the reason it is worth a PV of its
        // own: an allocation fails on the largest free block, not on the free
        // total, so this is the number that falls before a connection stops
        // being able to start its thread.
        StatusPv::new(format!("{prefix}:MEM_BLK"), || {
            reading(mem_usage().largest_free.map(|v| v as f64))
        }),
        // The one value none of the others gives: a reset. Without it a client
        // cannot tell a board reboot from a network blip, because both look
        // like a reconnect.
        StatusPv::new(format!("{prefix}:UPTIME"), move || {
            EpicsValue::String(PvString::from_bytes(
                format_uptime(started.elapsed()).into_bytes(),
            ))
        }),
    ]
}

/// A running status-PV pusher.
///
/// # Dropping it does not stop it, deliberately
///
/// There is no `Drop`, and that is the point. The one caller that matters is an
/// entry point whose process never exits, and tying the facility's life to a
/// binding's scope there would mean a status PV freezes at its last value
/// because someone wrote `let _ =` — leaving a set of PVs that answer `caget`
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
type Published = (Box<dyn Fn() -> EpicsValue + Send>, Arc<ProcessVariable>);

/// Register every PV in `pvs` on `db` and start publishing them.
///
/// Each PV is registered with its **first sample**, not with a placeholder, so
/// the PV's native type is the type its reader produces — that is what makes
/// `UPTIME` a string PV without this function knowing which value is which.
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
        let initial = (pv.read)();
        let handle = block_on_sync(register(&db, &pv.name, initial))
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
async fn register(
    db: &PvDatabase,
    name: &str,
    initial: EpicsValue,
) -> CaResult<Arc<ProcessVariable>> {
    db.add_pv(name, initial).await?;
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
    let samples: Vec<(EpicsValue, &Arc<ProcessVariable>)> =
        published.iter().map(|(read, pv)| (read(), pv)).collect();
    block_on_sync(async {
        for (value, pv) in samples {
            pv.set(value);
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

    fn double(v: f64) -> impl Fn() -> EpicsValue + Send + 'static {
        move || EpicsValue::Double(v)
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
                EpicsValue::Double(reader.load(Ordering::Relaxed) as f64)
            })],
        )
        .expect("registration");
        pvs.stop();

        let pv = block_on_sync(db.find_pv("T:COUNT"))
            .expect("plain thread")
            .expect("the PV was registered");
        const DBE_VALUE: u16 = 1;
        let mut rx = pv
            .add_subscriber(1, DbFieldType::Double, DBE_VALUE)
            .expect("subscriber added");

        source.store(42, Ordering::Relaxed);
        assert!(push_once(&[(
            Box::new(move || EpicsValue::Double(source.load(Ordering::Relaxed) as f64)),
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
            "and the stored value must be the pushed sample, not the registered one"
        );
    }

    /// Registration failures belong to the caller, not to a detached thread.
    #[test]
    fn a_name_clash_is_reported_to_the_caller() {
        let db = database();
        let first = serve_status_pvs(db.clone(), vec![StatusPv::new("T:DUP", double(1.0))])
            .expect("the first registration succeeds");
        first.stop();

        let second = serve_status_pvs(db, vec![StatusPv::new("T:DUP", double(2.0))]);
        assert!(
            second.is_err(),
            "a status PV that collides with an existing name must fail the call, \
             not fail silently on the pusher thread"
        );
    }

    /// A PV takes its type from its reader's first sample, which is what lets
    /// `UPTIME` be a string PV — upstream's `stringin` — while its neighbours
    /// stay doubles, without this module knowing which is which.
    #[test]
    fn a_pv_takes_its_type_from_its_readers_first_sample() {
        let db = database();
        let handle = serve_status_pvs(
            db.clone(),
            vec![
                StatusPv::new("T:NUM", double(5.0)),
                StatusPv::new("T:STR", || {
                    EpicsValue::String(PvString::from_bytes(b"00:00:07".to_vec()))
                }),
            ],
        )
        .expect("registration");
        handle.stop();

        let num = block_on_sync(db.find_pv("T:NUM"))
            .expect("plain thread")
            .expect("registered");
        let text = block_on_sync(db.find_pv("T:STR"))
            .expect("plain thread")
            .expect("registered");
        assert!(matches!(num.get(), EpicsValue::Double(_)));
        assert!(
            matches!(text.get(), EpicsValue::String(_)),
            "a string reader must produce a string PV; a double PV here would \
             make UPTIME a number and break every site's stringin client"
        );
        assert_eq!(format!("{}", text.get()), "00:00:07");
    }

    /// Every value in one tick is read before any of them is published, so
    /// one tick reports one instant rather than a spread over however long the
    /// publishes take.
    ///
    /// Encoded by having the second value's reader look at what the *first*
    /// PV currently holds: read-all-then-publish-all means it still holds its
    /// registered sample at that moment, and an interleaved loop means it
    /// already holds this tick's.
    #[test]
    fn every_value_is_read_before_any_is_published() {
        let db = database();
        let handle = serve_status_pvs(
            db.clone(),
            vec![
                StatusPv::new("T:FIRST", double(1.0)),
                StatusPv::new("T:SECOND", double(2.0)),
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
                EpicsValue::Double(2.0)
            }
        };
        let published: Vec<Published> = vec![
            (Box::new(double(9.0)), first.clone()),
            (Box::new(observer), second),
        ];
        assert!(push_once(&published));

        assert_eq!(
            *seen.lock().expect("observation"),
            "1",
            "the second value was read AFTER the first was published — the tick \
             spans two instants instead of one"
        );
        assert_eq!(
            format!("{}", first.get()),
            "9",
            "precondition: this tick's sample really was published by it"
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
                EpicsValue::Double(counter.fetch_add(1, Ordering::Relaxed) as f64)
            })],
        )
        .expect("registration");
        drop(handle);

        let deadline = std::time::Instant::now() + PUSH_INTERVAL * 4;
        while ticks.load(Ordering::Relaxed) < 3 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ticks.load(Ordering::Relaxed) >= 3,
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
    ///
    /// `#[tokio::test]`, not `#[epics_test]`: the point of this test is the
    /// ambient current-thread tokio runtime — the exact context
    /// `block_on_sync` refuses. The census marker accounts for it.
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
            !push_once(&[(Box::new(double(1.0)), pv)]),
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

    /// The uptime string is a compatibility surface: sites parse it. Every
    /// boundary devIocStats' own formatter has is pinned here — the day part
    /// appearing at all, its singular form, and the two-digit padding.
    #[test]
    fn the_uptime_string_matches_dev_ioc_stats_exactly() {
        assert_eq!(format_uptime(Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_uptime(Duration::from_secs(7)), "00:00:07");
        assert_eq!(format_uptime(Duration::from_secs(3_661)), "01:01:01");
        assert_eq!(
            format_uptime(Duration::from_secs(86_399)),
            "23:59:59",
            "the last second before the day part appears"
        );
        assert_eq!(
            format_uptime(Duration::from_secs(86_400)),
            "1 day, 00:00:00",
            "singular, with a comma and a space — devIocStatsString.c:413-414"
        );
        assert_eq!(
            format_uptime(Duration::from_secs(172_800)),
            "2 days, 00:00:00"
        );
        assert_eq!(
            format_uptime(Duration::from_secs(90_061)),
            "1 day, 01:01:01"
        );
    }

    /// The names are devIocStats', spelled exactly, under one colon. A rename
    /// here silently breaks every OPI screen and archiver configuration keyed
    /// to the upstream surface, and nothing in the IOC would report it.
    #[test]
    fn the_common_set_uses_the_upstream_names() {
        let names: Vec<String> = target_status_pvs("IOC", Instant::now())
            .iter()
            .map(|pv| pv.name().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "IOC:FD_CNT",
                "IOC:FD_MAX",
                "IOC:FD_FREE",
                "IOC:MEM_FREE",
                "IOC:MEM_USED",
                "IOC:MEM_MAX",
                "IOC:MEM_BLK",
                "IOC:UPTIME",
            ]
        );
    }

    /// On a host build there is no boot shim, so the descriptor and heap
    /// readings do not exist. They must publish `NaN` rather than `0`: a
    /// `FD_FREE` of zero is what an IOC out of descriptors reports, and it is
    /// the alarm an operator acts on.
    #[test]
    fn an_absent_reading_publishes_nan_rather_than_zero() {
        let pvs = target_status_pvs("IOC", Instant::now());
        for pv in &pvs {
            if pv.name().ends_with(":UPTIME") {
                continue;
            }
            match (pv.read)() {
                EpicsValue::Double(v) => assert!(
                    v.is_nan(),
                    "{} published {v} with no reading behind it; zero free \
                     descriptors and zero free heap are real readings on the \
                     target and must not be confused with an absent one",
                    pv.name()
                ),
                other => panic!("{} is not a double: {other:?}", pv.name()),
            }
        }
    }

    /// Uptime is a string on every build, shim or no shim — it is measured in
    /// Rust and has no reading to be missing.
    #[test]
    fn uptime_is_a_string_on_every_build() {
        let pvs = target_status_pvs("IOC", Instant::now());
        let uptime = pvs
            .iter()
            .find(|pv| pv.name() == "IOC:UPTIME")
            .expect("UPTIME is in the common set");
        assert!(matches!((uptime.read)(), EpicsValue::String(_)));
    }
}
