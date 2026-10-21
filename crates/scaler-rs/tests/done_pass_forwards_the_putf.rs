//! A scaler's `recGblFwdLink` runs on the pass that finishes the user count
//! (scalerRecord.c:470-481), so the PUTF a `dbpf` to CNT set stays with the
//! record across the count and goes out with that pass's FLNK
//! (recGbl.c:295-302).
//!
//! oracleScalerIoc (drvScalerSoft, `P=SR:`) on these records, SR:T idle:
//!
//! ```text
//! dbpf SR:S.CNT 1; dbpf SR:T.PROC 1; 0.3 s; dbpf SR:s1 100
//!   at 1.5 s  SR:S.CNT Done  SR:T.PACT 1  SR:T.RPRO 1  SR:S.PUTF 0  SR:TC 0
//!   at 2.5 s  SR:TC 1
//!   at 5.0 s  SR:TC 2
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use epics_base_rs::types::EpicsValue;
use scaler_rs::device_support::soft_port::SoftScalerPorts;

/// `scalerSupport.dbd`'s `device(scaler,INST_IO,devScalerAsyn,"Asyn Scaler")`.
static ASYN_SCALER: DevSup = DevSup {
    name: "devScalerAsyn",
    choice: "Asyn Scaler",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(7, &[2, 4, 5, 6], false),
};

const DB: &str = r#"
record(scaler, "SR:S") {
    field(DTYP, "Asyn Scaler")
    field(OUT,  "@asyn(scalerPort 0 0)")
    field(FREQ, "10")
    field(TP,   "1")
    field(DLY,  "0")
    field(FLNK, "SR:T")
}
record(calcout, "SR:T") { field(CALC, "1") field(ODLY, "2.0") field(OOPT, "Every Time") field(FLNK, "SR:TC") }
record(calc, "SR:TC") { field(CALC, "A+1") field(INPA, "SR:TC") }
record(longout, "SR:s1") {}
record(longout, "SR:s2") {}
record(longout, "SR:s3") {}
record(longout, "SR:s4") {}
record(longout, "SR:s5") {}
record(longout, "SR:s6") {}
record(longout, "SR:s7") {}
record(longout, "SR:s8") {}
"#;

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// `dbpf`: `dbPutField`, no put-notify.
async fn put(db: &PvDatabase, rec: &str, field: &str, v: EpicsValue) {
    db.put_record_field_from_ca_no_notify(rec, field, v)
        .await
        .unwrap();
}

async fn sleep_until(t0: Instant, secs: f64) {
    let at = t0 + Duration::from_secs_f64(secs);
    if let Some(left) = at.checked_duration_since(Instant::now()) {
        tokio::time::sleep(left).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_done_pass_forwards_the_putf_of_the_count() {
    let ports = Arc::new(SoftScalerPorts::default());
    ports.configure("scalerPort", 8, "SR:s%d").unwrap();
    let (name, factory) = scaler_rs::scaler_record_factory();
    let dsets = Arc::clone(&ports);
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("scaler", &ASYN_SCALER, move || {
            Box::new(dsets.device_support("scalerPort").expect("configured"))
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    ports.connect(&db);
    // `iocRun`: the done interrupt processes the record only once
    // `interruptAccept` is set.
    db.scan_control().run();

    let t0 = Instant::now();
    put(&db, "SR:S", "CNT", EpicsValue::Short(1)).await;
    put(&db, "SR:T", "PROC", EpicsValue::Double(1.0)).await;
    sleep_until(t0, 0.3).await;
    put(&db, "SR:s1", "VAL", EpicsValue::Long(100)).await;
    sleep_until(t0, 1.5).await;
    let mid = ["SR:S.CNT", "SR:T.PACT", "SR:T.RPRO", "SR:S.PUTF"].map(|pv| num(&db, pv));
    let mut seen = vec![num(&db, "SR:TC")];
    for at in [2.5, 5.0] {
        sleep_until(t0, at).await;
        seen.push(num(&db, "SR:TC"));
    }
    assert_eq!(
        (mid, seen),
        ([0.0, 1.0, 1.0, 0.0], vec![0.0, 1.0, 2.0]),
        "CNT, SR:T PACT/RPRO, PUTF at 1.5 s; SR:TC at 1.5, 2.5 and 5 s"
    );
}
