//! A dset that [`DeviceSupport::lends_held`] gets its record lent around the
//! dset call, and puts through it as C device support calls `dbPut` and
//! `dbPutField` with its own record locked (StreamEpics.cc's `matchValue`
//! redirect): the status comes back to the dset at once, the target's links
//! run, and the record's own fields, `dbCommon` included, are written. Its
//! `init_record` is lent the same way in either init pass (StreamEpics.cc's
//! `@init` redirect, a plain `dbPut` that processes nothing), and so is the
//! init [`wire_device_to_record`] runs.

use std::collections::HashMap;
use std::sync::Mutex;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{
    AsyncDbHandle, DeviceDoneSource, FieldPut, HeldInstance, InitHold, PvDatabase,
};
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart, WriteStart,
    wire_device_to_record,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// Each put's status, as the dset saw it.
static PUTS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
/// Whether each record's init puts answered Ok.
static INITS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

struct Held {
    name: String,
    db: Option<AsyncDbHandle>,
}

impl Held {
    /// `T.VAL` by dbPutField, the refused `D.VAL` by dbPutField, and the own
    /// record's DESC by dbPut.
    fn puts(&self, held: &HeldInstance<'_>, val: f64, desc: &str) {
        let db = self.db.as_ref().unwrap();
        let addr = |pv: &str| db.name_to_dbaddr(pv, None).unwrap();
        let mut puts = Vec::new();
        let r = held.db_put_field(&addr("T.VAL"), EpicsValue::Double(val));
        puts.push((
            format!("{} T.VAL", self.name),
            matches!(r, FieldPut::Done(Ok(()))),
        ));
        let r = held.db_put_field(&addr("D.VAL"), EpicsValue::Double(val));
        puts.push((
            format!("{} D.VAL", self.name),
            matches!(r, FieldPut::Done(Ok(()))),
        ));
        let own = addr(&format!("{}.DESC", self.name));
        let r = held.db_put(&own, EpicsValue::String(desc.into()));
        puts.push((format!("{} DESC", self.name), r.is_ok()));
        PUTS.lock().unwrap().extend(puts);
    }
}

impl DeviceSupport for Held {
    fn bind_database(&mut self, db: AsyncDbHandle) {
        self.db = Some(db);
    }
    fn set_record_info(&mut self, name: &str, _: epics_base_rs::server::record::ScanType) {
        self.name = name.to_owned();
    }
    fn lends_held(&self) -> bool {
        true
    }
    /// `I<name>.VAL` and the own DESC, by dbPut.
    fn init_held(&mut self, hold: &mut InitHold<'_>) -> CaResult<DeviceInitOutcome> {
        let db = self.db.as_ref().unwrap();
        let addr = |pv: &str| db.name_to_dbaddr(pv, None).unwrap();
        let (other, own) = hold.held(|held| {
            let other = held.db_put(
                &addr(&format!("I{}.VAL", self.name)),
                EpicsValue::Double(5.0),
            );
            let own = held.db_put(
                &addr(&format!("{}.DESC", self.name)),
                EpicsValue::String("init".into()),
            );
            (other, own)
        });
        INITS
            .lock()
            .unwrap()
            .push((self.name.clone(), other.is_ok() && own.is_ok()));
        // Status 2, so mbboDirect folds its B0..B1F.
        Ok(DeviceInitOutcome::DoNotConvert(DeviceUdf::Untouched))
    }
    fn read_begin_held(
        &mut self,
        held: &HeldInstance<'_>,
        _: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.puts(held, 7.0, "read");
        Ok(ReadStart::Synchronous)
    }
    fn write_begin_held(
        &mut self,
        held: &HeldInstance<'_>,
        _: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        self.puts(held, 8.0, "write");
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
record(ai, "A") { field(DTYP, "held") field(INP, "@x") }
record(ao, "O") { field(DTYP, "held") field(OUT, "@x") }
record(ao, "T") { field(FLNK, "N") }
record(calc, "N") { field(CALC, "VAL+1") }
record(ao, "D") { field(DISP, "1") }
record(aai, "W") { field(DTYP, "held") field(INP, "@x") field(FTVL, "DOUBLE") field(NELM, "2") }
record(ao, "IA") { field(FLNK, "J") }
record(ao, "IO") { field(FLNK, "J") }
record(ao, "IW") { field(FLNK, "J") }
record(mbboDirect, "MD") { field(DTYP, "held") field(OUT, "@x") field(B0, "1") }
record(ao, "IMD") { field(FLNK, "J") }
record(calc, "J") { field(CALC, "VAL+1") }
"#;

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let devices: Vec<(&str, &[DevSup])> = vec![
        ("ai", &ROWS[..]),
        ("ao", &ROWS[..]),
        ("aai", &ROWS[..]),
        ("mbboDirect", &ROWS[..]),
    ];
    let ioc = IocBuilder::new()
        .register_dynamic_device_support(&devices, |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "held").then(|| {
                Box::new(Held {
                    name: String::new(),
                    db: None,
                }) as Box<dyn DeviceSupport>
            })
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

#[epics_macros_rs::epics_test]
async fn read_begin_and_write_begin_put_through_the_lent_record() {
    let db = ioc().await;
    PUTS.lock().unwrap().clear();
    db.put_record_field_from_ca_no_notify("A", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    assert_eq!(get(&db, "T"), "7");
    assert_eq!(get(&db, "N"), "1", "T's FLNK ran inside A's read_begin");
    assert_eq!(get(&db, "A.DESC"), "read");
    db.put_record_field_from_ca_no_notify("O", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    assert_eq!(get(&db, "T"), "8");
    assert_eq!(get(&db, "N"), "2", "T's FLNK ran inside O's write_begin");
    assert_eq!(get(&db, "O.DESC"), "write");
    assert_eq!(get(&db, "D"), "0");
    let puts = PUTS.lock().unwrap().clone();
    let want = [
        ("A T.VAL", true),
        ("A D.VAL", false),
        ("A DESC", true),
        ("O T.VAL", true),
        ("O D.VAL", false),
        ("O DESC", true),
    ];
    assert_eq!(
        puts,
        want.map(|(p, ok)| (p.to_string(), ok)),
        "the DISP refusal answers the dset"
    );
}

/// aai inits its dset in pass 0 (aaiRecord.c:108-120), ai and ao in pass 1.
#[epics_macros_rs::epics_test]
async fn init_puts_through_the_lent_record_in_either_pass() {
    let db = ioc().await;
    let mut inits = INITS.lock().unwrap().clone();
    inits.sort();
    assert_eq!(
        inits,
        ["A", "MD", "O", "W"].map(|r| (r.to_string(), true)),
        "every init put answered Ok"
    );
    for r in ["A", "MD", "O", "W"] {
        assert_eq!(get(&db, &format!("I{r}")), "5", "{r}'s put landed");
        assert_eq!(get(&db, &format!("I{r}.UDF")), "0");
        assert_eq!(get(&db, &format!("{r}.DESC")), "init");
    }
    assert_eq!(get(&db, "J"), "0", "dbPut processes nothing");
    // mbboDirectRecord.c:141-157's B0..B1F fold, pass 1's tail after the
    // dset init.
    assert_eq!(get(&db, "MD"), "1");
    assert_eq!(get(&db, "MD.UDF"), "0");
}

#[epics_macros_rs::epics_test]
async fn wire_device_to_record_lends_the_held_init() {
    let db = PvDatabase::new();
    db.add_record("WI", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("IWI", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("WI").unwrap();
    rec.write().common.dtyp = "held".into();
    let dev = Held {
        name: String::new(),
        db: None,
    };
    wire_device_to_record(&db, &rec, Box::new(dev));
    assert!(
        INITS.lock().unwrap().contains(&("WI".to_string(), true)),
        "init_held ran and its puts answered Ok"
    );
    assert_eq!(get(&db, "IWI"), "5");
    assert_eq!(get(&db, "IWI.UDF"), "0");
    assert_eq!(get(&db, "WI.DESC"), "init");
}
