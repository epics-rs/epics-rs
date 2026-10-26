//! BridgeChannel: single-record PVA channel.
//!
//! Corresponds to C++ QSRV's `PDBSinglePV` / `PDBSingleChannel`.

// RTEMS-EXEC-MODEL-ALLOW(16): checked - these run and pass in the exec-backend
// suite.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::char_view::{ChannelAddr, PvaDollar};
use epics_base_rs::types::{DbFieldType, DbfLinkClass, EpicsValue, RecordName};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, convert};
use epics_pva_rs::server_native::source::RemoteLog;

use super::group_config::GroupMember;
use super::provider::Channel;
use super::pvif::{
    self, NtType, View, build_field_desc_for_nt, pv_structure_to_epics, snapshot_to_pv_structure,
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
use epics_pva_rs::pvdata::encode::default_value_for;
use epics_pva_rs::server_native::source::RecordPutKind;
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
/// The first question is pvxs's own: `Field::Field` calls `Channel(def.channel)`,
/// i.e. [`db_channel_create`](epics_base_rs::server::database::filters::db_channel_create),
/// whose NULL — unknown filter line and all — drops the group
/// (`groupconfigprocessor.cpp:429-444`).
///
/// The `$` modifier is honoured, not refused. `dbChannelCreate` re-views a
/// `DBF_STRING` or link field as a `DBR_CHAR` array and returns
/// `S_dbLib_fieldNotFound` for every other field type
/// (`dbChannel.c:486-505`), and the group path reaches that view through the
/// same `IOCSource::get` the single-record path does: `groupsource.cpp:344,377`
/// and `singlesource.cpp:59,289` call one function. So the two surfaces are
/// asked one question here — [`MemberChannel::view_in`], the member's own
/// view of its address — and an ineligible `$` fails it with the same
/// `Invalid PV:` text pvxs prints. `dbChannelCreate` takes no lock, so
/// neither does this.
pub(super) fn resolve_db_channel(db: &PvDatabase, member: &MemberChannel) -> Result<(), String> {
    let name = &member.def.channel[..];
    let invalid = || format!("Invalid PV: {}", String::from_utf8_lossy(name));
    if epics_base_rs::server::database::filters::db_channel_create(db, name).is_err() {
        return Err(invalid());
    }
    db.get_record(&member.record)
        .and_then(|rec| member.addr(db, &rec))
        .and_then(|addr| member.view_in(&addr))
        .map(drop)
        .ok_or_else(invalid)
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
        Some(rec) => rec.record_type(),
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
    pub record: RecordName,
    /// Uppercased field name from the same split.
    pub field: String,
    /// `record[.FIELD]` with the `{json}` suffix and the `$` modifier
    /// peeled (`ChannelName::record_path`) — the name a database lookup
    /// takes. The raw `+channel` must never reach `parse_pv_name`, which
    /// does not peel and would take the suffix for part of the field name;
    /// binding the peeled form here is what lets the member subscription
    /// address `REC.VAL` while its chain carries the `{"dbnd":…}`.
    pub pv_name: RecordName,
    /// The `+channel` asked for the `$` view.
    pub string_view: bool,
    /// [`Self::put_field`].
    put_field: String,
    /// The value channel's filter chain — pvxs's `Field::value` filters.
    pub value_filters: Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// The PROPERTY channel's chain: an INDEPENDENT parse of the same
    /// suffix, because pvxs calls `Channel(def.channel)` twice and
    /// `dbChannelCreate` re-runs the filter constructors per channel
    /// (`dbChannel.c:471`). Filter state is per channel, so the value and
    /// property subscriptions must not share one `dbnd` baseline.
    pub property_filters: Arc<epics_base_rs::server::database::filters::FilterChain>,
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
                record: RecordName::from(&b""[..]),
                field: String::new(),
                pv_name: RecordName::from(&b""[..]),
                string_view: false,
                put_field: String::new(),
                value_filters: Arc::new(f::FilterChain::new()),
                property_filters: Arc::new(f::FilterChain::new()),
            };
        }
        let cn = f::parse_channel_name(&def.channel);
        // Two parses, not one clone: see `property_filters`. A suffix that
        // will not parse is refused by `resolve_db_channel`, which drops the
        // group before either chain serves.
        let parse = || {
            Arc::new(
                cn.json_suffix
                    .as_deref()
                    .and_then(|json| f::try_parse_filter_chain(json).ok())
                    .unwrap_or_default(),
            )
        };
        let (value_filters, property_filters) = (parse(), parse());
        let put_field = if cn.string_view {
            format!("{}$", cn.field)
        } else {
            cn.field.clone()
        };
        Self {
            def,
            record: cn.record,
            field: cn.field,
            pv_name: RecordName::from(cn.record_path),
            string_view: cn.string_view,
            put_field,
            value_filters,
            property_filters,
        }
    }

    /// True iff this member binds a backing `dbChannel` at all.
    pub fn has_channel(&self) -> bool {
        !self.def.channel.is_empty()
    }

    /// `(record, FIELD)` as the group paths need them.
    pub fn names(&self) -> (&[u8], &str) {
        (&self.record, &self.field)
    }

    /// The field a put to this member names: `field`, `$` kept, so the
    /// database resolves the address the channel was bound with.
    pub fn put_field(&self) -> &str {
        &self.put_field
    }

    /// The address of this member's field on `rec`, `None` where no channel
    /// reads it.
    pub(super) fn addr(
        &self,
        db: &PvDatabase,
        rec: &epics_base_rs::server::record::RecordCell,
    ) -> Option<ChannelAddr> {
        ChannelAddr::of(db, rec, &self.field)
    }

    /// This member's `$` decision on its address, `None` where `$` is
    /// refused. A path that needs both the value and the NT takes it once
    /// and hands it to both.
    ///
    /// Every group path that needs a member's value reads it through this
    /// view rather than resolving [`Self::names`]'s field itself: the bare
    /// name has lost the view by then, and a path that re-resolves it serves
    /// the unviewed value under a descriptor built from the viewed one.
    pub(super) fn view_in(&self, addr: &ChannelAddr) -> Option<View> {
        View::of_addr(addr, &self.field, self.string_view, &self.value_filters)
    }

    /// The value a read of this member serves: the field's value through
    /// the member's own filter chain ([`View::read`]).
    pub(super) fn read_with(
        &self,
        instance: &epics_base_rs::server::record::RecordInstance,
        view: View,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        let raw = instance.channel_field_value(&self.field, false)?;
        Some(view.read(raw, &self.value_filters))
    }

    /// [`Self::read_with`] with the field's full metadata attached — the
    /// snapshot the `+type:"scalar"` and `+type:"meta"` mappings serve.
    ///
    /// `backing` is the caller's, not this member's: a link-backed member
    /// answers its units/precision from the LINK TARGET's record, which has
    /// to be resolved before any member read guard is taken, so the two read
    /// paths resolve it themselves (`read_group`'s `member_backings` for the
    /// atomic one, `read_member` for the non-atomic one) and hand it down.
    /// This method owns only the `$` view; it carries `backing` through
    /// untouched.
    pub(super) fn snapshot_in(
        &self,
        instance: &epics_base_rs::server::record::RecordInstance,
        view: View,
        backing: epics_base_rs::server::database::LinkBacking<'_>,
    ) -> Option<epics_base_rs::server::snapshot::Snapshot> {
        let mut snap = instance.channel_snapshot_for_field(&self.field, false, backing)?;
        // pvxs splits every read it serves (`iocsource.cpp:239-248`); rsrv
        // never does, so the record serves the raw stamp.
        epics_base_rs::server::snapshot::apply_nsec_mask(&mut snap, instance.qtime_nsec_mask());
        Some(view.read_snapshot(snap, &self.value_filters))
    }

    /// Serve a value event's `log` over this member's `snap`, as pvxs reads
    /// the triggering member through its `pDbFieldLog`
    /// (`groupsource.cpp:338-340`). The log already ran this member's chain
    /// and `$` render on the drain, and its stamp is raw.
    pub(super) fn write_log_over(
        &self,
        snap: &mut epics_base_rs::server::snapshot::Snapshot,
        log: &epics_base_rs::server::pv::MonitorEvent,
        instance: &epics_base_rs::server::record::RecordInstance,
        view: View,
    ) {
        let served = self.log_value(log, view);
        log.write_log_over(snap);
        if let Some(value) = served {
            snap.value = value;
        }
        epics_base_rs::server::snapshot::apply_nsec_mask(snap, instance.qtime_nsec_mask());
    }

    /// The value a value event's `log` serves under `view`; `None` for a
    /// log that holds none.
    pub(super) fn log_value(
        &self,
        log: &epics_base_rs::server::pv::MonitorEvent,
        view: View,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        let value = log.value().as_value()?.clone();
        Some(match view {
            View::Field => value,
            View::Dollar(dollar) | View::Text(dollar) => dollar.text(value),
        })
    }

    /// The NT this member serves — the port's `getChannelValueType` asked
    /// with this member's view, so a `$` member is the same long string on
    /// the group surface as on the single-record one.
    pub(super) fn nt_type_in(&self, addr: &ChannelAddr, view: View) -> NtType {
        pvif::nt_type_for_channel(addr, &self.field, view, &self.value_filters)
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
    pv_name: RecordName,
    record_name: RecordName,
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
    /// Access control context — checked on every get/put.
    access: super::provider::AccessContext,
}

impl BridgeChannel {
    /// Create from cached metadata (no DB introspection needed).
    pub fn from_cached(
        db: Arc<PvDatabase>,
        pv_name: RecordName,
        record_name: RecordName,
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
    /// stashes the parsed filter chain on the channel for GET.
    pub async fn new(db: Arc<PvDatabase>, name: &[u8]) -> BridgeResult<Self> {
        use epics_base_rs::server::database::filters::{ChannelCreateError, db_channel_create};
        // A NULL channel refuses the create (`dbChannel.c:513-529`).
        let value = {
            db_channel_create(&db, name).map_err(|e| match e {
                ChannelCreateError::Filter(e) => BridgeError::ChannelFilterError(e.to_string()),
                ChannelCreateError::NotFound => {
                    let cn = epics_base_rs::server::database::filters::parse_channel_name(name);
                    match db.get_record(&cn.record) {
                        None => BridgeError::RecordNotFound(cn.record.to_string()),
                        Some(_) => BridgeError::FieldNotFound {
                            record: cn.record.to_string(),
                            field: if cn.string_view {
                                format!("{}$", cn.field)
                            } else {
                                cn.field
                            },
                        },
                    }
                }
            })?
        };
        let parsed = value.name;
        let channel_filters = std::sync::Arc::new(value.chain);
        // The `$` modifier (C `dbChannel.c:486-505`) is innermost in the
        // channel name (`REC.FIELD$[range]{json}`); `split_channel_name` has
        // already peeled `{json}` and `[range]`, and the parse peels the `$`.
        let (record_name, field_upper, string_view) =
            (&parsed.record[..], parsed.field.clone(), parsed.string_view);

        let rec = db.get_record(record_name).ok_or_else(|| {
            BridgeError::RecordNotFound(String::from_utf8_lossy(record_name).into_owned())
        })?;

        // `onCreate` builds its prototype from `dbChannelFinalFieldType` and
        // `dbChannelFinalElements` (`singlesource.cpp:189-206`) without
        // locking the record, so the channel is classified from the record's
        // settled facts and never waits on a holder.
        //
        // `None` means `dbChannelCreate` would have returned NULL — the
        // record has no such field, or `$` was applied to a field that cannot
        // be re-viewed (`dbChannel.c:460-462,486-505`; pvxs renders it
        // `Invalid PV:`, `ioc/channel.cpp:29-38`). Falling through used to
        // fabricate an NTScalar double prototype the client would connect to
        // and then fail every operation against (pvxs#193, server half).
        let (addr, view) = ChannelAddr::of(&db, &rec, &field_upper)
            .and_then(|addr| {
                let view = View::of_addr(&addr, &field_upper, string_view, &channel_filters)?;
                Some((addr, view))
            })
            .ok_or_else(|| BridgeError::FieldNotFound {
                record: String::from_utf8_lossy(record_name).into_owned(),
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
        // long-string idiom. It is asked with `string_view` rather than
        // branched around, so the group path cannot reach a different answer
        // for the same channel.
        let nt_type = pvif::nt_type_for_channel(&addr, &field_upper, view, &channel_filters);

        // DBF type for the bound field: `dbChannelFinalFieldType(chan)`
        // (singlesource.cpp:189-206, dbChannel.h:452), the channel's final
        // field type after lookup, which covers `dbCommon` fields, not only
        // record-specific ones. A char address is `DBR_CHAR` whatever it
        // serves; otherwise it is the field's declared type (a menu is
        // `DBF_ENUM`, `.PROC` `DBF_UCHAR`). The filters' probe has the last
        // word: `ts` serves its numbers as `DBF_ULONG` and its text as
        // `DBF_STRING` (`ts.c:350-362`).
        let value_dbf = channel_filters.final_field_type(addr.channel_field_type(string_view));

        Ok(Self {
            db,
            pv_name: RecordName::from(name),
            record_name: parsed.record.clone(),
            field: field_upper,
            string_view,
            nt_type,
            value_dbf,
            channel_filters,
            access: super::provider::AccessContext::allow_all(),
        })
    }

    /// This channel's `$` decision, `FieldNotFound` where `$` is refused.
    fn view(&self, rec: &epics_base_rs::server::record::RecordCell) -> BridgeResult<View> {
        ChannelAddr::of(&self.db, rec, &self.field)
            .and_then(|addr| {
                View::of_addr(&addr, &self.field, self.string_view, &self.channel_filters)
            })
            .ok_or_else(|| BridgeError::FieldNotFound {
                record: self.record_name.to_string(),
                field: self.field.clone(),
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
    pub fn record_name(&self) -> &[u8] {
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

        // pvxs `SingleSource::onPut`'s decision on whether `value` is
        // marked, the owner the native PVA source routes through too; here a
        // present `value` is a marked one.
        let desc = build_field_desc_for_nt(self.nt_type, dbf_to_scalar_type(self.value_dbf));
        let value_marked = value.get_field("value").is_some();
        let defaulted;
        let value = match RecordPutKind::decide(&desc, value_marked, &opts) {
            RecordPutKind::Write if value_marked => value,
            RecordPutKind::Write => match default_value_for(&desc) {
                PvField::Structure(s) => {
                    defaulted = s;
                    &defaulted
                }
                other => unreachable!("an NT type is a structure, not {other}"),
            },
            kind => {
                return self
                    .put_unwritten(grant, kind == RecordPutKind::PostProcess, opts)
                    .await;
            }
        };

        // Extract value from the NormativeType structure
        let raw_val = pv_structure_to_epics(value).ok_or_else(|| BridgeError::TypeMismatch {
            expected: "extractable value".into(),
            got: value.struct_id.to_string(),
        })?;

        // A put through the char address goes by the `$` name, which the
        // database resolves to it.
        let mut char_address = false;
        let epics_val = if self.nt_type == NtType::Dollar(PvaDollar::Chars) {
            // The `int8_t[]` a `$` channel serves: `putArray`'s
            // `dbPut(DBR_CHAR)` through the char address.
            char_address = true;
            epics_base_rs::server::database::char_view::pva_char_request(&raw_val)
                .ok_or_else(|| BridgeError::PutRejected(format!("{}: no $ view", self.pv_name)))?
        } else if matches!(
            self.nt_type,
            NtType::LongString | NtType::LongStringField | NtType::Dollar(PvaDollar::Text)
        ) {
            // Long-string channel: the QSRV value is a scalar string
            // (`value.as<std::string>()` in pvxs, which renders a non-string
            // scalar to its textual form). Never retype it to the bound
            // `DBF_CHAR` storage — that would parse the whole string as one
            // integer and reject the PUT.
            let text = pvif::long_string_value(&raw_val);
            let rec = self
                .db
                .get_record(&self.record_name)
                .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.to_string()))?;
            // `putLongString`'s `dbPut(DBR_CHAR, str, strlen+1)` through the
            // char address — `$`, or the plain one pvxs re-views.
            char_address = self.string_view
                || ChannelAddr::of(&self.db, &rec, &self.field)
                    .is_some_and(|addr| addr.re_view().is_some());
            if char_address {
                let mut chars = text.as_bytes().to_vec();
                chars.push(0);
                EpicsValue::CharArray(chars.into())
            } else if self.value_dbf == DbFieldType::Char {
                // The field's storage IS a `DBR_CHAR` array, so this is
                // pvxs's `putLongString`: `dbPut(DBR_CHAR, str, strlen+1)`,
                // i.e. the bytes plus the NUL. Writing the char image (not
                // an `EpicsValue::String`) is what puts the record on C's
                // long-string put path — bounded by SIZV / NELM, with
                // `LEN`/`NORD` = strlen+1 — rather than the DBR_STRING path,
                // which is capped at MAX_STRING_SIZE.
                pvif::long_string_put_image(&text)
            } else {
                // pvxs `putScalar`'s DBR_STRING buffer: `strncpy` of
                // `MAX_STRING_SIZE - 1` (iocsource.cpp:499-500) before the
                // `dbPut`, whose own bound is the field's SIZV.
                let bytes = text.as_bytes();
                EpicsValue::String(epics_base_rs::types::PvString::from_bytes(
                    &bytes[..bytes.len().min(39)],
                ))
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
        // (INP/OUT/FLNK traversal with cycle tracking), the same
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
        super::trap_write::put_with_trap(grant, self.trap_meta(), epics_val, |value| async move {
            // The whole `record._options.process`/`block` decision tree —
            // link-field override included — is the database's
            // `put_field_from_client`, shared with the native PVA source so
            // the two servers cannot drift on what `process=false` or
            // `block=true` means.
            let field = if char_address {
                std::borrow::Cow::Owned(format!("{}$", self.field))
            } else {
                std::borrow::Cow::Borrowed(self.field.as_str())
            };
            self.db
                .put_field_from_client(&self.record_name, &field, value, opts.process, opts.block)
                .await
                .map_err(|e| BridgeError::PutRejected(e.to_string()))
        })
        .await
    }
}

impl BridgeChannel {
    /// A put that writes nothing: the null-array post-process, or the
    /// blocking `processRequest` that waits out a busy record. The put log
    /// names the field as it stands, which is what the record holds after.
    async fn put_unwritten(
        &self,
        grant: super::provider::WriteGrant,
        post_process: bool,
        opts: PutOptions,
    ) -> BridgeResult<()> {
        let rec = self
            .db
            .get_record(&self.record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.to_string()))?;
        let current = rec
            .read()
            .client_field_value(&self.field)
            .unwrap_or(EpicsValue::Long(0));
        super::trap_write::put_with_trap(grant, self.trap_meta(), current, |_| async move {
            let done = if post_process {
                self.db
                    .process_field_from_client(&self.record_name, &self.field, opts.process)
                    .await
            } else {
                self.db
                    .notify_unwritten_from_client(&self.record_name, &self.field)
                    .await
            };
            done.map_err(|e| BridgeError::PutRejected(e.to_string()))
        })
        .await
    }

    fn trap_meta(&self) -> super::trap_write::TrapWriteMeta<'_> {
        super::trap_write::TrapWriteMeta {
            pv_name: &self.pv_name,
            user: self.access.creds.user.as_bytes(),
            host: &self.access.creds.host,
            peer: &self.access.creds.host,
            dbr_type: self.value_dbf as u16,
        }
    }
}

impl Channel for BridgeChannel {
    fn channel_name(&self) -> &[u8] {
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
            .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.to_string()))?;

        // Through the database, which resolves a link-backed field's metadata
        // from the LINK TARGET's record — a second lock this holds none of.
        let mut snapshot = self
            .db
            .channel_snapshot_for_field(&rec, &self.field, false)
            .ok_or_else(|| BridgeError::FieldNotFound {
                record: self.record_name.to_string(),
                field: self.field.clone(),
            })?;
        // pvxs's `Q:time:tag` split (`iocsource.cpp:239-248`).
        epics_base_rs::server::snapshot::apply_nsec_mask(&mut snapshot, rec.qtime_nsec_mask());

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
        let snapshot = self
            .view(&rec)?
            .read_snapshot(snapshot, &self.channel_filters);

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
        let ch = BridgeChannel::new(db, br#"REC.{"arr":{s:2,i:2,e:8}}"#)
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
        let res = BridgeChannel::new(db, br#"REC.{"no_such":{}}"#).await;
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
        let ch = BridgeChannel::new(db.clone(), br#"REC.VAL{"dbnd":{"d":10.0}}"#)
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
        let res = BridgeChannel::new(db, br#"REC.{not json}"#).await;
        assert!(matches!(res, Err(BridgeError::ChannelFilterError(_))));
    }

    /// An invalid `dec` body (missing required `n`) is a hard reject,
    /// matching `chf_value` / `parse_end` failure → `parse_stop`.
    #[tokio::test]
    async fn channel_filter_rejects_invalid_dec_body() {
        let db = db_with_rec().await;
        let res = BridgeChannel::new(db, br#"REC.{"dec":{}}"#).await;
        assert!(matches!(res, Err(BridgeError::ChannelFilterError(_))));
    }

    /// A plain (unfiltered) channel still creates with an empty chain —
    /// the reject path must not regress the no-suffix common case.
    #[tokio::test]
    async fn channel_without_filter_suffix_still_creates() {
        let db = db_with_rec().await;
        let ch = BridgeChannel::new(db, b"REC")
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
        let res = BridgeChannel::new(db, b"REC.NOSUCH").await;
        assert!(matches!(res, Err(BridgeError::FieldNotFound { .. })));
    }

    // ---- Force + block (record[process=true,block=true]) end-to-end wiring ----

    /// End-to-end proof that `put_with_options(Force, block)` routes a
    /// *synchronous* put through the put-notify barrier and returns only
    /// after the full processing cycle — including the OUT link write — has
    /// run.
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
        sc.special("CALC", true, &Default::default()).unwrap();
        sc.oopt = 0;
        sc.put_field("ODLY", EpicsValue::Double(0.0)).unwrap();
        sc.put_field("OUT", EpicsValue::String("TGT0".into()))
            .unwrap();
        db.add_record("SC0", Box::new(sc)).await.unwrap();

        // `A` is pp(TRUE): only a pp field or PROC processes under a
        // blocking put (`dbNotify.c:243-247`).
        let ch = BridgeChannel::new(db.clone(), b"SC0.A")
            .await
            .expect("channel over scalcout A must create");
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
        sc.special("CALC", true, &Default::default()).unwrap();
        sc.oopt = 0;
        sc.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
        sc.put_field("OUT", EpicsValue::String("TGT1".into()))
            .unwrap();
        db.add_record("SC1", Box::new(sc)).await.unwrap();

        // `A` is pp(TRUE): only a pp field or PROC processes under a
        // blocking put (`dbNotify.c:243-247`).
        let ch = BridgeChannel::new(db.clone(), b"SC1.A")
            .await
            .expect("channel over scalcout A must create");
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
            Some(EpicsValue::UShort(1)),
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
        sc.special("CALC", true, &Default::default()).unwrap();
        sc.oopt = 0;
        sc.put_field("ODLY", EpicsValue::Double(0.05)).unwrap();
        sc.put_field("OUT", EpicsValue::String("TGT2".into()))
            .unwrap();
        db.add_record("SC2", Box::new(sc)).await.unwrap();

        // `A` is pp(TRUE): only a pp field or PROC processes under a
        // blocking put (`dbNotify.c:243-247`).
        let ch = BridgeChannel::new(db.clone(), b"SC2.A")
            .await
            .expect("channel over scalcout A must create");
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
            async fn can_write(&self, _: &[u8], _: &[u8], _: &str) -> bool {
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
        let denied = BridgeChannel::new(db.clone(), b"PS:ai")
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
        let ch = BridgeChannel::new(db.clone(), b"PS:ai").await.unwrap();
        let err = ch.put(&put).await.expect_err("DISP=1 must reject the put");
        assert_eq!(
            super::super::put_status::wire_message(&err),
            "Unable to put value: Field Disabled: S_db_putDisabled"
        );

        // Read-only (SPC_NOMOD) field on the SAME DISP=1 record → noMod wins,
        // because C tests `special == SPC_ATTRIBUTE` first.
        let ro = BridgeChannel::new(db.clone(), b"PS:ai.AFVL").await.unwrap();
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

        let m = member(r#"GRP:AI.{"arr":{"s":0}}"#);
        assert_eq!(
            m.names(),
            (&b"GRP:AI"[..], "VAL"),
            "the suffix is peeled first"
        );
        assert_eq!(m.value_filters.len(), 1);
        assert_eq!(m.property_filters.len(), 1);
        resolve_db_channel(&db, &m).expect("a parseable filter suffix is admitted");
        resolve_db_channel(&db, &member("GRP:AI")).expect("the unfiltered member still resolves");
    }

    /// The errlog lines `f` produced.
    async fn errlog_of(f: impl std::future::Future<Output = ()>) -> Vec<String> {
        let heard = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = heard.clone();
        let id = epics_base_rs::runtime::log::errlog_add_listener(move |m| {
            sink.lock().unwrap().push(m.to_string());
        });
        f.await;
        epics_base_rs::runtime::log::errlog_flush();
        epics_base_rs::runtime::log::errlog_remove_listener(id);
        heard.lock().unwrap().clone()
    }

    const BAD_FILTER_LINE: &str = "dbChannelCreate: Channel filter 'bad' not found";

    /// softIocPVX prints `dbChannelCreate: Channel filter 'bad' not found`
    /// once per create of `Y.VAL{"bad":{}}` and refuses it.
    #[tokio::test]
    async fn a_single_channel_with_an_unknown_filter_prints_the_line_once() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = Arc::new(PvDatabase::new());
        db.add_record("Y", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();
        let lines = errlog_of(async {
            assert!(
                BridgeChannel::new(db.clone(), br#"Y.VAL{"bad":{}}"#)
                    .await
                    .is_err()
            );
        })
        .await;
        let n = lines.iter().filter(|l| l.contains(BAD_FILTER_LINE)).count();
        assert_eq!(n, 1, "{lines:?}");
    }

    // ---- unmarked value: pvxs `SingleSource::onPut` (singlesource.cpp:347-383) ----

    async fn ioc_db(text: &str) -> Arc<PvDatabase> {
        let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
            .db_string(text, &std::collections::HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        db
    }

    /// A record that has never processed still carries the EPICS epoch in
    /// `TIME`; a processing cycle stamps it from the wall clock.
    fn has_processed(db: &PvDatabase, record: &str) -> bool {
        db.get_record(record)
            .unwrap()
            .read()
            .common
            .time
            .unix_secs()
            > 700_000_000
    }

    fn put_options(block: bool) -> PutOptions {
        PutOptions {
            process: ProcessMode::Passive,
            block,
        }
    }

    /// A non-blocking put that marks no scalar `value` writes the type
    /// default, as pvxs writes a fresh unmarked value.
    #[tokio::test]
    async fn unmarked_scalar_put_writes_the_default() {
        let db = ioc_db(r#"record(ao, "AO") { field(VAL, "5") }"#).await;
        let ch = BridgeChannel::new(db.clone(), b"AO").await.unwrap();
        ch.put_with_options(
            &PvStructure::new("epics:nt/NTScalar:1.0"),
            put_options(false),
        )
        .await
        .expect("an unmarked put is not a type mismatch");
        let val = db.get_record("AO").unwrap().read().resolve_field("VAL");
        assert_eq!(val.and_then(|v| v.to_f64()), Some(0.0));
        assert!(has_processed(&db, "AO"));
    }

    /// A blocking unmarked put is `processRequest`, which on a PACT record
    /// replies only once the cycle ends (`dbNotify.c:225-231`).
    #[tokio::test]
    async fn blocking_unmarked_put_on_a_busy_record_waits_for_it() {
        let db = ioc_db(r#"record(calcout, "CO") { field(ODLY, "0.3") field(CALC, "1") }"#).await;
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links("CO", &mut visited)
            .await
            .unwrap();
        let busy = || db.get_record("CO").unwrap().read().is_processing();
        assert!(busy(), "ODLY holds the record PACT");
        let ch = BridgeChannel::new(db.clone(), b"CO").await.unwrap();
        ch.put_with_options(
            &PvStructure::new("epics:nt/NTScalar:1.0"),
            put_options(true),
        )
        .await
        .expect("PUT must succeed");
        assert!(!busy(), "the reply came while the record was still PACT");
    }

    /// An unmarked array is dropped before any `dbPut`
    /// (`iocsource.cpp:542-543`) and the record still processes.
    #[tokio::test]
    async fn unmarked_array_put_writes_nothing_and_processes() {
        let db =
            ioc_db(r#"record(waveform, "WF") { field(FTVL, "DOUBLE") field(NELM, "4") }"#).await;
        db.put_pv("WF", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()))
            .await
            .unwrap();
        let ch = BridgeChannel::new(db.clone(), b"WF").await.unwrap();
        ch.put_with_options(
            &PvStructure::new("epics:nt/NTScalarArray:1.0"),
            put_options(false),
        )
        .await
        .expect("PUT must succeed");
        let val = db.get_record("WF").unwrap().read().resolve_field("VAL");
        assert_eq!(val.map(|v| v.count()), Some(3), "elements untouched");
        assert!(has_processed(&db, "WF"));
    }

    // RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
    // suite.

    /// An unknown filter drops the group. softIocPVX on
    /// `info(Q:group, {"G":{"v":{+channel:"VAL{\"bad\":{}}"}}})`:
    ///
    /// ```text
    /// dbChannelCreate: Channel filter 'bad' not found
    /// G: Error Group not created: Invalid PV: Y.VAL{"bad":{}}
    /// ```
    #[tokio::test]
    async fn an_unknown_group_member_filter_drops_the_group() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = PvDatabase::new();
        db.add_record("Y", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let m = member(r#"Y.VAL{"bad":{}}"#);
        assert!(m.value_filters.is_empty(), "no chain is served");
        let mut err = None;
        let lines = errlog_of(async {
            err = resolve_db_channel(&db, &m).err();
        })
        .await;
        assert_eq!(err.as_deref(), Some(r#"Invalid PV: Y.VAL{"bad":{}}"#));
        let n = lines.iter().filter(|l| l.contains(BAD_FILTER_LINE)).count();
        assert_eq!(n, 1, "{lines:?}");
    }

    /// The member split is `dbChannelCreate`'s order — suffix first, then
    /// the last `.` (`dbChannel.c:448-532`). A bare `rsplit_once('.')`
    /// tears the JSON apart at its own decimal point, which is what the
    /// twelve former call sites would have done once the refusal lifted.
    #[test]
    fn member_channel_peels_the_suffix_before_splitting() {
        assert_eq!(
            member(r#"REC.VAL{"dbnd":{"d":0.5}}"#).names(),
            (&b"REC"[..], "VAL")
        );
        assert_eq!(member("REC.VAL[1:3]").names(), (&b"REC"[..], "VAL"));
        assert_eq!(member("REC.VAL$").names(), (&b"REC"[..], "VAL"));
        assert!(member("REC.VAL$").string_view);
        // the ordinary cases the old split already got right
        assert_eq!(member("REC.egu").names(), (&b"REC"[..], "EGU"));
        assert_eq!(member("REC").names(), (&b"REC"[..], "VAL"));
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

    /// Every base record type, with the char and long-string shapes pvxs
    /// reshapes and a one-element buffer of each kind.
    const EVERY_TYPE: &str = r#"
record(ai, "ai") {}
record(bi, "bi") { field(ZNAM, "z") field(ONAM, "o") }
record(stringin, "stringin") { info(Q:form, "String") }
record(stringout, "stringout") {}
record(longin, "longin") {}
record(int64in, "int64in") {}
record(lsi, "lsi") { field(SIZV, "100") }
record(lso, "lso") { field(SIZV, "20") info(Q:form, "String") }
record(lsi, "lsi41") { field(SIZV, "41") }
record(mbbo, "mbbo") { field(ZRST, "zero") field(ONST, "one") }
record(mbbiDirect, "mbbiDirect") {}
record(event, "event") {}
record(printf, "printf") { field(SIZV, "60") }
record(waveform, "wfS") { field(FTVL, "STRING") field(NELM, "4") }
record(waveform, "wfC") { field(FTVL, "CHAR") field(NELM, "16") info(Q:form, "String") }
record(waveform, "wfC1") { field(FTVL, "CHAR") field(NELM, "1") info(Q:form, "String") }
record(waveform, "wfUC") { field(FTVL, "UCHAR") field(NELM, "16") }
record(waveform, "wfD") { field(FTVL, "DOUBLE") field(NELM, "8") }
record(waveform, "wfE") { field(FTVL, "ENUM") field(NELM, "1") }
record(aai, "aai") { field(FTVL, "STRING") field(NELM, "2") }
record(aai, "aai1") { field(FTVL, "DOUBLE") field(NELM, "1") }
record(subArray, "subArray") { field(FTVL, "FLOAT") field(MALM, "10") field(NELM, "4") }
record(calc, "calc") { field(CALC, "A+1") info(Q:form, "String") }
record(seq, "seq") {}
record(compress, "compress") { field(NSAM, "6") }
record(histogram, "histogram") { field(NELM, "5") }
record(aSub, "aSub") {
    field(FTA, "STRING") field(NOA, "4")
    field(FTB, "CHAR") field(NOB, "10")
    field(FTVA, "DOUBLE") field(NOVA, "3")
}
record(state, "state") {}
record(ai, "linked") { field(INP, "ai.VAL CP") info(Q:form, "String") }
"#;

    async fn every_type() -> Arc<PvDatabase> {
        let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
            .db_string(EVERY_TYPE, &std::collections::HashMap::new())
            .unwrap()
            .build()
            .await
            .unwrap();
        db.ioc_init().await;
        db
    }

    /// A group with a `scalar`, a `plain` and an `any` member on `channel`.
    fn group_on(channel: &str) -> super::super::group_config::GroupPvDef {
        let chan = channel.replace('"', "\\\"");
        let json = format!(
            r#"{{"G:x": {{"s": {{"+channel": "{chan}", "+type": "scalar"}}, "p": {{"+channel": "{chan}", "+type": "plain"}}, "a": {{"+channel": "{chan}", "+type": "any"}}}}}}"#
        );
        super::super::group_config::parse_group_config(&json)
            .unwrap()
            .pop()
            .unwrap()
    }

    /// pvxs makes a single-record channel (`singlesource.cpp:189-206`) and
    /// a group member's type (`groupconfigprocessor.cpp:867-974`) without
    /// locking the record, so neither waits on another thread's hold.
    #[epics_base_rs::epics_test]
    async fn a_held_record_gets_a_channel_without_waiting_for_the_hold() {
        use super::super::group::GroupChannel;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let db = every_type().await;
        let h = db.async_handle();
        let (held_tx, held_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            h.with_record("ai", |_| {
                h.with_record("lso", |_| {
                    h.with_record("wfC", |_| {
                        held_tx.send(()).unwrap();
                        go_rx.recv().unwrap();
                    })
                })
            })
        });
        held_rx.recv().unwrap();
        let make = |name: &'static str| {
            let db = db.clone();
            std::thread::spawn(move || {
                epics_base_rs::runtime::task::test_block_on(async move {
                    let single = BridgeChannel::new(db.clone(), name.as_bytes())
                        .await
                        .map(|ch| ch.nt_type());
                    let def = group_on(name);
                    let gate = def
                        .channels
                        .iter()
                        .all(|m| resolve_db_channel(&db, m).is_ok());
                    let group = GroupChannel::new(db, def).get_field().await.is_ok();
                    (single.ok(), gate && group)
                })
            })
        };
        let made = [make("ai"), make("lso.VAL"), make("wfC"), make("lso.VAL$")];
        let deadline = Instant::now() + Duration::from_secs(10);
        while !made.iter().all(|t| t.is_finished()) {
            assert!(
                Instant::now() < deadline,
                "channel creation waited for the hold"
            );
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        go_tx.send(()).unwrap();
        holder.join().unwrap();
        let made: Vec<_> = made.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            made,
            [
                (Some(NtType::Scalar), true),
                (Some(NtType::LongStringField), true),
                (Some(NtType::LongString), true),
                (Some(NtType::Dollar(PvaDollar::Text)), true),
            ]
        );
    }

    /// `served` is a value of the type `desc` advertises, as far as the
    /// value leaf: an NT's id and value, a leaf's type.
    fn serves(desc: &FieldDesc, served: &PvField) -> bool {
        match (desc, served) {
            (FieldDesc::Scalar(t), PvField::Scalar(v)) => *t == v.scalar_type(),
            (FieldDesc::ScalarArray(t), PvField::ScalarArray(a)) => {
                a.first().is_none_or(|v| v.scalar_type() == *t)
            }
            (FieldDesc::ScalarArray(t), PvField::ScalarArrayTyped(a)) => a.scalar_type() == *t,
            (FieldDesc::Variant, PvField::Variant(_)) => true,
            (FieldDesc::Structure { struct_id, fields }, PvField::Structure(s)) => {
                *struct_id == s.struct_id
                    && fields.iter().all(|(name, desc)| {
                        !matches!(&name[..], "value" | "index" | "choices" | "s" | "p" | "a")
                            || s.get_field(name).is_some_and(|v| serves(desc, v))
                    })
            }
            _ => false,
        }
    }

    /// The type a channel and a group member take with no lock is the type
    /// a read under the lock serves: every field of every base type, with
    /// `$` and without, through each filter shape. A char view read through
    /// a filter chain is left out: its read serves a type of its own, as it
    /// did when the create read under the lock.
    #[epics_base_rs::epics_test]
    async fn the_lock_free_type_is_the_one_read_under_the_lock() {
        use super::super::group::GroupChannel;
        use epics_base_rs::server::record::FieldDeclaration;
        let db = every_type().await;
        let chains = [
            "",
            r#"{"arr":{"s":1,"e":2}}"#,
            r#"{"arr":{"s":0,"e":0}}"#,
            r#"{"ts":{"num":"dbl"}}"#,
            r#"{"ts":{"num":"ts"}}"#,
            r#"{"ts":{"str":"epics"}}"#,
        ];
        let names = EVERY_TYPE
            .lines()
            .filter(|l| l.starts_with("record("))
            .filter_map(|l| l.split('"').nth(1));
        let mut differ = Vec::new();
        let mut checked = 0;
        for rec in names {
            let fields = db.get_record(rec).unwrap().read().record.field_list();
            let fields = fields
                .iter()
                .chain(epics_base_rs::server::record::dbd_generated::DB_COMMON_FIELDS)
                .map(|f| f.name)
                .chain(["RTYP", "NOPE"]);
            let cell = db.get_record(rec).unwrap();
            for field in fields {
                let re_view =
                    ChannelAddr::of(&db, &cell, field).is_some_and(|a| a.re_view().is_some());
                for dollar in ["", "$"] {
                    for chain in chains {
                        if (re_view || !dollar.is_empty()) && !chain.is_empty() {
                            continue;
                        }
                        let name = format!("{rec}.{field}{dollar}{chain}");
                        let Ok(ch) = BridgeChannel::new(db.clone(), name.as_bytes()).await else {
                            continue;
                        };
                        let desc = ch.get_field().await.unwrap();
                        let served = ch.get(&PvStructure::new("")).await.unwrap();
                        if !serves(&desc, &PvField::Structure(served.clone())) {
                            differ.push(format!("{name}: {desc:?} serves {served:?}"));
                        }
                        let group = GroupChannel::new(db.clone(), group_on(&name));
                        let desc = group.get_field().await.unwrap();
                        let served = group.get(&PvStructure::new("")).await.unwrap();
                        if !serves(&desc, &PvField::Structure(served.clone())) {
                            differ.push(format!("group {name}: {desc:?} serves {served:?}"));
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 1000, "{checked}");
        assert!(differ.is_empty(), "{}", differ.join("\n"));
    }

    /// A one-element enum buffer is pvxs's NTEnum (`singlesource.cpp:200`),
    /// whose index is the buffer's first element (`iocsource.cpp:108`).
    #[epics_base_rs::epics_test]
    async fn a_one_element_enum_buffer_serves_its_first_element_as_the_index() {
        let db = every_type().await;
        db.put_record_field_from_ca_no_notify("wfE", "VAL", EpicsValue::EnumArray(vec![3].into()))
            .await
            .unwrap();
        let ch = BridgeChannel::new(db, b"wfE").await.unwrap();
        assert_eq!(ch.nt_type(), NtType::Enum);
        let served = ch.get(&PvStructure::new("")).await.unwrap();
        let Some(PvField::Structure(value)) = served.get_field("value") else {
            panic!("{served:?}");
        };
        assert_eq!(
            value.get_field("index"),
            Some(&PvField::Scalar(ScalarValue::Int(3)))
        );
    }
}
