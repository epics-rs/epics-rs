//! A CP/CPP link's holder processes once when the link connects at iocRun —
//! C `dbCaRun` releasing the first monitor event of every CP link, whose
//! `eventCallback` adds `CA_DBPROCESS` (`dbCa.c:1005-1012`). `VAL+1` counts
//! the processes; every number is C softIoc's `dbgf` on the same database
//! (R7.0.10-146), after init and after `dbpf X 1`.

mod calink_ioc;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "X") { field(VAL, "0") }
record(ao, "XP") { field(VAL, "0") field(PINI, "YES") }
record(calc, "HCP") { field(INPA, "X CP") field(CALC, "VAL+1") }
record(calc, "HCPP") { field(INPA, "X CPP") field(CALC, "VAL+1") }
record(calc, "HEV") { field(SCAN, "Event") field(EVNT, "77") field(INPA, "X CPP") field(CALC, "VAL+1") }
record(calc, "HEVCP") { field(SCAN, "Event") field(EVNT, "77") field(INPA, "X CP") field(CALC, "VAL+1") }
record(calc, "HPINI") { field(INPA, "XP CP") field(CALC, "VAL+1") }
record(calc, "HTWO") { field(INPA, "X CP") field(INPB, "X CP") field(CALC, "VAL+1") }
record(calc, "HMIX") { field(INPA, "X CP") field(INPB, "X CPP") field(CALC, "VAL+1") }
record(calc, "HMIXEV") { field(SCAN, "Event") field(EVNT, "77") field(INPA, "X CP") field(INPB, "X CPP") field(CALC, "VAL+1") }
record(calc, "HTWOCPP") { field(INPA, "X CPP") field(INPB, "X CPP") field(CALC, "VAL+1") }
"#;

/// softIoc: (holder, after init, after `dbpf X 1`). A CPP holder off Passive
/// is never processed, and a PINI source's own PINI process reaches no
/// holder, because its link has not connected yet. Each link is its own
/// `caLink`: two inputs from `X` process their holder twice, and a CPP beside
/// a CP keeps its own gate.
const C_COUNTS: &[(&str, f64, f64)] = &[
    ("HCP", 1.0, 2.0),
    ("HCPP", 1.0, 2.0),
    ("HEV", 0.0, 0.0),
    ("HEVCP", 1.0, 2.0),
    ("HPINI", 1.0, 1.0),
    ("HTWO", 2.0, 4.0),
    ("HMIX", 2.0, 4.0),
    ("HMIXEV", 1.0, 2.0),
    ("HTWOCPP", 2.0, 4.0),
];

type Counts = Vec<(String, f64)>;

fn counts(db: &PvDatabase) -> Counts {
    C_COUNTS
        .iter()
        .map(|(h, _, _)| (h.to_string(), db.get_pv(h).unwrap().to_f64().unwrap()))
        .collect()
}

/// Wait for X's pass to reach `HTWOCPP.INPB`, X's last edge, so every edge
/// before it has been decided; XP's own pass is waited for by `HPINI`.
async fn settle(db: &PvDatabase, after_put: bool) {
    let last = if after_put { 4.0 } else { 2.0 };
    let num = |pv| db.get_pv(pv).unwrap().to_f64().unwrap();
    calink_ioc::until("X's pass reaches its last edge", || num("HTWOCPP") == last).await;
    calink_ioc::until("XP's pass processes HPINI", || num("HPINI") == 1.0).await;
}

async fn put_x_and_settle(db: &PvDatabase) {
    db.put_record_field_from_ca("X", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    settle(db, true).await;
}

fn assert_counts(got: &Counts, after_put: bool) {
    for ((holder, init, put), (name, v)) in C_COUNTS.iter().zip(got) {
        assert_eq!(holder, name);
        let want = if after_put { *put } else { *init };
        assert_eq!(
            *v,
            want,
            "{holder} after {}",
            if after_put { "dbpf X 1" } else { "init" }
        );
    }
}

/// The server-builder bring-up: the scan owner runs PINI and then this pass.
#[epics_macros_rs::epics_test]
async fn the_scan_owner_processes_each_cp_holder_once_on_connect() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    let _owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    db.wait_for_pini().await;
    settle(&db, false).await;
    assert_counts(&counts(&db), false);
    put_x_and_settle(&db).await;
    assert_counts(&counts(&db), true);
}

/// The `IocApplication` bring-up: PINI runs in iocBuild, this pass at iocRun.
#[epics_macros_rs::epics_test]
async fn ioc_init_processes_each_cp_holder_once_on_connect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cp.db");
    std::fs::write(&path, DB).expect("write db");
    let seen: Arc<Mutex<Option<(Counts, Counts)>>> = Arc::default();
    let out = seen.clone();
    IocApplication::new()
        .port(0)
        .startup_line(&format!("dbLoadRecords(\"{}\")", path.display()))
        .run(move |config| async move {
            settle(&config.db, false).await;
            let init = counts(&config.db);
            put_x_and_settle(&config.db).await;
            *out.lock().unwrap() = Some((init, counts(&config.db)));
            Ok(())
        })
        .await
        .unwrap();
    let (init, put) = seen.lock().unwrap().take().expect("the runner ran");
    assert_counts(&init, false);
    assert_counts(&put, true);
}
