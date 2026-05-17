use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::{LinkType, ProcessAction, Record, link_field_type};
use epics_base_rs::types::EpicsValue;

use crate::records::epid::EpidRecord;

/// Async Soft Channel device support for the epid record.
///
/// Same PID algorithm as `EpidSoftDeviceSupport`, but with an
/// asynchronous readback trigger via the TRIG link.
///
/// Processing flow:
/// 1. First pass (triggered=false): Write TVAL to TRIG link via
///    ProcessAction::WriteDbLink, and request a re-process via
///    ProcessAction::ReprocessAfter(1ms). The TRIG write triggers
///    the readback hardware to update the INP PV.
/// 2. Second pass (triggered=true): INP has been updated by the
///    triggered readback. Run PID with the fresh CVAL.
///
/// Ported from `devEpidSoftCallback.c`.
pub struct EpidSoftCallbackDeviceSupport {
    /// Whether the trigger has been sent and we're on the second pass.
    triggered: bool,
}

impl Default for EpidSoftCallbackDeviceSupport {
    fn default() -> Self {
        Self::new()
    }
}

impl EpidSoftCallbackDeviceSupport {
    pub fn new() -> Self {
        Self { triggered: false }
    }
}

impl DeviceSupport for EpidSoftCallbackDeviceSupport {
    fn dtyp(&self) -> &str {
        "Epid Async Soft"
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let epid = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<EpidRecord>())
            .expect("EpidSoftCallbackDeviceSupport requires an EpidRecord");

        if !self.triggered {
            // C `devEpidSoftCallback.c:116-147` — execute the
            // readback-trigger link, then branch on its TYPE:
            //
            //   if (ptriglink->type != CA_LINK) {
            //       status = dbPutLink(ptriglink,DBR_DOUBLE,&pepid->tval,1);
            //       ...                       // fall through to PID
            //   } else {
            //       status = dbCaPutLinkCallback(ptriglink,...);
            //       pepid->pact = TRUE;       // wait for the callback
            //       return(0);
            //   }
            //
            // A DB (or CONSTANT/empty) TRIG link is written synchronously
            // and the record falls straight through to the PID compute
            // in the SAME process pass — there is no `pact`. A CA TRIG
            // link cannot be waited on synchronously, so the trigger is
            // fired, the record is re-processed after the callback, and
            // the PID compute is deferred to that second pass.
            match link_field_type(&epid.trig) {
                LinkType::Ca => {
                    // CA TRIG link: fire the trigger and arrange a
                    // re-process — the PID compute happens on the
                    // second pass. C sets `pact=TRUE` and returns.
                    let actions = vec![
                        ProcessAction::WriteDbLink {
                            link_field: "TRIG",
                            value: EpicsValue::Double(epid.tval),
                        },
                        ProcessAction::ReprocessAfter(std::time::Duration::from_millis(1)),
                    ];
                    self.triggered = true;
                    return Ok(DeviceReadOutcome::computed_with(actions));
                }
                LinkType::Db => {
                    // DB TRIG link: the trigger write is C's
                    // `dbPutLink(ptriglink, ...)` (`devEpidSoftCallback
                    // .c:121-127`) — a *synchronous* write that
                    // processes the triggered source, and it must land
                    // BEFORE C's `dbGetLink(&pepid->inp, ...)` reads
                    // CVAL (`devEpidSoftCallback.c:151`).
                    //
                    // `read()` runs after the framework's `INP -> CVAL`
                    // input-link fetch, so emitting the TRIG write here
                    // would land a cycle late. Instead `EpidRecord::
                    // pre_input_link_actions` emits it as a pre-input
                    // action — the framework executes that strictly
                    // before the input-link fetch. By the time this
                    // `read()` runs, the trigger has already fired and
                    // CVAL already holds the freshly-triggered value;
                    // just run the PID, in this same pass (no `pact`).
                    super::epid_soft::EpidSoftDeviceSupport::do_pid(epid);
                    return Ok(DeviceReadOutcome::computed());
                }
                // CONSTANT / empty / Other TRIG link — `type != CA_LINK`,
                // so still synchronous: nothing to trigger, run PID now.
                LinkType::Constant | LinkType::Empty | LinkType::Other => {}
            }
        }

        // Second pass (CA path completed), or a non-CA link with no
        // trigger to fire: execute PID.
        self.triggered = false;
        super::epid_soft::EpidSoftDeviceSupport::do_pid(epid);
        Ok(DeviceReadOutcome::computed())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}
