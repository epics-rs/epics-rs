//! `PvDatabase::get_pv_blocking` across caller contexts.
//!
//! It is a sync bridge over the async `get_pv`, and whether blocking is sound
//! depends entirely on which thread the caller is on:
//!
//! - **No tokio runtime** (a plain `std::thread`) — sound. `get_pv` awaits only
//!   `tokio::sync` primitives; whoever holds them is a task on some *other*
//!   runtime's threads and keeps running while we park. This is the one context
//!   the name "blocking" is actually for, and it is the one the old code
//!   *rejected* outright ("no runtime for get_pv_blocking").
//! - **Multi-threaded runtime** — sound via `block_in_place`, which hands this
//!   worker's other tasks to a sibling before we park it.
//! - **Current-thread runtime** — NOT soundly blockable. Parking the thread
//!   halts every task on that runtime, including whichever one holds the
//!   database lock we are about to await. The old code called `block_in_place`,
//!   which panics there.

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

/// A current-thread runtime cannot be blocked soundly: parking its only thread
/// stops every task on it, including whichever holds the lock we would await.
/// This must be an ERROR naming the async alternative — not a panic (today), and
/// not a silent deadlock.
///
/// On unfixed main this panics:
/// `can call blocking only when running on the multi-threaded runtime`.
#[test]
fn get_pv_blocking_from_current_thread_runtime_errors() {
    with_watchdog("current-thread", || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let db = db_with_pv().await;
            let err = db
                .get_pv_blocking("BLOCKING:PV")
                .expect_err("a current-thread runtime cannot be blocked soundly");
            let msg = err.to_string();
            assert!(
                msg.contains("get_pv"),
                "the error must name the async alternative, got: {msg}"
            );
        });
    });
}
