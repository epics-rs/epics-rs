//! A put-callback (`caput -c`) to a DBF link field must land the value even
//! when the record is PACT-parked.
//!
//! C `dbProcessNotify` (`dbNotify.c:337-353`) handles a put-notify to a DBF
//! link field (`DBF_INLINK`/`DBF_OUTLINK`/`DBF_FWDLINK`) as a dedicated early
//! case, ABOVE the PACT logic:
//!
//! ```c
//! /* Must handle DBF_XXXLINKs as special case.
//!  * Only dbPutField will change link fields.
//!  * Also the record is not processed as a result
//!  */
//! if (dbfType>=DBF_INLINK && dbfType<=DBF_FWDLINK) {
//!     if (ppn->requestType == putProcessRequest || ...) {
//!         ... ppn->putCallback(ppn, putFieldType);   /* writes via dbPutField */
//!     }
//!     ...
//!     ppn->doneCallback(ppn);   /* fires the callback immediately */
//!     return;                   /* never reaches the PACT test, never processes */
//! }
//! ```
//!
//! A bare `record(sub, "…") {}` has an empty `SNAM`, so C `subRecord.c:119-122`
//! sets `prec->pact = TRUE` and the record is parked forever (verified:
//! `caget SUB.PACT` reads 1). Without C's link-field special case, a put-notify
//! to such a record parks on a `PactExit` that never comes: the value is never
//! written and the callback never fires. Ground truth, C softIoc
//! (`bin/linux-x86_64`):
//!
//! ```text
//! $ caput -c SUB.INPA '0'
//! New : SUB.INPA   0        <- value written, callback returns immediately
//! $ caget -t SUB.INPA
//! 0
//! ```
//!
//! The port deferred it instead and read back "" (empty). This test drives the
//! same notify entry (`put_record_field_from_ca`, which is `NotifyRequest::New`)
//! and asserts the value lands with immediate completion on every one of sub's
//! link fields — the record-specific `INPA..INPU` and the dbCommon
//! `TSEL`/`SDIS`/`FLNK` — while a real DB link and an empty link still
//! round-trip verbatim.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(sub, "W:SUB")  {}
record(ai,  "W:OTHER") {}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn is_parked(db: &PvDatabase, rec: &str) -> bool {
    db.get_record(rec).unwrap().read().is_processing()
}

/// The put-notify entry (`caput -c`). Returns `true` when the write completed
/// synchronously (C's link-field immediate `doneCallback`), `false` when it was
/// deferred to a completion that a parked record will never deliver.
async fn put_notify(db: &PvDatabase, pv: &str, field: &str, value: &str) -> bool {
    let rx = db
        .put_record_field_from_ca(pv, field, EpicsValue::String(value.into()))
        .await
        .unwrap_or_else(|e| panic!("caput -c {pv}.{field} '{value}': {e:?}"));
    rx.is_sync()
}

async fn readback(db: &PvDatabase, pv: &str, field: &str) -> String {
    match db.get_pv(&format!("{pv}.{field}")).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv}.{field} is not a string: {other:?}"),
    }
}

const INP_FIELDS: &[&str] = &[
    "INPA", "INPB", "INPC", "INPD", "INPE", "INPF", "INPG", "INPH", "INPI", "INPJ", "INPK", "INPL",
    "INPM", "INPN", "INPO", "INPP", "INPQ", "INPR", "INPS", "INPT", "INPU",
];

/// The finding's own probe on every link field of a parked `sub`: the constant
/// string "0" is written and read back, with the callback completing
/// synchronously (not parked on the never-firing PACT exit).
#[tokio::test]
async fn constant_string_lands_on_every_parked_sub_link_field() {
    let db = build().await;
    assert!(
        is_parked(&db, "W:SUB").await,
        "a bare `sub` (empty SNAM) parks PACT=TRUE — the whole point of this test"
    );

    // dbCommon links (TSEL/SDIS/FLNK) and the 21 record-specific INPA..INPU.
    let fields: Vec<&str> = ["TSEL", "SDIS", "FLNK"]
        .into_iter()
        .chain(INP_FIELDS.iter().copied())
        .collect();

    for field in fields {
        let completed = put_notify(&db, "W:SUB", field, "0").await;
        assert!(
            completed,
            "{field}: a put-notify to a DBF link field completes immediately in C \
             (dbNotify.c:337-353), even on a PACT-parked record — it must not park"
        );
        assert_eq!(
            readback(&db, "W:SUB", field).await.as_str(),
            "0",
            "{field}: C echoes the constant link string \"0\"; the port dropped it to \"\""
        );
    }
}

/// Preservation: a real DB link string on a parked `sub` link field still
/// round-trips verbatim — the fix only removes the erroneous PACT-park, it does
/// not touch the write itself.
#[tokio::test]
async fn real_db_link_string_round_trips_on_parked_sub() {
    let db = build().await;
    assert!(is_parked(&db, "W:SUB").await);

    assert!(put_notify(&db, "W:SUB", "INPB", "W:OTHER.VAL CP").await);
    assert_eq!(
        readback(&db, "W:SUB", "INPB").await.as_str(),
        "W:OTHER.VAL CP"
    );
}

/// Preservation: an empty link string still round-trips as empty (a cleared
/// link), not spuriously kept from a prior value.
#[tokio::test]
async fn empty_link_string_round_trips_on_parked_sub() {
    let db = build().await;
    assert!(is_parked(&db, "W:SUB").await);

    // Seed a value, then clear it — the clear must land and read back empty.
    assert!(put_notify(&db, "W:SUB", "INPC", "0").await);
    assert_eq!(readback(&db, "W:SUB", "INPC").await.as_str(), "0");
    assert!(put_notify(&db, "W:SUB", "INPC", "").await);
    assert_eq!(readback(&db, "W:SUB", "INPC").await.as_str(), "");
}
