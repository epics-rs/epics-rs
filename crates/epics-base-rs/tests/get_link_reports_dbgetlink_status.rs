//! A read a record queues after `process()` or from `special()` reports C's
//! `dbGetLink` status back to it, and the actions it answers with run next.
//!
//! C throttle reads its sync link into a private buffer and decides `VAL` and
//! `STS` from the status (`throttleRecord.c:628-647`), and `valuePut` calls
//! that sync straight after a landed `dbPutLink` (`:564-572`). The port's
//! post-process `ReadDbLink` discarded the status, and a link write's report
//! could not start a read. The boundary axis is the link class, which is what
//! decides the status: empty and CONSTANT return 0 having written nothing
//! (`dbConstGetValue`, `dbConstLink.c:219-225`), a live DB link returns 0 with
//! the value, a dead one is the non-zero status `setLinkAlarm` follows
//! (`dbLink.c:316-323`).

use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{
    AlarmSeverity, FieldDesc, LinkRead, ProcessAction, ProcessActions, ProcessOutcome, Record,
};
use epics_base_rs::types::EpicsValue;

#[derive(Clone, Copy)]
enum Mode {
    /// `process()` queues `GetLink { LNK }`.
    Get,
    /// `process()` queues `ReadDbLink { LNK -> VAL }`.
    Read,
    /// `process()` queues `WriteDbLink { OUT }`; a landed write answers with
    /// `GetLink { LNK }`, throttle's `valuePut` -> `valueSync`.
    WriteThenGet,
    /// `process()` queues nothing; a `SYNC` put's `special()` queues
    /// `GetLink { LNK }`.
    Special,
}

#[derive(Debug, PartialEq)]
enum Heard {
    Wrote(bool),
    Read(&'static str, LinkRead),
}

struct Probe {
    lnk: String,
    out: String,
    val: f64,
    mode: Mode,
    queued: Vec<ProcessAction>,
    heard: Arc<Mutex<Vec<Heard>>>,
}

impl Record for Probe {
    fn record_type(&self) -> &'static str {
        "getlink_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let mut outcome = ProcessOutcome::complete();
        outcome.actions = match self.mode {
            Mode::Get => vec![ProcessAction::GetLink { link_field: "LNK" }],
            Mode::Read => vec![ProcessAction::ReadDbLink {
                link_field: "LNK",
                target_field: "VAL",
            }],
            Mode::WriteThenGet => vec![ProcessAction::WriteDbLink {
                link_field: "OUT",
                value: EpicsValue::Double(7.0),
            }],
            Mode::Special => Vec::new(),
        };
        Ok(outcome)
    }
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
        if after && field == "SYNC" {
            self.queued
                .push(ProcessAction::GetLink { link_field: "LNK" });
        }
        Ok(())
    }
    fn take_special_actions(&mut self) -> Vec<ProcessAction> {
        std::mem::take(&mut self.queued)
    }
    fn set_out_link_write_status(
        &mut self,
        _link_field: &'static str,
        _value: &EpicsValue,
        failed: bool,
    ) -> ProcessActions {
        self.heard.lock().unwrap().push(Heard::Wrote(!failed));
        if failed {
            return ProcessActions::new();
        }
        ProcessActions::from(vec![ProcessAction::GetLink { link_field: "LNK" }])
    }
    fn link_read_result(&mut self, link_field: &'static str, read: LinkRead) -> ProcessActions {
        self.heard
            .lock()
            .unwrap()
            .push(Heard::Read(link_field, read));
        ProcessActions::new()
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "LNK" => Some(EpicsValue::String(self.lnk.clone().into())),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SYNC" => Some(EpicsValue::Short(0)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            self.val = value.to_f64().unwrap_or(self.val);
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        PROBE_FIELDS
    }
}

static PROBE_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        declared_dbf: epics_base_rs::types::DbfCode::Inlink,
        ..FieldDesc::new("LNK", epics_base_rs::types::DbFieldType::String, false)
    },
    FieldDesc {
        declared_dbf: epics_base_rs::types::DbfCode::Outlink,
        ..FieldDesc::new("OUT", epics_base_rs::types::DbFieldType::String, false)
    },
    FieldDesc::new("VAL", epics_base_rs::types::DbFieldType::Double, false),
    FieldDesc::new("SYNC", epics_base_rs::types::DbFieldType::Short, false),
];

struct Run {
    heard: Vec<Heard>,
    val: f64,
    sevr: AlarmSeverity,
}

async fn probe(mode: Mode, lnk: &str, out: &str) -> (PvDatabase, Arc<Mutex<Vec<Heard>>>) {
    let db = PvDatabase::new();
    epics_base_rs::server::record::register_declared_fields("getlink_probe", PROBE_FIELDS);
    let quiet = Arc::new(Mutex::new(Vec::new()));
    db.add_record(
        "SRC",
        Box::new(Probe {
            lnk: String::new(),
            out: String::new(),
            val: 42.0,
            mode: Mode::Special,
            queued: Vec::new(),
            heard: quiet,
        }),
    )
    .await
    .unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    db.add_record(
        "P",
        Box::new(Probe {
            lnk: lnk.to_string(),
            out: out.to_string(),
            val: 1.0,
            mode,
            queued: Vec::new(),
            heard: heard.clone(),
        }),
    )
    .await
    .unwrap();
    (db, heard)
}

fn settle(db: &PvDatabase, heard: &Mutex<Vec<Heard>>) -> Run {
    let inst = db.get_record("P").unwrap();
    let guard = inst.read();
    Run {
        heard: std::mem::take(&mut *heard.lock().unwrap()),
        val: guard.record.get_field("VAL").unwrap().to_f64().unwrap(),
        sevr: guard.common.sevr,
    }
}

async fn cycle(mode: Mode, lnk: &str, out: &str) -> Run {
    let (db, heard) = probe(mode, lnk, out).await;
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("P", &mut visited).await;
    settle(&db, &heard)
}

#[epics_macros_rs::epics_test]
async fn get_link_on_an_empty_link_succeeds_with_nothing() {
    let run = cycle(Mode::Get, "", "").await;
    assert_eq!(run.heard, [Heard::Read("LNK", LinkRead::Succeeded)]);
    assert_eq!(run.sevr, AlarmSeverity::NoAlarm);
}

#[epics_macros_rs::epics_test]
async fn get_link_on_a_constant_link_succeeds_with_nothing() {
    let run = cycle(Mode::Get, "5", "").await;
    assert_eq!(run.heard, [Heard::Read("LNK", LinkRead::Succeeded)]);
    assert_eq!(run.sevr, AlarmSeverity::NoAlarm);
}

#[epics_macros_rs::epics_test]
async fn get_link_on_a_live_link_hands_over_the_value_and_stores_nothing() {
    let run = cycle(Mode::Get, "SRC.VAL", "").await;
    assert_eq!(
        run.heard,
        [Heard::Read(
            "LNK",
            LinkRead::Value(EpicsValue::Double(42.0))
        )]
    );
    assert_eq!(run.val, 1.0, "the buffer is the record's, not a field");
    assert_eq!(run.sevr, AlarmSeverity::NoAlarm);
}

#[epics_macros_rs::epics_test]
async fn get_link_on_a_dead_link_fails_and_raises_the_link_alarm() {
    let run = cycle(Mode::Get, "NOSUCHREC.VAL", "").await;
    assert_eq!(run.heard, [Heard::Read("LNK", LinkRead::Failed)]);
    assert_eq!(run.sevr, AlarmSeverity::Invalid);
}

#[epics_macros_rs::epics_test]
async fn a_post_process_read_db_link_reports_its_status() {
    let run = cycle(Mode::Read, "SRC.VAL", "").await;
    assert_eq!(run.heard, [Heard::Read("LNK", LinkRead::Succeeded)]);
    assert_eq!(run.val, 42.0, "ReadDbLink still stores into VAL");

    let run = cycle(Mode::Read, "NOSUCHREC.VAL", "").await;
    assert_eq!(run.heard, [Heard::Read("LNK", LinkRead::Failed)]);
    assert_eq!(run.val, 1.0);
    assert_eq!(
        run.sevr,
        AlarmSeverity::Invalid,
        "C's dbGetLink runs inside process(), so monitor() commits its LINK alarm in the same cycle"
    );
}

#[epics_macros_rs::epics_test]
async fn a_landed_write_runs_the_read_it_answers_with() {
    let run = cycle(Mode::WriteThenGet, "SRC.VAL", "SRC.VAL").await;
    assert_eq!(
        run.heard,
        [
            Heard::Wrote(true),
            Heard::Read("LNK", LinkRead::Value(EpicsValue::Double(7.0))),
        ],
        "the read follows the write it answered, and sees what the write left"
    );
}

#[epics_macros_rs::epics_test]
async fn a_failed_write_answers_with_nothing() {
    let run = cycle(Mode::WriteThenGet, "SRC.VAL", "NOSUCHREC.VAL").await;
    assert_eq!(run.heard, [Heard::Wrote(false)]);
}

#[epics_macros_rs::epics_test]
async fn a_get_link_queued_by_special_reports_on_the_put() {
    let (db, heard) = probe(Mode::Special, "SRC.VAL", "").await;
    db.put_record_field_from_ca("P", "SYNC", EpicsValue::Short(1))
        .await
        .unwrap();
    let run = settle(&db, &heard);
    assert_eq!(
        run.heard,
        [Heard::Read(
            "LNK",
            LinkRead::Value(EpicsValue::Double(42.0))
        )]
    );
}

/// A hardware link has no lset (`dbLink.c:112-113`), so `dbPutLink` fails
/// `S_db_noLSET` before `setLinkAlarm` (`dbLink.c:440-441`): a failed put,
/// no alarm.
#[epics_macros_rs::epics_test]
async fn a_write_through_a_hardware_link_fails_without_an_alarm() {
    let run = cycle(Mode::WriteThenGet, "SRC.VAL", "#C0 S0").await;
    assert_eq!(run.heard, [Heard::Wrote(false)]);
    assert_eq!(run.sevr, AlarmSeverity::NoAlarm);
}

/// A constant link's `dbConstPutValue` answers 0 (`dbConstLink.c:228-232`).
#[epics_macros_rs::epics_test]
async fn a_write_through_a_constant_link_lands_as_a_no_op() {
    let run = cycle(Mode::WriteThenGet, "SRC.VAL", "5").await;
    assert_eq!(run.heard[0], Heard::Wrote(true));
    assert_eq!(run.sevr, AlarmSeverity::NoAlarm);
}
