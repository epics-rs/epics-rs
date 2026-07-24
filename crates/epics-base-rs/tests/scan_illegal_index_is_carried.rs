//! An out-of-menu `SCAN` index is a value C stores; it is not "Passive".
//!
//! `dbPut` writes the `epicsEnum16` the client sent and only THEN calls
//! `scanAdd`, which decides membership:
//!
//! ```c
//! /* dbScan.c:241-251 */
//! scan = precord->scan;
//! if (scan == menuScanPassive) return;
//! if (scan < 0 || scan >= nPeriodic + SCAN_1ST_PERIODIC) {
//!     recGblRecordError(-1, precord, "scanAdd detected illegal SCAN value");
//! } else if (scan == menuScanEvent) { ... }
//! ```
//!
//! So an illegal index is STORED and scanned by NOTHING. The port modelled SCAN
//! as the nine legal choices alone, so `ScanType::from_u16` erased any index
//! ≥ 10 to `Passive`: the field read back `0` instead of `10`, and the record
//! became put-processable (C tests `precord->scan == 0` literally in
//! `dbPutField`, dbAccess.c:1263).
//!
//! # Ground truth
//!
//! Measured on the compiled softIoc (`/home/stevek/work/epics-base`), record
//! `record(ai,"T:A"){ field(SCAN,"1 second") }`:
//!
//! ```text
//! caput T:A.SCAN 10   -> New : T:A.SCAN 10      caget -n T:A.SCAN -> 10
//! caput T:A.SCAN 9    -> New : T:A.SCAN .1 second               -> 9
//! caput T:A.SSCN 10   -> New : T:A.SSCN 10      caget -n T:A.SSCN -> 10
//! ioc log: "recGblRecordError: scanAdd detected illegal SCAN value  PV: T:A"
//! ```
//!
//! (A *string* put of `"10"` is a different converter — `putStringMenu` bounds
//! the index by `nChoice` and answers `S_db_badChoice`; that boundary is pinned
//! in `menu_common_field_scan_pini.rs`. `caput` sends a numeric put for an enum
//! channel whose text matches no choice, which is the row measured above.)

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::ScanType;
use epics_base_rs::types::EpicsValue;

async fn db_with_scanned_ai() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record(
        "REC",
        Box::new(epics_base_rs::server::records::ai::AiRecord::default()),
    )
    .await
    .unwrap();
    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::Enum(6))
        .await
        .unwrap();
    assert_eq!(db.records_for_scan(ScanType::Sec1).await, vec!["REC"]);
    db
}

async fn scan_index(db: &PvDatabase) -> u16 {
    match db.get_pv("REC.SCAN").unwrap() {
        EpicsValue::Enum(v) => v,
        other => panic!("SCAN is a DBF_MENU: {other:?}"),
    }
}

/// Every scan list the IOC drives. If a record is in none of them, nothing
/// scans it — which is what `scanAdd` leaves an illegal SCAN in.
async fn scanned_anywhere(db: &PvDatabase, name: &str) -> bool {
    for scan in [
        ScanType::Event,
        ScanType::IoIntr,
        ScanType::Sec10,
        ScanType::Sec5,
        ScanType::Sec2,
        ScanType::Sec1,
        ScanType::Sec05,
        ScanType::Sec02,
        ScanType::Sec01,
    ] {
        if db.records_for_scan(scan).await.iter().any(|n| n == name) {
            return true;
        }
    }
    false
}

/// The menu boundary: 9 is the last choice, 10 is the first illegal index.
#[epics_macros_rs::epics_test]
async fn the_last_menu_choice_scans_and_the_first_illegal_index_does_not() {
    let db = db_with_scanned_ai().await;

    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::Enum(9))
        .await
        .unwrap();
    assert_eq!(scan_index(&db).await, 9);
    assert_eq!(db.records_for_scan(ScanType::Sec01).await, vec!["REC"]);

    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::Enum(10))
        .await
        .expect("C stores the index it was sent");
    assert_eq!(scan_index(&db).await, 10, "the written index reads back");
    assert!(
        !scanned_anywhere(&db, "REC").await,
        "scanAdd puts an illegal SCAN in no list"
    );
}

/// `-1` reaches the field as the `epicsEnum16` 65535 — still stored, still
/// scanned by nothing, and still not `Passive`.
#[epics_macros_rs::epics_test]
async fn the_top_of_the_enum_is_stored_and_is_not_passive() {
    let db = db_with_scanned_ai().await;

    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::Enum(65535))
        .await
        .unwrap();
    assert_eq!(scan_index(&db).await, 65535);
    assert!(!scanned_anywhere(&db, "REC").await);

    let rec = db.get_record("REC").unwrap();
    assert_ne!(
        rec.read().common.scan,
        ScanType::Passive,
        "an illegal SCAN is not Passive: C's `dbPutField` scan==0 test \
         (dbAccess.c:1263) is literal, so the record is NOT put-processable"
    );
}

/// A legal index put back after an illegal one re-joins its list — the illegal
/// state is carried, not sticky.
#[epics_macros_rs::epics_test]
async fn an_illegal_index_is_left_behind_when_a_legal_one_is_written() {
    let db = db_with_scanned_ai().await;

    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::Enum(42))
        .await
        .unwrap();
    assert_eq!(scan_index(&db).await, 42);
    assert!(!scanned_anywhere(&db, "REC").await);

    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::Enum(3))
        .await
        .unwrap();
    assert_eq!(scan_index(&db).await, 3);
    assert_eq!(db.records_for_scan(ScanType::Sec10).await, vec!["REC"]);
}

/// SSCN is the same menu, so it carries an illegal index too — and 10 is NOT
/// the 65535 "unset" sentinel that `recGblCheckSimm` bails on.
#[epics_macros_rs::epics_test]
async fn sscn_carries_an_illegal_index_and_only_65535_is_the_sentinel() {
    let db = db_with_scanned_ai().await;

    db.put_record_field_from_ca("REC", "SSCN", EpicsValue::Enum(10))
        .await
        .unwrap();
    let EpicsValue::Enum(v) = db.get_pv("REC.SSCN").unwrap() else {
        panic!("SSCN is a DBF_MENU")
    };
    assert_eq!(v, 10);

    let rec = db.get_record("REC").unwrap();
    assert!(
        !rec.read().common.sscn.is_unset(),
        "recGbl's simulation helpers test `*psscn == USHRT_MAX` and nothing else"
    );
}
