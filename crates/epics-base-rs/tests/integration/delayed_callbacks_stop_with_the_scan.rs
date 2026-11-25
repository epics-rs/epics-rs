//! C `iocShutdown` stops the callback threads right after the scan facility
//! (iocInit.c:733-735), and `callbackStop` (callback.c:237-260) leaves every
//! callback still pending, delayed or not, unrun. So a seq group behind its
//! `DLYn` (seqRecord.c:214, `processCallback`) and a throttle's cooldown
//! `valuePut` (throttleRecord.c:595, `delayFuncCallback`) never land once
//! shutdown has begun; without it they land on time.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{ProcessAction, ProcessActions, ProcessOutcome, Record};
use epics_base_rs::types::EpicsValue;

const DELAY: f64 = 0.3;

/// C throttle's cooldown reduced to its timer: each `process()` arms
/// `delayFuncCb` for `DLY`, and `FIRES` counts the `delayFuncCallback` runs.
#[derive(Default)]
struct Cooldown {
    dly: f64,
    fires: i32,
}

impl Record for Cooldown {
    fn record_type(&self) -> &'static str {
        "cooldown"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete_with(vec![
            ProcessAction::ValuePutAfter(Duration::from_secs_f64(self.dly)),
        ]))
    }
    fn delayed_value_put(&mut self) -> ProcessActions {
        self.fires += 1;
        ProcessActions::new()
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "DLY" => Some(EpicsValue::Double(self.dly)),
            "FIRES" => Some(EpicsValue::Long(self.fires)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "DLY" => self.dly = value.to_f64().unwrap_or(0.0),
            _ => return Err(CaError::FieldNotFound(name.into())),
        }
        Ok(())
    }
}

const DB: &str = r#"
record(ao, "A") {}
record(ao, "B") {}
record(seq, "S") {
    field(DO0, "1")
    field(LNK0, "A.VAL PP")
    field(DLY1, "0.3")
    field(DO1, "2")
    field(LNK1, "B.VAL PP")
}
record(cooldown, "T") { field(DLY, "0.3") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("cooldown", || Box::new(Cooldown::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

fn read(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

fn fires(db: &PvDatabase) -> i32 {
    match db.get_record("T").unwrap().read().record.get_field("FIRES") {
        Some(EpicsValue::Long(n)) => n,
        other => panic!("T.FIRES = {other:?}"),
    }
}

async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "{what} never happened");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// Past the delay with margin: a callback still due would have run by now.
async fn past_the_delay() {
    // absence-window: a callback refused at its expiry leaves nothing to wait on.
    epics_base_rs::runtime::task::sleep(Duration::from_secs_f64(DELAY * 2.0)).await;
}

#[epics_macros_rs::epics_test]
async fn a_seq_group_behind_its_delay_runs_while_the_ioc_runs() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=1", || read(&db, "A") == 1.0).await;
    until("B=2", || read(&db, "B") == 2.0).await;
}

/// C `iocPause` runs `scanPause` and leaves the callback threads up
/// (iocInit.c:281-297).
#[epics_macros_rs::epics_test]
async fn a_seq_group_behind_its_delay_runs_while_the_ioc_is_paused() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=1", || read(&db, "A") == 1.0).await;
    db.scan_control().pause();
    until("B=2", || read(&db, "B") == 2.0).await;
}

#[epics_macros_rs::epics_test]
async fn a_seq_group_behind_its_delay_never_runs_once_shutdown_began() {
    let db = ioc().await;
    process(&db, "S").await;
    until("A=1", || read(&db, "A") == 1.0).await;
    db.scan_control().stop();
    past_the_delay().await;
    assert_eq!(read(&db, "B"), 0.0, "group 1 ran after scanStop");
}

#[epics_macros_rs::epics_test]
async fn a_throttle_cooldown_fires_while_the_ioc_runs() {
    let db = ioc().await;
    process(&db, "T").await;
    until("the cooldown", || fires(&db) == 1).await;
}

#[epics_macros_rs::epics_test]
async fn a_throttle_cooldown_never_fires_once_shutdown_began() {
    let db = ioc().await;
    process(&db, "T").await;
    db.scan_control().stop();
    past_the_delay().await;
    assert_eq!(fires(&db), 0, "delayFuncCallback ran after scanStop");
}
