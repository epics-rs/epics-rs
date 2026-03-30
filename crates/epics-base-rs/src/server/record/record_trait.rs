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

/// Result of a record's process() call.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordProcessResult {
    /// Processing completed synchronously this cycle.
    Complete,
    /// Processing started but not yet complete (PACT stays set).
    AsyncPending,
    /// Async pending, but notify these intermediate field changes immediately.
    /// Used by motor records to flush DMOV=0 before the move completes.
    AsyncPendingNotify(Vec<(String, EpicsValue)>),
}

/// Result of setting a common field, indicating what scan index updates are needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonFieldPutResult {
    NoChange,
    ScanChanged { old_scan: ScanType, new_scan: ScanType, phas: i16 },
    PhasChanged { scan: ScanType, old_phas: i16, new_phas: i16 },
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
    fn process(&mut self) -> CaResult<RecordProcessResult> {
        Ok(RecordProcessResult::Complete)
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
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        self.put_field(self.primary_field(), value)
    }

    /// Whether this record type supports device write (output records only).
    fn can_device_write(&self) -> bool {
        matches!(self.record_type(), "ao" | "bo" | "longout" | "mbbo" | "stringout")
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
    /// Defaults to true. Override in calcout to implement OOPT conditional output.
    fn should_output(&self) -> bool {
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
}

/// Subroutine function type for sub records.
pub type SubroutineFn = Box<dyn Fn(&mut dyn Record) -> CaResult<()> + Send + Sync>;
