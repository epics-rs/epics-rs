//! `field_metadata_override` outranks the record-level metadata cache.
//!
//! `finish_field_snapshot` layers three suppliers onto one served field, in
//! this order: the record-level cache (`record_instance.rs:1970-1973`, the
//! VAL metadata built by `populate_display_info` / `populate_control_info`),
//! then `route_field_metadata` (`:1978`), then
//! `apply_field_metadata_override` (`:1983`). The last one wins every slot
//! it supplies.
//!
//! Nothing else in the tree pins that order, and real correctness rests on
//! it. `graphic_limit_fields` and `control_limit_source` each carry a
//! `"motor"` arm that is redundant with `motor`'s own
//! `field_metadata_override`: deleting either arm leaves all 135 served
//! motor windows byte-identical, because the override overwrites the
//! record-level pair on every field. Those arms are kept deliberately —
//! dropping them would leave the record-level cache serving motor the
//! operator range, a wrong value masked only by the layer above it — but
//! "masked by the layer above" is a claim about ORDER, and an unpinned
//! order is one refactor away from being false.
//!
//! `motor` cannot host this test: both of its layers answer HLM/LLM, so
//! they agree and no reordering is observable. `histogram` can, because its
//! `field_metadata_override` (`histogram.rs:223-249`) answers WDTH, SDEL
//! and SDLY with values that differ from the record's own — which is the
//! only shape that distinguishes "the override ran last" from "the
//! record-level layer never ran at all".

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

/// `ULIM`/`LLIM` set the WDTH override window to `(0, ULIM - LLIM)` =
/// `(0, 20)`; `HOPR`/`LOPR` set the record-level window to `(11, 77)`. The
/// two must not overlap in either endpoint, or a reorder could pass.
///
/// `histogram`'s HOPR/LOPR are `DBF_ULONG` bucket counts, so both bounds are
/// non-negative here on purpose — a negative LOPR would land as 0 and the
/// lower endpoints would collide.
async fn histogram_whose_layers_disagree() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("H", Box::new(HistogramRecord::default()))
        .await
        .unwrap();
    for (pv, value) in [
        ("H.ULIM", EpicsValue::Double(30.0)),
        ("H.LLIM", EpicsValue::Double(10.0)),
        ("H.HOPR", EpicsValue::Double(77.0)),
        ("H.LOPR", EpicsValue::Double(11.0)),
        ("H.PREC", EpicsValue::Short(5)),
    ] {
        db.put_pv(pv, value)
            .await
            .unwrap_or_else(|e| panic!("{pv} failed to load: {e:?}"));
    }
    db
}

fn snapshot_of(db: &PvDatabase, field: &str) -> Snapshot {
    let rec = db.get_record("H").expect("H not in the database");
    let guard = rec.read();
    guard
        .snapshot_for_field(field)
        .unwrap_or_else(|| panic!("H.{field} has no snapshot"))
}

/// The contested slot and an uncontested one, on the SAME field.
///
/// WDTH's window comes from the override and its precision comes from the
/// record-level cache. Asserting both together is what makes this an
/// ordering test rather than a value test: the precision proves the
/// record-level layer did run on this field, so a passing window cannot be
/// explained away as "the cache never contributed". Swap `:1978` and
/// `:1983` and the window becomes the record-level `(11, 77)`.
#[epics_macros_rs::epics_test]
async fn an_overridden_window_beats_the_record_level_window_on_the_same_field() {
    let db = histogram_whose_layers_disagree().await;
    let snap = snapshot_of(&db, "WDTH");

    assert_eq!(
        snap.display_limits(),
        Some((0.0, 20.0)),
        "WDTH must serve the override's ULIM-LLIM window, not the record's HOPR/LOPR"
    );
    assert_eq!(
        snap.control_limits(),
        Some((0.0, 20.0)),
        "the control window is the same switch and must resolve the same way"
    );
    assert_eq!(
        snap.precision(),
        Some(5),
        "WDTH's precision has no override, so the record-level layer must still \
         reach it — this is what proves the layer ran at all"
    );
}

/// The other two slots the same override contests, on a different field.
/// `histogram.rs:231-237` answers SDEL with units `s` and precision 2 where
/// the record-level layer would serve the empty EGU and PREC 5.
#[epics_macros_rs::epics_test]
async fn overridden_units_and_precision_beat_the_record_level_ones() {
    let db = histogram_whose_layers_disagree().await;
    let snap = snapshot_of(&db, "SDEL");

    assert_eq!(
        snap.units().map(|u| u.as_str_lossy().into_owned()),
        Some("s".to_string()),
        "SDEL must serve the override's units, not the record's EGU"
    );
    assert_eq!(
        snap.precision(),
        Some(2),
        "SDEL must serve the override's precision, not the record's PREC"
    );
}

/// `histogram.rs:238-248` gives SDLY precision 0 and says in so many words
/// that "the record-level PREC cache would otherwise reach it". Zero is the
/// value a skipped override layer is most likely to be confused with, so it
/// gets its own case against a record PREC of 5.
#[epics_macros_rs::epics_test]
async fn an_override_of_zero_still_beats_a_nonzero_record_level_precision() {
    let db = histogram_whose_layers_disagree().await;

    assert_eq!(
        snapshot_of(&db, "SDLY").precision(),
        Some(0),
        "SDLY's override is 0 and must win over the record's PREC 5"
    );
}

/// The overlay half of the invariant: a field the override does not answer
/// keeps what the layers below it produced. Without this case, making
/// `apply_field_metadata_override` authoritative for every field — clearing
/// the slots it does not supply — would satisfy every assertion above.
#[epics_macros_rs::epics_test]
async fn a_field_with_no_override_keeps_the_record_level_window() {
    let db = histogram_whose_layers_disagree().await;
    let snap = snapshot_of(&db, "VAL");

    assert_eq!(
        snap.display_limits(),
        Some((11.0, 77.0)),
        "VAL has no override, so the record-level HOPR/LOPR window must stand"
    );
    assert_eq!(
        snap.control_limits(),
        Some((11.0, 77.0)),
        "and the control window with it"
    );
}
