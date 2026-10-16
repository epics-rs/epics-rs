//! `ReadStart::Reentered` runs the re-entered pass nested, as C mca does on a
//! port without `ASYN_CANBLOCK`: `queueRequest` runs `asynCallback` inline
//! (asynManager.c:1555-1584), which calls `prset->process` (devMcaAsyn.c:
//! 357-382). The passes' heads run in order and their rests innermost first,
//! each with its own monitor and FLNK — so a READ cycle reads twice and links
//! three times (mcaRecord.c:770-790).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::DeviceDoneSource;
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, ReadStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::DbFieldType;
use epics_base_rs::types::EpicsValue;

type Log = Arc<Mutex<Vec<String>>>;

/// Its first two `read_begin`s complete inline; the third reads.
struct Inline {
    log: Log,
    begins: usize,
    reads: f64,
}

impl DeviceSupport for Inline {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.begins += 1;
        self.log
            .lock()
            .unwrap()
            .push(format!("begin pact={}", done.pact()));
        Ok(if self.begins < 3 {
            ReadStart::Reentered
        } else {
            ReadStart::Synchronous
        })
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.reads += 1.0;
        self.log
            .lock()
            .unwrap()
            .push(format!("read {}", self.reads));
        record.set_val(EpicsValue::Double(self.reads))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Inline"
    }
}

/// The FLNK target: logs each pass.
struct Sink(Log);

impl DeviceSupport for Sink {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.0.lock().unwrap().push("flnk".into());
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Sink"
    }
}

#[epics_macros_rs::epics_test]
async fn an_inline_completion_runs_its_pass_nested() {
    let log: Log = Arc::default();
    let l = Arc::clone(&log);
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| match ctx.dtyp {
            "Inline" => Some(Box::new(Inline {
                log: Arc::clone(&l),
                begins: 0,
                reads: 0.0,
            }) as Box<dyn DeviceSupport>),
            "Sink" => Some(Box::new(Sink(Arc::clone(&l))) as Box<dyn DeviceSupport>),
            _ => None,
        })
        .db_string(
            r#"
            record(ai, "AI") { field(DTYP, "Inline") field(FLNK, "SINK") }
            record(ao, "SINK") { field(DTYP, "Sink") }
            "#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let mut val_rx = db
        .get_record("AI")
        .unwrap()
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
        .unwrap();
    while val_rx.try_recv().is_ok() {}

    db.process_record("AI").await.unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        [
            "begin pact=false",
            "begin pact=true",
            "begin pact=true",
            "read 1",
            "flnk",
            "read 2",
            "flnk",
            "read 3",
            "flnk",
        ]
    );
    // Each pass's monitor posts the value its own read left.
    let mut posted = Vec::new();
    while let Ok(e) = val_rx.try_recv() {
        posted.push(e.value().as_value().cloned().unwrap());
    }
    assert_eq!(posted, [1.0, 2.0, 3.0].map(EpicsValue::Double));
    let rec = db.get_record("AI").unwrap();
    assert!(!rec.read().is_processing(), "the outer pass ends the cycle");
    assert_eq!(db.get_pv("AI").unwrap(), EpicsValue::Double(3.0));
}
