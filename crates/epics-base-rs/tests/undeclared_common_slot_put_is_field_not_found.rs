//! A put to a `CommonFields` slot the record type's `.dbd` does not declare is
//! `S_dbLib_fieldNotFound`, as the read of it is.
//!
//! The port stores INP/OUT (and swait's OUTN) on `CommonFields` for every
//! record, so the store had a slot for `bi.OUT` even though `biRecord.dbd`
//! declares none: the put returned `Ok` while `get_pv` answered "not found".
//! C `dbNameToAddr` resolves nothing for such a name and `dbPutField` never
//! runs. swait is the case a database meets: its output is `OUTN`, and
//! `field(OUT,...)` is a load error.

use std::collections::HashMap;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

fn builder() -> IocBuilder {
    IocBuilder::new().register_record_type("swait", || Box::new(SwaitRecord::default()))
}

#[epics_macros_rs::epics_test]
async fn an_undeclared_inp_or_out_put_is_field_not_found() {
    let db = builder()
        .db_string(
            r#"
record(ai, "A") {}
record(bi, "B") {}
record(calc, "C") {}
record(ao, "O") {}
record(swait, "W") { field(OUTN, "A") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for pv in [
        "A.OUT", "B.OUT", "C.OUT", "C.INP", "O.INP", "W.OUT", "W.INP",
    ] {
        let err = db
            .put_pv_no_process(pv, EpicsValue::String("A".into()))
            .await
            .unwrap_err();
        assert!(matches!(err, CaError::FieldNotFound(_)), "{pv}: {err:?}");
        assert!(db.get_pv(pv).is_err(), "{pv}");
    }
    // The declared slot on each side of the boundary still takes the put.
    for pv in ["A.INP", "O.OUT"] {
        db.put_pv_no_process(pv, EpicsValue::String("B".into()))
            .await
            .unwrap();
        let text = db.get_pv(pv).unwrap().to_string();
        assert!(text.starts_with('B'), "{pv}: {text}");
    }
    assert_eq!(
        db.get_pv("W.OUTN").unwrap(),
        EpicsValue::String("A".into()),
        "the refused W.OUT put leaves OUTN"
    );
}

#[epics_macros_rs::epics_test]
async fn field_out_on_a_swait_is_a_load_error() {
    let built = match builder().db_string(
        r#"record(swait, "X") { field(OUT, "T1") }"#,
        &HashMap::new(),
    ) {
        Err(e) => Err(e.to_string()),
        Ok(b) => b.build().await.map(|_| ()).map_err(|e| e.to_string()),
    };
    built.expect_err("C's swait dbd has no OUT");
}
