//! R9-64 — a transform channel whose INPx link fails to read is ZEROED.
//!
//! C `transformRecord.c:537-541`, in the input-link loop:
//!
//! ```c
//! if (plink->type != CONSTANT) {
//!     status = dbGetLink(plink, DBR_DOUBLE, pval, NULL, NULL);
//!     if (!RTN_SUCCESS(status)) { *pval = 0.; }
//! }
//! ```
//!
//! transform-specific: `calcRecord.c::fetch_values` (427-443) leaves the value
//! stale on the identical failure. The port kept the stale value here too, so a
//! disconnected INPx source re-drove its OUTx with the last good value where C
//! drives 0.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn r9_64_failed_input_link_zeroes_its_channel_and_drives_zero_out() {
    let db = PvDatabase::new();
    // OUT target seeded 111 — must be driven to 0 by the zeroed channel.
    db.add_record("TGT", Box::new(AiRecord::new(111.0)))
        .await
        .unwrap();

    let mut tr = TransformRecord::new();
    // INPA names a PV that does not exist: dbGetLink fails every cycle.
    tr.put_field("INPA", EpicsValue::String("NOSUCHPV".into()))
        .unwrap();
    tr.put_field("OUTA", EpicsValue::String("TGT".into()))
        .unwrap();
    // Channel A pre-loaded with a "last good value" the port used to keep.
    tr.put_field("A", EpicsValue::Double(42.0)).unwrap();
    // Channel B has NO link — C's CONSTANT case: never read, never zeroed.
    tr.put_field("B", EpicsValue::Double(7.0)).unwrap();
    db.add_record("TR", Box::new(tr)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("TR", &mut v, 0).await.unwrap();

    let inst = db.get_record("TR").unwrap();
    {
        let rec = inst.read();
        assert_eq!(
            rec.record.get_field("A"),
            Some(EpicsValue::Double(0.0)),
            "a configured INPA that fails to read zeroes channel A \
             (transformRecord.c:537-541), it does not keep 42"
        );
        assert_eq!(
            rec.record.get_field("B"),
            Some(EpicsValue::Double(7.0)),
            "channel B has no input link — C's `plink->type == CONSTANT` case is \
             never read and never zeroed"
        );
    }

    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(0.0),
        "OUTA drives the zeroed channel: C outputs 0 for a dead INPx source"
    );
}
