//! GroupChannel and GroupMonitor: multi-record composite PVA channel.
//!
//! Corresponds to C++ QSRV's `PDBGroupPV` / `PDBGroupChannel` / `PDBGroupMonitor`.
//! A group PV combines fields from multiple EPICS database records
//! into a single PvStructure.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::types::DbFieldType;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, VariantValue};

use super::group_config::{GroupMember, GroupPvDef, TriggerDef};
use super::monitor::BridgeMonitor;
use super::pvif::{self, FieldMapping, NtType};
use crate::convert::{dbf_to_scalar_type, epics_to_pv_field};
use crate::error::{BridgeError, BridgeResult};

// ---------------------------------------------------------------------------
// FieldName — path parser with array index support (pvxs fieldname.h)
// ---------------------------------------------------------------------------

/// A single component in a field path: `name` with optional `[index]`.
#[derive(Debug, Clone, PartialEq)]
struct FieldNameComponent {
    name: String,
    index: Option<u32>,
}

/// Parse a field path like `"a.b[0].c"` into components.
///
/// Corresponds to C++ QSRV `FieldName` (fieldname.cpp:30-66).
/// Empty components from trailing/leading/double dots are filtered out,
/// matching pvxs validation (fieldname.cpp:35-36).
fn parse_field_path(path: &str) -> Vec<FieldNameComponent> {
    if path.is_empty() {
        return Vec::new();
    }

    path.split('.')
        .filter(|s| !s.is_empty())
        .map(|part| {
            if let Some(bracket) = part.find('[') {
                let name = part[..bracket].to_string();
                let rest = &part[bracket + 1..];
                let index = rest.strip_suffix(']').and_then(|s| s.parse::<u32>().ok());
                FieldNameComponent { name, index }
            } else {
                FieldNameComponent {
                    name: part.to_string(),
                    index: None,
                }
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Nested field path support
// ---------------------------------------------------------------------------

/// Navigate a field path (e.g., `"a.b[0].c"`) within a PvStructure,
/// returning the leaf [`PvField`]. Supports array indexing via `[N]`.
///
/// Plain (non-indexed) paths borrow into the input — no allocation. Indexed
/// terminals (`field[N]` where `field` is a `ScalarArray`) clone the
/// element into a fresh `PvField::Scalar` and return `Cow::Owned`, so
/// callers see a single PV scalar value rather than the whole array.
///
/// Corresponds to C++ QSRV `FieldName` + `Field::findIn`.
pub fn get_nested_field<'a>(pv: &'a PvStructure, path: &str) -> Option<Cow<'a, PvField>> {
    let components = parse_field_path(path);
    if components.is_empty() {
        return None;
    }

    let mut current_struct = pv;
    for (i, comp) in components.iter().enumerate() {
        let field = current_struct.get_field(&comp.name)?;
        let is_last = i == components.len() - 1;

        if let Some(idx) = comp.index {
            // Indexed terminal `field[N]`: extract element N as a fresh
            // PvField. ScalarArray → PvField::Scalar, StructureArray →
            // PvField::Structure. Anything else fails.
            if !is_last {
                // Mid-path index (`field[N].child`) requires a
                // StructureArray. Index into the Vec<PvStructure> and
                // continue navigating.
                if let PvField::StructureArray(items) = field {
                    // a null (`None`) element has no struct to
                    // navigate into — treat as not-found.
                    let element = items.get(idx as usize)?.as_ref()?;
                    current_struct = element;
                    continue;
                }
                return None;
            }
            // Terminal index.
            return match field {
                PvField::ScalarArray(arr) => {
                    let sv = arr.get(idx as usize)?.clone();
                    Some(Cow::Owned(PvField::Scalar(sv)))
                }
                PvField::StructureArray(items) => {
                    // A null element resolves to no value.
                    let element = items.get(idx as usize)?.clone()?;
                    Some(Cow::Owned(PvField::Structure(element)))
                }
                _ => None,
            };
        }

        if is_last {
            return Some(Cow::Borrowed(field));
        }
        match field {
            PvField::Structure(s) => current_struct = s,
            _ => return None,
        }
    }
    None
}

/// pvxs default monitor queue depth for groups — pvxs
/// `MonitorOp::limit` defaults to `4u` (`servermon.cpp:66`), and a
/// `record._options.queueSize` that fails to parse or is `< 2` leaves
/// that default in place (`servermon.cpp:533-540`). Used for the GET
/// path (no queue to negotiate) and as the monitor fallback when the
/// MONITOR INIT pvRequest carries no usable `queueSize`.
pub const GROUP_DEFAULT_QUEUE_SIZE: i32 = 4;

/// resolve the *negotiated* monitor queue size from a
/// MONITOR INIT pvRequest's `record._options.queueSize`.
///
/// pvxs `servermon.cpp:533-540`: `uint32_t qSize = op->limit;` then
/// `op->limit = qSize` only when `queueSize` parses AND `qSize >= 2`;
/// otherwise the default `op->limit` (4) is kept. The group source
/// then stamps `stats.limitQueue` (= `op->limit`) into the monitor
/// value (`groupsource.cpp:359`). This mirrors that negotiation so a
/// group monitor reports the queue depth the client actually
/// requested, not a hardcoded constant.
pub fn negotiated_queue_size(pv_request: &PvStructure) -> i32 {
    use epics_pva_rs::pvdata::ScalarValue;
    let parsed = pv_request
        .fields
        .iter()
        .find_map(|(k, v)| (k == "record").then_some(v))
        .and_then(|v| match v {
            PvField::Structure(s) => Some(s),
            _ => None,
        })
        .and_then(|rec| {
            rec.fields
                .iter()
                .find_map(|(k, v)| (k == "_options").then_some(v))
        })
        .and_then(|v| match v {
            PvField::Structure(s) => Some(s),
            _ => None,
        })
        .and_then(|opt| {
            opt.fields
                .iter()
                .find_map(|(k, v)| (k == "queueSize").then_some(v))
        })
        .and_then(|v| match v {
            // Same scalar shapes the native PVA server accepts in
            // `monitor_pipeline_options` — typed-builder INT/UINT/…
            // and the `record[queueSize=N]` STRING form.
            PvField::Scalar(ScalarValue::String(s)) => s.parse::<i32>().ok(),
            PvField::Scalar(ScalarValue::Byte(i)) => Some(i32::from(*i)),
            PvField::Scalar(ScalarValue::UByte(i)) => Some(i32::from(*i)),
            PvField::Scalar(ScalarValue::Short(i)) => Some(i32::from(*i)),
            PvField::Scalar(ScalarValue::UShort(i)) => Some(i32::from(*i)),
            PvField::Scalar(ScalarValue::Int(i)) => Some(*i),
            PvField::Scalar(ScalarValue::UInt(i)) => i32::try_from(*i).ok(),
            PvField::Scalar(ScalarValue::Long(l)) => i32::try_from(*l).ok(),
            PvField::Scalar(ScalarValue::ULong(l)) => i32::try_from(*l).ok(),
            _ => None,
        });
    // pvxs keeps the default unless the request value is >= 2.
    match parsed {
        Some(n) if n >= 2 => n,
        _ => GROUP_DEFAULT_QUEUE_SIZE,
    }
}

/// stamp `record._options.queueSize` (int) and
/// `record._options.atomic` (boolean) onto a group GET / MONITOR
/// value. Adds them at the root, replacing the previous values if
/// `_options` already exists (e.g. composed by an earlier read).
///
/// `queue_size` is the negotiated monitor queue depth (see
/// [`negotiated_queue_size`]) on the MONITOR path; the GET path passes
/// `0` — pvxs's GET stamps the value-template default and never a queue
/// depth (groupsource.cpp:480-485, test/testqgroup.cpp:60-66).
pub fn push_record_options(pv: &mut PvStructure, atomic: bool, queue_size: i32) {
    use epics_pva_rs::pvdata::ScalarValue;
    let mut options = PvStructure::new("");
    options.fields.push((
        "queueSize".into(),
        PvField::Scalar(ScalarValue::Int(queue_size)),
    ));
    options.fields.push((
        "atomic".into(),
        PvField::Scalar(ScalarValue::Boolean(atomic)),
    ));
    let mut record = PvStructure::new("");
    record
        .fields
        .push(("_options".into(), PvField::Structure(options)));
    let record_field = PvField::Structure(record);
    if let Some(pos) = pv.fields.iter().position(|(n, _)| n == "record") {
        pv.fields[pos].1 = record_field;
    } else {
        pv.fields.push(("record".into(), record_field));
    }
}

/// Descriptor twin of [`push_record_options`]: the introspection shape
/// of the built-in `record._options` subtree (`queueSize` int,
/// `atomic` boolean). pvxs builds this branch into `group.valueTemplate`
/// (ioc/groupconfigprocessor.cpp:499-523), so CREATE_CHANNEL / GET_FIELD
/// negotiation advertises it and every GET/MONITOR value conforms.
/// Keep the field names and scalar types here in lockstep with
/// `push_record_options` so the descriptor never diverges from the value.
fn record_options_field_desc() -> FieldDesc {
    let options = FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![
            ("queueSize".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("atomic".into(), FieldDesc::Scalar(ScalarType::Boolean)),
        ],
    };
    FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![("_options".into(), options)],
    }
}

/// place a member's resolved value into the group structure.
///
/// pvxs allows a `+type:"meta"` member with an empty key (`""`) to
/// merge its `alarm` / `timeStamp` sub-fields into the *root* of the
/// group structure (test/ntenum.db:6). The earlier Rust path routed
/// every member through `set_nested_field`, which silently no-oped on
/// empty paths and dropped the root meta entirely. This helper handles
/// the empty-path Meta case by flattening the meta sub-structure into
/// the root `pv` before falling through to the normal nested-path
/// setter for every other shape.
pub fn set_member_field(pv: &mut PvStructure, member: &GroupMember, value: PvField) {
    use crate::qsrv::FieldMapping;

    if member.field_name.is_empty() && member.mapping == FieldMapping::Meta {
        // Meta builds `{alarm, timeStamp}` — merge each sub-leaf onto
        // the root, overwriting if a previous member already placed
        // a field of the same name.
        if let PvField::Structure(meta) = value {
            for (name, sub) in meta.fields {
                if let Some(pos) = pv.fields.iter().position(|(n, _)| n == &name) {
                    pv.fields[pos].1 = sub;
                } else {
                    pv.fields.push((name, sub));
                }
            }
        }
        return;
    }
    set_nested_field(pv, &member.field_name, value);
}

/// Set a value at a field path within a PvStructure.
/// Creates intermediate structures as needed. Supports `[N]` notation.
pub fn set_nested_field(pv: &mut PvStructure, path: &str, value: PvField) {
    let components = parse_field_path(path);
    if components.is_empty() {
        return;
    }

    set_nested_field_recursive(pv, &components, value);
}

fn set_nested_field_recursive(
    pv: &mut PvStructure,
    components: &[FieldNameComponent],
    value: PvField,
) {
    if components.is_empty() {
        return;
    }

    let comp = &components[0];

    if components.len() == 1 && comp.index.is_none() {
        // Leaf: direct field set
        if let Some(pos) = pv.fields.iter().position(|(n, _)| n == &comp.name) {
            pv.fields[pos].1 = value;
        } else {
            pv.fields.push((comp.name.clone(), value));
        }
        return;
    }

    // Navigate/create the intermediate structure
    let sub = get_or_create_struct_field(pv, &comp.name);

    // If this component has an array index, we don't currently support
    // structure arrays in PvField. Skip the index and navigate as if
    // it were a plain structure (matches current epics-rs PvField limitation).
    set_nested_field_recursive(sub, &components[1..], value);
}

/// Find or create a named sub-structure within `pv`.
fn get_or_create_struct_field<'a>(pv: &'a mut PvStructure, name: &str) -> &'a mut PvStructure {
    let pos = pv.fields.iter().position(|(n, _)| n == name);

    if let Some(pos) = pos {
        if !matches!(pv.fields[pos].1, PvField::Structure(_)) {
            pv.fields[pos].1 = PvField::Structure(PvStructure::new(""));
        }
        if let PvField::Structure(ref mut s) = pv.fields[pos].1 {
            s
        } else {
            unreachable!()
        }
    } else {
        pv.fields
            .push((name.to_string(), PvField::Structure(PvStructure::new(""))));
        if let PvField::Structure(ref mut s) = pv.fields.last_mut().unwrap().1 {
            s
        } else {
            unreachable!()
        }
    }
}

/// Counterpart of [`set_member_field`] for the descriptor
/// builder. An empty-path Meta member flattens its `{alarm,
/// timeStamp}` sub-descriptors onto the group root.
pub fn set_member_field_desc(
    fields: &mut Vec<(String, FieldDesc)>,
    member: &GroupMember,
    leaf: FieldDesc,
) {
    use crate::qsrv::FieldMapping;

    if member.field_name.is_empty() && member.mapping == FieldMapping::Meta {
        if let FieldDesc::Structure {
            fields: meta_fields,
            ..
        } = leaf
        {
            for (name, sub) in meta_fields {
                if let Some(pos) = fields.iter().position(|(n, _)| n == &name) {
                    fields[pos].1 = sub;
                } else {
                    fields.push((name, sub));
                }
            }
        }
        return;
    }
    set_nested_field_desc(fields, &member.field_name, leaf);
}

/// Insert a nested FieldDesc at a field path (supports `[N]` notation).
///
/// Counterpart of [`set_nested_field`] for type introspection. Builds
/// intermediate `Structure` descriptors as needed so the advertised
/// schema matches the runtime payload shape.
pub fn set_nested_field_desc(fields: &mut Vec<(String, FieldDesc)>, path: &str, leaf: FieldDesc) {
    let components = parse_field_path(path);
    if components.is_empty() {
        return;
    }
    set_nested_field_desc_recursive(fields, &components, leaf);
}

fn set_nested_field_desc_recursive(
    fields: &mut Vec<(String, FieldDesc)>,
    components: &[FieldNameComponent],
    leaf: FieldDesc,
) {
    if components.is_empty() {
        return;
    }

    let comp = &components[0];

    if components.len() == 1 && comp.index.is_none() {
        if let Some(pos) = fields.iter().position(|(n, _)| n == &comp.name) {
            fields[pos].1 = leaf;
        } else {
            fields.push((comp.name.clone(), leaf));
        }
        return;
    }

    // Find or create the intermediate structure descriptor
    let sub_fields: &mut Vec<(String, FieldDesc)> =
        if let Some(pos) = fields.iter().position(|(n, _)| n == &comp.name) {
            match &mut fields[pos].1 {
                FieldDesc::Structure { fields: f, .. } => f,
                other => {
                    *other = FieldDesc::Structure {
                        struct_id: String::new(),
                        fields: Vec::new(),
                    };
                    if let FieldDesc::Structure { fields: f, .. } = &mut fields[pos].1 {
                        f
                    } else {
                        unreachable!()
                    }
                }
            }
        } else {
            fields.push((
                comp.name.clone(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: Vec::new(),
                },
            ));
            if let FieldDesc::Structure { fields: f, .. } = &mut fields.last_mut().unwrap().1 {
                f
            } else {
                unreachable!()
            }
        };

    set_nested_field_desc_recursive(sub_fields, &components[1..], leaf);
}

// ---------------------------------------------------------------------------
// Atomic multi-record locking (pvxs DBManyLocker equivalent)
// ---------------------------------------------------------------------------

/// Acquire read locks on all records backing a group's members, in sorted
/// order to prevent deadlocks. Corresponds to C++ QSRV `DBManyLocker`
/// (dbmanylocker.h). Returns guards that hold the locks.
async fn lock_group_records_read(
    db: &PvDatabase,
    members: &[GroupMember],
) -> Vec<(
    String,
    tokio::sync::OwnedRwLockReadGuard<epics_base_rs::server::record::RecordInstance>,
)> {
    // Collect unique record names and sort for deterministic lock order.
    let mut record_names: Vec<String> = members
        .iter()
        .filter(|m| !m.channel.is_empty())
        .map(|m| {
            let (rec, _) = epics_base_rs::server::database::parse_pv_name(&m.channel);
            rec.to_string()
        })
        .collect();
    record_names.sort();
    record_names.dedup();

    let mut guards = Vec::new();
    for name in &record_names {
        if let Some(rec) = db.get_record(name).await {
            guards.push((name.clone(), rec.read_owned().await));
        }
    }
    guards
}

/// collect the **canonical** record names backing a group's
/// writable members, for the `DBManyLock`-equivalent write gate.
///
/// pvxs builds `group.value.lock` (a `DBManyLock`) over every member
/// record (`groupconfigprocessor.cpp:1165`) and takes a `DBManyLocker`
/// across the whole atomic PUT loop (`groupsource.cpp:569`). The Rust
/// equivalent is [`PvDatabase::lock_records`] over the same record
/// set. Names are resolved through the alias map so the gate key
/// matches the one a direct CA/PVA write would take in
/// `put_record_field_from_ca` / `put_pv` / `process_record`.
async fn group_member_record_names(db: &PvDatabase, members: &[GroupMember]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for m in members {
        if m.channel.is_empty() {
            continue; // Structure / Const — no backing record
        }
        let (rec, _) = epics_base_rs::server::database::parse_pv_name(&m.channel);
        let canonical = db
            .resolve_alias(rec)
            .await
            .unwrap_or_else(|| rec.to_string());
        names.push(canonical);
    }
    names.sort();
    names.dedup();
    names
}

// ---------------------------------------------------------------------------
// GroupChannel
// ---------------------------------------------------------------------------

/// A PVA channel backed by a group of EPICS database records.
pub struct GroupChannel {
    db: Arc<PvDatabase>,
    def: GroupPvDef,
    access: super::provider::AccessContext,
    /// negotiated monitor queue depth stamped into
    /// `record._options.queueSize`. `None` for the GET path (a GET
    /// has no subscription queue → [`GROUP_DEFAULT_QUEUE_SIZE`]);
    /// `Some(n)` when a `GroupMonitor` built this channel from the
    /// MONITOR INIT pvRequest's negotiated `queueSize`.
    monitor_queue_size: Option<i32>,
    /// Marks this channel as a MONITOR source, selecting pvxs's
    /// monitor-path `record._options` stamping over the GET path:
    ///   - `atomic` is stamped `true` unconditionally
    ///     (ioc/groupsource.cpp:401-405), whereas GET stamps the
    ///     *selected* operation atomicity (groupsource.cpp:480-485);
    ///   - `queueSize` is stamped with the negotiated monitor queue
    ///     depth (groupsource.cpp:359 `stats.limitQueue`), whereas GET
    ///     leaves the value-template default `0` — pvxs's GET path
    ///     (groupsource.cpp:480-485) stamps only `atomic` and never
    ///     `queueSize`, so a GET reports `queueSize int32_t = 0`
    ///     (test/testqgroup.cpp:60-66), not a monitor queue depth.
    ///
    /// Only a `GroupMonitor`-built channel sets this; the GET path leaves
    /// it `false`.
    monitor_stamp: bool,
}

impl GroupChannel {
    pub fn new(db: Arc<PvDatabase>, def: GroupPvDef) -> Self {
        Self {
            db,
            def,
            access: super::provider::AccessContext::allow_all(),
            monitor_queue_size: None,
            monitor_stamp: false,
        }
    }

    /// Inject an access control context (for [`super::provider::BridgeProvider`]).
    pub fn with_access(mut self, access: super::provider::AccessContext) -> Self {
        self.access = access;
        self
    }

    /// set the negotiated monitor queue depth this
    /// channel stamps into `record._options.queueSize`. Called by
    /// `GroupMonitor::start` with the value resolved from the MONITOR
    /// INIT pvRequest.
    pub fn with_monitor_queue_size(mut self, queue_size: i32) -> Self {
        self.monitor_queue_size = Some(queue_size);
        self
    }

    /// Mark this channel as a MONITOR source so composed values use pvxs's
    /// monitor-path `record._options` stamping (`atomic = true`
    /// unconditionally per ioc/groupsource.cpp:401-405, and the negotiated
    /// `queueSize` per groupsource.cpp:359). The GET path never calls this,
    /// so GET stamps the request/default atomicity and the `queueSize=0`
    /// value-template default (groupsource.cpp:480-485).
    pub fn with_monitor_stamp(mut self) -> Self {
        self.monitor_stamp = true;
        self
    }

    /// Read all member values and compose into a single PvStructure.
    ///
    /// Internal method. Both `Channel::get()` and `GroupMonitor::poll()`
    /// (via the cached `group_channel`) call this. Performs an access
    /// read check on entry — defensive: callers also check, but if a
    /// new caller is added later this guarantees the policy still holds.
    pub(crate) async fn read_group(&self) -> BridgeResult<PvStructure> {
        self.read_group_atomic(self.def.atomic).await
    }

    /// Root structure ID advertised for this group's value and descriptor.
    ///
    /// pvxs leaves `GroupDefinition::structureId` an empty `std::string`
    /// unless a top-level `+id` is configured (groupdefinition.h:30-40,
    /// groupconfigprocessor.cpp:183-189) and builds the group type as
    /// `TypeDef(TypeCode::Struct, structureId, {})` — the empty string when
    /// no `+id` (groupconfigprocessor.cpp:517-523). A non-empty Rust-only
    /// fallback (`"structure"`) would change the group's public type
    /// identity, so clients keying type adapters/caches on the structure ID
    /// would see a different type than the same pvxs group. Single source of
    /// truth so the value and descriptor paths never diverge on the ID.
    fn root_struct_id(&self) -> &str {
        self.def.struct_id.as_deref().unwrap_or("")
    }

    /// read with a caller-specified atomic mode, overriding
    /// the group default. Used by `Channel::get` when the operation
    /// pvRequest carries `record._options.atomic`.
    pub(crate) async fn read_group_atomic(&self, atomic: bool) -> BridgeResult<PvStructure> {
        if !self.access.can_read(&self.def.name) {
            return Err(BridgeError::PutRejected(format!(
                "read denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.user, self.access.host
            )));
        }

        let struct_id = self.root_struct_id();
        let mut pv = PvStructure::new(struct_id);

        // For atomic groups, hold all record locks simultaneously to
        // prevent intermediate states from being observed (pvxs
        // groupsource.cpp:444-459 DBManyLocker pattern).
        //
        // CRITICAL: an atomic group MUST NOT re-lock a member record
        // inside `read_member` — `lock_group_records_read` already
        // holds an `OwnedRwLockReadGuard` on every member record, and
        // `tokio::sync::RwLock` is write-preferring. A plain CA/PVA
        // writer queued between the first guard and a second `.read()`
        // would make that `.read().await` block behind the writer,
        // which itself blocks behind the still-held first guard — a
        // recursive-read deadlock. So the atomic path resolves every
        // member against the pre-acquired guards and never re-locks.
        if atomic {
            let guards = lock_group_records_read(&self.db, &self.def.members).await;
            // Build a name→guard lookup so each member resolves
            // against the already-held guard for its backing record.
            let guard_map: HashMap<&str, &epics_base_rs::server::record::RecordInstance> = guards
                .iter()
                .map(|(name, g)| (name.as_str(), &**g))
                .collect();
            for member in &self.def.members {
                // Only `proc` places no value field. A `+type:"structure"`
                // member emits an empty struct branch (resolved by
                // read_member -> read_member_channelless), matching the
                // advertised descriptor. pvxs adds the empty Struct to the
                // value template (groupconfigprocessor.cpp:922-930) and
                // clones it into every GET/MONITOR snapshot
                // (groupsource.cpp:480-518).
                if member.mapping == FieldMapping::Proc {
                    continue;
                }
                let field = self.read_member_locked(member, &guard_map)?;
                set_member_field(&mut pv, member, field);
            }
        } else {
            for member in &self.def.members {
                // Only `proc` places no value field. A `+type:"structure"`
                // member emits an empty struct branch (resolved by
                // read_member -> read_member_channelless), matching the
                // advertised descriptor. pvxs adds the empty Struct to the
                // value template (groupconfigprocessor.cpp:922-930) and
                // clones it into every GET/MONITOR snapshot
                // (groupsource.cpp:480-518).
                if member.mapping == FieldMapping::Proc {
                    continue;
                }
                let field = self.read_member(member).await?;
                set_member_field(&mut pv, member, field);
            }
        }

        // `record._options` stamping differs between the MONITOR and GET
        // paths in pvxs; `monitor_stamp` selects between them (only the
        // cached monitor `group_channel` sets it).
        //
        // queueSize: a MONITOR stamps the negotiated subscription queue
        // depth (`groupsource.cpp:359` `stats.limitQueue`), resolved from
        // the MONITOR INIT pvRequest via `negotiated_queue_size`
        // (`servermon.cpp:533-540`) and threaded in by
        // `with_monitor_queue_size`. A GET has no subscription queue, so
        // pvxs leaves the value-template default `0` —
        // `groupsource.cpp:480-485` (GroupSource::onOp) stamps only
        // `atomic`, never `queueSize`, and `test/testqgroup.cpp:60-66`
        // confirms a GET reports `record._options.queueSize int32_t = 0`.
        // Before this split a GET stamped GROUP_DEFAULT_QUEUE_SIZE (4),
        // reporting a monitor-looking depth for an operation with none.
        //
        // atomic: a MONITOR stamps `true` unconditionally
        // (`groupsource.cpp:401-405`, GroupMonitor::onStart — a monitor
        // delivers a single consistent snapshot, so it reports itself
        // atomic), while a GET stamps the *operation* atomicity (the
        // pvRequest value, defaulting to the group default). Locking still
        // uses the real `atomic` mode resolved above.
        let queue_size = if self.monitor_stamp {
            self.monitor_queue_size.unwrap_or(GROUP_DEFAULT_QUEUE_SIZE)
        } else {
            0
        };
        let stamp_atomic = if self.monitor_stamp { true } else { atomic };
        push_record_options(&mut pv, stamp_atomic, queue_size);

        Ok(pv)
    }

    /// Read only specific members by field name and compose a partial PvStructure.
    /// Same access enforcement as [`read_group`].
    #[allow(dead_code)]
    async fn read_partial(&self, field_names: &[String]) -> BridgeResult<PvStructure> {
        if !self.access.can_read(&self.def.name) {
            return Err(BridgeError::PutRejected(format!(
                "read denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.user, self.access.host
            )));
        }

        let struct_id = self.root_struct_id();
        let mut pv = PvStructure::new(struct_id);

        for member in &self.def.members {
            // Only `proc` places no value field; a `+type:"structure"`
            // member emits an empty struct branch like the full read path.
            if member.mapping == FieldMapping::Proc {
                continue;
            }
            if !field_names.contains(&member.field_name) {
                continue;
            }

            let field = self.read_member(member).await?;
            set_nested_field(&mut pv, &member.field_name, field);
        }

        Ok(pv)
    }

    /// Resolve the channel-less mappings (Const / Structure / Proc)
    /// that need no record lock. Returns `Some(field)` for those
    /// mappings, `None` for a mapping that requires a backing record.
    fn read_member_channelless(member: &GroupMember) -> Option<PvField> {
        match member.mapping {
            FieldMapping::Const => Some(
                member
                    .const_value
                    .clone()
                    .unwrap_or(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(0))),
            ),
            // Empty struct branch carrying the member `+id` so the value
            // matches the descriptor built in `get_field`
            // (pvxs adds `Struct(id)` to the value template,
            // groupconfigprocessor.cpp:922-930).
            FieldMapping::Structure => Some(PvField::Structure(PvStructure::new(
                member.struct_id.as_deref().unwrap_or(""),
            ))),
            FieldMapping::Proc => Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(0))),
            _ => None,
        }
    }

    /// Read a single member's value from the database. Used by the
    /// non-atomic [`read_group`] path: it locks the backing record
    /// itself (no pre-held guard exists). The atomic path MUST use
    /// [`Self::read_member_locked`] instead — see the deadlock note
    /// in [`read_group`].
    async fn read_member(&self, member: &GroupMember) -> BridgeResult<PvField> {
        if let Some(field) = Self::read_member_channelless(member) {
            return Ok(field);
        }

        let (record_name, field_name) =
            epics_base_rs::server::database::parse_pv_name(&member.channel);

        let rec = self
            .db
            .get_record(record_name)
            .await
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read().await;
        Self::decode_member(member, record_name, field_name, &instance)
    }

    /// Read a single member's value against a record instance that the
    /// caller already holds a read guard on. The atomic [`read_group`]
    /// path uses this so it never re-locks a record whose guard is
    /// held by `lock_group_records_read` (recursive-read deadlock).
    fn read_member_locked(
        &self,
        member: &GroupMember,
        guard_map: &HashMap<&str, &epics_base_rs::server::record::RecordInstance>,
    ) -> BridgeResult<PvField> {
        if let Some(field) = Self::read_member_channelless(member) {
            return Ok(field);
        }

        let (record_name, field_name) =
            epics_base_rs::server::database::parse_pv_name(&member.channel);

        let instance = *guard_map
            .get(record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;
        Self::decode_member(member, record_name, field_name, instance)
    }

    /// Decode one member's value from an already-borrowed record
    /// instance. Shared by the locked (atomic) and self-locking
    /// (non-atomic) read paths so both produce identical output.
    fn decode_member(
        member: &GroupMember,
        record_name: &str,
        field_name: &str,
        instance: &epics_base_rs::server::record::RecordInstance,
    ) -> BridgeResult<PvField> {
        match member.mapping {
            FieldMapping::Scalar => {
                let snapshot = instance.snapshot_for_field(field_name).ok_or_else(|| {
                    BridgeError::FieldNotFound {
                        record: record_name.to_string(),
                        field: field_name.to_string(),
                    }
                })?;
                // Derive the NT shape from the configured field's resolved
                // value (record → common → virtual), not from the owning
                // record type: a `REC.SCAN` member is NTEnum and a
                // `BI.DESC` member is NTScalar string regardless of the
                // record's type. `snapshot.value` IS the resolved field
                // value and `snapshot_for_field` already populated common
                // enum choices (e.g. `.SCAN`). Matches the single-record
                // path and pvxs's per-channel `getChannelValueType`
                // (groupconfigprocessor.cpp:960-974).
                let rtyp = instance.record.record_type();
                let nt_type = pvif::nt_type_for_field(rtyp, field_name, Some(&snapshot.value));
                Ok(PvField::Structure(pvif::snapshot_to_pv_structure(
                    &snapshot, nt_type,
                )))
            }
            FieldMapping::Plain => {
                let value = instance.resolve_field(field_name).ok_or_else(|| {
                    BridgeError::FieldNotFound {
                        record: record_name.to_string(),
                        field: field_name.to_string(),
                    }
                })?;
                Ok(epics_to_pv_field(&value))
            }
            FieldMapping::Meta => {
                let snapshot = instance.snapshot_for_field(field_name).ok_or_else(|| {
                    BridgeError::FieldNotFound {
                        record: record_name.to_string(),
                        field: field_name.to_string(),
                    }
                })?;
                let mut meta = PvStructure::new("meta_t");
                meta.fields.push((
                    "alarm".into(),
                    PvField::Structure(build_alarm_from_snapshot(&snapshot)),
                ));
                meta.fields.push((
                    "timeStamp".into(),
                    PvField::Structure(build_timestamp_from_snapshot_masked(
                        &snapshot,
                        member.nsec_mask,
                    )),
                ));
                Ok(PvField::Structure(meta))
            }
            FieldMapping::Any => {
                let value = instance.resolve_field(field_name).ok_or_else(|| {
                    BridgeError::FieldNotFound {
                        record: record_name.to_string(),
                        field: field_name.to_string(),
                    }
                })?;
                // pvxs serves `+type:"any"` as a PVA `any` slot whose
                // payload carries the concrete DB field type: `IOCSource::
                // get` allocates `anyType.cloneEmpty()` and writes the
                // scalar/array value into it (iocsource.cpp:335-349). Wrap
                // the converted value in a Variant tagged with its own
                // wire-faithful descriptor so the slot decodes as `any`,
                // not a fixed scalar.
                let pv = epics_to_pv_field(&value);
                let desc = pv.wire_descriptor();
                Ok(PvField::Variant(Box::new(VariantValue { desc, value: pv })))
            }
            // Proc, Structure, Const handled by early return above
            FieldMapping::Proc | FieldMapping::Structure | FieldMapping::Const => unreachable!(),
        }
    }

    /// Introspect a group scalar member's NT shape and DBF type from the
    /// configured channel's final field — the same final-field metadata
    /// the single-record path uses ([`super::channel::BridgeChannel::new`]),
    /// not the owning record type. Resolving the field's actual value once
    /// (record → common → virtual) is the single source of truth for both
    /// the advertised NT shape and the descriptor's DBF, so the
    /// descriptor cannot drift from the value the GET path serializes:
    /// `REC.SCAN` advertises NTEnum, `REC.DESC` advertises NTScalar
    /// string, and `BI.DESC` stays a string member on an enum record.
    /// pvxs builds scalar group member descriptors from
    /// `getTypeDefForChannel`/`getChannelValueType` on the field-specific
    /// dbChannel (groupconfigprocessor.cpp:867-974), not the record type.
    async fn introspect_member(&self, member: &GroupMember) -> BridgeResult<(NtType, ScalarType)> {
        let (record_name, field_name) =
            epics_base_rs::server::database::parse_pv_name(&member.channel);

        let rec = self
            .db
            .get_record(record_name)
            .await
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read().await;
        let rtyp = instance.record.record_type();
        let field_upper = field_name.to_ascii_uppercase();
        let resolved = instance.resolve_field(&field_upper);
        let nt_type = pvif::nt_type_for_field(rtyp, &field_upper, resolved.as_ref());
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

        Ok((nt_type, dbf_to_scalar_type(value_dbf)))
    }

    /// Look up a member's actual DBF field type for the PUT conversion
    /// target. Resolves the configured field's value first (record →
    /// common → virtual), so a common/virtual member field (e.g.
    /// `REC.DESC` string, `REC.UTAG` uint64) converts against its real
    /// type instead of falling back to `Double` — same final-field
    /// resolution as the descriptor path above. Returns `Double` only
    /// when the record/field cannot be resolved at all.
    async fn member_dbf_type(&self, member: &GroupMember) -> DbFieldType {
        let (record_name, field_name) =
            epics_base_rs::server::database::parse_pv_name(&member.channel);

        let rec = match self.db.get_record(record_name).await {
            Some(r) => r,
            None => return DbFieldType::Double,
        };
        let instance = rec.read().await;
        let field_upper = field_name.to_ascii_uppercase();
        instance
            .resolve_field(&field_upper)
            .map(|v| v.db_field_type())
            .or_else(|| {
                instance
                    .record
                    .field_list()
                    .iter()
                    .find(|f| f.name == field_upper)
                    .map(|f| f.dbf_type)
            })
            .unwrap_or(DbFieldType::Double)
    }

    /// Convert an incoming PvField to an EpicsValue typed against the
    /// member's actual DBF field. This avoids context-free fallback
    /// conversions (e.g. ScalarValue::Long → EpicsValue::Double).
    ///
    /// For arrays and structures, falls back to `pv_field_to_epics`.
    async fn convert_member_value(
        &self,
        member: &GroupMember,
        pv_field: &epics_pva_rs::pvdata::PvField,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        use epics_pva_rs::pvdata::PvField;
        // A `+type:"any"` member is advertised as a PVA `any` slot, so a
        // pvxs-compatible client PUTs a Variant wrapper. Dereference it
        // (pvxs `IOCSource::put` does `node["->"]`, iocsource.cpp:575-586)
        // and convert the inner concrete value; an unconvertible inner
        // shape (Structure/Union/…) still returns None below and rejects
        // the PUT, matching pvxs.
        let pv_field = match pv_field {
            PvField::Variant(v) => &v.value,
            other => other,
        };
        match pv_field {
            PvField::Scalar(sv) => {
                let target = self.member_dbf_type(member).await;
                // A non-numeric string bound for a numeric member yields
                // `None` here, which the caller treats as an unconvertible
                // member and rejects the whole group PUT (pvxs `parseTo<T>`
                // → `NoConvert`), rather than silently writing 0.
                crate::convert::scalar_to_epics_typed(sv, target).ok()
            }
            // Arrays and structures: defer to the fallback array converter.
            // C++ QSRV uses dbChannelFinalNoElements + DBR types for arrays;
            // for now we delegate to pv_field_to_epics which preserves
            // element types.
            _ => crate::convert::pv_field_to_epics(pv_field),
        }
    }

    /// Apply one ordinary (value) group-member write under the
    /// requested [`ProcessMode`], the single owner of the tri-state →
    /// write mapping for group member application. Mirrors pvxs
    /// `putGroupField` → `IOCSource::put` + `doPostProcessing(
    /// forceProcessing)` (groupsource.cpp:563-571, iocsource.cpp:
    /// 397-420), which preserves the full `TriState forceProcessing`
    /// per member rather than collapsing it to a boolean:
    ///
    /// - `Force` (process=true): raw-write the field, then run a full
    ///   link-aware processing cycle (`dbProcess` equivalent), so a
    ///   forced group PUT processes the backing record even when the
    ///   target field is not process-passive or the record is not
    ///   Passive-scanned.
    /// - `Passive` (process unset): the CA-style put, which processes
    ///   only a `pp(TRUE)` field on a `SCAN=Passive` record
    ///   (`forceProcessing == Unset`).
    /// - `Inhibit` (process=false): raw-write the field, no processing.
    ///
    /// `already_locked` selects the gate-holding variants for the
    /// atomic PUT (which owns every member gate via `lock_records`; the
    /// gate `Mutex` is not reentrant) vs the gate-acquiring variants for
    /// the non-atomic per-member path.
    async fn apply_member_value(
        &self,
        record_name: &str,
        field_name: &str,
        value: epics_base_rs::types::EpicsValue,
        process: super::channel::ProcessMode,
        already_locked: bool,
    ) -> BridgeResult<()> {
        use super::channel::ProcessMode;
        let to_err = |e: epics_base_rs::error::CaError| BridgeError::PutRejected(e.to_string());
        match process {
            ProcessMode::Inhibit => {
                let pv = format!("{record_name}.{field_name}");
                if already_locked {
                    self.db.put_pv_already_locked(&pv, value).await
                } else {
                    self.db.put_pv(&pv, value).await
                }
                .map_err(to_err)?;
            }
            ProcessMode::Passive => {
                if already_locked {
                    self.db
                        .put_record_field_from_ca_already_locked(record_name, field_name, value)
                        .await
                } else {
                    self.db
                        .put_record_field_from_ca(record_name, field_name, value)
                        .await
                }
                .map_err(to_err)?;
            }
            ProcessMode::Force => {
                let pv = format!("{record_name}.{field_name}");
                if already_locked {
                    self.db.put_pv_already_locked(&pv, value).await
                } else {
                    self.db.put_pv(&pv, value).await
                }
                .map_err(to_err)?;
                let mut visited = std::collections::HashSet::new();
                if already_locked {
                    self.db
                        .process_record_with_links_already_locked(record_name, &mut visited, 0)
                        .await
                } else {
                    self.db
                        .process_record_with_links(record_name, &mut visited, 0)
                        .await
                }
                .map_err(to_err)?;
            }
        }
        Ok(())
    }

    /// group PUT with explicit per-operation options.
    ///
    /// pvAccess delivers PUT options (`record._options.process`,
    /// `record._options.atomic`, `record._options.block`) in the INIT
    /// pvRequest, not in the data-phase value (pvxs
    /// `groupsource.cpp:540` reads `putOperation->pvRequest()
    /// ["record._options.atomic"]`, and `:181` runs
    /// `setForceProcessingFlag` against `pvRequest()`). The native
    /// wire path captures the INIT pvRequest on `ChannelContext` and
    /// passes the parsed [`PutOptions`] plus the explicit atomic
    /// override here. `atomic_override` is `None` when the request
    /// did not set the option, in which case the group's configured
    /// default (`self.def.atomic`) applies — matching pvxs's
    /// `bool atomic = group.atomicPutGet;` initializer.
    ///
    /// The trait `put` delegates to this method with options derived
    /// from the data-phase value, preserving the value-embedded-option
    /// contract for in-process callers (gateway, tests).
    pub async fn put_with_options(
        &self,
        value: &PvStructure,
        opts: super::channel::PutOptions,
        atomic_override: Option<bool>,
    ) -> BridgeResult<()> {
        if !self.access.can_write(&self.def.name) {
            return Err(BridgeError::PutRejected(format!(
                "write denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.user, self.access.host
            )));
        }

        // pvRequest can override the group default atomicity
        // (`record._options.atomic = true|false`). Falls back to the
        // group default when the option is absent.
        let atomic = atomic_override.unwrap_or(self.def.atomic);

        // Build the PUT apply order. A *value* member is putable only
        // with an explicit `+putorder`: pvxs's sentinel `i64::MIN`
        // (fieldconfig.h:37) means "not putable" (groupsource.cpp:503),
        // so a no-`+putorder` value member is ignored, never written
        // under an implicit `0`.
        //
        // A `proc` member is the exception: pvxs's `doPostProcessing`
        // returns true for `MappingInfo::Proc` independent of putable
        // (groupsource.cpp:547-573), so a proc hook runs on every group
        // PUT even without `+putorder`. Keep proc members in the apply
        // list regardless; a no-`+putorder` proc sorts at the sentinel
        // position (first), matching the absent-putOrder ordering. Before
        // this fix the `filter_map` dropped them, so a proc-only save/apply
        // hook without `+putorder` silently never ran.
        let mut ordered: Vec<(&GroupMember, i64)> = self
            .def
            .members
            .iter()
            .filter_map(|m| match m.put_order {
                Some(po) => Some((m, po)),
                None if m.mapping == FieldMapping::Proc => Some((m, i64::MIN)),
                None => None,
            })
            .collect();
        ordered.sort_by_key(|(_, po)| *po);
        let ordered: Vec<&GroupMember> = ordered.into_iter().map(|(m, _)| m).collect();

        // A member participates in *this* PUT iff it is a proc hook
        // (runs on every group PUT, allowProc — pvxs
        // groupsource.cpp:547-573) or a value member whose field is
        // present in the incoming value. On the native PVA path the
        // value is pruned to the client's marked members (presence ==
        // marked), so this mirrors pvxs writing only marked members
        // (`marked = leafNode.isMarked(true,true) && field.value`,
        // groupsource.cpp:547-567). An absent (unmarked) value member
        // must not be link-rejected, access-checked, or written — the
        // up-front per-member pre-checks otherwise let an unmarked,
        // unwritable, or link-targeting member reject a partial PUT to
        // an unrelated marked member. Whole-value callers (in-process
        // put()) supply every member, so all stay active — same rule,
        // no special case.
        let member_is_active = |m: &GroupMember| -> bool {
            match m.mapping {
                FieldMapping::Proc => true,
                FieldMapping::Structure | FieldMapping::Const => false,
                _ => get_nested_field(value, &m.field_name).is_some(),
            }
        };

        // pvxs's `groupsource.cpp:548` rejects group PUT
        // preparation for `DBF_INLINK..DBF_FWDLINK` fields —
        // writing into a record's link field via group PUT is
        // semantically meaningless (the link is metadata, not
        // value state) and was a wire compatibility gap. EPICS
        // link fields have well-known names (FLNK, DOL, INP,
        // INP*, OUT, OUT*, SDIS). Reject any member whose target
        // field is in that set before any write fires — but only for
        // members this PUT actually acts on, so an unmarked
        // link-targeting member cannot reject a partial PUT.
        for m in &ordered {
            if !member_is_active(m) {
                continue;
            }
            if member_targets_link_field(&m.channel) {
                return Err(BridgeError::PutRejected(format!(
                    "group {} PUT: member '{}' targets link field '{}' \
                     (pvxs groupsource.cpp:548 rejects link-class field writes)",
                    self.def.name, m.field_name, m.channel
                )));
            }
        }

        // pvxs builds a per-field SecurityClient at group PUT
        // (groupsource.cpp:161 + 515) so a group PV writable for the
        // caller doesn't tunnel writes into members the caller cannot
        // write directly. Re-check write access for each member's
        // backing dbChannel under the caller's identity (already
        // captured in `self.access`). A single denial fails the whole
        // PUT — matching pvxs's "any member denied → operation
        // rejected" remote-error behavior. Only members this PUT acts
        // on are checked: pvxs builds the SecurityClient over the
        // changed fields, so an unmarked, unwritable member must not
        // reject a partial PUT to an unrelated marked one.
        for m in &ordered {
            if !member_is_active(m) {
                continue;
            }
            if m.channel.is_empty() {
                // Structure / Const members have no backing channel
                // to security-check; pvxs skips these in the
                // SecurityClient list as well.
                continue;
            }
            if !self.access.can_write(&m.channel) {
                return Err(BridgeError::PutRejected(format!(
                    "group {} PUT: member '{}' field '{}' write denied for \
                     user='{}' host='{}' (per-member ACF, pvxs \
                     groupsource.cpp:161)",
                    self.def.name, m.field_name, m.channel, self.access.user, self.access.host
                )));
            }
        }

        // track whether any member write/proc actually fired so a
        // marked PUT that writes nothing returns an error like pvxs
        // (groupsource.cpp:605-608) instead of silently succeeding.
        let mut did_something = false;

        if atomic {
            // atomic PUT — `DBManyLock`-equivalent exclusion.
            //
            // pvxs builds a `DBManyLock` over every group-member
            // record (`groupconfigprocessor.cpp:1165`
            // `initialiseDbLocker`) and takes a `DBManyLocker` across
            // the whole atomic PUT member loop
            // (`groupsource.cpp:569`). Because `DBManyLock` locks the
            // same `dbCommon::lock` mutexes that a plain `dbPutField`
            // takes via `dbScanLock`, a direct CA/PVA write to a
            // backing member record cannot interleave with the
            // atomic group transaction.
            //
            // The Rust equivalent: `PvDatabase::lock_records` over
            // every member record acquires the per-record advisory
            // write gates (`dbScanLock` analogue) in canonical sorted
            // order. The plain write path
            // (`put_record_field_from_ca` / `put_pv` /
            // `process_record`) takes the same gate, so a direct
            // backing-record write now blocks until this atomic PUT
            // completes — closing the gap the previous
            // `atomic_write_lock`-only design left open.
            //
            // The member writes below MUST use the `_already_locked`
            // helper variants: this transaction already owns every
            // member-record gate, and the per-record gate `Mutex` is
            // not reentrant.
            let member_records = group_member_record_names(&self.db, &self.def.members).await;
            let _many_guard = self.db.lock_records(&member_records).await;

            // `atomic_write_lock` is retained as an internal aid so
            // two PUTs through the *same* group PV also serialize
            // even before either reaches `lock_records` (e.g. the
            // up-front value-conversion phase).
            let _atomic_guard = self.def.atomic_write_lock.lock().await;

            // Convert all values up-front (DBF-typed), then perform the
            // actual writes in order.
            let mut writes: Vec<(&GroupMember, Option<epics_base_rs::types::EpicsValue>)> =
                Vec::new();

            for member in &ordered {
                if member.mapping == FieldMapping::Proc {
                    // Proc has no value — write entry stays None,
                    // process_record() runs in the apply phase
                    writes.push((member, None));
                    continue;
                }
                if member.mapping == FieldMapping::Structure
                    || member.mapping == FieldMapping::Const
                {
                    continue; // no backing channel, nothing to write
                }

                // Use nested lookup so members with dotted field paths
                // (e.g., "axis.position") resolve correctly. The read
                // path uses set_nested_field — put must use the same
                // path semantics.
                // A supplied-but-unconvertible value is a conversion error,
                // not a "field absent" no-op (see the non-atomic path for the
                // pvxs IOCSource::put/groupsource.cpp parity rationale). Fail
                // the whole atomic PUT here, in the pre-write conversion phase,
                // before any member record is touched — nothing has been
                // applied yet, so the all-or-nothing guarantee holds.
                let epics_val = match get_nested_field(value, &member.field_name) {
                    Some(pv_field) => match self.convert_member_value(member, &pv_field).await {
                        Some(v) => Some(v),
                        None => {
                            return Err(BridgeError::PutRejected(format!(
                                "group {} PUT: member '{}' value is not convertible \
                                 to backing field '{}'",
                                self.def.name, member.field_name, member.channel
                            )));
                        }
                    },
                    None => None, // field not supplied by client → legitimate skip
                };
                writes.push((member, epics_val));
            }

            for (member, val) in writes {
                let (record_name, field_name) =
                    epics_base_rs::server::database::parse_pv_name(&member.channel);

                if member.mapping == FieldMapping::Proc {
                    // A `+proc` member forces a full record-processing
                    // cycle on every group PUT — pvxs's `doPostProcessing`
                    // calls `dbProcess(precord)` for a proc field
                    // (iocsource.cpp:397-417), the link-aware entry that
                    // runs INP/OUT/FLNK side effects. Route through
                    // `process_record_with_links`, not the value-only
                    // `process_record` (process_local + notify) which
                    // skips the link chain. `_already_locked` — this
                    // atomic PUT owns every member-record gate via
                    // `lock_records` (the gate `Mutex` is not reentrant).
                    let mut visited = std::collections::HashSet::new();
                    self.db
                        .process_record_with_links_already_locked(record_name, &mut visited, 0)
                        .await
                        .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                    did_something = true;
                } else if let Some(epics_val) = val {
                    // `_already_locked` — this atomic PUT owns every
                    // member-record gate via `lock_records`.
                    self.apply_member_value(record_name, field_name, epics_val, opts.process, true)
                        .await?;
                    did_something = true;
                }
            }
        } else {
            // Non-atomic put: write each member individually.
            // IMPORTANT: Proc members are checked BEFORE the request-field
            // lookup because they have no value to read — process_record()
            // must run regardless of whether the request contains that field
            // (matches C++ pdbgroup.cpp:300+ allowProc semantics).
            for member in ordered {
                if member.mapping == FieldMapping::Structure
                    || member.mapping == FieldMapping::Const
                {
                    continue; // no backing channel, nothing to write
                }

                let (record_name, field_name) =
                    epics_base_rs::server::database::parse_pv_name(&member.channel);

                if member.mapping == FieldMapping::Proc {
                    // Force a full record-processing cycle (INP/OUT/FLNK)
                    // through the link-aware owner, matching pvxs
                    // `doPostProcessing` → `dbProcess` for a proc field
                    // (iocsource.cpp:397-417). The non-atomic path holds
                    // no member gate, so this is a foreign gate-acquiring
                    // entry. The bare `process_record` would run only the
                    // local record body and skip the link chain.
                    let mut visited = std::collections::HashSet::new();
                    self.db
                        .process_record_with_links(record_name, &mut visited, 0)
                        .await
                        .map_err(|e| BridgeError::PutRejected(e.to_string()))?;
                    did_something = true;
                    continue;
                }

                // Nested-aware lookup (matches read-side set_nested_field)
                let pv_field = match get_nested_field(value, &member.field_name) {
                    Some(f) => f,
                    None => continue,
                };

                // The field WAS supplied by the client; failing to convert
                // it is a conversion error, not a no-op. pvxs's
                // `IOCSource::put` throws on an unsupported conversion
                // (iocsource.cpp:114) and the group put handler turns that
                // into a remote error (groupsource.cpp:665), distinct from
                // the "No fields changed" reply (:656) which fires only when
                // nothing putable was marked. Mirror that: surface the
                // failure instead of silently dropping the member's write.
                let epics_val = match self.convert_member_value(member, &pv_field).await {
                    Some(v) => v,
                    None => {
                        return Err(BridgeError::PutRejected(format!(
                            "group {} PUT: member '{}' value is not convertible \
                             to backing field '{}'",
                            self.def.name, member.field_name, member.channel
                        )));
                    }
                };

                // non-atomic per-member write — gate-acquiring variants.
                self.apply_member_value(record_name, field_name, epics_val, opts.process, false)
                    .await?;
                did_something = true;
            }
        }

        // pvxs returns a remote error "No fields changed" when the
        // client marked fields but nothing was actually written
        // (groupsource.cpp:605-608, `!didSomething && value.isMarked`).
        // Approximate `value.isMarked` by "the client supplied at least one
        // group-member field in the incoming value": if so and nothing
        // fired, reject. A genuinely empty PUT (no member field present)
        // stays a silent no-op, matching pvxs (`value.isMarked` false).
        if !did_something {
            let client_supplied_field = self.def.members.iter().any(|m| {
                !m.field_name.is_empty() && get_nested_field(value, &m.field_name).is_some()
            });
            if client_supplied_field {
                return Err(BridgeError::PutRejected(format!(
                    "group {} PUT: No fields changed",
                    self.def.name
                )));
            }
        }

        Ok(())
    }
}

impl super::provider::Channel for GroupChannel {
    fn channel_name(&self) -> &str {
        &self.def.name
    }

    async fn get(&self, request: &PvStructure) -> BridgeResult<PvStructure> {
        if !self.access.can_read(&self.def.name) {
            return Err(BridgeError::PutRejected(format!(
                "read denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.user, self.access.host
            )));
        }
        // pvRequest can override the group default atomicity.
        let atomic = super::channel::atomic_from_pv_request(request).unwrap_or(self.def.atomic);
        let full = self.read_group_atomic(atomic).await?;
        Ok(pvif::filter_by_request(&full, request))
    }

    async fn put(&self, value: &PvStructure) -> BridgeResult<()> {
        // in-process / value-embedded callers (gateway, tests)
        // carry per-operation options inside the data-phase structure.
        // The native PVA wire path uses `put_with_options` instead so
        // INIT-pvRequest options are honored — see that method.
        let opts = super::channel::PutOptions::from_pv_request(value);
        let atomic_override = super::channel::atomic_from_pv_request(value);
        self.put_with_options(value, opts, atomic_override).await
    }

    async fn get_field(&self) -> BridgeResult<FieldDesc> {
        let struct_id = self.root_struct_id();
        let mut fields: Vec<(String, FieldDesc)> = Vec::new();

        for member in &self.def.members {
            if member.mapping == FieldMapping::Proc {
                continue;
            }

            // Structure and Const have no backing channel — skip introspection.
            let mut desc = match member.mapping {
                FieldMapping::Structure => {
                    let sid = member.struct_id.as_deref().unwrap_or("");
                    FieldDesc::Structure {
                        struct_id: sid.into(),
                        fields: Vec::new(),
                    }
                }
                FieldMapping::Const => {
                    // Derive descriptor from the constant value
                    match &member.const_value {
                        Some(pv_field) => pv_field_to_field_desc(pv_field),
                        None => FieldDesc::Scalar(ScalarType::Int),
                    }
                }
                _ => {
                    let (nt_type, scalar_type) = self.introspect_member(member).await?;
                    match member.mapping {
                        FieldMapping::Scalar => pvif::build_field_desc_for_nt(nt_type, scalar_type),
                        FieldMapping::Plain => FieldDesc::Scalar(scalar_type),
                        FieldMapping::Meta => meta_desc(),
                        // pvxs advertises `+type:"any"` as `Member(TypeCode
                        // ::Any, …)` (groupconfigprocessor.cpp:904-910), an
                        // `any` slot whose concrete payload type is carried
                        // by the value — not a fixed scalar fixed at
                        // introspection time.
                        FieldMapping::Any => FieldDesc::Variant,
                        _ => continue,
                    }
                }
            };
            if let Some(member_id) = &member.struct_id
                && let FieldDesc::Structure { struct_id, .. } = &mut desc
            {
                *struct_id = member_id.clone();
            }

            // Place the descriptor at its (possibly nested) path.
            // The read side uses set_member_field — introspection must
            // emit the same shape so clients see consistent type info.
            set_member_field_desc(&mut fields, member, desc);
        }

        // Advertise the built-in `record._options` branch the value
        // side stamps via push_record_options, placed last to match the
        // value's field order (the value adds `record` after members).
        // pvxs carries it in group.valueTemplate so descriptor and
        // payload agree (groupconfigprocessor.cpp:499-523).
        if let Some(pos) = fields.iter().position(|(n, _)| n == "record") {
            fields[pos].1 = record_options_field_desc();
        } else {
            fields.push(("record".into(), record_options_field_desc()));
        }

        Ok(FieldDesc::Structure {
            struct_id: struct_id.into(),
            fields,
        })
    }

    async fn create_monitor(&self) -> BridgeResult<AnyMonitor> {
        // Read enforcement: deny monitor creation when the client lacks
        // read access. start() also re-checks defensively.
        if !self.access.can_read(&self.def.name) {
            return Err(BridgeError::PutRejected(format!(
                "monitor create denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.user, self.access.host
            )));
        }
        Ok(AnyMonitor::Group(Box::new(
            GroupMonitor::new(self.db.clone(), self.def.clone()).with_access(self.access.clone()),
        )))
    }
}

// ---------------------------------------------------------------------------
// GroupMonitor
// ---------------------------------------------------------------------------

/// The kind of event received from a member subscription.
#[derive(Debug, Clone, Copy)]
enum MemberEventKind {
    /// Value or alarm change (DBE_VALUE | DBE_ALARM).
    Value,
    /// Property change — display limits, enum choices, etc. (DBE_PROPERTY).
    Property,
}

/// Event from a group member subscription, sent through the fan-in channel.
struct MemberEvent {
    member_index: usize,
    kind: MemberEventKind,
}

/// A PVA monitor for a group PV that subscribes to all member records.
///
/// Corresponds to C++ QSRV's `PDBGroupMonitor` + `pdb_group_event()`.
/// Uses a fan-in channel pattern: each member subscription spawns a task
/// that forwards events to a single receiver, enabling concurrent wait
/// across all members.
///
/// Drop-guarded per-member task handle. Aborts the spawned forwarder
/// when the GroupMonitor drops so a quiet PV doesn't leak the task.
pub struct MemberTaskGuard(tokio::task::AbortHandle);

impl Drop for MemberTaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct GroupMonitor {
    db: Arc<PvDatabase>,
    def: GroupPvDef,
    running: bool,
    /// Reusable GroupChannel for read_group/read_partial calls.
    /// Created once in start() instead of per-event in poll().
    /// The internal GroupChannel inherits the same `access` context so
    /// any read enforcement applied at create_monitor time stays in effect.
    group_channel: Option<GroupChannel>,
    /// Fan-in receiver for member events
    event_rx: Option<tokio::sync::mpsc::Receiver<MemberEvent>>,
    /// Handles for spawned per-member tasks. Wrapped in AbortOnDrop
    /// so a quiet PV (no events between subscribe-then-drop cycles)
    /// doesn't leak member-subscription tasks. Each task drives a
    /// DbSubscription and forwards into the fan-in mpsc; without
    /// the abort guard those tasks survive group-monitor teardown
    /// until the parent record's broadcast disconnects.
    _tasks: Vec<MemberTaskGuard>,
    /// Access control context propagated from the parent GroupChannel.
    access: super::provider::AccessContext,
    /// negotiated monitor queue depth, resolved from
    /// the MONITOR INIT pvRequest's `record._options.queueSize`
    /// ([`negotiated_queue_size`]). Stamped into every monitor
    /// value's `record._options.queueSize` via the internal
    /// `GroupChannel`. Defaults to [`GROUP_DEFAULT_QUEUE_SIZE`] when
    /// the pvRequest carries no usable `queueSize`.
    monitor_queue_size: i32,
}

/// how a member subscription event maps onto a group
/// monitor post — pvxs `groupsource.cpp:283-300` (value) /
/// `:310-340` (property) / `subscriptionPost` `:207`.
enum EventMark {
    /// Post the group, marking exactly these group field paths
    /// (the resolved `+trigger` target set, assigned-not-changed).
    Marked(Vec<String>),
    /// Post the group; the server derives the changed-bitset (full
    /// request mask, or diff for a pure self-trigger group).
    Derive,
    /// No post — `TriggerDef::None`, or every named target dropped
    /// (pvxs `subscriptionPost` `if(empty && !first) return`).
    Skip,
}

impl GroupMonitor {
    pub fn new(db: Arc<PvDatabase>, def: GroupPvDef) -> Self {
        Self {
            db,
            def,
            running: false,
            group_channel: None,
            event_rx: None,
            _tasks: Vec::new(),
            access: super::provider::AccessContext::allow_all(),
            monitor_queue_size: GROUP_DEFAULT_QUEUE_SIZE,
        }
    }

    /// Inject an access control context. Called by `GroupChannel::create_monitor`.
    pub fn with_access(mut self, access: super::provider::AccessContext) -> Self {
        self.access = access;
        self
    }

    /// set the per-operation negotiated monitor queue
    /// depth, resolved from the MONITOR INIT pvRequest by the QSRV
    /// adapter ([`negotiated_queue_size`]). Threaded into the internal
    /// `GroupChannel` so every monitor value reports the depth the
    /// client actually requested instead of a hardcoded default.
    pub fn with_queue_size(mut self, queue_size: i32) -> Self {
        self.monitor_queue_size = queue_size;
        self
    }

    /// resolve the marked-leaf field paths for a *value*
    /// event from `source_idx`, mirroring pvxs `groupsource.cpp:283`
    /// iterating `field.triggers` and marking each target.
    ///
    /// A *pure self-trigger* group keeps the existing path
    /// ([`EventMark::Derive`]) so the value-diff narrowing and its
    /// tests are untouched — this finding is about explicit `+trigger`
    /// graphs, where `SelfOnly`, `All`, and named `Fields` must stay
    /// distinct instead of all re-reading the full group.
    ///
    /// Takes `&GroupPvDef` (not `&self`) so `poll` can call it while
    /// holding the `&mut self.event_rx` borrow — `def` is a disjoint
    /// field.
    fn value_event_mark(def: &GroupPvDef, source_idx: usize) -> EventMark {
        let Some(source) = def.members.get(source_idx) else {
            return EventMark::Skip;
        };
        if def.is_pure_self_trigger() {
            return match source.triggers {
                TriggerDef::None => EventMark::Skip,
                _ => EventMark::Derive,
            };
        }
        let targets: Vec<&str> = match &source.triggers {
            TriggerDef::None => return EventMark::Skip,
            // Self-trigger inside a mixed group marks only its own field.
            TriggerDef::SelfOnly => vec![source.field_name.as_str()],
            // `"*"` marks every member field WITH A CHANNEL. pvxs drops
            // channel-less Const/Structure targets from the `*` expansion
            // (`groupconfigprocessor.cpp:387-388`: `if(!…channel.empty())`)
            // — a channel-less member never produces a runtime event, so
            // marking it would flag it "changed" + re-serialize on every
            // update for nothing.
            TriggerDef::All => def
                .members
                .iter()
                .filter(|m| !m.channel.is_empty())
                .map(|m| m.field_name.as_str())
                .collect(),
            // Named targets: pvxs resolves only references that name an
            // existing field WITH A CHANNEL (`groupconfigprocessor.cpp:
            // 405-409`: a target whose `channel.empty()` is ignored).
            // Unknown refs were warned + dropped at parse time and
            // are absent from `channeled` here too.
            TriggerDef::Fields(refs) => {
                let channeled: std::collections::HashSet<&str> = def
                    .members
                    .iter()
                    .filter(|m| !m.channel.is_empty())
                    .map(|m| m.field_name.as_str())
                    .collect();
                refs.iter()
                    .map(String::as_str)
                    .filter(|r| channeled.contains(r))
                    .collect()
            }
        };
        Self::mark_or_derive(targets)
    }

    /// a *property* event marks only the source field's
    /// own mapping and never its triggers — pvxs `groupsource.cpp:325`
    /// ("we (may) only post changes to the field mapping in question.
    /// But never the triggered fields."). A pure self-trigger group
    /// keeps the diff path.
    fn property_event_mark(def: &GroupPvDef, source_idx: usize) -> EventMark {
        let Some(source) = def.members.get(source_idx) else {
            return EventMark::Skip;
        };
        if def.is_pure_self_trigger() {
            return EventMark::Derive;
        }
        Self::mark_or_derive(vec![source.field_name.as_str()])
    }

    /// Turn a resolved target list into an [`EventMark`]. An empty list
    /// means every target was dropped → no post. A target that cannot
    /// be addressed by a structure path (a root-flattened `Meta` member
    /// with an empty field name) falls back to [`EventMark::Derive`] —
    /// a full mask — rather than under-marking and losing data.
    fn mark_or_derive(targets: Vec<&str>) -> EventMark {
        if targets.is_empty() {
            return EventMark::Skip;
        }
        if targets.iter().any(|n| n.is_empty()) {
            return EventMark::Derive;
        }
        EventMark::Marked(targets.into_iter().map(str::to_string).collect())
    }
}

impl super::provider::PvaMonitor for GroupMonitor {
    async fn start(&mut self) -> BridgeResult<()> {
        if self.running {
            return Ok(());
        }

        // Read enforcement: refuse to spin up upstream subscriptions
        // for a client that lacks read permission on this group.
        if !self.access.can_read(&self.def.name) {
            return Err(BridgeError::PutRejected(format!(
                "monitor read denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.user, self.access.host
            )));
        }

        // Create fan-in channel for member events. Capacity scales
        // with member count so a many-record group with simultaneous
        // updates doesn't backpressure each member's
        // DbSubscription (which would lose events to broadcast Lag).
        // 64 was the original constant; 4× members.len() gives slow
        // groups the same headroom while bounded by member count.
        let cap = (self.def.members.len() * 4).max(64);
        let (tx, rx) = tokio::sync::mpsc::channel::<MemberEvent>(cap);

        // Subscribe to ALL members with channels, regardless of trigger
        // setting — pvxs subscribes every field with a dbChannel
        // (groupsource.cpp:375-398). TriggerDef::None only means "don't
        // update the group when this field changes"; its events are
        // filtered to EventMark::Skip in poll() rather than gating the
        // stream.
        for (idx, member) in self.def.members.iter().enumerate() {
            if member.channel.is_empty() {
                continue; // Structure/Const/Proc-without-channel — no backing channel
            }

            // subscribe value events against the full
            // `member.channel` (e.g. `REC.RVAL`), not the bare record
            // name. pvxs `field.cpp:25-26` builds both the value and
            // properties dbChannels from the same `def.channel`
            // (`groupsource.cpp:386,395`), so the subscription identity
            // is the configured member field. The previous code parsed
            // off the field suffix and subscribed against `REC.VAL`, so
            // a non-`VAL` member woke on unrelated `VAL` posts and
            // missed posts made only by its own field. `record_name`
            // is no longer needed here — the read path
            // (`read_member`) re-parses `member.channel` for the
            // field it actually decodes.
            //
            // choose the value mask per member mapping.
            // pvxs `groupsource.cpp:386` subscribes `Meta` value-side
            // events with `DBE_ALARM` only; `groupsource.cpp:389` uses
            // `DBE_VALUE | DBE_ALARM | DBE_ARCHIVE` for non-meta
            // mappings. `DBE_ARCHIVE` (epics-base-rs's `EventMask::LOG`)
            // is the archive-class event records post via
            // `recGblFwdLink` when the LOG deadband fires; folding it
            // into the value path lets archiver-like clients watching
            // the group PV see the same posts the backing record's CA
            // monitor would. A `Meta` member carries only
            // `{alarm, timeStamp}` (see `FieldMapping::Meta` decode),
            // so waking it on plain value/log posts produced group
            // deltas whose only changed fields were metadata
            // timestamps — extra traffic pvxs does not emit.
            let value_mask = if member.mapping == FieldMapping::Meta {
                epics_base_rs::server::recgbl::EventMask::ALARM.bits()
            } else {
                (epics_base_rs::server::recgbl::EventMask::VALUE
                    | epics_base_rs::server::recgbl::EventMask::ALARM
                    | epics_base_rs::server::recgbl::EventMask::LOG)
                    .bits()
            };
            if let Some(mut sub) =
                DbSubscription::subscribe_with_mask(&self.db, &member.channel, 0, value_mask).await
            {
                let tx = tx.clone();
                let handle = tokio::spawn(async move {
                    while sub.recv_snapshot().await.is_some() {
                        if tx
                            .send(MemberEvent {
                                member_index: idx,
                                kind: MemberEventKind::Value,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                self._tasks.push(MemberTaskGuard(handle.abort_handle()));
            }

            // Property subscription (DBE_PROPERTY) — only for Scalar/Meta
            // mappings that include metadata. Plain/Any/Proc don't need it.
            // target the same `member.channel` as the value
            // subscription (pvxs `field.cpp:26` derives the properties
            // dbChannel from the identical `def.channel`); the record
            // default would mis-scope members configured on a non-`VAL`
            // field.
            if member.mapping == FieldMapping::Scalar || member.mapping == FieldMapping::Meta {
                let prop_mask = epics_base_rs::server::recgbl::EventMask::PROPERTY.bits();
                if let Some(mut sub) =
                    DbSubscription::subscribe_with_mask(&self.db, &member.channel, 0, prop_mask)
                        .await
                {
                    let tx = tx.clone();
                    let handle = tokio::spawn(async move {
                        while sub.recv_snapshot().await.is_some() {
                            if tx
                                .send(MemberEvent {
                                    member_index: idx,
                                    kind: MemberEventKind::Property,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                    self._tasks.push(MemberTaskGuard(handle.abort_handle()));
                }
            }
        }

        // Create a reusable GroupChannel once (instead of per-event in poll).
        // Propagate the same access context so any subsequent reads triggered
        // by trigger evaluation also honor read enforcement.
        //
        // thread the per-operation negotiated monitor
        // queue depth so every monitor value stamps
        // `record._options.queueSize` with the client-requested depth
        // (pvxs `groupsource.cpp:359` `stats.limitQueue`).
        let group_channel = GroupChannel::new(self.db.clone(), self.def.clone())
            .with_access(self.access.clone())
            .with_monitor_queue_size(self.monitor_queue_size)
            .with_monitor_stamp();

        self.group_channel = Some(group_channel);
        self.event_rx = Some(rx);
        self.running = true;
        Ok(())
    }

    async fn poll(&mut self) -> Option<super::provider::MonitorPoll> {
        // Purely event-driven: the wire layer already sent the initial
        // frame via get_value_checked() at MONITOR INIT for every source
        // (server_native/tcp.rs:build_monitor_payload), so this stream
        // carries only fresh member deltas — never an initial snapshot.
        //
        // A channel-less (all-const) group has no member subscriptions, so
        // this never wakes and the client sees exactly one DATA frame (the
        // wire initial). A channel-backed group forwards each member event
        // as it arrives, with NO gate on other members posting first: pvxs
        // primes every field from sampled values at start via
        // db_post_single_event (groupsource.cpp:289-297), so a quiet member
        // never withholds an active one. The previous per-member priming
        // gate both withheld every delta until all members changed and
        // re-emitted a full snapshot the wire layer had already sent — one
        // structural defect (the priming gate) producing two symptoms.
        let rx = self.event_rx.as_mut()?;

        loop {
            let event = rx.recv().await?;

            // resolve which group field paths this event
            // marks, instead of treating every trigger kind identically.
            // pvxs iterates `field.triggers` (value) or the source field
            // alone (property), refreshes those targets, then posts the
            // full group with only those leaves marked. `read_group()`
            // still reads every member (the posted Value is complete);
            // the marked set is what the PVA layer turns into the wire
            // changed-bitset.
            let mark = match event.kind {
                MemberEventKind::Value => Self::value_event_mark(&self.def, event.member_index),
                MemberEventKind::Property => {
                    Self::property_event_mark(&self.def, event.member_index)
                }
            };
            let marked = match mark {
                EventMark::Skip => continue,
                EventMark::Derive => None,
                EventMark::Marked(paths) => Some(paths),
            };

            let group_channel = self.group_channel.as_ref()?;
            return group_channel
                .read_group()
                .await
                .ok()
                .map(|value| super::provider::MonitorPoll { value, marked });
        }
    }

    async fn stop(&mut self) {
        // Drop the receiver first to signal tasks to stop
        self.event_rx = None;

        // Abort spawned tasks. Drop fires the AbortOnDrop guard.
        self._tasks.clear();

        self.running = false;
        self.group_channel = None;
    }
}

// ---------------------------------------------------------------------------
// AnyMonitor
// ---------------------------------------------------------------------------

/// Enum dispatch for monitor types (single record vs group).
pub enum AnyMonitor {
    Single(Box<BridgeMonitor>),
    Group(Box<GroupMonitor>),
}

impl AnyMonitor {
    /// apply the per-operation negotiated monitor
    /// queue depth (resolved from the MONITOR INIT pvRequest's
    /// `record._options.queueSize`). Only a group monitor stamps
    /// `record._options.queueSize` into its values — a single-record
    /// monitor has no group-style `record._options` branch, so this
    /// is a no-op for the `Single` variant.
    pub fn with_queue_size(self, queue_size: i32) -> Self {
        match self {
            Self::Group(m) => Self::Group(Box::new(m.with_queue_size(queue_size))),
            single => single,
        }
    }
}

impl super::provider::PvaMonitor for AnyMonitor {
    async fn poll(&mut self) -> Option<super::provider::MonitorPoll> {
        match self {
            Self::Single(m) => m.poll().await,
            Self::Group(m) => m.poll().await,
        }
    }

    async fn start(&mut self) -> BridgeResult<()> {
        match self {
            Self::Single(m) => m.start().await,
            Self::Group(m) => m.start().await,
        }
    }

    async fn stop(&mut self) {
        match self {
            Self::Single(m) => m.stop().await,
            Self::Group(m) => m.stop().await,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// pvxs `groupsource.cpp:596-606` refuses group PUT preparation for any
/// field whose `dbChannelFinalFieldType` is in `DBF_INLINK..DBF_FWDLINK`
/// — a DBF-class rule, not a name rule.
///
/// The durable fix classifies by DBF type. `epics-base-rs` does not yet
/// expose a link-field DBF class (`DbFieldType` has no
/// `INLINK`/`OUTLINK`/`FWDLINK` variants; link fields surface as
/// `String`), so this is a NAME-based stopgap. It is intentionally a
/// superset and is INCOMPLETE: a custom record type with a link field
/// outside these families is not caught, and the real close requires
/// `epics-base-rs` to expose the field's DBF class (tracked as a
/// cross-crate follow-up). Until then we enumerate the standard EPICS
/// Base link-field name families so the common record types reject
/// link writes the same way pvxs does.
///
/// Covered families (all `DBF_INLINK`/`DBF_OUTLINK`/`DBF_FWDLINK` in
/// EPICS Base `*.dbd.pod`):
///   - `FLNK` (forward), `SDIS` (disable), `DOL`, `INP`, `OUT`
///   - simulation links `SIOL` / `SIML` and `selRecord` `NVL`,
///     `histogramRecord` `SVL`
///   - indexed/lettered families: `INP*` / `OUT*` (calc/aSub),
///     `DOL*` (`seqRecord` `DOL0..DOL9`/`DOLA`/`DOLF`), and
///     `LNK*` (`seqRecord` `LNK0..LNK9`/`LNKF`) where `*` is a single
///     alphanumeric character.
fn member_targets_link_field(channel: &str) -> bool {
    let (_, field) = epics_base_rs::server::database::parse_pv_name(channel);
    let f = field.to_ascii_uppercase();
    if matches!(
        f.as_str(),
        "FLNK" | "DOL" | "SDIS" | "INP" | "OUT" | "NVL" | "SVL" | "SIOL" | "SIML"
    ) {
        return true;
    }
    // Indexed/lettered link families: a known link prefix followed by
    // exactly one alphanumeric suffix character. This is a superset
    // (record-type specifics may treat a given `INPx` as non-link) but
    // rejecting it through group PUT is the safe direction — pvxs's rule
    // is "never write a link through a group".
    for prefix in ["INP", "OUT", "DOL", "LNK"] {
        if let Some(rest) = f.strip_prefix(prefix)
            && rest.len() == 1
            && rest.chars().next().unwrap().is_ascii_alphanumeric()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod link_field_tests {
    use super::member_targets_link_field;

    #[test]
    fn link_class_field_names_rejected() {
        for f in ["FLNK", "DOL", "SDIS", "INP", "OUT", "INPA", "OUTL", "INPZ"] {
            assert!(
                member_targets_link_field(&format!("REC.{f}")),
                "expected {f} to be classified as a link field"
            );
        }
    }

    #[test]
    fn standard_record_link_families_rejected() {
        // The families the original name subset missed, each defined as
        // a link field in EPICS Base `*.dbd.pod`:
        //   seqRecord DOL0 (INLINK) / LNK0 (OUTLINK) / DOLA / DOLF / LNKF
        //   selRecord NVL (INLINK); histogramRecord SVL (INLINK)
        //   boRecord SIOL (OUTLINK) / SIML (INLINK)
        for f in [
            "DOL0", "LNK0", "DOLA", "DOLF", "LNKF", "NVL", "SVL", "SIOL", "SIML",
        ] {
            assert!(
                member_targets_link_field(&format!("REC.{f}")),
                "expected {f} to be classified as a link field"
            );
        }
    }

    #[test]
    fn value_class_field_names_allowed() {
        for f in ["VAL", "DESC", "EGU", "PREC", "SCAN", "HIHI", "LOLO", "RVAL"] {
            assert!(
                !member_targets_link_field(&format!("REC.{f}")),
                "expected {f} to be classified as a value field, not a link"
            );
        }
    }

    #[test]
    fn bare_record_default_is_val_not_link() {
        // parse_pv_name returns ("REC", "VAL") for "REC".
        assert!(!member_targets_link_field("REC"));
    }
}

fn meta_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "meta_t".into(),
        fields: vec![
            (
                "alarm".into(),
                FieldDesc::Structure {
                    struct_id: "alarm_t".into(),
                    fields: vec![
                        ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                    ],
                },
            ),
            (
                "timeStamp".into(),
                FieldDesc::Structure {
                    struct_id: "time_t".into(),
                    fields: vec![
                        (
                            "secondsPastEpoch".into(),
                            FieldDesc::Scalar(ScalarType::Long),
                        ),
                        ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
                    ],
                },
            ),
        ],
    }
}

/// Derive a FieldDesc from a PvField value (used for Const mapping introspection).
fn pv_field_to_field_desc(field: &PvField) -> FieldDesc {
    use epics_pva_rs::pvdata::ScalarValue;
    match field {
        PvField::Scalar(sv) => FieldDesc::Scalar(match sv {
            ScalarValue::Boolean(_) => ScalarType::Boolean,
            ScalarValue::Byte(_) => ScalarType::Byte,
            ScalarValue::Short(_) => ScalarType::Short,
            ScalarValue::Int(_) => ScalarType::Int,
            ScalarValue::Long(_) => ScalarType::Long,
            ScalarValue::UByte(_) => ScalarType::UByte,
            ScalarValue::UShort(_) => ScalarType::UShort,
            ScalarValue::UInt(_) => ScalarType::UInt,
            ScalarValue::ULong(_) => ScalarType::ULong,
            ScalarValue::Float(_) => ScalarType::Float,
            ScalarValue::Double(_) => ScalarType::Double,
            ScalarValue::String(_) => ScalarType::String,
        }),
        PvField::ScalarArray(arr) => {
            let elem_type = arr
                .first()
                .map(|sv| match sv {
                    ScalarValue::Boolean(_) => ScalarType::Boolean,
                    ScalarValue::Byte(_) => ScalarType::Byte,
                    ScalarValue::Short(_) => ScalarType::Short,
                    ScalarValue::Int(_) => ScalarType::Int,
                    ScalarValue::Long(_) => ScalarType::Long,
                    ScalarValue::UByte(_) => ScalarType::UByte,
                    ScalarValue::UShort(_) => ScalarType::UShort,
                    ScalarValue::UInt(_) => ScalarType::UInt,
                    ScalarValue::ULong(_) => ScalarType::ULong,
                    ScalarValue::Float(_) => ScalarType::Float,
                    ScalarValue::Double(_) => ScalarType::Double,
                    ScalarValue::String(_) => ScalarType::String,
                })
                .unwrap_or(ScalarType::Double);
            FieldDesc::ScalarArray(elem_type)
        }
        PvField::ScalarArrayTyped(arr) => FieldDesc::ScalarArray(arr.scalar_type()),
        PvField::Structure(s) => FieldDesc::Structure {
            struct_id: s.struct_id.clone(),
            fields: s
                .fields
                .iter()
                .map(|(name, f)| (name.clone(), pv_field_to_field_desc(f)))
                .collect(),
        },
        // Other shapes don't appear in qsrv group Const mappings; return a
        // benign empty structure so callers never see a partial decode.
        PvField::StructureArray(_)
        | PvField::Union { .. }
        | PvField::UnionArray(_)
        | PvField::Variant(_)
        | PvField::VariantArray(_)
        | PvField::Null => FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new(),
        },
    }
}

fn build_alarm_from_snapshot(snapshot: &epics_base_rs::server::snapshot::Snapshot) -> PvStructure {
    use epics_pva_rs::pvdata::ScalarValue;
    let mut alarm = PvStructure::new("alarm_t");
    alarm.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(snapshot.alarm.severity as i32)),
    ));
    // PVA alarm.status is the status CLASS and alarm.message is
    // the condition string (pvxs iocsource.cpp:187-236), not the raw
    // condition code / empty string.
    alarm.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(pvif::alarm_status_class(
            snapshot.alarm.status,
        ))),
    ));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(
            pvif::alarm_condition_string(snapshot.alarm.status).to_string(),
        )),
    ));
    alarm
}

/// Build a timestamp PvStructure with optional nsecMask.
///
/// When `nsec_mask` is non-zero, the lower bits of nanoseconds are
/// extracted and placed in `userTag` (pvxs iocsource.cpp:241-247).
fn build_timestamp_from_snapshot_masked(
    snapshot: &epics_base_rs::server::snapshot::Snapshot,
    nsec_mask: u32,
) -> PvStructure {
    use epics_pva_rs::pvdata::ScalarValue;
    use std::time::UNIX_EPOCH;

    let mut ts = PvStructure::new("time_t");
    let (secs, raw_nanos) = match snapshot.timestamp.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(_) => (0, 0),
    };
    let nanos = if nsec_mask != 0 {
        (raw_nanos & !nsec_mask) as i32
    } else {
        raw_nanos as i32
    };
    let user_tag = if nsec_mask != 0 {
        (raw_nanos & nsec_mask) as i32
    } else {
        // No group `+nsecmask`: serve the snapshot's userTag, which the
        // record snapshot builder defaults to `common.utag` (pvxs
        // `iocsource.cpp:245`). Pre-fix this hard-coded 0, dropping the
        // record's utag on group serve.
        snapshot.user_tag
    };
    ts.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(secs)),
    ));
    ts.fields.push((
        "nanoseconds".into(),
        PvField::Scalar(ScalarValue::Int(nanos)),
    ));
    ts.fields.push((
        "userTag".into(),
        PvField::Scalar(ScalarValue::Int(user_tag)),
    ));
    ts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qsrv::provider::Channel;
    use std::time::Duration;

    #[test]
    fn nested_field_set_simple() {
        let mut pv = PvStructure::new("test");
        set_nested_field(
            &mut pv,
            "x",
            PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(42)),
        );
        assert!(pv.get_field("x").is_some());
    }

    /// a group `meta` timeStamp serves the record's userTag (carried in
    /// `Snapshot.user_tag`, which the record snapshot builder defaults to
    /// `common.utag`) when the group field has no `+nsecmask` — pvxs
    /// `iocsource.cpp:245`. Pre-fix the no-mask branch hard-coded 0,
    /// dropping the record's tag. A `+nsecmask` overrides with the masked
    /// nanosecond bits (:247). The bit-31 tag also pins that the
    /// snapshot's i32 userTag passes through unchanged.
    #[test]
    fn group_timestamp_serves_record_usertag_without_nsecmask() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::pvdata::{PvField, ScalarValue};
        use std::time::UNIX_EPOCH;

        let tag = |s: &PvStructure| match s.get_field("userTag") {
            Some(PvField::Scalar(ScalarValue::Int(v))) => *v,
            other => panic!("userTag must be Int, got {other:?}"),
        };

        let mut snap = Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            UNIX_EPOCH + Duration::new(1_700_000_000, 0x0000_00FF),
        );
        snap.user_tag = 0x9000_0000u32 as i32;

        // No `+nsecmask` → serve the snapshot's (record) userTag, not 0.
        assert_eq!(
            tag(&build_timestamp_from_snapshot_masked(&snap, 0)),
            0x9000_0000u32 as i32,
            "no-nsecmask group member must serve the record's utag"
        );
        // `+nsecmask` → userTag is the masked nanosecond bits (override).
        assert_eq!(
            tag(&build_timestamp_from_snapshot_masked(&snap, 0xFF)),
            0x0000_00FF,
            "nsecmask group member must serve the masked nanosecond bits"
        );
    }

    /// a `+trigger` target without a backing channel (Const /
    /// Structure member) must NOT be marked in the changed-bitset. pvxs
    /// filters channel-less members out of BOTH the `*` expansion
    /// (`groupconfigprocessor.cpp:387-388`) and named-target resolution
    /// (`405-409`). Pre-fix the Rust `*` and named arms marked them, so a
    /// `+trigger:"*"` group with a const/structure member flagged it
    /// "changed" and re-serialized it on every update.
    #[test]
    fn value_trigger_excludes_channelless_members() {
        use crate::qsrv::group_config::parse_group_config;

        fn src_idx(def: &GroupPvDef) -> usize {
            def.members
                .iter()
                .position(|m| m.field_name == "src")
                .expect("src member present")
        }

        // `*` arm: marks every CHANNELED member, never the structure one.
        let star = r#"{ "GRP": {
            "src":  { "+channel": "R:src", "+trigger": "*" },
            "chan": { "+channel": "R:chan" },
            "meta": { "+type": "structure", "+id": "x/v1" }
        } }"#;
        let def = parse_group_config(star).unwrap().pop().unwrap();
        match GroupMonitor::value_event_mark(&def, src_idx(&def)) {
            EventMark::Marked(paths) => {
                assert!(
                    paths.contains(&"chan".to_string()),
                    "channeled member must be marked: {paths:?}"
                );
                assert!(
                    !paths.contains(&"meta".to_string()),
                    "channel-less structure member must NOT be marked by `*`: {paths:?}"
                );
            }
            EventMark::Derive => panic!("expected Marked from a `*` trigger, got Derive"),
            EventMark::Skip => panic!("expected Marked from a `*` trigger, got Skip"),
        }

        // Named arm: a named channel-less target is dropped, channeled kept.
        let named = r#"{ "GRP2": {
            "src":  { "+channel": "R:src", "+trigger": "chan,meta" },
            "chan": { "+channel": "R:chan" },
            "meta": { "+type": "structure", "+id": "x/v1" }
        } }"#;
        let def = parse_group_config(named).unwrap().pop().unwrap();
        match GroupMonitor::value_event_mark(&def, src_idx(&def)) {
            EventMark::Marked(paths) => {
                assert!(
                    paths.contains(&"chan".to_string()),
                    "named channeled target must be marked: {paths:?}"
                );
                assert!(
                    !paths.contains(&"meta".to_string()),
                    "named channel-less target must NOT be marked: {paths:?}"
                );
            }
            EventMark::Derive => panic!("expected Marked from named triggers, got Derive"),
            EventMark::Skip => panic!("expected Marked from named triggers, got Skip"),
        }
    }

    #[test]
    fn nested_field_set_deep() {
        let mut pv = PvStructure::new("test");
        set_nested_field(
            &mut pv,
            "a.b.c",
            PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(2.5)),
        );
        let a = pv.get_field("a");
        assert!(a.is_some());
        if let Some(PvField::Structure(a_struct)) = a {
            if let Some(PvField::Structure(b_struct)) = a_struct.get_field("b") {
                assert!(b_struct.get_field("c").is_some());
            } else {
                panic!("expected b structure");
            }
        } else {
            panic!("expected a structure");
        }
    }

    #[test]
    fn nested_field_roundtrip() {
        use epics_pva_rs::pvdata::ScalarValue;

        let mut pv = PvStructure::new("test");
        set_nested_field(&mut pv, "a.b", PvField::Scalar(ScalarValue::Int(99)));

        // Verify get_nested_field returns the same value
        let field = get_nested_field(&pv, "a.b");
        assert!(field.is_some());
        if let Some(PvField::Scalar(ScalarValue::Int(v))) = field.as_deref() {
            assert_eq!(*v, 99);
        } else {
            panic!("expected Int(99)");
        }
    }

    #[test]
    fn nested_field_overwrite() {
        use epics_pva_rs::pvdata::ScalarValue;

        let mut pv = PvStructure::new("test");
        set_nested_field(&mut pv, "x.y", PvField::Scalar(ScalarValue::Int(1)));
        set_nested_field(&mut pv, "x.y", PvField::Scalar(ScalarValue::Int(2)));

        if let Some(PvField::Scalar(ScalarValue::Int(v))) = get_nested_field(&pv, "x.y").as_deref()
        {
            assert_eq!(*v, 2);
        } else {
            panic!("expected Int(2)");
        }
    }

    #[test]
    fn nested_field_siblings() {
        use epics_pva_rs::pvdata::ScalarValue;

        let mut pv = PvStructure::new("test");
        set_nested_field(&mut pv, "a.x", PvField::Scalar(ScalarValue::Int(1)));
        set_nested_field(&mut pv, "a.y", PvField::Scalar(ScalarValue::Int(2)));

        assert!(get_nested_field(&pv, "a.x").is_some());
        assert!(get_nested_field(&pv, "a.y").is_some());
    }

    /// `field[N]` on a ScalarArray must return the indexed scalar
    /// element wrapped as a fresh PvField::Scalar — NOT the whole
    /// array. Regression test: prior implementation returned the
    /// array unchanged, silently breaking NTTable column[N] paths.
    #[test]
    fn nested_field_scalar_array_index() {
        use epics_pva_rs::pvdata::ScalarValue;

        let mut pv = PvStructure::new("test");
        pv.fields.push((
            "samples".into(),
            PvField::ScalarArray(vec![
                ScalarValue::Double(1.5),
                ScalarValue::Double(2.5),
                ScalarValue::Double(3.5),
            ]),
        ));

        match get_nested_field(&pv, "samples[1]").as_deref() {
            Some(PvField::Scalar(ScalarValue::Double(v))) => assert_eq!(*v, 2.5),
            other => panic!("expected Scalar(Double(2.5)), got {other:?}"),
        }

        // Out-of-bounds index → None.
        assert!(get_nested_field(&pv, "samples[99]").is_none());
    }

    /// Mid-path index `field[N].child` must descend into a
    /// StructureArray element and continue navigating.
    #[test]
    fn nested_field_structure_array_index() {
        use epics_pva_rs::pvdata::ScalarValue;

        let mut elem0 = PvStructure::new("entry");
        elem0.fields.push((
            "name".into(),
            PvField::Scalar(ScalarValue::String("a".into())),
        ));
        let mut elem1 = PvStructure::new("entry");
        elem1.fields.push((
            "name".into(),
            PvField::Scalar(ScalarValue::String("b".into())),
        ));

        let mut pv = PvStructure::new("test");
        pv.fields.push((
            "entries".into(),
            PvField::StructureArray(vec![Some(elem0), Some(elem1)]),
        ));

        match get_nested_field(&pv, "entries[1].name").as_deref() {
            Some(PvField::Scalar(ScalarValue::String(s))) => assert_eq!(s, "b"),
            other => panic!("expected Scalar(String(\"b\")), got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // FieldDesc nested schema tests (for get_field introspection)
    // -----------------------------------------------------------------

    #[test]
    fn nested_desc_simple() {
        use epics_pva_rs::pvdata::ScalarType;

        let mut fields: Vec<(String, FieldDesc)> = Vec::new();
        set_nested_field_desc(&mut fields, "x", FieldDesc::Scalar(ScalarType::Double));
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "x");
        assert!(matches!(fields[0].1, FieldDesc::Scalar(ScalarType::Double)));
    }

    #[test]
    fn nested_desc_deep() {
        use epics_pva_rs::pvdata::ScalarType;

        let mut fields: Vec<(String, FieldDesc)> = Vec::new();
        set_nested_field_desc(
            &mut fields,
            "axis.position",
            FieldDesc::Scalar(ScalarType::Double),
        );
        set_nested_field_desc(
            &mut fields,
            "axis.velocity",
            FieldDesc::Scalar(ScalarType::Double),
        );

        // Should produce: [axis: structure { position: Double, velocity: Double }]
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "axis");
        if let FieldDesc::Structure { fields: sub, .. } = &fields[0].1 {
            assert_eq!(sub.len(), 2);
            assert_eq!(sub[0].0, "position");
            assert_eq!(sub[1].0, "velocity");
        } else {
            panic!("expected nested structure");
        }
    }

    #[test]
    fn nested_desc_overwrite() {
        use epics_pva_rs::pvdata::ScalarType;

        let mut fields: Vec<(String, FieldDesc)> = Vec::new();
        set_nested_field_desc(&mut fields, "x", FieldDesc::Scalar(ScalarType::Int));
        set_nested_field_desc(&mut fields, "x", FieldDesc::Scalar(ScalarType::Double));
        assert_eq!(fields.len(), 1);
        assert!(matches!(fields[0].1, FieldDesc::Scalar(ScalarType::Double)));
    }

    #[test]
    fn nested_desc_mixed_depth() {
        use epics_pva_rs::pvdata::ScalarType;

        let mut fields: Vec<(String, FieldDesc)> = Vec::new();
        set_nested_field_desc(&mut fields, "name", FieldDesc::Scalar(ScalarType::String));
        set_nested_field_desc(
            &mut fields,
            "axis.position",
            FieldDesc::Scalar(ScalarType::Double),
        );

        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "name");
        assert_eq!(fields[1].0, "axis");
    }

    // -----------------------------------------------------------------
    // FieldName parser tests
    // -----------------------------------------------------------------

    #[test]
    fn parse_field_path_simple() {
        let comps = parse_field_path("abc");
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].name, "abc");
        assert_eq!(comps[0].index, None);
    }

    #[test]
    fn parse_field_path_dotted() {
        let comps = parse_field_path("a.b.c");
        assert_eq!(comps.len(), 3);
        assert_eq!(comps[0].name, "a");
        assert_eq!(comps[1].name, "b");
        assert_eq!(comps[2].name, "c");
        assert!(comps.iter().all(|c| c.index.is_none()));
    }

    #[test]
    fn parse_field_path_with_index() {
        let comps = parse_field_path("a.b[0].c");
        assert_eq!(comps.len(), 3);
        assert_eq!(comps[0].name, "a");
        assert_eq!(comps[0].index, None);
        assert_eq!(comps[1].name, "b");
        assert_eq!(comps[1].index, Some(0));
        assert_eq!(comps[2].name, "c");
        assert_eq!(comps[2].index, None);
    }

    #[test]
    fn parse_field_path_index_at_leaf() {
        let comps = parse_field_path("arr[3]");
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].name, "arr");
        assert_eq!(comps[0].index, Some(3));
    }

    #[test]
    fn parse_field_path_multiple_indices() {
        let comps = parse_field_path("a[1].b[2]");
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].index, Some(1));
        assert_eq!(comps[1].index, Some(2));
    }

    // ---- BUG 4: atomic-group PUT serialization ----

    /// Build a two-member atomic group over `A:rec` / `B:rec`,
    /// returning the db and the parsed `GroupPvDef`.
    async fn atomic_group_fixture() -> (Arc<PvDatabase>, GroupPvDef) {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = Arc::new(PvDatabase::new());
        db.add_record("A:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("B:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "ATOMIC:GRP": {
                "+atomic": true,
                "a": {"+type": "plain", "+channel": "A:rec.VAL", "+putorder": 0},
                "b": {"+type": "plain", "+channel": "B:rec.VAL", "+putorder": 1}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        assert!(def.atomic);
        (db, def)
    }

    /// A `proc` group member WITHOUT `+putorder` must still process its
    /// target record on every group PUT, on both the atomic and
    /// non-atomic paths. pvxs's `doPostProcessing` returns true for
    /// `MappingInfo::Proc` independent of putable (groupsource.cpp:
    /// 547-573); a no-`+putorder` proc keeps the sentinel order
    /// (fieldconfig.h:37) and is still processed. Before the fix the
    /// PUT-candidate `filter_map(put_order)` dropped it, so a proc-only
    /// save/apply hook silently never ran. Observable: a freshly added
    /// AiRecord has `INIT=0`; its first process sets `INIT=1`.
    #[tokio::test]
    async fn proc_member_without_putorder_is_processed_atomic_and_nonatomic() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;

        async fn init_flag(db: &Arc<PvDatabase>, rec: &str) -> i64 {
            match db.get_pv(&format!("{rec}.INIT")).await.unwrap() {
                EpicsValue::Char(c) => c as i64,
                EpicsValue::Long(v) => v as i64,
                other => panic!("unexpected INIT type: {other:?}"),
            }
        }

        for atomic in [false, true] {
            let db = Arc::new(PvDatabase::new());
            db.add_record("HOOK:rec", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            // A proc-only member, no `+putorder`.
            let cfg = format!(
                r#"{{ "PROC:GRP": {{ "+atomic": {atomic},
                    "go": {{ "+type": "proc", "+channel": "HOOK:rec" }} }} }}"#
            );
            let mut defs = super::super::group_config::parse_group_config(&cfg).unwrap();
            let def = defs.pop().unwrap();
            let channel = GroupChannel::new(db.clone(), def);

            assert_eq!(
                init_flag(&db, "HOOK:rec").await,
                0,
                "fresh record not processed"
            );

            // An empty PUT still fires the proc hook (proc runs regardless
            // of which value fields the client supplied).
            channel
                .put(&PvStructure::new("structure"))
                .await
                .expect("proc-only group PUT must succeed");

            assert_eq!(
                init_flag(&db, "HOOK:rec").await,
                1,
                "proc member without +putorder must process its record (atomic={atomic})"
            );
        }
    }

    /// A QSRV group scalar member must derive its NT shape and DBF type
    /// from the configured channel's final field, not the owning record
    /// type: `REC.SCAN` is NTEnum, `REC.DESC` is NTScalar string, and a
    /// common string field on an enum record (`BI.DESC`) stays NTScalar
    /// string rather than being routed through the NTEnum encoder. Before
    /// the fix `introspect_member`/`decode_member` used
    /// `NtType::from_record_type` + a `field_list`-only DBF lookup that
    /// fell back to `pvDouble` for common fields.
    #[tokio::test]
    async fn group_scalar_member_nt_type_follows_configured_field_not_record_type() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::server::records::bi::BiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        // Pull the "value" FieldDesc out of a named group member.
        fn member_value_desc<'a>(group: &'a FieldDesc, name: &str) -> &'a FieldDesc {
            let FieldDesc::Structure { fields, .. } = group else {
                panic!("group descriptor must be a structure");
            };
            let member = &fields
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("member '{name}' missing from group descriptor"))
                .1;
            let FieldDesc::Structure { fields, .. } = member else {
                panic!("member '{name}' must be an NT structure");
            };
            &fields
                .iter()
                .find(|(n, _)| n == "value")
                .expect("member must have a value field")
                .1
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record("BR62:ai", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("BR62:bi", Box::new(BiRecord::new(0)))
            .await
            .unwrap();

        let cfg = r#"{
            "BR62:GRP": {
                "d":  {"+type": "scalar", "+channel": "BR62:ai.DESC"},
                "s":  {"+type": "scalar", "+channel": "BR62:ai.SCAN"},
                "bd": {"+type": "scalar", "+channel": "BR62:bi.DESC"}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let channel = GroupChannel::new(db.clone(), def);

        // ---- descriptor (introspect_member) ----
        let desc = channel.get_field().await.expect("get_field");
        assert_eq!(
            member_value_desc(&desc, "d"),
            &FieldDesc::Scalar(ScalarType::String),
            "REC.DESC member must advertise an NTScalar string value"
        );
        assert!(
            matches!(member_value_desc(&desc, "s"), FieldDesc::Structure { .. }),
            "REC.SCAN member must advertise an NTEnum value (index/choices \
             struct), got {:?}",
            member_value_desc(&desc, "s")
        );
        assert_eq!(
            member_value_desc(&desc, "bd"),
            &FieldDesc::Scalar(ScalarType::String),
            "BI.DESC must stay an NTScalar string even though the record \
             type is enum"
        );

        // ---- runtime value (decode_member) ----
        let val = channel
            .get(&PvStructure::new("structure"))
            .await
            .expect("group GET");
        let member = |name: &str| match val.get_field(name) {
            Some(PvField::Structure(s)) => s.clone(),
            other => panic!("member '{name}' value must be a structure, got {other:?}"),
        };
        // SCAN member's value is the enum_t sub-structure (index/choices).
        let s_val = member("s");
        assert!(
            matches!(s_val.get_field("value"), Some(PvField::Structure(_))),
            "REC.SCAN GET value must be an enum index/choices struct"
        );
        // DESC members' value is a plain string scalar.
        for name in ["d", "bd"] {
            let m = member(name);
            assert!(
                matches!(
                    m.get_field("value"),
                    Some(PvField::Scalar(ScalarValue::String(_)))
                ),
                "member '{name}' GET value must be a string scalar, got {:?}",
                m.get_field("value")
            );
        }
    }

    /// Regression. Two boundaries in one
    /// fixture:
    ///
    /// FR-6 — a member configured on a non-`VAL` field (`RVAL`) must
    /// register its value+property subscribers under that field, never
    /// under the record-default `VAL`. Before the fix the subscription
    /// stripped the suffix and bound to `VAL`, so the member woke on
    /// unrelated `VAL` posts and missed `RVAL`-only posts.
    ///
    /// FR-7 — the value-event mask is chosen per mapping: a `meta`
    /// member subscribes value events with `ALARM` only (pvxs
    /// `groupsource.cpp:386`), while non-meta members keep
    /// `VALUE | ALARM | LOG`. The `meta` member also retains its
    /// `PROPERTY` subscription.
    #[tokio::test]
    async fn br_fr6_fr7_group_subscribes_member_field_with_per_mapping_mask() {
        use crate::qsrv::provider::PvaMonitor;
        use epics_base_rs::server::recgbl::EventMask;
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("R:plain", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("R:meta", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let cfg = r#"{
            "FR6:GRP": {
                "p": {"+type": "plain", "+channel": "R:plain.RVAL"},
                "m": {"+type": "meta",  "+channel": "R:meta.VAL"}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let mut mon = GroupMonitor::new(db.clone(), def);
        mon.start().await.expect("group monitor starts");

        // FR-6: plain member subscribes its configured RVAL field, not VAL.
        let plain = db.get_record("R:plain").await.unwrap();
        let plain_inst = plain.read().await;
        assert!(
            plain_inst.subscribers.contains_key("RVAL"),
            "plain member must subscribe its configured field RVAL"
        );
        assert!(
            !plain_inst.subscribers.contains_key("VAL"),
            "plain member must NOT subscribe the record-default VAL"
        );
        // FR-7: a plain (non-meta) member has exactly one value
        // subscription (no property sub) with VALUE|ALARM|LOG.
        let rval_subs = &plain_inst.subscribers["RVAL"];
        assert_eq!(
            rval_subs.len(),
            1,
            "plain mapping opens only the value subscription"
        );
        assert_eq!(
            rval_subs[0].mask,
            (EventMask::VALUE | EventMask::ALARM | EventMask::LOG).bits(),
            "non-meta value mask must be VALUE|ALARM|LOG"
        );
        drop(plain_inst);

        // FR-7: meta member value subscription is ALARM-only; the
        // PROPERTY subscription is retained on the same field.
        let meta = db.get_record("R:meta").await.unwrap();
        let meta_inst = meta.read().await;
        let val_subs = &meta_inst.subscribers["VAL"];
        let masks: Vec<u16> = val_subs.iter().map(|s| s.mask).collect();
        assert!(
            masks.contains(&EventMask::ALARM.bits()),
            "meta value subscription must be ALARM-only, got {masks:?}"
        );
        assert!(
            masks.contains(&EventMask::PROPERTY.bits()),
            "meta member must retain its PROPERTY subscription, got {masks:?}"
        );
        assert!(
            !masks
                .iter()
                .any(|m| EventMask::from_bits(*m).intersects(EventMask::VALUE | EventMask::LOG)),
            "meta member must not wake on plain value/log posts, got {masks:?}"
        );
    }

    /// BRIDGE-114 regression: a group whose only members are `+const`
    /// has no backing channels, so it is primed by construction. It must
    /// NOT forward a manufactured initial snapshot through the monitor
    /// stream — the native PVA server already sends the initial frame via
    /// get_value_checked() at MONITOR INIT, so a forwarded snapshot would
    /// be a duplicate DATA frame for a value that never changes. After
    /// start(), poll() must yield nothing (it returns None as soon as the
    /// empty event channel closes), so the client sees exactly one DATA
    /// frame and no source-side update follows.
    #[tokio::test]
    async fn bridge114_all_const_group_monitor_emits_no_stream_snapshot() {
        use crate::qsrv::provider::PvaMonitor;

        let db = Arc::new(PvDatabase::new());
        let cfg = r#"{
            "CONST:GRP": {
                "a": {"+type": "const", "+const": 7},
                "b": {"+type": "const", "+const": "static"}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let mut mon = GroupMonitor::new(db.clone(), def);
        mon.start().await.expect("all-const group monitor starts");

        // poll() must NOT manufacture an initial snapshot. With no member
        // subscriptions the event channel is already closed, so poll()
        // resolves to None promptly (no manufactured DATA frame, no hang).
        let polled = tokio::time::timeout(Duration::from_millis(200), mon.poll()).await;
        assert!(
            matches!(polled, Ok(None)),
            "all-const group monitor must forward no stream snapshot, got {polled:?}"
        );
    }

    /// A PvStructure carrying `a` and `b` plain double values for the
    /// atomic group PUT path.
    fn atomic_put_value(a: f64, b: f64) -> PvStructure {
        use epics_pva_rs::pvdata::ScalarValue;
        let mut pv = PvStructure::new("structure");
        pv.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Double(a))));
        pv.fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Double(b))));
        pv
    }

    /// BUG 4 regression: an atomic-group PUT holds the group's shared
    /// `atomic_write_lock` for the whole member-write loop, so a
    /// concurrent PUT to the same atomic group cannot run a member
    /// write in between. Pre-fix the atomic branch `.await`-ed each
    /// member write with no cross-write lock, letting a second PUT
    /// interleave and leave the group observably half-applied.
    #[tokio::test]
    async fn bug4_atomic_put_serializes_on_group_lock() {
        let (db, def) = atomic_group_fixture().await;
        let channel = GroupChannel::new(db.clone(), def.clone());

        // Hold the group's atomic_write_lock — exactly the guard the
        // atomic PUT branch acquires. While held, a `put` on the same
        // group def must not be able to enter the member-write loop.
        let guard = def.atomic_write_lock.clone().lock_owned().await;

        let put_fut = tokio::spawn(async move {
            channel.put(&atomic_put_value(11.0, 22.0)).await.unwrap();
        });

        // The PUT must still be blocked on the lock.
        let blocked = tokio::time::timeout(Duration::from_millis(150), async {}).await;
        assert!(blocked.is_ok());
        assert!(
            !put_fut.is_finished(),
            "atomic PUT must block while another holder owns atomic_write_lock"
        );

        // Release the lock — the PUT now proceeds and completes.
        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), put_fut)
            .await
            .expect("atomic PUT must complete once the lock is free")
            .expect("put task did not panic");

        // Both member records received the written values.
        let a = db.get_pv("A:rec.VAL").await.unwrap();
        let b = db.get_pv("B:rec.VAL").await.unwrap();
        match (a, b) {
            (
                epics_base_rs::types::EpicsValue::Double(va),
                epics_base_rs::types::EpicsValue::Double(vb),
            ) => {
                assert_eq!(va, 11.0);
                assert_eq!(vb, 22.0);
            }
            other => panic!("unexpected member values: {other:?}"),
        }
    }

    /// CRITICAL regression: an atomic-group `read_group` must not
    /// deadlock when a plain writer is contending for a member
    /// record's `RwLock`. Pre-fix `read_group` held an
    /// `OwnedRwLockReadGuard` on every member record, then
    /// `read_member` called `rec.read().await` on the SAME lock — a
    /// recursive read that, with a write-preferring `tokio::RwLock`
    /// and a writer queued in between, deadlocked unresolvably. The
    /// fixed path resolves members against the pre-held guards and
    /// never re-locks, so a concurrent writer cannot wedge it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn critical_atomic_read_group_no_deadlock_under_writer() {
        let (db, def) = atomic_group_fixture().await;
        let channel = GroupChannel::new(db.clone(), def.clone());

        // Spawn a writer that hammers a member record (A:rec) — each
        // `put_pv` acquires the record write lock. Without the fix, a
        // writer queued between the read_group guard and the
        // read_member re-lock wedges the reader forever.
        let writer_db = db.clone();
        let writer = tokio::spawn(async move {
            for i in 0..200 {
                let _ = writer_db
                    .put_pv(
                        "A:rec.VAL",
                        epics_base_rs::types::EpicsValue::Double(i as f64),
                    )
                    .await;
                tokio::task::yield_now().await;
            }
        });

        // Concurrently read the atomic group many times. Each
        // `read_group` must complete — a deadlock would hang the
        // timeout below.
        let reader = tokio::spawn(async move {
            for _ in 0..200 {
                channel
                    .read_group()
                    .await
                    .expect("atomic read_group must succeed");
                tokio::task::yield_now().await;
            }
        });

        tokio::time::timeout(Duration::from_secs(10), async {
            reader.await.expect("reader task panicked");
            writer.await.expect("writer task panicked");
        })
        .await
        .expect("atomic read_group deadlocked under a concurrent writer");
    }

    /// CRITICAL regression: the atomic read path returns the same
    /// values the non-atomic path would — proves `read_member_locked`
    /// (guard reuse) decodes identically to `read_member` (self-lock).
    #[tokio::test]
    async fn critical_atomic_read_group_returns_member_values() {
        let (db, def) = atomic_group_fixture().await;
        db.put_pv("A:rec.VAL", epics_base_rs::types::EpicsValue::Double(7.5))
            .await
            .unwrap();
        db.put_pv("B:rec.VAL", epics_base_rs::types::EpicsValue::Double(9.25))
            .await
            .unwrap();

        let channel = GroupChannel::new(db.clone(), def);
        let pv = channel.read_group().await.unwrap();

        match get_nested_field(&pv, "a").as_deref() {
            Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(v))) => {
                assert_eq!(*v, 7.5)
            }
            other => panic!("member a: expected Double(7.5), got {other:?}"),
        }
        match get_nested_field(&pv, "b").as_deref() {
            Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(v))) => {
                assert_eq!(*v, 9.25)
            }
            other => panic!("member b: expected Double(9.25), got {other:?}"),
        }
    }

    /// BR-123: a group with no top-level `+id` advertises an EMPTY root
    /// structure ID on BOTH the value and the descriptor — pvxs leaves
    /// `GroupDefinition::structureId` empty and builds
    /// `TypeDef(TypeCode::Struct, "", {})` (groupconfigprocessor.cpp:
    /// 517-523). The prior Rust-only `"structure"` literal changed the
    /// group's public type identity. An explicit `+id` still wins.
    #[tokio::test]
    async fn br_123_group_root_struct_id_empty_without_plus_id() {
        use epics_base_rs::server::records::ai::AiRecord;
        let db = Arc::new(PvDatabase::new());
        db.add_record("R:rec", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();

        // No top-level +id → empty root struct ID on value and descriptor.
        let cfg = r#"{ "G:noid": { "a": {"+type":"plain","+channel":"R:rec.VAL"} } }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();
        let channel = GroupChannel::new(db.clone(), def);
        let pv = channel.read_group().await.unwrap();
        assert_eq!(
            pv.struct_id, "",
            "value root struct ID must be empty without +id"
        );
        match channel.get_field().await.unwrap() {
            FieldDesc::Structure { struct_id, .. } => {
                assert_eq!(
                    struct_id, "",
                    "descriptor root struct ID must be empty without +id"
                );
            }
            other => panic!("expected root Structure descriptor, got {other:?}"),
        }

        // Explicit +id wins on both value and descriptor.
        let cfg2 = r#"{ "G:id": { "+id": "epics:nt/NTScalar:1.0", "a": {"+type":"plain","+channel":"R:rec.VAL"} } }"#;
        let def2 = super::super::group_config::parse_group_config(cfg2)
            .unwrap()
            .pop()
            .unwrap();
        let channel2 = GroupChannel::new(db.clone(), def2);
        let pv2 = channel2.read_group().await.unwrap();
        assert_eq!(pv2.struct_id, "epics:nt/NTScalar:1.0");
        match channel2.get_field().await.unwrap() {
            FieldDesc::Structure { struct_id, .. } => {
                assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
            }
            other => panic!("expected root Structure descriptor, got {other:?}"),
        }
    }

    /// BUG 4 regression: two concurrent atomic-group PUTs serialize —
    /// the second cannot start its member-write loop until the first
    /// releases `atomic_write_lock`. With the lock removed the two
    /// `.await`-ing loops would interleave member writes.
    #[tokio::test]
    async fn bug4_concurrent_atomic_puts_do_not_interleave() {
        let (db, def) = atomic_group_fixture().await;

        let ch1 = GroupChannel::new(db.clone(), def.clone());
        let ch2 = GroupChannel::new(db.clone(), def.clone());

        // Pre-acquire the lock so PUT #1 blocks deterministically;
        // start both PUTs, then release. They must run strictly
        // serially through the shared lock.
        let guard = def.atomic_write_lock.clone().lock_owned().await;
        let p1 = tokio::spawn(async move {
            ch1.put(&atomic_put_value(1.0, 1.0)).await.unwrap();
        });
        let p2 = tokio::spawn(async move {
            ch2.put(&atomic_put_value(2.0, 2.0)).await.unwrap();
        });
        // Neither PUT can proceed while the lock is held externally.
        tokio::time::timeout(Duration::from_millis(120), async {})
            .await
            .ok();
        assert!(!p1.is_finished() && !p2.is_finished());
        drop(guard);

        tokio::time::timeout(Duration::from_secs(5), async {
            p1.await.unwrap();
            p2.await.unwrap();
        })
        .await
        .expect("both atomic PUTs must complete");

        // Final state is one of the two PUTs fully applied — never a
        // mix (1.0,2.0) / (2.0,1.0). The lock guarantees the loops did
        // not interleave member writes.
        let a = db.get_pv("A:rec.VAL").await.unwrap();
        let b = db.get_pv("B:rec.VAL").await.unwrap();
        match (a, b) {
            (
                epics_base_rs::types::EpicsValue::Double(va),
                epics_base_rs::types::EpicsValue::Double(vb),
            ) => {
                assert_eq!(
                    va, vb,
                    "atomic group must not be half-applied: a={va} b={vb}"
                );
                assert!(va == 1.0 || va == 2.0);
            }
            other => panic!("unexpected member values: {other:?}"),
        }
    }

    // ---- atomic group PUT is DBManyLock-equivalent ----

    /// Regression: a QSRV atomic group PUT must exclude a
    /// *direct* CA/PVA write to a backing member record for the whole
    /// member-write loop — pvxs holds a `DBManyLocker`
    /// (`groupsource.cpp:569`) over the same per-record locks a plain
    /// `dbPutField` takes. Pre-fix the atomic PUT only held the
    /// per-group `atomic_write_lock`, which a non-group writer never
    /// consults, so a direct backing-record write could land between
    /// member writes and leave the group observably half-applied.
    ///
    /// This test holds the `DBManyLock`-equivalent gate set
    /// (`PvDatabase::lock_records`) over the member records — exactly
    /// what `GroupChannel::put`'s atomic branch acquires — and proves
    /// a direct `put_record_field_from_ca` to a member record blocks
    /// until that gate set is released. On `main` `lock_records` does
    /// not exist and `put_record_field_from_ca` takes no such gate,
    /// so this fix-defining behaviour is absent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn br_r15_atomic_group_excludes_direct_member_write() {
        let (db, _def) = atomic_group_fixture().await;

        // Hold the member-record write gates — the in-flight atomic
        // group PUT's `DBManyLocker` equivalent.
        let many = db.lock_records(["A:rec", "B:rec"]).await;

        // A direct CA write to a member record must block on the same
        // gate (`put_record_field_from_ca` takes `lock_record`).
        let db_w = db.clone();
        let direct = tokio::spawn(async move {
            db_w.put_record_field_from_ca(
                "A:rec",
                "VAL",
                epics_base_rs::types::EpicsValue::Double(99.0),
            )
            .await
            .unwrap();
        });

        tokio::time::timeout(Duration::from_millis(150), async {})
            .await
            .ok();
        assert!(
            !direct.is_finished(),
            "direct member write must block while the atomic group's \
             DBManyLock-equivalent gates are held"
        );

        // Release the gate set: the direct write now proceeds.
        drop(many);
        tokio::time::timeout(Duration::from_secs(5), direct)
            .await
            .expect("direct write must complete once gates are released")
            .expect("direct write task panicked");

        match db.get_pv("A:rec.VAL").await.unwrap() {
            epics_base_rs::types::EpicsValue::Double(v) => assert_eq!(v, 99.0),
            other => panic!("unexpected A:rec.VAL: {other:?}"),
        }
    }

    /// Regression: the real `GroupChannel::put` atomic path
    /// itself acquires the member-record gate set. Holding the gates
    /// externally must block an atomic group PUT from entering its
    /// member-write loop, and the PUT must complete once released.
    /// This proves the atomic PUT uses `lock_records`, not only the
    /// per-group `atomic_write_lock`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn br_r15_atomic_put_blocks_on_member_record_gates() {
        let (db, def) = atomic_group_fixture().await;
        let channel = GroupChannel::new(db.clone(), def.clone());

        // Hold one member record's gate. The atomic group PUT must
        // block trying to acquire it via `lock_records`.
        let held = db.lock_record("B:rec").await;

        let put = tokio::spawn(async move {
            channel.put(&atomic_put_value(5.0, 6.0)).await.unwrap();
        });

        tokio::time::timeout(Duration::from_millis(150), async {})
            .await
            .ok();
        assert!(
            !put.is_finished(),
            "atomic group PUT must block while a member-record gate is held"
        );

        drop(held);
        tokio::time::timeout(Duration::from_secs(5), put)
            .await
            .expect("atomic PUT must complete once the member gate is free")
            .expect("atomic PUT task panicked");

        let a = db.get_pv("A:rec.VAL").await.unwrap();
        let b = db.get_pv("B:rec.VAL").await.unwrap();
        match (a, b) {
            (
                epics_base_rs::types::EpicsValue::Double(va),
                epics_base_rs::types::EpicsValue::Double(vb),
            ) => {
                assert_eq!(va, 5.0);
                assert_eq!(vb, 6.0);
            }
            other => panic!("unexpected member values: {other:?}"),
        }
    }

    /// A partial group PUT must not access-check unmarked members. pvxs
    /// builds the per-field SecurityClient over the *changed* fields
    /// (groupsource.cpp:161,515,547-567), so an unwritable member that
    /// the client did not mark cannot reject a PUT to an unrelated
    /// marked member. Whole-value callers still check every member.
    #[tokio::test]
    async fn br120_partial_put_skips_access_check_for_unmarked_member() {
        use super::super::provider::{AccessContext, AccessControl};
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        // Deny writes to member b's backing channel only.
        struct DenyChannel(&'static str);
        impl AccessControl for DenyChannel {
            fn can_write(&self, channel: &str, _user: &str, _host: &str) -> bool {
                channel != self.0
            }
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record("PA:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("PB:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "DENY:GRP": {
                "+atomic": false,
                "a": {"+type":"plain","+channel":"PA:rec.VAL","+putorder":0},
                "b": {"+type":"plain","+channel":"PB:rec.VAL","+putorder":1}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();

        let access = AccessContext::with_identity(
            Arc::new(DenyChannel("PB:rec.VAL")),
            "u".into(),
            "h".into(),
        );
        let channel = GroupChannel::new(db.clone(), def).with_access(access);

        // Partial PUT marking only `a` (b absent): b is unmarked, so its
        // write-deny must not be checked and the PUT must succeed.
        let mut partial = PvStructure::new("structure");
        partial
            .fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Double(5.0))));
        channel
            .put_with_options(
                &partial,
                super::super::channel::PutOptions::default(),
                Some(false),
            )
            .await
            .expect("partial PUT to writable a must not be blocked by unwritable unmarked b");

        // Full PUT (both present): b is now acted on, so its write-deny
        // rejects the whole PUT.
        let mut full = PvStructure::new("structure");
        full.fields
            .push(("a".into(), PvField::Scalar(ScalarValue::Double(6.0))));
        full.fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Double(7.0))));
        let res = channel
            .put_with_options(
                &full,
                super::super::channel::PutOptions::default(),
                Some(false),
            )
            .await;
        assert!(
            matches!(res, Err(BridgeError::PutRejected(_))),
            "full PUT including denied member b must be rejected, got {res:?}"
        );
    }
}
