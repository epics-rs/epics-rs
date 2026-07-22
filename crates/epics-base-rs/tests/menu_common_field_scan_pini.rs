//! R8-4 — `SCAN`, `SSCN` and `PINI` are `DBF_MENU` fields like any other, so a
//! string put goes through the SAME converter (`dbConvert.c::putStringMenu`)
//! against the SAME menu table (`menuScan.dbd`, `menuPini.dbd`).
//!
//! Each used to carry a hand-written `from_str` that had drifted from C:
//!
//! * `ScanType::from_str` lower-cased the input and invented aliases —
//!   `"0.5 second"`, `"0.2 second"`, `"0.1 second"`, `"iointr"` — for menuScan
//!   choices whose only C spellings are `".5 second"`, `".2 second"`,
//!   `".1 second"`, `"I/O Intr"`. It also fed any parsable index through
//!   `ScanType::from_u16`, which maps everything out of 0-9 to `Passive`, so
//!   `caput REC.SCAN 42` silently made the record Passive where C returns
//!   `S_db_badChoice`.
//! * `SimModeScan::from_str` accepted any `u16`.
//! * `PiniMode::from_str` trimmed.
//!
//! C has exactly one converter for all of them and it does none of that. The
//! one place the two C converters legitimately differ is the out-of-menu bound
//! (`putStringMenu` vs the loader's `dbPutStringNum`), which is what lets
//! `field(SSCN,"65535")` — menuScan's out-of-range "use SCAN" sentinel — load
//! from a `.db` while `caput REC.SSCN 65535` is refused at runtime.

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the feature-ON suite.

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{RecordInstance, ScanType, SimModeScan};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

async fn ai_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(AiRecord::default()))
        .await
        .unwrap();
    db
}

async fn scan_of(db: &PvDatabase) -> ScanType {
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    inst.common.scan
}

/// menuScan's periodic choices are spelled `".5 second"` — with no leading
/// zero. The `"0.5 second"` alias is not a menuScan choice in any C release.
#[tokio::test]
async fn invented_scan_aliases_are_rejected() {
    let db = ai_db().await;

    for alias in ["0.5 second", "0.2 second", "0.1 second", "iointr"] {
        let err = db
            .put_record_field_from_ca("REC", "SCAN", EpicsValue::String(alias.into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaError::BadChoice(_)),
            "{alias:?} is not a menuScan choice; got {err:?}"
        );
        assert_eq!(scan_of(&db).await, ScanType::Passive);
    }
}

#[tokio::test]
async fn canonical_scan_labels_and_indices_still_resolve() {
    let db = ai_db().await;

    for (label, expect) in [
        (".5 second", ScanType::Sec05),
        (".2 second", ScanType::Sec02),
        (".1 second", ScanType::Sec01),
        ("I/O Intr", ScanType::IoIntr),
        ("10 second", ScanType::Sec10),
        ("Passive", ScanType::Passive),
    ] {
        db.put_record_field_from_ca("REC", "SCAN", EpicsValue::String(label.into()))
            .await
            .unwrap_or_else(|e| panic!("{label:?} is a menuScan choice: {e}"));
        assert_eq!(scan_of(&db).await, expect, "for {label:?}");
    }

    // A bare in-range index is what `epicsParseUInt16` accepts.
    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::String("6".into()))
        .await
        .unwrap();
    assert_eq!(scan_of(&db).await, ScanType::Sec1);
}

/// The case that used to end in `Passive` instead of an error: `from_u16`
/// clamps, so every out-of-menu index silently became a Passive record.
#[tokio::test]
async fn out_of_menu_scan_index_is_bad_choice_not_passive() {
    let db = ai_db().await;
    db.put_record_field_from_ca("REC", "SCAN", EpicsValue::String("1 second".into()))
        .await
        .unwrap();

    for bad in ["10", "42", "65535", "Passive ", "passive"] {
        let err = db
            .put_record_field_from_ca("REC", "SCAN", EpicsValue::String(bad.into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaError::BadChoice(_)),
            "SCAN {bad:?} must be S_db_badChoice; got {err:?}"
        );
        // The refused put leaves the scan mechanism alone — C never reaches
        // the store, let alone `scanAdd`.
        assert_eq!(scan_of(&db).await, ScanType::Sec1, "after {bad:?}");
    }
}

#[tokio::test]
async fn pini_uses_the_menu_converter() {
    let db = ai_db().await;
    let rec = db.get_record("REC").unwrap();

    db.put_record_field_from_ca("REC", "PINI", EpicsValue::String("RUN".into()))
        .await
        .unwrap();
    assert_eq!(rec.read().common.pini, 2);

    for bad in [" RUN", "run", "6", "true"] {
        let err = db
            .put_record_field_from_ca("REC", "PINI", EpicsValue::String(bad.into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaError::BadChoice(_)),
            "PINI {bad:?} must be S_db_badChoice; got {err:?}"
        );
        assert_eq!(rec.read().common.pini, 2, "after {bad:?}");
    }
}

/// The two C converters differ at exactly one place that matters in practice:
/// SSCN's `65535` sentinel. The loader (`dbPutStringNum`) takes it; a runtime
/// `dbPut` (`putStringMenu`, bound `val < nChoice`) does not.
#[tokio::test]
async fn sscn_sentinel_loads_from_db_but_is_refused_at_runtime() {
    let mut inst = RecordInstance::new("SIM".to_string(), AiRecord::default());
    inst.put_common_field_db_load("SSCN", EpicsValue::String("65535".into()))
        .expect("field(SSCN,\"65535\") is what the dbd initial() is");
    assert!(inst.common.sscn.is_unset());
    assert_eq!(inst.common.sscn.to_u16(), SimModeScan::DO_NOT_USE);

    inst.put_common_field_db_load("SSCN", EpicsValue::String("1 second".into()))
        .unwrap();
    assert_eq!(inst.common.sscn, SimModeScan::from_scan(ScanType::Sec1));

    let err = inst
        .put_common_field("SSCN", EpicsValue::String("65535".into()))
        .expect_err("putStringMenu bounds the index by nChoice");
    assert!(matches!(err, CaError::BadChoice(_)), "got {err:?}");
    assert_eq!(inst.common.sscn, SimModeScan::from_scan(ScanType::Sec1));
}
