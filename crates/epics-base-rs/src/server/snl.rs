//! The start-up gate of a ported SNL program — the wait C's `seq` does for
//! every program before its first state runs.
//!
//! A C sequencer program runs no state until every PV it declared has
//! connected (`seq_main.c`: the program thread blocks in its start-up wait
//! until `pvConnectCount() == pvAssignCount()`), and a channel to the IOC's
//! own records connects only once `iocRun` has started rsrv — after
//! `initialProcess`, so the program's first read of a `PINI=YES` record sees
//! the processed value and its first write is not undone by the PINI pass.
//! The port's programs reach records directly, so nothing connects and
//! nothing waited: a program started from `st.cmd` ran ahead of the PINI
//! pass, read the unprocessed defaults, and had what it wrote overwritten
//! by PINI. [`spawn_program`] is that wait, at the one point every `seqStart`
//! passes through, so a program cannot be started ahead of PINI at all.

use std::future::Future;

use crate::runtime::task::BlockingBridge;
use crate::server::database::PvDatabase;

/// Start `run` as a task once the database has finished its PINI pass.
///
/// `run` is the program's `run(config, db)` with the config already bound;
/// `program` names it in the one error line a failed program leaves, the way
/// `seq` prints the program name.
pub fn spawn_program<F, Fut, E>(
    bridge: &BlockingBridge,
    db: &PvDatabase,
    program: &'static str,
    run: F,
) where
    F: FnOnce(PvDatabase) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), E>> + Send + 'static,
    E: std::fmt::Display,
{
    let db = db.clone();
    bridge.spawn(async move {
        db.wait_for_pini().await;
        if let Err(e) = run(db).await {
            eprintln!("{program} error: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    /// Poll until `probe` holds, or give up.
    async fn eventually(probe: impl Fn() -> bool) -> bool {
        for _ in 0..300 {
            if probe() {
                return true;
            }
            crate::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    fn started_flag() -> (
        Arc<AtomicBool>,
        impl FnOnce(PvDatabase) -> std::future::Ready<Result<(), String>> + Send + 'static,
    ) {
        let started = Arc::new(AtomicBool::new(false));
        let mark = Arc::clone(&started);
        (started, move |_db: PvDatabase| {
            mark.store(true, Ordering::Release);
            std::future::ready(Ok(()))
        })
    }

    #[epics_macros_rs::epics_test]
    async fn a_program_started_before_pini_runs_only_after_it() {
        let db = PvDatabase::new();
        let bridge = BlockingBridge::capture();
        let (started, run) = started_flag();
        spawn_program(&bridge, &db, "probe", run);

        crate::runtime::task::sleep(Duration::from_millis(100)).await;
        assert!(
            !started.load(Ordering::Acquire),
            "the program ran before the PINI pass was published"
        );

        db.mark_pini_done();
        assert!(
            eventually(|| started.load(Ordering::Acquire)).await,
            "the program did not start once PINI was published"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn a_program_started_after_pini_runs_at_once() {
        let db = PvDatabase::new();
        db.mark_pini_done();
        let bridge = BlockingBridge::capture();
        let (started, run) = started_flag();
        spawn_program(&bridge, &db, "probe", run);

        assert!(eventually(|| started.load(Ordering::Acquire)).await);
    }
}
