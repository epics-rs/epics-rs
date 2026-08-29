//! R8-3 — a `DBR_STRING` put into a `DBF_MENU` field goes through C's
//! `putStringMenu` (`dbConvert.c:1206-1229`) and nothing else:
//!
//! ```c
//! for (i = 0; i < nChoice; i++)
//!     if (strcmp(pbuffer, papChoiceSet[i]) == 0) { *(epicsEnum16*)pfield = i; return 0; }
//! status = epicsParseUInt16(pbuffer, &val, dbConvertBase, NULL);
//! if (status || val >= nChoice) return S_db_badChoice;
//! ```
//!
//! Exact `strcmp` — no trim, no case folding — then a base-0 index that must be
//! BELOW `nChoice`. Anything else is `S_db_badChoice`, returned from inside
//! `dbPut` *before* the value is stored (`dbAccess.c:1357` `if (status) goto
//! done`), so the field keeps its previous value, no monitor is posted, and the
//! record is not processed. rsrv answers a put-callback with ECA_PUTFAIL
//! (`db_access.c:1041` → `camessage.c:1386`).
//!
//! The port used to trim the label, accept any parsable index, and — worst —
//! fall through to `EpicsValue::convert_to` on a miss, where an unrecognised
//! string became `to_f64().unwrap_or(0.0) as u16`, i.e. **menu index 0**. So
//! `caput FAN.SELM Bogus` silently selected `All`.

use epics_base_rs::error::{CaError, CaOp};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::fanout::FanoutRecord;
use epics_base_rs::types::EpicsValue;

/// `defmsg(CA_K_WARNING, 20)` — what rsrv sends for any failed `dbPut`.
const ECA_PUTFAIL: u32 = 160;

async fn fanout_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("FAN", Box::new(FanoutRecord::default()))
        .await
        .unwrap();
    db
}

async fn selm(db: &PvDatabase) -> i16 {
    match db.get_pv("FAN.SELM").unwrap() {
        EpicsValue::Short(v) => v,
        other => panic!("SELM read back as {other:?}"),
    }
}

/// `menu(fanoutSELM)` = All(0) / Specified(1) / Mask(2) — `nChoice` is 3.
#[epics_macros_rs::epics_test]
async fn out_of_menu_index_is_bad_choice_and_stores_nothing() {
    let db = fanout_db().await;
    db.put_record_field_from_ca("FAN", "SELM", EpicsValue::String("Mask".into()))
        .await
        .unwrap();
    assert_eq!(selm(&db).await, 2);

    for out_of_range in ["3", "99", "65535"] {
        let err = db
            .put_record_field_from_ca("FAN", "SELM", EpicsValue::String(out_of_range.into()))
            .await
            .expect_err("C: val >= nChoice → S_db_badChoice");
        assert!(matches!(err, CaError::BadChoice(_)), "got {err:?}");
        assert_eq!(err.to_eca_status(CaOp::Write), ECA_PUTFAIL);
        // C returns before `dbFastPutConvertRoutine` ever runs.
        assert_eq!(selm(&db).await, 2, "a refused put must not touch the field");
    }
}

/// The pre-fix fallback's real damage: an unrecognised string was coerced by
/// the field-blind `convert_to`, landing as index 0 (`All`) with a SUCCESS
/// status. C stores nothing and fails the put.
#[epics_macros_rs::epics_test]
async fn unknown_label_fails_instead_of_collapsing_to_index_zero() {
    let db = fanout_db().await;
    db.put_record_field_from_ca("FAN", "SELM", EpicsValue::String("Mask".into()))
        .await
        .unwrap();

    let err = db
        .put_record_field_from_ca("FAN", "SELM", EpicsValue::String("Bogus".into()))
        .await
        .expect_err("no choice, no index → S_db_badChoice");
    assert!(matches!(err, CaError::BadChoice(_)), "got {err:?}");
    assert_eq!(selm(&db).await, 2);
}

/// C matches with `strcmp`: no trimming, no case folding.
#[epics_macros_rs::epics_test]
async fn label_match_is_exact_strcmp() {
    let db = fanout_db().await;

    for inexact in [" Specified", "Specified ", "specified", "SPECIFIED"] {
        let err = db
            .put_record_field_from_ca("FAN", "SELM", EpicsValue::String(inexact.into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaError::BadChoice(_)),
            "{inexact:?} must not match `Specified`; got {err:?}"
        );
        assert_eq!(selm(&db).await, 0);
    }

    db.put_record_field_from_ca("FAN", "SELM", EpicsValue::String("Specified".into()))
        .await
        .expect("the exact label is the one thing strcmp accepts");
    assert_eq!(selm(&db).await, 1);
}

/// `epicsParseUInt16(pbuffer, &val, dbConvertBase, NULL)` — `dbConvertBase` is
/// 0 (`epicsConvert.c:37`), so `strtoul` base 0 applies: whitespace around the
/// digits is fine, `0x`/leading-`0` change the radix.
#[epics_macros_rs::epics_test]
async fn in_range_index_is_parsed_the_way_epics_parse_uint16_parses_it() {
    let db = fanout_db().await;

    for (text, expect) in [("1", 1i16), (" 2 ", 2), ("0x2", 2), ("02", 2), ("+1", 1)] {
        db.put_record_field_from_ca("FAN", "SELM", EpicsValue::String(text.into()))
            .await
            .unwrap_or_else(|e| panic!("{text:?} is a valid index in C: {e}"));
        assert_eq!(selm(&db).await, expect, "for {text:?}");
    }

    // `S_stdlib_extraneous` — a trailing non-space character is not a number.
    let err = db
        .put_record_field_from_ca("FAN", "SELM", EpicsValue::String("1x".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, CaError::BadChoice(_)), "got {err:?}");
}

/// The same converter owns the *shared* menu common fields — `PRIO` is
/// `menu(menuPriority)` (LOW/MEDIUM/HIGH), reached through
/// `RecordInstance::put_common_field`, which used to hand a miss to
/// `EpicsValue::parse` and drop it.
#[epics_macros_rs::epics_test]
async fn shared_menu_common_field_uses_the_same_converter() {
    let db = fanout_db().await;

    db.put_record_field_from_ca("FAN", "PRIO", EpicsValue::String("HIGH".into()))
        .await
        .unwrap();
    assert_eq!(db.get_pv("FAN.PRIO").unwrap(), EpicsValue::Short(2));

    for bad in ["High", "3", "Bogus"] {
        let err = db
            .put_record_field_from_ca("FAN", "PRIO", EpicsValue::String(bad.into()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CaError::BadChoice(_)),
            "{bad:?} must be S_db_badChoice; got {err:?}"
        );
        assert_eq!(db.get_pv("FAN.PRIO").unwrap(), EpicsValue::Short(2));
    }
}
