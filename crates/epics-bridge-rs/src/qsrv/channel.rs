//! BridgeChannel: single-record PVA channel.
//!
//! Corresponds to C++ QSRV's `PDBSinglePV` / `PDBSingleChannel`.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarValue};

use super::monitor::BridgeMonitor;
use super::provider::Channel;
use super::pvif::{
    self, NtType, build_field_desc_for_nt, pv_structure_to_epics, snapshot_to_pv_structure,
};
use crate::convert::{dbf_to_scalar_type, scalar_to_epics_typed};
use crate::error::{BridgeError, BridgeResult};

// ---------------------------------------------------------------------------
// PutOptions: pvRequest option parsing
// ---------------------------------------------------------------------------

/// Process mode for put operations.
///
/// Corresponds to C++ QSRV's `record._options.process` pvRequest field.
/// See `pdbsingle.cpp:305-338`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessMode {
    /// Default: process if record SCAN is Passive.
    Passive,
    /// "true": always trigger record processing.
    Force,
    /// "false": write value without triggering processing.
    Inhibit,
}

/// Options extracted from a pvRequest structure.
///
/// Corresponds to C++ QSRV's pvRequest option parsing in `PDBSinglePut` constructor.
#[derive(Debug, Clone)]
pub struct PutOptions {
    pub process: ProcessMode,
    /// If true, block until record processing completes (uses put_notify).
    pub block: bool,
}

impl Default for PutOptions {
    fn default() -> Self {
        Self {
            process: ProcessMode::Passive,
            block: false,
        }
    }
}

/// BR-R5: parse `record._options.DBE` from a MONITOR INIT
/// pvRequest. Returns the DBE bitmask as an EPICS event mask
/// (`EventMask::VALUE | ALARM | LOG | PROPERTY`), or `None` if
/// the option is absent / unparseable.
///
/// pvxs accepts a flag string with any of these tokens separated by
/// `|`, `,`, or whitespace: `VALUE`, `ALARM`, `LOG`/`ARCHIVE`,
/// `PROPERTY`. Numeric form is also accepted (e.g. `"5"` =
/// `VALUE|ALARM`).
pub fn dbe_mask_from_pv_request(request: &PvStructure) -> Option<u16> {
    use epics_base_rs::server::recgbl::EventMask;

    let options = request
        .get_field("record")
        .and_then(|f| match f {
            PvField::Structure(s) => s.get_field("_options"),
            _ => None,
        })
        .and_then(|f| match f {
            PvField::Structure(s) => Some(s),
            _ => None,
        })?;

    let dbe = options.get_field("DBE")?;
    let raw = match dbe {
        PvField::Scalar(ScalarValue::String(s)) => s.clone(),
        PvField::Scalar(ScalarValue::Int(n)) => return Some((*n as u32 & 0xFFFF) as u16),
        PvField::Scalar(ScalarValue::Long(n)) => return Some((*n as u32 & 0xFFFF) as u16),
        _ => return None,
    };

    // Numeric-as-string: `"5"` resolves to a raw mask.
    if let Ok(n) = raw.trim().parse::<u32>() {
        return Some((n & 0xFFFF) as u16);
    }

    let mut mask = EventMask::NONE;
    for tok in raw.split(|c: char| c == '|' || c == ',' || c.is_whitespace()) {
        let t = tok.trim().to_ascii_uppercase();
        let t = t.strip_prefix("DBE_").unwrap_or(&t);
        match t {
            "" => continue,
            "VALUE" => mask |= EventMask::VALUE,
            "ALARM" => mask |= EventMask::ALARM,
            // pvxs accepts both LOG and ARCHIVE for the legacy DBE_LOG bit.
            "LOG" | "ARCHIVE" => mask |= EventMask::LOG,
            "PROPERTY" => mask |= EventMask::PROPERTY,
            _ => return None,
        }
    }

    if mask.is_empty() {
        None
    } else {
        Some(mask.bits())
    }
}

/// BR-R16: parse `record._options.atomic` from a group operation
/// pvRequest. Returns `Some(true|false)` when the option is set,
/// `None` when absent — the caller then falls back to the group's
/// default atomicity. pvxs accepts either a boolean scalar
/// (`record._options.atomic = true`) or a `"true"`/`"false"` string;
/// both forms are supported here.
pub fn atomic_from_pv_request(request: &PvStructure) -> Option<bool> {
    let options = request
        .get_field("record")
        .and_then(|f| match f {
            PvField::Structure(s) => s.get_field("_options"),
            _ => None,
        })
        .and_then(|f| match f {
            PvField::Structure(s) => Some(s),
            _ => None,
        })?;

    match options.get_field("atomic")? {
        PvField::Scalar(ScalarValue::Boolean(b)) => Some(*b),
        PvField::Scalar(ScalarValue::String(s)) => match s.to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

impl PutOptions {
    /// Extract process/block options from a PvStructure.
    ///
    /// Looks for `record._options.process` ("true"|"false"|"passive")
    /// and `record._options.block` (boolean) fields.
    pub fn from_pv_request(request: &PvStructure) -> Self {
        let mut opts = Self::default();

        // Navigate: record -> _options -> process/block
        let options = request
            .get_field("record")
            .and_then(|f| match f {
                PvField::Structure(s) => s.get_field("_options"),
                _ => None,
            })
            .and_then(|f| match f {
                PvField::Structure(s) => Some(s),
                _ => None,
            });

        if let Some(opt_struct) = options {
            // process option
            if let Some(PvField::Scalar(ScalarValue::String(s))) = opt_struct.get_field("process") {
                opts.process = match s.as_str() {
                    "true" => ProcessMode::Force,
                    "false" => ProcessMode::Inhibit,
                    _ => ProcessMode::Passive,
                };
            }

            // block option
            if let Some(PvField::Scalar(ScalarValue::Boolean(b))) = opt_struct.get_field("block") {
                opts.block = *b;
                // No point blocking if we're not processing
                if opts.process == ProcessMode::Inhibit {
                    opts.block = false;
                }
            }
        }

        opts
    }
}

// ---------------------------------------------------------------------------
// BridgeChannel
// ---------------------------------------------------------------------------

/// A PVA channel backed by a single EPICS database record.
///
/// BR-R2: a channel binds to `record.FIELD`, not just `record`.
/// `pv_name` is the full client-facing PV identity (used by ACF,
/// monitor identity, error messages); `record_name` is the resolved
/// canonical record; `field` is the uppercased field name used by
/// every snapshot / put call against this channel. When the client
/// names a record without a field suffix, `field` defaults to
/// `"VAL"` (matching `parse_pv_name`).
pub struct BridgeChannel {
    db: Arc<PvDatabase>,
    /// Full client-facing PV name (`record.FIELD`, or `record` when
    /// no suffix was requested). Used for `channel_name()` and ACF.
    pv_name: String,
    record_name: String,
    /// Uppercased field name. Defaults to `"VAL"`.
    field: String,
    nt_type: NtType,
    /// The DBF type of the bound field (not always VAL — BR-R2).
    value_dbf: DbFieldType,
    /// Access control context — checked on every get/put.
    access: super::provider::AccessContext,
}

impl BridgeChannel {
    /// Choose the NormativeType for a single-record channel bound to
    /// `field`. Non-VAL field PVs are scalar in pvxs QSRV regardless of
    /// the record's record-type-derived NT mapping (a `waveform.NORD`
    /// is an `NTScalar` even though `waveform.VAL` is `NTScalarArray`).
    fn nt_type_for_field(record_type: &str, field: &str) -> NtType {
        if field.eq_ignore_ascii_case("VAL") {
            NtType::from_record_type(record_type)
        } else {
            NtType::Scalar
        }
    }

    /// Create from cached metadata (no DB introspection needed).
    pub fn from_cached(
        db: Arc<PvDatabase>,
        pv_name: String,
        record_name: String,
        field: String,
        nt_type: NtType,
        value_dbf: DbFieldType,
    ) -> Self {
        Self {
            db,
            pv_name,
            record_name,
            field,
            nt_type,
            value_dbf,
            access: super::provider::AccessContext::allow_all(),
        }
    }

    /// Inject an access control context. Called by [`super::provider::BridgeProvider`]
    /// after channel creation when client identity is known.
    pub fn with_access(mut self, access: super::provider::AccessContext) -> Self {
        self.access = access;
        self
    }

    /// Create a new channel for a record (or a `record.FIELD` PV).
    ///
    /// Reads the record's field list to determine the bound field's
    /// DBF type, and derives the NormativeType from the field-vs-VAL
    /// shape.
    pub async fn new(db: Arc<PvDatabase>, name: &str) -> BridgeResult<Self> {
        let (record_name, field) = epics_base_rs::server::database::parse_pv_name(name);
        let field_upper = field.to_ascii_uppercase();

        let rec = db
            .get_record(record_name)
            .await
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read().await;
        let rtyp = instance.record.record_type();
        let nt_type = Self::nt_type_for_field(rtyp, &field_upper);

        // DBF type for the bound field. Falls back to Double if the
        // record's `field_list` does not enumerate it explicitly —
        // common-fields like `.SCAN` / `.PROC` may not be in
        // record-specific tables but are still resolvable through
        // `resolve_field` and serialize through the generic Snapshot
        // path.
        let value_dbf = instance
            .record
            .field_list()
            .iter()
            .find(|f| f.name == field_upper)
            .map(|f| f.dbf_type)
            .unwrap_or(DbFieldType::Double);

        Ok(Self {
            db,
            pv_name: name.to_string(),
            record_name: record_name.to_string(),
            field: field_upper,
            nt_type,
            value_dbf,
            access: super::provider::AccessContext::allow_all(),
        })
    }

    /// The NormativeType for this channel.
    pub fn nt_type(&self) -> NtType {
        self.nt_type
    }

    /// The DBF type of the bound field (not always VAL).
    pub fn value_dbf(&self) -> DbFieldType {
        self.value_dbf
    }

    /// Resolved canonical record name (no field suffix).
    pub fn record_name(&self) -> &str {
        &self.record_name
    }

    /// Uppercased bound field name (defaults to `VAL`).
    pub fn field(&self) -> &str {
        &self.field
    }

    /// BR-R3: PUT with caller-supplied options.
    ///
    /// pvxs reads `record._options.process` and `record._options.block`
    /// from the INIT pvRequest (`iocsource.cpp:429`), not from the
    /// data-phase value. The wire layer captures the INIT pvRequest
    /// and forwards it via [`epics_pva_rs::server_native::source::
    /// ChannelContext::pv_request`]; the bridge converts it to
    /// [`PutOptions`] and calls this method directly.
    ///
    /// The access-control check is identical to [`Channel::put`].
    pub async fn put_with_options(
        &self,
        value: &PvStructure,
        opts: PutOptions,
    ) -> BridgeResult<()> {
        if !self.access.can_write(&self.pv_name) {
            return Err(BridgeError::PutRejected(format!(
                "write denied for {} (user='{}' host='{}')",
                self.pv_name, self.access.user, self.access.host
            )));
        }

        // Extract value from the NormativeType structure
        let raw_val = pv_structure_to_epics(value).ok_or_else(|| BridgeError::TypeMismatch {
            expected: "extractable value".into(),
            got: value.struct_id.to_string(),
        })?;

        // Use typed conversion to match the bound field's actual DBF type
        let epics_val = match &raw_val {
            EpicsValue::Double(_)
            | EpicsValue::Float(_)
            | EpicsValue::Short(_)
            | EpicsValue::Long(_)
            | EpicsValue::Char(_)
            | EpicsValue::Enum(_)
            | EpicsValue::String(_) => {
                let sv = crate::convert::epics_to_scalar(&raw_val);
                scalar_to_epics_typed(&sv, self.value_dbf)
            }
            // Arrays pass through directly
            _ => raw_val,
        };

        // BR-R20: pvxs distinguishes Force vs Passive — both write the
        // bound field, but Force *also* triggers an explicit
        // process-record afterwards.
        match opts.process {
            ProcessMode::Inhibit => {
                self.db
                    .put_pv(&format!("{}.{}", self.record_name, self.field), epics_val)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
            }
            ProcessMode::Passive => {
                let notify_rx = self
                    .db
                    .put_record_field_from_ca(&self.record_name, &self.field, epics_val)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                if opts.block
                    && let Some(rx) = notify_rx
                {
                    let _ = rx.await;
                }
            }
            ProcessMode::Force => {
                self.db
                    .put_pv(&format!("{}.{}", self.record_name, self.field), epics_val)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                self.db
                    .process_record(&self.record_name)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
            }
        }

        Ok(())
    }
}

impl Channel for BridgeChannel {
    fn channel_name(&self) -> &str {
        // BR-R2: report the full PV identity (`record.FIELD`) so ACF
        // checks and error messages distinguish field PVs from the
        // record PV.
        &self.pv_name
    }

    async fn get(&self, request: &PvStructure) -> BridgeResult<PvStructure> {
        if !self.access.can_read(&self.pv_name) {
            return Err(BridgeError::PutRejected(format!(
                "read denied for {} (user='{}' host='{}')",
                self.pv_name, self.access.user, self.access.host
            )));
        }

        let rec = self
            .db
            .get_record(&self.record_name)
            .await
            .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.clone()))?;

        let instance = rec.read().await;
        let snapshot =
            instance
                .snapshot_for_field(&self.field)
                .ok_or_else(|| BridgeError::FieldNotFound {
                    record: self.record_name.clone(),
                    field: self.field.clone(),
                })?;

        let full = snapshot_to_pv_structure(&snapshot, self.nt_type);
        Ok(pvif::filter_by_request(&full, request))
    }

    async fn put(&self, value: &PvStructure) -> BridgeResult<()> {
        // Backward-compat entry: parses options from the value
        // structure (the legacy location). New callers should
        // prefer [`BridgeChannel::put_with_options`] and pass options
        // extracted from the INIT pvRequest (BR-R3).
        let opts = PutOptions::from_pv_request(value);
        self.put_with_options(value, opts).await
    }

    async fn get_field(&self) -> BridgeResult<FieldDesc> {
        let scalar_type = dbf_to_scalar_type(self.value_dbf);
        Ok(build_field_desc_for_nt(self.nt_type, scalar_type))
    }

    async fn create_monitor(&self) -> BridgeResult<super::group::AnyMonitor> {
        self.create_monitor_with_value_mask(None).await
    }
}

impl BridgeChannel {
    /// BR-R5: create a monitor with an explicit value-subscription DBE
    /// mask. Called by `QsrvPvStore::subscribe_checked` after parsing
    /// `record._options.DBE` from the MONITOR INIT pvRequest. `None`
    /// uses the pvxs-parity default (`VALUE | ALARM`).
    pub async fn create_monitor_with_value_mask(
        &self,
        value_mask: Option<u16>,
    ) -> BridgeResult<super::group::AnyMonitor> {
        // Check read permission up front so a denied client cannot
        // even obtain a monitor handle. start() also re-checks (defense
        // in depth: handles created via with_access elsewhere).
        if !self.access.can_read(&self.pv_name) {
            return Err(BridgeError::PutRejected(format!(
                "monitor create denied for {} (user='{}' host='{}')",
                self.pv_name, self.access.user, self.access.host
            )));
        }
        let mut monitor = BridgeMonitor::new(
            self.db.clone(),
            self.record_name.clone(),
            self.field.clone(),
            self.nt_type,
        )
        .with_access(self.access.clone());
        if let Some(mask) = value_mask {
            monitor = monitor.with_value_mask(mask);
        }
        Ok(super::group::AnyMonitor::Single(Box::new(monitor)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_options_default() {
        let opts = PutOptions::default();
        assert_eq!(opts.process, ProcessMode::Passive);
        assert!(!opts.block);
    }

    #[test]
    fn put_options_from_empty_request() {
        let req = PvStructure::new("empty");
        let opts = PutOptions::from_pv_request(&req);
        assert_eq!(opts.process, ProcessMode::Passive);
        assert!(!opts.block);
    }

    #[test]
    fn put_options_process_true() {
        let mut options = PvStructure::new("");
        options.fields.push((
            "process".into(),
            PvField::Scalar(ScalarValue::String("true".into())),
        ));
        options
            .fields
            .push(("block".into(), PvField::Scalar(ScalarValue::Boolean(true))));

        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));

        let mut req = PvStructure::new("request");
        req.fields
            .push(("record".into(), PvField::Structure(record)));

        let opts = PutOptions::from_pv_request(&req);
        assert_eq!(opts.process, ProcessMode::Force);
        assert!(opts.block);
    }

    #[test]
    fn put_options_inhibit_disables_block() {
        let mut options = PvStructure::new("");
        options.fields.push((
            "process".into(),
            PvField::Scalar(ScalarValue::String("false".into())),
        ));
        options
            .fields
            .push(("block".into(), PvField::Scalar(ScalarValue::Boolean(true))));

        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));

        let mut req = PvStructure::new("request");
        req.fields
            .push(("record".into(), PvField::Structure(record)));

        let opts = PutOptions::from_pv_request(&req);
        assert_eq!(opts.process, ProcessMode::Inhibit);
        assert!(!opts.block); // block disabled when process=false
    }

    fn req_with_dbe(value: PvField) -> PvStructure {
        let mut options = PvStructure::new("");
        options.fields.push(("DBE".into(), value));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));
        let mut req = PvStructure::new("request");
        req.fields
            .push(("record".into(), PvField::Structure(record)));
        req
    }

    /// BR-R5: pvxs-style flag string with `|`-separated tokens
    /// resolves to the corresponding EPICS event mask bits.
    #[test]
    fn dbe_mask_parses_value_alarm() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String("VALUE | ALARM".into())));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// BR-R5: `DBE_` prefix and `ARCHIVE` alias for LOG are accepted.
    #[test]
    fn dbe_mask_accepts_dbe_prefix_and_archive_alias() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String(
            "DBE_VALUE,DBE_ARCHIVE,PROPERTY".into(),
        )));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(
            mask,
            (EventMask::VALUE | EventMask::LOG | EventMask::PROPERTY).bits()
        );
    }

    /// BR-R5: numeric integer DBE option is accepted as the raw mask.
    #[test]
    fn dbe_mask_accepts_integer_form() {
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(5)));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, 5);
    }

    /// BR-R5: missing DBE option resolves to None so the monitor
    /// falls back to the pvxs-parity default mask.
    #[test]
    fn dbe_mask_absent_returns_none() {
        let req = PvStructure::new("request");
        assert!(dbe_mask_from_pv_request(&req).is_none());
    }

    fn req_with_atomic(value: PvField) -> PvStructure {
        let mut options = PvStructure::new("");
        options.fields.push(("atomic".into(), value));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));
        let mut req = PvStructure::new("request");
        req.fields
            .push(("record".into(), PvField::Structure(record)));
        req
    }

    /// BR-R16: boolean `record._options.atomic = true` resolves to
    /// `Some(true)` so the group operation overrides the default.
    #[test]
    fn atomic_option_parses_boolean_true() {
        let req = req_with_atomic(PvField::Scalar(ScalarValue::Boolean(true)));
        assert_eq!(atomic_from_pv_request(&req), Some(true));
    }

    /// BR-R16: string form (`"false"`) is also accepted, matching
    /// pvxs's lenient option parsing.
    #[test]
    fn atomic_option_parses_string_false() {
        let req = req_with_atomic(PvField::Scalar(ScalarValue::String("false".into())));
        assert_eq!(atomic_from_pv_request(&req), Some(false));
    }

    /// BR-R16: absent option resolves to None so callers fall back to
    /// the group default.
    #[test]
    fn atomic_option_absent_returns_none() {
        let req = PvStructure::new("request");
        assert!(atomic_from_pv_request(&req).is_none());
    }
}
