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

/// Reduce a parsed numeric DBE mask to pvxs's value-subscription
/// class. pvxs masks `record._options.DBE` down to the value-class
/// bits `DBE_VALUE | DBE_ARCHIVE | DBE_ALARM` and falls back to
/// `DBE_VALUE | DBE_ALARM` when nothing in that class survives
/// (ioc/singlesource.cpp:142-144). PROPERTY and any out-of-class bits
/// must never reach the value subscription — the property
/// subscription is opened separately and unconditionally
/// (singlesource.cpp:161-167). EPICS `DBE_*` bit values coincide with
/// [`EventMask`] bits (VALUE=1, ARCHIVE/LOG=2, ALARM=4), so the raw
/// client mask maps straight through.
fn dbe_value_class_mask(raw: u16) -> u16 {
    use epics_base_rs::server::recgbl::EventMask;
    let m = raw & (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits();
    if m == 0 {
        (EventMask::VALUE | EventMask::ALARM).bits()
    } else {
        m
    }
}

/// parse `record._options.DBE` from a MONITOR INIT pvRequest into the
/// value-subscription mask. Returns `None` only when the option is
/// absent; a present option always resolves to a non-empty value-class
/// mask (pvxs's `VALUE|ALARM` fallback applies when nothing in the
/// value class is selected).
///
/// String form mirrors pvxs's "sloppy" substring parse
/// (singlesource.cpp:122-127): only `VALUE`, `ARCHIVE`, and `ALARM` are
/// recognized for the value mask. `LOG` is not a recognized spelling,
/// and `PROPERTY` is deliberately excluded — the property subscription
/// is separate and unconditional (singlesource.cpp:161-167). Numeric
/// form (`"5"` or an integer scalar) is masked to the value class the
/// same way.
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
    // Numeric DBE: pvxs reads it as `dbe = fld.as<uint8_t>()` then
    // applies the value-class mask + fallback (singlesource.cpp:134-144).
    // PROPERTY / out-of-class bits are stripped here so they cannot
    // leak into the value subscription.
    let raw = match dbe {
        PvField::Scalar(ScalarValue::String(s)) => s.clone(),
        PvField::Scalar(ScalarValue::Int(n)) => {
            return Some(dbe_value_class_mask((*n as u32 & 0xFFFF) as u16));
        }
        PvField::Scalar(ScalarValue::Long(n)) => {
            return Some(dbe_value_class_mask((*n as u32 & 0xFFFF) as u16));
        }
        _ => return None,
    };

    // Numeric-as-string: `"5"` resolves through the same value-class
    // mask as the integer form.
    if let Ok(n) = raw.trim().parse::<u32>() {
        return Some(dbe_value_class_mask((n & 0xFFFF) as u16));
    }

    // String DBE: pvxs does "sloppy" substring matching for only VALUE,
    // ARCHIVE, and ALARM (singlesource.cpp:122-127). `LOG` is NOT a
    // recognized spelling — the DBE_LOG/DBE_ARCHIVE bit (they are the
    // same EPICS bit) is selected only by the substring `ARCHIVE`. And
    // PROPERTY is deliberately excluded from the value mask
    // (`CASE(PROPERTY)` is commented out): the property subscription is
    // opened separately and unconditionally (singlesource.cpp:161-167),
    // so a `PROPERTY` token must never reach the value subscription.
    // Unknown text is ignored (pure substring search). The value-class
    // mask + VALUE|ALARM fallback (singlesource.cpp:142-144) then
    // applies, so a present `DBE` that selects an empty value mask
    // (PROPERTY-only, or unrecognized text like `LOG`) falls back to
    // VALUE|ALARM rather than leaving the value subscription empty.
    let upper = raw.to_ascii_uppercase();
    let mut raw_mask = 0u16;
    if upper.contains("VALUE") {
        raw_mask |= EventMask::VALUE.bits();
    }
    if upper.contains("ARCHIVE") {
        raw_mask |= EventMask::LOG.bits();
    }
    if upper.contains("ALARM") {
        raw_mask |= EventMask::ALARM.bits();
    }
    Some(dbe_value_class_mask(raw_mask))
}

/// parse `record._options.atomic` from a group operation
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

/// Map a `record._options.process` scalar to a [`ProcessMode`],
/// mirroring pvxs `setForceProcessingFlag` (ioc/iocsource.cpp:426-448):
/// `Value::as<bool>` accepts a bool, any integer (nonzero ⇒ true), or a
/// bool-parsable string; the literal `"passive"` (and any other
/// unrecognized value) leaves the mode at the passive default.
fn process_mode_from_scalar(sv: &ScalarValue) -> ProcessMode {
    let force = |b: bool| {
        if b {
            ProcessMode::Force
        } else {
            ProcessMode::Inhibit
        }
    };
    match sv {
        ScalarValue::Boolean(b) => force(*b),
        ScalarValue::Byte(n) => force(*n != 0),
        ScalarValue::Short(n) => force(*n != 0),
        ScalarValue::Int(n) => force(*n != 0),
        ScalarValue::Long(n) => force(*n != 0),
        ScalarValue::UByte(n) => force(*n != 0),
        ScalarValue::UShort(n) => force(*n != 0),
        ScalarValue::UInt(n) => force(*n != 0),
        ScalarValue::ULong(n) => force(*n != 0),
        ScalarValue::String(s) => match s.trim() {
            "true" => ProcessMode::Force,
            "false" => ProcessMode::Inhibit,
            // "passive" and any other string fall back to passive,
            // matching pvxs's `as<bool>` failure → "passive" check →
            // ignore-and-default chain.
            _ => ProcessMode::Passive,
        },
        // Float/Double are not a documented `process` encoding; pvxs's
        // as<bool> is not relied upon here. Leave passive.
        ScalarValue::Float(_) | ScalarValue::Double(_) => ProcessMode::Passive,
    }
}

/// Coerce a pvRequest scalar to a bool exactly as pvxs `Value::as<bool>`
/// does. `copyOutScalar` (`src/data.cpp:399-409`) converts a bool, any
/// signed/unsigned integer (nonzero ⇒ true), or a real to bool; the
/// string store (`src/data.cpp:459-462`) accepts only the exact tokens
/// `"true"` / `"false"` (no trim, case-sensitive) and otherwise raises
/// `NoConvert`. `None` mirrors that `NoConvert` outcome — the caller
/// keeps its default, matching pvxs `as<bool>(fallback)` returning the
/// fallback for an absent or unconvertible field.
///
/// This is intentionally distinct from [`process_mode_from_scalar`],
/// which is a tri-state (Force/Inhibit/Passive) routed through pvxs's
/// separate `setForceProcessingFlag` chain (real ⇒ passive, `"passive"`
/// recognized) — mirroring the two different pvxs code paths rather than
/// forcing one parser to serve both.
fn scalar_as_bool(sv: &ScalarValue) -> Option<bool> {
    match sv {
        ScalarValue::Boolean(b) => Some(*b),
        ScalarValue::Byte(n) => Some(*n != 0),
        ScalarValue::Short(n) => Some(*n != 0),
        ScalarValue::Int(n) => Some(*n != 0),
        ScalarValue::Long(n) => Some(*n != 0),
        ScalarValue::UByte(n) => Some(*n != 0),
        ScalarValue::UShort(n) => Some(*n != 0),
        ScalarValue::UInt(n) => Some(*n != 0),
        ScalarValue::ULong(n) => Some(*n != 0),
        ScalarValue::Float(v) => Some(*v != 0.0),
        ScalarValue::Double(v) => Some(*v != 0.0),
        ScalarValue::String(s) => match s.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
    }
}

impl PutOptions {
    /// Extract process/block options from a PvStructure.
    ///
    /// Looks for `record._options.process` (bool / integer / "true" /
    /// "false" / "passive") and `record._options.block` (bool / integer /
    /// unsigned / real / "true" / "false", via pvxs `as<bool>` coercion).
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
            // process option. pvxs reads `record._options.process` via
            // `Value::as<bool>` first — accepting an actual bool, an
            // integer, or a bool-parsable string — then falls back to the
            // literal string "passive"; anything else is ignored and the
            // mode stays passive/default (ioc/iocsource.cpp:426-448). The
            // earlier Rust path matched only `String`, silently dropping
            // the boolean and numeric forms a PVA client can legally send.
            if let Some(PvField::Scalar(sv)) = opt_struct.get_field("process") {
                opts.process = process_mode_from_scalar(sv);
            }

            // block option. pvxs reads `record._options.block` via
            // `Value::as<bool>` (ioc/singlesource.cpp:346-352), which
            // coerces bool / integer / unsigned / real / `"true"` /
            // `"false"` through `copyOutScalar`. The earlier path matched
            // only `Boolean`, silently dropping the integer and string
            // forms a PVA client can legally send — so a `block=1` or
            // `block="true"` lost the put-notify completion barrier.
            if let Some(PvField::Scalar(sv)) = opt_struct.get_field("block") {
                if let Some(b) = scalar_as_bool(sv) {
                    opts.block = b;
                }
            }
            // No point blocking if processing is inhibited
            // (singlesource.cpp:350-352: `doWait` is cleared whenever the
            // PUT does not process). Applied uniformly after parsing, so
            // the invariant `process == Inhibit ⇒ !block` holds for every
            // accepted `block` encoding, not only the boolean one.
            if opts.process == ProcessMode::Inhibit {
                opts.block = false;
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
/// a channel binds to `record.FIELD`, not just `record`.
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
    /// The DBF type of the bound field (not always VAL).
    value_dbf: DbFieldType,
    /// Parsed pvxs-compatible channel-filter chain from the trailing
    /// JSON suffix on the PV name (`PV.VAL{"dbnd":{"d":2.0}}`,
    /// `PV.VAL{"arr":{"s":1,"e":2}}`, …). Empty chain when the name
    /// carries no suffix. pvxs attaches the chain to the `dbChannel`, so
    /// it governs BOTH the monitor subscription AND one-shot GET reads:
    /// GET wraps the read in a `LocalFieldLog` and runs the pre/post
    /// chain (`ioc/singlesource.cpp:278-292`, `localfieldlog.cpp:15-24`).
    /// The monitor path installs the chain on its subscription; the GET
    /// path applies it in read context via
    /// [`FilterChain::apply_to_read_value`]. PUT writes the raw value
    /// (filters are read-side only).
    channel_filters: std::sync::Arc<epics_base_rs::server::database::filters::FilterChain>,
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
            channel_filters: std::sync::Arc::new(
                epics_base_rs::server::database::filters::FilterChain::new(),
            ),
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
    ///
    /// also peels off any trailing pvxs channel-filter JSON
    /// suffix (e.g. `test:ai.VAL{"dbnd":{"d":0.0}}`) via
    /// `split_channel_name` before record/field resolution, and
    /// stashes the parsed filter chain on the channel so the next
    /// `create_monitor` attaches it to the subscription.
    pub async fn new(db: Arc<PvDatabase>, name: &str) -> BridgeResult<Self> {
        let parsed = epics_base_rs::server::database::filters::split_channel_name(name);
        // A syntactically-present filter suffix that cannot be parsed
        // into the requested chain aborts channel creation, mirroring
        // EPICS `dbChannelCreate()` (`dbChannel.c:512-529`). Fail-open
        // to an unfiltered monitor would silently drop the requested
        // throttling/slicing semantics.
        let channel_filters = match parsed.json_suffix.as_deref() {
            Some(json) => std::sync::Arc::new(
                epics_base_rs::server::database::filters::try_parse_filter_chain(json)
                    .map_err(|e| BridgeError::ChannelFilterError(e.to_string()))?,
            ),
            None => {
                std::sync::Arc::new(epics_base_rs::server::database::filters::FilterChain::new())
            }
        };
        let resolution_name = parsed.record_path.as_str();
        let (record_name, field) = epics_base_rs::server::database::parse_pv_name(resolution_name);
        let field_upper = field.to_ascii_uppercase();

        let rec = db
            .get_record(record_name)
            .await
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read().await;
        let rtyp = instance.record.record_type();
        // Resolve the bound field's actual value once (record field →
        // common field → virtual field). This is the single source of
        // truth for both the served DBF type and (below) the NT shape,
        // so the advertised descriptor cannot drift from the value the
        // GET path will serialize.
        let resolved = instance.resolve_field(&field_upper);
        // A long-string field (`lsi`/`lso` VAL/OVAL, `printf` VAL) is a
        // `DBF_CHAR` array that semantically holds a NUL-terminated
        // string. Serve it as a scalar-string NTScalar (pvxs's
        // `form = "String"` view), not the byte scalar the `DBF_CHAR`
        // type would otherwise select. The record declares these fields
        // via `Record::long_string_fields`, so the bridge does not have
        // to hard-code record-type names.
        let nt_type = if instance
            .record
            .long_string_fields()
            .iter()
            .any(|f| f.eq_ignore_ascii_case(&field_upper))
        {
            NtType::LongString
        } else {
            Self::nt_type_for_field(rtyp, &field_upper)
        };

        // DBF type for the bound field, taken from the field's actual
        // resolved value. pvxs serves the type from
        // `dbChannelFinalFieldType(chan)` (singlesource.cpp:189-205,
        // dbChannel.h:452) — the channel's final field type after lookup,
        // which covers `dbCommon` fields, not only record-specific ones.
        // Deriving the DBF from the resolved `EpicsValue` makes the
        // advertised descriptor agree with the value the GET path returns
        // *by construction*: common/virtual fields such as `.SCAN`
        // (enum), `.DESC` (string), `.PROC` (char), and `.UTAG` (unsigned
        // 64) no longer fall back to `double`. The earlier `field_list`
        // lookup + `Double` fallback advertised `double value` for every
        // field a record's table did not enumerate, contradicting the
        // value the snapshot then serialized — a descriptor/value
        // wire-schema mismatch, not mere display drift. The
        // record-specific `field_list` is the fallback only for a field
        // that resolves to no concrete value, and `Double` the final
        // backstop.
        let value_dbf = resolved
            .as_ref()
            .map(|v| v.db_field_type())
            .or_else(|| {
                instance
                    .record
                    .field_list()
                    .iter()
                    .find(|f| f.name == field_upper)
                    .map(|f| f.dbf_type)
            })
            .unwrap_or(DbFieldType::Double);

        Ok(Self {
            db,
            pv_name: name.to_string(),
            record_name: record_name.to_string(),
            field: field_upper,
            nt_type,
            value_dbf,
            channel_filters,
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

    /// PUT with caller-supplied options.
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

        let epics_val = if self.nt_type == NtType::LongString {
            // Long-string channel: the QSRV value is a scalar string. The
            // backing record's `put_field` accepts `EpicsValue::String`
            // (and a legacy `CharArray`) directly and applies its own
            // SIZV bound. Do NOT retype the string to the bound
            // `DBF_CHAR` storage, which would try to parse the whole
            // string as a single integer and reject the PUT.
            match raw_val {
                EpicsValue::String(_) | EpicsValue::CharArray(_) => raw_val,
                // A non-string scalar PUT into a long-string field is
                // rendered to its textual form (pvxs string conversion);
                // the record then stores it.
                other => EpicsValue::String(match crate::convert::epics_to_scalar(&other) {
                    ScalarValue::String(s) => s,
                    sv => sv.to_string(),
                }),
            }
        } else {
            // Use typed conversion to match the bound field's actual DBF
            // type. `UInt64`/`Int64` MUST be in this scalar arm.
            // `pv_structure_to_epics` preserves a scalar PVA `ulong` as
            // `EpicsValue::UInt64` (and `long` as `Int64`) instead of
            // folding it into `Double`; routing it back through
            // `epics_to_scalar` recovers `ScalarValue::ULong`/`Long`, so
            // `scalar_to_epics_typed` sees the original 64-bit scalar and
            // retypes it to the bound field's DBF without an `f64`
            // round-trip. Omitting them would (a) skip retyping for a
            // `ulong` PUT into a non-`UINT64` field and (b) — before the
            // `scalar_to_epics` fix — still see a precision-lost `Double`.
            match &raw_val {
                EpicsValue::Double(_)
                | EpicsValue::Float(_)
                | EpicsValue::Short(_)
                | EpicsValue::Long(_)
                | EpicsValue::Int64(_)
                | EpicsValue::UInt64(_)
                | EpicsValue::Char(_)
                | EpicsValue::Enum(_)
                | EpicsValue::String(_) => {
                    let sv = crate::convert::epics_to_scalar(&raw_val);
                    // A string value bound for a numeric field that cannot
                    // be parsed is rejected here (pvxs `parseTo<T>` →
                    // `NoConvert`), not silently written as 0.
                    scalar_to_epics_typed(&sv, self.value_dbf)
                        .map_err(|e| BridgeError::PutRejected(e.to_string()))?
                }
                // Arrays pass through directly
                _ => raw_val,
            }
        };

        // pvxs distinguishes Force vs Passive — both write the
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
        // report the full PV identity (`record.FIELD`) so ACF
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
        let mut snapshot =
            instance
                .snapshot_for_field(&self.field)
                .ok_or_else(|| BridgeError::FieldNotFound {
                    record: self.record_name.clone(),
                    field: self.field.clone(),
                })?;

        // Apply the channel-filter chain in READ context. pvxs wraps
        // every QSRV GET in a `LocalFieldLog` and runs the field-log
        // pre/post chain before serialization (ioc/singlesource.cpp:
        // 278-292, ioc/localfieldlog.cpp:15-24); a GET on a filtered
        // channel must return the same transformed value as the monitor,
        // not the raw record snapshot. `arr` slicing and `ts` tagging
        // transform the value; the stream-only filters (`dbnd`/`dec`/
        // `sync`) short-circuit in read context. A chain that drops the
        // read yields no value — surface the error rather than serving
        // the unfiltered snapshot (matching the C `if(pLog)` no-frame
        // contract that `apply_to_event_value` also honors).
        if !self.channel_filters.is_empty() {
            snapshot.value = self
                .channel_filters
                .apply_to_read_value(snapshot.value)
                .ok_or_else(|| {
                    BridgeError::ChannelFilterError(format!(
                        "filter chain dropped the read value for {}",
                        self.pv_name
                    ))
                })?;
        }

        let full = snapshot_to_pv_structure(&snapshot, self.nt_type);
        Ok(pvif::filter_by_request(&full, request))
    }

    async fn put(&self, value: &PvStructure) -> BridgeResult<()> {
        // Backward-compat entry: parses options from the value
        // structure (the legacy location). New callers should
        // prefer [`BridgeChannel::put_with_options`] and pass options
        // extracted from the INIT pvRequest.
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
    /// create a monitor with an explicit value-subscription DBE
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
        .with_access(self.access.clone())
        // thread the channel's parsed filter chain (from the
        // pvxs `PV.VAL{...}` JSON suffix) into the monitor so its
        // subscription installs the filters at the dbChannel level.
        .with_filters(self.channel_filters.clone());
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

    fn req_with_process(value: PvField) -> PvStructure {
        let mut options = PvStructure::new("");
        options.fields.push(("process".into(), value));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));
        let mut req = PvStructure::new("request");
        req.fields
            .push(("record".into(), PvField::Structure(record)));
        req
    }

    /// pvxs reads `record._options.process` via `as<bool>` (bool,
    /// integer, or bool-parsable string) before the "passive" string
    /// (iocsource.cpp:426-448). Boolean and integer scalar forms must
    /// map to Force/Inhibit, not silently fall back to passive.
    #[test]
    fn put_options_process_accepts_boolean_and_integer_scalars() {
        let f = |v| PutOptions::from_pv_request(&req_with_process(PvField::Scalar(v))).process;

        assert_eq!(f(ScalarValue::Boolean(true)), ProcessMode::Force);
        assert_eq!(f(ScalarValue::Boolean(false)), ProcessMode::Inhibit);
        assert_eq!(f(ScalarValue::Int(1)), ProcessMode::Force);
        assert_eq!(f(ScalarValue::Int(0)), ProcessMode::Inhibit);
        assert_eq!(f(ScalarValue::Long(5)), ProcessMode::Force);
        // String forms still resolve; "passive" stays passive.
        assert_eq!(f(ScalarValue::String("true".into())), ProcessMode::Force);
        assert_eq!(f(ScalarValue::String("false".into())), ProcessMode::Inhibit);
        assert_eq!(
            f(ScalarValue::String("passive".into())),
            ProcessMode::Passive
        );
    }

    fn req_with_block(value: PvField) -> PvStructure {
        let mut options = PvStructure::new("");
        // A non-Inhibit process so the block flag is not cleared by the
        // `process == Inhibit ⇒ !block` rule — isolates the block parse.
        options.fields.push((
            "process".into(),
            PvField::Scalar(ScalarValue::Boolean(true)),
        ));
        options.fields.push(("block".into(), value));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));
        let mut req = PvStructure::new("request");
        req.fields
            .push(("record".into(), PvField::Structure(record)));
        req
    }

    /// pvxs reads `record._options.block` via `as<bool>`
    /// (singlesource.cpp:346-352), coercing bool, integer, unsigned,
    /// real, and the exact strings `"true"`/`"false"` (data.cpp:399-409,
    /// 459-462). The earlier parser accepted only a boolean scalar, so a
    /// client sending `block=1` or `block="true"` lost the put-notify
    /// completion barrier.
    #[test]
    fn put_options_block_accepts_integer_and_string_forms() {
        let f = |v| PutOptions::from_pv_request(&req_with_block(PvField::Scalar(v))).block;

        // boolean (already worked)
        assert!(f(ScalarValue::Boolean(true)));
        assert!(!f(ScalarValue::Boolean(false)));
        // integer 1 / 0
        assert!(f(ScalarValue::Int(1)));
        assert!(!f(ScalarValue::Int(0)));
        // unsigned + 64-bit integer
        assert!(f(ScalarValue::UInt(1)));
        assert!(f(ScalarValue::Long(5)));
        // real
        assert!(f(ScalarValue::Double(1.0)));
        assert!(!f(ScalarValue::Double(0.0)));
        // exact "true" / "false" strings
        assert!(f(ScalarValue::String("true".into())));
        assert!(!f(ScalarValue::String("false".into())));
        // unconvertible string keeps the default (false)
        assert!(!f(ScalarValue::String("yes".into())));
    }

    /// The `process == Inhibit ⇒ !block` rule (singlesource.cpp:350-352)
    /// must hold for every accepted `block` encoding, not only boolean.
    #[test]
    fn put_options_inhibit_disables_integer_block() {
        let mut options = PvStructure::new("");
        options.fields.push((
            "process".into(),
            PvField::Scalar(ScalarValue::String("false".into())),
        ));
        options
            .fields
            .push(("block".into(), PvField::Scalar(ScalarValue::Int(1))));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(options)));
        let mut req = PvStructure::new("request");
        req.fields
            .push(("record".into(), PvField::Structure(record)));

        let opts = PutOptions::from_pv_request(&req);
        assert_eq!(opts.process, ProcessMode::Inhibit);
        assert!(!opts.block, "block=1 must be cleared when process=false");
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

    /// pvxs-style flag string with `|`-separated tokens
    /// resolves to the corresponding EPICS event mask bits.
    #[test]
    fn dbe_mask_parses_value_alarm() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String("VALUE | ALARM".into())));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// String DBE substring-matches VALUE and ARCHIVE (the DBE_LOG bit),
    /// but a `PROPERTY` token must NOT enter the value mask — pvxs
    /// comments out `CASE(PROPERTY)` because the property subscription is
    /// separate (singlesource.cpp:126,161-167). `DBE_`-prefixed spellings
    /// still match by substring.
    #[test]
    fn dbe_string_archive_selected_property_excluded() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String(
            "DBE_VALUE,DBE_ARCHIVE,PROPERTY".into(),
        )));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(
            mask,
            (EventMask::VALUE | EventMask::LOG).bits(),
            "ARCHIVE selects the LOG bit; PROPERTY is excluded from the value mask"
        );
    }

    /// A `PROPERTY`-only string carries no value-class bit, so pvxs warns
    /// and falls back to VALUE|ALARM for the value subscription
    /// (singlesource.cpp:128-131,142-144). Before the fix the Rust value
    /// subscription became PROPERTY-only, so VALUE/ALARM posts stopped
    /// waking the monitor.
    #[test]
    fn dbe_string_property_only_falls_back_to_value_alarm() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String("PROPERTY".into())));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// The `LOG` spelling is NOT recognized by pvxs's string parser (only
    /// `ARCHIVE` selects the DBE_LOG bit); a lone `LOG` selects an empty
    /// value class and falls back to VALUE|ALARM, unlike the prior Rust
    /// behavior that treated `LOG` as the archive bit.
    #[test]
    fn dbe_string_log_spelling_not_recognized() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String("LOG".into())));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// numeric integer DBE within the value class passes through
    /// unchanged (`5` = VALUE|ALARM).
    #[test]
    fn dbe_mask_accepts_integer_form() {
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(5)));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, 5);
    }

    /// A numeric DBE selecting only PROPERTY (8) carries no
    /// value-class bit, so pvxs falls back to VALUE|ALARM for the value
    /// subscription (singlesource.cpp:142-144); PROPERTY is delivered
    /// by the separate property subscription.
    #[test]
    fn dbe_numeric_property_only_falls_back_to_value_alarm() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(
            EventMask::PROPERTY.bits() as i32,
        )));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// numeric DBE=0 still yields the pvxs value-class fallback, not
    /// an empty value subscription.
    #[test]
    fn dbe_numeric_zero_falls_back_to_value_alarm() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(0)));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// numeric DBE with an out-of-class PROPERTY bit alongside VALUE
    /// (9 = VALUE|PROPERTY) keeps only the value-class VALUE bit.
    #[test]
    fn dbe_numeric_strips_property_bit_from_value_mask() {
        use epics_base_rs::server::recgbl::EventMask;
        let raw = (EventMask::VALUE | EventMask::PROPERTY).bits();
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(raw as i32)));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, EventMask::VALUE.bits());
    }

    /// numeric-string DBE goes through the same value-class mask as
    /// the integer form (`"8"` = PROPERTY → VALUE|ALARM fallback).
    #[test]
    fn dbe_numeric_string_uses_value_class_mask() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String("8".into())));
        let mask = dbe_mask_from_pv_request(&req).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// missing DBE option resolves to None so the monitor
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

    /// boolean `record._options.atomic = true` resolves to
    /// `Some(true)` so the group operation overrides the default.
    #[test]
    fn atomic_option_parses_boolean_true() {
        let req = req_with_atomic(PvField::Scalar(ScalarValue::Boolean(true)));
        assert_eq!(atomic_from_pv_request(&req), Some(true));
    }

    /// string form (`"false"`) is also accepted, matching
    /// pvxs's lenient option parsing.
    #[test]
    fn atomic_option_parses_string_false() {
        let req = req_with_atomic(PvField::Scalar(ScalarValue::String("false".into())));
        assert_eq!(atomic_from_pv_request(&req), Some(false));
    }

    /// absent option resolves to None so callers fall back to
    /// the group default.
    #[test]
    fn atomic_option_absent_returns_none() {
        let req = PvStructure::new("request");
        assert!(atomic_from_pv_request(&req).is_none());
    }

    // ---- channel-filter accept/reject parity with dbChannelCreate ----

    async fn db_with_rec() -> Arc<PvDatabase> {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = Arc::new(PvDatabase::new());
        db.add_record("REC", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db
    }

    /// A documented JSON5 array filter (`filters.dbd.pod:415-419`,
    /// unquoted parameter keys) is accepted — the channel is created
    /// with the parsed filter chain rather than rejected.
    #[tokio::test]
    async fn channel_filter_accepts_documented_json5_arr() {
        let db = db_with_rec().await;
        let ch = BridgeChannel::new(db, r#"REC.{"arr":{s:2,i:2,e:8}}"#)
            .await
            .expect("documented JSON5 arr filter must create the channel");
        assert_eq!(ch.channel_filters.len(), 1);
    }

    /// An unknown filter name aborts channel creation, matching
    /// `dbChannel.c:176-182` `parse_stop` → `S_db_notFound`.
    #[tokio::test]
    async fn channel_filter_rejects_unknown_filter() {
        // `BridgeChannel` is not `Debug`, so match the `Result` rather
        // than `expect_err` (which would require `Ok: Debug`).
        let db = db_with_rec().await;
        let res = BridgeChannel::new(db, r#"REC.{"no_such":{}}"#).await;
        assert!(matches!(res, Err(BridgeError::ChannelFilterError(_))));
    }

    /// Malformed JSON aborts channel creation rather than failing open
    /// to an unfiltered monitor (`dbChannel.c:512-529`).
    #[tokio::test]
    async fn channel_filter_rejects_malformed_json() {
        let db = db_with_rec().await;
        let res = BridgeChannel::new(db, r#"REC.{not json}"#).await;
        assert!(matches!(res, Err(BridgeError::ChannelFilterError(_))));
    }

    /// An invalid `dec` body (missing required `n`) is a hard reject,
    /// matching `chf_value` / `parse_end` failure → `parse_stop`.
    #[tokio::test]
    async fn channel_filter_rejects_invalid_dec_body() {
        let db = db_with_rec().await;
        let res = BridgeChannel::new(db, r#"REC.{"dec":{}}"#).await;
        assert!(matches!(res, Err(BridgeError::ChannelFilterError(_))));
    }

    /// A plain (unfiltered) channel still creates with an empty chain —
    /// the reject path must not regress the no-suffix common case.
    #[tokio::test]
    async fn channel_without_filter_suffix_still_creates() {
        let db = db_with_rec().await;
        let ch = BridgeChannel::new(db, "REC")
            .await
            .expect("unfiltered channel must create");
        assert!(ch.channel_filters.is_empty());
    }
}
