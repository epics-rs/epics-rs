//! R18-93 / R18-94: `processTarget` and the gates that reach it.
//!
//! C `processTarget` (dbDbLink.c:474-528) is the ONLY link-side writer of a
//! target's `PUTF`/`RPRO`, and it is reachable from exactly two places, both of
//! which return before it for the wrong kind of target:
//!
//! * `dbScanPassive` (:427-434) — `if (pto->scan != 0) return 0;` — FLNK,
//!   fanout `LNKn`, and every `PP` output link;
//! * `dbDbPutValue` (:387-389) — `if (dbChannelField(chan) == &pdest->proc ||
//!   (pvlMask & pvlOptPP && pdest->scan == 0))`. Note the `.PROC` arm carries
//!   NO scan test: a DB link writing `TARGET.PROC` processes the target on ANY
//!   scan.
//!
//! The port applied the Passive test to the *process* call only and wrote
//! PUTF/RPRO above it (R18-93), and it required Passive on the `.PROC` arm too
//! (R18-94).
//!
//! softIoc 7.0.10.1-DEV — `TRIG` (calcout, `FLNK="ASY"`), `ASY` (calcout,
//! `SCAN="1 second"`, `ODLY=3`, so it is PACT for most of every second):
//!
//! ```text
//! epics> dbgf ASY.PACT      DBF_UCHAR: 1     <- genuinely busy
//! epics> dbpf TRIG.PROC 1   DBF_UCHAR: 1     <- TRIG.PUTF = 1, FLNK fires
//! epics> dbgf ASY.PACT      DBF_UCHAR: 1
//! epics> dbgf ASY.RPRO      DBF_UCHAR: 0     <- NOT set: ASY is not Passive
//! epics> dbgf ASY.PUTF      DBF_UCHAR: 0
//! ```

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// `TRIG`, whose FLNK names `ASY` — the C probe's database. `scan` is `ASY`'s
/// SCAN field, the whole point of the gate.
async fn build(scan: &str) -> Arc<PvDatabase> {
    let db_text = format!(
        r#"
record(calcout, "TRIG") {{
    field(CALC, "1")
    field(FLNK, "ASY")
}}
record(ai, "ASY") {{
    field(SCAN, "{scan}")
}}
"#
    );
    let (db, _) = IocBuilder::new()
        .db_string(&db_text, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    // C's ODLY window: the target is genuinely PACT when the FLNK fires.
    let asy = db.get_record("ASY").unwrap();
    asy.write().enter_pact();
    db
}

async fn flags(db: &PvDatabase, rec: &str) -> (bool, bool) {
    let inst = db.get_record(rec).unwrap();
    let inst = inst.read();
    (inst.common.rpro != 0, inst.common.putf)
}

/// R18-93: an FLNK to a BUSY, non-Passive target must not be touched at all.
/// `dbScanPassive` returns above `processTarget`, so no PUTF, no RPRO — and so
/// no extra unscheduled cycle when the target's async completes.
#[epics_macros_rs::epics_test]
async fn flnk_to_busy_non_passive_target_sets_no_rpro() {
    let db = build("1 second").await;

    // A `dbPutField` on `.PROC` — the source's PUTF is 1, which is what made
    // the pre-fix code take the `rpro = true` arm on the target.
    db.put_record_field_from_ca("TRIG", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();

    let (rpro, putf) = flags(&db, "ASY").await;
    assert!(
        !rpro,
        "ASY is not Passive: dbScanPassive returns above processTarget, so RPRO stays 0"
    );
    assert!(!putf, "and PUTF is not propagated to it either");
}

/// The Passive half of the same gate still works: a busy PASSIVE target does
/// get `RPRO = 1` (C `processTarget`'s `else if (psrc->putf && claim_dst)`).
#[epics_macros_rs::epics_test]
async fn flnk_to_busy_passive_target_still_sets_rpro() {
    let db = build("Passive").await;

    db.put_record_field_from_ca("TRIG", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();

    let (rpro, putf) = flags(&db, "ASY").await;
    assert!(rpro, "a busy PASSIVE target is marked for reprocessing");
    assert!(!putf, "and its PUTF is cleared, as C does");
}

/// R18-94: the OTHER gate. A DB link writing `TARGET.PROC` carries no scan test
/// (dbDbLink.c:387), so it processes the target on ANY scan — the arm the port
/// had ANDed with Passive, while its own CA put route already honoured it.
///
/// softIoc — `SRCO.OUT="TGT.PROC"`, `TGT` on `SCAN="10 second"`:
///
/// ```text
/// epics> dbgf TGT.VAL      DBF_DOUBLE: 0
/// epics> dbpf SRCO.PROC 1
/// epics> dbgf TGT.VAL      DBF_DOUBLE: 1
/// epics> dbpf SRCO.PROC 1
/// epics> dbgf TGT.VAL      DBF_DOUBLE: 2
/// ```
#[epics_macros_rs::epics_test]
async fn db_link_to_proc_field_processes_a_non_passive_target() {
    const DB: &str = r#"
record(calcout, "SRCO") {
    field(CALC, "1")
    field(OUT, "TGT.PROC")
}
record(calc, "TGT") {
    field(SCAN, "10 second")
    field(CALC, "VAL+1")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    let val = async |db: &PvDatabase| -> f64 {
        let inst = db.get_record("TGT").unwrap();
        let inst = inst.read();
        match inst.record.get_field("VAL") {
            Some(EpicsValue::Double(v)) => v,
            other => panic!("TGT.VAL: {other:?}"),
        }
    };

    assert_eq!(val(&db).await, 0.0);
    db.put_record_field_from_ca("SRCO", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    assert_eq!(
        val(&db).await,
        1.0,
        "a .PROC write processes a 10s-scan target"
    );
    db.put_record_field_from_ca("SRCO", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    assert_eq!(val(&db).await, 2.0, "and again on the next write");
}

/// The `.PROC` gate is the only one that ignores SCAN: an ordinary `PP` OUT
/// link to the same non-Passive target still does NOT process it
/// (dbDbLink.c:388 — `pvlOptPP && pdest->scan == 0`).
#[epics_macros_rs::epics_test]
async fn pp_link_to_a_non_passive_target_still_does_not_process_it() {
    const DB: &str = r#"
record(calcout, "SRCV") {
    field(CALC, "1")
    field(OUT, "TGT.A PP")
}
record(calc, "TGT") {
    field(SCAN, "10 second")
    field(CALC, "A+1")
}
"#;
    let (db, _) = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    db.put_record_field_from_ca("SRCV", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();

    let inst = db.get_record("TGT").unwrap();
    let inst = inst.read();
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Double(0.0)),
        "the PP arm keeps its scan test: a non-Passive target is not processed"
    );
}
