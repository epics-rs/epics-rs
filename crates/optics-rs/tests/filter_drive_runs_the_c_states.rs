//! The compiled `filterDrive.st` on `filterDrive.db` and
//! `filterBladeNoSensor.db` blades: the `needRecalc`, `externalIO` and
//! `STATUS_BUSY` clauses of `waiting` (`filterDrive.st:411-465`), and C's
//! console lines against snc 2.2.9's build of the same program.

use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::{SeqThreadId, seq, seq_stop};
use optics_rs::snl::filter_drive::filterDrive;
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

/// The program on the IOC; the scan owner keeps the scan threads up.
struct Fd {
    ioc: Arc<PvDatabase>,
    _scan: ScanOwner,
    tid: SeqThreadId,
}

/// The IOC with `n` blades, booted past C's four seconds of `start`/`init`
/// sleeps and the `update_filters` passes they end in.
async fn boot(n: usize) -> Fd {
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
    let tid = seq(
        &filterDrive,
        &format!("NAME=filterDrive,P=F:,R=f:,NUM_FILTERS={n}"),
        0,
        &ioc,
    )
    .expect("filterDrive starts");
    // The first pass computes every blade and clears the busy `init` set;
    // the passes behind it take milliseconds.
    let last = format!("F:f:Fi{}:Transmission", n.min(32));
    let t0 = std::time::Instant::now();
    while f64_of(&ioc, &last).await == 0.0 || f64_of(&ioc, "F:f:Status").await != 0.0 {
        assert!(t0.elapsed() < Duration::from_secs(60), "boot never ended");
        task::sleep(Duration::from_millis(50)).await;
    }
    task::sleep(Duration::from_millis(1000)).await;
    Fd {
        ioc,
        _scan: scan,
        tid,
    }
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
    let f = boot(2).await;
    assert_eq!(f64_of(&f.ioc, "F:f:NumFilters").await, 2.0);
    assert_eq!(f64_of(&f.ioc, "F:f:Status").await, 0.0);
    assert_eq!(string_of(&f.ioc, "F:f:Message").await, "OK");
    seq_stop(f.tid);
}

/// `sync THICK needRecalc`: a thickness change recomputes that blade.
#[epics_base_rs::epics_test]
async fn a_thickness_change_recomputes_the_blade() {
    let f = boot(2).await;
    let before = f64_of(&f.ioc, "F:f:Fi1:Transmission").await;
    put(&f.ioc, "F:f:Fi1:Thickness", EpicsValue::Double(400.0)).await;
    task::sleep(Duration::from_millis(500)).await;
    let after = f64_of(&f.ioc, "F:f:Fi1:Transmission").await;
    assert!(after < before, "{after} after 400 um, {before} before");
    seq_stop(f.tid);
}

/// `sync OUTGET externalIO`: a blade moved by something else takes its `Set`
/// from the readback, and says so.
#[epics_base_rs::epics_test]
async fn an_external_out_change_takes_the_blade_set() {
    let f = boot(2).await;
    put(&f.ioc, "F:o2", EpicsValue::String("in".into())).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&f.ioc, "F:f:Fi2:Set").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "F:f:FilterMask").await, 2.0);
    assert_eq!(
        string_of(&f.ioc, "F:f:Message").await,
        "Filter state set by external agent."
    );
    seq_stop(f.tid);
}

/// `when (STATUS_BUSY)`: a busy record set from outside runs
/// `update_filters`, whose `changeFlag` clears it.
#[epics_base_rs::epics_test]
async fn an_outside_busy_is_cleared_by_update_filters() {
    let f = boot(2).await;
    put(&f.ioc, "F:f:Status", EpicsValue::Enum(1)).await;
    task::sleep(Duration::from_millis(500)).await;
    assert_eq!(f64_of(&f.ioc, "F:f:Status").await, 0.0);
    seq_stop(f.tid);
}

/// With no blade enabled the table is one pattern, all out, and both steps
/// fall back to the current transmission (`filterDrive.st:534-625`).
#[epics_base_rs::epics_test]
async fn no_enabled_blade_leaves_one_pattern() {
    let f = boot(2).await;
    assert!(f64_of(&f.ioc, "F:f:TransmissionDown").await < 1.0);
    for pv in ["F:f:Fi1:Enable", "F:f:Fi2:Enable"] {
        put(&f.ioc, pv, EpicsValue::Enum(0)).await;
        task::sleep(Duration::from_millis(1000)).await;
    }
    for pv in [
        "F:f:Transmission",
        "F:f:TransmissionUp",
        "F:f:TransmissionDown",
    ] {
        assert_eq!(f64_of(&f.ioc, pv).await, 1.0, "{pv}");
    }
    seq_stop(f.tid);
}

/// A locked inserted blade is in every pattern and a disabled one in none:
/// of three blades, the table is blade 1 alone and blades 1 and 2.
///
/// The blades go in from the hardware side. A `Set` put races its own
/// readback, in C too: `SetBusy` wakes `update_filters` before the `OutGet`
/// CP link has the new output, and the stale `OUTGET` is put back to `Set`.
#[epics_base_rs::epics_test]
async fn locked_and_disabled_blades_shape_the_patterns() {
    let f = boot(3).await;
    for (pv, v) in [
        ("F:o1", EpicsValue::String("in".into())),
        ("F:f:Fi1:Lock", EpicsValue::Enum(1)),
        ("F:o3", EpicsValue::String("in".into())),
        ("F:f:Fi3:Enable", EpicsValue::Enum(0)),
    ] {
        put(&f.ioc, pv, v).await;
        task::sleep(Duration::from_millis(1000)).await;
    }
    let t1 = f64_of(&f.ioc, "F:f:Fi1:Transmission").await;
    let t2 = f64_of(&f.ioc, "F:f:Fi2:Transmission").await;
    assert_eq!(f64_of(&f.ioc, "F:f:FilterMask").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "F:f:Transmission").await, t1);
    assert_eq!(f64_of(&f.ioc, "F:f:TransmissionUp").await, t1);
    assert_eq!(f64_of(&f.ioc, "F:f:TransmissionDown").await, t1 * t2);
    seq_stop(f.tid);
}

/// A blade in the beam with a NaN thickness leaves the current pattern
/// unfound (`filterDrive.st:574-580`): the steps fall back to it, and a step
/// down selects no pattern and moves nothing. Blade 1 goes in from the
/// hardware side, as in [`locked_and_disabled_blades_shape_the_patterns`].
///
/// C defect fixed: C read and printed `fltrsIn[-1]` there.
#[epics_base_rs::epics_test]
async fn an_unfound_current_pattern_selects_nothing() {
    let f = boot(2).await;
    for (pv, v) in [
        ("F:f:Debug", EpicsValue::Double(1.0)),
        ("F:o1", EpicsValue::String("in".into())),
        ("F:f:Fi1:Thickness", EpicsValue::Double(f64::NAN)),
    ] {
        put(&f.ioc, pv, v).await;
        task::sleep(Duration::from_millis(1000)).await;
    }
    assert!(f64_of(&f.ioc, "F:f:TransmissionDown").await.is_nan());
    put(&f.ioc, "F:f:TransmissionStepDown", EpicsValue::Enum(1)).await;
    task::sleep(Duration::from_millis(1000)).await;
    assert_eq!(f64_of(&f.ioc, "F:f:Fi1:Set").await, 1.0);
    assert_eq!(f64_of(&f.ioc, "F:f:Fi2:Set").await, 0.0);
    assert_eq!(f64_of(&f.ioc, "F:f:Status").await, 0.0);
    seq_stop(f.tid);
}

/// A setpoint below every pattern when none is below 1.0 (both blades 0 um)
/// keeps the current pattern (`filterDrive.st:672-690`).
///
/// C defect fixed: C took `indMin` from the earlier table, where both blades
/// in was the lowest, and put them in.
#[epics_base_rs::epics_test]
async fn no_pattern_below_one_keeps_the_current_one() {
    let f = boot(2).await;
    for (pv, v) in [
        ("F:f:TransmissionSetpoint", 0.0001),
        ("F:f:TransmissionSetpoint", 1.0),
        ("F:f:Fi1:Thickness", 0.0),
        ("F:f:Fi2:Thickness", 0.0),
        ("F:f:TransmissionSetpoint", 0.5),
    ] {
        put(&f.ioc, pv, EpicsValue::Double(v)).await;
        task::sleep(Duration::from_millis(2000)).await;
    }
    assert_eq!(f64_of(&f.ioc, "F:f:Fi1:Set").await, 0.0);
    assert_eq!(f64_of(&f.ioc, "F:f:Fi2:Set").await, 0.0);
    assert_eq!(string_of(&f.ioc, "F:f:Message").await, "OK - No change");
    seq_stop(f.tid);
}

/// With `MAX_FILTERS` blades, 16 of them disabled, a mask in range takes
/// its blades (`filterDrive.st:750-797`).
///
/// C defect fixed: C bounded the mask by `(1 << 32) - 1`, which x86 makes
/// `0`, and refused every mask but 0.
#[epics_base_rs::epics_test]
async fn a_mask_with_32_blades_takes_its_blades() {
    let f = boot(32).await;
    for i in 17..=32 {
        put(&f.ioc, &format!("F:f:Fi{i}:Enable"), EpicsValue::Enum(0)).await;
        task::sleep(Duration::from_millis(100)).await;
    }
    task::sleep(Duration::from_millis(1000)).await;
    put(&f.ioc, "F:f:FilterMaskSetpoint", EpicsValue::Double(5.0)).await;
    task::sleep(Duration::from_millis(3000)).await;
    assert_eq!(string_of(&f.ioc, "F:f:Message").await, "OK");
    for (i, set) in [(1, 1.0), (2, 0.0), (3, 1.0)] {
        assert_eq!(
            f64_of(&f.ioc, &format!("F:f:Fi{i}:Set")).await,
            set,
            "blade {i}"
        );
    }
    seq_stop(f.tid);
}

/// `NUM_FILTERS` past `MAX_FILTERS` runs `MAX_FILTERS` blades
/// (`filterDrive.st:342-381`).
///
/// C defect fixed: C assigned `THICK[32]`..`OUTGET[32]`, past the arrays.
#[epics_base_rs::epics_test]
async fn num_filters_past_max_filters_runs_32_blades() {
    let f = boot(33).await;
    assert_eq!(f64_of(&f.ioc, "F:f:NumFilters").await, 32.0);
    assert_eq!(f64_of(&f.ioc, "F:f:Status").await, 0.0);
    seq_stop(f.tid);
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
    seq_stop(drive(2, STAGES).await.tid);
}

/// `n` blades driven through `stages`, each headed by its mark, and the
/// IOC they ran on.
async fn drive(n: usize, stages: &[Stage]) -> Fd {
    let f = boot(n).await;
    for (stage, puts) in stages {
        println!("=== {stage}");
        for (pv, v) in *puts {
            let value = match v.parse::<f64>() {
                Ok(x) => EpicsValue::Double(x),
                Err(_) => EpicsValue::String((*v).into()),
            };
            put(&f.ioc, pv, value).await;
            task::sleep(Duration::from_millis(1000)).await;
        }
        task::sleep(Duration::from_millis(1000)).await;
    }
    println!("=== end");
    f
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
    seq_stop(drive(7, STAGES7).await.tid);
}

/// More free blades than the table holds: C's line and message, an empty
/// table, and steps and masks that select no pattern
/// (`filterDrive.st:521-539`).
///
/// C defect fixed: C went on to `nPerms = 1 << nEnabled` and filled
/// `transMis`/`fltrsIn`, sized `MAX_PERMS`, past their end.
#[epics_base_rs::epics_test]
async fn seventeen_free_blades_leave_the_table_empty() {
    if std::env::var_os(CHILD).is_none() {
        let update = [
            "filterDrive: ERROR: Too many filters unlocked (17/16).",
            "ERROR: Could not find current filter index",
        ];
        let busy = [
            "filterDrive: Busy record was set",
            "filterDrive: entering state: update_filters",
            update[0],
            update[1],
            "filterDrive: clearing busy record.",
        ];
        let want: Vec<&str> = [
            &[
                "=== debug 1, energy 12",
                "filterDrive: Energy change detected",
                "filterDrive: entering state: update_filters",
            ][..],
            &update,
            &[
                "=== step up at the top",
                "filterDrive: step up.",
                "Action = 1",
                "filterDrive: No more steps up available!",
            ],
            &busy,
            &[
                "=== mask 1",
                "filterDrive: filter mask setpoint changed.",
                "Action = 3",
                "filterDrive: Required filter mask: 1",
                "filterDrive: Adjusted filter mask: 1",
                "filterDrive: Cannot find the requested filter pattern!",
            ],
            &busy,
            &["=== end"],
        ]
        .concat();
        assert_eq!(
            trace_of("seventeen_free_blades_leave_the_table_empty"),
            want
        );
        return;
    }
    let stages: &[Stage] = &[
        ("debug 1, energy 12", &[("F:f:Debug", "1"), ("F:kev", "12")]),
        ("step up at the top", &[("F:f:TransmissionStepUp", "1")]),
        ("mask 1", &[("F:f:FilterMaskSetpoint", "1")]),
    ];
    let f = drive(17, stages).await;
    assert_eq!(
        string_of(&f.ioc, "F:f:Message").await,
        "ERROR: Too many unlocked (17/16)"
    );
    assert_eq!(f64_of(&f.ioc, "F:f:Fi1:Set").await, 0.0);
    seq_stop(f.tid);
}
