//! C binds `prec->dset` once at iocInit (`iocInit.c:529`); a later DTYP put
//! leaves it. The TRIG write belongs to `devEpidSoftCallback`
//! (`devEpidSoftCallback.c:120-132`) and not to `devEpidSoft`, so it follows
//! the dset the epid was loaded with, whatever DTYP is put afterwards.
//! rs-only pin: no built C std module is present to run softIoc with epid.
//! The menu is `stdSupport.dbd:13-14`'s, which std-rs does not register.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;

static EPID_MENU: &[&str] = &["Soft Channel", "Async Soft Channel"];

async fn trig_fires_after_dtyp_put(load: &str, put: &str) -> bool {
    epics_base_rs::server::record::register_device_menu("epid", EPID_MENU);
    let db_str = format!(
        r#"
record(ao, "SRC") {{ field(VAL, "1.0") }}
record(epid, "PID") {{
    field(DTYP, "{load}") field(KP, "1.0") field(VAL, "100.0")
    field(DRVH, "1000") field(DRVL, "-1000") field(INP, "SRC")
    field(TRIG, "SRC PP") field(TVAL, "42.0") field(FBON, "1")
}}
"#
    );
    let server = CaServerBuilder::new()
        .port(0)
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_device_support("Async Soft Channel", || {
            Box::new(
                std_rs::device_support::epid_soft_callback::EpidSoftCallbackDeviceSupport::new(),
            )
        })
        .db_string(&db_str, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();
    let index = EPID_MENU.iter().position(|n| *n == put).unwrap() as u16;
    db.put_record_field_from_ca("PID", "DTYP", EpicsValue::Enum(index))
        .await
        .unwrap();
    assert_eq!(
        server.get("PID.DTYP").await.unwrap(),
        EpicsValue::Enum(index)
    );
    db.put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.get("SRC").await.unwrap() == EpicsValue::Double(42.0)
}

#[tokio::test]
async fn a_runtime_dtyp_put_keeps_the_epid_trig_dset_iocinit_529() {
    assert!(
        !trig_fires_after_dtyp_put("Soft Channel", "Async Soft Channel").await,
        "devEpidSoft bound at init writes no TRIG"
    );
    assert!(
        trig_fires_after_dtyp_put("Async Soft Channel", "Soft Channel").await,
        "devEpidSoftCallback bound at init still writes TRIG"
    );
}
