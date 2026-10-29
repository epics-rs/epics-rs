//! C `dbParseLink` makes an empty link CONSTANT (`dbStaticLib.c:2346-2349`),
//! so `devEpidSoft.c:110-112` and `devEpidSoftCallback.c:112-114` raise
//! SOFT/INVALID for an epid whose INP was never set, as for a literal.

#![cfg(tokio_backend)]

use std::collections::HashMap;

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;

async fn stat_sevr_after_process(dtyp: &str) -> (EpicsValue, EpicsValue) {
    std_rs::register_std_dbd();
    let db_str = format!(
        r#"
record(epid, "PID") {{
    field(DTYP, "{dtyp}") field(KP, "1.0") field(VAL, "100.0")
    field(DRVH, "1000") field(DRVL, "-1000") field(FBON, "1")
}}
"#
    );
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(&db_str, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    server
        .database()
        .put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    (
        server.get("PID.STAT").await.unwrap(),
        server.get("PID.SEVR").await.unwrap(),
    )
}

#[tokio::test]
async fn an_epid_with_no_inp_is_soft_invalid() {
    for dtyp in ["Soft Channel", "Async Soft Channel"] {
        assert_eq!(
            stat_sevr_after_process(dtyp).await,
            (EpicsValue::Short(15), EpicsValue::Short(3)),
            "{dtyp}: SOFT/INVALID"
        );
    }
}
