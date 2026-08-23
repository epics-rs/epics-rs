//! A periodic scan list is ordered record-type-major, then by `.db` load order
//! — not by `.db` load order alone.
//!
//! C reference. `buildScanLists` (`dbScan.c:1054-1076`) is a nested walk:
//!
//! ```c
//!     for (pdbRecordType = ellFirst(&pdbbase->recordTypeList); ...)     /* OUTER */
//!         for (pdbRecordNode = ellFirst(&pdbRecordType->recList); ...)  /* INNER */
//!             scanAdd(precord);
//! ```
//!
//! so at `iocInit` every `ai` is `scanAdd`ed before every `calc`, whatever
//! order the `.db` declared them in. `addToList` (`:1085-1091`) then appends
//! after the last element whose `phas <=` the new record's, which preserves
//! that feed order within a PHAS. Record-type order is DBD include order
//! (`dbd/stdRecords.dbd`, `aiRecord.dbd` before `calcRecord.dbd`).
//!
//! The port keyed the list `(PHAS, load_order, name)` — the FIFO was stable,
//! but over the wrong feed order, so every same-PHAS reader/writer pair whose
//! declaration order contradicts DBD order was inverted by exactly one scan
//! cycle.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{
    FieldDesc, ProcessOutcome, Record, ScanType, dbd_generated::RECORD_TYPE_ORDER,
};
use epics_base_rs::types::EpicsValue;

async fn order(db: &str) -> Vec<String> {
    IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
        .records_for_scan(ScanType::Sec1)
        .await
}

/// BOUNDARY: same PHAS, declaration order contradicts DBD order. The brief's
/// case — `ai` reads `calc`, `calc` is declared first, and C still scans the
/// `ai` first because `aiRecord.dbd` precedes `calcRecord.dbd`.
#[epics_macros_rs::epics_test]
async fn record_type_order_beats_declaration_order() {
    let got = order(
        r#"
record(calc, "B") { field(SCAN,"1 second") field(CALC,"B+1") }
record(ai,   "A") { field(SCAN,"1 second") field(DTYP,"Soft Channel")
                    field(INP,"B.VAL NPP NMS") }
"#,
    )
    .await;

    assert_eq!(
        got,
        vec!["A".to_string(), "B".to_string()],
        "ai is fed to scanAdd before calc, so A reads the PREVIOUS cycle's B"
    );
}

/// BOUNDARY: PHAS still outranks the record type. C's `addToList` keys on
/// `phas` first and only then falls back to feed order.
#[epics_macros_rs::epics_test]
async fn phas_still_outranks_the_record_type() {
    let got = order(
        r#"
record(calc, "B") { field(SCAN,"1 second") field(PHAS,"0") field(CALC,"B+1") }
record(ai,   "A") { field(SCAN,"1 second") field(PHAS,"1") }
"#,
    )
    .await;

    assert_eq!(got, vec!["B".to_string(), "A".to_string()]);
}

/// BOUNDARY: within one record type the inner walk is `.db` load order, and it
/// is NOT the record name — two `ai` records declared Z then A scan Z first.
#[epics_macros_rs::epics_test]
async fn load_order_breaks_a_tie_inside_one_record_type() {
    let got = order(
        r#"
record(ai, "Z") { field(SCAN,"1 second") }
record(ai, "A") { field(SCAN,"1 second") }
"#,
    )
    .await;

    assert_eq!(got, vec!["Z".to_string(), "A".to_string()]);
}

/// BOUNDARY: a record type no vendored `.dbd` declares. C reaches it through a
/// module `.dbd` included after `base.dbd`, so it joins `recordTypeList`
/// behind every base type — declaring it first must not move it first.
#[epics_macros_rs::epics_test]
async fn an_undeclared_record_type_sorts_after_every_declared_one() {
    struct Foreign;
    impl Record for Foreign {
        fn record_type(&self) -> &'static str {
            "scan_order_foreign_test"
        }
        fn process(&mut self) -> CaResult<ProcessOutcome> {
            Ok(ProcessOutcome::complete())
        }
        fn get_field(&self, _name: &str) -> Option<EpicsValue> {
            None
        }
        fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
            Err(epics_base_rs::error::CaError::FieldNotFound(name.into()))
        }
        fn declared_fields(&self) -> &'static [FieldDesc] {
            &[]
        }
    }

    assert!(
        !RECORD_TYPE_ORDER.contains(&"scan_order_foreign_test"),
        "the test double must be a type no vendored .dbd declares"
    );

    let db: Arc<PvDatabase> = Arc::new(PvDatabase::new());
    db.add_record("FIRST", Box::new(Foreign)).await.unwrap();
    db.add_record(
        "SECOND",
        Box::new(epics_base_rs::server::records::ai::AiRecord::new(0.0)),
    )
    .await
    .unwrap();
    for name in ["FIRST", "SECOND"] {
        db.get_record(name).unwrap().write().common.scan = ScanType::Sec1;
        db.update_scan_index(name, ScanType::Passive, ScanType::Sec1, 0, 0);
    }

    assert_eq!(
        db.records_for_scan(ScanType::Sec1).await,
        vec!["SECOND".to_string(), "FIRST".to_string()],
        "an undeclared type sorts behind every declared one, as in C"
    );
}
