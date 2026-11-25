//! A failed link inside `do_pid` sets LINK/INVALID. The INP read
//! (`devEpidSoft.c:113-116`, `devEpidSoftCallback.c:149-152`) and the TRIG put
//! (`devEpidSoftCallback.c:121-124` and `:136-139`) then return before the PID,
//! and `epidRecord.c:207-214` still runs the tail. The OUTL read on the
//! feedback-on edge (`devEpidSoft.c:154-157`) and the OUTL put (`:220-224`)
//! carry on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{LinkPutOp, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// A link set whose reads fail, and whose puts land only if `put_ok`.
struct Dead {
    connected: bool,
    put_ok: bool,
}

#[epics_base_rs::async_trait]
impl LinkSet for Dead {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        self.connected
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: LinkRef<'_>, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        if self.put_ok {
            Ok(())
        } else {
            Err("put failed".into())
        }
    }
}

fn get(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// `PID` holds VAL 100 from a constant STPL, reads `SRC` (1.0) unless `inp`
/// names another link, and has feedback on; `fields` adds the rest.
async fn process_once(dtyp: &str, inp: &str, fields: &str, lset: Dead) -> Arc<PvDatabase> {
    std_rs::register_std_dbd();
    let db = format!(
        r#"
record(ao, "SRC") {{ field(VAL, "1.0") }}
record(ao, "OUT") {{ }}
record(epid, "PID") {{
    field(DTYP, "{dtyp}") field(STPL, "100") field(KP, "1") field(MDT, "0")
    field(DRVH, "1000") field(DRVL, "-1000") field(INP, "{inp}") field(FBON, "1")
    {fields}
}}
"#
    );
    let lset = Arc::new(lset);
    let (ca, pva) = (lset.clone(), lset);
    let ioc = IocBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_link_type("ca", move |db| async move {
            db.register_link_set("ca", ca).await;
            Vec::new()
        })
        .register_link_type("pva", move |db| async move {
            db.register_link_set("pva", pva).await;
            Vec::new()
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db.put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    // Long enough for a CA trigger's re-entry to have run.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    db
}

fn assert_link_invalid(db: &PvDatabase) {
    assert_eq!(get(db, "PID.STAT") as u16, alarm_status::LINK_ALARM);
    assert_eq!(get(db, "PID.SEVR") as u16, 3, "INVALID");
}

/// The PID did not run: CVAL was not read, P and OVAL kept 0, OUTL was not
/// written, and PACT is clear.
fn assert_no_pid(db: &PvDatabase) {
    assert_eq!(get(db, "PID.CVAL"), 0.0);
    assert_eq!(get(db, "PID.P"), 0.0);
    assert_eq!(get(db, "PID.OVAL"), 0.0);
    assert_eq!(get(db, "OUT"), 0.0);
    assert_eq!(get(db, "PID.PACT"), 0.0);
}

const DOWN: Dead = Dead {
    connected: false,
    put_ok: false,
};

/// `devEpidSoftCallback.c:136-139`: `dbCaPutLinkCallback` on an unconnected
/// link returns -1, so the epid neither waits nor runs the PID.
#[epics_base_rs::epics_test]
async fn a_disconnected_ca_trig_is_link_invalid_and_skips_the_pid() {
    let db = process_once(
        "Async Soft Channel",
        "SRC",
        r#"field(TRIG, "R:T CA") field(TVAL, "42") field(OUTL, "OUT")"#,
        DOWN,
    )
    .await;
    assert_link_invalid(&db);
    assert_no_pid(&db);
}

/// `devEpidSoftCallback.c:121-124`: a `{pva:}` TRIG is a `JSON_LINK`, so it
/// takes the `dbPutLink` arm, which fails at once on a link that is not
/// connected (`pvalink_lset.cpp:617`) and returns the same way.
#[epics_base_rs::epics_test]
async fn a_failed_non_ca_trig_put_is_link_invalid_and_skips_the_pid() {
    let db = process_once(
        "Async Soft Channel",
        "SRC",
        r#"field(TRIG, "{pva:\"R:T\"}") field(TVAL, "42") field(OUTL, "OUT")"#,
        DOWN,
    )
    .await;
    assert_link_invalid(&db);
    assert_no_pid(&db);
}

/// `devEpidSoft.c:113-116`.
#[epics_base_rs::epics_test]
async fn a_failed_inp_read_is_link_invalid_and_skips_the_pid() {
    let db = process_once("Soft Channel", "R:I CA", r#"field(OUTL, "OUT")"#, DOWN).await;
    assert_link_invalid(&db);
    assert_no_pid(&db);
}

/// `devEpidSoft.c:154-157`: the integral term keeps 0 (KI is 0 anyway), the
/// PID runs on, and its OVAL is put to OUTL (`:220-224`), which lands.
#[epics_base_rs::epics_test]
async fn a_failed_outl_read_on_the_feedback_edge_is_link_invalid() {
    let db = process_once(
        "Soft Channel",
        "SRC",
        r#"field(OUTL, "{pva:\"R:O\"}")"#,
        Dead {
            connected: true,
            put_ok: true,
        },
    )
    .await;
    assert_link_invalid(&db);
    assert_eq!(get(&db, "PID.CVAL"), 1.0);
    assert_eq!(get(&db, "PID.P"), 99.0);
    assert_eq!(get(&db, "PID.OVAL"), 99.0);
    assert_eq!(get(&db, "PID.FBOP"), 1.0);
}

/// `devEpidSoft.c:220-224`: on the cycle after the feedback-on edge OUTL is
/// not read, and the failed put alone raises LINK/INVALID after the PID ran.
#[epics_base_rs::epics_test]
async fn a_failed_outl_put_is_link_invalid_after_the_pid() {
    let db = process_once("Soft Channel", "SRC", r#"field(OUTL, "R:O CA")"#, DOWN).await;
    db.put_record_field_from_ca("PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_link_invalid(&db);
    assert_eq!(get(&db, "PID.P"), 99.0);
    assert_eq!(get(&db, "PID.OVAL"), 99.0);
    assert_eq!(get(&db, "PID.FBOP"), 1.0);
}
