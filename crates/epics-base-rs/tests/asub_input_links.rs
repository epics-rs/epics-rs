//! aSub input fetch honors the declared FTA..FTU / NOA..NOU.
//!
//! C `aSubRecord.c::fetch_values` (278-288):
//!
//! ```c
//! long nRequest = (&prec->noa)[i];
//! status = dbGetLink(plink, (&prec->fta)[i], (&prec->a)[i], 0, &nRequest);
//! (&prec->nea)[i] = nRequest;
//! ```
//!
//! — every input link is read as the channel's declared FTx, up to NOx
//! elements, into the FTx-typed buffer `initFields` allocated. The port's
//! cells all started as scalar `Double(0.0)` whatever FTx said, so the store
//! judged the destination by that stale scalar: an array source was reduced
//! to its first element, and a string source was dropped outright by the
//! numeric funnel.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, SubroutineFn};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::server::records::waveform::WaveformRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn field(db: &PvDatabase, rec: &str, f: &str) -> Option<EpicsValue> {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.record.get_field(f)
}

/// db with a waveform source, a string source, and an aSub declaring
/// FTA=DOUBLE/NOA=8 on INPA and FTC=STRING on INPC. The subroutine is a
/// no-op returning 0 — the assertions are about what `fetch_values`
/// landed in A and C.
async fn asub_db() -> PvDatabase {
    let db = PvDatabase::new();

    let mut wf = WaveformRecord::new(8, DbFieldType::Double);
    wf.put_field(
        "VAL",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
    )
    .unwrap();
    db.add_record("WF", Box::new(wf)).await.unwrap();
    db.add_record("SEL", Box::new(StringoutRecord::new("rrt_connect")))
        .await
        .unwrap();

    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert(
        "check".into(),
        Arc::new(Box::new(|_: &mut dyn Record| Ok(0i64)) as SubroutineFn),
    );
    db.install_subroutine_registry(registry).await;
    db.add_record("NAME_HOLDER", Box::new(StringoutRecord::new("check")))
        .await
        .unwrap();

    let mut rec = ASubRecord::default();
    rec.put_field("SUBL", EpicsValue::String("NAME_HOLDER".into()))
        .unwrap();
    rec.put_field("LFLG", EpicsValue::Short(1)).unwrap(); // READ: resolve SNAM
    rec.put_field("FTA", EpicsValue::Short(10)).unwrap(); // DOUBLE
    rec.put_field("NOA", EpicsValue::Long(8)).unwrap();
    rec.put_field("INPA", EpicsValue::String("WF".into()))
        .unwrap();
    rec.put_field("FTC", EpicsValue::Short(0)).unwrap(); // STRING
    rec.put_field("INPC", EpicsValue::String("SEL".into()))
        .unwrap();
    db.add_record("ASUB", Box::new(rec)).await.unwrap();
    db
}

/// An array source lands whole in a `NOx > 1` channel — not reduced to its
/// first element by a scalar-shaped destination.
#[epics_macros_rs::epics_test]
async fn array_source_lands_whole_in_a_declared_array_channel() {
    let db = asub_db().await;

    process(&db, "ASUB").await;

    assert_eq!(
        field(&db, "ASUB", "A").await,
        Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0])),
        "the waveform's NORD elements must all reach A"
    );
    assert_eq!(
        field(&db, "ASUB", "NEA").await,
        Some(EpicsValue::Long(5)),
        "NEx is the delivered element count (aSubRecord.c:287)"
    );
}

/// A string source lands in an `FTx = STRING` channel — not dropped by the
/// numeric store funnel.
#[epics_macros_rs::epics_test]
async fn string_source_lands_in_a_string_channel() {
    let db = asub_db().await;

    process(&db, "ASUB").await;

    assert_eq!(
        field(&db, "ASUB", "C").await,
        Some(EpicsValue::String("rrt_connect".into())),
        "a DBR_STRING read of the scalar source must reach C"
    );
}
