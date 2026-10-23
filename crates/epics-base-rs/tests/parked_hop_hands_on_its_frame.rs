//! A parked hop hands its put-notify set and its TPRO scope on as the
//! recursing frame does, and a chain driven inside a hop target resumes only
//! its own cycles, at every switch between recursing and parked targets
//! (`hop_rule::sweep`).

mod hop_rule;

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "ZZ_HOP_TRACE_CHILD";

/// A put-notify on A reaches B two hops down, through M's parked cycle; B at
/// VAL=1 withholds its FLNK, so it keeps the set and the put stays pending.
#[epics_macros_rs::epics_test]
async fn a_parked_hop_hands_its_notify_set_on() {
    hop_rule::sweep(|rule| async move {
        let (db, _) = IocBuilder::new()
            .register_record_type("busy", || Box::new(BusyRecord::default()))
            .db_string(
                r#"
record(bo, "A") { field(FLNK, "M") }
record(calc, "M") { field(CALC, "VAL+1") field(FLNK, "B") }
record(busy, "B") { field(ZNAM, "Done") field(ONAM, "Busy") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        db.put_record_field_from_ca_no_notify("B", "VAL", EpicsValue::Double(1.0))
            .await
            .unwrap();
        let held = db
            .put_record_field_from_ca("A", "VAL", EpicsValue::Double(1.0))
            .await
            .unwrap();
        assert!(
            db.get_record("B").unwrap().read().has_notify(),
            "{rule}: B joined A's set"
        );
        assert!(
            !held.is_sync(),
            "{rule}: B withholds its FLNK, so the put is pending"
        );
    })
    .await;
}

/// C: TPRO on A traces A, B, C; with A.TPRO back at 0 the second pass of the
/// chain prints nothing. Each run of the sweep prints the four lines.
#[epics_macros_rs::epics_test]
async fn a_parked_tpro_frame_ends_its_trace() {
    if std::env::var_os(CHILD).is_none() {
        let me = "a_parked_tpro_frame_ends_its_trace";
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([me, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let trace: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter(|l| l.contains("dbProcess") || l.contains("MARK"))
            .map(|l| l.split_once(": ").map_or(l, |(_, r)| r).to_string())
            .collect();
        let run = [
            "dbProcess of 'A'",
            "dbProcess of 'B'",
            "dbProcess of 'C'",
            "MARK",
        ];
        assert!(trace.len() >= 3 * run.len(), "{trace:?}");
        for printed in trace.chunks(run.len()) {
            assert_eq!(printed, run, "{trace:?}");
        }
        return;
    }
    hop_rule::sweep(|_| async {
        let (db, _) = IocBuilder::new()
            .db_string(
                r#"
record(calc, "H") { field(CALC, "VAL+1") field(FLNK, "A") }
record(calc, "A") { field(CALC, "VAL+1") field(FLNK, "B") field(TPRO, "1") }
record(calc, "B") { field(CALC, "VAL+1") field(FLNK, "C") }
record(calc, "C") { field(CALC, "VAL+1") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        db.put_record_field_from_ca_no_notify("H", "PROC", EpicsValue::Char(1))
            .await
            .unwrap();
        db.put_record_field_from_ca_no_notify("A", "TPRO", EpicsValue::Char(0))
            .await
            .unwrap();
        println!("x: MARK");
        db.put_record_field_from_ca_no_notify("H", "PROC", EpicsValue::Char(1))
            .await
            .unwrap();
    })
    .await;
}

/// B is a hop target whose OUT PP starts a parked chain of its own (X, Y,
/// Z): that chain's driver resumes X and stops, leaving A parked under it.
#[epics_macros_rs::epics_test]
async fn a_chain_inside_a_hop_target_resumes_only_its_own_cycles() {
    hop_rule::sweep(|rule| async move {
        let (db, _) = IocBuilder::new()
            .db_string(
                r#"
record(calc, "H") { field(CALC, "VAL+1") field(FLNK, "A") }
record(calc, "A") { field(CALC, "VAL+1") field(FLNK, "B") }
record(calcout, "B") { field(CALC, "VAL+1") field(OUT, "X.PROC PP") field(FLNK, "W") }
record(calc, "X") { field(CALC, "VAL+1") field(FLNK, "Y") }
record(calc, "Y") { field(CALC, "VAL+1") field(FLNK, "Z") }
record(calc, "Z") { field(CALC, "VAL+1") }
record(calc, "W") { field(INPA, "A.PACT NPP") field(CALC, "A") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        for _ in 0..2 {
            db.put_record_field_from_ca_no_notify("H", "PROC", EpicsValue::Char(1))
                .await
                .unwrap();
        }
        let vals: Vec<f64> = ["H", "A", "B", "X", "Y", "Z", "W"]
            .iter()
            .map(|n| db.get_pv(n).unwrap().to_f64().unwrap())
            .collect();
        assert_eq!(
            vals,
            [2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 1.0],
            "{rule}: W reads A.PACT 1, as C"
        );
    })
    .await;
}

/// B's INPA PP processes X inside B's input fetch, where B cannot park: X's
/// chain gets a driver of its own over A's parked cycle, which resumes X and
/// stops. softIoc: every record 2, W 1.
#[epics_macros_rs::epics_test]
async fn a_chain_inside_a_hop_targets_input_resumes_only_its_own_cycles() {
    hop_rule::sweep(|rule| async move {
        let (db, _) = IocBuilder::new()
            .db_string(
                r#"
record(calc, "H") { field(CALC, "VAL+1") field(FLNK, "A") }
record(calc, "A") { field(CALC, "VAL+1") field(FLNK, "B") }
record(calc, "B") { field(INPA, "X PP") field(CALC, "VAL+1") field(FLNK, "W") }
record(calc, "X") { field(CALC, "VAL+1") field(FLNK, "Y") }
record(calc, "Y") { field(CALC, "VAL+1") field(FLNK, "Z") }
record(calc, "Z") { field(CALC, "VAL+1") }
record(calc, "W") { field(INPA, "A.PACT NPP") field(CALC, "A") }
"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        for _ in 0..2 {
            db.put_record_field_from_ca_no_notify("H", "PROC", EpicsValue::Char(1))
                .await
                .unwrap();
        }
        let vals: Vec<f64> = ["H", "A", "B", "X", "Y", "Z", "W"]
            .iter()
            .map(|n| db.get_pv(n).unwrap().to_f64().unwrap())
            .collect();
        assert_eq!(
            vals,
            [2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 1.0],
            "{rule}: W reads A.PACT 1, as C"
        );
    })
    .await;
}
