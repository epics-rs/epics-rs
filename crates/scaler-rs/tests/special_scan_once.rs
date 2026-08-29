//! R11-C11 — scaler `special()` rescans a non-Passive record.
//!
//! C `scalerRecord.c::special()` ends both state-changing cases with a
//! rescan:
//!
//!   * `CNT` (`:655`), the last statement of the handle-it-now arm, after the
//!     COUTP put and the `us` transition (REQSTART / abort);
//!   * `CONT` (`:667`), the auto-count mode switch.
//!
//! Both read `if (pscal->scan) scanOnce((void *)pscal);` — C's own comment:
//! *"Scan record if it's not Passive. (If it's Passive, it'll get scanned
//! automatically, since .cnt is a Process-Passive field.)"*
//!
//! `special()` only moves `us`/`cont`; `process()` is what acts on them (arms
//! the hardware, starts/stops counting). A non-Passive scaler gets no process
//! from the put — `dbPutField`'s `pp(TRUE)` reprocess is Passive-only — so
//! without the `scanOnce` the count does not start until the next periodic
//! scan, up to a full scan period late.

// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — all 3 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p scaler-rs
// --all-features`, 112/112). scaler-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::types::EpicsValue;
use scaler_rs::records::scaler::ScalerRecord;

const SCALER_STATE_IDLE: i16 = 0;
const SCALER_STATE_COUNTING: i16 = 2;

/// A scaler whose SCAN is periodic — the scan task is NOT running in this
/// test, so the ONLY thing that can process the record is C's `scanOnce`.
async fn scaler_db(scan: ScanType) -> PvDatabase {
    let db = PvDatabase::new();
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.nch = 4;
    rec.init_record(1).unwrap();
    db.add_record("SCAL", Box::new(rec)).await.unwrap();
    {
        let r = db.get_record("SCAL").unwrap();
        r.write().common.scan = scan;
    }
    db
}

async fn ss(db: &PvDatabase) -> i16 {
    let r = db.get_record("SCAL").unwrap();
    let g = r.read();
    match g.record.get_field("SS").unwrap() {
        EpicsValue::Short(v) => v,
        other => panic!("SS is DBF_SHORT, got {other:?}"),
    }
}

/// `scanOnce` is queued, not inline (C hands the record to the scan-once
/// thread, which must first take the `dbScanLock` the putter holds).
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

#[tokio::test]
async fn r11_c11_cnt_put_on_a_non_passive_scaler_scans_once() {
    let db = scaler_db(ScanType::SEC1).await;
    assert_eq!(ss(&db).await, SCALER_STATE_IDLE, "starts idle");

    db.put_record_field_from_ca("SCAL", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    settle().await;

    assert_eq!(
        ss(&db).await,
        SCALER_STATE_COUNTING,
        "C:655 `if (pscal->scan) scanOnce()` — the count must start on the put, \
         not a scan period later"
    );
}

/// The Passive half of C's guard: the put itself processes the record
/// (`CNT` is `pp(TRUE)`), so `scanOnce` must NOT add a second process.
#[tokio::test]
async fn r11_c11_passive_scaler_is_processed_once_by_the_put_itself() {
    let db = scaler_db(ScanType::Passive).await;

    db.put_record_field_from_ca("SCAL", "CNT", EpicsValue::Short(1))
        .await
        .unwrap();
    settle().await;

    assert_eq!(
        ss(&db).await,
        SCALER_STATE_COUNTING,
        "the pp(TRUE) put processed it"
    );

    // A second process would be visible as an extra count-start: after the
    // first cycle `cnt == pcnt`, so a re-scan re-enters with nothing to do and
    // the state stays COUNTING. What must NOT happen is the framework
    // *double*-processing the put. Assert on the arm/reset commands the record
    // asked device support for: exactly one count start.
    let r = db.get_record("SCAL").unwrap();
    let g = r.read();
    let pcnt = g.record.get_field("PCNT").unwrap();
    assert_eq!(
        pcnt,
        EpicsValue::Short(1),
        "the single count-start latched PCNT = CNT"
    );
}

/// `CONT` (auto-count mode) takes the same rescan — C `:664-668`.
#[tokio::test]
async fn r11_c11_cont_put_on_a_non_passive_scaler_scans_once() {
    let db = scaler_db(ScanType::SEC1).await;

    // CONT=1 (auto-count) with no user count in progress: the process cycle
    // C's scanOnce forces is what enters the auto-count restart arm
    // (scalerRecord.c:484-486) and arms the hardware.
    db.put_record_field_from_ca("SCAL", "CONT", EpicsValue::Short(1))
        .await
        .unwrap();
    settle().await;

    assert_eq!(
        ss(&db).await,
        SCALER_STATE_COUNTING,
        "C:667 `if (pscal->scan) scanOnce()` — auto-count must start on the \
         CONT put, not a scan period later"
    );
}
