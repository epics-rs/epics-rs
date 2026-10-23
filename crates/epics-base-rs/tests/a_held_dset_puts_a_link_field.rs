//! A dset putting another record's link field from inside its own record's
//! hold, as StreamEpics.cc's `%(L.OUT)` redirect does: C `dbPutField` hands
//! it to `dbPutFieldLink` (`dbAccess.c:1261-1262`), which rewires the link,
//! moves the lock sets and processes nothing, and a CP link it writes
//! connects. `dbPut` refuses a link field (`dbAccess.c:1339-1346`).

mod calink_ioc;

use std::collections::HashMap;
use std::sync::Mutex;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{
    AsyncDbHandle, DeviceDoneSource, FieldPut, HeldInstance, PvDatabase,
};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, WriteStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

/// Each put's status, as the dset saw it.
static PUTS: Mutex<Vec<(&str, bool)>> = Mutex::new(Vec::new());

struct Held {
    db: Option<AsyncDbHandle>,
}

impl DeviceSupport for Held {
    fn bind_database(&mut self, db: AsyncDbHandle) {
        self.db = Some(db);
    }
    fn lends_held(&self) -> bool {
        true
    }
    fn write_begin_held(
        &mut self,
        held: &HeldInstance<'_>,
        _: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        let db = self.db.as_ref().unwrap();
        let addr = |pv: &str| db.name_to_dbaddr(pv, None).unwrap();
        let text = |t: &str| EpicsValue::String(t.into());
        let puts = [
            (
                "dbPutField L.OUT",
                matches!(
                    held.db_put_field(&addr("L.OUT"), text("T2 PP")),
                    FieldPut::Done(Ok(()))
                ),
            ),
            (
                "dbPutField C.INPA",
                matches!(
                    held.db_put_field(&addr("C.INPA"), text("T2 CP")),
                    FieldPut::Done(Ok(()))
                ),
            ),
            (
                "dbPut L.FLNK",
                held.db_put(&addr("L.FLNK"), text("T2")).is_ok(),
            ),
        ];
        PUTS.lock().unwrap().extend(puts);
        Ok(WriteStart::Completed)
    }
    fn read(&mut self, _: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Untouched))
    }
    fn write(&mut self, _: &mut dyn Record) -> CaResult<()> {
        unreachable!("write_begin completes")
    }
    fn dtyp(&self) -> &str {
        "held"
    }
}

static ROWS: [DevSup; 1] = [DevSup {
    name: "devHeld",
    choice: "held",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 4], false),
}];

const DB: &str = r#"
record(ao, "O") { field(DTYP, "held") field(OUT, "@x") }
record(ao, "L") { field(OUT, "T1 PP") }
record(ao, "T1") {}
record(ao, "T2") {}
record(calc, "C") { field(CALC, "A") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let devices: Vec<(&str, &[DevSup])> = vec![("ao", &ROWS[..])];
    let ioc = IocBuilder::new()
        .register_dynamic_device_support(&devices, |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "held").then(|| Box::new(Held { db: None }) as Box<dyn DeviceSupport>)
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

fn get(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => v.to_f64().unwrap().to_string(),
    }
}

fn set_id(db: &PvDatabase, record: &str) -> u64 {
    db.lock_set_of(record).unwrap().id
}

#[epics_macros_rs::epics_test]
async fn a_held_db_put_field_rewires_a_link_and_db_put_refuses_it() {
    let db = ioc().await;
    assert_eq!(set_id(&db, "L"), set_id(&db, "T1"));
    let flnk = get(&db, "L.FLNK");
    db.put_record_field_from_ca_no_notify("O", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(
        *PUTS.lock().unwrap(),
        [
            ("dbPutField L.OUT", true),
            ("dbPutField C.INPA", true),
            ("dbPut L.FLNK", false),
        ]
    );
    assert_eq!(get(&db, "L.OUT"), "T2 PP NMS");
    assert_eq!(get(&db, "L.FLNK"), flnk, "dbPut wrote nothing");
    assert_eq!(get(&db, "L"), "0", "dbPutFieldLink processes nothing");
    assert_eq!(set_id(&db, "L"), set_id(&db, "T2"), "the new target merged");
    assert_ne!(set_id(&db, "L"), set_id(&db, "T1"), "the old one split off");

    db.put_record_field_from_ca_no_notify("L", "VAL", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!(get(&db, "T2"), "3", "L writes its new target");
    assert_eq!(get(&db, "T1"), "0");

    db.put_record_field_from_ca_no_notify("T2", "VAL", EpicsValue::Double(5.0))
        .await
        .unwrap();
    calink_ioc::until("T2 drives C through the CP edge the put settled", || {
        get(&db, "C") == "5"
    })
    .await;
}
