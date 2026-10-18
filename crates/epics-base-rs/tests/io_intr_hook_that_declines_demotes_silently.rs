//! C `scanAdd` (`dbScan.c:280-293`) logs a dset with no `get_ioint_info`, and
//! a hook that accepts but hands back no list, but a hook that returns
//! non-zero only demotes the record to Passive — devAsynInt32's
//! `getIoIntInfo` after a failed `initCommon` (`return -1`). An iocInit join
//! and a later SCAN put say the same.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::device_support::IoIntrCmd;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{CommonFields, DbLinkType, DevSup, Dset, Record, ScanType};
use epics_base_rs::types::EpicsValue;

static DECLINES: DevSup = DevSup {
    name: "devAiDeclines",
    choice: "Hook Declines",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
};

static NO_HOOK: DevSup = DevSup {
    name: "devAiNoHook",
    choice: "No Hook",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 4], false),
};

static NO_LIST: DevSup = DevSup {
    name: "devAiNoList",
    choice: "No List",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
};

/// The default `io_intr_receiver`: no scan list. "Hook Declines" refuses
/// `get_ioint_info(0)`; the others accept it.
struct NoList(&'static str);

impl DeviceSupport for NoList {
    fn get_ioint_info(
        &mut self,
        _record: &mut dyn Record,
        _common: &mut CommonFields,
        _cmd: IoIntrCmd,
    ) -> bool {
        self.0 != "Hook Declines"
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        self.0
    }
}

const DB: &str = r#"record(ai, "DECLINES") { field(DTYP, "Hook Declines") field(INP, "@x") field(SCAN, "I/O Intr") }
record(ai, "NOHOOK") { field(DTYP, "No Hook") field(INP, "@x") field(SCAN, "I/O Intr") }
record(ai, "NOLIST") { field(DTYP, "No List") field(INP, "@x") field(SCAN, "I/O Intr") }
"#;

#[epics_macros_rs::epics_test]
async fn a_declining_hook_demotes_without_a_line() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.to_string()));
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &DECLINES, || Box::new(NoList("Hook Declines")))
        .register_device_support("ai", &NO_HOOK, || Box::new(NoList("No Hook")))
        .register_device_support("ai", &NO_LIST, || Box::new(NoList("No List")))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let said = || {
        errlog_flush();
        let said: Vec<String> = std::mem::take(&mut *heard.lock().unwrap())
            .iter()
            .flat_map(|m| m.lines().map(str::to_string).collect::<Vec<_>>())
            .filter(|l| l.contains("scanAdd"))
            .collect();
        said
    };
    let want = [
        "recGblRecordError: scanAdd: I/O Intr not valid (no get_ioint_info)  PV: NOHOOK",
        "recGblRecordError: scanAdd: I/O Intr not valid  PV: NOLIST",
    ];
    assert_eq!(said(), want);
    let scans = || {
        for name in ["DECLINES", "NOHOOK", "NOLIST"] {
            let rec = db.get_record(name).unwrap();
            assert_eq!(rec.read().common.scan, ScanType::Passive, "{name}");
        }
    };
    scans();

    for name in ["DECLINES", "NOHOOK", "NOLIST"] {
        db.put_pv(
            &format!("{name}.SCAN"),
            EpicsValue::String("I/O Intr".into()),
        )
        .await
        .unwrap();
    }
    assert_eq!(said(), want, "a SCAN put");
    scans();
}
