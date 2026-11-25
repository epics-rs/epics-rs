//! A link to a runtime-typed field (`mbbo.VAL`, `waveform.VAL`) names a LOCAL
//! target, and its type is the one C's `cvt_dbaddr` gives the address.
//!
//! `PvDatabase::link_target_field_type` answered `None` for such a field
//! because its `.dbd` row carries no type, so every link-status classifier
//! reported `Ext PV NC` and sseq's `DTn`/`LTn` read `DBF_unknown`.
//!
//! Measured on oracleIoc (EPICS 7.0.10 + calc R3-7-5) with this database:
//! every status reads `Local PV`, and sseq answers `DT1 11` (mbbo with
//! states, `DBF_ENUM`), `DT2 1` (`FTVL=CHAR`), `DT3 10` (ai), `DT4 4` (mbbo
//! with no states, `DBF_USHORT`) and `LT1 11`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::sseq::SseqRecord;

const LOC: i16 = 2;

const DB: &str = r#"
record(mbbo, "RT_M") { field(ZRST, "a") field(ONST, "b") }
record(mbbo, "RT_MN") {}
record(waveform, "RT_W") { field(FTVL, "CHAR") field(NELM, "8") }
record(ai, "RT_A") {}
record(calcout, "RT_C") {
    field(INPA, "RT_M") field(INPB, "RT_W") field(INPC, "RT_A") field(INPD, "RT_MN")
    field(CALC, "A")
}
record(scalcout, "RT_S") {
    field(INPA, "RT_M NPP") field(INPB, "RT_W") field(INPC, "RT_A") field(CALC, "A")
}
record(sseq, "RT_Q") {
    field(DOL1, "RT_M") field(DOL2, "RT_W") field(DOL3, "RT_A") field(DOL4, "RT_MN")
    field(LNK1, "RT_M")
}
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn read(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv)
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
}

/// The classification is spawned at iocInit; wait for the control (the ai
/// target) to settle before reading the rest.
async fn settled(db: &PvDatabase, pv: &str, want: i16) {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if db.get_pv(pv).ok().and_then(|v| v.to_f64()) == Some(f64::from(want)) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{pv} never reached {want}: {:?}", db.get_pv(pv));
}

#[epics_macros_rs::epics_test]
async fn a_link_to_a_runtime_typed_field_is_local() {
    let db = build().await;
    settled(&db, "RT_C.INCV", LOC).await;
    settled(&db, "RT_S.INCV", LOC).await;
    settled(&db, "RT_Q.DOL3V", LOC).await;
    for pv in [
        "RT_C.INAV",
        "RT_C.INBV",
        "RT_C.INDV",
        "RT_S.INAV",
        "RT_S.INBV",
        "RT_Q.DOL1V",
        "RT_Q.DOL2V",
        "RT_Q.DOL4V",
        "RT_Q.LNK1V",
    ] {
        assert_eq!(read(&db, pv), LOC, "{pv}");
    }
}

#[epics_macros_rs::epics_test]
async fn a_runtime_typed_target_reports_its_address_type() {
    let db = build().await;
    settled(&db, "RT_Q.DT3", 10).await;
    for (pv, want) in [
        ("RT_Q.DT1", 11),
        ("RT_Q.DT2", 1),
        ("RT_Q.DT4", 4),
        ("RT_Q.LT1", 11),
    ] {
        assert_eq!(read(&db, pv), want, "{pv}");
    }
}
