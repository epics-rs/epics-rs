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
pub struct BridgeChannel {
    db: Arc<PvDatabase>,
    record_name: String,
    nt_type: NtType,
    /// The DBF type of the primary value field.
    value_dbf: DbFieldType,
    /// Access control context — checked on every get/put.
    access: super::provider::AccessContext,
}

impl BridgeChannel {
    /// Create from cached metadata (no DB introspection needed).
    pub fn from_cached(
        db: Arc<PvDatabase>,
        record_name: String,
        nt_type: NtType,
        value_dbf: DbFieldType,
    ) -> Self {
        Self {
            db,
            record_name,
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

    /// Create a new channel for a record.
    ///
    /// Reads the record type to determine the NormativeType mapping.
    pub async fn new(db: Arc<PvDatabase>, name: &str) -> BridgeResult<Self> {
        let (record_name, _field) = epics_base_rs::server::database::parse_pv_name(name);

        let rec = db
            .get_record(record_name)
            .await
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read().await;
        let rtyp = instance.record.record_type();
        let nt_type = NtType::from_record_type(rtyp);

        // Determine the DBF type of the primary value field
        let value_dbf = instance
            .record
            .field_list()
            .iter()
            .find(|f| f.name == "VAL")
            .map(|f| f.dbf_type)
            .unwrap_or(DbFieldType::Double);

        Ok(Self {
            db,
            record_name: record_name.to_string(),
            nt_type,
            value_dbf,
            access: super::provider::AccessContext::allow_all(),
        })
    }

    /// The NormativeType for this channel.
    pub fn nt_type(&self) -> NtType {
        self.nt_type
    }

    /// The DBF type of the primary value field.
    pub fn value_dbf(&self) -> DbFieldType {
        self.value_dbf
    }
}

impl Channel for BridgeChannel {
    fn channel_name(&self) -> &str {
        &self.record_name
    }

    async fn get(&self, request: &PvStructure) -> BridgeResult<PvStructure> {
        if !self.access.can_read(&self.record_name) {
            return Err(BridgeError::PutRejected(format!(
                "read denied for {} (user='{}' host='{}')",
                self.record_name, self.access.user, self.access.host
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
                .snapshot_for_field("VAL")
                .ok_or_else(|| BridgeError::FieldNotFound {
                    record: self.record_name.clone(),
                    field: "VAL".into(),
                })?;

        let full = snapshot_to_pv_structure(&snapshot, self.nt_type);
        Ok(pvif::filter_by_request(&full, request))
    }

    async fn put(&self, value: &PvStructure) -> BridgeResult<()> {
        if !self.access.can_write(&self.record_name) {
            return Err(BridgeError::PutRejected(format!(
                "write denied for {} (user='{}' host='{}')",
                self.record_name, self.access.user, self.access.host
            )));
        }

        let opts = PutOptions::from_pv_request(value);

        // Extract value from the NormativeType structure
        let raw_val = pv_structure_to_epics(value).ok_or_else(|| BridgeError::TypeMismatch {
            expected: "extractable value".into(),
            got: value.struct_id.to_string(),
        })?;

        // Use typed conversion to match the record's actual DBF type
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

        // BR-R20: pvxs distinguishes Force vs Passive — both write
        // VAL, but Force *also* triggers an explicit
        // process-record afterwards. The Rust path previously
        // collapsed Force into Passive, so a `record[process=true]`
        // put against a SCAN=Passive record without the field
        // having PP=YES would silently NOT process. Mirror pvxs
        // by routing Force through put_pv + process_record (the
        // dbProcess-equivalent that ignores PP / SCAN), and
        // leave Passive on the standard put-record-field path
        // that honors PP.
        match opts.process {
            ProcessMode::Inhibit => {
                // Write without processing (C++ ProcInhibit).
                self.db
                    .put_pv(&format!("{}.VAL", self.record_name), epics_val)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
            }
            ProcessMode::Passive => {
                // C++ ProcPassive: write VAL through the standard
                // CA path, which honors PP and SCAN.
                let notify_rx = self
                    .db
                    .put_record_field_from_ca(&self.record_name, "VAL", epics_val)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                if opts.block
                    && let Some(rx) = notify_rx
                {
                    let _ = rx.await;
                }
            }
            ProcessMode::Force => {
                // C++ ProcForce: write VAL, then explicitly process
                // the record regardless of PP / SCAN.
                self.db
                    .put_pv(&format!("{}.VAL", self.record_name), epics_val)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                self.db
                    .process_record(&self.record_name)
                    .await
                    .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                // `process_record` is synchronous from this call's
                // perspective — every async record device hook has
                // already been driven by the time it returns, so
                // `block=true` is satisfied by simply waiting for
                // this future.
            }
        }

        Ok(())
    }

    async fn get_field(&self) -> BridgeResult<FieldDesc> {
        let scalar_type = dbf_to_scalar_type(self.value_dbf);
        Ok(build_field_desc_for_nt(self.nt_type, scalar_type))
    }

    async fn create_monitor(&self) -> BridgeResult<super::group::AnyMonitor> {
        // Check read permission up front so a denied client cannot
        // even obtain a monitor handle. start() also re-checks (defense
        // in depth: handles created via with_access elsewhere).
        if !self.access.can_read(&self.record_name) {
            return Err(BridgeError::PutRejected(format!(
                "monitor create denied for {} (user='{}' host='{}')",
                self.record_name, self.access.user, self.access.host
            )));
        }
        Ok(super::group::AnyMonitor::Single(Box::new(
            BridgeMonitor::new(self.db.clone(), self.record_name.clone(), self.nt_type)
                .with_access(self.access.clone()),
        )))
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
}
