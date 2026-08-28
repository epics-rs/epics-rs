//! A `BG` write moves `NORD` in C, exactly as a `VAL` write does.
//!
//! `put_array_info` has no `fieldIndex` branch (`mcaRecord.c:875-881`):
//!
//! ```c
//! static long put_array_info(struct dbAddr *paddr, long nNew)
//! {
//!     mcaRecord *pmca=(mcaRecord *)paddr->precord;
//!
//!     pmca->nord = nNew;
//!     if (pmca->nord > pmca->nmax) pmca->nord = pmca->nmax;
//!     return(0);
//! }
//! ```
//!
//! and `dbPut` calls it for EVERY `special(SPC_DBADDR)` field it writes
//! (`dbAccess.c:1365-1368`, with `nRequest` already cut to the `cvt_dbaddr`
//! capacity at `:1360-1361`; the local checkout `8f5015b66` sits five lines
//! lower and reads `:1370-1373` and `:1365-1366`). `VAL` and `BG` are the
//! two such fields
//! (`mcaRecord.dbd:35`, `:49`), so a background write sets the record's one
//! `NORD` — and `get_array_info` has no `fieldIndex` branch either, so it then
//! governs what BOTH fields serve.
//!
//! The port set `NORD` only from the `VAL` arm, so a `caput BG` left the
//! spectrum reporting its old length. This is the served-count half of the
//! buffer-width finding; the width itself is settled at init and unaffected.
//!
//! Boundaries: a background shorter than the spectrum, one exactly as long,
//! and one longer than the capacity.

// RTEMS-EXEC-MODEL-ALLOW(4): checked, not waived — all 4 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p mca-rs
// --all-features`, 62/62). mca-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

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

async fn acquired() -> PvDatabase {
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
    db.put_pv("MCA1", EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7, 8]))
        .await
        .unwrap();
    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(8));
    db
}

/// The shorter background: `NORD` drops to what the writer supplied, and the
/// spectrum is served through the same count.
#[tokio::test]
async fn a_shorter_background_write_shortens_nord() {
    let db = acquired().await;

    db.put_pv("MCA1.BG", EpicsValue::LongArray(vec![5, 5]))
        .await
        .unwrap();

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(2));
    assert_eq!(
        db.get_pv("MCA1.BG").unwrap(),
        EpicsValue::LongArray(vec![5, 5])
    );
    assert_eq!(
        db.get_pv("MCA1").unwrap(),
        EpicsValue::LongArray(vec![1, 2])
    );
}

/// A background exactly as long as the spectrum leaves `NORD` where it was.
#[tokio::test]
async fn a_full_length_background_write_leaves_nord_alone() {
    let db = acquired().await;

    db.put_pv("MCA1.BG", EpicsValue::LongArray(vec![9; 8]))
        .await
        .unwrap();

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(8));
}

/// C's `if (pmca->nord > pmca->nmax)` clamp: an over-long background cannot
/// push `NORD` past the capacity.
#[tokio::test]
async fn an_over_long_background_write_clamps_nord_to_nmax() {
    let db = acquired().await;

    db.put_pv("MCA1.BG", EpicsValue::LongArray(vec![3; 12]))
        .await
        .unwrap();

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(8));
}

/// The `VAL` half of the same hook, unchanged.
#[tokio::test]
async fn a_shorter_spectrum_write_shortens_nord() {
    let db = acquired().await;

    db.put_pv("MCA1", EpicsValue::LongArray(vec![7, 7, 7]))
        .await
        .unwrap();

    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(3));
}
