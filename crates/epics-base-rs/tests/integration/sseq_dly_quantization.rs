//! R9-70 — sseq rounds DLYn to a whole OS clock tick.
//!
//! Two C sites, with different scopes:
//!
//! * `sseqRecord.c::init_record` (198-200) rounds EVERY `DLYn`:
//!
//!   ```c
//!   for (index = 0; index < NUM_LINKS; index++, plinkGroup++) {
//!       plinkGroup->dly = epicsThreadSleepQuantum() *
//!           NINT(plinkGroup->dly/epicsThreadSleepQuantum());
//!       db_post_events(pR, &plinkGroup->dly, DBE_VALUE);
//!   ```
//!
//! * `sseqRecord.c::special` (1140-1156), on a put to a `DLYn`, rounds it and
//!   posts it with `DBE_VALUE`. C means the written `DLYn` but rounds and
//!   posts DLY1, because it never adds `lnkIndex` to `plinkGroup` as the
//!   `STRn` case above it does; the port rounds and posts the `DLYn` written.
//!
//! The port stored and used the raw value at both sites, so a `DLY3=0.003`
//! waited 3 ms where C waits 0, and every DLYn read back unrounded.
//!
//! `epicsThreadSleepQuantum()` is `1/sysconf(_SC_CLK_TCK)` = 0.01 s on Linux
//! and macOS, and `NINT(f) = (long)(f > 0 ? f+0.5 : f-0.5)` (sseqRecord.c:67).

use epics_base_rs::runtime::time::thread_sleep_quantum;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

fn dly(rec: &SseqRecord, n: &str) -> f64 {
    rec.get_field(&format!("DLY{n}"))
        .and_then(|v| v.to_f64())
        .unwrap()
}

/// C `NINT(x/q)*q`, computed independently of the implementation under test.
fn expect_quantized(seconds: f64) -> f64 {
    let q = thread_sleep_quantum();
    let ticks = seconds / q;
    q * (ticks + 0.5).trunc()
}

#[epics_macros_rs::epics_test]
async fn r9_70_init_rounds_every_dly_to_a_clock_tick() {
    let mut rec = SseqRecord::default();
    // Raw db-file values: put_field alone stores them verbatim (C `dbPut` at
    // load time does the same — `special` has not run yet).
    rec.put_field("DLY1", EpicsValue::Double(0.017)).unwrap();
    rec.put_field("DLY3", EpicsValue::Double(0.003)).unwrap();
    rec.put_field("DLY7", EpicsValue::Double(1.234)).unwrap();
    rec.put_field("DLYA", EpicsValue::Double(0.025)).unwrap();

    // C `init_record`; the quantize is pass 1's (`sseqRecord.c:198`).
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    let q = thread_sleep_quantum();
    assert!(q > 0.0, "test assumes a positive clock quantum, got {q}");

    assert_eq!(
        dly(&rec, "3"),
        0.0,
        "DLY3=0.003 is less than half a 0.01 s tick: C rounds it to 0, so the \
         step fires with no delay at all"
    );
    assert_eq!(
        dly(&rec, "1"),
        expect_quantized(0.017),
        "DLY1=0.017 rounds up to 2 ticks (0.02)"
    );
    assert_eq!(
        dly(&rec, "7"),
        expect_quantized(1.234),
        "DLY7=1.234 rounds to 123 ticks (1.23)"
    );
    assert_eq!(
        dly(&rec, "A"),
        expect_quantized(0.025),
        "DLYA=0.025 rounds to 3 ticks — C's NINT is round-half-away-from-zero"
    );
    // Rounded means "a whole number of ticks".
    for n in ["1", "3", "7", "A"] {
        let ticks = dly(&rec, n) / q;
        assert!(
            (ticks - ticks.round()).abs() < 1e-9,
            "DLY{n} must be a whole number of {q} s ticks, got {} ({ticks} ticks)",
            dly(&rec, n)
        );
    }
}

#[epics_macros_rs::epics_test]
async fn r9_70_put_to_dly1_quantizes_dly1() {
    let mut rec = SseqRecord::default();
    rec.put_field("DLY1", EpicsValue::Double(0.037)).unwrap();
    rec.special("DLY1", true, &Default::default()).unwrap();

    assert_eq!(
        dly(&rec, "1"),
        expect_quantized(0.037),
        "a put to DLY1 rounds it to 4 ticks (0.04)"
    );
}

#[epics_macros_rs::epics_test]
async fn r9_70_put_to_dlya_quantizes_dlya_and_leaves_dly1_alone() {
    let mut rec = SseqRecord::default();
    // DLY1 left unrounded, so a round of the wrong field shows.
    rec.put_field("DLY1", EpicsValue::Double(0.037)).unwrap();
    rec.put_field("DLYA", EpicsValue::Double(0.083)).unwrap();

    rec.special("DLYA", true, &Default::default()).unwrap();

    assert_eq!(dly(&rec, "A"), expect_quantized(0.083));
    assert_eq!(dly(&rec, "1"), 0.037);
}

/// Oracle symptom (read/mon): the DEFAULT `DLYn` is `0.0`, and C's `NINT`
/// casts `0.0` to the integer `0`, so every served/monitored `DLYn` is `+0.0`
/// — it renders `"0"`. An `f64::trunc` port produced `-0.0` (`(-0.0 -
/// 0.5).trunc()` = `-0.0`), which renders `"-0"`.
#[epics_macros_rs::epics_test]
async fn r9_70_default_dly_serves_positive_zero_not_negative_zero() {
    let mut rec = SseqRecord::default();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    for n in ["1", "2", "3", "4", "5", "6", "7", "8", "9", "A"] {
        let v = dly(&rec, n);
        assert_eq!(v, 0.0, "default DLY{n} is 0.0");
        assert!(
            v.is_sign_positive(),
            "served DLY{n} must be +0.0 (renders \"0\"), got a negative zero \
             (renders \"-0\")"
        );
    }
}

/// Oracle symptom (put): a huge `caput DLY1` overflows C's `(long)` cast to
/// i64::MIN, so the served value is `quantum * i64::MIN` ≈ -9.22e16 — finite
/// and negative. An `f64::trunc` port kept `inf`.
#[epics_macros_rs::epics_test]
async fn r9_70_huge_put_to_dly1_overflows_to_c_long_min() {
    let q = thread_sleep_quantum();
    let expect_overflow = q * (i64::MIN as f64);

    let mut rec = SseqRecord::default();
    rec.put_field("DLY1", EpicsValue::Double(1e300)).unwrap();
    rec.special("DLY1", true, &Default::default()).unwrap();
    let v = dly(&rec, "1");
    assert_eq!(
        v, expect_overflow,
        "a huge DLY1 must serve C's overflow value ~-9.22e16, not inf"
    );
    assert!(v.is_finite() && v < 0.0, "must be finite negative, got {v}");

    // The same for a value that rounds to exactly zero on the put path.
    let mut rec2 = SseqRecord::default();
    rec2.put_field("DLY1", EpicsValue::Double(-0.0)).unwrap();
    rec2.special("DLY1", true, &Default::default()).unwrap();
    let z = dly(&rec2, "1");
    assert_eq!(z, 0.0);
    assert!(
        z.is_sign_positive(),
        "a -0.0 put to DLY1 must serve +0.0, not -0.0"
    );
}

/// The values `reader` has been sent since it was last read.
fn posted(reader: &mut EventReader) -> Vec<Option<f64>> {
    std::iter::from_fn(|| reader.try_recv().ok())
        .map(|e| e.value().as_value().and_then(|v| v.to_f64()))
        .collect()
}

/// A put to DLYA, end to end: `special` posts the rounded DLYA with
/// `DBE_VALUE` before the put posts it with `DBE_VALUE | DBE_LOG`
/// (dbAccess.c:1403-1418), and DLY1 is neither changed nor posted.
#[epics_macros_rs::epics_test]
async fn r9_70_put_to_dlya_posts_the_rounded_dlya() {
    use epics_base_rs::server::database::PvDatabase;

    let db = PvDatabase::new();
    db.add_record("SQ", Box::new(SseqRecord::default()))
        .await
        .unwrap();
    // Unrounded, past the init that rounds every DLYn, so a round of the
    // wrong field shows.
    db.get_record("SQ")
        .unwrap()
        .write()
        .record
        .put_field("DLY1", EpicsValue::Double(0.037))
        .unwrap();
    let subscribe = |field: &str, sid: u32, mask: EventMask| {
        let mut reader = db
            .get_record("SQ")
            .unwrap()
            .write()
            .add_subscriber(field, sid, DbFieldType::Double, mask.bits())
            .unwrap_or_else(|_| panic!("a {field} subscription must be accepted"));
        posted(&mut reader);
        reader
    };
    let mut dlya_value = subscribe("DLYA", 1, EventMask::VALUE);
    let mut dlya_log = subscribe("DLYA", 2, EventMask::LOG);
    let mut dly1 = subscribe("DLY1", 3, EventMask::VALUE | EventMask::LOG);

    db.put_pv("SQ.DLYA", EpicsValue::Double(0.083))
        .await
        .unwrap();

    let rounded = Some(expect_quantized(0.083));
    assert_eq!(posted(&mut dlya_value), vec![rounded, rounded]);
    assert_eq!(posted(&mut dlya_log), vec![rounded]);
    assert_eq!(posted(&mut dly1), vec![]);
    assert_eq!(db.get_pv("SQ.DLY1").unwrap().to_f64(), Some(0.037));
}

/// C returns from pass 0 before the quantize (`sseqRecord.c:167`), so DLYn
/// keeps its loaded value between the passes.
#[test]
fn pass_0_leaves_dly_as_loaded() {
    let mut rec = SseqRecord::default();
    rec.put_field("DLY1", EpicsValue::Double(0.017)).unwrap();
    rec.init_record(0).unwrap();
    assert_eq!(rec.get_field("DLY1"), Some(EpicsValue::Double(0.017)));
}
