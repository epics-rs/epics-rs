//! Boundary tests for the `sseq` `DOLn` value path — a string-class `DOLn`
//! source must reach `LNKn` byte-exact, not collapse through the numeric
//! `DOn` slot.
//!
//! C `sseqRecord.c::processCallback` (sseqRecord.c:643-705) reads `DOLn`
//! typed by `dol_field_type`: a `DBF_STRING` source is read with `DBR_STRING`
//! into `s`/`STRn` and forwarded to `LNKn` with `DBR_STRING`
//! (sseqRecord.c:714-756); a numeric source is read with `DBR_DOUBLE` into
//! `dov`/`DOn`. The Rust port carries the value through `ProcessAction::
//! ReadDbLink`, which delivers the link target's NATIVE `EpicsValue`; `sseq`
//! preserves a string in `STRn` (byte-exact) instead of coercing to `DOn`.

use std::collections::HashSet;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::{EpicsValue, PvString};

async fn poll_field(db: &PvDatabase, record: &str, field: &str, label: &str) -> EpicsValue {
    for _ in 0..400 {
        if let Some(rec) = db.get_record(record) {
            let v = rec.read().record.get_field(field);
            if let Some(v) = v {
                // The destination starts at its default; wait until the
                // forwarded value lands.
                let landed = match &v {
                    EpicsValue::String(s) => !s.is_empty(),
                    EpicsValue::Double(d) => *d != 0.0,
                    _ => true,
                };
                if landed {
                    return v;
                }
            }
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{label}: {record}.{field} did not receive the forwarded value before timeout");
}

/// A string-class `DOLn` source (here non-UTF-8 bytes) must reach a string
/// `LNKn` target byte-exact — no numeric coercion, no `as_str_lossy` on the
/// value path. Pre-fix the read funnelled through `DOn` (Double), so the
/// bytes were lost.
#[epics_macros_rs::epics_test]
async fn sseq_string_dol_forwards_string_byte_exact() {
    let db = PvDatabase::new();

    // Non-UTF-8 payload (≤ C's s[40]) so the test also pins that the value
    // path never round-trips through UTF-8.
    let payload = PvString::from_bytes(vec![0xff, 0xfe, b'a', b'b', 0x80]);

    let mut src = StringoutRecord::new("");
    src.put_field("VAL", EpicsValue::String(payload.clone()))
        .unwrap();
    db.add_record("SSEQ_STR_SRC", Box::new(src)).await.unwrap();

    // String destination — its VAL must end up byte-identical to the source.
    db.add_record("SSEQ_STR_DST", Box::new(StringoutRecord::new("")))
        .await
        .unwrap();

    // sseq step 1: DOL1 reads the string source, LNK1 writes the string dest.
    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    sseq.put_field("DOL1", EpicsValue::String("SSEQ_STR_SRC.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_STR_DST".into()))
        .unwrap();
    db.add_record("SSEQ_STR", Box::new(sseq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SSEQ_STR", &mut visited, 0)
        .await
        .unwrap();

    let got = poll_field(&db, "SSEQ_STR_DST", "VAL", "string DOL → string LNK").await;
    assert_eq!(
        got,
        EpicsValue::String(payload.clone()),
        "string DOL must forward the bytes exactly; got {got:?}"
    );
    // Pin byte-exactness explicitly (no replacement char, no UTF-8 round-trip).
    if let EpicsValue::String(s) = &got {
        assert_eq!(
            s.as_bytes(),
            &[0xff, 0xfe, b'a', b'b', 0x80],
            "forwarded string bytes must match the source byte-for-byte"
        );
    } else {
        panic!("expected a String, got {got:?}");
    }
}

/// A numeric `DOLn` source still forwards a `Double` to `LNKn` — the
/// pre-existing behavior must be unchanged.
#[epics_macros_rs::epics_test]
async fn sseq_numeric_dol_forwards_double_unchanged() {
    let db = PvDatabase::new();

    db.add_record("SSEQ_NUM_SRC", Box::new(AoRecord::new(42.5)))
        .await
        .unwrap();
    db.add_record("SSEQ_NUM_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    sseq.put_field("DOL1", EpicsValue::String("SSEQ_NUM_SRC.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_NUM_DST".into()))
        .unwrap();
    db.add_record("SSEQ_NUM", Box::new(sseq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SSEQ_NUM", &mut visited, 0)
        .await
        .unwrap();

    let got = poll_field(&db, "SSEQ_NUM_DST", "VAL", "numeric DOL → numeric LNK").await;
    assert_eq!(
        got,
        EpicsValue::Double(42.5),
        "numeric DOL must forward the Double unchanged; got {got:?}"
    );
}

/// After a numeric `DOLn` read, C makes `s`/`STRn` agree with `dov` via
/// `cvtDoubleToString(dov, str, prec)` and posts it (sseqRecord.c:676-679). A
/// client GET of `STRn` must return the record-PREC rendering of the numeric
/// value, not the stale string left over from before the read.
#[epics_macros_rs::epics_test]
async fn sseq_numeric_dol_refreshes_strn_with_prec() {
    let db = PvDatabase::new();

    db.add_record("SSEQ_STRN_SRC", Box::new(AoRecord::new(42.5)))
        .await
        .unwrap();
    db.add_record("SSEQ_STRN_DST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    sseq.put_field("PREC", EpicsValue::Short(3)).unwrap();
    sseq.put_field("DOL1", EpicsValue::String("SSEQ_STRN_SRC.VAL".into()))
        .unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SSEQ_STRN_DST".into()))
        .unwrap();
    db.add_record("SSEQ_STRN", Box::new(sseq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SSEQ_STRN", &mut visited, 0)
        .await
        .unwrap();

    // The LNK write lands AFTER the DOL read in the same Fire cycle, so once
    // the forward reaches DST the numeric DOL read (and STRn refresh) is done.
    let _ = poll_field(&db, "SSEQ_STRN_DST", "VAL", "numeric DOL → numeric LNK").await;

    let rec = db.get_record("SSEQ_STRN").unwrap();
    let str1 = rec.read().record.get_field("STR1");
    assert_eq!(
        str1,
        Some(EpicsValue::String("42.500".into())),
        "numeric DOL must refresh STR1 with the PREC=3 rendering of 42.5; got {str1:?}"
    );
}
