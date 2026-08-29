//! `04 M4`: does the `arr` filter slice a circular buffer from the logical
//! start, as C does, or from physical index 0?
//!
//! C `arr.c:111-130` reads the ring through `dbChannelGetArrayInfo`, which
//! hands back the RAW buffer plus the record's element offset, and then
//! rotates the slice origin itself:
//!
//! ```c
//! dbChannelGetArrayInfo(chan, &pSource, &nSource, &offset);
//! nTarget = wrapArrayIndices(&start, my->incr, &end, nSource);
//! ...
//! /* must do the wrap-around with the original no_elements */
//! offset = (offset + start) % pfl->no_elements;
//! dbExtractArray(pSource, pTarget, pfl->field_size,
//!     nTarget, pfl->no_elements, offset, my->incr);
//! ```
//!
//! The port linearises earlier instead: `CompressRecord::get_field("VAL")`
//! returns `linearise_val()`, oldest→newest, so the value that reaches
//! `slice_with` is already in logical order and its offset is zero by
//! construction. These cases pin that the two arrive at the same elements —
//! the observable C's rotation exists to produce.

use epics_base_rs::server::database::filters::arr::{ArrayFilter, ArrayFilterConfig};
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::compress::CompressRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

/// nsam=5, ALG=Circular Buffer, seven samples driven in through INP the way a
/// real IOC does. FIFO writes at `off` and post-increments, so the physical
/// buffer ends `[5, 6, 2, 3, 4]` with `off == 2`, and the logical
/// (oldest→newest) order is `[2, 3, 4, 5, 6]`.
async fn rotated_compress_db() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("src", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("CB", Box::new(CompressRecord::new(5, 4)))
        .await
        .unwrap();
    {
        let rec = db.get_record("CB").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("src".into()))
            .unwrap();
    }
    for i in 0..7 {
        db.put_pv("src", EpicsValue::Double(i as f64))
            .await
            .unwrap();
        let mut visited = HashSet::new();
        db.process_record_with_links("CB", &mut visited, 0)
            .await
            .unwrap();
    }
    db
}

fn served_val(db: &PvDatabase) -> Vec<f64> {
    let handle = db.get_record("CB").unwrap();
    let inst = handle.read();
    match inst.client_field_value("VAL").unwrap() {
        EpicsValue::DoubleArray(v) => v,
        other => panic!("expected DoubleArray, got {other:?}"),
    }
}

fn through_arr(values: Vec<f64>, start: i64, end: i64) -> Vec<f64> {
    let cfg = ArrayFilterConfig {
        start,
        incr: 1,
        end,
    };
    let event = FilteredMonitorEvent::new(MonitorEvent {
        snapshot: std::sync::Arc::new(Snapshot::new(
            EpicsValue::DoubleArray(values),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        )),
        origin: 0,
        mask: EventMask::VALUE,
    });
    let out = ArrayFilter::new(cfg).apply(event).unwrap();
    match std::sync::Arc::unwrap_or_clone(out.event.snapshot).value {
        EpicsValue::DoubleArray(v) => v,
        other => panic!("expected DoubleArray, got {other:?}"),
    }
}

/// The premise: the record really is rotated. `off == 2` and the raw buffer is
/// NOT in logical order, so an unrotated slice would be visibly wrong.
#[epics_macros_rs::epics_test]
async fn the_buffer_under_test_is_actually_rotated() {
    let db = rotated_compress_db().await;
    let handle = db.get_record("CB").unwrap();
    let inst = handle.read();
    assert_eq!(
        inst.record.get_field("OFF").unwrap(),
        EpicsValue::ULong(2),
        "seven samples into nsam=5 leaves the write cursor at 2"
    );
    assert_eq!(
        inst.record.get_field("NUSE").unwrap(),
        EpicsValue::ULong(5),
        "the ring is full, so every physical slot holds a live sample"
    );
    // off=2 with a full ring means physical order is [5, 6, 2, 3, 4] and
    // logical order is [2, 3, 4, 5, 6] — they differ, which is the whole
    // premise of the row.
}

/// What the filter is handed. C hands `arr` the raw ring and an offset of 2;
/// the port hands it the linearised array and an implicit offset of 0. Same
/// elements, same order.
#[epics_macros_rs::epics_test]
async fn the_value_reaching_the_filter_is_already_logical() {
    let db = rotated_compress_db().await;
    assert_eq!(
        served_val(&db),
        vec![2.0, 3.0, 4.0, 5.0, 6.0],
        "get_field linearises oldest->newest, which is what C's offset produces"
    );
}

/// The row's observable. C: `wrapArrayIndices` gives `start=1`, `nTarget=4`,
/// then `offset = (2 + 1) % 5 = 3`, so `dbExtractArray` copies physical
/// `[3, 4, 0, 1]` = `[3, 4, 5, 6]`. Slicing physical index 0 would give
/// `[6, 2, 3, 4]` — the rotated wrong answer the row describes.
#[epics_macros_rs::epics_test]
async fn arr_s1_returns_c_elements_not_physical_index_one() {
    let db = rotated_compress_db().await;
    let served = served_val(&db);
    assert_eq!(
        through_arr(served, 1, -1),
        vec![3.0, 4.0, 5.0, 6.0],
        "C extracts from physical 3 after (off + start) % nsam; a physical-0 \
         slice would return [6, 2, 3, 4]"
    );
}

/// A bounded slice, where a rotation error shows up in the middle rather than
/// at the head. C: `start=1`, `end=2`, `nTarget=2`, `offset=3` → physical
/// `[3, 4]` = `[3.0, 4.0]`.
#[epics_macros_rs::epics_test]
async fn arr_s1_e2_returns_c_elements() {
    let db = rotated_compress_db().await;
    let served = served_val(&db);
    assert_eq!(through_arr(served, 1, 2), vec![3.0, 4.0]);
}

/// `s=0` is the case a physical-index slice happens to get right, so it cannot
/// distinguish the two implementations — pinned so a future change that breaks
/// the aligned case is caught too.
#[epics_macros_rs::epics_test]
async fn arr_s0_returns_the_whole_logical_buffer() {
    let db = rotated_compress_db().await;
    let served = served_val(&db);
    assert_eq!(through_arr(served, 0, -1), vec![2.0, 3.0, 4.0, 5.0, 6.0]);
}

/// Negative indices, where C's `wrapArrayIndices` resolves against `nSource`
/// (the ring's live count) before the rotation is applied. C: `start=-2` → 3,
/// `end=-1` → 4, `nTarget=2`, `offset = (2 + 3) % 5 = 0` → physical `[0, 1]`
/// = `[5.0, 6.0]`. A physical-0 slice would give `[3.0, 4.0]`.
#[epics_macros_rs::epics_test]
async fn arr_negative_start_returns_c_elements() {
    let db = rotated_compress_db().await;
    let served = served_val(&db);
    assert_eq!(through_arr(served, -2, -1), vec![5.0, 6.0]);
}

/// `compressRecord.c` is the ONLY record in `modules/database/src` at
/// `R7.0.10` whose `get_array_info` writes a non-zero `*offset` — every other
/// implementation leaves it 0, so it is the only source a rotation could come
/// from, and the port linearises it at the record. This case pins the ordinary
/// (offset-free) array so a change to the linearisation cannot silently move
/// the aligned path instead.
#[epics_macros_rs::epics_test]
async fn a_plain_array_is_unaffected() {
    assert_eq!(
        through_arr(vec![10.0, 11.0, 12.0, 13.0, 14.0], 1, 3),
        vec![11.0, 12.0, 13.0]
    );
}
