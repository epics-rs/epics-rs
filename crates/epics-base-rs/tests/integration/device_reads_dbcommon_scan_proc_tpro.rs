//! Device support reads `dbCommon`'s SCAN, PROC and TPRO on a read and on a
//! write, as StreamEpics.cc `Stream::process` does (`proc == 2` runs
//! `@init`), and a driver thread reaches the record's `dbCommon` under its
//! lock set through `AsyncDbHandle::with_instance`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, ProcessContext, Record, ScanType};
use epics_base_rs::types::EpicsValue;

static PEEK_IN: DevSup = DevSup {
    name: "devAiPeek",
    choice: "Peek",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
};

static PEEK_OUT: DevSup = DevSup {
    name: "devAoPeek",
    choice: "Peek",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 3, 4], false),
};

type Seen = Arc<Mutex<Vec<ProcessContext>>>;

struct Peek(Seen);

impl DeviceSupport for Peek {
    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.0.lock().unwrap().push(ctx.clone());
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Peek"
    }
}

const DB: &str = r#"record(ai, "I") { field(DTYP, "Peek") field(INP, "@x") field(SCAN, "1 second") }
record(ao, "O") { field(DTYP, "Peek") field(OUT, "@x") field(TPRO, "1") }
"#;

#[epics_macros_rs::epics_test]
async fn read_and_write_see_scan_proc_and_tpro() {
    let seen: Seen = Arc::default();
    let (s_in, s_out) = (seen.clone(), seen.clone());
    let (db, _) = IocBuilder::new()
        .register_device_support("ai", &PEEK_IN, move || Box::new(Peek(s_in.clone())))
        .register_device_support("ao", &PEEK_OUT, move || Box::new(Peek(s_out.clone())))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    seen.lock().unwrap().clear();
    db.put_record_field_from_ca_no_notify("O", "PROC", EpicsValue::String("2".into()))
        .await
        .unwrap();
    let ctx = seen
        .lock()
        .unwrap()
        .pop()
        .expect("the write saw no context");
    assert_eq!(
        (ctx.scan, ctx.proc_field, ctx.tpro),
        (ScanType::Passive, 2, 1)
    );

    seen.lock().unwrap().clear();
    db.put_record_field_from_ca_no_notify("I", "PROC", EpicsValue::String("1".into()))
        .await
        .unwrap();
    let ctx = seen.lock().unwrap().pop().expect("the read saw no context");
    assert_eq!((ctx.scan, ctx.proc_field, ctx.tpro), (ScanType::SEC1, 1, 0));

    let handle = db.async_handle();
    let got = std::thread::spawn(move || {
        let scan = handle.with_instance("I", |_, common| {
            common.nsev = epics_base_rs::server::record::AlarmSeverity::Major;
            common.scan
        });
        (scan, handle.with_instance("NOPE", |_, _| ()))
    })
    .join()
    .unwrap();
    assert_eq!(got, (Some(ScanType::SEC1), None));
    let rec = db.get_record("I").unwrap();
    assert_eq!(
        rec.read().common.nsev,
        epics_base_rs::server::record::AlarmSeverity::Major
    );
}
