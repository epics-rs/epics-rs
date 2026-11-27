//! C `scanAdd`/`scanDelete` ask the dset's `get_ioint_info` with `cmd` 0 when
//! a record joins the I/O Intr list, at `iocInit` and on a SCAN put, and
//! with `cmd` 1 when it leaves; a nonzero answer to `cmd` 0 leaves the record
//! Passive (dbScan.c:285, :343). StreamDevice starts and aborts its I/O Intr
//! protocol there (StreamEpics.cc streamGetIointInfo), and reads the
//! record's SCAN, the list being joined or left (`record->scan`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::{CaResult, LinkRefusal};
use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceSupport, IoIntrCmd};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record, ScanType};
use epics_base_rs::types::EpicsValue;

static HOOKED: DevSup = DevSup {
    name: "devAiHooked",
    choice: "Hooked",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
};

type Calls = Arc<Mutex<Vec<(String, IoIntrCmd, ScanType)>>>;

struct Hooked {
    name: String,
    calls: Calls,
    _pulses: Option<mpsc::Sender<()>>,
}

impl DeviceSupport for Hooked {
    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.name = name.to_string();
    }

    fn get_ioint_info(
        &mut self,
        _record: &mut dyn Record,
        common: &mut CommonFields,
        cmd: IoIntrCmd,
    ) -> bool {
        self.calls
            .lock()
            .unwrap()
            .push((self.name.clone(), cmd, common.scan));
        !self.name.starts_with("REFUSE")
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(1);
        self._pulses = Some(tx);
        Some(rx)
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Hooked"
    }
}

const DB: &str = r#"record(ai, "ON") { field(DTYP, "Hooked") field(INP, "@x") field(SCAN, "I/O Intr") }
record(ai, "REFUSE") { field(DTYP, "Hooked") field(INP, "@x") field(SCAN, "I/O Intr") }
record(ai, "LATER") { field(DTYP, "Hooked") field(INP, "@x") }
"#;

#[epics_macros_rs::epics_test]
async fn get_ioint_info_is_asked_on_each_io_intr_join_and_leave() {
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &HOOKED, move || {
            Box::new(Hooked {
                name: String::new(),
                calls: seen.clone(),
                _pulses: None,
            })
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let scan = |name: &str| db.get_record(name).unwrap().read().common.scan;
    assert_eq!(scan("ON"), ScanType::IoIntr);
    assert_eq!(scan("REFUSE"), ScanType::Passive);

    let intr = || EpicsValue::String("I/O Intr".into());
    let passive = || EpicsValue::String("Passive".into());
    db.put_pv("ON.SCAN", passive()).await.unwrap();
    db.put_pv("ON.SCAN", intr()).await.unwrap();
    // A move between two lists that are not I/O Intr asks nothing.
    db.put_pv("LATER.SCAN", EpicsValue::String("1 second".into()))
        .await
        .unwrap();
    db.put_pv("LATER.SCAN", intr()).await.unwrap();
    db.put_pv("REFUSE.SCAN", intr()).await.unwrap();
    assert_eq!(scan("ON"), ScanType::IoIntr);
    assert_eq!(scan("LATER"), ScanType::IoIntr);
    assert_eq!(scan("REFUSE"), ScanType::Passive);

    use IoIntrCmd::{Add, Delete};
    let calls = calls.lock().unwrap().clone();
    let of = |n: &str| -> Vec<(IoIntrCmd, ScanType)> {
        calls
            .iter()
            .filter(|c| c.0 == n)
            .map(|c| (c.1, c.2))
            .collect()
    };
    // Each ask sees the I/O Intr SCAN it joins or leaves.
    let intr = ScanType::IoIntr;
    assert_eq!(of("ON"), [(Add, intr), (Delete, intr), (Add, intr)]);
    assert_eq!(of("LATER"), [(Add, intr)]);
    // Refused at iocInit, so never on the list and never asked to leave.
    assert_eq!(of("REFUSE"), [(Add, intr), (Add, intr)]);
}

/// A dset with a `dsxt`, so a device-link put rebinds a fresh object.
static RELINKED: DevSup = DevSup {
    name: "devAiRelinked",
    choice: "Relinked",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

type RelinkCalls = Arc<Mutex<Vec<(usize, IoIntrCmd, ScanType)>>>;

/// Answers as the factory call that made it, refusing `cmd` 0 for a link
/// that says so, and `del_record` once it joined with a link that says so.
struct Relinked {
    id: usize,
    calls: RelinkCalls,
    keep: bool,
    _pulses: Option<mpsc::Sender<()>>,
}

impl DeviceSupport for Relinked {
    fn get_ioint_info(
        &mut self,
        _record: &mut dyn Record,
        common: &mut CommonFields,
        cmd: IoIntrCmd,
    ) -> bool {
        self.calls.lock().unwrap().push((self.id, cmd, common.scan));
        let inp = common.text.inp.as_str_lossy();
        if cmd == IoIntrCmd::Add {
            self.keep = inp.contains("keep");
        }
        !(cmd == IoIntrCmd::Add && inp.contains("refuse"))
    }

    fn del_record(&mut self) -> Result<(), LinkRefusal> {
        if self.keep {
            return Err(LinkRefusal::del_record("devAiRelinked"));
        }
        Ok(())
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(1);
        self._pulses = Some(tx);
        Some(rx)
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Relinked"
    }
}

/// C `dbPutFieldLink` on a device link of an I/O Intr record
/// (`dbAccess.c:1155-1158`, `:1218-1222`): `scanDelete` asks the old dset
/// `get_ioint_info(1)` before `del_record`, and `restoreScan`'s `scanAdd`
/// asks the new one `get_ioint_info(0)`, whose refusal leaves the record
/// Passive and off the I/O Intr list.
async fn relinked_ioc(inp: &str) -> (Arc<PvDatabase>, RelinkCalls) {
    let calls: RelinkCalls = Arc::default();
    let seen = calls.clone();
    let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &RELINKED, move || {
            Box::new(Relinked {
                id: made.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                calls: seen.clone(),
                keep: false,
                _pulses: None,
            })
        })
        .db_string(
            &format!(
                r#"record(ai, "RL") {{ field(DTYP, "Relinked") field(INP, "{inp}") field(SCAN, "I/O Intr") }}"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    (db, calls)
}

async fn listed(db: &PvDatabase) -> bool {
    db.records_for_scan(ScanType::IoIntr)
        .await
        .iter()
        .any(|n| n.as_bytes() == b"RL")
}

#[epics_macros_rs::epics_test]
async fn a_device_link_put_moves_an_io_intr_record_through_get_ioint_info() {
    let (db, calls) = relinked_ioc("@a").await;
    let scan = || db.get_record("RL").unwrap().read().common.scan;
    let listed = || listed(&db);
    let take = || std::mem::take(&mut *calls.lock().unwrap());
    let intr = ScanType::IoIntr;
    use IoIntrCmd::{Add, Delete};
    assert_eq!(take(), [(0, Add, intr)]);

    db.put_record_field_from_ca_no_notify("RL", "INP", EpicsValue::String("@b".into()))
        .await
        .unwrap();
    assert_eq!(take(), [(0, Delete, intr), (1, Add, intr)]);
    assert_eq!(scan(), intr);
    assert!(listed().await);

    db.put_record_field_from_ca_no_notify("RL", "INP", EpicsValue::String("@refuse".into()))
        .await
        .unwrap();
    assert_eq!(take(), [(1, Delete, intr), (2, Add, intr)]);
    assert_eq!(scan(), ScanType::Passive);
    assert!(!listed().await);
}

/// The `del_record` refusal's `goto restoreScan` (`dbAccess.c:1160-1164`,
/// `:1218-1222`): the old dset, still bound, is asked `get_ioint_info(0)`
/// again and the record goes back on the I/O Intr list.
#[epics_macros_rs::epics_test]
async fn a_del_record_refusal_puts_an_io_intr_record_back_on_its_list() {
    let (db, calls) = relinked_ioc("@keep").await;
    let take = || std::mem::take(&mut *calls.lock().unwrap());
    let intr = ScanType::IoIntr;
    use IoIntrCmd::{Add, Delete};
    assert_eq!(take(), [(0, Add, intr)]);

    let put = db
        .put_record_field_from_ca_no_notify("RL", "INP", EpicsValue::String("@b".into()))
        .await;
    assert!(put.is_err(), "{put:?}");
    assert_eq!(take(), [(0, Delete, intr), (0, Add, intr)]);
    let rec = db.get_record("RL").unwrap();
    assert_eq!(rec.read().common.scan, intr);
    assert_eq!(rec.read().common.text.inp.as_str_lossy(), "@keep");
    assert!(listed(&db).await);
}

static STICKY: DevSup = DevSup {
    name: "devAiSticky",
    choice: "Sticky",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], true),
};

/// What every `Sticky` object refuses, switched by the test.
#[derive(Default)]
struct Refuse {
    del_record: std::sync::atomic::AtomicBool,
    add: std::sync::atomic::AtomicBool,
}

struct Sticky {
    id: usize,
    calls: RelinkCalls,
    refuse: Arc<Refuse>,
    _pulses: Option<mpsc::Sender<()>>,
}

impl DeviceSupport for Sticky {
    fn get_ioint_info(
        &mut self,
        _record: &mut dyn Record,
        common: &mut CommonFields,
        cmd: IoIntrCmd,
    ) -> bool {
        self.calls.lock().unwrap().push((self.id, cmd, common.scan));
        !(cmd == IoIntrCmd::Add && self.refuse.add.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn io_intr_receiver(&mut self) -> Option<mpsc::Receiver<()>> {
        let (tx, rx) = mpsc::channel(1);
        self._pulses = Some(tx);
        Some(rx)
    }

    fn del_record(&mut self) -> Result<(), epics_base_rs::error::LinkRefusal> {
        if self
            .refuse
            .del_record
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(epics_base_rs::error::LinkRefusal::del_record("devAiSticky"));
        }
        Ok(())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Sticky"
    }
}

async fn sticky_ioc() -> (
    Arc<epics_base_rs::server::database::PvDatabase>,
    RelinkCalls,
    Arc<Refuse>,
) {
    let calls: RelinkCalls = Arc::default();
    let refuse: Arc<Refuse> = Arc::default();
    let (seen, refusing) = (calls.clone(), refuse.clone());
    let made = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &STICKY, move || {
            Box::new(Sticky {
                id: made.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                calls: seen.clone(),
                refuse: refusing.clone(),
                _pulses: None,
            })
        })
        .db_string(
            r#"record(ai, "ST") { field(DTYP, "Sticky") field(INP, "@a") field(SCAN, "I/O Intr") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    std::mem::take(&mut *calls.lock().unwrap());
    (db, calls, refuse)
}

async fn on_io_intr_list(db: &epics_base_rs::server::database::PvDatabase, name: &str) -> bool {
    db.records_for_scan(ScanType::IoIntr)
        .await
        .iter()
        .any(|n| n == name)
}

/// `dbPutFieldLink`'s `goto restoreScan` on a `del_record` refusal
/// (`dbAccess.c:1160-1166`, `:1218-1222`): the old dset goes back on the
/// I/O Intr list through `scanAdd`.
#[epics_macros_rs::epics_test]
async fn a_refused_del_record_puts_the_record_back_on_io_intr() {
    let (db, calls, refuse) = sticky_ioc().await;
    refuse
        .del_record
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let put = db
        .put_record_field_from_ca_no_notify("ST", "INP", EpicsValue::String("@b".into()))
        .await;
    assert!(put.is_err(), "{put:?}");
    use IoIntrCmd::{Add, Delete};
    let intr = ScanType::IoIntr;
    assert_eq!(*calls.lock().unwrap(), [(0, Delete, intr), (0, Add, intr)]);
    assert_eq!(db.get_record("ST").unwrap().read().common.scan, intr);
    assert!(on_io_intr_list(&db, "ST").await);
}

/// A refused `del_record` whose `restoreScan` `scanAdd` is refused too
/// (`dbScan.c:285-288`): the record reads Passive, and the scan index
/// follows it off the I/O Intr list.
#[epics_macros_rs::epics_test]
async fn a_refused_restore_leaves_the_io_intr_list() {
    let (db, _, refuse) = sticky_ioc().await;
    refuse
        .del_record
        .store(true, std::sync::atomic::Ordering::Relaxed);
    refuse.add.store(true, std::sync::atomic::Ordering::Relaxed);
    let put = db
        .put_record_field_from_ca_no_notify("ST", "INP", EpicsValue::String("@b".into()))
        .await;
    assert!(put.is_err(), "{put:?}");
    assert_eq!(
        db.get_record("ST").unwrap().read().common.scan,
        ScanType::Passive
    );
    assert!(!on_io_intr_list(&db, "ST").await);
}

/// `dbPutFieldLink`'s `postScanEvent` (`dbAccess.c:1223-1225`): a put that
/// leaves the record off I/O Intr posts SCAN.
#[epics_macros_rs::epics_test]
async fn a_device_link_put_that_ends_off_io_intr_posts_scan() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::types::DbFieldType;
    let (db, _, refuse) = sticky_ioc().await;
    let mut scan = db
        .get_record("ST")
        .unwrap()
        .write()
        .add_subscriber("SCAN", 1, DbFieldType::Enum, EventMask::VALUE.bits())
        .unwrap();
    while scan.try_recv().is_ok() {}
    refuse.add.store(true, std::sync::atomic::Ordering::Relaxed);
    db.put_record_field_from_ca_no_notify("ST", "INP", EpicsValue::String("@b".into()))
        .await
        .unwrap();
    assert_eq!(
        db.get_record("ST").unwrap().read().common.scan,
        ScanType::Passive
    );
    assert!(scan.try_recv().is_ok(), "no SCAN post");
}

/// A constant SIML that turns simulation on at init swaps SSCN in through
/// `recGblCheckSimm`'s `scanAdd` (recGbl.c:430-435) before `scanInit`'s
/// `buildScanLists` adds every record (dbScan.c:1073). C asks
/// `get_ioint_info(0)` at both and files the record on the list twice; it is
/// asked once.
#[epics_macros_rs::epics_test]
async fn a_scan_swapped_in_before_scan_init_is_added_once() {
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &HOOKED, move || {
            Box::new(Hooked {
                name: String::new(),
                calls: seen.clone(),
                _pulses: None,
            })
        })
        .db_string(
            r#"record(ai, "SIM") { field(DTYP, "Hooked") field(INP, "@x") field(SIML, "1") field(SSCN, "I/O Intr") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(
        db.get_record("SIM").unwrap().read().common.scan,
        ScanType::IoIntr
    );
    let adds = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.1 == IoIntrCmd::Add)
        .count();
    assert_eq!(adds, 1);
}
