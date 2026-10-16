//! `DTYP "Async Soft Channel"` on an output is C's `devXxxSoftCallback`:
//!
//! ```c
//! if (prec->pact)
//!     return 0;
//! status = dbPutLinkAsync(plink, DBR_DOUBLE, &prec->oval, 1);
//! if (!status)
//!     prec->pact = TRUE;
//! else if (status == S_db_noLSET)
//!     status = dbPutLink(plink, DBR_DOUBLE, &prec->oval, 1);
//! ```
//!
//! (`devAoSoftCallback.c:41-54`). A CA/pva link's `putAsync` takes the put and
//! the record stays PACT until `dbCaCallbackProcess` completes it; a link
//! without one puts synchronously; a refused `putAsync` raises the link alarm
//! (`dbLink.c:459-473`) and leaves PACT clear. The output used to be applied
//! synchronously on every link.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::database::{LinkPutOp, LinkSet, PutAdmission, PvDatabase};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// Records each put; a put to a gated PV waits for a permit, so the test sees
/// the record while the put is in flight.
struct Lset {
    writes: Arc<Mutex<Vec<(f64, LinkPutOp)>>>,
    admission: PutAdmission,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl LinkSet for Lset {
    fn is_connected(&self, _: &[u8]) -> bool {
        self.admission == PutAdmission::Connected
    }
    fn put_admission(&self, _: &[u8]) -> PutAdmission {
        self.admission
    }
    fn get_cached_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }
    async fn put_value(&self, _: &[u8], value: EpicsValue, op: LinkPutOp) -> Result<(), String> {
        self.release
            .acquire()
            .await
            .expect("semaphore is never closed")
            .forget();
        self.writes
            .lock()
            .unwrap()
            .push((value.to_f64().expect("tests only put doubles"), op));
        Ok(())
    }
}

struct Rig {
    db: PvDatabase,
    writes: Arc<Mutex<Vec<(f64, LinkPutOp)>>>,
    release: Arc<tokio::sync::Semaphore>,
}

async fn rig(admission: PutAdmission) -> Rig {
    rig_on("ca", admission).await
}

async fn rig_on(scheme: &str, admission: PutAdmission) -> Rig {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let db = PvDatabase::new();
    db.register_link_set(
        scheme,
        Arc::new(Lset {
            writes: Arc::clone(&writes),
            admission,
            release: Arc::clone(&release),
        }),
    )
    .await;
    Rig {
        db,
        writes,
        release,
    }
}

async fn add_ao(db: &PvDatabase, name: &str, out: &str, val: f64) {
    db.add_record(name, Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.common.dtyp = "Async Soft Channel".into();
    inst.put_common_field("OUT", EpicsValue::String(out.into()))
        .unwrap();
    inst.common.udf = 0;
    inst.record
        .put_field("VAL", EpicsValue::Double(val))
        .unwrap();
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(name, &mut visited)
        .await
        .unwrap();
}

fn pact(db: &PvDatabase, name: &str) -> bool {
    db.get_record(name).unwrap().read().is_processing()
}

fn alarm(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

async fn idle(db: &PvDatabase, name: &str) {
    for _ in 0..400 {
        if !pact(db, name) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{name} never completed");
}

/// A CA or pva link's `putAsync` takes the put: PACT until the put
/// completes, then the completing pass (`if (prec->pact) return 0`) puts
/// nothing more.
async fn an_external_link_holds_pact_until_the_put_completes(scheme: &str) {
    let r = rig_on(scheme, PutAdmission::Connected).await;
    add_ao(&r.db, "AO_CB", &format!("{scheme}://REMOTE:OUT"), 4.5).await;

    process(&r.db, "AO_CB").await;
    assert!(pact(&r.db, "AO_CB"), "putAsync took the put: PACT is set");

    r.release.add_permits(8);
    r.db.sync_external_link_puts().await;
    idle(&r.db, "AO_CB").await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
    r.db.sync_external_link_puts().await;

    assert_eq!(
        r.writes.lock().unwrap().as_slice(),
        [(4.5, LinkPutOp::Async)],
        "one put, with its completion; the completing pass puts nothing"
    );
    assert_eq!(
        alarm(&r.db, "AO_CB"),
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm)
    );
}

#[epics_macros_rs::epics_test]
async fn a_ca_link_holds_pact_until_the_put_completes() {
    an_external_link_holds_pact_until_the_put_completes("ca").await;
}

#[epics_macros_rs::epics_test]
async fn a_pva_link_holds_pact_until_the_put_completes() {
    an_external_link_holds_pact_until_the_put_completes("pva").await;
}

/// A DB link has no `putAsync` (`S_db_noLSET`): `dbPutLink` puts at once and
/// the record finishes in the same pass.
#[epics_macros_rs::epics_test]
async fn a_db_link_puts_synchronously() {
    let r = rig(PutAdmission::Connected).await;
    r.db.add_record("TARGET", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    add_ao(&r.db, "AO_DB", "TARGET", 2.5).await;

    process(&r.db, "AO_DB").await;

    assert!(!pact(&r.db, "AO_DB"), "no putAsync: the record is idle");
    assert_eq!(r.db.get_pv("TARGET").unwrap(), EpicsValue::Double(2.5));
    assert!(r.writes.lock().unwrap().is_empty());
}

/// A refused `putAsync` (`dbCa.c:558-561`, link down) returns its status:
/// `dbPutLinkAsync` raises LINK/INVALID and `write_ao` leaves PACT clear.
#[epics_macros_rs::epics_test]
async fn a_refused_put_raises_the_link_alarm_and_stays_idle() {
    let r = rig(PutAdmission::Refused).await;
    add_ao(&r.db, "AO_DOWN", "ca://REMOTE:OUT", 1.0).await;

    process(&r.db, "AO_DOWN").await;
    r.db.sync_external_link_puts().await;

    assert!(!pact(&r.db, "AO_DOWN"));
    assert_eq!(
        alarm(&r.db, "AO_DOWN"),
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid)
    );
    assert!(r.writes.lock().unwrap().is_empty());
}
