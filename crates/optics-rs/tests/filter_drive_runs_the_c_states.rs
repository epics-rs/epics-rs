//! filterDrive's runner against `filterDrive.db` and `filterBladeNoSensor.db`
//! blades: the `needRecalc`, `externalIO` and `STATUS_BUSY` clauses of
//! `waiting` (`filterDrive.st:396-450`).

use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use optics_rs::snl::filter_drive::{self, FilterDriveConfig};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/");

/// Each of `n` blades' output channel, and the energy.
fn outs(n: usize) -> String {
    let mut db: String = (1..=n)
        .map(|i| format!("record(stringout, \"F:o{i}\") {{ field(VAL, \"out\") }}\n"))
        .collect();
    db.push_str("record(ao, \"F:kev\") { field(VAL, \"10\") }\n");
    db
}

fn macros(kv: &[(&str, &str)]) -> HashMap<String, String> {
    kv.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The IOC with `n` blades, booted past C's four seconds of `start`/`init`
/// sleeps.
async fn boot(n: usize) -> (Arc<PvDatabase>, ScanOwner) {
    let mut b = IocBuilder::new()
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(&outs(n), &macros(&[]))
        .unwrap()
        .db_file(
            &format!("{DB}filterDrive.db"),
            &macros(&[("P", "F:"), ("R", "f:"), ("DESC", "fd"), ("KEV", "F:kev")]),
        )
        .unwrap();
    for i in 1..=n {
        let (i, out) = (i.to_string(), format!("F:o{i}"));
        b = b
            .db_file(
                &format!("{DB}filterBladeNoSensor.db"),
                &macros(&[
                    ("P", "F:"),
                    ("R", "f:"),
                    ("N", &i),
                    ("DESC", "b"),
                    ("OUT", &out),
                    ("OUT_STRING", "out"),
                    ("IN_STRING", "in"),
                ]),
            )
            .unwrap();
    }
    let ioc = b.build_served().await.unwrap().0;
    let scan = ScanOwner::start(ioc.clone());
    let db = PvDatabase::clone(&ioc);
    BlockingBridge::capture().spawn(filter_drive::run(
        FilterDriveConfig::new("F:", "f:", n as i32),
        db,
    ));
    task::sleep(Duration::from_millis(5000)).await;
    (ioc, scan)
}

async fn put(ioc: &PvDatabase, pv: &str, v: EpicsValue) {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    ioc.put_record_field_from_ca(rec, field, v).await.unwrap();
}

async fn f64_of(ioc: &PvDatabase, pv: &str) -> f64 {
    ioc.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn string_of(ioc: &PvDatabase, pv: &str) -> String {
    match ioc.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

/// `init` raises `needRecalc` and the connect monitors raise the rest, so
/// boot ends in `update_filters`: the count is published and the busy record
/// the init puts set is cleared.
#[epics_base_rs::epics_test]
async fn boot_publishes_the_count_and_clears_busy() {
    let (ioc, _scan) = boot(2).await;
    assert_eq!(f64_of(&ioc, "F:f:NumFilters").await, 2.0);
    assert_eq!(f64_of(&ioc, "F:f:Status").await, 0.0);
    assert_eq!(string_of(&ioc, "F:f:Message").await, "OK");
}

/// `sync THICK needRecalc`: a thickness change recomputes that blade.
#[epics_base_rs::epics_test]
async fn a_thickness_change_recomputes_the_blade() {
    let (ioc, _scan) = boot(2).await;
    let before = f64_of(&ioc, "F:f:Fi1:Transmission").await;
    put(&ioc, "F:f:Fi1:Thickness", EpicsValue::Double(400.0)).await;
    task::sleep(Duration::from_millis(500)).await;
    let after = f64_of(&ioc, "F:f:Fi1:Transmission").await;
    assert!(after < before, "{after} after 400 um, {before} before");
}

/// `sync OUTGET externalIO`: a blade moved by something else takes its `Set`
/// from the readback, and says so.
#[epics_base_rs::epics_test]
async fn an_external_out_change_takes_the_blade_set() {
    let (ioc, _scan) = boot(2).await;
    put(&ioc, "F:o2", EpicsValue::String("in".into())).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&ioc, "F:f:Fi2:Set").await, 1.0);
    assert_eq!(f64_of(&ioc, "F:f:FilterMask").await, 2.0);
    assert_eq!(
        string_of(&ioc, "F:f:Message").await,
        "Filter state set by external agent."
    );
}

/// `when (STATUS_BUSY)`: a busy record set from outside runs
/// `update_filters`, whose `changeFlag` clears it.
#[epics_base_rs::epics_test]
async fn an_outside_busy_is_cleared_by_update_filters() {
    let (ioc, _scan) = boot(2).await;
    put(&ioc, "F:f:Status", EpicsValue::Enum(1)).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&ioc, "F:f:Status").await, 0.0);
}

const CHILD: &str = "FILTER_DRIVE_DEBUG_CHILD";

/// `test` run again in a child whose stdout is not captured, and what it
/// printed from its first stage mark through `=== end`.
fn trace_of(test: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // libtest's `test <name> ... ` heads the first line, unterminated.
    let start = stdout.find("=== ").expect("no stage mark");
    let mut trace: Vec<String> = Vec::new();
    for line in stdout[start..].lines() {
        trace.push(line.to_string());
        if line == "=== end" {
            break;
        }
    }
    trace
}

/// A stage's name, and its puts one second apart and a second after the
/// last.
type Stage = (&'static str, &'static [(&'static str, &'static str)]);

/// The stages of `tests/oracle/filterDrive_debug.txt`.
const STAGES: &[Stage] = &[
    (
        "debug 15, energy 12",
        &[("F:f:Debug", "15"), ("F:kev", "12")],
    ),
    (
        "debug 1, setpoint 0.5",
        &[("F:f:Debug", "1"), ("F:f:TransmissionSetpoint", "0.5")],
    ),
    ("step up", &[("F:f:TransmissionStepUp", "1")]),
    ("step down", &[("F:f:TransmissionStepDown", "1")]),
    ("mask 9", &[("F:f:FilterMaskSetpoint", "9")]),
    ("mask 2", &[("F:f:FilterMaskSetpoint", "2")]),
    ("setpoint 0.5", &[("F:f:TransmissionSetpoint", "0.5")]),
    ("setpoint 0.5 again", &[("F:f:TransmissionSetpoint", "0.5")]),
    (
        "step down at the bottom",
        &[("F:f:TransmissionStepDown", "1")],
    ),
    ("mask 0", &[("F:f:FilterMaskSetpoint", "0")]),
    ("step up at the top", &[("F:f:TransmissionStepUp", "1")]),
    ("energy 1000", &[("F:kev", "1000")]),
    (
        "debug 0, material Xx, energy 10",
        &[
            ("F:f:Debug", "0"),
            ("F:f:Fi1:Material.ZRST", "Xx"),
            ("F:kev", "10"),
        ],
    ),
    (
        "debug 1, material 0",
        &[("F:f:Debug", "1"), ("F:f:Fi1:Material", "0")],
    ),
    // Each `calcTrans` and table bit alone, blade 1 still on `Xx`.
    ("debug 2, energy 12", &[("F:f:Debug", "2"), ("F:kev", "12")]),
    ("debug 4, energy 13", &[("F:f:Debug", "4"), ("F:kev", "13")]),
    ("debug 8, energy 14", &[("F:f:Debug", "8"), ("F:kev", "14")]),
    ("energy 1000 at debug 8", &[("F:kev", "1000")]),
];

/// With `Debug` set, the program prints C's debug lines: the oracle is
/// `filterDrive.st` built by snc 2.2.9 against the same databases, driven by
/// the same puts from `dbpf`.
#[epics_base_rs::epics_test]
async fn debug_prints_c_lines() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("debug_prints_c_lines");
        let c: Vec<&str> = include_str!("oracle/filterDrive_debug.txt")
            .lines()
            .collect();
        assert_eq!(trace, c);
        return;
    }
    drive(2, STAGES).await;
}

/// `n` blades driven through `stages`, each headed by its mark, and the
/// IOC they ran on.
async fn drive(n: usize, stages: &[Stage]) -> (Arc<PvDatabase>, ScanOwner) {
    let (ioc, scan) = boot(n).await;
    for (stage, puts) in stages {
        println!("=== {stage}");
        for (pv, v) in *puts {
            let value = match v.parse::<f64>() {
                Ok(x) => EpicsValue::Double(x),
                Err(_) => EpicsValue::String((*v).into()),
            };
            put(&ioc, pv, value).await;
            task::sleep(Duration::from_millis(1000)).await;
        }
        task::sleep(Duration::from_millis(1000)).await;
    }
    println!("=== end");
    (ioc, scan)
}

/// The stages of `tests/oracle/filterDrive_debug7.txt`.
const STAGES7: &[Stage] = &[
    // 128 patterns, of which C prints 100.
    ("debug 3, energy 12", &[("F:f:Debug", "3"), ("F:kev", "12")]),
    // The masks print in hex.
    (
        "debug 1, mask a",
        &[("F:f:Debug", "1"), ("F:f:FilterMaskSetpoint", "10")],
    ),
    // Six free blades of seven: the rows' two widths differ.
    (
        "debug 3, lock blade 2, mask 35",
        &[
            ("F:f:Debug", "3"),
            ("F:f:Fi2:Lock", "1"),
            ("F:f:FilterMaskSetpoint", "53"),
        ],
    ),
    (
        "debug 1, blade 7 in from outside",
        &[("F:f:Debug", "1"), ("F:o7", "in")],
    ),
];

/// Seven blades: the pattern table's 100-row cap, the hex mask lines and
/// the `externalIO` clause, against the same C program on seven blades.
#[epics_base_rs::epics_test]
async fn seven_blades_print_c_lines() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("seven_blades_print_c_lines");
        let c: Vec<&str> = include_str!("oracle/filterDrive_debug7.txt")
            .lines()
            .collect();
        assert_eq!(trace, c);
        return;
    }
    drive(7, STAGES7).await;
}

/// More free blades than C's table holds: C's line and message, pinned on
/// this side because C's run is undefined after them.
///
/// C defect not copied: after the line C goes on to `nPerms = 1 << nEnabled`
/// (`filterDrive.st:519`) and fills `transMis`/`fltrsIn`, sized `MAX_PERMS`
/// (`:223-224`), past their end.
#[epics_base_rs::epics_test]
async fn seventeen_free_blades_print_too_many() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("seventeen_free_blades_print_too_many"),
            [
                "=== debug 1, energy 12",
                "filterDrive: Energy change detected",
                "filterDrive: entering state: update_filters",
                "filterDrive: ERROR: Too many filters unlocked (17/16).",
                "=== step up at the top",
                "filterDrive: step up.",
                "Action = 1",
                "filterDrive: No more steps up available!",
                "filterDrive: Next filter pattern is 0000.0000.0000.0000.0,  Transmission = 1.000000e+00",
                "filterDrive: Busy record was set",
                "filterDrive: entering state: update_filters",
                "filterDrive: ERROR: Too many filters unlocked (17/16).",
                "filterDrive: clearing busy record.",
                "=== end",
            ]
        );
        return;
    }
    let stages: &[Stage] = &[
        ("debug 1, energy 12", &[("F:f:Debug", "1"), ("F:kev", "12")]),
        // Its update's entry puts the step's message, which the line's
        // message then replaces.
        ("step up at the top", &[("F:f:TransmissionStepUp", "1")]),
    ];
    let (ioc, _scan) = drive(17, stages).await;
    assert_eq!(
        string_of(&ioc, "F:f:Message").await,
        "ERROR: Too many unlocked (17/16)"
    );
}
