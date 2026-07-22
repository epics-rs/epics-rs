//! Family D: a histogram `CMD` put with an over-max menu ordinal must store the
//! raw ordinal, exactly as C's `special(SPC_CALC)` leaves it — NOT run the
//! Read/Clear arm and reset `cmd` to 0.
//!
//! `CMD` is `field(CMD,DBF_MENU) special(SPC_CALC) menu(histogramCMD)`
//! (histogramRecord.dbd.pod:176-182); `menu(histogramCMD)` has four choices —
//! 0=Read, 1=Clear, 2=Start, 3=Stop (histogramRecord.dbd.pod:26-31). `4` is one
//! past the last choice. A `caput CMD 4` on an enum field is delivered as a
//! `DBR_ENUM` ordinal (C `putEnumMenu`, a raw copy with no range check — not
//! `putStringMenu`, which would reject an out-of-range numeric string), so the
//! ordinal reaches `prec->cmd` verbatim before `special()` runs.
//!
//! C `histogramRecord.c::special` SPC_CALC (:246-259) is an `if / else if`
//! chain, not a catch-all:
//!
//! ```c
//! case SPC_CALC:
//!     if (prec->cmd <= 1) { clear_histogram(prec); prec->cmd = 0; }
//!     else if (prec->cmd == 2) { prec->csta = TRUE;  prec->cmd = 0; }
//!     else if (prec->cmd == 3) { prec->csta = FALSE; prec->cmd = 0; }
//!     return 0;
//! ```
//!
//! `cmd == 4` matches no branch: C does NOTHING and the raw 4 survives (the
//! oracle reads back CMD=4, STAT=UDF SEVR=INVALID — the STAT/SEVR are the fresh
//! record's undefined state, unchanged by the no-op command). The port ran a
//! catch-all `_ => clear_histogram(); cmd = 0`, so an over-max CMD read back as
//! 0/"Read" — the divergence this test pins.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::types::EpicsValue;

/// A `caput REC.CMD <ordinal>` over the CA put path the oracle drives. `caput`
/// on an enum field delivers the ordinal as a `DBR_ENUM`, which the port carries
/// as a scalar the CMD field stores natively.
async fn caput_cmd(db: &PvDatabase, ordinal: i16) -> Result<(), String> {
    db.put_record_field_from_ca("REC", "CMD", EpicsValue::Short(ordinal))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The raw stored field value (not the CA-served projection).
async fn stored(db: &PvDatabase, field: &str) -> EpicsValue {
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    inst.record.get_field(field).unwrap()
}

async fn db_with(record: HistogramRecord) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", Box::new(record)).await.unwrap();
    db
}

/// The reported case: a bare `record(histogram,"X"){}` then `caput X.CMD 4`. The
/// over-max ordinal is stored verbatim; the command switch has no matching case,
/// so counting state and buckets are untouched.
#[tokio::test]
async fn cmd_over_max_ordinal_stores_raw() {
    let db = db_with(HistogramRecord::default()).await;
    caput_cmd(&db, 4).await.unwrap();
    assert_eq!(
        stored(&db, "CMD").await,
        EpicsValue::Short(4),
        "out-of-range CMD survives verbatim (C special() matches no branch)"
    );
    // The record still counts by default (CSTA defaults TRUE) — a no-op command
    // does not touch CSTA, unlike the Start/Stop arms.
    assert_eq!(
        stored(&db, "CSTA").await,
        EpicsValue::Short(1),
        "over-max CMD leaves CSTA unchanged"
    );
}

/// The in-range commands are NOT regressed: each valid ordinal executes its arm
/// and C resets `cmd` back to 0 afterwards.
#[tokio::test]
async fn cmd_in_range_commands_execute_and_reset_to_zero() {
    let db = db_with(HistogramRecord::new(2, 0.0, 10.0)).await;

    // 3 = Stop → csta FALSE, cmd reset to 0.
    caput_cmd(&db, 3).await.unwrap();
    assert_eq!(
        stored(&db, "CSTA").await,
        EpicsValue::Short(0),
        "Stop clears CSTA"
    );
    assert_eq!(
        stored(&db, "CMD").await,
        EpicsValue::Short(0),
        "Stop resets CMD to 0"
    );

    // 2 = Start → csta TRUE, cmd reset to 0.
    caput_cmd(&db, 2).await.unwrap();
    assert_eq!(
        stored(&db, "CSTA").await,
        EpicsValue::Short(1),
        "Start sets CSTA"
    );
    assert_eq!(
        stored(&db, "CMD").await,
        EpicsValue::Short(0),
        "Start resets CMD to 0"
    );

    // Populate a bucket while counting (CSTA TRUE after Start): a SGNL caput is
    // C's SPC_MOD add_count. SGNL 1.0 lands in bucket 0 of the [0,10) 2-bin range.
    db.put_record_field_from_ca("REC", "SGNL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(
        stored(&db, "VAL").await,
        EpicsValue::ULongArray(vec![1, 0]),
        "SGNL caput counts into bucket 0 while CSTA is TRUE"
    );

    // 1 = Clear → zero buckets, cmd reset to 0.
    caput_cmd(&db, 1).await.unwrap();
    assert_eq!(
        stored(&db, "VAL").await,
        EpicsValue::ULongArray(vec![0, 0]),
        "Clear zeros the buckets"
    );
    assert_eq!(
        stored(&db, "CMD").await,
        EpicsValue::Short(0),
        "Clear resets CMD to 0"
    );
}
