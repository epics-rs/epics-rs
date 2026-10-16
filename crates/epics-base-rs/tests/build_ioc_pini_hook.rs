//! `piniProcessHook` (`iocInit.c:630-651`) on the `build_ioc` path, the one
//! `oracle-ioc` runs: every `iocPause` runs the PAUSE and PAUSED passes and
//! every `iocRun` the RUN and RUNNING passes, and a refused transition runs
//! none. Each record bumps CNT, so its VAL is the step it last processed at.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::{ioc_pause, ioc_run, ioc_shutdown};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(ao, "CNT") {}
record(calcout, "PP") { field(PINI, "PAUSE") field(INPA, "CNT") field(CALC, "A+1") field(OUT, "CNT") }
record(calcout, "PD") { field(PINI, "PAUSED") field(INPA, "CNT") field(CALC, "A+1") field(OUT, "CNT") }
record(calcout, "PR") { field(PINI, "RUN") field(INPA, "CNT") field(CALC, "A+1") field(OUT, "CNT") }
record(calcout, "PG") { field(PINI, "RUNNING") field(INPA, "CNT") field(CALC, "A+1") field(OUT, "CNT") }
"#;

/// The step each of PP, PD, PR and PG last processed at.
fn steps(db: &PvDatabase) -> [f64; 4] {
    ["PP", "PD", "PR", "PG"].map(|name| {
        match db.get_record(name).unwrap().read().record.get_field("VAL") {
            Some(EpicsValue::Double(v)) => v,
            other => panic!("{name}.VAL is {other:?}"),
        }
    })
}

#[epics_macros_rs::epics_test]
async fn every_pause_and_run_runs_its_pini_passes() {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = std::sync::Arc::clone(ioc.database());
    let steps = || steps(&db);

    assert_eq!(ioc_pause(), -1, "a built IOC is not running");
    assert_eq!(steps(), [0.0; 4], "a refused iocPause runs no pass");

    assert!(ioc.run().await, "a built IOC runs");
    assert_eq!(steps(), [0.0, 0.0, 1.0, 2.0]);
    assert_eq!(ioc_run(), -1, "a running IOC is not paused");
    assert_eq!(
        steps(),
        [0.0, 0.0, 1.0, 2.0],
        "a refused iocRun runs no pass"
    );

    assert_eq!(ioc_pause(), 0);
    assert_eq!(steps(), [3.0, 4.0, 1.0, 2.0]);
    assert_eq!(ioc_run(), 0);
    assert_eq!(steps(), [3.0, 4.0, 5.0, 6.0]);
}

/// C registers `piniProcessHook` once (`initHookRegister` ignores a known
/// function) and it walks the one `pdbbase`, so after `iocShutdown` and a
/// second build only the second database runs passes, and runs each once.
#[epics_macros_rs::epics_test]
async fn a_rebuild_runs_its_passes_on_the_new_database_only() {
    let build = || async {
        IocBuilder::new()
            .db_string(DB, &HashMap::new())
            .unwrap()
            .build_ioc()
            .await
            .unwrap()
    };
    let first = build().await;
    let old = std::sync::Arc::clone(first.database());
    assert!(first.run().await, "a built IOC runs");
    assert_eq!(steps(&old), [0.0, 0.0, 1.0, 2.0]);
    assert_eq!(ioc_shutdown(), 0);

    let second = build().await;
    let new = std::sync::Arc::clone(second.database());
    assert!(second.run().await, "a rebuilt IOC runs");
    assert_eq!(ioc_pause(), 0);
    assert_eq!(ioc_run(), 0);

    assert_eq!(steps(&new), [3.0, 4.0, 5.0, 6.0]);
    assert_eq!(
        steps(&old),
        [0.0, 0.0, 1.0, 2.0],
        "the old database runs no pass"
    );
}
