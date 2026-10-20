//! throttle `process()` calls `checkLink` only inside `if (proc_flag)`, and
//! only while OUT is a CA link (`throttleRecord.c:285-294`): a value the
//! drive limits reject leaves OV as the last check read it.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::{LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const EXT_NC: i16 = 0;
const EXT_OK: i16 = 1;

const DB: &str = r#"
record(throttle, "T") {
  field(OUT, "R:X CA")
  field(DRVLH, "10")
  field(DRVLL, "0")
}
"#;

/// The CA link set, connected as its flag says.
struct Lset(Arc<AtomicBool>);

#[epics_base_rs::async_trait]
impl LinkSet for Lset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        None
    }
}

fn ov(db: &PvDatabase) -> i16 {
    db.get_pv("T.OV").unwrap().to_f64().unwrap() as i16
}

async fn put_val(db: &PvDatabase, val: f64) {
    db.put_record_field_from_ca_no_notify("T", "VAL", EpicsValue::Double(val))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_rejected_value_leaves_ov_unchecked() {
    std_rs::register_std_dbd();
    let up = Arc::new(AtomicBool::new(false));
    let lset = up.clone();
    let ioc = IocBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .register_link_type("ca", |db| async move {
            db.register_link_set("ca", Arc::new(Lset(lset))).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);

    // The init re-check moves OV once the link connects.
    up.store(true, Ordering::SeqCst);
    for _ in 0..400 {
        if ov(&db) == EXT_OK {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(ov(&db), EXT_OK);

    up.store(false, Ordering::SeqCst);
    put_val(&db, 20.0).await;
    assert_eq!(ov(&db), EXT_OK, "a rejected value checked the link");
    put_val(&db, 5.0).await;
    assert_eq!(ov(&db), EXT_NC, "an accepted value did not check the link");
}
