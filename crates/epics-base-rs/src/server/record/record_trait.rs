use crate::error::CaResult;
use crate::types::{DbFieldType, EpicsValue};

use super::scan::ScanType;

/// Metadata describing a single field in a record.
#[derive(Debug, Clone)]
pub struct FieldDesc {
    pub name: &'static str,
    pub dbf_type: DbFieldType,
    pub read_only: bool,
}

/// Side-effect actions that a record requests from the processing framework.
///
/// Records return these from `process()` via `ProcessOutcome::actions`.
/// The framework executes them at the appropriate point in the processing
/// cycle, keeping records as pure state machines without direct DB access.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessAction {
    /// Write a value to a DB link. The framework reads `link_field` from the
    /// record to get the target PV name, then writes `value` to that PV.
    ///
    /// Executed after alarm/snapshot, before FLNK.
    /// Example: scaler writes CNT to COUT/COUTP links.
    WriteDbLink {
        link_field: &'static str,
        value: EpicsValue,
    },

    /// Read a value from a DB link into a record field. The framework reads
    /// `link_field` from the record to get the source PV name, reads that PV,
    /// and writes the result into `target_field` via an internal put that
    /// bypasses read-only checks.
    ///
    /// **Pre-process action**: executed BEFORE the next process() cycle so
    /// the value is immediately available. This matches C EPICS `dbGetLink()`
    /// which is synchronous/immediate.
    ///
    /// Example: throttle reads SINP into VAL when SYNC is triggered.
    ReadDbLink {
        link_field: &'static str,
        target_field: &'static str,
    },

    /// Schedule a re-process of this record after the given duration.
    /// The framework spawns `tokio::spawn(sleep(d) + process_record(name))`.
    /// The current cycle's OUT/FLNK/notify proceed normally.
    ///
    /// Equivalent to C EPICS `callbackRequestDelayed()` + `scanOnce()`.
    ReprocessAfter(std::time::Duration),

    /// Send a named command to the device support driver.
    /// The framework calls `DeviceSupport::handle_command()` with this data.
    /// Used by scaler to request reset/arm/write_preset operations
    /// without the record holding a direct driver reference.
    DeviceCommand {
        command: &'static str,
        args: Vec<EpicsValue>,
    },
}

/// Result of a record's process() call.
///
/// Determines how the framework handles the current processing cycle.
/// Side-effect actions (link writes, delayed reprocess, etc.) are expressed
/// separately in `ProcessOutcome::actions`.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordProcessResult {
    /// Processing completed synchronously this cycle.
    /// Framework proceeds with alarm/timestamp/snapshot/OUT/FLNK.
    Complete,
    /// Processing started but not yet complete (PACT stays set).
    /// Current cycle skips alarm/timestamp/snapshot/OUT/FLNK.
    /// ProcessActions (if any) are still executed.
    AsyncPending,
    /// Async pending, but notify these intermediate field changes immediately.
    /// Used by motor records to flush DMOV=0 before the move completes.
    AsyncPendingNotify(Vec<(String, EpicsValue)>),
}

/// Complete outcome of a record's process() call.
///
/// Contains the processing result (Complete, AsyncPending, etc.) and a list
/// of side-effect actions for the framework to execute.
#[derive(Clone, Debug)]
pub struct ProcessOutcome {
    pub result: RecordProcessResult,
    pub actions: Vec<ProcessAction>,
    /// Set by the framework when device support's read() returned
    /// `did_compute: true`. The record's process() can check this to
    /// skip its built-in computation (e.g., PID). Replaces the `pid_done`
    /// flag pattern.
    pub device_did_compute: bool,
}

impl ProcessOutcome {
    /// Shorthand for a simple Complete with no actions.
    pub fn complete() -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions: Vec::new(),
            device_did_compute: false,
        }
    }

    /// Shorthand for Complete with actions.
    pub fn complete_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions,
            device_did_compute: false,
        }
    }

    /// Shorthand for AsyncPending with no actions.
    pub fn async_pending() -> Self {
        Self {
            result: RecordProcessResult::AsyncPending,
            actions: Vec::new(),
            device_did_compute: false,
        }
    }
}

impl Default for ProcessOutcome {
    fn default() -> Self {
        Self::complete()
    }
}

/// Result of setting a common field, indicating what scan index updates are needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonFieldPutResult {
    NoChange,
    ScanChanged {
        old_scan: ScanType,
        new_scan: ScanType,
        phas: i16,
    },
    PhasChanged {
        scan: ScanType,
        old_phas: i16,
        new_phas: i16,
    },
}

/// Snapshot of changes from a process cycle, used for notify outside lock.
pub struct ProcessSnapshot {
    pub changed_fields: Vec<(String, EpicsValue)>,
    /// Event mask computed for this cycle.
    pub event_mask: crate::server::recgbl::EventMask,
}

/// Trait that all EPICS record types must implement.
pub trait Record: Send + Sync + 'static {
    /// Return the record type name (e.g., "ai", "ao", "bi").
    fn record_type(&self) -> &'static str;

    /// Process the record (scan/compute cycle).
    ///
    /// Returns a `ProcessOutcome` containing the processing result and any
    /// side-effect actions for the framework to execute.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    /// Optional: report whether this record's last `process()` call
    /// mutated a metadata-class field (EGU/PREC/HOPR/LOPR/HLM/LLM/
    /// alarm limits / DRVH/DRVL / state strings).
    ///
    /// The framework checks this after every `process()` call and, if
    /// true, invalidates the record's metadata cache so the next
    /// snapshot rebuilds from the new values.
    ///
    /// Default: `false` — most records never touch metadata fields
    /// during processing. Override only when your record dynamically
    /// adjusts limits or unit strings (e.g., a motor that recomputes
    /// HLM/LLM after a hardware homing operation).
    ///
    /// Implementations should reset their internal flag after returning
    /// `true` so the next cycle starts clean.
    fn took_metadata_change(&mut self) -> bool {
        false
    }

    /// Get a field value by name.
    fn get_field(&self, name: &str) -> Option<EpicsValue>;

    /// Set a field value by name.
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()>;

    /// Return the list of field descriptors.
    fn field_list(&self) -> &'static [FieldDesc];

    /// Validate a put before it is applied. Return Err to reject.
    fn validate_put(&self, _field: &str, _value: &EpicsValue) -> CaResult<()> {
        Ok(())
    }

    /// Hook called after a successful put_field.
    fn on_put(&mut self, _field: &str) {}

    /// Primary field name (default "VAL"). Override for waveform etc.
    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// Get the primary value.
    fn val(&self) -> Option<EpicsValue> {
        self.get_field(self.primary_field())
    }

    /// Set the primary value.
    ///
    /// Matches C EPICS `dbPut` behavior: if the value type doesn't match
    /// the field type, it is automatically coerced (e.g., Long→Double for
    /// ai, Long→Enum for bi/mbbi). This prevents silent failures when
    /// asyn device support provides Int32 values to Enum-typed records.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        let field = self.primary_field();
        match self.put_field(field, value.clone()) {
            Ok(()) => Ok(()),
            Err(crate::error::CaError::TypeMismatch(_)) => {
                // Auto-coerce: determine target type from current VAL
                let target_type = self
                    .get_field(field)
                    .map(|v| v.db_field_type())
                    .unwrap_or(DbFieldType::Double);
                let coerced = value.convert_to(target_type);
                self.put_field(field, coerced)
            }
            Err(e) => Err(e),
        }
    }

    /// Apply IVOA=2 ("set outputs to IVOV") semantics: copy the
    /// IVOV value into whatever output staging field the OUT
    /// writeback consumes for this record type. Mirrors the
    /// per-record C `recXxx.c` behaviour:
    ///
    /// - `ao`/`lso`: `OVAL = IVOV; VAL = OVAL`
    /// - `bo`/`busy`/`mbbo`/`mbboDirect`: `RVAL = IVOV; VAL = IVOV`
    /// - `calcout`/`scalcout`: `OVAL = IVOV` (VAL is calc input, not
    ///   touched on invalid-output)
    /// - `dfanout`: `VAL = IVOV` (the broadcast value)
    ///
    /// Default uses [`Record::set_val`] for records whose OUT path
    /// reads VAL only.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        self.set_val(ivov)
    }

    /// Whether this record type supports device write (output records only).
    fn can_device_write(&self) -> bool {
        matches!(
            self.record_type(),
            "ao" | "bo" | "longout" | "int64out" | "mbbo" | "mbboDirect" | "stringout" | "lso"
        )
    }

    /// Whether async processing has completed and put_notify can respond.
    /// Records that return AsyncPendingNotify should return false while
    /// async work is in progress, and true when done.
    /// Default: true (synchronous records are always complete).
    fn is_put_complete(&self) -> bool {
        true
    }

    /// Whether this record should fire its forward link after processing.
    fn should_fire_forward_link(&self) -> bool {
        true
    }

    /// Whether this record's OUT link should be written after processing.
    /// Defaults to true. Override in calcout / longout to implement OOPT
    /// conditional output (epics-base 7.0.8).
    fn should_output(&self) -> bool {
        true
    }

    /// Notify the record that the OUT-link / device write completed
    /// successfully on this cycle. The framework calls this right after
    /// the actual write so transition-detection state (e.g.
    /// `longout.pval`) can update for the next cycle's
    /// [`Self::should_output`] check. Default: no-op.
    fn on_output_complete(&mut self) {}

    /// Whether this record uses MDEL/ADEL deadband for monitor posting.
    /// Binary records (bi, bo, busy, mbbi, mbbo) return false because
    /// C EPICS always posts monitors for these record types regardless
    /// of whether the value changed.
    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    /// Initialize record (pass 0: field defaults; pass 1: dependent init).
    fn init_record(&mut self, _pass: u8) -> CaResult<()> {
        Ok(())
    }

    /// Called before/after a field put for side-effect processing.
    fn special(&mut self, _field: &str, _after: bool) -> CaResult<()> {
        Ok(())
    }

    /// Downcast to concrete type for device support init injection.
    /// Override in record types that need device support to inject state (e.g., MotorRecord).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

    /// Whether processing this record should clear UDF.
    /// Override to return false for record types that don't produce a valid value every cycle.
    fn clears_udf(&self) -> bool {
        true
    }

    /// Return multi-input link field pairs: (link_field, value_field).
    /// Override in calc, calcout, sel, sub to return INPA..INPL → A..L mappings.
    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// Return multi-output link field pairs: (link_field, value_field).
    /// Override in transform to return OUTA..OUTP → A..P mappings.
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// Internal field write that bypasses read-only checks.
    /// Used by the framework to write values from ReadDbLink actions
    /// into fields that are normally read-only (e.g., epid.CVAL).
    /// Default implementation delegates to put_field().
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_field(name, value)
    }

    /// Return pre-process actions (ReadDbLink) that the framework should
    /// execute BEFORE calling process(). This is called once per cycle.
    /// Default returns empty. Override in records that need link reads
    /// to be available during process().
    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        Vec::new()
    }

    /// Called by the framework before process() to indicate whether device
    /// support's read() already performed the record's compute step.
    /// Override in records that have a built-in compute (e.g., epid PID)
    /// to skip it when device support already ran it.
    /// Default: ignore.
    fn set_device_did_compute(&mut self, _did_compute: bool) {}
}

/// Subroutine function type for sub records.
pub type SubroutineFn = Box<dyn Fn(&mut dyn Record) -> CaResult<()> + Send + Sync>;
