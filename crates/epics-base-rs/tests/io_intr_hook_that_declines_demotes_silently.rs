//! C `scanAdd` (`dbScan.c:280-289`) logs a dset with no `get_ioint_info`, but a
//! hook that runs and returns non-zero only demotes the record to Passive —
//! devAsynInt32's `getIoIntInfo` after a failed `initCommon` (`return -1`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, ScanType};

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

/// The default `io_intr_receiver`: no scan list.
struct NoList(&'static str);

impl DeviceSupport for NoList {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        self.0
    }
}

const DB: &str = r#"record(ai, "DECLINES") { field(DTYP, "Hook Declines") field(INP, "@x") field(SCAN, "I/O Intr") }
record(ai, "NOHOOK") { field(DTYP, "No Hook") field(INP, "@x") field(SCAN, "I/O Intr") }
"#;

#[epics_macros_rs::epics_test]
async fn a_declining_hook_demotes_without_a_line() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.to_string()));
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &DECLINES, || Box::new(NoList("Hook Declines")))
        .register_device_support("ai", &NO_HOOK, || Box::new(NoList("No Hook")))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    errlog_flush();
    let said: Vec<String> = heard
        .lock()
        .unwrap()
        .iter()
        .flat_map(|m| m.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|l| l.contains("scanAdd"))
        .collect();
    assert_eq!(
        said,
        ["recGblRecordError: scanAdd: I/O Intr not valid (no get_ioint_info)  PV: NOHOOK"]
    );
    for name in ["DECLINES", "NOHOOK"] {
        let rec = db.get_record(name).unwrap();
        assert_eq!(rec.read().common.scan, ScanType::Passive, "{name}");
    }
}
