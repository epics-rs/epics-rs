//! BridgeChannel: single-record PVA channel.
//!
//! Corresponds to C++ QSRV's `PDBSinglePV` / `PDBSingleChannel`.

// RTEMS-EXEC-MODEL-ALLOW(9): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_pva_rs::pvdata::{FieldDesc, NoConvert, PvField, PvStructure, ScalarValue, convert};
use epics_pva_rs::server_native::source::RemoteLog;

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
/// [`epics_base_rs::server::recgbl::EventMask`] bits (VALUE=1, ARCHIVE/LOG=2, ALARM=4), so the raw
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
/// value-subscription mask. `Ok(None)` only when the option is
/// absent; a present option always resolves to a non-empty value-class
/// mask (pvxs's `VALUE|ALARM` fallback applies when nothing in the
/// value class is selected).
///
/// pvxs dispatches DBE on the field's KIND — `fld.type().kind()`
/// (singlesource.cpp:118), the class nibble of the type code (`code & 0xe0`,
/// data.h:140-147) — and then CONVERTS. Exactly two arms read a value:
///
/// * `Kind::String` → `fld.as<std::string>()`, then the sloppy substring scan
///   (`:119-133`);
/// * `Kind::Integer` / `Kind::Real` → `dbe = fld.as<uint8_t>()` (`:134-137`).
///
/// Every other kind — `Kind::Bool` and `Kind::Compound` (struct / union / any)
/// included — hits `default: break` (`:138-139`) with `dbe` still 0, so the
/// value class selects nothing and the `DBE_VALUE | DBE_ALARM` fallback applies
/// (`:141-144`). `as<uint8_t>()` *could* convert bool storage
/// (data.cpp:428-435); pvxs simply never calls it for a bool.
///
/// Both value-reading arms use the THROWING `as<T>()`, and KIND IS NOT STORAGE:
/// `Int32A` is `Kind::Integer` but stores as an array, and `Value::copyOut` has
/// no scalar arm for array storage (`data.cpp:466-499`). So an ARRAY-typed
/// `DBE` of integer, real, or string element kind reaches the conversion and
/// raises `NoConvert` — `Err` here, which the caller
/// ([`ChannelSource::check_monitor_request`](epics_pva_rs::server_native::source::ChannelSource::check_monitor_request))
/// turns into an op-level error reply. pvxs instead lets the throw reset the
/// whole TCP circuit; that is CBUG-C2 and the port deliberately does not
/// reproduce it. A BOOLEAN array does not throw: `Kind::Bool` never reaches a
/// conversion. (R9-35 — the port used to serve VALUE|ALARM for every array.)
///
/// String form mirrors pvxs's "sloppy" substring parse
/// (singlesource.cpp:122-127): only `VALUE`, `ARCHIVE`, and `ALARM` are
/// recognized for the value mask. `LOG` is not a recognized spelling,
/// and `PROPERTY` is deliberately excluded — the property subscription
/// is separate and unconditional (singlesource.cpp:161-167).
///
/// `log` is the operation's [`RemoteLog`]: a string DBE that selects
/// NOTHING in the value class still falls back to `VALUE|ALARM`, but pvxs
/// tells the client so first (singlesource.cpp:128-130) rather than
/// letting the fallback pass for an honored request.
pub fn dbe_mask_from_pv_request(
    request: &PvStructure,
    log: &RemoteLog,
) -> Result<Option<u16>, NoConvert> {
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
        });
    let Some(options) = options else {
        return Ok(None);
    };
    let Some(dbe) = options.get_field("DBE") else {
        return Ok(None);
    };
    // pvxs `switch(fld.type().kind())`, through the shared kind owner
    // (`convert::kind` IS `Value::type().kind()`). Every arm converges on the
    // single value-class mask + `VALUE|ALARM` fallback below
    // (singlesource.cpp:141-144), so PROPERTY / out-of-class bits are stripped
    // in one place and cannot leak into the value subscription.
    let raw = match convert::kind(dbe) {
        // `fld.as<std::string>()` — throws on a string ARRAY.
        convert::Kind::String => convert::as_string(dbe)?,
        // `dbe = fld.as<uint8_t>()` — throws on an integer / real ARRAY. The
        // narrowing to `u8` is C's `(uint8_t)` truncation in `copyOutScalar`
        // (data.cpp:402-416); DBE bits live in the low nibble, so the high bits
        // pvxs discards are irrelevant.
        convert::Kind::Integer | convert::Kind::Real => {
            return Ok(Some(dbe_value_class_mask(u16::from(convert::as_u8(dbe)?))));
        }
        // pvxs's `default: break` — Bool and Compound select no DBE bit and
        // never reach a conversion, so they cannot throw. This is NOT `None`
        // (which the caller reads as "the option is absent"): the option is
        // present, it just selects nothing.
        convert::Kind::Bool | convert::Kind::Compound | convert::Kind::Null => {
            return Ok(Some(dbe_value_class_mask(0)));
        }
    };

    // A String-typed DBE is NEVER parsed numerically. pvxs switches on the
    // field's *kind* (singlesource.cpp:117-140): `Kind::String` runs the
    // substring scan below and nothing else, while only `Kind::Integer` /
    // `Kind::Real` reach `fld.as<uint8_t>()`. So `DBE="1"` selects no bit,
    // draws the empty-mask warning, and falls back to VALUE|ALARM — it does
    // NOT mean DBE_VALUE. (This is unlike `queueSize`/`block`/`atomic`, which
    // pvxs reads with `as<T>()` regardless of kind; `Value::as` does
    // `parseTo<int64_t>` on string storage, data.cpp:442-449, so their
    // numeric-string parse is correct.) The port used to parse the string
    // first, so `DBE="1"` selected VALUE-only where pvxs gives VALUE|ALARM,
    // and the warning never fired.

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
    // Case-SENSITIVE substring search on the original string, exactly like
    // pvxs `mask.find("VALUE"/"ARCHIVE"/"ALARM")` (singlesource.cpp:122-125).
    // pvxs does NOT case-fold the option, so a lowercase token such as
    // `"alarm"` or `"value"` matches nothing, selects an empty value mask,
    // and falls through to the `VALUE|ALARM` fallback below — whereas an
    // uppercase substring (including a prefixed spelling like `"DBE_VALUE"`)
    // selects its bit. Folding to uppercase first (as Rust previously did)
    // made lowercase tokens select a narrower mask than pvxs, hiding value
    // changes from clients that used lowercase option strings.
    let mut raw_mask = 0u16;
    if raw.contains("VALUE") {
        raw_mask |= EventMask::VALUE.bits();
    }
    if raw.contains("ARCHIVE") {
        raw_mask |= EventMask::LOG.bits();
    }
    if raw.contains("ALARM") {
        raw_mask |= EventMask::ALARM.bits();
    }
    // pvxs `singlesource.cpp:128-130` — `if(!dbe && !mask.empty())`. The
    // client named an event class the substring parse recognized nothing in
    // (`"LOG"`, a lowercase `"value"`, `"PROPERTY"` alone), so the request
    // is honored by the `VALUE|ALARM` fallback below rather than by what was
    // asked for. pvxs reports that before falling back; an empty string
    // (`DBE=""`) is not a selection at all and draws no warning.
    if raw_mask == 0 && !raw.is_empty() {
        log.warn(format!("record._options.DBE=\"{raw}\" selects empty mask"));
    }
    Ok(Some(dbe_value_class_mask(raw_mask)))
}

/// parse `record._options.atomic` from a group operation
/// pvRequest. Returns `Some(true|false)` when the option is set and
/// convertible, `None` when absent or unconvertible — the caller then
/// falls back to the group's default atomicity.
///
/// pvxs resolves group atomicity with `pvRequest["record._options.atomic"]
/// .as(atomic)` on both PUT and GET (`groupsource.cpp:203,480`), i.e.
/// `Value::as(bool&)`. Route through the shared
/// [`epics_pva_rs::pvdata::convert::as_bool`] owner so the coercion is
/// identical to `record._options.block` / `process` — and to the native
/// server's `record._options.pipeline`: a bool, any signed/unsigned integer
/// or real scalar maps by nonzero truthiness, and a string is accepted only
/// as the exact tokens `"true"` / `"false"`. Numeric `0` / `1` / `UInt(1)` /
/// `Double(1.0)` therefore override the group default instead of being
/// silently ignored.
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

    convert::as_bool(options.get_field("atomic")?).ok()
}

/// Map a present `record._options.process` field to a [`ProcessMode`],
/// mirroring pvxs `setForceProcessingFlag` (ioc/iocsource.cpp:426-448)
/// including which values it reports to the client:
///
/// ```text
///   proc.as<bool>() succeeds  -> forceProc = True | False   (silent)
///   else "passive"            -> forceProc = Unset          (silent)
///   else                      -> forceProc left Unset, logRemote(Warn)
/// ```
///
/// `True`/`False`/`Unset` map to Force/Inhibit/Passive here. The `as<bool>`
/// coercion is the *same* one `record._options.atomic`/`block` use, so it
/// routes through the shared [`epics_pva_rs::pvdata::convert::as_bool`] owner
/// rather than being re-derived: a bool, any signed/unsigned integer or real
/// scalar maps by nonzero truthiness (`copyOutScalar` `bool(src)`,
/// src/data.cpp:402-408), and a string is accepted only as the exact tokens
/// `"true"`/`"false"` — no trim, case sensitive (src/data.cpp:459-461).
///
/// The third arm is what this function exists for: `"passive"` is a
/// SUPPORTED spelling of the default and is silent, while a
/// whitespace-wrapped `" false "`, a typo, or a non-scalar field is an
/// UNSUPPORTED value — same passive outcome, but pvxs names it to the
/// client. Collapsing both into a silent passive (as this did) loses that
/// distinction, which is the only thing the client can act on: the PUT it
/// asked to force will silently not process.
fn process_mode_from_field(field: &PvField, log: &RemoteLog) -> ProcessMode {
    match convert::as_bool(field) {
        Ok(true) => return ProcessMode::Force,
        Ok(false) => return ProcessMode::Inhibit,
        // NoConvert — pvxs falls through to its `proc.as(s)` check.
        Err(_) => {
            if matches!(field, PvField::Scalar(ScalarValue::String(s)) if s.as_str_lossy() == "passive")
            {
                return ProcessMode::Passive;
            }
        }
    }
    // pvxs iocsource.cpp:446-447 — "oops, unsupported type or unexpected
    // value". `forceProc` keeps its incoming default (Unset ⇒ Passive) and
    // the client is told which option was ignored, and with what value.
    log.warn(format!(
        "Ignoring unsupported record._options.process: {}",
        render_option_value(field)
    ));
    ProcessMode::Passive
}

/// Render a pvRequest option value the way pvxs's `SB()<<value` does —
/// [`epics_pva_rs::pvdata::render_value`], the single owner of that
/// rendering, shared with the native PVA server's monitor-option
/// diagnostics.
///
/// This used to be a second, divergent copy: it invented the pvData type
/// spellings (`int32`, `float64`) where pvxs's `TypeCode::name()` prints the
/// C-ish `int32_t` / `double` (`src/type.cpp:126-166`), and it collapsed every
/// non-scalar to `<non-scalar>` (R10-36).
use epics_pva_rs::pvdata::render_value as render_option_value;

impl PutOptions {
    /// Extract process/block options from a PvStructure.
    ///
    /// Looks for `record._options.process` (bool / integer / "true" /
    /// "false" / "passive") and `record._options.block` (bool / integer /
    /// unsigned / real / "true" / "false", via pvxs `as<bool>` coercion).
    ///
    /// `log` is the operation's [`RemoteLog`]: a `process` value pvxs
    /// cannot interpret is reported to the client rather than silently
    /// defaulted (see `process_mode_from_field`).
    pub fn from_pv_request(request: &PvStructure, log: &RemoteLog) -> Self {
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
            // literal string "passive"; anything else keeps the passive
            // default AND is reported to the client
            // (ioc/iocsource.cpp:426-448). An ABSENT option is not a value
            // at all: pvxs returns before the log, so the field must be
            // matched on presence, not on being a scalar — a non-scalar
            // `process` is a present-but-unsupported value and draws the
            // same warning.
            if let Some(field) = opt_struct.get_field("process") {
                opts.process = process_mode_from_field(field, log);
            }

            // block option. pvxs reads `record._options.block` via
            // `Value::as<bool>` (ioc/singlesource.cpp:346-352), which
            // coerces bool / integer / unsigned / real / `"true"` /
            // `"false"` through `copyOutScalar`. The earlier path matched
            // only `Boolean`, silently dropping the integer and string
            // forms a PVA client can legally send — so a `block=1` or
            // `block="true"` lost the put-notify completion barrier.
            if let Some(field) = opt_struct.get_field("block")
                && let Ok(b) = convert::as_bool(field)
            {
                opts.block = b;
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

/// Resolve a channel's record path (the name with any `{json}` / `[range]`
/// filter suffix already peeled by `split_channel_name`) into the record it
/// names, its uppercased field, and whether the `$` long-string modifier was
/// requested. The single owner of the `REC.FIELD` → field rule — a bare `REC`
/// binds to `VAL` (`parse_pv_name`), and the `$` modifier is peeled before the
/// split (C `dbChannel.c:486-505`).
pub(super) fn resolve_record_field(record_path: &str) -> (&str, String, bool) {
    let (core, string_view) = match record_path.strip_suffix('$') {
        Some(core) => (core, true),
        None => (record_path, false),
    };
    let (record_name, field) = epics_base_rs::server::database::parse_pv_name(core);
    (record_name, field.to_ascii_uppercase(), string_view)
}

/// `dbChannelCreate(name)` admission test (C `dbChannel.c:440-530`) for a
/// QSRV channel name: does `name` name a field of a record in `db`, and — if
/// it carries the `$` modifier — is that field `$`-eligible?
///
/// pvxs runs exactly this test whenever it binds a channel: `Channel::Channel`
/// throws `Invalid PV: <name>` when `dbChannelCreate` returns NULL
/// (`ioc/channel.cpp:29-38`). The single-channel path already performs it
/// implicitly, by resolving the field before it can build a `BridgeChannel`;
/// the group path had no such gate, so this is the shared owner both use to
/// answer "would `dbChannelCreate` succeed?" without constructing anything.
///
/// Returns the pvxs error text on refusal so the caller can reproduce
/// `createGroups`' `"%s: Error Group not created: %s\n"` line verbatim.
///
/// DEVIATION (Tier 2), stated where it is made: the group value/put/monitor
/// paths bind `member.channel` field-by-name and have no `$` char-array view,
/// so a `$` member this port admitted would connect and then answer
/// `FieldNotFound` to every operation — the very state pvxs's refusal exists
/// to prevent. Until the group path grows the view, a `$` `+channel` is
/// refused with a named reason rather than half-served. pvxs would create the
/// `DBF_STRING` case (it serves the raw `int8_t[]`); it refuses the ineligible
/// case exactly as we do, with the same `Invalid PV:` text.
pub(super) async fn resolve_db_channel(db: &PvDatabase, name: &str) -> Result<(), String> {
    let parsed = epics_base_rs::server::database::filters::split_channel_name(name);
    let (record_name, field, string_view) = resolve_record_field(&parsed.record_path);
    let Some(rec) = db.get_record(record_name) else {
        return Err(format!("Invalid PV: {name}"));
    };
    let instance = rec.read();
    if string_view {
        // `$` is `S_dbLib_fieldNotFound` on anything but a `DBF_STRING` or
        // link field (`dbChannel.c:486-505`), which aborts channel creation.
        // `resolve_string_view_field` is the base's owner of that eligibility
        // rule — the same one `BridgeChannel::new` consults — so a `$` on a
        // `DBF_CHAR` waveform is refused here (verbatim pvxs text) instead of
        // reaching the introspection builder as an unresolvable field it
        // renders as a fabricated `double` leaf.
        return if instance.resolve_string_view_field(&field).is_none() {
            Err(format!("Invalid PV: {name}"))
        } else {
            Err(format!(
                "long-string '$' channel is not supported in a group: {name}"
            ))
        };
    }
    if instance.resolve_field(&field).is_some() {
        Ok(())
    } else {
        Err(format!("Invalid PV: {name}"))
    }
}

/// `dbIsValueField(dbChannelFldDes(chan))` for a QSRV channel name (single
/// channel) or a group member's `+channel` — the fact QSRV's
/// `IOCSource::initialize` gates `display.form.index` on
/// (`iocsource.cpp:53`). Peels the filter suffix and `$` modifier first, so
/// `REC.VAL{"dbnd":…}` and `REC` are VAL while `REC.RVAL` is not.
pub(super) fn channel_is_value_field(name: &str) -> bool {
    let parsed = epics_base_rs::server::database::filters::split_channel_name(name);
    let (_, field, _) = resolve_record_field(&parsed.record_path);
    epics_base_rs::server::database::is_value_field(&field)
}

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
    /// [`FilterChain::apply_to_read_value`](epics_base_rs::server::database::filters::FilterChain::apply_to_read_value). PUT writes the raw value
    /// (filters are read-side only).
    channel_filters: std::sync::Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// An INDEPENDENT re-parse of the same channel-filter suffix, for the
    /// monitor's PROPERTY subscription. pvxs builds `pPropertiesChannel`
    /// from `dbChannelName(sInfo->chan)` — the same filtered channel name
    /// (`singlesrcsubscriptionctx.cpp:24`) — so both the value and property
    /// dbChannels carry the client's filter, each with its own state
    /// (`dbChannelCreate` re-parses the suffix per channel,
    /// `dbChannel.c:471`). Held separately from `channel_filters` so a
    /// stateful filter (`dbnd` last-sent, `dec` counter) on the value
    /// subscription never shares state with the property subscription.
    property_filters: std::sync::Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// Access control context — checked on every get/put.
    access: super::provider::AccessContext,
}

impl BridgeChannel {
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
            property_filters: std::sync::Arc::new(
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
        // Parse the suffix into TWO independent chains: one for the value
        // subscription / GET read path (`channel_filters`) and one for the
        // monitor's PROPERTY subscription (`property_filters`). pvxs builds
        // the property dbChannel from the same filtered channel name
        // (`singlesrcsubscriptionctx.cpp:24`), and `dbChannelCreate`
        // re-parses the suffix per channel (`dbChannel.c:471`), so each
        // channel owns independent filter state — a stateful `dbnd`/`dec`
        // on the value stream must not have its baseline/counter perturbed
        // by a DBE_PROPERTY event on the property stream.
        let (channel_filters, property_filters) = match parsed.json_suffix.as_deref() {
            Some(json) => {
                let value = epics_base_rs::server::database::filters::try_parse_filter_chain(json)
                    .map_err(|e| BridgeError::ChannelFilterError(e.to_string()))?;
                let property =
                    epics_base_rs::server::database::filters::try_parse_filter_chain(json)
                        .map_err(|e| BridgeError::ChannelFilterError(e.to_string()))?;
                (std::sync::Arc::new(value), std::sync::Arc::new(property))
            }
            None => (
                std::sync::Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
                std::sync::Arc::new(epics_base_rs::server::database::filters::FilterChain::new()),
            ),
        };
        // EPICS `$` long-string field modifier (C `dbChannel.c:486-505`):
        // a trailing `$` re-views a `DBF_STRING` or link field as a
        // `DBR_CHAR` character array, which pvxs serves as the
        // `form = "String"` long-string `NTScalar`
        // (`ioc/iocsource.cpp:133-136`, `ioc/channel.cpp:62-74`). The `$`
        // is innermost in the channel name (`REC.FIELD$[range]{json}`) and
        // `split_channel_name` has already peeled `{json}` and `[range]`,
        // so the modifier is the final character of the record path. It is
        // left there (the CA server detects it on the record path too,
        // `epics-ca-rs` `tcp.rs`) and peeled here for field resolution.
        let (record_name, field_upper, string_view) = resolve_record_field(&parsed.record_path);

        let rec = db
            .get_record(record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read();
        // Resolve the bound field's actual value once (record field →
        // common field → virtual field). This is the single source of
        // truth for both the served DBF type and (below) the NT shape,
        // so the advertised descriptor cannot drift from the value the
        // GET path will serialize.
        let (resolved, nt_type) = if string_view {
            // The `$` modifier is valid only on a `DBF_STRING` or link
            // field; every other field type is `S_dbLib_fieldNotFound` and
            // aborts channel creation, matching `dbChannelCreate`
            // (`dbChannel.c:500-503`). An eligible field is served as the
            // long-string string view — the same `form = "String"`
            // `NTScalar` as an `lsi`/`lso`/`printf` long-string field —
            // because pvxs collapses the `DBR_CHAR` `$` view to a
            // NUL-terminated `pvString` (`ioc/iocsource.cpp:133-136`), so
            // the `$` view reuses the existing string-view path rather than
            // a parallel byte array that would diverge from the wire.
            let v = instance
                .resolve_string_view_field(&field_upper)
                .ok_or_else(|| BridgeError::FieldNotFound {
                    record: record_name.to_string(),
                    field: format!("{field_upper}$"),
                })?;
            (Some(v), NtType::LongString)
        } else {
            // `client_field_value`, not `resolve_field`: the value projected
            // onto the field's DECLARED type, which is what every delivery
            // path serves. Reading the DBF off the raw stored variant is what
            // advertised `.PROC` (`DBF_UCHAR`) as a signed byte and a
            // `DBF_MENU` field as a short.
            let resolved = instance.client_field_value(&field_upper);
            // `pvif::nt_type_for_channel` is the single owner of the NT
            // choice (the port's `getChannelValueType`): a record-declared
            // long-string field (`lsi`/`lso` VAL/OVAL, `printf` VAL) AND a
            // `DBF_CHAR` array VAL carrying `info(Q:form, "String")` — the
            // QSRV long-string idiom — both resolve to the scalar-string
            // NTScalar, not the byte array the `DBF_CHAR` type alone would
            // select.
            let nt_type = pvif::nt_type_for_channel(&instance, &field_upper, resolved.as_ref());
            (resolved, nt_type)
        };

        // DBF type for the bound field. pvxs serves the type from
        // `dbChannelFinalFieldType(chan)` (singlesource.cpp:189-205,
        // dbChannel.h:452) — the channel's final field type after lookup,
        // which covers `dbCommon` fields, not only record-specific ones.
        //
        // `resolved` above is `client_field_value`, i.e. already projected onto
        // the field's declared type, so reading the DBF off it IS reading the
        // declaration — and the advertised descriptor agrees with the value the
        // GET path serializes by construction, because both are that one
        // projection. Taking it from the RAW stored variant used to advertise
        // `.PROC` (`DBF_UCHAR`) as a signed byte and every `DBF_MENU` field as
        // a short: agreement with the value, but agreement on the wrong type.
        //
        // `Double` remains the backstop for a field that resolves to no value
        // at all.
        let value_dbf = resolved
            .as_ref()
            .map(|v| v.db_field_type())
            .unwrap_or(DbFieldType::Double);

        Ok(Self {
            db,
            pv_name: name.to_string(),
            record_name: record_name.to_string(),
            field: field_upper,
            nt_type,
            value_dbf,
            channel_filters,
            property_filters,
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
        // C `IOCSource::doPreProcessing` (iocsource.cpp:363-375), which
        // pvxs runs on every QSRV put in every process mode
        // (singlesource.cpp:354-356) BEFORE the write-ACF check: reject a
        // put to a DISP-disabled record (except the DISP field) or a
        // read-only field. The `Passive` route enforces this inside
        // `put_record_field_from_ca`, but the `Force`/`Inhibit` routes go
        // through `put_pv` (the internal `dbPut` analogue, which by design
        // does not gate DISP), so the gate must run at this boundary for
        // all three process modes. `put_status` owns the wire text.
        super::put_status::check_preconditions(&self.db, &self.record_name, &self.field).await?;

        // One access evaluation yields both the allow/deny decision and
        // the matched rule's TRAPWRITE flag (`WriteGrant`). The grant is
        // the single source of "is this a trapped write" — the PUT below
        // routes through it and never re-derives the trap flag.
        let grant = self.access.write_grant(&self.pv_name).await;
        if !grant.allowed {
            // pvxs `doFieldPreProcessing` (iocsource.cpp:385) — the wire
            // carries "Put not permitted"; identity goes to the log.
            return Err(super::put_status::put_not_permitted(&format!(
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
            // Long-string channel: the QSRV value is a scalar string
            // (`value.as<std::string>()` in pvxs, which renders a non-string
            // scalar to its textual form). Never retype it to the bound
            // `DBF_CHAR` storage — that would parse the whole string as one
            // integer and reject the PUT.
            let text = pvif::long_string_value(&raw_val);
            if self.value_dbf == DbFieldType::Char {
                // The field's storage IS a `DBR_CHAR` array, so this is
                // pvxs's `putLongString`: `dbPut(DBR_CHAR, str, strlen+1)`,
                // i.e. the bytes plus the NUL. Writing the char image (not
                // an `EpicsValue::String`) is what puts the record on C's
                // long-string put path — bounded by SIZV / NELM, with
                // `LEN`/`NORD` = strlen+1 — rather than the DBR_STRING path,
                // which is capped at MAX_STRING_SIZE.
                pvif::long_string_put_image(&text)
            } else {
                // String-backed storage (a `$` view of a DBF_STRING or link
                // field): the record's own String put is the equivalent of
                // C's re-viewed put.
                EpicsValue::String(text)
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
        // full record-processing cycle afterwards. pvxs's
        // `doPostProcessing(forceProcessing==True)` calls
        // `dbProcess(precord)` (iocsource.cpp:397-417), the full
        // record-processing entry that runs record support, OUT
        // links, and FLNK side effects — not a value-only local
        // notification. The Rust analogue is `process_record_with_links`
        // (INP/OUT/FLNK traversal with cycle/depth tracking), the same
        // owner the CA-style passive PUT (`put_record_field_from_ca`)
        // re-enters; the bare `process_record` (process_local + notify)
        // would reply success after only the local record body ran,
        // skipping the link chain.
        // Bracket the backing write with the EPICS `asTrapWrite`
        // put-logging hook (pvxs wraps every QSRV put in a
        // `SecurityLogger`, singlesource.cpp:354-360). The `grant`
        // gates emission; a non-trapped put runs the write unbracketed.
        // `dbr_type` is the channel's final field type
        // (`dbChannelFinalCAType`); `value_str`/`no_elements` are
        // rendered from the value inside the helper.
        let meta = super::trap_write::TrapWriteMeta {
            pv_name: &self.pv_name,
            user: &self.access.user,
            host: &self.access.host,
            peer: &self.access.host,
            dbr_type: self.value_dbf as u16,
        };
        super::trap_write::put_with_trap(grant, meta, epics_val, |value| async move {
            match opts.process {
                ProcessMode::Inhibit => {
                    self.db
                        .put_pv(&format!("{}.{}", self.record_name, self.field), value)
                        .await
                        .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                }
                ProcessMode::Passive => {
                    if opts.block {
                        let notify_rx = self
                            .db
                            .put_record_field_from_ca(&self.record_name, &self.field, value)
                            .await
                            .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                        if let epics_base_rs::server::record::ProcessCompletion::Async(rx) =
                            notify_rx
                        {
                            let _ = rx.await;
                        }
                    } else {
                        // Non-blocking PVA put — pvxs completes it
                        // without `dbNotify` state (C parity:
                        // `dbPutField`, not `dbPutNotify`). Parking a
                        // wait-set here and dropping its receiver would
                        // occupy the record's notify slot until any
                        // async processing settles, failing legitimate
                        // blocking puts in the meantime.
                        self.db
                            .put_record_field_from_ca_no_notify(
                                &self.record_name,
                                &self.field,
                                value,
                            )
                            .await
                            .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                    }
                }
                ProcessMode::Force => {
                    self.db
                        .put_pv(&format!("{}.{}", self.record_name, self.field), value)
                        .await
                        .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                    if opts.block {
                        // pvxs honors `block` for Force exactly as for Passive:
                        // a `record[process=true,block=true]` put routes through
                        // `dbProcessNotify` and the reply is withheld until
                        // processing — including async device completion (a
                        // motor move, an asyn-backed AO) — finishes
                        // (`singlesource.cpp:360-369`; `if forceProcessing==False
                        // doWait=false` clears the wait for Inhibit only, never
                        // for Force). The bare `process_record_with_links` below
                        // returns success as soon as the record goes PACT, so a
                        // blocking forced put to an async record must instead
                        // await the put-notify completion.
                        let notify_rx = self
                            .db
                            .process_record_with_notify(&self.record_name)
                            .await
                            .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                        if let epics_base_rs::server::record::ProcessCompletion::Async(rx) =
                            notify_rx
                        {
                            let _ = rx.await;
                        }
                    } else {
                        // The non-blocking forced put is pvxs's
                        // `doPostProcessing(forceProcessing==True)`
                        // (`singlesource.cpp:382`) — the SAME owner the group
                        // PUT reaches (`groupsource.cpp:570`), and it splits on
                        // PACT: an async-active record takes `rpro = TRUE` and
                        // is not processed, an idle one takes `putf = TRUE` and
                        // processes (`iocsource.cpp:404-419`). Calling
                        // `process_record_with_links` here set neither flag and
                        // landed in the port's `dbProcess` PACT guard — the
                        // LCNT bump and SCAN_ALARM/INVALID after MAX_LOCK that
                        // `doPostProcessing` exists to avoid — while dropping
                        // the deferred reprocess. `put_driven_process` is the
                        // database's single owner of that transition.
                        self.db
                            .put_driven_process(&self.record_name)
                            .await
                            .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                    }
                }
            }
            Ok(())
        })
        .await
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
        if !self.access.can_read(&self.pv_name).await {
            return Err(BridgeError::PutRejected(format!(
                "read denied for {} (user='{}' host='{}')",
                self.pv_name, self.access.user, self.access.host
            )));
        }

        let rec = self
            .db
            .get_record(&self.record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.clone()))?;

        let instance = rec.read();
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
        let opts = PutOptions::from_pv_request(value, &RemoteLog::default());
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
        if !self.access.can_read(&self.pv_name).await {
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
        // subscription installs the filters at the dbChannel level. The
        // value stream and the PROPERTY stream each get an independent
        // re-parse of the same suffix (pvxs builds both dbChannels from
        // the same filtered name, with per-channel filter state).
        .with_filters(self.channel_filters.clone())
        .with_property_filters(self.property_filters.clone());
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
        let opts = PutOptions::from_pv_request(&req, &RemoteLog::default());
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

        let opts = PutOptions::from_pv_request(&req, &RemoteLog::default());
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

        let opts = PutOptions::from_pv_request(&req, &RemoteLog::default());
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
        let f = |v| {
            PutOptions::from_pv_request(
                &req_with_process(PvField::Scalar(v)),
                &RemoteLog::default(),
            )
            .process
        };

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

    /// pvxs reads `record._options.process` through the SAME `proc.as<bool>`
    /// coercion as `atomic`/`block` (iocsource.cpp:436 → copyOutScalar /
    /// string store). Real scalars therefore coerce by nonzero truthiness
    /// (`bool(src)`, data.cpp:402-408), and the string store is exact:
    /// `" true "`/`" false "` are NoConvert and stay passive (data.cpp:
    /// 459-461). The earlier parser forced real scalars to passive and
    /// `.trim()`-ed strings, so `Double(1.0)` was dropped to passive and a
    /// whitespace-wrapped `" false "` wrongly inhibited processing.
    #[test]
    fn put_options_process_real_truthiness_and_no_trim() {
        let f = |v| {
            PutOptions::from_pv_request(
                &req_with_process(PvField::Scalar(v)),
                &RemoteLog::default(),
            )
            .process
        };

        // real scalars coerce by nonzero truthiness, not silently passive
        assert_eq!(f(ScalarValue::Double(1.0)), ProcessMode::Force);
        assert_eq!(f(ScalarValue::Double(0.0)), ProcessMode::Inhibit);
        assert_eq!(f(ScalarValue::Float(2.5)), ProcessMode::Force);
        assert_eq!(f(ScalarValue::Float(0.0)), ProcessMode::Inhibit);
        // unsigned integer truthiness
        assert_eq!(f(ScalarValue::UInt(1)), ProcessMode::Force);
        // strings are matched exactly: surrounding whitespace is NoConvert,
        // so the mode stays passive (pvxs never trims before the
        // "true"/"false" store) instead of the trimmed parser's
        // Force/Inhibit.
        assert_eq!(
            f(ScalarValue::String(" true ".into())),
            ProcessMode::Passive
        );
        assert_eq!(
            f(ScalarValue::String(" false ".into())),
            ProcessMode::Passive
        );
        // case-sensitive, like pvxs's exact string store
        assert_eq!(f(ScalarValue::String("TRUE".into())), ProcessMode::Passive);
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
        let f = |v| {
            PutOptions::from_pv_request(&req_with_block(PvField::Scalar(v)), &RemoteLog::default())
                .block
        };

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

        let opts = PutOptions::from_pv_request(&req, &RemoteLog::default());
        assert_eq!(opts.process, ProcessMode::Inhibit);
        assert!(!opts.block, "block=1 must be cleared when process=false");
    }

    /// Unwrap the DBE mask, asserting the option CONVERTED. An `Err` here is
    /// pvxs throwing out of `onSubscribe` and resetting the circuit — see
    /// [`dbe_array_typed_throws_and_resets_the_circuit`].
    fn dbe_mask(req: &PvStructure, log: &RemoteLog) -> Option<u16> {
        dbe_mask_from_pv_request(req, log)
            .expect("DBE must convert; an Err would reset the circuit in pvxs")
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
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
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
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
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
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
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
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// pvxs's string DBE parse is
    /// case-SENSITIVE (`mask.find("VALUE"/"ARCHIVE"/"ALARM")`,
    /// singlesource.cpp:122-125), so a lowercase token selects an empty value
    /// mask and falls back to `VALUE|ALARM`. Before the fix Rust uppercased
    /// the option first, so `"alarm"` subscribed alarm-only and `"archive"`
    /// archive/log-only — narrower than pvxs and hiding value changes.
    #[test]
    fn dbe_string_lowercase_tokens_fall_back_to_value_alarm() {
        use epics_base_rs::server::recgbl::EventMask;
        let fallback = (EventMask::VALUE | EventMask::ALARM).bits();
        for token in ["alarm", "value", "archive", "value | alarm", "dbe_value"] {
            let req = req_with_dbe(PvField::Scalar(ScalarValue::String(token.into())));
            let mask = dbe_mask(&req, &RemoteLog::default()).expect("present option must parse");
            assert_eq!(
                mask, fallback,
                "lowercase DBE token {token:?} matches no uppercase substring, so pvxs falls back to VALUE|ALARM"
            );
        }
    }

    /// An uppercase substring inside
    /// mixed-case text still selects its bit, because pvxs searches for the
    /// exact uppercase substring rather than tokenizing. `"alarmALARM"`
    /// therefore selects ALARM (the embedded uppercase `ALARM`), proving the
    /// match is a case-sensitive substring search, not a case fold.
    #[test]
    fn dbe_string_uppercase_substring_in_mixed_case_still_selects() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::String("alarmALARM".into())));
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("present option must parse");
        assert_eq!(mask, EventMask::ALARM.bits());
    }

    /// numeric integer DBE within the value class passes through
    /// unchanged (`5` = VALUE|ALARM).
    #[test]
    fn dbe_mask_accepts_integer_form() {
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(5)));
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
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
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// numeric DBE=0 still yields the pvxs value-class fallback, not
    /// an empty value subscription.
    #[test]
    fn dbe_numeric_zero_falls_back_to_value_alarm() {
        use epics_base_rs::server::recgbl::EventMask;
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(0)));
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
        assert_eq!(mask, (EventMask::VALUE | EventMask::ALARM).bits());
    }

    /// numeric DBE with an out-of-class PROPERTY bit alongside VALUE
    /// (9 = VALUE|PROPERTY) keeps only the value-class VALUE bit.
    #[test]
    fn dbe_numeric_strips_property_bit_from_value_mask() {
        use epics_base_rs::server::recgbl::EventMask;
        let raw = (EventMask::VALUE | EventMask::PROPERTY).bits();
        let req = req_with_dbe(PvField::Scalar(ScalarValue::Int(raw as i32)));
        let mask = dbe_mask(&req, &RemoteLog::default()).expect("must parse");
        assert_eq!(mask, EventMask::VALUE.bits());
    }

    /// A String-typed DBE is never parsed numerically. pvxs switches on the
    /// field's kind (singlesource.cpp:117-140): `Kind::String` does the
    /// substring scan and nothing else — only `Kind::Integer`/`Kind::Real`
    /// reach `fld.as<uint8_t>()`. So `"1"` does NOT mean DBE_VALUE: it
    /// matches no token, selects an empty mask, draws the warning, and falls
    /// back to VALUE|ALARM. The port used to parse it, giving VALUE-only and
    /// no warning. Boundary cases: `"1"`/`"2"` (would have been a strict
    /// subset of the fallback), `"7"` (would have equalled it silently),
    /// `"8"` (PROPERTY-only), `"0"`.
    #[test]
    fn dbe_numeric_string_is_not_parsed_numerically() {
        use epics_base_rs::server::recgbl::EventMask;
        let fallback = (EventMask::VALUE | EventMask::ALARM).bits();
        for raw in ["0", "1", "2", "7", "8", " 5 "] {
            let log = RemoteLog::default();
            let req = req_with_dbe(PvField::Scalar(ScalarValue::String(raw.into())));
            let mask = dbe_mask(&req, &log).expect("must parse");
            assert_eq!(
                mask, fallback,
                "a numeric string selects no event class, so DBE={raw:?} falls back to VALUE|ALARM"
            );
            let logged = log.take();
            assert_eq!(
                logged.len(),
                1,
                "an empty-mask selection owes the client one warning, DBE={raw:?}: {logged:?}"
            );
            assert_eq!(
                logged[0].message,
                format!("record._options.DBE=\"{raw}\" selects empty mask")
            );
        }
    }

    /// missing DBE option resolves to None so the monitor
    /// falls back to the pvxs-parity default mask.
    #[test]
    fn dbe_mask_absent_returns_none() {
        let req = PvStructure::new("request");
        assert!(dbe_mask(&req, &RemoteLog::default()).is_none());
    }

    /// A numeric `record._options.DBE`
    /// option must select the requested value class regardless of which PVA
    /// numeric scalar type carries it. pvxs reads the field as
    /// `fld.as<uint8_t>()` (singlesource.cpp:134-137) for the `Kind::Integer`
    /// and `Kind::Real` arms of its kind switch, coercing every such storage
    /// through one `copyOutScalar()` path (data.cpp:402-416). Before the fix
    /// only `Int`/`Long` were handled and every other numeric scalar fell to
    /// `None`, so the monitor silently used the default `VALUE|ALARM` (5)
    /// mask. Each value below resolves to a class distinct from that default,
    /// proving the coercion actually ran.
    ///
    /// `Boolean` is deliberately NOT in this list: it is `Kind::Bool`, which
    /// pvxs's switch does not route to `as<uint8_t>()` at all — see
    /// [`dbe_bool_is_unselected_and_falls_back`].
    #[test]
    fn dbe_numeric_coerces_every_scalar_variant() {
        use epics_base_rs::server::recgbl::EventMask;
        let cases = [
            (PvField::Scalar(ScalarValue::UInt(2)), EventMask::LOG.bits()),
            (
                PvField::Scalar(ScalarValue::UByte(4)),
                EventMask::ALARM.bits(),
            ),
            (
                PvField::Scalar(ScalarValue::Short(2)),
                EventMask::LOG.bits(),
            ),
            (
                PvField::Scalar(ScalarValue::Double(2.0)),
                EventMask::LOG.bits(),
            ),
        ];
        for (field, expected) in cases {
            let label = format!("{field:?}");
            let req = req_with_dbe(field);
            let mask = dbe_mask(&req, &RemoteLog::default())
                .unwrap_or_else(|| panic!("DBE {label} must coerce to a value-class mask"));
            assert_eq!(
                mask, expected,
                "DBE {label} resolved to the wrong value mask"
            );
        }
    }

    /// R9-31. A BOOLEAN `record._options.DBE` is `Kind::Bool`, which pvxs's
    /// kind switch (singlesource.cpp:118-140) does not read: it falls to
    /// `default: break` with `dbe` still 0, so `dbe &= 7` leaves 0 and the
    /// `DBE_VALUE | DBE_ALARM` fallback fires (`:141-144`). `Value::as<uint8_t>()`
    /// *can* convert bool storage (data.cpp:428-435) — pvxs just never calls it
    /// for this kind.
    ///
    /// The port used to map `Boolean(b)` through the numeric arm as `b as u8`,
    /// so `DBE=true` became mask 1 = DBE_VALUE: a VALUE-only subscription that
    /// never delivers the alarm-only transitions pvxs sends. `DBE=false`
    /// coincidentally agreed (0 → fallback), which is why only the `true`
    /// boundary showed the defect. A prior version of
    /// `dbe_numeric_coerces_every_scalar_variant` asserted the buggy
    /// `Boolean(true) → VALUE`; that expectation was invented, not taken from
    /// pvxs, and is corrected here.
    #[test]
    fn dbe_bool_is_unselected_and_falls_back() {
        use epics_base_rs::server::recgbl::EventMask;
        let fallback = (EventMask::VALUE | EventMask::ALARM).bits();
        for b in [true, false] {
            let req = req_with_dbe(PvField::Scalar(ScalarValue::Boolean(b)));
            let mask = dbe_mask(&req, &RemoteLog::default())
                .expect("a present DBE option always resolves to a mask");
            assert_eq!(
                mask, fallback,
                "DBE={b} (Kind::Bool) must hit pvxs's `default: break` and fall back to \
                 VALUE|ALARM, not select DBE_VALUE"
            );
        }
    }

    /// R9-35. pvxs dispatches DBE on KIND but converts through STORAGE, and the
    /// two disagree for arrays: `Int32A` is `Kind::Integer` (`code & 0xe0`), so
    /// it reaches `fld.as<uint8_t>()` (singlesource.cpp:134-137) — and
    /// `Value::copyOut` has no scalar arm for array storage, so it raises
    /// `NoConvert` (data.cpp:466-499). Same for a real array, and for a STRING
    /// array (`StringA` is `Kind::String` → `fld.as<std::string>()`).
    ///
    /// Nothing catches that inside `onSubscribe`; `conn.cpp:277-282` does
    /// `bev.reset()`. The port used to fall into its non-scalar `_ =>` arm and
    /// serve VALUE|ALARM — handing the client a working monitor where pvxs hangs
    /// the circuit up.
    #[test]
    fn dbe_array_typed_throws_and_resets_the_circuit() {
        use epics_pva_rs::pvdata::TypedScalarArray;
        let throwing = [
            // Kind::Integer, array storage.
            PvField::ScalarArrayTyped(TypedScalarArray::Int(vec![1].into())),
            PvField::ScalarArrayTyped(TypedScalarArray::UByte(vec![4].into())),
            // Kind::Real, array storage.
            PvField::ScalarArrayTyped(TypedScalarArray::Double(vec![2.0].into())),
            // Kind::String, array storage.
            PvField::ScalarArrayTyped(TypedScalarArray::String(vec!["VALUE".into()].into())),
            // An empty array is still array storage — the throw is about the
            // storage class, not the element count.
            PvField::ScalarArrayTyped(TypedScalarArray::Int(Vec::new().into())),
        ];
        for field in throwing {
            let label = format!("{field:?}");
            let req = req_with_dbe(field);
            assert!(
                dbe_mask_from_pv_request(&req, &RemoteLog::default()).is_err(),
                "DBE {label} must throw NoConvert (circuit reset), not serve a mask"
            );
        }
    }

    /// R9-35, the kind boundary. `BoolA` is `Kind::Bool` and a struct / union is
    /// `Kind::Compound`; BOTH hit pvxs's `default: break` and never reach a
    /// conversion, so they cannot throw — they select nothing and take the
    /// `VALUE|ALARM` fallback. Only the Integer / Real / String kinds convert,
    /// and only those can therefore reset the circuit.
    #[test]
    fn dbe_bool_array_and_compound_do_not_throw() {
        use epics_base_rs::server::recgbl::EventMask;
        use epics_pva_rs::pvdata::TypedScalarArray;
        let fallback = (EventMask::VALUE | EventMask::ALARM).bits();
        let non_throwing = [
            PvField::ScalarArrayTyped(TypedScalarArray::Boolean(vec![true].into())),
            PvField::Structure(PvStructure::new("")),
            PvField::Union {
                selector: -1,
                variant_name: String::new(),
                value: Box::new(PvField::Null),
            },
        ];
        for field in non_throwing {
            let label = format!("{field:?}");
            let req = req_with_dbe(field);
            let mask = dbe_mask(&req, &RemoteLog::default())
                .expect("a present DBE option always resolves to a mask");
            assert_eq!(
                mask, fallback,
                "DBE {label} is a `default: break` kind: no conversion, no throw, VALUE|ALARM"
            );
        }
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

    /// pvxs `Value::as<bool>` coerces numeric scalars by nonzero
    /// truthiness, so a typed `record._options.atomic = 0` must override
    /// an atomic-by-default group (returns `Some(false)`), and `1`
    /// returns `Some(true)` — not the silent `None` that lets the group
    /// default win. Covers signed int, unsigned int, and real.
    #[test]
    fn atomic_option_coerces_numeric_scalars() {
        assert_eq!(
            atomic_from_pv_request(&req_with_atomic(PvField::Scalar(ScalarValue::Int(0)))),
            Some(false),
            "signed int 0 ⇒ false (overrides atomic-default group)"
        );
        assert_eq!(
            atomic_from_pv_request(&req_with_atomic(PvField::Scalar(ScalarValue::Int(1)))),
            Some(true),
            "signed int 1 ⇒ true"
        );
        assert_eq!(
            atomic_from_pv_request(&req_with_atomic(PvField::Scalar(ScalarValue::UInt(1)))),
            Some(true),
            "unsigned int 1 ⇒ true"
        );
        assert_eq!(
            atomic_from_pv_request(&req_with_atomic(PvField::Scalar(ScalarValue::Double(1.0)))),
            Some(true),
            "real 1.0 ⇒ true"
        );
        assert_eq!(
            atomic_from_pv_request(&req_with_atomic(PvField::Scalar(ScalarValue::Double(0.0)))),
            Some(false),
            "real 0.0 ⇒ false"
        );
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

    // ---- Force + block (record[process=true,block=true]) end-to-end wiring ----

    /// End-to-end proof that `put_with_options(Force, block)` routes a
    /// *synchronous* forced put through the put-notify barrier
    /// (`process_record_with_notify`) and returns only after the full
    /// processing cycle — including the OUT link write — has run. This is the
    /// `BridgeChannel` PUT-path twin of the `epics-base-rs`
    /// `force_block_sync_record_returns_none_and_processes` primitive test:
    /// the primitive proves the database method drives the OUT link; this
    /// proves the bridge Force+block branch actually calls it.
    #[tokio::test]
    async fn put_force_block_sync_drives_out_link_before_returning() {
        use epics_base_rs::server::record::Record;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::scalcout::ScalcoutRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("TGT0", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // scalcout ODLY=0: CALC="42" ⇒ VAL=OVAL=42, OOPT=Every ⇒ OUT fires
        // with no delay, so the whole cycle (incl. the OUT write) is synchronous.
        let mut sc = ScalcoutRecord::default();
        sc.put_field("CALC", EpicsValue::String("42".into()))
            .unwrap();
        sc.special("CALC", true).unwrap();
        sc.oopt = 0;
        sc.put_field("ODLY", EpicsValue::Double(0.0)).unwrap();
        sc.put_field("OUT", EpicsValue::String("TGT0".into()))
            .unwrap();
        db.add_record("SC0", Box::new(sc)).await.unwrap();

        let ch = BridgeChannel::new(db.clone(), "SC0")
            .await
            .expect("channel over scalcout VAL must create");
        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(0.0))));
        ch.put_with_options(
            &put,
            PutOptions {
                process: ProcessMode::Force,
                block: true,
            },
        )
        .await
        .expect("forced blocking put must succeed");

        // The barrier held until processing finished: the OUT link drove
        // TGT0.VAL = 42 before the PUT returned.
        let tgt = db.get_record("TGT0").unwrap();
        let v = tgt.read().record.get_field("VAL");
        assert_eq!(
            v,
            Some(EpicsValue::Double(42.0)),
            "Force+block must have driven the OUT write before returning, got {v:?}"
        );
    }

    /// End-to-end proof that `put_with_options(Force, block)` HOLDS the reply
    /// barrier for an *async* record: with ODLY=100 s the record stays PACT
    /// across the (un-fireable-in-test) delay, so the put-notify completion
    /// never arrives and the PUT future must not resolve. A regression that
    /// reverted the Force+block branch to the bare `process_record_with_links`
    /// (which returns as soon as the record goes PACT) would let the PUT
    /// return immediately — the timeout below would NOT fire and this test
    /// would fail. The timeout firing is the barrier proof.
    #[tokio::test]
    async fn put_force_block_async_holds_barrier_until_processing_done() {
        use epics_base_rs::server::record::Record;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::scalcout::ScalcoutRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("TGT1", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // ODLY=100 s: the OUT write is deferred and the record stays PACT
        // across the (un-fireable-in-test) delay — the async shape a blocking
        // forced put must wait on.
        let mut sc = ScalcoutRecord::default();
        sc.put_field("CALC", EpicsValue::String("42".into()))
            .unwrap();
        sc.special("CALC", true).unwrap();
        sc.oopt = 0;
        sc.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
        sc.put_field("OUT", EpicsValue::String("TGT1".into()))
            .unwrap();
        db.add_record("SC1", Box::new(sc)).await.unwrap();

        let ch = BridgeChannel::new(db.clone(), "SC1")
            .await
            .expect("channel over scalcout VAL must create");
        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(0.0))));
        let putting = ch.put_with_options(
            &put,
            PutOptions {
                process: ProcessMode::Force,
                block: true,
            },
        );
        // The barrier must hold: the put future cannot resolve while the
        // record is PACT on the 100 s delay. If it resolves, the Force+block
        // wiring regressed to a non-blocking process call.
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), putting).await;
        assert!(
            outcome.is_err(),
            "Force+block put must NOT return while the async record is PACT; \
             it returned {outcome:?} — the reply barrier regressed"
        );

        // The barrier is genuinely async-pending: DLYA armed, OUT still deferred.
        let sc_rec = db.get_record("SC1").unwrap();
        let dlya = sc_rec.read().record.get_field("DLYA");
        assert_eq!(
            dlya,
            Some(EpicsValue::Short(1)),
            "ODLY cycle must arm DLYA (record held ACTIVE across the delay), got {dlya:?}"
        );
        let tgt = db.get_record("TGT1").unwrap();
        let v = tgt.read().record.get_field("VAL");
        assert_eq!(
            v,
            Some(EpicsValue::Double(0.0)),
            "OUT must stay deferred until the delay completes, got {v:?}"
        );
    }

    /// The barrier's RELEASE path: a Force+block put to an async record whose
    /// delay DOES complete in-test must return `Ok` only AFTER the deferred
    /// OUT link fired. `ODLY≈0.05 s` arms the async barrier; the real tokio
    /// timer fires within the 5 s guard; the reprocess drives OUT → TGT2.VAL=42
    /// and completes the put-notify wait-set → the put resolves. A barrier that
    /// held forever (rx never fired even after the timer) would hang and trip
    /// the timeout. The sync test and the async-hold test never observe this
    /// successful completion — this is the third boundary the reviewer flagged.
    #[tokio::test]
    async fn put_force_block_async_releases_after_delay_completes() {
        use epics_base_rs::server::record::Record;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::scalcout::ScalcoutRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("TGT2", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // Short ODLY: the record goes PACT and arms the delay, then the timer
        // fires and the reprocess drives the OUT — the async barrier both holds
        // and then releases inside the test.
        let mut sc = ScalcoutRecord::default();
        sc.put_field("CALC", EpicsValue::String("42".into()))
            .unwrap();
        sc.special("CALC", true).unwrap();
        sc.oopt = 0;
        sc.put_field("ODLY", EpicsValue::Double(0.05)).unwrap();
        sc.put_field("OUT", EpicsValue::String("TGT2".into()))
            .unwrap();
        db.add_record("SC2", Box::new(sc)).await.unwrap();

        let ch = BridgeChannel::new(db.clone(), "SC2")
            .await
            .expect("channel over scalcout VAL must create");
        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(0.0))));
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ch.put_with_options(
                &put,
                PutOptions {
                    process: ProcessMode::Force,
                    block: true,
                },
            ),
        )
        .await;
        assert!(
            matches!(outcome, Ok(Ok(()))),
            "Force+block put must release with Ok once the ODLY delay completes, got {outcome:?}"
        );
        // Released only after the deferred OUT actually fired.
        let tgt = db.get_record("TGT2").unwrap();
        let v = tgt.read().record.get_field("VAL");
        assert_eq!(
            v,
            Some(EpicsValue::Double(42.0)),
            "the barrier must release only after the deferred OUT drove TGT2.VAL=42, got {v:?}"
        );
    }

    /// A rejected single-record QSRV PUT must carry pvxs's bare contract
    /// text on the wire (the `BridgeError::PutRejected` message becomes the
    /// `Status.message` via `OpError::failed`). pvxs throws
    /// `"Unable to put value: Field Disabled: S_db_putDisabled"` /
    /// `"…: Modifications not allowed: S_db_noMod"` (iocsource.cpp:366-368)
    /// and `"Put not permitted"` (:385) — no record name, no user/host, no
    /// source citation. Boundary: SPC_ATTRIBUTE is tested *before* `disp` in
    /// C, so a read-only field on a DISP=1 record reports noMod.
    #[tokio::test]
    async fn put_rejection_messages_are_pvxs_contract_text() {
        use crate::qsrv::provider::{AccessContext, AccessControl};
        use epics_base_rs::server::records::ai::AiRecord;

        struct DenyWrites;
        #[async_trait::async_trait]
        impl AccessControl for DenyWrites {
            async fn can_write(&self, _: &str, _: &str, _: &str) -> bool {
                false
            }
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record("PS:ai", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();

        let mut put = PvStructure::new("epics:nt/NTScalar:1.0");
        put.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Double(2.0))));

        // write-ACF denial → "Put not permitted"
        let denied = BridgeChannel::new(db.clone(), "PS:ai")
            .await
            .unwrap()
            .with_access(AccessContext::with_identity(
                Arc::new(DenyWrites),
                "alice".into(),
                "host1".into(),
            ));
        let err = denied.put(&put).await.expect_err("write must be denied");
        assert_eq!(
            super::super::put_status::wire_message(&err),
            "Put not permitted",
            "ACF denial must not leak PV name / user / host onto the wire"
        );

        // DISP=1 → "Unable to put value: Field Disabled: S_db_putDisabled"
        db.get_record("PS:ai").unwrap().write().common.disp = 1;
        let ch = BridgeChannel::new(db.clone(), "PS:ai").await.unwrap();
        let err = ch.put(&put).await.expect_err("DISP=1 must reject the put");
        assert_eq!(
            super::super::put_status::wire_message(&err),
            "Unable to put value: Field Disabled: S_db_putDisabled"
        );

        // Read-only (SPC_NOMOD) field on the SAME DISP=1 record → noMod wins,
        // because C tests `special == SPC_ATTRIBUTE` first.
        let ro = BridgeChannel::new(db.clone(), "PS:ai.AFVL").await.unwrap();
        let err = ro
            .put(&put)
            .await
            .expect_err("a read-only field must reject the put");
        assert_eq!(
            super::super::put_status::wire_message(&err),
            "Unable to put value: Modifications not allowed: S_db_noMod",
            "SPC_ATTRIBUTE is tested before disp (iocsource.cpp:365-369)"
        );
    }
}
