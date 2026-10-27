//! A hop frame's OUT put (taken by a record that is itself an OUT PP target)
//! or FLNK carries its PUTF and wait-set to the next target, its output stage
//! resumes with the OOPT state and the `.PROC` gate, and a cycle the chain
//! reaches again runs again. Values are C softIoc's on the same records.
//! Each case runs at every switch point (`hop_rule::sweep`).

mod hop_rule;

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(calc, "N2B") { field(CALC, "VAL+1") }
record(calcout, "T2B") { field(ODLY, "0.5") field(OOPT, "Every Time") field(CALC, "A") field(FLNK, "N2B") }
record(ao, "U2B") { field(OUT, "T2B.A PP") }
record(ao, "S2B") { field(OUT, "U2B PP") }

record(calcout, "T3B") { field(ODLY, "1") field(OOPT, "Every Time") field(CALC, "A") }
record(ao, "U3B") { field(OUT, "T3B.A PP") }
record(ao, "S3B") { field(OUT, "U3B PP") }

record(calc, "M4X") { field(CALC, "VAL+1") }
record(longout, "L4X") { field(OOPT, "On Change") field(OUT, "M4X.A PP") }
record(ao, "S4X") { field(OUT, "L4X PP") }

record(calc, "Y13X") { field(SCAN, "Event") field(EVNT, "99") field(CALC, "VAL+1") }
record(ao, "U13X") { field(OUT, "Y13X.PROC") }
record(ao, "S13X") { field(OUT, "U13X PP") }

record(calc, "NF") { field(CALC, "VAL+1") }
record(calcout, "TF") { field(ODLY, "0.5") field(OOPT, "Every Time") field(CALC, "A") field(FLNK, "NF") }
record(calc, "MF") { field(CALC, "VAL+1") field(FLNK, "TF") }
record(ao, "PF") { field(FLNK, "MF") }

record(calc, "O14") { field(CALC, "VAL+1") }
record(calc, "K14") { field(CALC, "VAL+1") }
record(calcout, "A14") { field(CALC, "VAL+1") field(OUT, "O14.A PP") field(FLNK, "K14") }
record(calcout, "X14") { field(CALC, "1") field(OUT, "A14.A PP") field(FLNK, "A14") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn put(db: &PvDatabase, rec: &str, field: &str, v: f64) {
    let _ = db
        .put_record_field_from_ca(rec, field, EpicsValue::Double(v))
        .await
        .unwrap();
}

/// Until `count` reads `n` and `rec`, the delayed record feeding it, is idle
/// with no RPRO owed. A PACT poll alone cannot wait out RPRO: the restart
/// leaves PACT clear before the rerun begins.
async fn counted(db: &PvDatabase, count: &str, n: f64, rec: &str) {
    let cell = db.get_record(rec).unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        loop {
            if num(db, count) >= n {
                let g = cell.read();
                if !g.is_processing() && g.common.rpro == 0 {
                    return;
                }
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{count} never reached {n} with {rec} idle"));
}

/// T2B is busy in its ODLY when U2B's put reaches it: `processTarget` sets
/// RPRO because the put came from `dbPutField` (`dbDbLink.c:497-502`), so
/// T2B runs again and N2B counts 2.
#[epics_macros_rs::epics_test]
async fn a_hop_frames_put_carries_its_putf_to_a_busy_target() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        put(&db, "T2B", "PROC", 1.0).await;
        put(&db, "S2B", "VAL", 1.0).await;
        counted(&db, "N2B", 2.0, "T2B").await;
        assert_eq!(num(&db, "N2B"), 2.0, "{rule}: T2B reprocessed on RPRO");
    })
    .await;
}

/// The FLNK form: MF's hop hands PF's PUTF on to a busy TF, which runs
/// again on RPRO (`dbDbLink.c:497-502` through `dbScanFwdLink`).
#[epics_macros_rs::epics_test]
async fn a_hop_frames_forward_link_carries_its_putf_to_a_busy_target() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        put(&db, "TF", "PROC", 1.0).await;
        put(&db, "PF", "VAL", 1.0).await;
        counted(&db, "NF", 2.0, "TF").await;
        assert_eq!(num(&db, "NF"), 2.0, "{rule}: TF reprocessed on RPRO");
    })
    .await;
}

/// The put-notify on S3B completes when T3B's delayed output does.
#[epics_macros_rs::epics_test]
async fn a_hop_frames_put_carries_its_wait_set_on() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        let done = db
            .put_record_field_from_ca("S3B", "VAL", EpicsValue::Double(8.0))
            .await
            .unwrap();
        assert!(done.is_async(), "{rule}: T3B joined the set");
        let mut rx = done.into_handle().unwrap();
        assert!(
            epics_base_rs::runtime::task::timeout(Duration::from_millis(300), &mut rx)
                .await
                .is_err(),
            "{rule}: completed before T3B's ODLY ran out"
        );
        epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, rx)
            .await
            .expect("completes once T3B finishes")
            .unwrap();
        assert_eq!(num(&db, "T3B.A"), 8.0, "{rule}");
    })
    .await;
}

/// L4X writes on change only: 5, 5, 6, 6 puts M4X twice and leaves PVAL 6.
#[epics_macros_rs::epics_test]
async fn a_hop_frames_on_change_output_advances_its_last_value() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        for v in [5.0, 5.0, 6.0, 6.0] {
            put(&db, "S4X", "VAL", v).await;
        }
        assert_eq!(num(&db, "M4X"), 2.0, "{rule}");
        assert_eq!(num(&db, "L4X.PVAL"), 6.0, "{rule}");
    })
    .await;
}

/// A put to `.PROC` processes a non-Passive target (`dbDbLink.c:387`).
#[epics_macros_rs::epics_test]
async fn a_hop_frames_proc_put_processes_an_event_scanned_target() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        put(&db, "S13X", "VAL", 1.0).await;
        put(&db, "S13X", "VAL", 2.0).await;
        assert_eq!(num(&db, "Y13X"), 2.0, "{rule}");
    })
    .await;
}

/// A14 runs as X14's OUT target and again as its FLNK target.
#[epics_macros_rs::epics_test]
async fn a_record_reached_twice_by_one_chain_runs_twice() {
    hop_rule::sweep(|rule| async move {
        let db = ioc().await;
        put(&db, "X14", "PROC", 1.0).await;
        assert_eq!(num(&db, "A14"), 2.0, "{rule}");
        assert_eq!(num(&db, "O14"), 2.0, "{rule}");
        assert_eq!(num(&db, "K14"), 2.0, "{rule}");
    })
    .await;
}
