//! The `SPC_NOMOD` fields of timestamp, throttle and epid: `put_field`
//! stores them like any field, as C's `dbPutString` does at load, and a
//! runtime put is refused by the database's NOMOD gate alone, as C's
//! `dbPutSpecial` pass 0 refuses it (`dbAccess.c:122-127`). epid DTP and
//! FBOP carry no `special(SPC_NOMOD)` (`epidRecord.dbd`), so a client put
//! to them lands.

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

const CASES: [(&str, &[&str]); 3] = [
    ("timestamp", &["OVAL"]),
    (
        "throttle",
        &[
            "OVAL", "SENT", "OSENT", "WAIT", "DRVLS", "VER", "STS", "OV", "SIV",
        ],
    ),
    (
        "epid",
        &[
            "CVAL", "CVLP", "OVAL", "OVLP", "P", "PP", "D", "DP", "ERR", "ERRP", "LALM", "ALST",
            "MLST",
        ],
    ),
];

fn record(rtype: &str) -> Box<dyn Record> {
    match rtype {
        "timestamp" => Box::new(std_rs::TimestampRecord::default()),
        "throttle" => Box::new(std_rs::ThrottleRecord::default()),
        _ => Box::new(std_rs::records::epid::EpidRecord::default()),
    }
}

#[test]
fn put_field_stores_a_nomod_field() {
    for (rtype, fields) in CASES.into_iter().chain([("epid", &["DTP", "FBOP"][..])]) {
        let mut rec = record(rtype);
        for field in fields {
            let value = rec.get_field(field).unwrap();
            rec.put_field(field, value)
                .unwrap_or_else(|e| panic!("{rtype}.{field}: {e}"));
        }
    }
}

#[epics_base_rs::epics_test]
async fn a_runtime_put_to_a_nomod_field_is_refused_by_the_gate() {
    for (rtype, fields) in CASES {
        let (db, _) = IocBuilder::new()
            .register_record_type(rtype, move || record(rtype))
            .db_string(&format!("record({rtype}, \"R\") {{\n}}\n"), &HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        for field in fields {
            let before = db.get_pv(format!("R.{field}")).unwrap();
            assert!(
                matches!(
                    db.check_external_put_preconditions("R", field).await,
                    Err(CaError::ReadOnlyField(_))
                ),
                "{rtype}.{field}: check_no_mod must refuse it"
            );
            let put = db
                .put_record_field_from_ca("R", field, before.clone())
                .await;
            assert!(
                matches!(put, Err(CaError::ReadOnlyField(_))),
                "{rtype}.{field}: {put:?}"
            );
            assert_eq!(db.get_pv(format!("R.{field}")).unwrap(), before);
        }
    }
}

#[epics_base_rs::epics_test]
async fn epid_dtp_and_fbop_take_a_runtime_put() {
    let (db, _) = IocBuilder::new()
        .register_record_type("epid", || record("epid"))
        .db_string("record(epid, \"R\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for (field, value) in [
        ("DTP", EpicsValue::Double(0.5)),
        ("FBOP", EpicsValue::Short(1)),
    ] {
        db.put_record_field_from_ca("R", field, value.clone())
            .await
            .unwrap_or_else(|e| panic!("epid.{field}: {e}"));
        assert_eq!(
            db.get_pv(format!("R.{field}")).unwrap().to_f64(),
            value.to_f64(),
            "epid.{field}"
        );
    }
}
