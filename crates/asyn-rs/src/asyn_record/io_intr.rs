//! asynRecord `SCAN="I/O Intr"` — the driver-interrupt readback mode.
//!
//! C reference: `asynRecord.c`
//!   * `getIoIntInfo` (:582-597) — the DSET hook `scanAdd`/`scanDelete` call
//!     when SCAN enters / leaves `I/O Intr`: clear `gotValue`, then
//!     `registerInterrupts` / `cancelInterrupts`.
//!   * `registerInterrupts` / `cancelInterrupts` (:599-707) — dispatch on
//!     IFACE, refuse ("No asynXxx interface") when the port does not implement
//!     it, and pass UI32MASK for asynUInt32Digital.
//!   * `callbackInterruptOctet/Int32/UInt32/Float64` (:709-792) — store the
//!     pushed value into TINP / I32INP / UI32INP / F64INP, set `gotValue = 1`
//!     and `scanIoRequest`; a callback that finds `gotValue == 1` **drops** its
//!     value (the record has not processed the previous one yet).
//!   * `process` (:341) — `if (pasynRecPvt->gotValue) goto done`: the cycle an
//!     interrupt drove does no I/O at all, and `gotValue` is cleared at the end
//!     of process (:370).
//!   * `cancelIOInterruptScan` (:794-806) — REASON / IFACE / UI32MASK / PCNCT=0
//!     puts force SCAN back to Passive, which is what cancels the registration.
//!
//! # The `gotValue` gate
//!
//! C's `gotValue` is an `int` flag beside the value it describes: the callback
//! writes the record field *and* sets the flag, `process` reads the flag to skip
//! its read, and the end of `process` clears it. Three sites, two pieces of
//! state, one invariant held by convention.
//!
//! Here the flag and the value are ONE cell — [`IoIntrScan::sample`], an
//! `Option<IoIntrSample>`. `Some` *is* `gotValue == 1`, and it carries the value
//! the interrupt delivered, so:
//!
//! * the callback cannot set the flag without a value, or a value without the
//!   flag;
//! * `process` consumes the cell with `take()` — the read of the flag, the read
//!   of the value and C's `gotValue = 0` are one operation that cannot be
//!   half-done;
//! * the "callback while unprocessed" drop rule is `slot.is_some()`, not a
//!   separate flag test.
//!
//! # The registration invariant
//!
//! MUST: a live driver subscription exists **iff** the record is on the I/O Intr
//! scan list AND it is bound to a port that implements IFACE.
//!
//! Owner: [`IoIntrScan::refresh`]. Every input to that predicate — SCAN
//! membership ([`IoIntrScan::set_active`], driven by the framework's
//! `get_ioint_info` hook), the port binding ([`IoIntrScan::rebind`], driven by
//! `connect_device` and by the REASON / IFACE / UI32MASK puts) — is a write
//! *through* it, so the subscription can never describe a binding the record no
//! longer has.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::InterfaceType;
use crate::interrupt::{InterruptFilter, InterruptValue, SyncCallbackSubscription};
use crate::param::ParamValue;
use crate::port_handle::PortHandle;
use crate::trace::TraceIoMask;

/// Depth of the `scanIoRequest` wakeup channel. One pending wakeup is all the
/// record can use: a second interrupt cannot even be stored while the first is
/// unprocessed (the `sample` cell is occupied), so a deeper channel would only
/// queue redundant process requests. C's `scanIoRequest` collapses the same way
/// — the record is on the scan list once.
const WAKEUP_DEPTH: usize = 1;

/// One interrupt value, in the shape of the record field C's callback writes it
/// to (`asynRecord.c:709-792`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IoIntrSample {
    /// `callbackInterruptOctet` → TINP (escaped), :725.
    Octet(String),
    /// `callbackInterruptInt32` → I32INP, :747.
    Int32(i32),
    /// `callbackInterruptUInt32` → UI32INP, :768.
    UInt32(u32),
    /// `callbackInterruptFloat64` → F64INP, :789.
    Float64(f64),
}

impl IoIntrSample {
    /// The driver value as the record's IFACE-selected callback would receive
    /// it. C has one callback per asyn interface and the driver's per-interface
    /// interrupt list only ever delivers that interface's type, so a value of
    /// another type is not a case C can reach — the interface filter
    /// ([`InterruptFilter::iface`]) already restricts typed fires. A driver that
    /// fires an *untyped* value (`call_param_callbacks`) reaches every
    /// subscriber, so the payload is still matched against IFACE here and a
    /// mismatch is dropped, exactly as C's per-interface lists drop it.
    fn from_value(iface: InterfaceType, value: &ParamValue) -> Option<Self> {
        match (iface, value) {
            // C `epicsStrSnPrintEscaped(pasynRec->tinp, sizeof(pasynRec->tinp),
            // ...)` (:725) — the same escaping, under the same 40-byte TINP
            // bound, that the polled octet read applies.
            (InterfaceType::Octet, ParamValue::Octet(s)) => Some(Self::Octet(
                crate::escape::escaped_from_raw(s, super::TINP_SIZE),
            )),
            (InterfaceType::Int32, ParamValue::Int32(v)) => Some(Self::Int32(*v)),
            (InterfaceType::UInt32Digital, ParamValue::UInt32Digital(v)) => Some(Self::UInt32(*v)),
            (InterfaceType::Float64, ParamValue::Float64(v)) => Some(Self::Float64(*v)),
            _ => None,
        }
    }
}

/// What the record's interrupt subscription is bound to — the record fields C's
/// `registerInterrupts` reads off `pasynRec` at registration time
/// (asynRecord.c:599-656).
#[derive(Clone)]
pub(crate) struct IoIntrBinding {
    pub handle: PortHandle,
    pub iface: InterfaceType,
    pub addr: i32,
    pub reason: usize,
    pub ui32mask: u32,
}

#[derive(Default)]
struct Registration {
    /// SCAN == "I/O Intr" (C: the record is on the ioScanPvt list).
    active: bool,
    /// The port/interface the record currently points at; `None` while it is
    /// bound to no port (C `stateNoDevice`).
    binding: Option<IoIntrBinding>,
    /// The live driver registration. RAII — dropping it is C `cancelInterrupts`.
    sub: Option<SyncCallbackSubscription>,
    /// C `interruptAccept` (asynRecord.c:716,738,759,780): no interrupt may
    /// drive a scan before the IOC has finished initialising. Set when the
    /// framework takes the wakeup receiver (`iocInit`'s `setup_io_intr`), which
    /// is the point the record's I/O Intr scan actually exists.
    started: bool,
}

/// The record's I/O Intr machinery: the `gotValue` cell, the driver
/// registration, and the `scanIoRequest` wakeup channel. Shared (`Arc`) between
/// the record and its `asynRecordDevice` device support, which is the
/// framework's `get_ioint_info` seam.
pub(crate) struct IoIntrScan {
    reg: Mutex<Registration>,
    /// C `gotValue` + the value it describes, as one cell. See the module doc.
    /// Its own `Arc` because the driver callback must reach it without holding a
    /// reference to the record or to `IoIntrScan`.
    sample: Arc<Mutex<Option<IoIntrSample>>>,
    /// C `scanIoRequest(pasynRecPvt->ioScanPvt)`.
    tx: mpsc::Sender<()>,
    rx: Mutex<Option<mpsc::Receiver<()>>>,
}

impl IoIntrScan {
    pub(crate) fn new() -> Self {
        let (tx, rx) = mpsc::channel(WAKEUP_DEPTH);
        Self {
            reg: Mutex::new(Registration::default()),
            sample: Arc::new(Mutex::new(None)),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// The framework's `get_ioint_info` seam: hand over the wakeup receiver
    /// (C `*iopvt = pasynRecPvt->ioScanPvt`) and arm the record's
    /// `interruptAccept` gate. Taken once, at `iocInit`.
    pub(crate) fn take_receiver(&self) -> Option<mpsc::Receiver<()>> {
        let rx = self.rx.lock().unwrap().take()?;
        {
            let mut reg = self.reg.lock().unwrap();
            reg.started = true;
        }
        // A record that loaded with SCAN="I/O Intr" set `active` before the IOC
        // was running; this is the first moment its registration may exist.
        let _ = self.refresh();
        Some(rx)
    }

    /// C `getIoIntInfo(cmd)` (asynRecord.c:588-594): the record joined (`true`)
    /// or left (`false`) the I/O Intr scan list. Returns the C
    /// `registerInterrupts` failure text ("No asynInt32 interface", …), which
    /// the caller reports into ERRS.
    pub(crate) fn set_active(&self, active: bool) -> Result<(), String> {
        {
            let mut reg = self.reg.lock().unwrap();
            if reg.active == active {
                return Ok(());
            }
            reg.active = active;
        }
        self.refresh()
    }

    /// Publish the record's current port binding — C `registerInterrupts`
    /// re-reads PORT / IFACE / ADDR / REASON / UI32MASK off the record every
    /// time it registers, so the port keeps the same fields in the one place the
    /// registration is derived from. `None` = bound to no port.
    pub(crate) fn rebind(&self, binding: Option<IoIntrBinding>) -> Result<(), String> {
        {
            let mut reg = self.reg.lock().unwrap();
            reg.binding = binding;
        }
        self.refresh()
    }

    pub(crate) fn is_active(&self) -> bool {
        self.reg.lock().unwrap().active
    }

    /// C `process`'s `if (pasynRecPvt->gotValue) goto done` (:341) *and* its
    /// `gotValue = 0` (:370), as one operation. `Some` means this cycle was
    /// driven by an interrupt: the value is already in hand, so the record does
    /// no I/O.
    pub(crate) fn take_sample(&self) -> Option<IoIntrSample> {
        self.sample.lock().unwrap().take()
    }

    /// C `process`'s `gotValue = 0` at `done:` (asynRecord.c:370) for the cycles
    /// that did NOT consume the value — the `stateNoDevice` refusal, the
    /// completion re-entry, the "Special is active" arm. C clears the flag on
    /// every path that reaches `done:`, so an interrupt the record could not act
    /// on is discarded, never carried into a later cycle.
    pub(crate) fn clear_sample(&self) {
        *self.sample.lock().unwrap() = None;
    }

    /// The single owner of the registration invariant (see the module doc):
    /// the subscription exists exactly while the record is on the I/O Intr list,
    /// is bound to a port, and the port implements IFACE.
    fn refresh(&self) -> Result<(), String> {
        let mut reg = self.reg.lock().unwrap();

        // Every binding change discards whatever the *previous* binding pushed.
        // C clears `gotValue` when it (re)registers (`getIoIntInfo` cmd == 0,
        // asynRecord.c:589) — "a fresh registration never inherits a value left
        // over from a previous one" — and its `process` `done:` clears it on the
        // cancel side. Doing it here, on every path through the single owner of
        // the subscription, is the same rule with one owner instead of two: the
        // sample and the subscription that produced it are replaced together, so
        // a value from a port the record is no longer bound to cannot be
        // published.
        *self.sample.lock().unwrap() = None;

        let binding = match (reg.active && reg.started, reg.binding.clone()) {
            (true, Some(b)) => b,
            // Not on the scan list (or not bound / not yet running): C
            // `cancelInterrupts`. Dropping the RAII handle unregisters it.
            _ => {
                reg.sub = None;
                return Ok(());
            }
        };

        // C `registerInterrupts` gates every arm on the interface-valid flag and
        // reports "No asynXxx interface" when the port does not have it
        // (asynRecord.c:611-649). Ask the port's registry, not the record's *IV
        // readback — same rule as `AsynRecord::has_interface`.
        if !binding.handle.has_interface(binding.iface.registry_type()) {
            reg.sub = None;
            return Err(format!("No {} interface", binding.iface.c_asyn_name()));
        }

        let filter = InterruptFilter {
            reason: Some(binding.reason),
            addr: Some(binding.addr),
            // C passes the record's UI32MASK to
            // `pasynUInt32->registerInterruptUser` (:635); the driver then fires
            // only when the changed bits overlap it. No other interface carries
            // a mask.
            uint32_mask: match binding.iface {
                InterfaceType::UInt32Digital => Some(binding.ui32mask),
                _ => None,
            },
            iface: Some(binding.iface.registry_type()),
        };

        let iface = binding.iface;
        let tx = self.tx.clone();
        let sample_cell = self.sample.clone();
        // C registers a *synchronous* callback (`registerInterruptUser`): the
        // driver invokes it inline, per value. The mailbox path would coalesce
        // to the LATEST value, which is the opposite of C's rule — C keeps the
        // FIRST unprocessed value and drops the ones after it (`if (gotValue ==
        // 1) return`).
        let sub = binding.handle.interrupts().register_sync_callback(
            filter,
            move |iv: &InterruptValue| {
                let Some(sample) = IoIntrSample::from_value(iface, &iv.value) else {
                    return;
                };
                {
                    let mut slot = sample_cell.lock().unwrap();
                    // C `callbackInterrupt*`: "If gotValue==1 then the record
                    // has not yet finished processing the previous interrupt,
                    // just return" (:717-719).
                    if slot.is_some() {
                        return;
                    }
                    *slot = Some(sample);
                }
                // C `scanIoRequest(pasynRecPvt->ioScanPvt)`. The callback runs
                // inline on the driver's thread and must not block; a full
                // channel means a process request is already pending, which is
                // what a second `scanIoRequest` would collapse into anyway.
                let _ = tx.try_send(());
            },
        );

        reg.sub = Some(sub);
        Ok(())
    }
}
