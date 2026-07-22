//! `PvDatabase::get_pv_blocking` across caller contexts.
//!
//! It USED to be a sync bridge over an async `get_pv`, and whether blocking was
//! sound depended on which thread the caller sat on — a plain thread and a
//! multi-threaded worker were fine, a current-thread runtime was not blockable
//! at all and had to be refused with an error.
//!
//! Since the H6 restructure (`doc/rtems-priority-locks-design.md`) `get_pv` is
//! a `fn`: the read path is cache-only lock work, exactly as C `dbGetField`
//! is a plain call from any thread. There is no bridge left, so there is no
//! context-dependent soundness question either — every context below must
//! simply return the value. The three cases are kept as BOUNDARIES: if a future
//! change re-introduces a blocking bridge, the current-thread case fails again.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;

const WATCHDOG: Duration = Duration::from_secs(10);

/// Never let a regression hang CI: run on a scratch thread with a deadline and
/// re-raise a panic as a panic.
fn with_watchdog<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(format!("watchdog-{what}"))
        .spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)));
        })
        .expect("spawn");
    match rx.recv_timeout(WATCHDOG) {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("{what} hung (no result within {WATCHDOG:?})"),
    }
}

async fn db_with_pv() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_pv("BLOCKING:PV", EpicsValue::Long(7)).await.unwrap();
    db
}

/// The context the API exists for: a plain thread with no runtime entered.
///
/// On unfixed main this returns
/// `Err(InvalidValue("no runtime for get_pv_blocking"))` — the predicate is
/// inverted, refusing the one caller it can safely serve.
#[test]
fn get_pv_blocking_from_a_plain_thread_succeeds() {
    let db = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(db_with_pv());

    with_watchdog("plain-thread", move || {
        // No runtime is entered on this thread.
        assert!(tokio::runtime::Handle::try_current().is_err());
        let value = db
            .get_pv_blocking("BLOCKING:PV")
            .expect("a plain thread is exactly the context this API is for");
        assert_eq!(value, EpicsValue::Long(7));
    });
}

/// A multi-threaded worker stays supported: `block_in_place` yields this worker
/// before parking it, so the tasks that hold the database locks keep running.
#[test]
fn get_pv_blocking_from_multi_thread_runtime_succeeds() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let db = rt.block_on(db_with_pv());

    rt.block_on(async move {
        let value = tokio::task::spawn_blocking(move || db.get_pv_blocking("BLOCKING:PV"))
            .await
            .unwrap()
            .expect("multi-thread runtime must keep working");
        assert_eq!(value, EpicsValue::Long(7));
    });
}

/// The case that used to be unserviceable: a current-thread runtime, whose one
/// thread cannot be parked without halting whoever holds the lock. It is now
/// served like any other, because nothing is parked — the read is lock work.
///
/// This is the regression boundary for the H6 property: re-introducing a
/// blocking bridge under `get_pv_blocking` makes this case error or hang again.
#[test]
fn get_pv_blocking_from_current_thread_runtime_succeeds() {
    with_watchdog("current-thread", || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = db_with_pv().await;
            let value = db
                .get_pv_blocking("BLOCKING:PV")
                .expect("a cache-only read needs no runtime of any shape");
            assert_eq!(value, EpicsValue::Long(7));
        });
    });
}
