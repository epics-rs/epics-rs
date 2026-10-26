//! One state set's foreign call does not hold another's monitors. In C each
//! state set runs on a thread of its own and `seq_mon_handler`
//! (`seq_ca.c:180`) writes a post on `CAC-event` at once, so while
//! XPS_trajectoryScan's xpsTrajectoryRun is inside `trajectoryExecute` for
//! the whole trajectory, xpsTrajectoryAbort still takes the abort.
//!
//! `programs/foreignCall.rs` is snc-rs's output for `programs/foreignCall.st`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::db_access::DbChannel;
use epics_base_rs::server::ioc_builder::IocBuilder;

/// The companion: the escaped C, and `trajectoryExecute`'s Rust.
mod foreign_call {
    use std::sync::{Condvar, Mutex};

    macro_rules! seqg_escaped {
        (r#"extern void trajectoryExecute(void);"#) => {};
        ($env:ident, r#"trajectoryExecute();"#) => {
            trajectory_execute()
        };
    }

    include!("programs/foreignCall.rs");

    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Trajectory {
        Idle,
        Executing,
        Ended,
    }

    pub static TRAJECTORY: Mutex<Trajectory> = Mutex::new(Trajectory::Idle);
    pub static CHANGED: Condvar = Condvar::new();

    /// XPS's `MultipleAxesPVTExecution`: it returns when the trajectory
    /// ends, here when the test ends it.
    fn trajectory_execute() {
        let mut t = TRAJECTORY.lock().unwrap();
        *t = Trajectory::Executing;
        CHANGED.notify_all();
        while *t != Trajectory::Ended {
            t = CHANGED.wait(t).unwrap();
        }
    }
}

use foreign_call::{CHANGED, TRAJECTORY, Trajectory};

/// What an IOC calls to register the program, as C's dbd `registrar`.
const _: fn() = foreign_call::foreignCallRegistrar;

fn trajectory() -> Trajectory {
    *TRAJECTORY.lock().unwrap()
}

fn end_trajectory() {
    *TRAJECTORY.lock().unwrap() = Trajectory::Ended;
    CHANGED.notify_all();
}

async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        task::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[epics_macros_rs::epics_test]
async fn an_abort_lands_while_another_state_set_is_in_a_foreign_call() {
    let ioc = IocBuilder::new()
        .db_string(
            "record(longout, \"fc:execState\") {}\nrecord(longout, \"fc:abort\") {}\n",
            &HashMap::new(),
        )
        .expect("dbLoadRecords")
        .build_ioc()
        .await
        .expect("iocBuild");
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await, "iocRun");
    epics_seq_rs::seq(&foreign_call::foreignCall, "", 0, &db).expect("foreignCall starts");
    let read = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();

    until("xpsTrajectoryRun enters trajectoryExecute", || {
        trajectory() == Trajectory::Executing
    })
    .await;
    assert_eq!(read("fc:execState"), 1.0);
    DbChannel::new(&db, "fc:abort")
        .put_f64_process(1.0)
        .await
        .unwrap();
    until("xpsTrajectoryAbort clears abort", || {
        read("fc:abort") == 0.0
    })
    .await;
    assert_eq!(
        trajectory(),
        Trajectory::Executing,
        "the trajectory still runs"
    );
    end_trajectory();
}
