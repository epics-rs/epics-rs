//! A driver callback that reaches its record from inside a dset call on the
//! same thread runs when that call returns, ahead of the rest of the cycle.
//! C's devMotorAsyn queues its command from inside the motor record's dset
//! call (devMotorAsyn.c:641); on a port without `ASYN_CANBLOCK` the queue runs
//! `asynCallback` inline (asynManager.c:1555-1584), and it and the driver's
//! `statusCallback` take the record's `dbScanLock` again and process it
//! (devMotorAsyn.c:717-726, 755-762) before the dset call returns.
//!
//! One test per dset return the cycle runs owed callbacks at. `W` is
//! processed after it in the same hold, so a callback left to the set's
//! release logs after `W`.

mod declared_rows;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{
    AsyncDbHandle, DeviceDone, DeviceDoneSource, ProcStack, PvDatabase,
};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, IoIntrAction, IoIntrRun, ReadStart, WriteStart,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, Record};

type Log = Arc<Mutex<Vec<String>>>;

/// The dset call `Caller` calls back from, once.
#[derive(Clone, Copy)]
enum At {
    /// `read_begin`, answering this the first time and `Synchronous` after.
    ReadBegin(fn() -> ReadStart),
    Read,
    /// `write_begin`, answering `Pending`.
    WriteBegin,
    /// `read` on a callback-readback pass.
    Readback,
}

struct Caller {
    at: At,
    log: Log,
    handle: Arc<OnceLock<AsyncDbHandle>>,
    called: bool,
    queued: Option<DeviceDone>,
}

impl Caller {
    fn log(&self, what: &str) {
        self.log.lock().unwrap().push(what.into());
    }

    /// The driver calling back from inside the dset call, as a port without
    /// `ASYN_CANBLOCK` does.
    fn call_back(&mut self) -> bool {
        if std::mem::replace(&mut self.called, true) {
            return false;
        }
        let log = self.log.clone();
        let run = self
            .handle
            .get()
            .unwrap()
            .run_io_intr_callback_with("B", move |_| {
                log.lock().unwrap().push("callback".into());
                IoIntrAction::Skip
            });
        assert_eq!(run, IoIntrRun::Owed);
        true
    }
}

impl DeviceSupport for Caller {
    fn dtyp(&self) -> &str {
        "Caller"
    }
    fn output_callback_readback(&self) -> bool {
        matches!(self.at, At::Readback)
    }
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        self.log("begin");
        let At::ReadBegin(answer) = self.at else {
            return Ok(ReadStart::Synchronous);
        };
        if !self.call_back() {
            return Ok(ReadStart::Synchronous);
        }
        let answer = answer();
        if matches!(answer, ReadStart::Pending) {
            self.queued = Some(done.device_done());
        }
        Ok(answer)
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.log("read");
        if matches!(self.at, At::Read | At::Readback) {
            self.call_back();
        }
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        if !matches!(self.at, At::WriteBegin) {
            return Ok(WriteStart::Synchronous);
        }
        self.log("write");
        self.call_back();
        self.queued = Some(done.device_done());
        Ok(WriteStart::Pending)
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.log("write");
        Ok(())
    }
}

/// Logs each pass of `W`.
struct Witness(Log);

impl DeviceSupport for Witness {
    fn dtyp(&self) -> &str {
        "Witness"
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.0.lock().unwrap().push("witness".into());
        Ok(())
    }
}

/// `B`, of `b_type`, calls back from `at`; `A` processes `B` and then `W`,
/// and `B` forward-links to `W`.
async fn ioc(b_type: &str, at: At) -> (Arc<PvDatabase>, Log) {
    let log: Log = Arc::default();
    let handle: Arc<OnceLock<AsyncDbHandle>> = Arc::default();
    let devices = [
        ("ai", declared_rows::rows(&["Caller"], DbLinkType::Constant)),
        (
            "ao",
            declared_rows::rows(&["Caller", "Witness"], DbLinkType::Constant),
        ),
    ];
    let (l, h) = (log.clone(), handle.clone());
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            match ctx.dtyp {
                "Caller" => Some(Box::new(Caller {
                    at,
                    log: l.clone(),
                    handle: h.clone(),
                    called: false,
                    queued: None,
                }) as Box<dyn DeviceSupport>),
                "Witness" => Some(Box::new(Witness(l.clone())) as Box<dyn DeviceSupport>),
                _ => None,
            }
        })
        .db_string(
            &format!(
                r#"
                record(fanout, "A") {{ field(LNK1, "B") field(LNK2, "W") }}
                record({b_type}, "B") {{ field(DTYP, "Caller") field(FLNK, "W") }}
                record(ao, "W") {{ field(DTYP, "Witness") }}
                "#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    handle.set(db.async_handle()).ok().unwrap();
    (db, log)
}

fn logged(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[epics_macros_rs::epics_test]
async fn a_callback_owed_in_read_begin_runs_before_the_reentered_pass() {
    let (db, log) = ioc("ai", At::ReadBegin(|| ReadStart::Reentered)).await;
    db.process_record("B").await.unwrap();
    assert_eq!(
        logged(&log),
        [
            "begin", "callback", "begin", "read", "witness", "begin", "read", "witness"
        ]
    );
}

#[epics_macros_rs::epics_test]
async fn a_callback_owed_in_a_queued_read_runs_before_the_pass_returns() {
    let (db, log) = ioc("ai", At::ReadBegin(|| ReadStart::Pending)).await;
    db.process_record("A").await.unwrap();
    assert_eq!(logged(&log), ["begin", "callback", "witness"]);
}

#[epics_macros_rs::epics_test]
async fn a_callback_owed_in_read_runs_before_the_record_body() {
    let (db, log) = ioc("ai", At::Read).await;
    db.process_record("B").await.unwrap();
    assert_eq!(logged(&log), ["begin", "read", "callback", "witness"]);
}

#[epics_macros_rs::epics_test]
async fn a_callback_owed_in_a_callback_readback_runs_before_the_record_goes_on() {
    let (db, log) = ioc("ao", At::Readback).await;
    db.process_record_readback("B", &mut ProcStack::new())
        .await
        .unwrap();
    assert_eq!(logged(&log), ["read", "callback", "witness"]);
}

#[epics_macros_rs::epics_test]
async fn a_callback_owed_in_a_queued_write_runs_before_the_pass_returns() {
    let (db, log) = ioc("ao", At::WriteBegin).await;
    db.process_record("A").await.unwrap();
    assert_eq!(logged(&log), ["write", "callback", "witness"]);
}
