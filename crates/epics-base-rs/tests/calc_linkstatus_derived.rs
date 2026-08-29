//! Calc-family link-status menus are DERIVED, read-only — not put-settable.
//!
//! acalcout/scalcout/transform each expose a `menu(...INAV/IAV)` field per
//! link (`INAV..INLV`, `IAAV..ILLV`, `OUTV`; transform `IAV..IPV`,
//! `OAV..OPV`), all `special(SPC_NOMOD)`. C `init_record` classifies the link
//! and stores `<rec>INAV_CON`(3) for a CONSTANT link
//! (`aCalcoutRecord.c:209-243`, `sCalcoutRecord.c`, `transformRecord.c:444-473`),
//! OVERWRITING the `.dbd` `initial("1")`. A default record's links are all
//! constant, so a `caget` reads `Constant`(3), and a direct `caput` is refused
//! (`S_db_noMod`) leaving that derived value standing.
//!
//! The oracle measured the divergence end-to-end: `caput ORACLE:ACALCOUT.INAV 0`
//! then `caget` returned `Ext PV OK`(1) on the port where C returns `Constant`.
//! Two independent bugs produced it — the loader seeded the raw `.dbd`
//! `initial("1")` into acalcout's modeled field (and scalcout/transform did not
//! model the field at all, so a caget fell through to that same `.dbd` initial),
//! and neither refused the seed. This drives the whole path through the loader.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(acalcout, "A") {}
record(scalcout, "S") {}
record(transform, "X") {}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Every link-status field of a default (all-constant-link) record reads
/// `Constant`(3) through the full DB-load path — NOT the `.dbd` `initial("1")`.
#[epics_macros_rs::epics_test]
async fn link_status_reads_derived_constant_after_load() {
    let db = build().await;
    let cases: &[(&str, &[&str])] = &[
        ("A", &["INAV", "INLV", "IAAV", "ILLV", "OUTV"]),
        ("S", &["INAV", "INLV", "IAAV", "ILLV", "OUTV"]),
        ("X", &["IAV", "IPV", "IOV", "OAV", "OPV"]),
    ];
    for (rec, fields) in cases {
        for f in *fields {
            let got = db.get_pv(&format!("{rec}.{f}")).unwrap();
            assert_eq!(
                got.to_f64(),
                Some(3.0),
                "{rec}.{f} should read Constant(3) after load, got {got:?}"
            );
        }
    }
}

/// A direct client put to a link-status field is refused (C `S_db_noMod`), and
/// the derived value is unchanged.
#[epics_macros_rs::epics_test]
async fn link_status_put_is_refused_and_value_stands() {
    let db = build().await;
    for (rec, f) in [("A", "INAV"), ("S", "IAAV"), ("X", "OAV"), ("X", "IAV")] {
        let err = db
            .put_record_field_from_ca(rec, f, EpicsValue::Enum(0))
            .await;
        assert!(
            err.is_err(),
            "{rec}.{f} is SPC_NOMOD — a client put must be refused, got {err:?}"
        );
        let got = db.get_pv(&format!("{rec}.{f}")).unwrap();
        assert_eq!(
            got.to_f64(),
            Some(3.0),
            "{rec}.{f} must stay at Constant(3) after the refused put, got {got:?}"
        );
    }
}
