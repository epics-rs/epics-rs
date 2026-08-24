//! `mca` reaches its declaration through `default_property_support`'s
//! `_ => NUMERIC` fallback, and the fallback is right — `mcaRecord.c:150-157`
//! NULLs nothing. What it did not have was a supplier: there is no `"mca"` arm
//! anywhere in the framework's metadata cache, so every one of the type's
//! fields carried empty units, and every DBF_DOUBLE outside the four
//! calibration fields carried precision 0, under a mark saying the record had
//! answered.
//!
//! `mcaRecord.c:884-890` copies `EGU` with no field test at all, and
//! `:892-907` seeds `pmca->prec` and departs from it only for
//! `CALO`/`CALS`/`CALQ`/`TTH`. Both are record-level answers; this file is the
//! cross-crate check that the record-level cache reaches a type declared only
//! by the fallback.

use epics_base_rs::server::record::{Record, RecordInstance};
use epics_base_rs::types::EpicsValue;
use mca_rs::record::McaRecord;

fn mca() -> RecordInstance {
    let mut rec = McaRecord::default();
    rec.put_field("PREC", EpicsValue::Short(3))
        .expect("mca models PREC");
    rec.put_field("EGU", EpicsValue::String("cts".into()))
        .expect("mca models EGU");
    RecordInstance::new("T:MCA".to_string(), rec)
}

#[test]
fn mca_copies_egu_into_every_field() {
    let inst = mca();

    for field in ["VAL", "DWEL", "CALO", "PLTM"] {
        let snap = inst
            .snapshot_for_field(field)
            .unwrap_or_else(|| panic!("{field} has no snapshot"));
        assert_eq!(
            snap.units()
                .unwrap_or_else(|| panic!("{field} serves no units leaf"))
                .as_str_lossy(),
            "cts",
            "mcaRecord.c:884-890 has no field test"
        );
    }
}

/// The boundary the calibration four sit on: 6 there, `PREC` everywhere else.
/// `recGblGetPrec` on a DBF_DOUBLE only clamps (`recGbl.c:135-139`), so the
/// `pmca->prec` seed survives the fall-through.
#[test]
fn mca_precision_is_prec_except_the_calibration_four() {
    let inst = mca();

    let prec_of = |field: &str| {
        inst.snapshot_for_field(field)
            .unwrap_or_else(|| panic!("{field} has no snapshot"))
            .precision()
            .unwrap_or_else(|| panic!("{field} serves no precision leaf"))
    };

    for field in ["CALO", "CALS", "CALQ", "TTH"] {
        assert_eq!(prec_of(field), 6, "mcaRecord.c:899-903");
    }
    for field in ["DWEL", "PLTM"] {
        assert_eq!(prec_of(field), 3, "mcaRecord.c:897 seeds pmca->prec");
    }
}
