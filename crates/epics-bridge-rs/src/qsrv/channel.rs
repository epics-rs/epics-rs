//! BridgeChannel: single-record PVA channel.
//!
//! Corresponds to C++ QSRV's `PDBSinglePV` / `PDBSingleChannel`.

// RTEMS-EXEC-MODEL-ALLOW(13): checked - these run and pass in the exec-backend
// suite.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::{DbFieldType, DbfLinkClass, EpicsValue};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, convert};
use epics_pva_rs::server_native::source::RemoteLog;

use super::group_config::GroupMember;
use super::monitor::BridgeMonitor;
use super::provider::Channel;
use super::pvif::{
    self, NtType, build_field_desc_for_nt, pv_structure_to_epics, snapshot_to_pv_structure,
};
use crate::convert::{dbf_to_scalar_type, scalar_to_epics_typed};
use crate::error::{BridgeError, BridgeResult};

// ---------------------------------------------------------------------------
// pvRequest option parsing
// ---------------------------------------------------------------------------
//
// The parsers moved to `epics_pva_rs::server_native::source`, next to
// `MonitorOptions`, so the native PVA source reads `record._options.process`
// / `.block` / `.DBE` from the SAME owner this bridge does; `ProcessMode`
// itself is the database's, since it is the term
// `PvDatabase::put_field_from_client` routes on. Re-exported here because
// `epics_bridge_rs::qsrv::{ProcessMode, PutOptions,
// dbe_mask_from_pv_request}` is a published path.
pub use epics_base_rs::server::database::ProcessMode;
pub use epics_pva_rs::server_native::source::{PutOptions, dbe_mask_from_pv_request};

/// parse `record._options.atomic` from a group operation
/// pvRequest. Returns `Some(true|false)` when the option is set and
/// convertible, `None` when absent or unconvertible — the caller then
/// falls back to the group's default atomicity.
///
/// pvxs resolves group atomicity with `pvRequest["record._options.atomic"]
/// .as(atomic)` on both PUT and GET (`groupsource.cpp:204,481`), i.e.
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

// ---------------------------------------------------------------------------
// BridgeChannel
// ---------------------------------------------------------------------------

/// `dbChannelCreate(name)` admission test (C `dbChannel.c:448-532`) for a
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
/// A `{json}` / `[range]` filter suffix is honoured: [`MemberChannel`] binds
/// the member's two chains once, so the only thing this gate has to answer is
/// whether the suffix parsed — the same question `dbChannelCreate` answers at
/// `dbChannel.c:513-529`, and the same failure pvxs turns into a dropped
/// group. It reads [`MemberChannel::filter_error`] rather than re-parsing, so
/// the chain a served member holds is the chain this gate judged.
///
/// The `$` modifier is honoured, not refused. `dbChannelCreate` re-views a
/// `DBF_STRING` or link field as a `DBR_CHAR` array and returns
/// `S_dbLib_fieldNotFound` for every other field type
/// (`dbChannel.c:486-505`), and the group path reaches that view through the
/// same `IOCSource::get` the single-record path does: `groupsource.cpp:344,377`
/// and `singlesource.cpp:59,289` call one function, whose long-string branch
/// is `iocsource.cpp:133-136` (which leaf types reach that branch, and where
/// this port deviates, is [`pvif::nt_type_for_channel`]). So the two surfaces
/// are asked one question here
/// — [`epics_base_rs::server::record::RecordInstance::channel_field_value`],
/// which resolves through the member's own view — and an ineligible `$` fails
/// it with the same `Invalid PV:` text pvxs prints.
pub(super) async fn resolve_db_channel(
    db: &PvDatabase,
    member: &MemberChannel,
) -> Result<(), String> {
    let name = member.def.channel.as_str();
    if let Some(e) = &member.filter_error {
        return Err(format!("Invalid PV: {name}: {e}"));
    }
    let Some(rec) = db.get_record(&member.record) else {
        return Err(format!("Invalid PV: {name}"));
    };
    let instance = rec.read();
    if member.value_in(&instance).is_some() {
        Ok(())
    } else {
        Err(format!("Invalid PV: {name}"))
    }
}

/// The dbStatic link class of the field a `+channel` names, or `None` when
/// it is not a link field — the one owner both the group CREATION gate
/// ([`super::provider::BridgeProvider::process_groups`]) and the group PUT
/// prep pass consult, so the two cannot drift apart on what a link is.
///
/// Each caller picks the range it means, because pvxs's two link tests do
/// not agree: `IOCSource::getChannelValueType(chan, errOnLinks = true)`
/// throws for `DBF_INLINK..=DBF_OUTLINK` on the type-build path
/// (`iocsource.cpp:626-630`), while `groupsource.cpp:603-604` tests
/// `DBF_INLINK..=DBF_FWDLINK` on the put path.
pub(super) fn channel_link_class(db: &PvDatabase, member: &MemberChannel) -> Option<DbfLinkClass> {
    // The record type resolves the two direction-ambiguous families
    // (`SIOL`, `LNK*`); an unresolvable record still classifies the
    // unambiguous ones by name.
    let record_type: &'static str = match db.get_record(&member.record) {
        Some(rec) => rec.read().record.record_type(),
        None => "",
    };
    epics_base_rs::types::dbf_link_class(record_type, &member.field)
}

// ---------------------------------------------------------------------------
// MemberChannel
// ---------------------------------------------------------------------------

/// One group member's resolved channel pair — this port's `Field::value` and
/// `Field::properties`.
///
/// pvxs binds a member's two `dbChannel`s ONCE, in `Field::Field`
/// (`ioc/field.cpp:23-26`: `value = Channel(def.channel); properties =
/// Channel(def.channel);`), and that `Field` lives in a `Group` owned by the
/// process-global `IOCGroupConfig::groupMap` (`ioc/group.h:39-53`, with
/// `Group(const Group&) = delete`). A group member's channel state is
/// therefore IOC-wide and shared by every client of the group — unlike the
/// single-record path, where `dbChannelCreate` runs per client channel.
/// [`GroupPvDef::channels`](super::group_config::GroupPvDef::channels) holds
/// these behind one `Arc`, so cloning a def per downstream channel shares
/// them the way pvxs's `Group&` does.
///
/// Binding once is also what gives a member somewhere to keep a
/// [`FilterChain`](epics_base_rs::server::database::filters::FilterChain).
/// A `{"dbnd":…}` chain carries a per-instance baseline; re-parsed per
/// operation it would reset that baseline on every call and never filter
/// anything, which is why the group paths could not honour a filtered
/// `+channel` while they re-derived `(record, field)` from the raw string.
pub struct MemberChannel {
    /// The config member this resolves. Group code reads mapping, triggers
    /// and `+putorder` through here, so a resolved channel and the
    /// definition it came from cannot be held apart.
    pub def: GroupMember,
    /// Record name from the member's `+channel`, filter suffix and `$`
    /// modifier already peeled (`dbChannel.c:448-532`). Empty for a
    /// channel-less (Structure / Const) member.
    pub record: String,
    /// Uppercased field name from the same split.
    pub field: String,
    /// `record[.FIELD]` with the `{json}` suffix and the `$` modifier
    /// peeled (`ChannelName::record_path`) — the name a database lookup
    /// takes. The raw `+channel` must never reach `parse_pv_name`, which
    /// does not peel and would take the suffix for part of the field name;
    /// binding the peeled form here is what lets the member subscription
    /// address `REC.VAL` while its chain carries the `{"dbnd":…}`.
    pub pv_name: String,
    /// The `+channel` asked for the `$` long-string view.
    pub string_view: bool,
    /// The value channel's filter chain — pvxs's `Field::value` filters.
    pub value_filters: Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// The PROPERTY channel's chain: an INDEPENDENT parse of the same
    /// suffix, because pvxs calls `Channel(def.channel)` twice and
    /// `dbChannelCreate` re-runs the filter constructors per channel
    /// (`dbChannel.c:471`). Filter state is per channel, so the value and
    /// property subscriptions must not share one `dbnd` baseline.
    pub property_filters: Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// Why the `{json}` suffix would not parse into a chain, when it would
    /// not. `dbChannelCreate` fails on an unparseable filter
    /// (`dbChannel.c:513-529`) and pvxs drops the WHOLE group
    /// (`groupconfigprocessor.cpp:429-444`); binding is infallible here, so
    /// the failure is recorded on the object and
    /// `resolve_db_channel` — the creation gate — reads it. Nothing else
    /// re-parses the suffix, so the chain a served member actually holds and
    /// the chain the gate judged are the same parse.
    pub filter_error: Option<String>,
}

impl MemberChannel {
    /// Bind `def.channel`. A channel-less member (Structure / Const, and a
    /// `proc` member without a `+channel`) binds nothing — pvxs guards the
    /// whole block on `if(!def.channel.empty())` (`ioc/field.cpp:23`).
    pub fn new(def: GroupMember) -> Self {
        use epics_base_rs::server::database::filters as f;
        if def.channel.is_empty() {
            return Self {
                def,
                record: String::new(),
                field: String::new(),
                pv_name: String::new(),
                string_view: false,
                value_filters: Arc::new(f::FilterChain::new()),
                property_filters: Arc::new(f::FilterChain::new()),
                filter_error: None,
            };
        }
        let cn = f::parse_channel_name(&def.channel);
        // Two parses, not one clone: see `property_filters`.
        let mut filter_error = None;
        let (value_filters, property_filters) = match cn.json_suffix.as_deref() {
            Some(json) => {
                let mut parse = || match f::try_parse_filter_chain(json) {
                    Ok(chain) => chain,
                    Err(e) => {
                        filter_error.get_or_insert_with(|| e.to_string());
                        f::FilterChain::new()
                    }
                };
                (Arc::new(parse()), Arc::new(parse()))
            }
            None => (
                Arc::new(f::FilterChain::new()),
                Arc::new(f::FilterChain::new()),
            ),
        };
        Self {
            def,
            record: cn.record,
            field: cn.field,
            pv_name: cn.record_path,
            string_view: cn.string_view,
            value_filters,
            property_filters,
            filter_error,
        }
    }

    /// True iff this member binds a backing `dbChannel` at all.
    pub fn has_channel(&self) -> bool {
        !self.def.channel.is_empty()
    }

    /// `(record, FIELD)` as the group paths need them.
    pub fn names(&self) -> (&str, &str) {
        (self.record.as_str(), self.field.as_str())
    }

    /// The value this member's channel serves, read through the `$` view it
    /// was bound with — [`epics_base_rs::server::record::RecordInstance::
    /// channel_field_value`] with this member's `string_view`.
    ///
    /// Every group path that needs a member's value goes through here rather
    /// than resolving [`Self::names`]'s field itself: the bare name has lost
    /// the view by then, and a path that re-resolves it serves the unviewed
    /// value under a descriptor built from the viewed one.
    pub fn value_in(
        &self,
        instance: &epics_base_rs::server::record::RecordInstance,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        instance.channel_field_value(&self.field, self.string_view)
    }

    /// [`Self::value_in`] with the field's full metadata attached — the
    /// snapshot the `+type:"scalar"` and `+type:"meta"` mappings serve.
    ///
    /// `backing` is the caller's, not this member's: a link-backed member
    /// answers its units/precision from the LINK TARGET's record, which has
    /// to be resolved before any member read guard is taken, so the two read
    /// paths resolve it themselves (`read_group`'s `member_backings` for the
    /// atomic one, `read_member` for the non-atomic one) and hand it down.
    /// This method owns only the `$` view; it carries `backing` through
    /// untouched.
    pub fn snapshot_in(
        &self,
        instance: &epics_base_rs::server::record::RecordInstance,
        backing: epics_base_rs::server::database::LinkBacking<'_>,
    ) -> Option<epics_base_rs::server::snapshot::Snapshot> {
        instance.channel_snapshot_for_field(&self.field, self.string_view, backing)
    }

    /// The NT this member serves — the port's `getChannelValueType` asked
    /// with this member's view, so a `$` member is the same long string on
    /// the group surface as on the single-record one.
    pub(super) fn nt_type_in(
        &self,
        instance: &epics_base_rs::server::record::RecordInstance,
        resolved: Option<&epics_base_rs::types::EpicsValue>,
    ) -> NtType {
        pvif::nt_type_for_channel(instance, &self.field, resolved, self.string_view)
    }

    /// `dbIsValueField(dbChannelFldDes(chan))` for this member — the fact
    /// `IOCSource::initialize` gates `display.form.index` on
    /// (`iocsource.cpp:54`). Answered off the bound field, so a
    /// `REC.VAL{"dbnd":…}` member is VAL and a `REC.RVAL` one is not.
    pub fn is_value_field(&self) -> bool {
        epics_base_rs::server::database::is_value_field(&self.field)
    }
}

impl std::fmt::Debug for MemberChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberChannel")
            .field("def", &self.def)
            .field("record", &self.record)
            .field("field", &self.field)
            .field("pv_name", &self.pv_name)
            .field("string_view", &self.string_view)
            .field("value_filters", &self.value_filters.len())
            .field("property_filters", &self.property_filters.len())
            .finish()
    }
}

/// `dbIsValueField(dbChannelFldDes(chan))` for a QSRV channel name (single
/// channel) or a group member's `+channel` — the fact QSRV's
/// `IOCSource::initialize` gates `display.form.index` on
/// (`iocsource.cpp:54`). Peels the filter suffix and `$` modifier first, so
/// `REC.VAL{"dbnd":…}` and `REC` are VAL while `REC.RVAL` is not.
pub(super) fn channel_is_value_field(name: &str) -> bool {
    let cn = epics_base_rs::server::database::filters::parse_channel_name(name);
    epics_base_rs::server::database::is_value_field(&cn.field)
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
    /// The `$` char-array re-view this channel was created with
    /// (`dbChannel.c:486-505`). Derived from [`Self::pv_name`] by BOTH
    /// constructors rather than passed in, so it can never disagree with the
    /// name the client asked for — and so no construction path can produce a
    /// channel that has a field but has lost its view.
    string_view: bool,
    nt_type: NtType,
    /// The DBF type of the bound field (not always VAL).
    value_dbf: DbFieldType,
    /// Parsed pvxs-compatible channel-filter chain from the trailing
    /// JSON suffix on the PV name (`PV.VAL{"dbnd":{"d":2.0}}`,
    /// `PV.VAL{"arr":{"s":1,"e":2}}`, …). Empty chain when the name
    /// carries no suffix. pvxs attaches the chain to the `dbChannel`, so
    /// it governs BOTH the monitor subscription AND one-shot GET reads:
    /// GET wraps the read in a `LocalFieldLog` and runs the pre/post
    /// chain (`ioc/singlesource.cpp:278-292`, `localfieldlog.cpp:15-27`).
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
            string_view: epics_base_rs::server::database::filters::parse_channel_name(&pv_name)
                .string_view,
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
        let parsed = epics_base_rs::server::database::filters::parse_channel_name(name);
        // A syntactically-present filter suffix that cannot be parsed
        // into the requested chain aborts channel creation, mirroring
        // EPICS `dbChannelCreate()` (`dbChannel.c:513-529`). Fail-open
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
        // `DBR_CHAR` character array, which this port serves as the
        // `form = "String"` long-string `NTScalar` — pvxs only for the link
        // half, see [`pvif::nt_type_for_channel`]. The `$`
        // is innermost in the channel name (`REC.FIELD$[range]{json}`) and
        // `split_channel_name` has already peeled `{json}` and `[range]`,
        // so the modifier is the final character of the record path. It is
        // left there (the CA server detects it on the record path too,
        // `epics-ca-rs` `tcp.rs`) and peeled here for field resolution.
        let (record_name, field_upper, string_view) = (
            parsed.record.as_str(),
            parsed.field.clone(),
            parsed.string_view,
        );

        let rec = db
            .get_record(record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read();
        // One resolution for both views. `RecordInstance::channel_field_value`
        // is the owner of "what does a channel bound to (field, `$`) serve":
        // without `$` it is `client_field_value`, the value projected onto the
        // field's DECLARED type, which is what every delivery path serves
        // (reading the DBF off the raw stored variant is what advertised
        // `.PROC` (`DBF_UCHAR`) as a signed byte and a `DBF_MENU` field as a
        // short); with `$` it is the char-array re-view, which this port
        // serves as the string it collapses to, the way pvxs collapses a
        // `TypeCode::String` leaf over a `DBR_CHAR` buffer
        // (`ioc/iocsource.cpp:133-136`) rather than shipping a parallel byte
        // array. Which channels get that leaf — and where pvxs and this port
        // part company over `$` — is [`pvif::nt_type_for_channel`].
        //
        // `None` means `dbChannelCreate` would have returned NULL — the
        // record has no such field, or `$` was applied to a field that cannot
        // be re-viewed (`dbChannel.c:460-462,486-505`; pvxs renders it
        // `Invalid PV:`, `ioc/channel.cpp:29-38`). Falling through used to
        // fabricate an NTScalar double prototype the client would connect to
        // and then fail every operation against (pvxs#193, server half).
        let value = instance
            .channel_field_value(&field_upper, string_view)
            .ok_or_else(|| BridgeError::FieldNotFound {
                record: record_name.to_string(),
                field: if string_view {
                    format!("{field_upper}$")
                } else {
                    field_upper.clone()
                },
            })?;
        // `pvif::nt_type_for_channel` is the single owner of the NT choice
        // (the port's `getChannelValueType`): the `$` view, a record-declared
        // long-string field (`lsi`/`lso` VAL/OVAL, `printf` VAL) and a
        // `DBF_CHAR` array VAL carrying `info(Q:form, "String")` — the QSRV
        // long-string idiom — all resolve to the scalar-string NTScalar. It
        // is asked with `string_view` rather than branched around, so the
        // group path cannot reach a different answer for the same channel.
        let nt_type = pvif::nt_type_for_channel(&instance, &field_upper, Some(&value), string_view);

        // DBF type for the bound field. pvxs serves the type from
        // `dbChannelFinalFieldType(chan)` (singlesource.cpp:189-206,
        // dbChannel.h:452) — the channel's final field type after lookup,
        // which covers `dbCommon` fields, not only record-specific ones.
        //
        // `value` above is `client_field_value`, i.e. already projected onto
        // the field's declared type, so reading the DBF off it IS reading the
        // declaration — and the advertised descriptor agrees with the value the
        // GET path serializes by construction, because both are that one
        // projection. Taking it from the RAW stored variant used to advertise
        // `.PROC` (`DBF_UCHAR`) as a signed byte and every `DBF_MENU` field as
        // a short: agreement with the value, but agreement on the wrong type.
        let value_dbf = value.db_field_type();

        Ok(Self {
            db,
            pv_name: name.to_string(),
            record_name: record_name.to_string(),
            field: field_upper,
            string_view,
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
    /// from the INIT pvRequest (`iocsource.cpp:430`), not from the
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
        // C `IOCSource::doPreProcessing` (iocsource.cpp:362-375), which
        // pvxs runs on every QSRV put in every process mode
        // (singlesource.cpp:356-359) BEFORE the write-ACF check: reject a
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
                self.pv_name, self.access.creds.user, self.access.creds.host
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
        // `dbProcess(precord)` (iocsource.cpp:397-421), the full
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
            user: &self.access.creds.user,
            host: &self.access.creds.host,
            peer: &self.access.creds.host,
            dbr_type: self.value_dbf as u16,
        };
        super::trap_write::put_with_trap(grant, meta, epics_val, |value| async move {
            // The whole `record._options.process`/`block` decision tree —
            // link-field override included — is the database's
            // `put_field_from_client`, shared with the native PVA source so
            // the two servers cannot drift on what `process=false` or
            // `block=true` means.
            self.db
                .put_field_from_client(
                    &self.record_name,
                    &self.field,
                    value,
                    opts.process,
                    opts.block,
                )
                .await
                .map_err(|e| BridgeError::PutRejected(e.to_string()))
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
                self.pv_name, self.access.creds.user, self.access.creds.host
            )));
        }

        let rec = self
            .db
            .get_record(&self.record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.clone()))?;

        // Through the database, which resolves a link-backed field's metadata
        // from the LINK TARGET's record — a second lock this holds none of —
        // and which is asked with this channel's `$` view, not its bare field
        // name. Reading the bare field here was the second door: `new` decided
        // the view, used it for `nt_type`, and dropped it, so the GET served
        // the unviewed value under a descriptor built from the viewed one.
        let mut snapshot = self
            .db
            .channel_snapshot_for_field(&rec, &self.field, self.string_view)
            .ok_or_else(|| BridgeError::FieldNotFound {
                record: self.record_name.clone(),
                field: self.field.clone(),
            })?;

        // Apply the channel-filter chain in READ context. pvxs wraps
        // every QSRV GET in a `LocalFieldLog` and runs the field-log
        // pre/post chain before serialization (ioc/singlesource.cpp:
        // 278-292, ioc/localfieldlog.cpp:15-27); a GET on a filtered
        // channel must return the same transformed value as the monitor,
        // not the raw record snapshot. `arr` slicing and `ts` tagging
        // transform the value.
        //
        // A chain that DROPS the read yields the unfiltered value, not an
        // error. `db_create_read_log` builds the log through
        // `db_create_field_log`'s `freeListCalloc` and sets only `ctx`
        // (`dbEvent.c:760-770`, `:702`), so `mask` is zero: `dbnd`'s
        // `send = pfl->mask & ~(DBE_VALUE|DBE_LOG)` starts at 0 and
        // `recGblCheckDeadband`'s zero `add_mask` can never raise it
        // (`filters/dbnd.c:83-88`), so the log is deleted and the chain
        // returns NULL. `LocalFieldLog` keeps that NULL
        // (`localfieldlog.cpp:15-27`) and `IOCSource::get` hands it to
        // `dbChannelGet` (`iocsource.cpp:79`), which reads the LIVE record
        // when `pfl` is null (`dbAccess.c:924-930`). So a `{"dbnd":…}`
        // channel answers every GET; only its event stream is gated.
        if !self.channel_filters.is_empty() {
            let raw = snapshot.value.clone();
            snapshot.value = self
                .channel_filters
                .apply_to_read_value(snapshot.value)
                .unwrap_or(raw);
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
                self.pv_name, self.access.creds.user, self.access.creds.host
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
    use epics_pva_rs::pvdata::ScalarValue;

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

    /// A value-gating filter never fails a GET.
    ///
    /// `dbnd` deletes the read log (its `send` starts at zero for a
    /// zero-mask read log and `recGblCheckDeadband`'s zero `add_mask`
    /// cannot raise it, `filters/dbnd.c:83-88`), and the resulting NULL
    /// makes `dbChannelGet` read the live record (`dbAccess.c:924-930`,
    /// via `localfieldlog.cpp:15-27` and `iocsource.cpp:79`). The port
    /// answered `ChannelFilterError` instead, so every GET after the
    /// first on a `{"dbnd":…}` channel failed where pvxs serves the
    /// current value.
    #[tokio::test]
    async fn a_deadband_filter_never_fails_a_get() {
        let db = db_with_rec().await;
        let ch = BridgeChannel::new(db.clone(), r#"REC.VAL{"dbnd":{"d":10.0}}"#)
            .await
            .expect("a dbnd channel is created");
        let request = PvStructure::new("");

        let value_of = |pv: &PvStructure| match pv.get_field("value") {
            Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(v))) => *v,
            other => panic!("value must be a double, got {other:?}"),
        };

        // The first GET moves the deadband baseline to the current value;
        // the second sits inside the band and the chain drops it.
        assert_eq!(value_of(&ch.get(&request).await.expect("first GET")), 1.0);
        assert_eq!(
            value_of(&ch.get(&request).await.expect("second GET must not fail")),
            1.0,
            "a dropped read serves the live record value"
        );
    }

    /// Malformed JSON aborts channel creation rather than failing open
    /// to an unfiltered monitor (`dbChannel.c:513-529`).
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

    /// pvxs#193, server half: a field the record does not declare aborts
    /// channel creation (C `dbChannelCreate` → `S_dbLib_fieldNotFound`,
    /// `dbChannel.c:457-462`) instead of fabricating an NTScalar double
    /// prototype the client connects to and then fails every GET against.
    #[tokio::test]
    async fn a_field_the_record_does_not_have_refuses_the_channel() {
        let db = db_with_rec().await;
        let res = BridgeChannel::new(db, "REC.NOSUCH").await;
        assert!(matches!(res, Err(BridgeError::FieldNotFound { .. })));
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
    /// and `"Put not permitted"` (iocsource.cpp:385) — no record name, no
    /// user/host, no source citation. Boundary: SPC_ATTRIBUTE is tested
    /// *before* `disp` in C, so a read-only field on a DISP=1 record reports
    /// noMod.
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

    fn member(channel: &str) -> MemberChannel {
        MemberChannel::new(GroupMember {
            field_name: "f".into(),
            channel: channel.into(),
            mapping: crate::qsrv::FieldMapping::Scalar,
            triggers: super::super::group_config::TriggerDef::SelfOnly,
            put_order: None,
            struct_id: None,
            const_value: None,
        })
    }

    /// A group `+channel` carrying a *parseable* channel filter is now
    /// admitted: [`MemberChannel`] binds the member's two chains once, so
    /// there is somewhere for the filter state to live, and the creation
    /// gate has nothing left to refuse. pvxs runs `dbChannelCreate` per
    /// member channel and honours the suffix (`ioc/channel.cpp:29-38`).
    #[tokio::test]
    async fn a_filtered_group_member_channel_is_admitted() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = PvDatabase::new();
        db.add_record("GRP:AI", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let m = member(r#"GRP:AI{"arr":{"s":0}}"#);
        assert_eq!(m.names(), ("GRP:AI", "VAL"), "the suffix is peeled first");
        assert_eq!(m.value_filters.len(), 1);
        assert_eq!(m.property_filters.len(), 1);
        resolve_db_channel(&db, &m)
            .await
            .expect("a parseable filter suffix is admitted");
        resolve_db_channel(&db, &member("GRP:AI"))
            .await
            .expect("the unfiltered member still resolves");
    }

    /// An UNPARSEABLE suffix still drops the group — `dbChannelCreate`
    /// fails (`dbChannel.c:513-529`) and pvxs's `createGroups` catches the
    /// throw (`groupconfigprocessor.cpp:429-444`). The gate reads the parse
    /// the member already performed, so it cannot disagree with the chain
    /// the member would serve.
    #[tokio::test]
    async fn an_unparseable_group_member_filter_drops_the_group() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = PvDatabase::new();
        db.add_record("GRP:AI", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let m = member(r#"GRP:AI{"nosuchfilter":{}}"#);
        assert!(m.filter_error.is_some(), "the bad suffix is recorded");
        assert!(m.value_filters.is_empty(), "and no chain is served");
        let err = resolve_db_channel(&db, &m)
            .await
            .expect_err("an unparseable filter must not be admitted");
        assert!(err.starts_with("Invalid PV: "), "got {err:?}");
    }

    /// The member split is `dbChannelCreate`'s order — suffix first, then
    /// the last `.` (`dbChannel.c:448-532`). A bare `rsplit_once('.')`
    /// tears the JSON apart at its own decimal point, which is what the
    /// twelve former call sites would have done once the refusal lifted.
    #[test]
    fn member_channel_peels_the_suffix_before_splitting() {
        assert_eq!(
            member(r#"REC.VAL{"dbnd":{"d":0.5}}"#).names(),
            ("REC", "VAL")
        );
        assert_eq!(member("REC.VAL[1:3]").names(), ("REC", "VAL"));
        assert_eq!(member("REC.VAL$").names(), ("REC", "VAL"));
        assert!(member("REC.VAL$").string_view);
        // the ordinary cases the old split already got right
        assert_eq!(member("REC.egu").names(), ("REC", "EGU"));
        assert_eq!(member("REC").names(), ("REC", "VAL"));
    }

    /// The two chains are independent parses, not one shared instance:
    /// pvxs calls `Channel(def.channel)` twice (`ioc/field.cpp:23-26`) and
    /// `dbChannelCreate` re-runs the filter constructors per channel
    /// (`dbChannel.c:471`), so a `dbnd` baseline moved by a value event
    /// must not move the property stream's.
    #[test]
    fn the_two_member_chains_do_not_share_state() {
        let m = member(r#"REC.VAL{"dbnd":{"d":10.0}}"#);
        assert!(
            !std::sync::Arc::ptr_eq(&m.value_filters, &m.property_filters),
            "the value and property chains must be separate instances"
        );
    }
}
