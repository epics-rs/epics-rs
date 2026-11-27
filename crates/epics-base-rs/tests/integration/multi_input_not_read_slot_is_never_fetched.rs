//! `InputLinkRequest::NotRead` is C's `default: break` before `dbGetLink`: the
//! slot's link is not read, its PP source is not processed, and a missing or
//! non-numeric source raises no LINK alarm. A control with a request reads and
//! fails/processes as before.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{
    AlarmSeverity, FieldDesc, InputFetchPolicy, InputLinkRequest, LinkReadAs, ProcessOutcome,
    Record, register_declared_fields,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

struct Source {
    val: EpicsValue,
    procs: Arc<AtomicUsize>,
}

impl Record for Source {
    fn record_type(&self) -> &'static str {
        "nr_src"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.procs.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then(|| self.val.clone())
    }
    fn put_field(&mut self, _: &str, _: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        SRC_FIELDS
    }
}

static SRC_FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::String, false)];

struct Reader {
    lnk: &'static str,
    request: InputLinkRequest,
    landed: Arc<AtomicUsize>,
}

impl Record for Reader {
    fn record_type(&self) -> &'static str {
        "nr_reader"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
        &[("LNK", "VAL")]
    }
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::ReadAllGateOnFailure
    }
    fn multi_input_fetch_is_db_get_link(&self) -> bool {
        true
    }
    fn input_link_request(&self, _: &str) -> InputLinkRequest {
        self.request
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "LNK" => Some(EpicsValue::String(self.lnk.into())),
            "VAL" => Some(EpicsValue::Double(0.0)),
            _ => None,
        }
    }
    fn put_field(&mut self, _: &str, _: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn put_field_internal(&mut self, name: &str, _: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            self.landed.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        READER_FIELDS
    }
}

static READER_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        declared_dbf: DbfCode::Inlink,
        ..FieldDesc::new("LNK", DbFieldType::String, false)
    },
    FieldDesc::new("VAL", DbFieldType::Double, false),
];

/// `(reader severity, source PROC count, VAL stores)` after one reader cycle.
async fn cycle(
    lnk: &'static str,
    with_src: bool,
    request: InputLinkRequest,
) -> (AlarmSeverity, usize, usize) {
    register_declared_fields("nr_reader", READER_FIELDS);
    register_declared_fields("nr_src", SRC_FIELDS);
    let db = PvDatabase::new();
    let procs = Arc::new(AtomicUsize::new(0));
    if with_src {
        db.add_record(
            "SRC",
            Box::new(Source {
                val: EpicsValue::String("abc".into()),
                procs: procs.clone(),
            }),
        )
        .await
        .unwrap();
    }
    let landed = Arc::new(AtomicUsize::new(0));
    db.add_record(
        "RD",
        Box::new(Reader {
            lnk,
            request,
            landed: landed.clone(),
        }),
    )
    .await
    .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("RD", &mut visited).await;
    let sevr = db.get_record("RD").unwrap().read().common.sevr;
    (
        sevr,
        procs.load(Ordering::SeqCst),
        landed.load(Ordering::SeqCst),
    )
}

const READ: InputLinkRequest = InputLinkRequest::As(LinkReadAs::Native);
const NOT: InputLinkRequest = InputLinkRequest::NotRead;

#[epics_macros_rs::epics_test]
async fn a_not_read_slot_to_a_missing_record_raises_nothing() {
    assert_eq!(
        cycle("NOPE.VAL", false, NOT).await,
        (AlarmSeverity::NoAlarm, 0, 0)
    );
    assert_eq!(
        cycle("NOPE.VAL", false, READ).await.0,
        AlarmSeverity::Invalid,
        "control: a read of a missing record is LINK/INVALID"
    );
}

#[epics_macros_rs::epics_test]
async fn a_not_read_slot_to_a_string_source_raises_nothing() {
    assert_eq!(
        cycle("SRC.VAL", true, NOT).await,
        (AlarmSeverity::NoAlarm, 0, 0)
    );
    assert_eq!(
        cycle("SRC.VAL", true, READ).await.0,
        AlarmSeverity::Invalid,
        "control: a numeric read of \"abc\" fails"
    );
}

#[epics_macros_rs::epics_test]
async fn a_pp_source_behind_a_not_read_slot_is_not_processed() {
    assert_eq!(
        cycle("SRC.VAL PP", true, NOT).await,
        (AlarmSeverity::NoAlarm, 0, 0)
    );
    assert_eq!(
        cycle("SRC.VAL PP", true, READ).await.1,
        1,
        "control: a read processes the PP source once"
    );
}
