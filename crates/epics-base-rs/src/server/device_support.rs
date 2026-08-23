use crate::error::CaResult;
use crate::server::record::{AlarmSeverity, ProcessAction, Record, RecordInstance, ScanType};

/// Check if a DTYP string represents a soft/built-in device support
/// that doesn't require an explicit device support registration.
/// Matches C EPICS built-in soft device support names.
pub fn is_soft_dtyp(dtyp: &str) -> bool {
    // Only the four genuine base soft-channel DTYPs. Timestamp-producing
    // DTYPs are NOT soft channels — they are real device support that writes
    // a resolved time stamp into VAL, so they must reach the device-lookup
    // path rather than short-circuit here:
    //  - "Soft Timestamp" (base `devTimestamp.c`) — served by the
    //    pre-registered `builtin_devices::builtin_dynamic_factory`.
    //  - "Sec Past Epoch" / "Time of Day" (epics-modules/std `devTimeOfDay.c`)
    //    — served by `std_rs::std_device_supports()`; if the IOC has not
    //    registered them they correctly warn as "no device support", not
    //    silently no-op as a soft channel. base-rs must not special-case a
    //    std-module DTYP (layering leak).
    dtyp.is_empty()
        || dtyp == "Soft Channel"
        || dtyp == "Raw Soft Channel"
        || dtyp == "Async Soft Channel"
}

/// Handle for waiting on asynchronous write completion.
/// Returned by [`DeviceSupport::write_begin`] when the write is submitted
/// to a worker queue rather than executed synchronously.
pub trait WriteCompletion: Send + 'static {
    /// Block until the write completes or timeout expires.
    fn wait(&self, timeout: std::time::Duration) -> CaResult<()>;
}

/// Outcome of a device support read() call.
///
/// Allows device support to return side-effect actions (link writes,
/// delayed reprocess) and signal that it has already performed the
/// Result of a device support `read()` call.
///
/// # `ok()` vs `computed()`
///
/// This mirrors the C EPICS `read_ai()` return convention:
///
/// - **`ok()`** (C return 0): Device support wrote to RVAL. The record's
///   `process()` will run its built-in conversion (e.g., ai applies
///   `ROFF → ASLO/AOFF → LINR/ESLO/EOFF → smoothing` to produce VAL
///   from RVAL).
///
/// - **`computed()`** (C return 2): Device support wrote to VAL directly.
///   The record's `process()` will **skip** its conversion and use the
///   VAL as-is. Use this when the device support provides engineering
///   units directly (e.g., soft channel, asyn, custom drivers that
///   call `record.put_field("VAL", ...)`).
///
/// **Common mistake:** returning `ok()` when VAL is set directly causes
/// the record's conversion to overwrite VAL with a value derived from
/// RVAL (typically 0), making the read appear broken.
#[derive(Default)]
pub struct DeviceReadOutcome {
    /// Actions for the framework to execute (WriteDbLink, ReprocessAfter, etc.)
    pub actions: Vec<ProcessAction>,
    /// If true, the record's built-in conversion (e.g., ai RVAL→VAL)
    /// is skipped. Set this when device support writes VAL directly.
    pub did_compute: bool,
}

impl DeviceReadOutcome {
    /// Device support wrote RVAL; record will run its conversion to produce VAL.
    ///
    /// C equivalent: `read_ai()` returns 0.
    pub fn ok() -> Self {
        Self::default()
    }

    /// Device support wrote VAL directly; record will skip conversion.
    ///
    /// C equivalent: `read_ai()` returns 2.
    pub fn computed() -> Self {
        Self {
            did_compute: true,
            actions: Vec::new(),
        }
    }

    /// Shorthand for a computed read with actions.
    pub fn computed_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            did_compute: true,
            actions,
        }
    }
}

/// Trait for custom device support implementations.
/// When DTYP is set to something other than "" or "Soft Channel",
/// the registered DeviceSupport is used instead of link resolution.
pub trait DeviceSupport: Send + Sync + 'static {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    /// Read from hardware into the record.
    ///
    /// Returns a `DeviceReadOutcome` containing:
    /// - `actions`: side-effect actions (link writes, delayed reprocess)
    ///   that the framework will execute after process()
    /// - `did_compute`: if true, the record's built-in compute was already
    ///   performed (e.g., device support ran PID), so process() should skip it
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let _ = record;
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()>;
    fn dtyp(&self) -> &str;

    /// Return the last alarm (status, severity) from the driver.
    /// None means the driver does not override alarms.
    fn last_alarm(&self) -> Option<(u16, u16)> {
        None
    }

    /// Return the last timestamp from the driver.
    /// None means the driver does not override timestamps.
    fn last_timestamp(&self) -> Option<std::time::SystemTime> {
        None
    }

    /// Return the userTag the driver attached to its reading, as the
    /// 64-bit `epicsUTag`. `None` means the driver provides no userTag
    /// and `common.utag` is left untouched.
    ///
    /// This is the channel a timing receiver (event system) uses to
    /// deliver a pulse-id / event tag: `epicsTimeStamp` itself carries
    /// no tag and the generalTime event path (`epicsTimeGetEvent`)
    /// delivers only the timestamp, so the tag must come through device
    /// support — mirroring C device support writing `prec->utag`
    /// directly during `read()` (alongside `prec->time`, TSE=-2).
    fn last_utag(&self) -> Option<u64> {
        None
    }

    /// Called by the framework immediately before [`read()`](DeviceSupport::read)
    /// to push a read-only snapshot of framework-owned `CommonFields`
    /// state ([`crate::server::record::ProcessContext`]) that the device
    /// support needs.
    ///
    /// `read()` receives only `&mut dyn Record`; it cannot reach
    /// `RecordInstance.common`. C device support reads `dbCommon`
    /// directly — `devTimeOfDay.c:122` selects its time format from
    /// `psi->phas`. A driver that needs `phas`/`udf`/`tse`/`tsel`
    /// overrides this to stash the values before `read()` runs.
    ///
    /// Additive framework-set-hook (same shape as
    /// [`DeviceSupport::set_record_info`]). Default: ignore.
    fn set_process_context(&mut self, _ctx: &crate::server::record::ProcessContext) {}

    /// Called after init() with the record name and scan type.
    fn set_record_info(&mut self, _name: &str, _scan: ScanType) {}

    /// Forward parsed `info("key", "value")` directives from the .db
    /// file to the device support. Default is a no-op; drivers that
    /// react to specific tags (asyn `asyn:READBACK`, EtherCAT terminal
    /// hints, etc.) override this. Called once after `set_record_info`
    /// during builder wiring; not called again at runtime.
    fn apply_record_info(&mut self, _info: &std::collections::HashMap<String, String>) {}

    /// Return a receiver for I/O Intr scan notifications.
    /// Called for records with `SCAN="I/O Intr"`, and for any device that
    /// reports [`io_intr_scan_independent`](Self::io_intr_scan_independent).
    fn io_intr_receiver(&mut self) -> Option<crate::runtime::sync::mpsc::Receiver<()>> {
        None
    }

    /// Return a receiver of out-of-band PROPERTY-class field posts.
    ///
    /// C parity: `registerInterruptUser(callbackEnum)` (devAsynInt32.c:319)
    /// plus the per-record enum callback
    /// (`interruptCallbackEnumMbbi`/`…Bi`, devAsynInt32.c:711-762), which
    /// calls `setEnums` to re-key the record's state strings/values/
    /// severities and then `db_post_events(precord, &precord->val,
    /// DBE_PROPERTY)` so CA/PVA clients re-read the enum choices. This is
    /// driven by the driver's `doCallbacksEnum`, independent of the
    /// record's `SCAN` (it is not a value scan, so it does not process the
    /// record).
    ///
    /// Each delivered message is the full `(field, value)` set to
    /// write-and-post (the C `setEnums` field block). The framework drains
    /// this receiver and calls
    /// [`crate::server::database::PvDatabase::post_property_fields`].
    /// Mirrors [`io_intr_receiver`](Self::io_intr_receiver): the device owns
    /// the source subscription, the framework owns the post. Default:
    /// `None` (device drives no property posts).
    fn property_post_receiver(
        &mut self,
    ) -> Option<crate::runtime::sync::mpsc::Receiver<Vec<(String, crate::types::EpicsValue)>>> {
        None
    }

    /// Whether this device drives record processing from its own callback
    /// channel independently of the runtime `SCAN` menu.
    ///
    /// C parity: a `motorRecord` device callback (`statusCallback`) does its
    /// own `dbScanLock` + `dbProcess` on every poll readback regardless of
    /// `SCAN`, and the record stays `SCAN="Passive"` so a `dbPutField` to a
    /// `pp(TRUE)` field (VAL/DVAL/...) still re-processes it
    /// (`dbAccess.c:1263-1268`). asyn readback records behave the same way
    /// (upstream PRs #60/#208 — output records follow driver-side changes
    /// regardless of `SCAN`).
    ///
    /// When `true`, the I/O Intr wiring processes the record on every pulse
    /// even when `SCAN != "I/O Intr"`. When `false` (default), processing is
    /// gated on the record's current `SCAN` being `"I/O Intr"`, matching C
    /// `scanIoRequest`, which honors scan-list membership.
    fn io_intr_scan_independent(&self) -> bool {
        false
    }

    /// Arm an output driver-callback (`asyn:READBACK`) cycle.
    ///
    /// Called by [`crate::server::database::PvDatabase::process_record_readback`]
    /// immediately before the processing pass, mirroring C
    /// `devAsynInt32.c::outputCallbackCallback` setting
    /// `newOutputCallbackValue = 1` before `dbProcess`. Pair with
    /// [`Self::reconcile_readback_callback`]: if the pass never reaches the
    /// device read stage (the PACT entry guard bails because a put / FLNK
    /// cycle still owns the record), the armed flag survives and reconcile
    /// discards the stale callback-ring entry so a callback ring never
    /// desyncs from the record's pop count. Default no-op — only output
    /// callback-driven device support (asyn readback) needs it.
    fn arm_readback_callback(&mut self) {}

    /// Reconcile an armed output driver-callback cycle after processing.
    ///
    /// C `outputCallbackCallback` fallback (devEpics `devAsynInt32.c`): after
    /// `dbProcess`, if `newOutputCallbackValue` is still set the record did
    /// not process, so `getCallbackValue` is called to drop the stale ring
    /// entry. Default no-op; see [`Self::arm_readback_callback`].
    fn reconcile_readback_callback(&mut self) {}

    /// Whether a driver-callback (`asyn:READBACK`) processing cycle replaces
    /// this device's output stage with a value readback.
    ///
    /// C devEpics (`devAsynInt32.c::processAo`/`processBo`/…) takes the
    /// `newOutputCallbackValue` readback branch on a callback cycle and never
    /// calls the output `write()` — re-writing would re-assert the setpoint
    /// and re-trigger the driver (the AD `Acquire` loop). Only device support
    /// implementing that contract (the [`Self::arm_readback_callback`] /
    /// [`Self::reconcile_readback_callback`] pair) returns `true`.
    ///
    /// Default `false`: a C `dbProcess` driven by a driver callback is a full
    /// record process, output stage included. `devMotorAsyn` has no readback
    /// suppression — the motor record dispatches its retry, backlash-leg,
    /// NTM-stop, and queued-motion-resume commands from exactly these
    /// CALLBACK_DATA passes, and suppressing the write strands them in the
    /// command mailbox (DMOV stuck 0, MIP=RETRY|MOVE, later puts time out).
    fn output_callback_readback(&self) -> bool {
        false
    }

    /// Begin an asynchronous write (submit only, no blocking).
    /// Returns `Some(handle)` if the write was submitted to a worker queue —
    /// the caller should wait on the handle outside any record lock.
    /// Returns `None` to fall back to synchronous [`write()`](DeviceSupport::write).
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
    ) -> CaResult<Option<Box<dyn WriteCompletion>>> {
        Ok(None)
    }

    /// Handle a named command from the record's process() via
    /// `ProcessAction::DeviceCommand`. This allows records to request
    /// driver operations (e.g., scaler reset/arm/write_preset) without
    /// holding a direct driver reference.
    ///
    /// `handle_command` runs AFTER the process snapshot has already been
    /// built and notified, so any record field it mutates would not be
    /// diffed by the snapshot path. The returned `Vec` names the record
    /// fields the command changed; the framework posts a `DBE_VALUE`
    /// monitor event for each, mirroring the explicit `db_post_events`
    /// calls a C record makes from inside `process()` (e.g.
    /// `scalerRecord.c:425-430` posts PR1/TP/FREQ after the driver
    /// write-back). Return an empty `Vec` when no record field changed.
    ///
    /// Default: ignore, no fields changed.
    fn handle_command(
        &mut self,
        _record: &mut dyn Record,
        _command: &str,
        _args: &[crate::types::EpicsValue],
    ) -> CaResult<Vec<&'static str>> {
        Ok(Vec::new())
    }
}

/// Canonical device-support init sequence — the single owner of the
/// "attach device support to a record" contract.
///
/// Both build paths (`crate::server::ioc_app::wire_device_support`
/// and [`crate::server::ioc_builder::IocBuilder::build`]) MUST call
/// this so a driver author can write one correct `init()`.
///
/// Order (C parity — `recGblInitConstantLink`-style field setup runs
/// before `init_record`; `set_record_info` / `apply_record_info` are
/// Rust extensions that supply that field context and therefore
/// precede `init`):
///
/// 1. `set_record_info(name, scan)` — give the driver its record
///    identity and scan mode.
/// 2. `apply_record_info(info)` — forward `info(...)` tags so a
///    driver that reads them inside `init()` sees a populated map.
/// 3. `init(record)` — driver `init_record` equivalent.
///
/// On `init()` failure the record is flagged `INVALID` severity with
/// a `SOFT` status and a diagnostic is logged, so the failure is
/// observable rather than silently attached as healthy.
///
/// Success clears NOTHING. In C, whether an `init_record` defines the
/// record is a property of the individual dset, not of the framework:
/// `devTimestamp.c` declares no `init_record` at all and clears
/// `prec->udf` only inside `read_ai`/`read_stringin` (`:40`, `:65`),
/// and `iocInit.c::doInitRecord0` (`:508-533`) only READS `udf` to
/// derive the initial severity. A record whose device support has
/// produced no value is still undefined, and says so.
///
/// The device is attached (`instance.device = Some(dev)`) regardless
/// of init outcome so the record is addressable; a failed init leaves
/// the alarm set.
pub fn wire_device_to_record(instance: &mut RecordInstance, mut dev: Box<dyn DeviceSupport>) {
    let name = instance.name.clone();
    dev.set_record_info(&name, instance.common.scan);
    dev.apply_record_info(&instance.info);
    if let Err(e) = dev.init(&mut *instance.record) {
        eprintln!(
            "device support init failed for record '{name}' (DTYP '{}'): {e}",
            instance.common.dtyp
        );
        // Flag the record so the failure is observable rather
        // than presenting a healthy-looking record.
        instance.common.sevr = AlarmSeverity::Invalid;
        instance.common.stat = crate::server::recgbl::alarm_status::SOFT_ALARM;
    }
    instance.device = Some(dev);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CaError;
    use crate::server::record::{AlarmSeverity, Record, RecordInstance, ScanType};
    use crate::server::records::ai::AiRecord;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Observed wiring state, shared with the test via `Arc` so it is
    /// inspectable after the device is moved into the record.
    #[derive(Default)]
    struct WireObservation {
        /// Info keys visible to `init()`.
        info_at_init: Vec<String>,
        /// Whether `set_record_info` ran before `init()`.
        record_info_before_init: bool,
        /// Whether `set_record_info` had run by the time `init` ran.
        init_ran: bool,
    }

    /// Device support that records the wiring order and fails `init`.
    struct ProbeDev {
        obs: Arc<Mutex<WireObservation>>,
        info: HashMap<String, String>,
        record_info_set: bool,
        fail_init: bool,
    }
    impl DeviceSupport for ProbeDev {
        fn dtyp(&self) -> &str {
            "ProbeDev"
        }
        fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
            Ok(())
        }
        fn set_record_info(&mut self, _name: &str, _scan: ScanType) {
            self.record_info_set = true;
        }
        fn apply_record_info(&mut self, info: &HashMap<String, String>) {
            self.info = info.clone();
        }
        fn init(&mut self, _record: &mut dyn Record) -> CaResult<()> {
            let mut o = self.obs.lock().unwrap();
            o.init_ran = true;
            o.record_info_before_init = self.record_info_set;
            o.info_at_init = self.info.keys().cloned().collect();
            if self.fail_init {
                Err(CaError::InvalidValue("device init failed".into()))
            } else {
                Ok(())
            }
        }
    }

    /// M2 regression: a device support whose `init()` returns `Err`
    /// must NOT be attached as a healthy record — the record is
    /// flagged INVALID severity with a SOFT status. (Pre-fix the
    /// IocBuilder path discarded the error with `let _ =`.)
    #[test]
    fn wire_device_init_failure_flags_record_invalid() {
        let mut instance = RecordInstance::new("TEST:AI".to_string(), AiRecord::new(0.0));
        instance.common.dtyp = "ProbeDev".to_string();
        let obs = Arc::new(Mutex::new(WireObservation::default()));
        let dev = Box::new(ProbeDev {
            obs: obs.clone(),
            info: HashMap::new(),
            record_info_set: false,
            fail_init: true,
        });

        wire_device_to_record(&mut instance, dev);

        assert_eq!(
            instance.common.sevr,
            AlarmSeverity::Invalid,
            "failed device init must flag the record INVALID"
        );
        assert_eq!(
            instance.common.stat,
            crate::server::recgbl::alarm_status::SOFT_ALARM,
        );
        assert!(
            instance.device.is_some(),
            "device is still attached so the record is addressable"
        );
    }

    /// M1 regression: the canonical wiring order is
    /// set_record_info → apply_record_info → init. A driver reading
    /// `info(...)` tags inside `init()` must see a populated map, and
    /// `set_record_info` must have run first.
    #[test]
    fn wire_device_applies_info_and_record_info_before_init() {
        let mut instance = RecordInstance::new("TEST:AI2".to_string(), AiRecord::new(0.0));
        instance.common.dtyp = "ProbeDev".to_string();
        instance.set_info("asyn:READBACK", "1");
        let obs = Arc::new(Mutex::new(WireObservation::default()));
        let dev = Box::new(ProbeDev {
            obs: obs.clone(),
            info: HashMap::new(),
            record_info_set: false,
            fail_init: false,
        });

        wire_device_to_record(&mut instance, dev);

        let o = obs.lock().unwrap();
        assert!(o.init_ran, "init must have run");
        assert!(
            o.record_info_before_init,
            "set_record_info must run before init"
        );
        assert!(
            o.info_at_init.iter().any(|k| k == "asyn:READBACK"),
            "info(...) tags must be visible inside init()"
        );
    }
}
