//! The two `mca` spectrum buffers are as wide as the record's capacity, not as
//! wide as the last value written to them.
//!
//! C allocates both once in `init_record` (`mcaRecord.c:426-431`) and never
//! reallocates either:
//!
//! ```c
//! if (pmca->nmax <= 0) pmca->nmax=1;
//! if (pmca->ftvl == 0) {
//!     pmca->bptr = (char *)calloc(pmca->nmax,MAX_STRING_SIZE);
//!     pmca->pbg  = (char *)calloc(pmca->nmax,MAX_STRING_SIZE);
//! } else {
//!     if (pmca->ftvl > DBF_DOUBLE) pmca->ftvl=2;
//!     pmca->bptr = (char *)calloc(pmca->nmax,sizeofTypes[pmca->ftvl]);
//!     pmca->pbg  = (char *)calloc(pmca->nmax,sizeofTypes[pmca->ftvl]);
//! }
//! ```
//!
//! `NMAX` and `FTVL` are both `special(SPC_NOMOD)` (`mcaRecord.dbd:78-98`), so
//! that geometry is settled at init and a write can never move it. The port had
//! made the width a property of the last write: the `BG` put arm re-sized the
//! incoming buffer to `NMAX` only when it happened to be a `DoubleArray`, so a
//! short `caput` to `BG` under any other `FTVL` left a short buffer behind.
//!
//! That is worse since the channel started advertising `NMAX` elements from
//! `cvt_dbaddr`: a two-element buffer sat behind a channel promising eight.
//!
//! Boundaries: `BG` written short under a non-`DOUBLE` `FTVL` and under
//! `DOUBLE`, `VAL` written short before a device support reports more channels
//! than it holds, and the capacity floor itself at `NMAX <= 0`.

// RTEMS-EXEC-MODEL-ALLOW(3): checked, not waived — all 3 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p mca-rs
// --all-features`, 62/62). mca-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::collections::HashMap;

use epics_base_rs::server::database::{PvDatabase, RecordLoad};
use epics_base_rs::server::db_loader::{apply_fields, create_record, parse_db};
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use mca_rs::McaRecord;

async fn loaded(db_text: &str) -> PvDatabase {
    mca_rs::register_mca_record_type();
    let db = PvDatabase::new();
    for def in parse_db(db_text, &HashMap::new()).unwrap() {
        let mut rec = create_record(&def.record_type).unwrap();
        let mut common = Vec::new();
        apply_fields(&mut rec, &def.fields, &mut common).unwrap();
        db.add_loaded_record(&def.name, rec, RecordLoad::from_common_fields(common))
            .await
            .unwrap();
    }
    db
}

const LONG_MCA: &str = r#"
record(mca, "MCA1") {
    field(NMAX, "8")
    field(FTVL, "LONG")
}
"#;

const DOUBLE_MCA: &str = r#"
record(mca, "MCA1") {
    field(NMAX, "8")
    field(FTVL, "DOUBLE")
}
"#;

/// The reported trigger. The short write cuts `NORD` to two through
/// `put_array_info`, so the eight channels it left behind are invisible until
/// the next full spectrum brings `NORD` back — and they have to still be there
/// when it does.
#[tokio::test]
async fn a_short_background_write_keeps_the_full_nmax_width() {
    let db = loaded(LONG_MCA).await;

    db.put_pv("MCA1", EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7, 8]))
        .await
        .unwrap();
    db.put_pv("MCA1.BG", EpicsValue::LongArray(vec![5, 5]))
        .await
        .unwrap();
    assert_eq!(db.get_pv("MCA1.NORD").unwrap(), EpicsValue::Long(2));

    db.put_pv("MCA1", EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7, 8]))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("MCA1.BG").unwrap(),
        EpicsValue::LongArray(vec![5, 5, 0, 0, 0, 0, 0, 0])
    );
}

/// The same write under the one `FTVL` the old arm did handle. It passed
/// before and it passes now: the fix made the rule uniform rather than moving
/// which element type is special.
#[tokio::test]
async fn a_short_double_background_write_keeps_the_full_nmax_width() {
    let db = loaded(DOUBLE_MCA).await;

    db.put_pv(
        "MCA1",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
    )
    .await
    .unwrap();
    db.put_pv("MCA1.BG", EpicsValue::DoubleArray(vec![5.0, 5.0]))
        .await
        .unwrap();
    db.put_pv(
        "MCA1",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
    )
    .await
    .unwrap();

    assert_eq!(
        db.get_pv("MCA1.BG").unwrap(),
        EpicsValue::DoubleArray(vec![5.0, 5.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    );
}

/// The `VAL` half of the same family. A device support that fills the record's
/// buffer in place reports only a channel count (`devMCA_soft.c:155-161`), so
/// `NORD` can rise above anything a client ever wrote to `VAL` — and the
/// buffer has to be `NMAX` deep for those channels to exist.
#[test]
fn a_short_spectrum_write_survives_a_read_that_reports_more_channels() {
    let mut rec = McaRecord {
        nmax: 8,
        nuse: 8,
        ..Default::default()
    };
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();

    rec.put_field("VAL", EpicsValue::LongArray(vec![1, 2]))
        .unwrap();
    rec.land_channel_count(8);

    assert_eq!(
        rec.get_field("VAL").unwrap(),
        EpicsValue::LongArray(vec![1, 2, 0, 0, 0, 0, 0, 0])
    );
}

/// The capacity floor, C `mcaRecord.c:424`: `if (pmca->nmax <= 0) pmca->nmax=1;`
/// A zero-width record is not constructible on either side.
#[tokio::test]
async fn a_non_positive_nmax_is_floored_to_one_channel() {
    for nmax in ["0", "-5"] {
        let text =
            format!(r#"record(mca, "MCA1") {{ field(NMAX, "{nmax}") field(FTVL, "LONG") }}"#);
        let db = loaded(&text).await;

        assert_eq!(db.get_pv("MCA1.NMAX").unwrap(), EpicsValue::Long(1));
        assert_eq!(db.get_pv("MCA1").unwrap(), EpicsValue::LongArray(vec![0]));
        db.put_pv("MCA1.BG", EpicsValue::LongArray(vec![7]))
            .await
            .unwrap();
        assert_eq!(
            db.get_pv("MCA1.BG").unwrap(),
            EpicsValue::LongArray(vec![7])
        );
    }
}
