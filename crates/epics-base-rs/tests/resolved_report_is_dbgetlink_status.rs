//! The per-cycle "resolved input links" report is C's
//! `RTN_SUCCESS(dbGetLink(...))` — one bit, one meaning, whichever fetch path
//! read the link.
//!
//! C hands its callers a single `long status` and they test only whether it is
//! zero:
//!
//! ```c
//! /* motorRecord.cc:3687-3698 */
//! rtnstat = dbGetLink(&(pmr->rdbl), DBR_DOUBLE, &rdblvalue, 0, 0);
//! if (!RTN_SUCCESS(rtnstat)) {
//!     if (pmr->mip != MIP_DONE) { clear_buttons(pmr); pmr->stop = 1; MARK(M_STOP); }
//! }
//! ```
//!
//! and `dbConstGetValue` (`dbConstLink.c:219-225`) returns 0 for a CONSTANT
//! link while writing nothing. So "the link delivered no value" and "the read
//! failed" are opposite answers to that test, and the port's `ReadDbLink`
//! executor used to give them the same one: it reported only the reads that
//! produced a VALUE. A motor with `field(RDBL,"5")` therefore stopped its own
//! axis on a read C calls successful, while the multi-input fetch loop — same
//! links, same records, other path — always used C's rule.
//!
//! The boundary axis is the link CLASS, because that is what decides C's
//! status: empty, constant, live DB, dead DB. A probe record captures the
//! report the framework hands it, which is the thing every consumer
//! (`motorRecord.cc:3687`, `epidRecord.c:191`, `aaoRecord.c::fetchValue`)
//! derives its failure from.

use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{
    AlarmSeverity, FieldDesc, ProcessAction, ProcessActions, ProcessOutcome, Record,
    ResolvedInputLinks,
};
use epics_base_rs::types::EpicsValue;

/// Declares one `ReadDbLink` on `LNK` and remembers what the framework said
/// about it — the shape `motorRecord`'s RDBL uses, with no link-class gate of
/// its own, so every class actually reaches the executor.
struct LinkProbe {
    lnk: String,
    val: f64,
    report: Arc<Mutex<Option<bool>>>,
    /// Read `LNK` through the multi-input fetch (`multi_input_links`, the
    /// shape `epidRecord`'s STPL uses) instead of a `ReadDbLink` action.
    via_multi_input: bool,
}

impl Record for LinkProbe {
    fn record_type(&self) -> &'static str {
        "link_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn pre_process_actions(&mut self) -> ProcessActions {
        if self.via_multi_input {
            return ProcessActions::new();
        }
        ProcessActions::from(vec![ProcessAction::ReadDbLink {
            link_field: "LNK",
            target_field: "VAL",
        }])
    }
    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
        if self.via_multi_input {
            &[("LNK", "VAL")]
        } else {
            &[]
        }
    }
    fn set_resolved_input_links(&mut self, resolved: ResolvedInputLinks<'_>) {
        *self.report.lock().unwrap() = Some(resolved.contains("LNK"));
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "LNK" => Some(EpicsValue::String(self.lnk.clone().into())),
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            self.val = value.to_f64().unwrap_or(self.val);
        }
        Ok(())
    }
    /// A field is readable only where the record type declares it, so a
    /// probe that serves `LNK`/`VAL` has to say so — the framework reads
    /// the link through the same `resolve_field` a client would.
    fn declared_fields(&self) -> &'static [FieldDesc] {
        LINK_PROBE_FIELDS
    }
}

static LINK_PROBE_FIELDS: &[FieldDesc] = &[
    FieldDesc::new("LNK", epics_base_rs::types::DbFieldType::String, false),
    FieldDesc::new("VAL", epics_base_rs::types::DbFieldType::Double, false),
];

/// Runs one cycle of a probe wired to `lnk` and returns
/// `(was it reported resolved, VAL after, reader severity)`.
async fn cycle(lnk: &str) -> (bool, f64, AlarmSeverity) {
    cycle_via(lnk, false).await
}

/// [`cycle`] with the probe reading `LNK` on the path `via_multi_input` picks.
async fn cycle_via(lnk: &str, via_multi_input: bool) -> (bool, f64, AlarmSeverity) {
    let db = PvDatabase::new();
    db.add_record(
        "SRC",
        Box::new(LinkProbe {
            lnk: String::new(),
            val: 42.0,
            report: Arc::new(Mutex::new(None)),
            via_multi_input,
        }),
    )
    .await
    .unwrap();

    let report = Arc::new(Mutex::new(None));
    db.add_record(
        "P",
        Box::new(LinkProbe {
            lnk: lnk.to_string(),
            val: 1.0,
            report: report.clone(),
            via_multi_input,
        }),
    )
    .await
    .unwrap();

    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("P", &mut visited).await;

    let inst = db.get_record("P").unwrap();
    let guard = inst.read();
    let resolved = report.lock().unwrap().expect("the report must be made");
    (
        resolved,
        guard.record.get_field("VAL").unwrap().to_f64().unwrap(),
        guard.common.sevr,
    )
}

/// Empty: C's CONSTANT lset with a NULL string. `dbConstGetValue` returns 0.
#[epics_macros_rs::epics_test]
async fn an_empty_link_reads_as_a_successful_dbgetlink() {
    let (resolved, val, sevr) = cycle("").await;
    assert!(resolved, "an unset link is not a failed read");
    assert_eq!(val, 1.0, "and it delivers nothing");
    assert_eq!(sevr, AlarmSeverity::NoAlarm, "no setLinkAlarm");
}

/// Constant: the case the executor got wrong. Status 0, buffer untouched.
#[epics_macros_rs::epics_test]
async fn a_constant_link_reads_as_a_successful_dbgetlink() {
    let (resolved, val, sevr) = cycle("5").await;
    assert!(
        resolved,
        "dbConstGetValue returns 0 — a constant RDBL must not read as a \
         failed readback"
    );
    assert_eq!(
        val, 1.0,
        "*pnRequest = 0: the constant reached the record at init, not here"
    );
    assert_eq!(sevr, AlarmSeverity::NoAlarm, "no setLinkAlarm");
}

/// Live DB link: status 0 AND a value.
#[epics_macros_rs::epics_test]
async fn a_live_db_link_reads_as_a_successful_dbgetlink() {
    let (resolved, val, sevr) = cycle("SRC.VAL").await;
    assert!(resolved);
    assert_eq!(val, 42.0, "the value lands in the target field");
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
}

/// Dead DB link: the one non-zero status, and the only one that may report a
/// failure. C `dbLink.c:314-321` attaches `setLinkAlarm` to exactly this case.
#[epics_macros_rs::epics_test]
async fn a_dead_db_link_is_the_only_failed_read() {
    let (resolved, val, sevr) = cycle("NOSUCHREC.VAL").await;
    assert!(!resolved, "a dead target is C's non-zero status");
    assert_eq!(val, 1.0, "nothing was delivered");
    assert_eq!(
        sevr,
        AlarmSeverity::Invalid,
        "setLinkAlarm raises LINK/INVALID on the reader"
    );
}

/// The multi-input fetch makes the same report, for every type that keeps
/// it: the report was once built only for types that opted in through a
/// second hook, and `epidRecord`'s STPL — a multi-input link on a type that
/// had not — read as a failed setpoint every cycle.
#[epics_macros_rs::epics_test]
async fn a_multi_input_link_is_reported_by_the_same_rule() {
    let (resolved, val, sevr) = cycle_via("SRC.VAL", true).await;
    assert!(resolved, "a live multi-input read is C's zero status");
    assert_eq!(val, 42.0);
    assert_eq!(sevr, AlarmSeverity::NoAlarm);

    let (resolved, val, sevr) = cycle_via("NOSUCHREC.VAL", true).await;
    assert!(
        !resolved,
        "a dead multi-input target is the non-zero status"
    );
    assert_eq!(val, 1.0, "nothing was delivered");
    assert_eq!(sevr, AlarmSeverity::Invalid);
}
