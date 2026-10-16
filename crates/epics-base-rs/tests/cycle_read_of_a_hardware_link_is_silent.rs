//! A record whose read routine calls `dbGetLink` on a hardware INP — a C
//! record type with a `VME_IO` dset loads one — gets `S_db_noLSET` from
//! `dbTryGetLink`, and `dbGetLink` returns -1 for it without `setLinkAlarm`
//! (`dbLink.c:335-336`). Any other failed read is LINK/INVALID. Both read
//! shapes: a `ReadDbLink` action and the multi-input fetch.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{
    AlarmSeverity, FieldDesc, InputFetchPolicy, InputLinkRequest, LinkReadAs, ProcessAction,
    ProcessActions, ProcessOutcome, Record, register_declared_fields,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

struct Reader {
    inp: &'static str,
    via_multi_input: bool,
    landed: Arc<AtomicUsize>,
}

impl Record for Reader {
    fn record_type(&self) -> &'static str {
        "hw_reader"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn pre_process_actions(&mut self) -> ProcessActions {
        if self.via_multi_input {
            return ProcessActions::new();
        }
        ProcessActions::from(vec![ProcessAction::ReadDbLink {
            link_field: "INP",
            target_field: "VAL",
        }])
    }
    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
        if self.via_multi_input {
            &[("INP", "VAL")]
        } else {
            &[]
        }
    }
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::ReadAllGateOnFailure
    }
    fn multi_input_fetch_is_db_get_link(&self) -> bool {
        true
    }
    fn input_link_request(&self, _: &str) -> InputLinkRequest {
        InputLinkRequest::As(LinkReadAs::Double)
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "INP" => Some(EpicsValue::String(self.inp.into())),
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
        FIELDS
    }
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        declared_dbf: DbfCode::Inlink,
        ..FieldDesc::new("INP", DbFieldType::String, false)
    },
    FieldDesc::new("VAL", DbFieldType::Double, false),
];

/// `(severity, AMSG, VAL stores)` after one cycle.
async fn cycle(inp: &'static str, via_multi_input: bool) -> (AlarmSeverity, String, usize) {
    register_declared_fields("hw_reader", FIELDS);
    let db = PvDatabase::new();
    let landed = Arc::new(AtomicUsize::new(0));
    db.add_record(
        "RD",
        Box::new(Reader {
            inp,
            via_multi_input,
            landed: landed.clone(),
        }),
    )
    .await
    .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("RD", &mut visited).await;
    let rec = db.get_record("RD").unwrap();
    let common = &rec.read().common;
    (
        common.sevr,
        common.amsg.to_string(),
        landed.load(Ordering::SeqCst),
    )
}

#[epics_macros_rs::epics_test]
async fn a_cycle_read_of_a_hardware_link_fails_without_an_alarm() {
    for via_multi_input in [false, true] {
        assert_eq!(
            cycle("#C0 S0", via_multi_input).await,
            (AlarmSeverity::NoAlarm, String::new(), 0),
            "multi-input: {via_multi_input}"
        );
        assert_eq!(
            cycle("NOPE.VAL", via_multi_input).await,
            (AlarmSeverity::Invalid, "field INP".to_string(), 0),
            "control, multi-input: {via_multi_input}: a read of a missing record is LINK/INVALID"
        );
    }
}
