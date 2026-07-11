//! R9-70 — sseq rounds DLYn to a whole OS clock tick.
//!
//! Two C sites, with different scopes:
//!
//! * `sseqRecord.c::init_record` (197-200) rounds EVERY `DLYn`:
//!
//!   ```c
//!   for (index = 0; index < NUM_LINKS; index++, plinkGroup++) {
//!       plinkGroup->dly = epicsThreadSleepQuantum() *
//!           NINT(plinkGroup->dly/epicsThreadSleepQuantum());
//!       db_post_events(pR, &plinkGroup->dly, DBE_VALUE);
//!   ```
//!
//! * `sseqRecord.c::special` (1140-1156), on a put to any `DLY1..DLYA`, rounds
//!   **DLY1** — it computes `lnkIndex` from the written field and then never
//!   applies it to `plinkGroup` (the `STRn` case immediately above it does:
//!   `plinkGroup += lnkIndex`). So the field the client wrote keeps its raw
//!   value and DLY1 is what gets rounded and posted. That is C's observable
//!   behaviour — present since the record moved into the calc module — so the
//!   port reproduces it rather than "fixing" it.
//!
//! The port stored and used the raw value at both sites, so a `DLY3=0.003`
//! waited 3 ms where C waits 0, and every DLYn read back unrounded.
//!
//! `epicsThreadSleepQuantum()` is `1/sysconf(_SC_CLK_TCK)` = 0.01 s on Linux
//! and macOS, and `NINT(f) = (long)(f > 0 ? f+0.5 : f-0.5)` (sseqRecord.c:67).

use epics_base_rs::runtime::time::thread_sleep_quantum;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

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

#[tokio::test]
async fn r9_70_init_rounds_every_dly_to_a_clock_tick() {
    let mut rec = SseqRecord::default();
    // Raw db-file values: put_field alone stores them verbatim (C `dbPut` at
    // load time does the same — `special` has not run yet).
    rec.put_field("DLY1", EpicsValue::Double(0.017)).unwrap();
    rec.put_field("DLY3", EpicsValue::Double(0.003)).unwrap();
    rec.put_field("DLY7", EpicsValue::Double(1.234)).unwrap();
    rec.put_field("DLYA", EpicsValue::Double(0.025)).unwrap();

    // C `init_record`, pass 0.
    rec.init_record(0).unwrap();

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

/// A put to DLY1 quantizes DLY1 — the one index where C's `special` quirk and
/// the obvious reading agree.
#[tokio::test]
async fn r9_70_put_to_dly1_quantizes_dly1() {
    let mut rec = SseqRecord::default();
    rec.put_field("DLY1", EpicsValue::Double(0.037)).unwrap();
    rec.special("DLY1", true).unwrap();

    assert_eq!(
        dly(&rec, "1"),
        expect_quantized(0.037),
        "a put to DLY1 rounds it to 4 ticks (0.04)"
    );
}

/// C's `special` quirk, pinned: a put to DLYA rounds DLY1 and leaves DLYA raw.
#[tokio::test]
async fn r9_70_put_to_dlya_quantizes_dly1_and_leaves_dlya_raw() {
    let mut rec = SseqRecord::default();
    // DLY1 left unrounded on purpose, so the quirk is observable.
    rec.put_field("DLY1", EpicsValue::Double(0.037)).unwrap();
    rec.put_field("DLYA", EpicsValue::Double(0.083)).unwrap();

    rec.special("DLYA", true).unwrap();

    assert_eq!(
        dly(&rec, "1"),
        expect_quantized(0.037),
        "C `special` never advances `plinkGroup` by `lnkIndex`, so the put to \
         DLYA rounds DLY1 (0.037 → 0.04)"
    );
    assert_eq!(
        dly(&rec, "A"),
        0.083,
        "the field actually written keeps its raw value — C rounds DLY1, not \
         DLYA (sseqRecord.c:1150-1155)"
    );
}

/// The framework put path (`special` after the store) must show the same
/// thing end to end: DLYA raw, DLY1 rounded.
#[tokio::test]
async fn r9_70_framework_put_path_matches_c() {
    use epics_base_rs::server::database::PvDatabase;

    let db = PvDatabase::new();
    let mut rec = SseqRecord::default();
    rec.put_field("DLY1", EpicsValue::Double(0.037)).unwrap();
    db.add_record("SQ", Box::new(rec)).await.unwrap();

    db.put_pv("SQ.DLYA", EpicsValue::Double(0.083))
        .await
        .unwrap();

    let inst = db.get_record("SQ").await.unwrap();
    let g = inst.read().await;
    assert_eq!(
        g.record.get_field("DLYA").and_then(|v| v.to_f64()),
        Some(0.083),
        "a CA put to DLYA leaves DLYA raw"
    );
    assert_eq!(
        g.record.get_field("DLY1").and_then(|v| v.to_f64()),
        Some(expect_quantized(0.037)),
        "...and rounds DLY1 instead"
    );
}
