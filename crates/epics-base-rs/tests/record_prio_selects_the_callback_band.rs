//! A record's `PRIO` selects which of the three callback queues its deferred
//! work runs on.
//!
//! C sets the band from the record and re-sets it every cycle —
//! `callbackSetPriority(prec->prio, &pcb->callback)` under the comment "Set
//! callback from PRIO" at the top of `seqRecord.c:145-146` `process()` — and
//! `callbackRequest` then dispatches on that stored value
//! (`callback.c:355-365`), so `PRIO` picks the queue and, through it, the
//! worker thread and its OS priority. The port used to hand every deferred
//! tail to one hard-coded band, which made `PRIO` select nothing at all.
//!
//! One case per band rather than one per record type: the record type only
//! decides *which* work is deferred, while the band is decided by the two
//! hops every deferral site now shares — `CommonFields::prio` →
//! `ProcessContext::callback_priority` → `spawn_background`. The observable
//! is the pool's own worker thread, named `cbLow`/`cbMedium`/`cbHigh` after C
//! `callback.c:324-327`.

use epics_base_rs::runtime::task::{CallbackPriority, spawn_background};
use epics_base_rs::server::record::CommonFields;

/// The band a record with this `PRIO` hands to `spawn_background`, and the
/// pool worker that actually ran the tail.
async fn band_and_worker_for_prio(prio: i16) -> (CallbackPriority, String) {
    let common = CommonFields {
        prio,
        ..Default::default()
    };
    // Exactly the hop every record-support deferral site takes: the framework
    // snapshots `common` before `process()`, the record stashes the band, and
    // the band is what reaches the pool.
    let band = common.process_context().callback_priority;
    let worker = spawn_background(band, async {
        std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string()
    })
    .await
    .expect("the callback pool ran the tail");
    (band, worker)
}

#[epics_macros_rs::epics_test]
async fn prio_low_runs_the_records_work_on_the_low_band() {
    let (band, worker) = band_and_worker_for_prio(0).await;
    assert_eq!(band, CallbackPriority::Low, "PRIO=LOW (menuPriority 0)");
    assert_eq!(worker, "cbLow", "the tail ran on the low-band worker");
}

#[epics_macros_rs::epics_test]
async fn prio_medium_runs_the_records_work_on_the_medium_band() {
    let (band, worker) = band_and_worker_for_prio(1).await;
    assert_eq!(
        band,
        CallbackPriority::Medium,
        "PRIO=MEDIUM (menuPriority 1)"
    );
    assert_eq!(worker, "cbMedium", "the tail ran on the medium-band worker");
}

#[epics_macros_rs::epics_test]
async fn prio_high_runs_the_records_work_on_the_high_band() {
    let (band, worker) = band_and_worker_for_prio(2).await;
    assert_eq!(band, CallbackPriority::High, "PRIO=HIGH (menuPriority 2)");
    assert_eq!(worker, "cbHigh", "the tail ran on the high-band worker");
}

/// The boundary either side of the menu. C validates the copied value only at
/// queue time and *drops* the callback — `callbackRequest` logs "Bad priority"
/// and returns `S_db_badField` (`callback.c:355-358`) — which loses the
/// record's cycle. The port keeps the work and runs it on the band an
/// unwritten `PRIO` already has.
#[epics_macros_rs::epics_test]
async fn a_prio_outside_the_menu_still_runs_the_work_on_the_low_band() {
    for prio in [-1i16, 3, i16::MAX] {
        let (band, worker) = band_and_worker_for_prio(prio).await;
        assert_eq!(
            band,
            CallbackPriority::Low,
            "PRIO={prio} is not a menu choice"
        );
        assert_eq!(worker, "cbLow", "PRIO={prio} kept its work");
    }
}

/// C re-runs `callbackSetPriority` inside `process()`, so a `PRIO` written
/// between cycles moves the *next* cycle's work. The snapshot the framework
/// pushes before each `process()` is what carries that here.
#[epics_macros_rs::epics_test]
async fn a_prio_written_between_cycles_moves_the_next_cycles_band() {
    let mut common = CommonFields {
        prio: 0,
        ..Default::default()
    };
    assert_eq!(
        common.process_context().callback_priority,
        CallbackPriority::Low
    );
    common.prio = 2;
    assert_eq!(
        common.process_context().callback_priority,
        CallbackPriority::High,
        "the next cycle's snapshot carries the new PRIO"
    );
}
