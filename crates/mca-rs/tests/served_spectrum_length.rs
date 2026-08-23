//! `mca` serves `NORD` channels, but never zero of them.
//!
//! C `get_array_info` (`mcaRecord.c:865-873`):
//!
//! ```c
//! *no_elements =  pmca->nord;
//! if (*no_elements == 0) *no_elements = 1;
//! ```
//!
//! `init_record` pass 0 allocates `bptr`/`pbg` as `calloc(nmax, ...)`, so the
//! floored element is always there to serve. The port truncated to `NORD`
//! with no floor, so a record that had not acquired yet served a zero-length
//! array — which `oldChannelNotify.cpp:287` refuses outright.
//!
//! Boundaries: `NORD` at 0, at 1, strictly inside `NMAX`, and at `NMAX`.

use std::collections::HashMap;

use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(mca, "MCA1") {
    field(NMAX, "8")
    field(FTVL, "LONG")
}
"#;

async fn loaded() -> PvDatabase {
    mca_rs::register_mca_record_type();
    let db = PvDatabase::new();
    for def in parse_db(DB, &HashMap::new()).unwrap() {
        let mut rec = create_record(&def.record_type).unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &def.fields, &mut common).unwrap();
        db.add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
            .await
            .unwrap();
    }
    db
}

fn count(v: &EpicsValue) -> usize {
    match v {
        EpicsValue::LongArray(a) => a.len(),
        other => panic!("expected a LONG spectrum, got {other:?}"),
    }
}

/// `NORD == 0`: the floor. A never-acquired mca serves the first zeroed
/// channel of its `NMAX`-wide buffer, not an empty array.
#[tokio::test]
async fn a_never_acquired_spectrum_serves_one_zero_not_nothing() {
    let db = loaded().await;

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(0));
    assert_eq!(db.get_pv("MCA1").unwrap(), EpicsValue::LongArray(vec![0]));
}

/// `get_array_info` has no `fieldIndex` branch, so the floor governs `BG`
/// exactly as it governs `VAL`.
#[tokio::test]
async fn the_background_is_floored_with_the_spectrum() {
    let db = loaded().await;

    assert_eq!(
        db.get_pv("MCA1.BG").unwrap(),
        EpicsValue::LongArray(vec![0])
    );
}

/// `NORD == 1`: the floor must not be a special case that also fires here —
/// one acquired channel serves one channel, the same one.
#[tokio::test]
async fn one_acquired_channel_serves_that_channel() {
    let db = loaded().await;
    db.put_pv("MCA1", EpicsValue::LongArray(vec![42]))
        .await
        .unwrap();

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(1));
    assert_eq!(db.get_pv("MCA1").unwrap(), EpicsValue::LongArray(vec![42]));
}

/// `0 < NORD < NMAX`: the head of the buffer, not the whole buffer.
#[tokio::test]
async fn a_partial_spectrum_serves_nord_channels() {
    let db = loaded().await;
    db.put_pv("MCA1", EpicsValue::LongArray(vec![1, 2, 3]))
        .await
        .unwrap();

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(3));
    assert_eq!(
        db.get_pv("MCA1").unwrap(),
        EpicsValue::LongArray(vec![1, 2, 3])
    );
    assert_eq!(db.get_pv("MCA1.NMAX").unwrap(), EpicsValue::Long(8));
}

/// `NORD == NMAX`: the whole buffer, and the floor changes nothing.
#[tokio::test]
async fn a_full_spectrum_serves_nmax_channels() {
    let db = loaded().await;
    let full: Vec<i32> = (1..=8).collect();
    db.put_pv("MCA1", EpicsValue::LongArray(full.clone()))
        .await
        .unwrap();

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(8));
    assert_eq!(count(&db.get_pv("MCA1").unwrap()), 8);
    assert_eq!(db.get_pv("MCA1").unwrap(), EpicsValue::LongArray(full));
}
