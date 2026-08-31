//! GroupChannel and GroupMonitor: multi-record composite PVA channel.
//!
//! Corresponds to C++ QSRV's `PDBGroupPV` / `PDBGroupChannel` / `PDBGroupMonitor`.
//! A group PV combines fields from multiple EPICS database records
//! into a single PvStructure.

// RTEMS-EXEC-MODEL-ALLOW(34): checked - these run and pass in the exec-backend
// suite.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::{DbSubscription, SubscriptionActivation};
use epics_base_rs::server::recgbl::EventMask as DbeMask;
use epics_base_rs::server::snapshot::PropertySupport;
use epics_base_rs::types::DbFieldType;
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, VariantValue};
use epics_pva_rs::server_native::source::RemoteLog;

use super::channel::MemberChannel;
use super::group_config::{GroupMember, GroupPvDef, TriggerDef};
use super::monitor::BridgeMonitor;
use super::pvif::{self, FieldMapping, NtType};
use crate::convert::dbf_to_scalar_type;
use crate::error::{BridgeError, BridgeResult};

// ---------------------------------------------------------------------------
// FieldName — path parser with array index support (pvxs fieldname.h)
// ---------------------------------------------------------------------------

/// A single component in a field path: `name` with optional `[index]`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FieldNameComponent {
    name: String,
    index: Option<u32>,
}

/// Parse a field path like `"a.b[0].c"` into components, enforcing the pvxs
/// `FieldName` constructor grammar (`ioc/fieldname.cpp:29-67`).
///
/// This is the single source of truth for the group field-name grammar. pvxs
/// splits the path on `.` with `std::getline` and **throws** — aborting that
/// group's build — on a malformed path; the throw set is byte-faithful to
/// `getline`/`strtol`:
///   - empty input → no components (pvxs skips the split; no throw);
///   - an empty leading or interior component (`.a`, `a..b`, `.`) is an error;
///     a single terminal empty from a trailing `.` (`a.`) is dropped at EOF
///     (`getline` fails on a zero-length extraction at end-of-stream) and is
///     NOT an error;
///   - a component is array-indexed only when it ENDS with `]`, in which case
///     it must contain a `[` and a non-negative decimal subscript (`a[x]`,
///     `a[1x]` are errors: the subscript is not a clean integer). A component
///     with no trailing `]` (`a[`) is a plain literal name, bracket included.
///
/// The infallible [`parse_field_path`] wraps this for navigating names already
/// validated at group-build time; `group_config::validate_field_name` calls it
/// to reject a malformed member (a per-group skip, matching pvxs's per-group
/// `try` at `groupconfigprocessor.cpp:431-446`).
///
/// Divergence note: this subscript grammar is intentionally STRICTER than pvxs
/// and rejects three degenerate forms that pvxs's `strtol`
/// (`fieldname.cpp:48-53`) accepts — we deliberately do NOT replicate the
/// `strtol` accidents:
///   - an empty subscript `a[]` — `strtol("]")` performs no conversion, returns
///     0, and leaves `endScan` on the `]`, so pvxs silently reads it as element
///     0; `"".parse::<u32>()` errors here;
///   - a whitespace/sign-padded subscript `a[ 5]` — `strtol` skips leading
///     whitespace and an optional sign and reads 5; `" 5".parse::<u32>()`
///     errors here;
///   - a negative or `u32`-overflowing subscript (`a[-1]`, `a[99999999999]`) —
///     `strtol` accepts it and only fails later at navigation.
///
/// Group array indices are non-negative and bounded, so none of these could
/// navigate to a real element; rejecting them at build is stricter-but-safe and
/// never touches a real config. All three are build-time-only divergences.
pub(crate) fn parse_field_path_checked(path: &str) -> Result<Vec<FieldNameComponent>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }

    // Replicate `while (getline(splitter, part, '.'))`: the terminal empty
    // produced by a single trailing '.' is dropped at EOF and never reaches
    // the empty-component check; every other empty is an error.
    let raw: Vec<&str> = path.split('.').collect();
    let count = if path.ends_with('.') {
        raw.len() - 1
    } else {
        raw.len()
    };

    let mut components = Vec::with_capacity(count);
    for part in &raw[..count] {
        if part.is_empty() {
            return Err(format!("Empty field component in: {path}"));
        }
        if let Some(part) = part.strip_suffix(']') {
            // Ends with ']': pvxs treats it as an array reference and requires
            // a '[' with an integer subscript in between.
            let open = part
                .rfind('[')
                .ok_or_else(|| format!("Invalid field array sub-script in : {path}"))?;
            let index: u32 = part[open + 1..]
                .parse()
                .map_err(|_| format!("Invalid field array sub-script in : {path}"))?;
            components.push(FieldNameComponent {
                name: part[..open].to_string(),
                index: Some(index),
            });
        } else {
            components.push(FieldNameComponent {
                name: part.to_string(),
                index: None,
            });
        }
    }
    Ok(components)
}

/// Parse a field path for navigation of a name already validated at
/// group-build time. Delegates to the canonical [`parse_field_path_checked`];
/// a name that somehow fails the grammar yields no components (navigation then
/// reports not-found), never a silently-normalized different structure.
fn parse_field_path(path: &str) -> Vec<FieldNameComponent> {
    parse_field_path_checked(path).unwrap_or_default()
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

/// Which pvxs `record._options` stamping a composed group value carries.
///
/// The two stamped members (`atomic`, `queueSize`) are one decision — the
/// operation kind — so they are one field. pvxs has two writers and no
/// third state:
///
///   * the static `onGet` (GET, `groupsource.cpp:480-485`) stamps the
///     *operation* atomicity and never touches `queueSize`, so a GET
///     reports the value-template default `0` (`test/testqgroup.cpp:60-66`)
///     — a GET has no subscription queue.
///   * `GroupSource::onSubscribe` (MONITOR, `groupsource.cpp:401-405`)
///     stamps `atomic = true` unconditionally and
///     `queueSize = stats.limitQueue`.
///
/// `limit` is that `stats.limitQueue`: the depth the SERVER negotiated
/// (`MonitorOp::limit`, servermon.cpp:313), handed to the source as
/// [`epics_pva_rs::server_native::MonitorOptions::queue_size`]. pvxs asks
/// the subscription control what it GOT; it never re-reads the client's
/// `record._options.queueSize` in the group source. The port used to parse
/// that option a second time here — a decimal-only, i32 parser that
/// disagreed with the server's own `Value::as<uint32>` negotiation on hex
/// strings, reals, and `>= 2^31` values (R10-33).
#[derive(Clone, Copy, Debug)]
enum OptionsStamp {
    /// The GET path (pvxs's static `onGet`).
    Get,
    /// The MONITOR path (`GroupSource::onSubscribe`), carrying the
    /// negotiated queue limit.
    Monitor { limit: u32 },
}

/// stamp `record._options.queueSize` (int) and
/// `record._options.atomic` (boolean) onto a group GET / MONITOR
/// value. Merged under the existing `record` structure (replacing only
/// the `_options` subtree if it already exists, e.g. composed by an
/// earlier read), so any user-configured `record.*` member — a group
/// field mapped under `record`, such as `record.status` — is preserved.
/// pvxs adds the built-in `record._options` branch to the same member
/// vector and `TypeDef::_append()` recursively merges matching compound
/// children (ioc/groupconfigprocessor.cpp:499-524, src/type.cpp:374-389);
/// a whole-`record` replacement would drop those user fields.
///
/// The SINGLE writer of both stamped members. [`OptionsStamp`] decides
/// them together: a GET reports the operation atomicity `op_atomic` and
/// `queueSize = 0`; a MONITOR reports `atomic = true` and the negotiated
/// limit. `op_atomic` is ignored on the MONITOR path — pvxs stamps `true`
/// there unconditionally (`groupsource.cpp:401-405`).
/// Put the built-in `record` branch at member 0, creating it empty when
/// no group member built one.
///
/// pvxs pushes the `record` Struct onto `groupMembersToAdd` BEFORE
/// `addTemplatesForDefinedFields` and appends the whole vector in that
/// order (`groupconfigprocessor.cpp:502-519`), while `TypeDef::_append`
/// merges a user `record` member into that same branch instead of adding
/// a second one (`type.cpp:374-389`). So upstream `record` is member 0
/// whichever half created it, which is what fixes `record._options
/// .queueSize` at bit 3 and `atomic` at bit 4 and leaves every user
/// member's bit index behind them. Both the descriptor and the value
/// composer stamp the branch after their member loop, so without this the
/// branch landed last and shifted every user bit for the same group
/// definition.
fn hoist_record_member<T>(fields: &mut Vec<(String, T)>, empty: impl FnOnce() -> T) {
    match fields.iter().position(|(n, _)| n == "record") {
        Some(0) => {}
        Some(pos) => {
            let entry = fields.remove(pos);
            fields.insert(0, entry);
        }
        None => fields.insert(0, ("record".to_string(), empty())),
    }
}

fn push_record_options(pv: &mut PvStructure, op_atomic: bool, stamp: OptionsStamp) {
    use epics_pva_rs::pvdata::ScalarValue;
    let (atomic, queue_size) = match stamp {
        OptionsStamp::Get => (op_atomic, 0),
        // pvxs assigns the `size_t` `stats.limitQueue` into the int32
        // `record._options.queueSize` member, a truncating cast — which
        // `as` reproduces for the wrapped limits a client can negotiate
        // (`queueSize = -1` converts to `op->limit = 0xFFFF_FFFF`).
        OptionsStamp::Monitor { limit } => (true, limit as i32),
    };
    let mut options = PvStructure::new("");
    options.fields.push((
        "queueSize".into(),
        PvField::Scalar(ScalarValue::Int(queue_size)),
    ));
    options.fields.push((
        "atomic".into(),
        PvField::Scalar(ScalarValue::Boolean(atomic)),
    ));
    // Merge the built-in options under `record._options`, navigating into
    // an existing `record` structure and replacing only the `_options`
    // child — user `record.*` siblings are left intact.
    hoist_record_member(&mut pv.fields, || PvField::Structure(PvStructure::new("")));
    set_nested_field(pv, "record._options", PvField::Structure(options));
}

/// Descriptor twin of [`push_record_options`]: the introspection shape
/// of the built-in `record._options` subtree *content* (`queueSize` int,
/// `atomic` boolean). The caller merges it under `record._options` via
/// `set_nested_field_desc`, so any user `record.*` member descriptor is
/// preserved. pvxs builds this branch into
/// `group.valueTemplate` via a recursive `TypeDef::_append()` merge
/// (ioc/groupconfigprocessor.cpp:499-524, src/type.cpp:374-389), so
/// CREATE_CHANNEL / GET_FIELD negotiation advertises it and every
/// GET/MONITOR value conforms. Keep the field names and scalar types here
/// in lockstep with `push_record_options` so the descriptor never
/// diverges from the value.
fn record_options_inner_field_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![
            ("queueSize".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("atomic".into(), FieldDesc::Scalar(ScalarType::Boolean)),
        ],
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

    // An indexed component (`field[N]…`) addresses a StructureArray
    // element, not a plain sub-structure. pvxs builds the group type
    // leaf-to-root and wraps indexed components with `StructA(...)`
    // (groupconfigprocessor.cpp:1005-1037); the runtime value then lands
    // in element `[N]` of that structure array (groupsource.cpp:414-425).
    if let Some(idx) = comp.index {
        let arr = get_or_create_struct_array_field(pv, &comp.name);
        let i = idx as usize;
        if arr.len() <= i {
            arr.resize_with(i + 1, || None);
        }
        if components.len() == 1 {
            // Terminal indexed component: the element itself is the value.
            // Only a Structure can inhabit a StructureArray element; group
            // configs always recurse into a child field after the index,
            // so a scalar terminal index has no representation and is
            // dropped.
            if let PvField::Structure(s) = value {
                arr[i] = Some(s);
            }
            return;
        }
        let element = arr[i].get_or_insert_with(|| PvStructure::new(""));
        set_nested_field_recursive(element, &components[1..], value);
        return;
    }

    // Plain (non-indexed) intermediate: navigate/create a sub-structure.
    let sub = get_or_create_struct_field(pv, &comp.name);
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

/// Find or create a named `StructureArray` field within `pv`, returning
/// its element vector. A field that exists but is not already a
/// `StructureArray` (e.g. a plain Structure built before an indexed
/// component was seen) is replaced — the configured `[N]` notation is
/// authoritative for the field's shape.
fn get_or_create_struct_array_field<'a>(
    pv: &'a mut PvStructure,
    name: &str,
) -> &'a mut Vec<Option<PvStructure>> {
    let pos = pv.fields.iter().position(|(n, _)| n == name);
    if let Some(pos) = pos {
        if !matches!(pv.fields[pos].1, PvField::StructureArray(_)) {
            pv.fields[pos].1 = PvField::StructureArray(Vec::new());
        }
        if let PvField::StructureArray(ref mut v) = pv.fields[pos].1 {
            v
        } else {
            unreachable!()
        }
    } else {
        pv.fields
            .push((name.to_string(), PvField::StructureArray(Vec::new())));
        if let PvField::StructureArray(ref mut v) = pv.fields.last_mut().unwrap().1 {
            v
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

    // Indexed component → StructureArray descriptor (pvxs `StructA`,
    // groupconfigprocessor.cpp:1005-1037), symmetric with the value
    // builder. Members at different indices of the same array share one
    // element schema, so their leaf descriptors accumulate into the same
    // element field list.
    if comp.index.is_some() {
        if components.len() == 1 {
            // Terminal indexed component: a Structure leaf supplies the
            // element schema directly; any other leaf yields an empty
            // element type.
            let (struct_id, elem_fields) = match leaf {
                FieldDesc::Structure { struct_id, fields } => (struct_id, fields),
                _ => (String::new(), Vec::new()),
            };
            let sa = FieldDesc::StructureArray {
                struct_id,
                fields: elem_fields,
            };
            if let Some(pos) = fields.iter().position(|(n, _)| n == &comp.name) {
                fields[pos].1 = sa;
            } else {
                fields.push((comp.name.clone(), sa));
            }
            return;
        }
        let elem_fields = get_or_create_struct_array_desc(fields, &comp.name);
        set_nested_field_desc_recursive(elem_fields, &components[1..], leaf);
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

/// Find or create a named `StructureArray` descriptor within `fields`,
/// returning its element field list. Symmetric with
/// [`get_or_create_struct_array_field`] on the value side; a field that
/// exists but is not a `StructureArray` is replaced.
fn get_or_create_struct_array_desc<'a>(
    fields: &'a mut Vec<(String, FieldDesc)>,
    name: &str,
) -> &'a mut Vec<(String, FieldDesc)> {
    if let Some(pos) = fields.iter().position(|(n, _)| n == name) {
        if !matches!(fields[pos].1, FieldDesc::StructureArray { .. }) {
            fields[pos].1 = FieldDesc::StructureArray {
                struct_id: String::new(),
                fields: Vec::new(),
            };
        }
        if let FieldDesc::StructureArray { fields: f, .. } = &mut fields[pos].1 {
            f
        } else {
            unreachable!()
        }
    } else {
        fields.push((
            name.to_string(),
            FieldDesc::StructureArray {
                struct_id: String::new(),
                fields: Vec::new(),
            },
        ));
        if let FieldDesc::StructureArray { fields: f, .. } = &mut fields.last_mut().unwrap().1 {
            f
        } else {
            unreachable!()
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic multi-record locking (pvxs DBManyLocker equivalent)
// ---------------------------------------------------------------------------

/// Collect the records backing a group's members, in sorted order to prevent
/// deadlocks. Corresponds to C++ QSRV `DBManyLocker` (dbmanylocker.h). The
/// caller acquires the per-record read guards synchronously from these handles:
/// a `parking_lot` read guard is `!Send`, so it cannot be returned across this
/// `async fn`'s `.await` boundary. The advisory gate the caller holds over the
/// same record set keeps that synchronous acquisition uncontended and
/// deadlock-free (no writer can hold any member's write guard meanwhile).
async fn lock_group_records_read(
    db: &PvDatabase,
    members: &[MemberChannel],
) -> Vec<(
    String,
    Arc<parking_lot::RwLock<epics_base_rs::server::record::RecordInstance>>,
)> {
    // Collect unique record names and sort for deterministic lock order.
    let mut record_names: Vec<String> = members
        .iter()
        .filter(|m| m.has_channel())
        .map(|m| m.record.clone())
        .collect();
    record_names.sort();
    record_names.dedup();

    let mut records = Vec::new();
    for name in &record_names {
        if let Some(rec) = db.get_record(name) {
            records.push((name.clone(), rec));
        }
    }
    records
}

/// collect the **canonical** record names backing a group's
/// writable members, for the `DBManyLock`-equivalent write gate.
///
/// pvxs builds `group.value.lock` (a `DBManyLock`) over every member
/// record (`groupconfigprocessor.cpp:1165`) and takes a `DBManyLocker`
/// across the whole atomic PUT loop (`groupsource.cpp:619-630`). The Rust
/// equivalent is [`PvDatabase::lock_records`] over the same record
/// set. Names are resolved through the alias map so the gate key
/// matches the one a direct CA/PVA write would take in
/// `put_record_field_from_ca` / `put_pv` / `process_record`.
fn group_member_record_names(db: &PvDatabase, members: &[MemberChannel]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for m in members {
        if !m.has_channel() {
            continue; // Structure / Const — no backing record
        }
        let canonical = db
            .resolve_alias(&m.record)
            .unwrap_or_else(|| m.record.clone());
        names.push(canonical);
    }
    names.sort();
    names.dedup();
    names
}

// ---------------------------------------------------------------------------
// Group PUT member classification
// ---------------------------------------------------------------------------

/// What a group PUT does with one member — pvxs `putGroupField`
/// (`groupsource.cpp:547-574`), whose two predicates are INDEPENDENT:
///
/// ```text
/// putable  = putOrder != int64_t::min()          // an explicit +putorder
/// marked   = leafNode.isMarked(true,true) && field.value   // client sent it,
///                                                          // and it has a channel
/// changing = marked && putable
///
/// if (changing)                    { doFieldPreProcessing(); IOCSource::put(); }
/// if (changing || type == Proc)    { doPostProcessing(); return true; }
/// ```
///
/// `IOCSource::put` (`iocsource.cpp:576-610`) writes for Scalar/Plain/Any and
/// returns without writing for Meta/Proc/Structure/Const. The port used to fuse
/// "has no writable leaf" with "does not participate" and skipped a `changing`
/// Meta member outright — so a Meta member with an explicit `+putorder`
/// processed its record in pvxs and did nothing here (R17-37).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberPutAction {
    /// `changing`, and `IOCSource::put` has a leaf to write.
    Write,
    /// Post-process only, no database write. `acf_checked` is C's
    /// `doFieldPreProcessing` (the `canWrite` gate), which runs for every
    /// `changing` field — including a Meta member that writes nothing — and
    /// never for a `proc` trigger, which is not `changing`.
    ProcessOnly { acf_checked: bool },
    /// Unmarked, or marked without `+putorder` (the not-putable sentinel), or
    /// no backing channel (Structure / Const).
    Skip,
}

/// The one classifier. Both PUT loops (atomic and non-atomic) and the
/// per-member ACF pass ask it; none of them re-derives "is this member
/// writable / marked / participating" on its own.
fn member_put_action(m: &GroupMember, value: &PvStructure) -> MemberPutAction {
    // `field.value` — a Structure/Const member has no dbChannel, so pvxs's
    // `marked` is false for it whatever the client sent.
    let marked = !m.channel.is_empty() && get_nested_field(value, &m.field_name).is_some();
    let putable = m.put_order.is_some();
    let changing = marked && putable;

    if changing && m.mapping.is_client_writable() {
        MemberPutAction::Write
    } else if changing {
        // Marked, putable, but `IOCSource::put` writes nothing for this mapping
        // (Meta). C still runs the write-ACF gate and still post-processes.
        MemberPutAction::ProcessOnly { acf_checked: true }
    } else if m.mapping == FieldMapping::Proc {
        // A `proc` trigger runs on every group PUT regardless of marks and of
        // `+putorder` (`changing || type==Proc`), and C never asks `canWrite`
        // for it — `dbProcess` is not a `dbPutField`.
        MemberPutAction::ProcessOnly { acf_checked: false }
    } else {
        MemberPutAction::Skip
    }
}

// ---------------------------------------------------------------------------
// GroupChannel
// ---------------------------------------------------------------------------

/// A PVA channel backed by a group of EPICS database records.
#[derive(Clone)]
pub struct GroupChannel {
    db: Arc<PvDatabase>,
    def: GroupPvDef,
    access: super::provider::AccessContext,
    /// Which pvxs `record._options` stamping this channel's composed
    /// values carry. [`OptionsStamp::Get`] unless a `GroupMonitor` built
    /// it, in which case the negotiated queue limit travels WITH the
    /// decision — "monitor-stamped" and "has a negotiated limit" are one
    /// state, so neither can be set without the other.
    stamp: OptionsStamp,
    /// The server-wide group drain this channel's monitors register with —
    /// pvxs's one `qsrvGroup` pump per `GroupSource`
    /// (`ioc/groupsource.cpp:96`). `BridgeProvider` injects its shared
    /// pump via [`Self::with_pump`] so every group subscription on the
    /// server drains through ONE task; a directly-constructed channel
    /// (tests) gets a private pump, which behaves identically with one
    /// group on it.
    pump: Arc<super::group_pump::GroupPump>,
}

impl GroupChannel {
    pub fn new(db: Arc<PvDatabase>, def: GroupPvDef) -> Self {
        Self {
            db,
            def,
            access: super::provider::AccessContext::allow_all(),
            stamp: OptionsStamp::Get,
            pump: super::group_pump::GroupPump::new(),
        }
    }

    /// Inject an access control context (for [`super::provider::BridgeProvider`]).
    pub fn with_access(mut self, access: super::provider::AccessContext) -> Self {
        self.access = access;
        self
    }

    /// Share the server-wide group drain (see the `pump` field docs).
    pub(crate) fn with_pump(mut self, pump: Arc<super::group_pump::GroupPump>) -> Self {
        self.pump = pump;
        self
    }

    /// Mark this channel as a MONITOR source so composed values use pvxs's
    /// monitor-path `record._options` stamping (`atomic = true`
    /// unconditionally per ioc/groupsource.cpp:401-405, and
    /// `queueSize = stats.limitQueue` per groupsource.cpp:404). `limit` is
    /// that negotiated depth — the server's `MonitorOp::limit`, delivered
    /// as `MonitorOptions::queue_size`. The GET path never calls this, so
    /// GET stamps the request/default atomicity and the `queueSize=0`
    /// value-template default (groupsource.cpp:480-485).
    pub(crate) fn with_monitor_stamp(mut self, limit: u32) -> Self {
        self.stamp = OptionsStamp::Monitor { limit };
        self
    }

    /// True when this channel composes MONITOR-stamped values.
    fn is_monitor(&self) -> bool {
        matches!(self.stamp, OptionsStamp::Monitor { .. })
    }

    /// Read all member values and compose into a single PvStructure.
    ///
    /// The MONITOR snapshot entry point. `GroupMonitor::seed()` (the INIT
    /// frame) and `GroupMonitor::poll()` (each value event) call this via the
    /// cached monitor `group_channel`; `Channel::get()` bypasses it and calls
    /// `read_group_atomic` directly with the operation atomicity. The access
    /// read check is performed by `read_group_atomic` on entry.
    ///
    /// A monitor ALWAYS composes its snapshot atomically, independent of the
    /// group's `+atomic` setting. pvxs locks the fired field's entire
    /// trigger-target record set (`DBManyLocker G(field.lock)`,
    /// `groupsource.cpp:326`) for every value callback and stamps the value
    /// `atomic=true` unconditionally (`:401-405`). So a monitor read forces the
    /// atomic path even for a `+atomic:false` group: otherwise a multi-target
    /// trigger (`*` or a named set) whose targets update concurrently would
    /// sample its marked leaves at different instants (the sequential
    /// per-member `read_group_atomic(false)` path) and ship a torn snapshot the
    /// wire still advertises as atomic (an `OptionsStamp::Monitor` channel
    /// stamps `atomic=true`). Forcing the atomic read here keeps that stamp
    /// truthful by construction. A `+atomic:false` group's non-atomic reads
    /// remain reachable only through GET's `read_group_atomic(false)`.
    pub(crate) async fn read_group(&self) -> BridgeResult<PvStructure> {
        self.read_group_atomic(self.is_monitor() || self.def.atomic)
            .await
    }

    /// Root structure ID advertised for this group's value and descriptor.
    ///
    /// pvxs leaves `GroupDefinition::structureId` an empty `std::string`
    /// unless a top-level `+id` is configured (groupdefinition.h:30-40,
    /// groupconfigprocessor.cpp:184-189) and builds the group type as
    /// `TypeDef(TypeCode::Struct, structureId, {})` — the empty string when
    /// no `+id` (groupconfigprocessor.cpp:518-523). A non-empty Rust-only
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
        if !self.access.can_read(&self.def.name).await {
            return Err(BridgeError::PutRejected(format!(
                "read denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.creds.user, self.access.creds.host
            )));
        }

        let struct_id = self.root_struct_id();
        let mut pv = PvStructure::new(struct_id);

        // For atomic groups, hold all record locks simultaneously to
        // prevent intermediate states from being observed (pvxs `onGet`'s
        // `DBManyLocker G(group.value.lock)`, groupsource.cpp:490-503).
        //
        // CRITICAL: an atomic group MUST NOT re-lock a member record
        // inside `read_member` — `lock_group_records_read` (this file,
        // `:637-660`) only resolves each member to a bare
        // `Arc<parking_lot::RwLock<RecordInstance>>`; no guard is held
        // yet at that point. The real `parking_lot::RwLockReadGuard`s
        // are taken synchronously, all at once, in the loop below (this
        // file, `:946-956`) into `guard_map`, with the advisory
        // `_many_guard` gate already held to keep writers out for that
        // window.
        // parking_lot's `RwLock` is task-fair: a reader blocks if a
        // writer is already waiting, even though the lock is logically
        // free for reads, so recursively acquiring a read lock on a
        // record already in `guard_map` can deadlock — a writer
        // queued after that guard was taken would make a second
        // `.read()` on the same record block behind the writer, which
        // itself blocks behind the still-held first guard. So the
        // atomic path resolves every member through
        // `read_member_locked` against `guard_map` and never calls
        // `.read()`/`.write()` on a member record a second time.
        if atomic {
            // Resolve every member's backing record handle BEFORE the gate is
            // taken. `lock_group_records_read` takes no per-record lock and
            // reads nothing the gate protects: it looks the names up in the
            // database's own `records`/`aliases` maps and clones the `Arc`s
            // (`database/mod.rs:1801-1807`), which are mutated only under
            // `registration_mutex` (`mod.rs:1411`, `:1547`), never under the
            // per-record advisory gate. Hoisting it above the gate therefore
            // leaves the gate-held region below with zero `.await`s, which is
            // what lets that gate become a blocking priority-inheritance lock.
            let member_guards = lock_group_records_read(&self.db, &self.def.channels).await;

            // C-parity: pvxs's `onGet` takes `DBManyLocker G(group.value.lock)`
            // — the SAME `DBManyLock` the atomic PUT holds
            // (`groupsource.cpp:492` onGet vs `:621` onPutGroup). Take the
            // `lock_records` advisory gate over every member record BEFORE the
            // per-record read guards below, so this atomic GET is mutually
            // exclusive with a concurrent atomic group PUT (which also takes
            // `lock_records`, `group.rs:1460`) and with a plain single-record
            // write (which takes the same per-record gate via `lock_record`,
            // `field_io.rs:630`). Every writer takes the advisory gate before
            // its `RwLock` write guard, so while this GET owns the gate set no
            // writer can hold any member's write guard — the incremental
            // read-guard acquisition below becomes uncontended and consistent.
            // Without this gate that incremental acquisition left a window: a
            // write-preferring writer could update
            // a later-sorted member between this GET's read of an earlier one,
            // yielding a torn snapshot (B updated, A stale) that defeats the
            // `atomic` flag — the GET-side twin of the PUT-side BR-R15 gap.
            let member_records = group_member_record_names(&self.db, &self.def.channels);
            let _many_guard = self.db.lock_records(&member_records);

            // Every member's link-backed metadata, resolved while the gate is
            // held (so it is as consistent with the values as the atomic read
            // is) but before any member's read guard is taken (so a resolve
            // can still take its link target's lock). Once the guards below
            // exist, no member may reach for a second record's lock.
            let member_backings: HashMap<
                &str,
                std::collections::HashMap<String, epics_base_rs::server::database::LinkMetadata>,
            > = member_guards
                .iter()
                .map(|(name, rec)| (name.as_str(), self.db.resolve_link_backed_metadata(rec)))
                .collect();

            // Acquire every backing record's read guard synchronously, in the
            // sorted order `member_guards` already carries. The advisory
            // `_many_guard` gate (held above over the same set) keeps every writer
            // out of its write guard for this window, so this acquisition is
            // uncontended and deadlock-free; the guards are consumed synchronously
            // by `read_member_locked` below and never held across an await.
            let guards: Vec<(
                &str,
                parking_lot::RwLockReadGuard<'_, epics_base_rs::server::record::RecordInstance>,
            )> = member_guards
                .iter()
                .map(|(name, rec)| (name.as_str(), rec.read()))
                .collect();
            // Build a name→guard lookup so each member resolves
            // against the already-held guard for its backing record.
            let guard_map: HashMap<&str, &epics_base_rs::server::record::RecordInstance> =
                guards.iter().map(|(name, g)| (*name, &**g)).collect();
            for member in self.def.channels.iter() {
                // Only `proc` places no value field. A `+type:"structure"`
                // member emits an empty struct branch (resolved by
                // read_member -> read_member_channelless), matching the
                // advertised descriptor. pvxs adds the empty Struct to the
                // value template (groupconfigprocessor.cpp:922-931) and
                // clones it into every GET/MONITOR snapshot
                // (groupsource.cpp:484, :398-399).
                if member.def.mapping == FieldMapping::Proc {
                    continue;
                }
                let field = self.read_member_locked(member, &guard_map, &member_backings)?;
                set_member_field(&mut pv, &member.def, field);
            }
        } else {
            for member in self.def.channels.iter() {
                // Only `proc` places no value field. A `+type:"structure"`
                // member emits an empty struct branch (resolved by
                // read_member -> read_member_channelless), matching the
                // advertised descriptor. pvxs adds the empty Struct to the
                // value template (groupconfigprocessor.cpp:922-931) and
                // clones it into every GET/MONITOR snapshot
                // (groupsource.cpp:484, :398-399).
                if member.def.mapping == FieldMapping::Proc {
                    continue;
                }
                let field = self.read_member(member).await?;
                set_member_field(&mut pv, &member.def, field);
            }
        }

        // `record._options` stamping differs between the MONITOR and GET
        // paths in pvxs; `self.stamp` — fixed when the channel was built —
        // carries BOTH the choice and, on the monitor path, the negotiated
        // queue limit, so the composer has no fallback of its own to get
        // wrong. See [`OptionsStamp`] and [`push_record_options`]. Locking
        // still uses the real `atomic` mode resolved above; only the
        // *stamped* atomicity is forced on the monitor path.
        push_record_options(&mut pv, atomic, self.stamp);

        Ok(pv)
    }

    /// Resolve the channel-less mappings (Const / Structure / Proc)
    /// that need no record lock. Returns `Some(field)` for those
    /// mappings, `None` for a mapping that requires a backing record.
    fn read_member_channelless(member: &MemberChannel) -> Option<PvField> {
        match member.def.mapping {
            FieldMapping::Const => Some(
                member
                    .def
                    .const_value
                    .clone()
                    .unwrap_or(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(0))),
            ),
            // Empty struct branch carrying the member `+id` so the value
            // matches the descriptor built in `get_field`
            // (pvxs adds `Struct(id)` to the value template,
            // groupconfigprocessor.cpp:922-931).
            FieldMapping::Structure => Some(PvField::Structure(PvStructure::new(
                member.def.struct_id.as_deref().unwrap_or(""),
            ))),
            FieldMapping::Proc => Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Int(0))),
            _ => None,
        }
    }

    /// Read a single member's value from the database. Used by the
    /// non-atomic `read_group` path: it locks the backing record
    /// itself (no pre-held guard exists). The atomic path MUST use
    /// [`Self::read_member_locked`] instead — see the deadlock note
    /// in `read_group`.
    async fn read_member(&self, member: &MemberChannel) -> BridgeResult<PvField> {
        if let Some(field) = Self::read_member_channelless(member) {
            return Ok(field);
        }

        let (record_name, field_name) = member.names();

        let rec = self
            .db
            .get_record(record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        // Resolved before the record's own guard: a link-backed member
        // (`CALC.A`) answers its units/precision from the LINK TARGET's
        // record, and that second lock cannot be taken from under this one.
        let backing = self.db.resolve_link_backed_metadata(&rec);
        let backing = epics_base_rs::server::database::LinkBacking::resolved(&backing);

        let instance = rec.read();
        Self::decode_member(member, record_name, field_name, &instance, backing)
    }

    /// Read a single member's value against a record instance that the
    /// caller already holds a read guard on. The atomic `read_group`
    /// path uses this so it never re-locks a record whose guard is
    /// held by `lock_group_records_read` (recursive-read deadlock).
    fn read_member_locked(
        &self,
        member: &MemberChannel,
        guard_map: &HashMap<&str, &epics_base_rs::server::record::RecordInstance>,
        backings: &HashMap<
            &str,
            std::collections::HashMap<String, epics_base_rs::server::database::LinkMetadata>,
        >,
    ) -> BridgeResult<PvField> {
        if let Some(field) = Self::read_member_channelless(member) {
            return Ok(field);
        }

        let (record_name, field_name) = member.names();

        let instance = *guard_map
            .get(record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;
        // Resolved by the caller before it took any member guard — the atomic
        // path holds every member's read guard at once, so nothing here may
        // reach for a link target's lock.
        let backing = backings
            .get(record_name)
            .map(epics_base_rs::server::database::LinkBacking::resolved)
            .unwrap_or_else(epics_base_rs::server::database::LinkBacking::none);
        Self::decode_member(member, record_name, field_name, instance, backing)
    }

    /// Decode one member's value from an already-borrowed record
    /// instance. Shared by the locked (atomic) and self-locking
    /// (non-atomic) read paths so both produce identical output.
    /// Run the member's VALUE-channel filter chain in READ context.
    ///
    /// pvxs reads a group member through `dbChannelGet` on that member's own
    /// `dbChannel` (`iocsource.cpp:79,127,175,268` under `IOCSource::get`),
    /// so `arr` slicing and `ts` tagging reach a group GET exactly as they
    /// reach a single-record one.
    ///
    /// A chain that DROPS the read yields the unfiltered value, not an
    /// error. C builds a read log with a zero `mask`
    /// (`db_create_read_log` → `db_create_field_log`'s `freeListCalloc`,
    /// `dbEvent.c:702,760-770`), so a value-gating filter's
    /// `send = pfl->mask & ~(DBE_VALUE|DBE_LOG)` starts at 0 and
    /// `recGblCheckDeadband`'s zero `add_mask` can never raise it
    /// (`filters/dbnd.c:83-88`) — the log is deleted and `NULL` reaches
    /// `IOCSource::get`. pvxs passes that `NULL` straight into
    /// `dbChannelGet` (`iocsource.cpp:79`, `localfieldlog.cpp:15-27`),
    /// which then reads the live record (`dbAccess.c:924-930`). So a
    /// `{"dbnd":…}` member serves every GET; only the event stream is
    /// gated.
    fn filter_read_value(
        member: &MemberChannel,
        value: epics_base_rs::types::EpicsValue,
    ) -> epics_base_rs::types::EpicsValue {
        if member.value_filters.is_empty() {
            return value;
        }
        member
            .value_filters
            .apply_to_read_value(value.clone())
            .unwrap_or(value)
    }

    fn decode_member(
        member: &MemberChannel,
        record_name: &str,
        field_name: &str,
        instance: &epics_base_rs::server::record::RecordInstance,
        backing: epics_base_rs::server::database::LinkBacking<'_>,
    ) -> BridgeResult<PvField> {
        match member.def.mapping {
            FieldMapping::Scalar => {
                let mut snapshot = member.snapshot_in(instance, backing).ok_or_else(|| {
                    BridgeError::FieldNotFound {
                        record: record_name.to_string(),
                        field: field_name.to_string(),
                    }
                })?;
                snapshot.value = Self::filter_read_value(member, snapshot.value);
                // Derive the NT shape from the configured field's resolved
                // value (record → common → virtual), not from the owning
                // record type: a `REC.SCAN` member is NTEnum and a
                // `BI.DESC` member is NTScalar string regardless of the
                // record's type. `snapshot.value` IS the resolved field
                // value and `snapshot_for_field` already populated common
                // enum choices (e.g. `.SCAN`). Matches the single-record
                // path and pvxs's per-channel `getChannelValueType`
                // (groupconfigprocessor.cpp:960-974).
                let nt_type = member.nt_type_in(instance, Some(&snapshot.value));
                Ok(PvField::Structure(pvif::snapshot_to_pv_structure(
                    &snapshot, nt_type,
                )))
            }
            FieldMapping::Plain => {
                let value =
                    member
                        .value_in(instance)
                        .ok_or_else(|| BridgeError::FieldNotFound {
                            record: record_name.to_string(),
                            field: field_name.to_string(),
                        })?;
                let value = Self::filter_read_value(member, value);
                // The bare leaf renders through the same classifier the
                // introspection uses, so the value can never be a shape the
                // advertised descriptor does not describe (R18-26: a
                // long-string member shipped `ScalarArray(bytes)` under a
                // `Scalar(Byte)` descriptor).
                Ok(pvif::BareLeaf::of_channel(
                    instance,
                    &member.field,
                    Some(&value),
                    value.db_field_type(),
                    member.string_view,
                )
                .value(&value))
            }
            FieldMapping::Meta => {
                let snapshot = member.snapshot_in(instance, backing).ok_or_else(|| {
                    BridgeError::FieldNotFound {
                        record: record_name.to_string(),
                        field: field_name.to_string(),
                    }
                })?;
                // Same unnamed enclosing structure the descriptor
                // advertises (see `meta_desc`), so the two cannot drift.
                let mut meta = PvStructure::new("");
                meta.fields.push((
                    "alarm".into(),
                    PvField::Structure(build_alarm_from_snapshot(&snapshot)),
                ));
                meta.fields.push((
                    "timeStamp".into(),
                    PvField::Structure(build_timestamp_from_snapshot(&snapshot)),
                ));
                Ok(PvField::Structure(meta))
            }
            FieldMapping::Any => {
                let value =
                    member
                        .value_in(instance)
                        .ok_or_else(|| BridgeError::FieldNotFound {
                            record: record_name.to_string(),
                            field: field_name.to_string(),
                        })?;
                let value = Self::filter_read_value(member, value);
                // pvxs serves `+type:"any"` as a PVA `any` slot whose
                // payload carries the concrete DB field type: `IOCSource::
                // get` allocates `anyType.cloneEmpty()` and writes the
                // scalar/array value into it (iocsource.cpp:335-349). Wrap
                // the converted value in a Variant tagged with its own
                // wire-faithful descriptor so the slot decodes as `any`,
                // not a fixed scalar.
                //
                // `anyType` is settled by the SAME element-count predicate
                // every other leaf takes (`ioc/field.cpp:38-45`), so it is
                // asked of the classifier rather than read off the stored
                // variant — a one-element array field's `any` payload is a
                // scalar, exactly as its plain and scalar members are.
                let pv = pvif::BareLeaf::any_payload_of_channel(instance, &member.field, &value)
                    .value(&value);
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
    /// not the owning record type. The value is taken through
    /// `client_field_value` — the stored value projected onto the field's
    /// DECLARED type — which is the same call the GET path above serializes,
    /// so the descriptor cannot drift from the bytes that follow it:
    /// `REC.SCAN` advertises NTEnum, `REC.DESC` advertises NTScalar
    /// string, and `BI.DESC` stays a string member on an enum record.
    /// pvxs builds scalar group member descriptors from
    /// `getTypeDefForChannel`/`getChannelValueType` on the field-specific
    /// dbChannel (groupconfigprocessor.cpp:867-974), not the record type.
    async fn introspect_member(
        &self,
        member: &MemberChannel,
    ) -> BridgeResult<(NtType, ScalarType)> {
        let record_name = member.record.as_str();

        let rec = self
            .db
            .get_record(record_name)
            .ok_or_else(|| BridgeError::RecordNotFound(record_name.to_string()))?;

        let instance = rec.read();
        let resolved = member.value_in(&instance);
        let nt_type = member.nt_type_in(&instance, resolved.as_ref());
        let value_dbf = resolved
            .as_ref()
            .map(|v| v.db_field_type())
            .or_else(|| instance.declared_field_type(&member.field))
            .unwrap_or(DbFieldType::Double);

        Ok((nt_type, dbf_to_scalar_type(value_dbf)))
    }

    /// Look up a member's DBF field type for the PUT conversion target —
    /// pvxs converts the incoming leaf to `dbChannelFinalFieldType`, the
    /// field's DECLARED type, so this must be the type the descriptor
    /// advertised, not the variant the record happens to store. Taken
    /// through the same `client_field_value` projection as the descriptor
    /// path above; `declared_field_type` covers the fields that carry no
    /// value at all, and `Double` only when the record/field is unknown.
    fn member_dbf_type(&self, member: &MemberChannel) -> DbFieldType {
        let rec = match self.db.get_record(&member.record) {
            Some(r) => r,
            None => return DbFieldType::Double,
        };
        let instance = rec.read();
        member
            .value_in(&instance)
            .map(|v| v.db_field_type())
            .or_else(|| instance.declared_field_type(&member.field))
            .unwrap_or(DbFieldType::Double)
    }

    /// True iff this member's backing field is a dbStatic link class
    /// (`DBF_INLINK..=DBF_FWDLINK`).
    ///
    /// pvxs rejects a group PUT that binds any link-class field during the
    /// all-field preparation pass (`ioc/groupsource.cpp:603-606`,
    /// `dbChannelFinalFieldType` in `DBF_INLINK..=DBF_FWDLINK`), before any
    /// marked/putable filtering. The Rust port has no dbStatic field table
    /// and link fields surface as `DbFieldType::String`, so classification
    /// routes through the record's `recordType` plus the field name into the
    /// canonical `dbf_link_class` classifier (`epics-base-rs`
    /// `types/dbr.rs`) — the single owner of the "is this field a link"
    /// rule — instead of a member-name heuristic that re-opens the bypass
    /// for any record spelling a link field outside the list.
    ///
    /// A member with no backing channel (`+type:structure` / `+const`) has
    /// no dbChannel to classify, matching pvxs skipping fields whose
    /// `field.value` is null.
    async fn member_targets_link_field(&self, member: &MemberChannel) -> bool {
        if !member.has_channel() {
            return false;
        }
        // Through the shared classifier so the put gate and the group
        // CREATION gate (`BridgeProvider::group_creation_error`) answer
        // "is this a link field" from one table.
        super::channel::channel_link_class(&self.db, member).is_some()
    }

    /// The node inside the member's incoming value that actually carries
    /// the data to write — the port of `IOCSource::put`'s
    /// `switch (info.type)` (pvxs `iocsource.cpp:578-597`), and the single
    /// owner of "which leaf does a member PUT write".
    ///
    /// The mapping decides the shape, so the shape must be selected FROM
    /// the mapping, never guessed from the incoming field:
    ///
    /// - `Scalar` — the member is advertised as an NTScalar/NTEnum
    ///   structure, so the client PUTs that wrapper: write `node["value"]`,
    ///   and for NTEnum (a `value` that is itself a structure)
    ///   `node["value"]["index"]`. Without this de-reference the wrapper
    ///   itself reached the converter and every `+type:"scalar"` member PUT
    ///   was rejected as unconvertible (R17-35).
    /// - `Plain` — the member is the bare leaf; write `node` (pvxs
    ///   `value = node`). Do NOT unwrap: a plain member's node has no
    ///   `value` child.
    /// - `Any` — a PVA `any` slot; de-reference the Variant (`node["->"]`).
    /// - `Meta` / `Proc` / `Structure` / `Const` — pvxs `IOCSource::put`
    ///   returns without writing ("can't write"): a `Const` member's value
    ///   comes from the config, and meta/proc/structure members have no
    ///   client-writable leaf.
    fn put_leaf(
        mapping: FieldMapping,
        node: &epics_pva_rs::pvdata::PvField,
    ) -> Option<&epics_pva_rs::pvdata::PvField> {
        use epics_pva_rs::pvdata::PvField;
        match mapping {
            FieldMapping::Plain => Some(node),
            FieldMapping::Any => match node {
                PvField::Variant(v) => Some(&v.value),
                other => Some(other),
            },
            FieldMapping::Scalar => {
                let PvField::Structure(st) = node else {
                    // pvxs `node["value"]` on a non-structure yields an
                    // empty Value and the put throws — the group type
                    // always advertises the wrapper, so a bare leaf here is
                    // a malformed PUT.
                    return None;
                };
                match st.get_field("value")? {
                    // NTEnum: the writable leaf is the index.
                    PvField::Structure(inner) => inner.get_field("index"),
                    leaf => Some(leaf),
                }
            }
            FieldMapping::Meta
            | FieldMapping::Proc
            | FieldMapping::Structure
            | FieldMapping::Const => None,
        }
    }

    /// True iff the member's backing field stores a `DBF_CHAR` array — the
    /// storage pvxs writes with `putLongString` when the incoming leaf is a
    /// string (`dbChannelFinalFieldType == DBR_CHAR && value is String`,
    /// iocsource.cpp:603-604).
    fn member_is_char_array(&self, member: &MemberChannel) -> bool {
        let Some(rec) = self.db.get_record(&member.record) else {
            return false;
        };
        let instance = rec.read();
        matches!(
            member.value_in(&instance),
            Some(epics_base_rs::types::EpicsValue::CharArray(_))
        )
    }

    /// Convert an incoming PvField to an EpicsValue typed against the
    /// member's actual DBF field. This avoids context-free fallback
    /// conversions (e.g. ScalarValue::Long → EpicsValue::Double).
    ///
    /// For arrays and structures, falls back to `pv_field_to_epics`.
    fn convert_member_value(
        &self,
        member: &MemberChannel,
        pv_field: &epics_pva_rs::pvdata::PvField,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        use epics_pva_rs::pvdata::PvField;
        // Select the writable leaf from the MEMBER'S MAPPING, exactly as
        // `IOCSource::put` does; the incoming node's own shape never
        // decides this.
        let pv_field = Self::put_leaf(member.def.mapping, pv_field)?;
        match pv_field {
            // A string into a `DBF_CHAR` array member is pvxs's
            // `putLongString`: `dbPut(DBR_CHAR, str, strlen+1)`, the same
            // NUL-terminated char image the single-record path writes. The
            // typed scalar conversion below would instead try to parse the
            // whole string as one integer and reject the PUT.
            PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::String(s))
                if self.member_is_char_array(member) =>
            {
                Some(pvif::long_string_put_image(s))
            }
            PvField::Scalar(sv) => {
                let target = self.member_dbf_type(member);
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

    /// `IOCSource::doPostProcessing` (`iocsource.cpp:397-403`) — the single
    /// owner of "does this group member's PUT process its backing record?".
    /// Shared by [`Self::post_process_member_already_locked`] (atomic PUT)
    /// and [`Self::post_process_member`] (non-atomic PUT): same decision,
    /// different lock discipline below it.
    ///
    /// C asks three questions, in order: is the bound field the record's
    /// `PROC`; did the client force processing (`record._options.process=true`);
    /// otherwise, is the field `pp(TRUE)` on a `SCAN=Passive` record (and the
    /// client neither forced nor inhibited). Only then `dbProcess`.
    ///
    /// The port used to process a `+type:"proc"` member's record
    /// UNCONDITIONALLY — every group PUT, whatever the member's field, whatever
    /// the record's SCAN, even under `process=false` (R18-30). The gate is not
    /// specific to `proc` members: it is the gate for every member whose write
    /// did not go through `dbPutField` — which is `proc` (no value to write) and
    /// the `changing`-but-unwritable members (Meta, R17-37). One owner, so a
    /// second such member class cannot re-open it.
    fn member_process_it(
        &self,
        record_name: &str,
        field_name: &str,
        process: super::channel::ProcessMode,
    ) -> bool {
        use super::channel::ProcessMode;
        match process {
            // `forceProcessing == True` — process regardless of field and SCAN.
            ProcessMode::Force => true,
            // `forceProcessing == False` — C's `doPostProcessing` still honors
            // `pfield == &precord->proc`: a PROC-bound member processes even
            // under process=false, because the disjunction's first term does
            // not consult `forceProcessing`.
            ProcessMode::Inhibit => field_name.eq_ignore_ascii_case("PROC"),
            // `forceProcessing == Unset` — the record's own rule, asked of the
            // database (`PROC`, or `pp(TRUE)` on a Passive record).
            ProcessMode::Passive => self.db.put_drives_processing(record_name, field_name),
        }
    }

    /// [`Self::member_process_it`], applied — the atomic-PUT entry. This
    /// transaction already owns every member-record gate via `lock_records`
    /// (the gate is not reentrant), so the transition below MUST use
    /// the `_already_locked` entry. Synchronous: after H6,
    /// `put_driven_process_already_locked` is a plain `fn`, so this reaches
    /// the end of the atomic PUT's
    /// `lock_records` window with zero `.await`s (§1.1 H9, §5 step 6).
    fn post_process_member_already_locked(
        &self,
        member: &MemberChannel,
        process: super::channel::ProcessMode,
    ) -> BridgeResult<()> {
        let (record_name, field_name) = member.names();
        if !self.member_process_it(record_name, field_name, process) {
            return Ok(());
        }
        // The DECISION is the group's (pvxs asks it in `doPostProcessing`); the
        // TRANSITION is the database's. `put_driven_process` is its declared
        // single owner — C `dbPutField:1264-1277` and pvxs
        // `iocsource.cpp:404-419` split identically on PACT: an async-active
        // record takes `rpro = TRUE` and is NOT processed (`recGblFwdLink`
        // re-queues it when the device round trip lands), an idle one takes
        // `putf = TRUE` and processes. Reaching for `process_record_with_links`
        // here instead set neither flag and dropped a group PUT into
        // `dbProcess`'s own PACT guard — the LCNT bump and the SCAN_ALARM /
        // INVALID after MAX_LOCK that C's `doPostProcessing` exists to avoid,
        // while losing the deferred reprocess: two rapid group PUTs to a Passive
        // async output wrote one value to the device where C writes both.
        self.db
            .put_driven_process_already_locked(record_name)
            .map_err(|e| BridgeError::PutRejected(e.to_string()))
    }

    /// [`Self::member_process_it`], applied — the non-atomic-PUT entry. No
    /// member gate is held here, so the transition takes the
    /// gate-acquiring `put_driven_process`.
    async fn post_process_member(
        &self,
        member: &MemberChannel,
        process: super::channel::ProcessMode,
    ) -> BridgeResult<()> {
        let (record_name, field_name) = member.names();
        if !self.member_process_it(record_name, field_name, process) {
            return Ok(());
        }
        self.db
            .put_driven_process(record_name)
            .await
            .map_err(|e| BridgeError::PutRejected(e.to_string()))
    }

    /// Apply one ordinary (value) group-member write under the
    /// requested [`super::channel::ProcessMode`], the single owner of the
    /// tri-state → write mapping for group member application. Mirrors pvxs
    /// `putGroupField` → `IOCSource::put` + `doPostProcessing(
    /// forceProcessing)` (groupsource.cpp:564-571, iocsource.cpp:
    /// 397-421), which preserves the full `TriState forceProcessing`
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
    /// Split into an already-locked entry (atomic PUT, this transaction owns
    /// every member gate via `lock_records`; the gate is not
    /// reentrant) and a gate-acquiring entry (non-atomic per-member path) —
    /// same tri-state mapping, different lock discipline.
    ///
    /// Bracket this member's backing write with the EPICS `asTrapWrite`
    /// put-logging hook. pvxs builds one `SecurityLogger` per group field
    /// (groupsource.cpp:594-602), so each member value write emits its own
    /// Before/After pair (a `proc` hook writes no value and is not routed
    /// here). The member `grant` gates emission; `dbr_type` is the value's
    /// final field type — `convert_member_value` already typed it to the
    /// member DBF.
    fn apply_member_value_already_locked(
        &self,
        record_name: &str,
        field_name: &str,
        value: epics_base_rs::types::EpicsValue,
        process: super::channel::ProcessMode,
        grant: super::provider::WriteGrant,
    ) -> BridgeResult<()> {
        use super::channel::ProcessMode;
        let pv_name = format!("{record_name}.{field_name}");
        let meta = super::trap_write::TrapWriteMeta {
            pv_name: &pv_name,
            user: &self.access.creds.user,
            host: &self.access.creds.host,
            peer: &self.access.creds.host,
            dbr_type: value.dbr_type() as u16,
        };
        // Synchronous bracket: after H6 every `_already_locked` callee below
        // is a plain `fn`, so this reaches the end of the atomic PUT's
        // `lock_records` window with zero `.await`s.
        super::trap_write::put_with_trap_already_locked(grant, meta, value, |value| {
            let to_err = |e: epics_base_rs::error::CaError| BridgeError::PutRejected(e.to_string());
            match process {
                ProcessMode::Inhibit => {
                    let pv = format!("{record_name}.{field_name}");
                    self.db.put_pv_already_locked(&pv, value).map_err(to_err)?;
                }
                ProcessMode::Passive => {
                    // Group member puts never await completion — use the
                    // fire-and-forget entry so no put-notify wait-set is
                    // parked on the member record (a dropped receiver
                    // would occupy its notify slot until async
                    // processing settles).
                    self.db
                        .put_record_field_from_ca_no_notify_already_locked(
                            record_name,
                            field_name,
                            value,
                        )
                        .map_err(to_err)?;
                }
                ProcessMode::Force => {
                    let pv = format!("{record_name}.{field_name}");
                    // Any PACT park this put releases replays on the cycle two
                    // lines down, from its tail — C's only restart owner.
                    self.db.put_pv_already_locked(&pv, value).map_err(to_err)?;
                    let mut visited = std::collections::HashSet::new();
                    self.db
                        .process_record_with_links_already_locked(record_name, &mut visited, 0)
                        .map_err(to_err)?;
                }
            }
            Ok(())
        })
    }

    /// [`Self::apply_member_value_already_locked`]'s gate-acquiring twin —
    /// the non-atomic per-member path. No member gate is held here, so every
    /// write below takes the gate-acquiring entry and the trap bracket
    /// awaits it.
    async fn apply_member_value(
        &self,
        record_name: &str,
        field_name: &str,
        value: epics_base_rs::types::EpicsValue,
        process: super::channel::ProcessMode,
        grant: super::provider::WriteGrant,
    ) -> BridgeResult<()> {
        use super::channel::ProcessMode;
        let pv_name = format!("{record_name}.{field_name}");
        let meta = super::trap_write::TrapWriteMeta {
            pv_name: &pv_name,
            user: &self.access.creds.user,
            host: &self.access.creds.host,
            peer: &self.access.creds.host,
            dbr_type: value.dbr_type() as u16,
        };
        super::trap_write::put_with_trap(grant, meta, value, |value| async move {
            let to_err = |e: epics_base_rs::error::CaError| BridgeError::PutRejected(e.to_string());
            match process {
                ProcessMode::Inhibit => {
                    let pv = format!("{record_name}.{field_name}");
                    self.db.put_pv(&pv, value).await.map_err(to_err)?;
                }
                ProcessMode::Passive => {
                    self.db
                        .put_record_field_from_ca_no_notify(record_name, field_name, value)
                        .await
                        .map_err(to_err)?;
                }
                ProcessMode::Force => {
                    let pv = format!("{record_name}.{field_name}");
                    self.db.put_pv(&pv, value).await.map_err(to_err)?;
                    let mut visited = std::collections::HashSet::new();
                    self.db
                        .process_record_with_links(record_name, &mut visited, 0)
                        .await
                        .map_err(to_err)?;
                }
            }
            Ok(())
        })
        .await
    }

    /// group PUT with explicit per-operation options.
    ///
    /// pvAccess delivers PUT options (`record._options.process`,
    /// `record._options.atomic`, `record._options.block`) in the INIT
    /// pvRequest, not in the data-phase value (pvxs
    /// `groupsource.cpp:202-204` reads
    /// `channelConnectOperation->pvRequest()["record._options.atomic"]`
    /// at INIT, and `:207` runs `setForceProcessingFlag` against that
    /// same `pvRequest()`). The native
    /// wire path captures the INIT pvRequest on `ChannelContext` and
    /// passes the parsed [`super::channel::PutOptions`] plus the explicit atomic
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
        log: &RemoteLog,
    ) -> BridgeResult<()> {
        if !self.access.can_write(&self.def.name).await {
            // Group-level ACF gate (the per-member gate below mirrors pvxs's
            // per-field SecurityClient). Both are write-ACF denials, so both
            // carry pvxs's `doFieldPreProcessing` text (iocsource.cpp:385).
            return Err(super::put_status::put_not_permitted(&format!(
                "write denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.creds.user, self.access.creds.host
            )));
        }

        // pvxs `putGroupField` (groupsource.cpp:554-561) computes
        // `marked && !putable` per group field and, when the client marked a
        // field the group cannot write (`+putorder` absent ⇒ `putOrder ==
        // int64_t::min()`, the not-putable sentinel), tells the client so with
        // a Warn `logRemote` naming the field before dropping the write. The
        // apply below drops those members silently (they never enter
        // `ordered`), so the diagnostic must be raised here, from the same
        // marked-set test the apply uses.
        //
        // `putable` is the ONLY thing tested — pvxs warns for a marked Proc
        // member without `+putorder` too (it still post-processes it). What it
        // cannot warn for is a member with no `dbChannel` (`field.value` null ⇒
        // `marked` false): Structure and Const members, whose `channel` is
        // empty here.
        for m in &self.def.members {
            if m.put_order.is_none()
                && !m.channel.is_empty()
                && get_nested_field(value, &m.field_name).is_some()
            {
                log.warn(format!("{}: no putorder, ignore write", m.field_name));
            }
        }

        // pvRequest can override the group default atomicity
        // (`record._options.atomic = true|false`). Falls back to the
        // group default when the option is absent.
        let atomic = atomic_override.unwrap_or(self.def.atomic);

        // Build the PUT apply order. A *value* member is putable only
        // with an explicit `+putorder`: pvxs's sentinel `i64::MIN`
        // (fieldconfig.h:37) means "not putable" (groupsource.cpp:555),
        // so a no-`+putorder` value member is ignored, never written
        // under an implicit `0`.
        //
        // A `proc` member is the exception: pvxs's `doPostProcessing`
        // returns true for `MappingInfo::Proc` independent of putable
        // (groupsource.cpp:547-574), so a proc hook runs on every group
        // PUT even without `+putorder`. Keep proc members in the apply
        // list regardless; a no-`+putorder` proc sorts at the sentinel
        // position (first), matching the absent-putOrder ordering. Before
        // this fix the `filter_map` dropped them, so a proc-only save/apply
        // hook without `+putorder` silently never ran.
        let mut ordered: Vec<(&MemberChannel, i64)> = self
            .def
            .channels
            .iter()
            .filter_map(|m| match m.def.put_order {
                Some(po) => Some((m, po)),
                None if m.def.mapping == FieldMapping::Proc => Some((m, i64::MIN)),
                None => None,
            })
            .collect();
        ordered.sort_by_key(|(_, po)| *po);
        let ordered: Vec<&MemberChannel> = ordered.into_iter().map(|(m, _)| m).collect();

        // What this PUT does with each member — one classifier, both loops.
        //
        // On the native PVA path the value is pruned to the client's marked
        // members, so presence == marked; a whole-value in-process caller
        // supplies every member and marks them all. An absent (unmarked) value
        // member must not be link-rejected, access-checked, or written — the
        // up-front per-member pre-checks would otherwise let an unmarked,
        // unwritable or link-targeting member reject a partial PUT to an
        // unrelated marked member.
        let classify = |m: &MemberChannel| -> MemberPutAction { member_put_action(&m.def, value) };

        // pvxs's group PUT preparation pass (groupsource.cpp:597-609)
        // iterates EVERY backing field before any marked/putable filtering
        // and, on paper, throws "Links not supported for put" for a link
        // field. It never fires: the test at groupsource.cpp:603-604 reads
        // `dbChannelFinalFieldType`, and ioc/channel.cpp:69-74 has already
        // set `addr.dbr_field_type = DBR_CHAR` for every link field before
        // `dbChannelOpen`, which epics-base seeds into `final_type`
        // (dbChannel.c:579, :621). So the value is 1, never >= DBF_INLINK,
        // and pvxs reaches `doDbPut` and really does write link fields.
        //
        // Two rulings follow, and they point opposite ways. Refusing a
        // link member the client asked to WRITE is kept: pvxs writes it
        // only because it tested the wrong accessor, and reproducing
        // another server's wrong-accessor bug is not parity. But refusing
        // the whole operation because of a member nobody is writing is
        // dropped — that behaviour existed only to mirror the dead test's
        // position in the prep pass, and it failed `pvput GRP v=1` on any
        // group that merely binds an FLNK. The check now runs on the
        // members this PUT writes, classified by the actual DBF link class
        // through the canonical classifier rather than a member-name
        // heuristic (a name list re-opens the bypass for any record that
        // spells a link field outside it).
        for m in self.def.channels.iter() {
            // A Structure/Const member has no backing dbChannel — pvxs's
            // prep pass gates each check on `field.value` being non-null
            // (groupsource.cpp:599), so skip those members here.
            if !m.has_channel() {
                continue;
            }
            let (record_name, field_name) = m.names();
            // pvxs runs `IOCSource::doPreProcessing` (iocsource.cpp:365-369)
            // on every channeled member in this prep pass
            // (groupsource.cpp:599-602) — before any marked/putable
            // filtering and in every process mode — rejecting the whole
            // group PUT if a member's backing record is DISP-disabled or the
            // bound field is read-only. An UNMARKED DISP=1 member still fails
            // the operation, matching C. This runs before the link-class
            // check to mirror C's per-field ordering (doPreProcessing, then
            // the link throw). Same gate the single-record path enforces —
            // the `Force`/`Inhibit` group routes go through `put_pv`, which
            // does not itself gate DISP.
            super::put_status::check_preconditions(&self.db, record_name, field_name).await?;
            if matches!(classify(m), MemberPutAction::Write)
                && self.member_targets_link_field(m).await
            {
                return Err(super::put_status::links_not_supported(&format!(
                    "group {} PUT: member '{}' targets link field '{}'",
                    self.def.name, m.def.field_name, m.def.channel
                )));
            }
        }

        // pvxs builds a per-field SecurityClient at group PUT
        // (groupsource.cpp:213-226 + 626) so a group PV writable for the
        // caller doesn't tunnel writes into members the caller cannot
        // write directly. Re-check write access for each member's
        // backing dbChannel under the caller's identity (already
        // captured in `self.access`). A single denial fails the whole
        // PUT — matching pvxs's "any member denied → operation
        // rejected" remote-error behavior. Only members this PUT acts
        // on are checked: pvxs builds the SecurityClient over the
        // changed fields, so an unmarked, unwritable member must not
        // reject a partial PUT to an unrelated marked one.
        //
        // The grant per member also carries the matched rule's
        // TRAPWRITE flag (`WriteGrant::rule_was_trap`), the single
        // source the member write below uses to gate `asTrapWrite`
        // put-logging — pvxs builds one `SecurityLogger` per group
        // field (groupsource.cpp:594-602). Resolve it once here and key
        // it by the member's own field name — unique per member, unlike
        // the backing channel, which two members may share — so the write
        // phase emits without re-deriving the trap flag.
        //
        // Which members get the check is the classifier's call, not a second
        // hand-rolled predicate: C runs `doFieldPreProcessing` inside
        // `if (changing)` (groupsource.cpp:564), so every changing member is
        // checked — INCLUDING a Meta member that writes nothing — and a `proc`
        // trigger never is (it is not changing; `dbProcess` is not a
        // `dbPutField`, so gating it on write access is a category error). A
        // proc member's DISP/read-only prep gate (`doPreProcessing`) still ran
        // in the precondition pass above.
        let mut member_grants: HashMap<String, super::provider::WriteGrant> = HashMap::new();
        for m in &ordered {
            let acf_checked = match classify(m) {
                MemberPutAction::Write => true,
                MemberPutAction::ProcessOnly { acf_checked } => acf_checked,
                MemberPutAction::Skip => false,
            };
            if !acf_checked {
                continue;
            }
            // The PEELED name: ACF matches a record's ASG, and a
            // `REC.VAL{"dbnd":…}` string names no record.
            let grant = self.access.write_grant(&m.pv_name).await;
            if !grant.allowed {
                return Err(super::put_status::put_not_permitted(&format!(
                    "group {} PUT: member '{}' field '{}' write denied for \
                     user='{}' host='{}' (per-member ACF)",
                    self.def.name,
                    m.def.field_name,
                    m.def.channel,
                    self.access.creds.user,
                    self.access.creds.host
                )));
            }
            member_grants.insert(m.def.field_name.clone(), grant);
        }

        // track whether any member write/proc actually fired so a
        // marked PUT that writes nothing returns an error like pvxs
        // (groupsource.cpp:656-659) instead of silently succeeding.
        let mut did_something = false;

        if atomic {
            // atomic PUT — `DBManyLock`-equivalent exclusion.
            //
            // `atomic_write_lock` (L33) is acquired FIRST, above and
            // outside `lock_records` (L1) — see the acquisition-order note
            // in `record_lock.rs`'s module doc. It is retained as an
            // internal aid so two PUTs through the *same* group PV also
            // serialize even before either reaches `lock_records`,
            // including the up-front
            // value-conversion phase below: a conversion failure returns
            // before `lock_records` is ever requested, so nothing has
            // been locked (or applied) when the atomic PUT aborts. Held
            // for the whole atomic block (`bug4_atomic_put_serializes_on_group_lock`,
            // `bug4_concurrent_atomic_puts_do_not_interleave`).
            //
            // A blocking `PriorityInheritanceMutex` since the L1 flip. It was
            // a `tokio::sync::Mutex` only because its window contained
            // `lock_records(…).await`; that window is now the conversion
            // phase, a synchronous `lock_records` and a synchronous member
            // loop, with zero `.await`s, so the `!Send` guard the connection
            // task would have rejected is exactly what now proves the
            // property.
            let _atomic_guard = self.def.atomic_write_lock.lock();

            // Convert all values up-front (DBF-typed), then perform the
            // actual writes in order. A member that only post-processes
            // (`proc` trigger, or a changing member with no writable leaf)
            // carries no value. Synchronous: `convert_member_value` and its
            // callees (`member_is_char_array`, `member_dbf_type`) do not
            // await anything, so this whole phase runs, and can fail,
            // before any member-record gate is even requested.
            let mut writes: Vec<(
                &MemberChannel,
                MemberPutAction,
                Option<epics_base_rs::types::EpicsValue>,
            )> = Vec::new();

            for member in &ordered {
                let action = classify(member);
                let epics_val = match action {
                    MemberPutAction::Skip => continue,
                    MemberPutAction::ProcessOnly { .. } => None,
                    MemberPutAction::Write => {
                        // Use nested lookup so members with dotted field paths
                        // (e.g., "axis.position") resolve correctly. The read
                        // path uses set_nested_field — put must use the same
                        // path semantics. The classifier proved the field is
                        // present, so a missing one here cannot happen.
                        //
                        // A supplied-but-unconvertible value is a conversion
                        // error, not a no-op: pvxs's `IOCSource::put` throws on
                        // an unsupported conversion (iocsource.cpp:114) and the
                        // group put handler turns that into a remote error
                        // (groupsource.cpp:666). Fail the whole atomic PUT here,
                        // in the pre-write conversion phase, before any member
                        // record is touched — nothing has been applied yet, so
                        // the all-or-nothing guarantee holds.
                        let pv_field = get_nested_field(value, &member.def.field_name)
                            .expect("classifier returned Write only for a supplied field");
                        match self.convert_member_value(member, &pv_field) {
                            Some(v) => Some(v),
                            None => {
                                return Err(BridgeError::PutRejected(format!(
                                    "group {} PUT: member '{}' value is not convertible \
                                     to backing field '{}'",
                                    self.def.name, member.def.field_name, member.def.channel
                                )));
                            }
                        }
                    }
                };
                writes.push((member, action, epics_val));
            }

            // pvxs builds a `DBManyLock` over every group-member
            // record (`groupconfigprocessor.cpp:1165`
            // `initialiseDbLocker`) and takes a `DBManyLocker` across
            // the whole atomic PUT member loop
            // (`groupsource.cpp:619-630`). Because `DBManyLock` locks the
            // same lock-set mutexes that a plain `dbPutField` takes via
            // `dbScanLock` (`dbLock.c:187`,`:196`), a direct CA/PVA write to a
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
            // member-record gate, and the per-record gate is
            // not reentrant. From here to the end of this block is
            // synchronous — zero `.await`s reached while `_many_guard`
            // is held.
            let member_records = group_member_record_names(&self.db, &self.def.channels);
            let _many_guard = self.db.lock_records(&member_records);

            for (member, action, val) in writes {
                let (record_name, field_name) = member.names();

                match action {
                    MemberPutAction::ProcessOnly { .. } => {
                        // pvxs skips `IOCSource::put` for this member and goes
                        // straight to `doPostProcessing` (groupsource.cpp:568),
                        // which decides whether the record actually processes.
                        // The gate lives in one owner — never inline here.
                        self.post_process_member_already_locked(member, opts.process)?;
                    }
                    MemberPutAction::Write => {
                        // `_already_locked` — this atomic PUT owns every
                        // member-record gate via `lock_records`. The member's
                        // trap grant (resolved in the per-member ACF pass)
                        // gates `asTrapWrite` emission for this write.
                        let epics_val =
                            val.expect("classifier returned Write, so a value was converted");
                        self.apply_member_value_already_locked(
                            record_name,
                            field_name,
                            epics_val,
                            opts.process,
                            member_grants
                                .get(member.def.field_name.as_str())
                                .copied()
                                .unwrap_or_default(),
                        )?;
                    }
                    MemberPutAction::Skip => continue,
                }
                // pvxs sets `didSomething` from `putGroupField` RETURNING true
                // — `changing || type==Proc` (groupsource.cpp:568-571) — not
                // from a write having landed or doPostProcessing's gate having
                // fired. A participating member that wrote nothing still keeps
                // the PUT out of "No fields changed".
                did_something = true;
            }
        } else {
            // Non-atomic put: write each member individually.
            // IMPORTANT: Proc members are checked BEFORE the request-field
            // lookup because they have no value to read — process_record()
            // must run regardless of whether the request contains that field
            // (matches C++ pdbgroup.cpp:300+ allowProc semantics).
            for member in ordered {
                let (record_name, field_name) = member.names();

                // Same classifier as the atomic loop, same three outcomes.
                let action = classify(member);
                match action {
                    MemberPutAction::Skip => continue,
                    MemberPutAction::ProcessOnly { .. } => {
                        // Same owner as the atomic loop — the gate is C's
                        // `doPostProcessing` (iocsource.cpp:397-403), not "this
                        // member always processes". The non-atomic path holds no
                        // member gate, so the owner takes the gate-acquiring
                        // entry.
                        self.post_process_member(member, opts.process).await?;
                    }
                    MemberPutAction::Write => {
                        // Nested-aware lookup (matches read-side
                        // set_nested_field); the classifier proved the field is
                        // present.
                        let pv_field = get_nested_field(value, &member.def.field_name)
                            .expect("classifier returned Write only for a supplied field");

                        // The field WAS supplied by the client; failing to
                        // convert it is a conversion error, not a no-op. pvxs's
                        // `IOCSource::put` throws on an unsupported conversion
                        // (iocsource.cpp:114) and the group put handler turns
                        // that into a remote error (groupsource.cpp:666),
                        // distinct from the "No fields changed" reply (:658)
                        // which fires only when nothing putable was marked.
                        let epics_val = match self.convert_member_value(member, &pv_field) {
                            Some(v) => v,
                            None => {
                                return Err(BridgeError::PutRejected(format!(
                                    "group {} PUT: member '{}' value is not convertible \
                                     to backing field '{}'",
                                    self.def.name, member.def.field_name, member.def.channel
                                )));
                            }
                        };

                        // non-atomic per-member write — gate-acquiring variants.
                        // The member's trap grant (resolved in the per-member
                        // ACF pass) gates `asTrapWrite` emission for this write.
                        self.apply_member_value(
                            record_name,
                            field_name,
                            epics_val,
                            opts.process,
                            member_grants
                                .get(member.def.field_name.as_str())
                                .copied()
                                .unwrap_or_default(),
                        )
                        .await?;
                    }
                }
                did_something = true;
            }
        }

        // pvxs returns a remote error "No fields changed" when the
        // client marked fields but nothing was actually written
        // (groupsource.cpp:656-659, `!didSomething && value.isMarked`).
        // Approximate `value.isMarked` by "the client supplied at least one
        // group-member field in the incoming value": if so and nothing
        // fired, reject. A genuinely empty PUT (no member field present)
        // stays a silent no-op, matching pvxs (`value.isMarked` false).
        if !did_something {
            let client_supplied_field = self.def.members.iter().any(|m| {
                !m.field_name.is_empty() && get_nested_field(value, &m.field_name).is_some()
            });
            if client_supplied_field {
                return Err(super::put_status::no_fields_changed(&format!(
                    "group {} PUT",
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
        if !self.access.can_read(&self.def.name).await {
            return Err(BridgeError::PutRejected(format!(
                "read denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.creds.user, self.access.creds.host
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
        let opts = super::channel::PutOptions::from_pv_request(value, &RemoteLog::default());
        let atomic_override = super::channel::atomic_from_pv_request(value);
        // In-process callers have no client connection: pvxs's
        // `logRemote` diagnostics have nowhere to go, so the sink is a
        // discard (nothing drains it).
        self.put_with_options(value, opts, atomic_override, &RemoteLog::default())
            .await
    }

    async fn get_field(&self) -> BridgeResult<FieldDesc> {
        let struct_id = self.root_struct_id();
        let mut fields: Vec<(String, FieldDesc)> = Vec::new();

        for member in self.def.channels.iter() {
            if member.def.mapping == FieldMapping::Proc {
                continue;
            }

            // Structure and Const have no backing channel — skip introspection.
            let mut desc = match member.def.mapping {
                FieldMapping::Structure => {
                    let sid = member.def.struct_id.as_deref().unwrap_or("");
                    FieldDesc::Structure {
                        struct_id: sid.into(),
                        fields: Vec::new(),
                    }
                }
                FieldMapping::Const => {
                    // Derive descriptor from the constant value
                    match &member.def.const_value {
                        Some(pv_field) => pv_field_to_field_desc(pv_field),
                        None => FieldDesc::Scalar(ScalarType::Int),
                    }
                }
                _ => {
                    let (nt_type, scalar_type) = self.introspect_member(member).await?;
                    match member.def.mapping {
                        FieldMapping::Scalar => pvif::build_field_desc_for_nt(nt_type, scalar_type),
                        // A `+type:"plain"` leaf carries the bare value with no
                        // NT wrapper. Its type is the channel's value type —
                        // pvxs `addMembersForPlainType`
                        // (groupconfigprocessor.cpp:886-895) builds the leaf
                        // straight from `getChannelValueType`, which is
                        // `valueType.arrayOf()` for an array field and
                        // `TypeCode::String` for a long string. `BareLeaf` is
                        // that one decision, and the read path renders its
                        // value from the same variant, so the descriptor and
                        // the wire bytes cannot disagree (R18-26).
                        FieldMapping::Plain => pvif::BareLeaf::from_nt(nt_type, scalar_type).desc(),
                        FieldMapping::Meta => meta_desc(),
                        // pvxs advertises `+type:"any"` as `Member(TypeCode
                        // ::Any, …)` (groupconfigprocessor.cpp:904-911), an
                        // `any` slot whose concrete payload type is carried
                        // by the value — not a fixed scalar fixed at
                        // introspection time.
                        FieldMapping::Any => FieldDesc::Variant,
                        _ => continue,
                    }
                }
            };
            // `+id` names the struct id of a `+type:"structure"` member and
            // of nothing else: `groupField.id` is read at exactly one place
            // upstream, `addMembersForStructureType`
            // (`groupconfigprocessor.cpp:922-931`). The scalar, plain, any
            // and meta builders never consult it, so an NT leaf keeps the id
            // its own type definition gives it and a meta member stays
            // unnamed.
            if member.def.mapping == FieldMapping::Structure
                && let Some(member_id) = &member.def.struct_id
                && let FieldDesc::Structure { struct_id, .. } = &mut desc
            {
                *struct_id = member_id.clone();
            }

            // Place the descriptor at its (possibly nested) path.
            // The read side uses set_member_field — introspection must
            // emit the same shape so clients see consistent type info.
            set_member_field_desc(&mut fields, &member.def, desc);
        }

        // Advertise the built-in `record._options` branch the value side
        // stamps via push_record_options. Merge it under `record`
        // (descending into a user-built `record` and replacing only the
        // `_options` child) so a user `record.*` member descriptor is
        // preserved and descriptor and payload agree. pvxs carries it in
        // group.valueTemplate via a recursive `TypeDef::_append()` merge,
        // not a whole-`record` replacement (groupconfigprocessor.cpp
        // :499-523, type.cpp:374-389) — and it is member 0 there, which
        // `hoist_record_member` owns.
        hoist_record_member(&mut fields, || FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new(),
        });
        set_nested_field_desc(
            &mut fields,
            "record._options",
            record_options_inner_field_desc(),
        );

        Ok(FieldDesc::Structure {
            struct_id: struct_id.into(),
            fields,
        })
    }

    async fn create_monitor(&self) -> BridgeResult<AnyMonitor> {
        // Read enforcement: deny monitor creation when the client lacks
        // read access. start() also re-checks defensively.
        if !self.access.can_read(&self.def.name).await {
            return Err(BridgeError::PutRejected(format!(
                "monitor create denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.creds.user, self.access.creds.host
            )));
        }
        Ok(AnyMonitor::Group(Box::new(
            GroupMonitor::new(self.db.clone(), self.def.clone())
                .with_access(self.access.clone())
                .with_pump(self.pump.clone()),
        )))
    }
}

// ---------------------------------------------------------------------------
// GroupMonitor
// ---------------------------------------------------------------------------

/// The kind of event received from a member subscription.
#[derive(Debug, Clone, Copy)]
pub(crate) enum MemberEventKind {
    /// Value or alarm change (DBE_VALUE | DBE_ALARM).
    Value,
    /// Property change — display limits, enum choices, etc. (DBE_PROPERTY).
    Property,
}

/// A PVA monitor for a group PV that subscribes to all member records.
///
/// Corresponds to C++ QSRV's `PDBGroupMonitor` + `pdb_group_event()`.
/// `PvaMonitor::start` opens the member subscriptions and registers them
/// with the server-wide `group_pump::GroupPump` — the
/// port of pvxs's single `qsrvGroup` event pump (`ioc/groupsource.cpp:96`).
/// The pump is the only consumer of member events: it resolves the marked
/// leaves, assembles the atomic snapshot and posts the assembled update
/// into this monitor's bounded update queue, which `PvaMonitor::poll`
/// drains. No per-member task exists anywhere — the task cost of a group
/// tick is O(1), not O(members).
pub struct GroupMonitor {
    db: Arc<PvDatabase>,
    def: GroupPvDef,
    running: bool,
    /// Reusable GroupChannel for the monitor-stamped `seed()` read. The
    /// pump's registration carries a clone, so the seed and every drained
    /// update share one stamping by construction.
    group_channel: Option<GroupChannel>,
    /// The server-wide drain this monitor registers with on `start()`.
    /// Injected by `GroupChannel::create_monitor` (the provider's shared
    /// pump); a directly-constructed monitor gets a private pump.
    pump: Arc<super::group_pump::GroupPump>,
    /// This subscription's registration finalizer. `Some` ⟺ the pump is
    /// draining this monitor's member subscriptions. Dropping it (in
    /// [`super::provider::PvaMonitor::stop`] or on monitor drop) is THE teardown path: it
    /// queues the pump's `Deregister`, which releases the member
    /// `DbSubscription`s and this monitor's update-queue producer.
    registration: Option<super::group_pump::RegistrationHandle>,
    /// Consumer half of the pump→monitor update queue. `poll()` parks on
    /// it; `None` from its `recv()` ⟺ the registration left the pump ⟺
    /// teardown. Never "no member events left" — a quiet group parks
    /// (pvxs keeps an all-const subscription open until the client
    /// cancels, `groupsource.cpp:241-298`).
    update_rx: Option<super::group_pump::UpdatePoller>,
    /// Detachable enable/disable handles for every member `DbSubscription`
    /// (value + PROPERTY) opened in [`super::provider::PvaMonitor::start`]. Collected before each
    /// subscription is moved into the pump's registration so the per-op
    /// MONITOR START/STOP gate can `db_event_disable`/`enable` the
    /// whole group's upstream on a client STOP/RESUME — pvxs
    /// `groupsource.cpp` `onStart` toggles every member `dbChannel`.
    activation_handles: Vec<SubscriptionActivation>,
    /// Access control context propagated from the parent GroupChannel.
    access: super::provider::AccessContext,
    /// The NEGOTIATED monitor queue limit the PVA server resolved for this
    /// operation (`MonitorOptions::queue_size` == pvxs `stats.limitQueue`).
    /// Stamped into every monitor value's `record._options.queueSize` via
    /// the internal `GroupChannel`. Defaults to the pvxs per-op default
    /// (`MonitorOp::limit = 4u`) for a caller that has no negotiation to
    /// report — never re-derived from the pvRequest, which the server
    /// already read.
    queue_limit: u32,
}

/// how a member subscription event maps onto a group
/// monitor post — pvxs `groupsource.cpp:306-353` (value) /
/// `:355-385` (property) / `subscriptionPost` `:250-281`.
pub(crate) enum EventMark {
    /// Post the group, marking exactly these group field paths
    /// (the resolved `+trigger` target set, assigned-not-changed).
    Marked(Vec<String>),
    /// No post — `TriggerDef::None`, every named target dropped, or no
    /// target's change classes assign a leaf (pvxs `subscriptionPost`
    /// `if(empty && !first) return`).
    Skip,
}

impl GroupMonitor {
    pub fn new(db: Arc<PvDatabase>, def: GroupPvDef) -> Self {
        Self {
            db,
            def,
            running: false,
            group_channel: None,
            pump: super::group_pump::GroupPump::new(),
            registration: None,
            update_rx: None,
            activation_handles: Vec::new(),
            access: super::provider::AccessContext::allow_all(),
            queue_limit: epics_pva_rs::server_native::source::DEFAULT_MONITOR_QUEUE_LIMIT,
        }
    }

    /// Share the server-wide group drain. Called by
    /// `GroupChannel::create_monitor` with the provider's pump; a monitor
    /// built without it drains through a private pump.
    pub(crate) fn with_pump(mut self, pump: Arc<super::group_pump::GroupPump>) -> Self {
        self.pump = pump;
        self
    }

    /// Inject an access control context. Called by `GroupChannel::create_monitor`.
    pub fn with_access(mut self, access: super::provider::AccessContext) -> Self {
        self.access = access;
        self
    }

    /// Carry the per-operation negotiated queue limit the PVA server handed
    /// the source (`MonitorOptions::queue_size`). Threaded into the internal
    /// `GroupChannel` so every monitor value reports the depth the
    /// subscription ACTUALLY got — pvxs reads it back from the subscription
    /// control (`stats.limitQueue`, groupsource.cpp:401-404) rather than
    /// parsing `record._options.queueSize` a second time.
    pub fn with_queue_size(mut self, limit: u32) -> Self {
        self.queue_limit = limit;
        self
    }

    /// Detachable enable/disable handles for every member subscription
    /// opened in [`super::provider::PvaMonitor::start`] (value + PROPERTY per channeled
    /// member). Used by the per-op MONITOR START/STOP gate so
    /// a client STOP suspends the whole group's upstream event flow.
    /// Empty before `start()`.
    pub fn activation_handles(&self) -> Vec<SubscriptionActivation> {
        self.activation_handles.clone()
    }

    /// Monitor-stamped seed snapshot for the MONITOR INIT DATA frame.
    ///
    /// pvxs delivers the first monitor post through the same
    /// `currentValue` that carries every subsequent update: `onSubscribe`
    /// stamps `record._options.atomic = true` and
    /// `record._options.queueSize = stats.limitQueue` on that value once
    /// (`groupsource.cpp:401-405`), then primes and posts it. The QSRV GET
    /// seed path (`Channel::get` → GET-stamped `read_group_atomic`) instead
    /// stamps the *operation* atomicity and `queueSize = 0`
    /// (`groupsource.cpp:480-485`), so seeding a group monitor from GET made
    /// the initial frame's `record._options` disagree with the update
    /// stream. Read through the monitor's own `group_channel` — built in
    /// [`super::provider::PvaMonitor::start`] with `with_monitor_stamp`/
    /// `with_monitor_queue_size`, the identical value path
    /// [`super::provider::PvaMonitor::poll`] drains — so the seed and the deltas share one
    /// stamping by construction. Returns the full (unfiltered) group value,
    /// matching the update stream and pvxs's fully-marked first event; the
    /// wire layer applies the client's pvRequest field mask uniformly to
    /// both. `None` before `start()` (no `group_channel`) or on a read
    /// error.
    pub async fn seed(&self) -> Option<PvStructure> {
        self.group_channel.as_ref()?.read_group().await.ok()
    }

    /// resolve the marked-leaf field paths for a *value*
    /// event from `source_idx`, mirroring pvxs `groupsource.cpp:328`
    /// iterating `field.triggers` and marking each target.
    ///
    /// A *pure self-trigger* group — the DEFAULT `+trigger` shape — is not
    /// special: pvxs seeds `field.triggers` with the field itself
    /// (`groupconfigprocessor.cpp:317-339`), so the self-triggered member
    /// runs the very same `IOCSource::get` + mark loop as an explicit
    /// `+trigger` target. Routing it through `marked_leaves` like every
    /// other trigger shape is what gives it the same leaf narrowing; the
    /// snapshot-diff path it used to take was two-sided divergence — WIDER
    /// (a property event diffed timeStamp/alarm leaves that pvxs's
    /// `UpdateType::Property` never assigns) and NARROWER (a value event
    /// whose limits did not change diffed to nothing, where pvxs carries
    /// them assigned-not-changed).
    ///
    /// Takes `&GroupPvDef` (not `&self`) so `poll` can call it while
    /// holding the `&mut self.event_rx` borrow — `def` is a disjoint
    /// field.
    ///
    /// `event_mask` is the event's DBE mask. pvxs refreshes every
    /// triggered target with `Value | Alarm`, EXCEPT the self-triggered
    /// field, which uses `pDbFieldLog->mask & UpdateType::Everything`
    /// (`groupsource.cpp:331-337`) — so an ALARM-only post on the source
    /// re-sends its own alarm/timeStamp but not its value, and an
    /// ARCHIVE-only post (DBE_LOG, masked out of `Everything`)
    /// contributes no self leaves at all. An empty mask (a legacy
    /// unmasked post carries no classification) falls back to
    /// `Value | Alarm`, the same default pvxs uses when no field log is
    /// available (pre-7.0.6 builds).
    pub(crate) fn value_event_mark(
        def: &GroupPvDef,
        props: &[PropertySupport],
        source_idx: usize,
        event_mask: DbeMask,
    ) -> EventMark {
        let Some(source) = def.members.get(source_idx) else {
            return EventMark::Skip;
        };
        let trigger_change = DbeMask::VALUE | DbeMask::ALARM;
        let everything = DbeMask::VALUE | DbeMask::ALARM | DbeMask::PROPERTY;
        let self_change = if event_mask.is_empty() {
            trigger_change
        } else {
            event_mask & everything
        };
        let change_for = |idx: usize| {
            if idx == source_idx {
                self_change
            } else {
                trigger_change
            }
        };
        let targets: Vec<(usize, &GroupMember, DbeMask)> = match &source.triggers {
            TriggerDef::None => return EventMark::Skip,
            // Self-trigger inside a mixed group marks only its own field.
            TriggerDef::SelfOnly => vec![(source_idx, source, self_change)],
            // `"*"` marks every member field WITH A CHANNEL. pvxs drops
            // channel-less Const/Structure targets from the `*` expansion
            // (`groupconfigprocessor.cpp:387-390`: `if(!…channel.empty())`)
            // — a channel-less member never produces a runtime event, so
            // marking it would flag it "changed" + re-serialize on every
            // update for nothing.
            TriggerDef::All => def
                .members
                .iter()
                .enumerate()
                .filter(|(_, m)| !m.channel.is_empty())
                .map(|(i, m)| (i, m, change_for(i)))
                .collect(),
            // Named targets: pvxs resolves only references that name an
            // existing field WITH A CHANNEL (`groupconfigprocessor.cpp:
            // 405-410`: a target whose `channel.empty()` is ignored).
            // Unknown refs were warned + dropped at parse time and are
            // absent from `members` here too.
            TriggerDef::Fields(refs) => def
                .members
                .iter()
                .enumerate()
                .filter(|(_, m)| !m.channel.is_empty() && refs.iter().any(|r| r == &m.field_name))
                .map(|(i, m)| (i, m, change_for(i)))
                .collect(),
        };
        Self::marked_leaves(props, targets)
    }

    /// a *property* event marks only the source field's
    /// own mapping and never its triggers — pvxs `groupsource.cpp:371-373`
    /// ("we (may) only post changes to the field mapping in question.
    /// But never the triggered fields."). The trigger graph is not
    /// consulted at all, so a pure self-trigger group takes this path like
    /// any other: `UpdateType::Property` assigns only the property leaves,
    /// and marking timeStamp/alarm here (as the snapshot diff did) is
    /// exactly the divergence `getTimeAlarm`'s `change & (Value | Alarm)`
    /// gate rules out (`iocsource.cpp:331-333`).
    pub(crate) fn property_event_mark(
        def: &GroupPvDef,
        props: &[PropertySupport],
        source_idx: usize,
    ) -> EventMark {
        let Some(source) = def.members.get(source_idx) else {
            return EventMark::Skip;
        };
        // pvxs passes `UpdateType::Property` unconditionally for a
        // property event (`groupsource.cpp:378`) — the event's own DBE
        // mask is not consulted.
        Self::marked_leaves(props, vec![(source_idx, source, DbeMask::PROPERTY)])
    }

    /// Expand each `(member, change)` pair into the wire leaves its
    /// change classes actually assign, mirroring pvxs `IOCSource::get`
    /// (`iocsource.cpp:312-352`). This is the leaf narrowing —
    /// previously a marked member flagged its WHOLE subtree, so a
    /// DBE_PROPERTY event re-sent value/alarm/timeStamp and a DBE_VALUE
    /// event re-sent the display/control limits.
    ///
    /// A root-flattened member (`field_name == ""`) is NOT special. The
    /// only mapping allowed at the struct top is `+type:"meta"`
    /// (`groupconfigprocessor.cpp:224-231`, mirrored in
    /// `group_config::parse_group_config`), whose leaves land at the group
    /// root as `alarm` / `timeStamp` (`set_member_field`) — both nameable,
    /// so [`pvif::change_leaf_paths`] with an empty prefix addresses them
    /// exactly. pvxs marks a root member through the same `Field::findIn`
    /// mark loop as any other (`field.cpp:56-81` returns the root value
    /// unchanged for an empty `fieldName`; `iocsource.cpp:312-352` runs the
    /// identical assignment), so it neither forces a full-value post nor
    /// widens the group's leaf narrowing.
    ///
    /// An empty target list → [`EventMark::Skip`]. A target whose change
    /// classes contribute no leaf (a `Const`/`Structure`/`Proc` member, a
    /// `Meta`/`Plain`/`Any` member on a property change, or a
    /// self-trigger whose event was ARCHIVE-only) is dropped; if every
    /// target drops, the post carries nothing → [`EventMark::Skip`]
    /// (pvxs `subscriptionPost` `if(empty && !first) return`).
    ///
    /// The per-mapping leaf set is [`pvif::change_leaf_paths`] — the one
    /// owner of pvxs `IOCSource::get`'s assignment, shared with the
    /// single-record monitor.
    fn marked_leaves(
        props: &[PropertySupport],
        targets: Vec<(usize, &GroupMember, DbeMask)>,
    ) -> EventMark {
        let mut leaves = Vec::new();
        for (idx, m, change) in targets {
            leaves.extend(super::pvif::change_leaf_paths(
                &m.field_name,
                m.mapping,
                change,
                props.get(idx).copied().unwrap_or(PropertySupport::NONE),
            ));
        }
        if leaves.is_empty() {
            return EventMark::Skip;
        }
        EventMark::Marked(leaves)
    }
}

impl super::provider::PvaMonitor for GroupMonitor {
    async fn start(&mut self) -> BridgeResult<()> {
        if self.running {
            return Ok(());
        }

        // Read enforcement: refuse to spin up upstream subscriptions
        // for a client that lacks read permission on this group.
        if !self.access.can_read(&self.def.name).await {
            return Err(BridgeError::PutRejected(format!(
                "monitor read denied for group {} (user='{}' host='{}')",
                self.def.name, self.access.creds.user, self.access.creds.host
            )));
        }

        // Resolve every member's property mask once, before the first event
        // can arrive — which leaves that member's channel actually supplies
        // (`dbChannelGet`'s narrowing of `getProperties`'s option mask). Built
        // by mapping over `def.members`, so the index correspondence
        // `member_props[i] <-> def.members[i]` holds by construction.
        let member_props = {
            let mut props = Vec::with_capacity(self.def.channels.len());
            for member in self.def.channels.iter() {
                props.push(super::provider::member_property_support(&self.db, member).await);
            }
            props
        };

        // Member subscriptions collect HERE and move into the pump's
        // registration below — no task is spawned per member. The pump is
        // the single consumer (pvxs's one `qsrvGroup` event thread,
        // `ioc/groupsource.cpp:96`); member events queue in each
        // subscription's own EvQue (C dbEvent ring, replace-in-place under
        // pressure) until the drain takes them.
        let mut member_subs: Vec<super::group_pump::MemberSub> = Vec::new();

        // Subscribe to ALL members with channels, regardless of trigger
        // setting — pvxs subscribes every field with a dbChannel
        // (groupsource.cpp:410-444). TriggerDef::None only means "don't
        // update the group when this field changes"; its events are
        // filtered to EventMark::Skip in the pump rather than gating the
        // stream.
        for (idx, member) in self.def.channels.iter().enumerate() {
            if !member.has_channel() {
                continue; // Structure/Const/Proc-without-channel — no backing channel
            }

            // subscribe value events against the full
            // `member.channel` (e.g. `REC.RVAL`), not the bare record
            // name. pvxs `field.cpp:25-26` builds both the value and
            // properties dbChannels from the same `def.channel`
            // (`groupsource.cpp:431,440`), so the subscription identity
            // is the configured member field. The previous code parsed
            // off the field suffix and subscribed against `REC.VAL`, so
            // a non-`VAL` member woke on unrelated `VAL` posts and
            // missed posts made only by its own field. `record_name`
            // is no longer needed here — the read path
            // (`read_member`) re-parses `member.channel` for the
            // field it actually decodes.
            //
            // choose the value mask per member mapping.
            // pvxs `groupsource.cpp:429-431` subscribes `Meta` value-side
            // events with `DBE_ALARM` only; `groupsource.cpp:432-434` uses
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
            let value_mask = if member.def.mapping == FieldMapping::Meta {
                epics_base_rs::server::recgbl::EventMask::ALARM.bits()
            } else {
                (epics_base_rs::server::recgbl::EventMask::VALUE
                    | epics_base_rs::server::recgbl::EventMask::ALARM
                    | epics_base_rs::server::recgbl::EventMask::LOG)
                    .bits()
            };
            // The member's OWN value chain, bound once on the
            // `MemberChannel` — a `{"dbnd":…}` / `{"dec":…}` `+channel`
            // gates this member's events exactly as it gates a
            // single-record monitor's. Re-parsing the suffix here would
            // hand every resubscribe a fresh baseline and never filter.
            if let Some(sub) = DbSubscription::subscribe_with_mask_and_filters(
                &self.db,
                &member.pv_name,
                0,
                value_mask,
                Some(&member.value_filters),
            )
            .await
            {
                // Capture the enable/disable handle BEFORE the subscription
                // moves into the pump's registration, so the per-op gate
                // can toggle this member's event flow without owning the sub.
                // The pump drains with `poll_recv_event` (not a snapshot
                // read) so the per-event DBE mask reaches the mark
                // resolution — pvxs reads `pDbFieldLog->mask` for the
                // self-trigger narrowing.
                self.activation_handles.push(sub.activation_handle());
                member_subs.push(super::group_pump::MemberSub {
                    member_index: idx,
                    kind: MemberEventKind::Value,
                    sub,
                });
            }

            // Property subscription (DBE_PROPERTY) — only for Scalar/Meta
            // mappings that include metadata. Plain/Any/Proc don't need it.
            // target the same `member.channel` as the value
            // subscription (pvxs `field.cpp:26` derives the properties
            // dbChannel from the identical `def.channel`); the record
            // default would mis-scope members configured on a non-`VAL`
            // field.
            if member.def.mapping == FieldMapping::Scalar
                || member.def.mapping == FieldMapping::Meta
            {
                let prop_mask = epics_base_rs::server::recgbl::EventMask::PROPERTY.bits();
                // The property channel's INDEPENDENT chain — see
                // `MemberChannel::property_filters`.
                if let Some(sub) = DbSubscription::subscribe_with_mask_and_filters(
                    &self.db,
                    &member.pv_name,
                    0,
                    prop_mask,
                    Some(&member.property_filters),
                )
                .await
                {
                    self.activation_handles.push(sub.activation_handle());
                    member_subs.push(super::group_pump::MemberSub {
                        member_index: idx,
                        kind: MemberEventKind::Property,
                        sub,
                    });
                }
            }
        }

        // Create a reusable GroupChannel once (instead of per-event in the
        // drain). Propagate the same access context so any subsequent reads
        // triggered by trigger evaluation also honor read enforcement.
        //
        // The monitor stamp carries the per-operation negotiated queue limit,
        // so every monitor value stamps `record._options.queueSize` with the
        // depth the subscription actually got (pvxs `groupsource.cpp:404`
        // `stats.limitQueue`). The pump's registration clones this channel,
        // so the seed and every drained update share one stamping by
        // construction.
        let group_channel = GroupChannel::new(self.db.clone(), self.def.clone())
            .with_access(self.access.clone())
            .with_monitor_stamp(self.queue_limit);

        // Register with the server-wide drain. The update queue's producer
        // lives in the pump's registration for this monitor's whole
        // subscribed life, so `poll()` *parks* on a quiet stream (all-const
        // group, every member quiet) instead of reading end-of-stream —
        // pvxs keeps an all-const subscription open until the client
        // cancels (groupsource.cpp:241-298). Its capacity is the negotiated
        // queue limit; on overflow the newest update replaces the tail in
        // place (C monitor latest-value coalescing).
        let (update_tx, update_rx) =
            super::group_pump::update_queue(self.queue_limit.max(1) as usize);
        let registration = self.pump.register(super::group_pump::RegistrationSpec {
            def: self.def.clone(),
            member_props,
            group_channel: group_channel.clone(),
            subs: member_subs,
            update_tx,
        });

        self.group_channel = Some(group_channel);
        self.update_rx = Some(update_rx);
        self.registration = Some(registration);
        self.running = true;
        Ok(())
    }

    async fn poll(&mut self) -> Option<super::provider::MonitorPoll> {
        // Purely event-driven: the wire layer already sent the initial
        // frame via read_checked() at MONITOR INIT for every source
        // (server_native/tcp.rs:build_monitor_payload), so this stream
        // carries only fresh assembled updates — never an initial snapshot.
        //
        // Everything per-event — trigger/mark resolution, the atomic
        // `read_group()` snapshot, enum-leaf narrowing, the skip on a
        // markless event or a per-event read failure (pvxs
        // `groupsource.cpp:350-352` parity) — happens in the server-wide
        // drain (`group_pump::process_event`), which posts assembled
        // updates into this monitor's bounded update queue. This method
        // only parks on that queue.
        //
        // A quiet group (all-const, every member idle) parks here: the
        // queue's producer lives in the pump's registration for the whole
        // subscribed life, so `recv()` cannot read end-of-stream early —
        // pvxs keeps an all-const subscription open until the client
        // cancels (groupsource.cpp:241-298). `None` therefore means
        // teardown (`stop()` cleared `update_rx`, or the registration left
        // the pump) — never "no member events left to forward".
        self.update_rx.as_mut()?.recv().await
    }

    async fn stop(&mut self) {
        // The ONE teardown path. Dropping `update_rx` marks the consumer
        // side closed (a drain push after this fails visibly and routes the
        // registration out through the pump's removal path); dropping
        // `registration` queues the pump's `Deregister` finalizer, which
        // releases the member `DbSubscription`s and the update-queue
        // producer — and terminates the drain task when this was the last
        // group subscription. A later poll() sees `update_rx == None` and
        // reports teardown.
        self.update_rx = None;
        self.registration = None;

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
    /// apply the per-operation negotiated monitor queue limit the PVA
    /// server resolved (`MonitorOptions::queue_size`). Only a group monitor
    /// stamps `record._options.queueSize` into its values — a single-record
    /// monitor has no group-style `record._options` branch, so this is a
    /// no-op for the `Single` variant.
    pub fn with_queue_size(self, limit: u32) -> Self {
        match self {
            Self::Group(m) => Self::Group(Box::new(m.with_queue_size(limit))),
            single => single,
        }
    }

    /// Monitor-stamped seed snapshot for the MONITOR INIT DATA frame, or
    /// `None` to fall back to the GET seed. Only a group monitor returns a
    /// value: its initial frame must share the monitor `record._options`
    /// stamping (`atomic = true`, negotiated `queueSize`) with its update
    /// stream — see [`GroupMonitor::seed`] and pvxs `groupsource.cpp:401-405`
    /// vs the GET path `:480-485`. A single-record monitor has no group-style
    /// `record._options` branch, so its GET seed and monitor frames already
    /// carry identical options; it returns `None` and the adapter keeps the
    /// GET seed. Valid after [`super::provider::PvaMonitor::start`].
    pub async fn seed(&self) -> Option<PvField> {
        match self {
            Self::Single(_) => None,
            Self::Group(m) => m.seed().await.map(PvField::Structure),
        }
    }

    /// Detachable enable/disable handles for this monitor's backing
    /// `DbSubscription`s, for the per-op MONITOR START/STOP gate.
    /// Valid after [`super::provider::PvaMonitor::start`].
    pub fn activation_handles(&self) -> Vec<SubscriptionActivation> {
        match self {
            Self::Single(m) => m.activation_handles(),
            Self::Group(m) => m.activation_handles(),
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

/// The `+type:"meta"` member shape: `{alarm, timeStamp}` under an
/// **unnamed** enclosing structure.
///
/// pvxs's `addMembersForMetaData` builds only those two members and hands
/// them to `setFieldTypeDefinition(..., isLeaf = false)`
/// (`groupconfigprocessor.cpp:940-953`), which wraps each path component
/// with the two-argument `members::Struct(name, children)` overload
/// (`:1031-1033`, `include/pvxs/data.h:348-354`) — the one that carries no
/// id. So the wire shows `structure m` with a single zero id byte, not
/// `meta_t m` with a seven-byte id string. A root meta member is spliced
/// straight into the group root and never sees this id at all.
fn meta_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: String::new(),
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
            pvif::alarm_condition_string(snapshot.alarm.status)
                .to_string()
                .into(),
        )),
    ));
    alarm
}

/// Build a `timeStamp` PvStructure directly from the record snapshot.
///
/// The nanosecond / userTag split for `info(Q:time:tag, "nsec:lsb:N")`
/// is applied once, at the record level, when the snapshot is built
/// (`epics_base_rs` `record_instance.rs` via `apply_nsec_mask`), so
/// `snapshot.timestamp` already carries the masked nanoseconds and
/// `snapshot.user_tag` already carries the split bits / `common.utag`.
/// The group encoder therefore serves them verbatim and must not remask
/// — pvxs derives the same split from the record's `Q:time:tag`
/// (`ioc/typeutils.cpp:79-87`, `ioc/iocsource.cpp:240-248`), never from
/// group JSON.
fn build_timestamp_from_snapshot(
    snapshot: &epics_base_rs::server::snapshot::Snapshot,
) -> PvStructure {
    use epics_pva_rs::pvdata::ScalarValue;

    let mut ts = PvStructure::new("time_t");
    let dur = snapshot.timestamp.since_unix_epoch();
    let (secs, nanos) = (dur.as_secs() as i64, dur.subsec_nanos() as i32);
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
        PvField::Scalar(ScalarValue::Int(snapshot.user_tag)),
    ));
    ts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qsrv::provider::Channel;
    use std::time::Duration;

    /// Has the record processed since it was added to the database?
    ///
    /// `ai.INIT` is C's `prec->init`: `init_record` sets it (so a record that
    /// has only been added reads 1) and the end of every `process` clears it
    /// (`aiRecord.c:114`, `:170`). The proc-member cases below use it as the
    /// "did the group PUT process this record?" probe; reading the phase
    /// through this helper keeps the C polarity in ONE place.
    async fn has_processed(db: &Arc<PvDatabase>, rec: &str) -> bool {
        use epics_base_rs::types::EpicsValue;
        match db.get_pv(&format!("{rec}.INIT")).unwrap() {
            EpicsValue::Short(v) => v == 0,
            other => panic!("unexpected INIT type: {other:?}"),
        }
    }

    /// Every member supplies every property — the mask a channel on an
    /// `mbbi`/`ai` VAL field resolves to. The leaf-narrowing cases below are
    /// about the DBE CHANGE CLASSES (which leaves a value / property event
    /// marks); the rset narrowing that decides which of those leaves the
    /// record type supplies at all is a separate boundary, covered by the
    /// `property_support` cases in `pvif` and `record_instance`.
    fn full_props(def: &GroupPvDef) -> Vec<PropertySupport> {
        vec![
            PropertySupport {
                enum_strs: true,
                ..PropertySupport::NUMERIC
            };
            def.members.len()
        ]
    }

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

    /// a group `meta` timeStamp serves the record snapshot's userTag and
    /// nanoseconds verbatim. The `Q:time:tag` nsec-LSB split is applied
    /// once at the record level (`record_instance.rs`
    /// `apply_nsec_mask`), so the group encoder must not remask —
    /// pvxs derives the split from the record, never from group JSON
    /// (`ioc/iocsource.cpp:240-248`). `Snapshot.user_tag` defaults to the
    /// record's `common.utag` (pvxs `iocsource.cpp:245`); the bit-31 tag
    /// pins that the snapshot's i32 userTag passes through unchanged and
    /// nanoseconds are not stripped by any group-level mask.
    #[test]
    fn group_timestamp_serves_record_snapshot_verbatim() {
        use epics_base_rs::server::snapshot::Snapshot;
        use epics_base_rs::types::{EpicsValue, WallTime};
        use epics_pva_rs::pvdata::{PvField, ScalarValue};

        let int_field = |s: &PvStructure, name: &str| match s.get_field(name) {
            Some(PvField::Scalar(ScalarValue::Int(v))) => *v,
            other => panic!("{name} must be Int, got {other:?}"),
        };

        // 0xFF ns (255) injected as exact integers; a `SystemTime` rounds it
        // to 200 ns on Windows (FILETIME 100 ns units) and breaks the verbatim
        // pass-through this test asserts.
        let mut snap = Snapshot::new(
            EpicsValue::Double(1.0),
            0,
            0,
            WallTime::from_unix(1_700_000_000, 0x0000_00FF),
        );
        snap.user_tag = 0x9000_0000u32 as i32;

        let ts = build_timestamp_from_snapshot(&snap);
        // userTag is the snapshot's (record) utag, not 0 and not remasked.
        assert_eq!(
            int_field(&ts, "userTag"),
            0x9000_0000u32 as i32,
            "group member must serve the record's utag verbatim"
        );
        // nanoseconds pass through unmasked at the group level.
        assert_eq!(
            int_field(&ts, "nanoseconds"),
            0x0000_00FF,
            "group member must serve the snapshot nanoseconds verbatim"
        );
    }

    /// a `+trigger` target without a backing channel (Const /
    /// Structure member) must NOT be marked in the changed-bitset. pvxs
    /// filters channel-less members out of BOTH the `*` expansion
    /// (`groupconfigprocessor.cpp:387-390`) and named-target resolution
    /// (`405-410`). Pre-fix the Rust `*` and named arms marked them, so a
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
        match GroupMonitor::value_event_mark(
            &def,
            &full_props(&def),
            src_idx(&def),
            DbeMask::VALUE | DbeMask::ALARM,
        ) {
            EventMark::Marked(paths) => {
                assert!(
                    paths.iter().any(|p| p == "chan" || p.starts_with("chan.")),
                    "channeled member must be marked: {paths:?}"
                );
                assert!(
                    !paths.iter().any(|p| p == "meta" || p.starts_with("meta.")),
                    "channel-less structure member must NOT be marked by `*`: {paths:?}"
                );
            }
            EventMark::Skip => panic!("expected Marked from a `*` trigger, got Skip"),
        }

        // Named arm: a named channel-less target is dropped, channeled kept.
        let named = r#"{ "GRP2": {
            "src":  { "+channel": "R:src", "+trigger": "chan,meta" },
            "chan": { "+channel": "R:chan" },
            "meta": { "+type": "structure", "+id": "x/v1" }
        } }"#;
        let def = parse_group_config(named).unwrap().pop().unwrap();
        match GroupMonitor::value_event_mark(
            &def,
            &full_props(&def),
            src_idx(&def),
            DbeMask::VALUE | DbeMask::ALARM,
        ) {
            EventMark::Marked(paths) => {
                assert!(
                    paths.iter().any(|p| p == "chan" || p.starts_with("chan.")),
                    "named channeled target must be marked: {paths:?}"
                );
                assert!(
                    !paths.iter().any(|p| p == "meta" || p.starts_with("meta.")),
                    "named channel-less target must NOT be marked: {paths:?}"
                );
            }
            EventMark::Skip => panic!("expected Marked from named triggers, got Skip"),
        }
    }

    /// a marked member is narrowed to the leaves its
    /// UpdateType actually assigns, not its whole subtree. A DBE_VALUE
    /// event marks value/alarm/timeStamp; a DBE_PROPERTY event marks the
    /// individual property leaves `getProperties` assigns
    /// (`iocsource.cpp:253-310`) — NOT the whole `display` / `control` /
    /// `valueAlarm` structures, which carry leaves pvxs never touches
    /// (`display.form`, `control.minStep`, `valueAlarm.active`, the four
    /// `*Severity` fields, `valueAlarm.hysteresis`). Neither event class
    /// crosses into the other's leaves, and a property event never marks
    /// triggered members. Mirrors pvxs `IOCSource::get`
    /// (`iocsource.cpp:312-352`).
    #[test]
    fn event_marks_narrow_to_update_type_leaves() {
        use crate::qsrv::group_config::parse_group_config;

        // Mixed-trigger group (a `*` member) so a value event marks the
        // triggered member `b` as well as the source.
        let json = r#"{ "GRP": {
            "a": { "+channel": "R:a", "+trigger": "*" },
            "b": { "+channel": "R:b" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        let src = def
            .members
            .iter()
            .position(|m| m.field_name == "a")
            .unwrap();

        // Value event: value/alarm/timeStamp of every channeled member,
        // never the property-metadata leaves.
        let EventMark::Marked(v) = GroupMonitor::value_event_mark(
            &def,
            &full_props(&def),
            src,
            DbeMask::VALUE | DbeMask::ALARM,
        ) else {
            panic!("expected Marked from a `*` value event");
        };
        for member in ["a", "b"] {
            assert!(v.contains(&format!("{member}.value")), "value leaf: {v:?}");
            assert!(v.contains(&format!("{member}.alarm")), "alarm leaf: {v:?}");
            assert!(
                v.contains(&format!("{member}.timeStamp")),
                "timeStamp leaf: {v:?}"
            );
        }
        assert!(
            !v.iter().any(|p| p.ends_with(".display")
                || p.ends_with(".control")
                || p.ends_with(".valueAlarm")),
            "value event must NOT mark property-metadata leaves: {v:?}"
        );

        // Property event: exactly the leaves `getProperties`
        // (`iocsource.cpp:253-310`) assigns on the source member — never
        // value/alarm/timeStamp, never the triggered member `b`, and never
        // the parent `display` / `control` / `valueAlarm` structures.
        let EventMark::Marked(p) = GroupMonitor::property_event_mark(&def, &full_props(&def), src)
        else {
            panic!("expected Marked from a property event");
        };
        assert_eq!(
            p,
            vec![
                "a.display.units",
                "a.value.choices",
                "a.display.limitLow",
                "a.display.limitHigh",
                "a.display.precision",
                "a.control.limitLow",
                "a.control.limitHigh",
                "a.valueAlarm.lowAlarmLimit",
                "a.valueAlarm.lowWarningLimit",
                "a.valueAlarm.highWarningLimit",
                "a.valueAlarm.highAlarmLimit",
                "a.display.description",
            ],
            "pvxs getProperties leaf list (iocsource.cpp:253-310)"
        );
        // The leaves the port's NT carries but pvxs never assigns must NOT
        // be marked — marking the parent structure would have marked them.
        for never in [
            "a.display",
            "a.control",
            "a.valueAlarm",
            "a.display.form",
            "a.control.minStep",
            "a.valueAlarm.active",
            "a.valueAlarm.lowAlarmSeverity",
            "a.valueAlarm.lowWarningSeverity",
            "a.valueAlarm.highWarningSeverity",
            "a.valueAlarm.highAlarmSeverity",
            "a.valueAlarm.hysteresis",
        ] {
            assert!(
                !p.iter().any(|leaf| leaf == never),
                "getProperties never assigns {never}: {p:?}"
            );
        }
        assert!(
            !p.iter()
                .any(|leaf| leaf == "a.value" || leaf == "a.alarm" || leaf == "a.timeStamp"),
            "property event must NOT mark value/alarm/timeStamp: {p:?}"
        );
        assert!(
            !p.iter().any(|leaf| leaf.starts_with("b.")),
            "property event must never mark triggered members: {p:?}"
        );
    }

    /// R14-32 — a PURE self-trigger group (no member declares `+trigger`:
    /// the default shape, and the common one) marks leaves through the
    /// same path as every explicit trigger graph. pvxs seeds
    /// `field.triggers` with the field itself
    /// (`groupconfigprocessor.cpp:317-339`) and then runs the identical
    /// `IOCSource::get` mark loop (`groupsource.cpp:328-346`), so there is
    /// no snapshot-diff special case to take.
    ///
    /// The diff path it used to take diverged on BOTH sides, so both are
    /// pinned here as boundaries:
    ///
    /// * WIDER — a property event: the diff marked whatever leaves changed,
    ///   including `timeStamp` (a record's property post restamps it). pvxs
    ///   passes `UpdateType::Property` (`groupsource.cpp:378`), and
    ///   `getTimeAlarm` is gated on `change & (Value | Alarm)`
    ///   (`iocsource.cpp:331-333`), so timeStamp/alarm/value are NEVER
    ///   assigned on a property event.
    /// * NARROWER — a value event whose bytes did not change (a re-post at
    ///   the same value, or metadata leaves the record rewrote identically)
    ///   diffed to nothing, framing an empty changed-bitset. pvxs marks
    ///   every leaf its UpdateType assigns, changed or not, so the mark set
    ///   is a pure function of the DBE mask and the mapping.
    #[test]
    fn pure_self_trigger_group_marks_leaves_like_every_other_trigger() {
        use crate::qsrv::group_config::parse_group_config;

        // No `+trigger` anywhere → every channeled member is self-triggered.
        let json = r#"{ "GRP": {
            "a": { "+channel": "R:a" },
            "b": { "+channel": "R:b" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        assert!(
            def.is_pure_self_trigger(),
            "the default +trigger shape is a pure self-trigger group"
        );
        let src = def
            .members
            .iter()
            .position(|m| m.field_name == "a")
            .unwrap();

        // Property boundary: exactly the `getProperties` leaves of the
        // source member — no timeStamp, no alarm, no value, no other member.
        let EventMark::Marked(p) = GroupMonitor::property_event_mark(&def, &full_props(&def), src)
        else {
            panic!("a pure self-trigger property event must mark leaves, not derive");
        };
        assert!(
            p.contains(&"a.display.limitLow".to_string())
                && p.contains(&"a.valueAlarm.highAlarmLimit".to_string()),
            "the property leaves getProperties assigns: {p:?}"
        );
        assert!(
            !p.iter()
                .any(|leaf| leaf == "a.timeStamp" || leaf == "a.alarm" || leaf == "a.value"),
            "UpdateType::Property never reaches getTimeAlarm: {p:?}"
        );
        assert!(
            !p.iter().any(|leaf| leaf.starts_with("b.")),
            "a property event marks only the source's own mapping: {p:?}"
        );

        // Value boundary: the leaves the DBE mask assigns on the SELF member,
        // marked assigned-not-changed — carried whether or not they differ
        // from the last snapshot. Property leaves stay out (getProperties is
        // gated on `change & Property`).
        let EventMark::Marked(v) = GroupMonitor::value_event_mark(
            &def,
            &full_props(&def),
            src,
            DbeMask::VALUE | DbeMask::ALARM,
        ) else {
            panic!("a pure self-trigger value event must mark leaves, not derive");
        };
        assert_eq!(
            v,
            vec!["a.timeStamp", "a.alarm", "a.value"],
            "Value|Alarm assigns timeStamp + alarm + value on the self member"
        );

        // An ALARM-only self post carries no value leaf; a VALUE-only one
        // carries no alarm leaf (`getTimeAlarm`'s `change & Alarm` gate).
        let EventMark::Marked(v) =
            GroupMonitor::value_event_mark(&def, &full_props(&def), src, DbeMask::ALARM)
        else {
            panic!("expected Marked from an alarm-only event");
        };
        assert_eq!(v, vec!["a.timeStamp", "a.alarm"], "no value leaf: {v:?}");
        let EventMark::Marked(v) =
            GroupMonitor::value_event_mark(&def, &full_props(&def), src, DbeMask::VALUE)
        else {
            panic!("expected Marked from a value-only event");
        };
        assert_eq!(v, vec!["a.timeStamp", "a.value"], "no alarm leaf: {v:?}");
    }

    /// a `+type:meta` member marks alarm/timeStamp (no
    /// value) on a value event and contributes nothing on a property
    /// event (pvxs getProperties is Scalar-only); a value-only `plain`
    /// member marks the member node whole (no metadata sub-tree).
    #[test]
    fn meta_and_plain_member_leaves() {
        use crate::qsrv::group_config::parse_group_config;

        let json = r#"{ "GRP": {
            "m": { "+channel": "R:m", "+type": "meta", "+trigger": "*" },
            "p": { "+channel": "R:p", "+type": "plain" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        let src = def
            .members
            .iter()
            .position(|m| m.field_name == "m")
            .unwrap();

        let EventMark::Marked(v) = GroupMonitor::value_event_mark(
            &def,
            &full_props(&def),
            src,
            DbeMask::VALUE | DbeMask::ALARM,
        ) else {
            panic!("expected Marked");
        };
        // meta member: alarm + timeStamp, no value leaf.
        assert!(v.contains(&"m.alarm".to_string()), "{v:?}");
        assert!(v.contains(&"m.timeStamp".to_string()), "{v:?}");
        assert!(
            !v.contains(&"m.value".to_string()),
            "meta member has no value leaf: {v:?}"
        );
        // plain member: the member node itself (value-only).
        assert!(
            v.contains(&"p".to_string()),
            "plain member marks its node: {v:?}"
        );
        assert!(
            !v.iter().any(|leaf| leaf.starts_with("p.")),
            "plain member has no metadata sub-leaves: {v:?}"
        );

        // Property event on the meta source contributes nothing → Skip.
        assert!(
            matches!(
                GroupMonitor::property_event_mark(&def, &full_props(&def), src),
                EventMark::Skip
            ),
            "meta member carries no property-metadata leaves"
        );
    }

    /// R15-31 — a ROOT-flattened `+type:meta` member (the empty key, the
    /// only mapping pvxs permits at the struct top:
    /// `groupconfigprocessor.cpp:224-231`) marks by field path like every
    /// other member. pvxs has no special case for it — `Field::findIn`
    /// returns the root value unchanged for an empty `fieldName`
    /// (`field.cpp:56-81`) and `IOCSource::get` runs the same assignment
    /// (`iocsource.cpp:312-352`) — and its leaves land at the group root as
    /// `alarm` / `timeStamp` (`set_member_field`), both nameable.
    ///
    /// Two boundaries, both of which the old bail-to-full-mask broke:
    ///
    /// * a DBE_PROPERTY event on the root-meta member marks NOTHING (pvxs
    ///   `getProperties` is `info.type == Scalar`-only), so there is no post
    ///   at all — the old code emitted a full-value frame;
    /// * a root-meta member inside a `+trigger:"*"` group does not widen the
    ///   group's narrowing: the other members keep their per-UpdateType
    ///   leaves instead of every post collapsing to the whole request mask.
    #[test]
    fn root_meta_member_marks_leaves_like_every_other_member() {
        use crate::qsrv::group_config::parse_group_config;

        // "" is the root-flattened meta member; `a` is an ordinary scalar.
        let json = r#"{ "GRP": {
            "":  { "+channel": "R:m", "+type": "meta", "+trigger": "*" },
            "a": { "+channel": "R:a" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        let root = def
            .members
            .iter()
            .position(|m| m.field_name.is_empty())
            .expect("root meta member survives config parsing");

        // Value event: the root member's own leaves are the ROOT `timeStamp`
        // / `alarm` (no member prefix), and the `*` trigger still narrows the
        // other member to its Value|Alarm leaves.
        let EventMark::Marked(v) = GroupMonitor::value_event_mark(
            &def,
            &full_props(&def),
            root,
            DbeMask::VALUE | DbeMask::ALARM,
        ) else {
            panic!("a root-meta value event must mark leaves");
        };
        assert!(
            v.contains(&"timeStamp".to_string()) && v.contains(&"alarm".to_string()),
            "root meta marks the root alarm/timeStamp: {v:?}"
        );
        assert_eq!(
            v.iter().filter(|p| p.starts_with("a.")).collect::<Vec<_>>(),
            vec!["a.timeStamp", "a.alarm", "a.value"],
            "the co-triggered member keeps its leaf narrowing — a root-meta \
             member must not collapse the group onto the full request mask: {v:?}"
        );
        assert!(
            !v.iter().any(|p| p == "a.display.limitLow"),
            "a value event never marks property leaves: {v:?}"
        );

        // Property event on the root-meta member: getProperties is
        // Scalar-only, so nothing is assigned and pvxs posts nothing.
        assert!(
            matches!(
                GroupMonitor::property_event_mark(&def, &full_props(&def), root),
                EventMark::Skip
            ),
            "DBE_PROPERTY on a root-meta member marks nothing → no post"
        );
    }

    /// R15-34 — an ARRAY-SUBSCRIPT member (`a[0].x`) marks the enclosing
    /// `StructureArray` field, `a`. pvxs assigns into the array ELEMENT and
    /// `Value::mark` (`data.cpp:256-270`) walks the element's enclosing tops,
    /// so the one bit that lands in the parent store is the array field's own;
    /// `to_wire_valid` then serializes that whole field.
    ///
    /// The port marked `"a[0].x"`, and `marked_changed_bitset` builds its
    /// candidate paths from `FieldDesc` names — it never descends a
    /// `StructureArray` — so the path matched NOTHING: an empty wire bitset,
    /// which `MonitorQueue::real` drops. Every monitor update for an array
    /// member was silently discarded, and an all-array-member group delivered
    /// nothing at all after the seed.
    #[test]
    fn array_subscript_member_marks_the_enclosing_array_field() {
        use crate::qsrv::group_config::parse_group_config;
        use epics_pva_rs::proto::BitSet;
        use epics_pva_rs::pvdata::FieldDesc;
        use epics_pva_rs::pvdata::ScalarType;

        // All-array-member group (testqgroup `a[N].x` shape) plus a plain
        // member, so the mixed case is covered by the same event.
        let json = r#"{ "GRP": {
            "a[0].x": { "+channel": "R:r0.VAL", "+type": "plain", "+trigger": "*" },
            "a[1].x": { "+channel": "R:r1.VAL", "+type": "plain" },
            "p":      { "+channel": "R:p.VAL",  "+type": "plain" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        let src = def
            .members
            .iter()
            .position(|m| m.field_name == "a[0].x")
            .expect("subscripted member");

        let EventMark::Marked(v) = GroupMonitor::value_event_mark(
            &def,
            &full_props(&def),
            src,
            DbeMask::VALUE | DbeMask::ALARM,
        ) else {
            panic!("an array-member value event must mark leaves, not skip");
        };
        // Both subscripted members collapse onto the one array field; no
        // subscripted path survives into the marked set.
        assert!(
            !v.iter().any(|p| p.contains('[')),
            "a subscripted path addresses no bit in the root bitset: {v:?}"
        );
        assert_eq!(
            v.iter().filter(|p| p.as_str() == "a").count(),
            2,
            "each array member marks the enclosing array field `a`: {v:?}"
        );
        assert!(
            v.contains(&"p".to_string()),
            "the co-triggered plain member still marks its own node: {v:?}"
        );

        // The mark now frames a real bit: { a: StructureArray{x}, p: Double }
        // → bit 1 is `a`, bit 2 is `p`. A StructureArray occupies ONE bit
        // (`FieldDesc::total_bits`), exactly as pvxs's parent store does.
        let intro = FieldDesc::Structure {
            struct_id: "structure".into(),
            fields: vec![
                (
                    "a".into(),
                    FieldDesc::StructureArray {
                        struct_id: "structure".into(),
                        fields: vec![("x".into(), FieldDesc::Scalar(ScalarType::Double))],
                    },
                ),
                ("p".into(), FieldDesc::Scalar(ScalarType::Double)),
            ],
        };
        let mask = BitSet::all_set(intro.total_bits());
        let changed = epics_pva_rs::pvdata::encode::marked_wire_changed_bitset(&intro, &v, &mask);
        assert_eq!(
            changed.iter().collect::<Vec<usize>>(),
            vec![1, 2],
            "the array field's single bit is set (whole field serializes), \
             plus the plain member's — pre-fix this bitset was empty and the \
             post was dropped by the enqueue gate"
        );
    }

    /// the SELF-triggered member's leaves narrow to the
    /// event's own DBE classes, while every other triggered member keeps
    /// the fixed `Value | Alarm` refresh — pvxs `subscriptionValueCallback`
    /// uses `pDbFieldLog->mask & UpdateType::Everything` only for
    /// `isSelfTrig` (`groupsource.cpp:331-337`), then `IOCSource::get`
    /// assigns `value` iff `change & Value` and the alarm leaves iff
    /// `change & Alarm` (`iocsource.cpp:327-351`, `:183-251`).
    #[test]
    fn self_trigger_leaves_narrow_to_event_dbe_mask() {
        use crate::qsrv::group_config::parse_group_config;

        let json = r#"{ "GRP": {
            "a": { "+channel": "R:a", "+trigger": "*" },
            "b": { "+channel": "R:b" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        let src = def
            .members
            .iter()
            .position(|m| m.field_name == "a")
            .unwrap();

        // ALARM-only event: self re-sends alarm + timeStamp but NOT its
        // value; the triggered member keeps the full Value|Alarm refresh.
        let EventMark::Marked(v) =
            GroupMonitor::value_event_mark(&def, &full_props(&def), src, DbeMask::ALARM)
        else {
            panic!("expected Marked from an ALARM-only event");
        };
        assert!(v.contains(&"a.alarm".to_string()), "{v:?}");
        assert!(v.contains(&"a.timeStamp".to_string()), "{v:?}");
        assert!(
            !v.contains(&"a.value".to_string()),
            "ALARM-only event must not mark the self value: {v:?}"
        );
        assert!(v.contains(&"b.value".to_string()), "{v:?}");
        assert!(v.contains(&"b.alarm".to_string()), "{v:?}");

        // VALUE-only event: self marks value + timeStamp but NOT alarm.
        let EventMark::Marked(v) =
            GroupMonitor::value_event_mark(&def, &full_props(&def), src, DbeMask::VALUE)
        else {
            panic!("expected Marked from a VALUE-only event");
        };
        assert!(v.contains(&"a.value".to_string()), "{v:?}");
        assert!(v.contains(&"a.timeStamp".to_string()), "{v:?}");
        assert!(
            !v.contains(&"a.alarm".to_string()),
            "VALUE-only event must not mark the self alarm: {v:?}"
        );
    }

    /// an ARCHIVE-only (DBE_LOG) event contributes no
    /// self leaves — pvxs masks the field-log mask with
    /// `UpdateType::Everything` = `DBE_VALUE | DBE_ALARM | DBE_PROPERTY`
    /// (`iocsource.h:36-41`), which has no Archive bit. A `SelfOnly`
    /// trigger therefore marks nothing and the post is suppressed
    /// entirely (pvxs `subscriptionPost` `if(empty && !first) return`,
    /// `groupsource.cpp:268-275`); with other triggered targets the
    /// group still posts, carrying only those targets' leaves.
    #[test]
    fn archive_only_event_contributes_no_self_leaves() {
        use crate::qsrv::group_config::parse_group_config;

        // SelfOnly trigger in a mixed group: LOG-only → Skip.
        let json = r#"{ "GRP": {
            "a": { "+channel": "R:a", "+trigger": "a" },
            "b": { "+channel": "R:b", "+trigger": "*" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        let src = def
            .members
            .iter()
            .position(|m| m.field_name == "a")
            .unwrap();
        assert!(
            matches!(
                GroupMonitor::value_event_mark(&def, &full_props(&def), src, DbeMask::LOG),
                EventMark::Skip
            ),
            "an ARCHIVE-only self-trigger event must be suppressed"
        );

        // `*` trigger: the LOG-only source contributes nothing, the
        // other triggered member still refreshes with Value|Alarm.
        let src_b = def
            .members
            .iter()
            .position(|m| m.field_name == "b")
            .unwrap();
        let EventMark::Marked(v) =
            GroupMonitor::value_event_mark(&def, &full_props(&def), src_b, DbeMask::LOG)
        else {
            panic!("expected Marked: the non-self target still refreshes");
        };
        assert!(
            !v.iter().any(|p| p.starts_with("b.")),
            "the ARCHIVE-only source must contribute no self leaves: {v:?}"
        );
        assert!(v.contains(&"a.value".to_string()), "{v:?}");
        assert!(v.contains(&"a.alarm".to_string()), "{v:?}");
        assert!(v.contains(&"a.timeStamp".to_string()), "{v:?}");
    }

    /// a legacy unmasked post (empty DBE mask) carries no
    /// classification — the self-trigger falls back to the fixed
    /// `Value | Alarm` refresh, the same default pvxs uses when no
    /// `db_field_log` is available (`groupsource.cpp:331-337`,
    /// pre-7.0.6 builds).
    #[test]
    fn empty_event_mask_falls_back_to_value_alarm() {
        use crate::qsrv::group_config::parse_group_config;

        let json = r#"{ "GRP": {
            "a": { "+channel": "R:a", "+trigger": "a" },
            "b": { "+channel": "R:b", "+trigger": "*" }
        } }"#;
        let def = parse_group_config(json).unwrap().pop().unwrap();
        let src = def
            .members
            .iter()
            .position(|m| m.field_name == "a")
            .unwrap();
        let EventMark::Marked(v) =
            GroupMonitor::value_event_mark(&def, &full_props(&def), src, DbeMask::NONE)
        else {
            panic!("expected Marked from an unmasked event");
        };
        assert!(v.contains(&"a.value".to_string()), "{v:?}");
        assert!(v.contains(&"a.alarm".to_string()), "{v:?}");
        assert!(v.contains(&"a.timeStamp".to_string()), "{v:?}");
    }

    /// A value/alarm event on an
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

    // ---- Q1: field-name grammar mirrors the pvxs `FieldName` ctor throw set ----

    /// An empty leading or interior component is an error (pvxs `getline`
    /// yields an empty part → `throw "Empty field component"`,
    /// fieldname.cpp:35-36). A single trailing '.' is dropped at EOF and is
    /// NOT an error.
    #[test]
    fn parse_field_path_checked_empty_component() {
        assert!(parse_field_path_checked(".a").is_err(), "leading dot");
        assert!(
            parse_field_path_checked("a..b").is_err(),
            "interior double dot"
        );
        assert!(parse_field_path_checked(".").is_err(), "lone dot");
        assert!(
            parse_field_path_checked("a..").is_err(),
            "interior empty before trailing"
        );

        // Trailing dot: `getline` fails the zero-length final extraction at
        // EOF, so `a.` is just `a` — no error (fieldname.cpp getline loop).
        let comps = parse_field_path_checked("a.").expect("trailing dot is dropped, not an error");
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].name, "a");
    }

    /// A component ending in ']' must carry a '[' and a non-negative decimal
    /// subscript. NOTE: `value[]` and `value[-1]` are rejected here
    /// INTENTIONALLY stricter than pvxs — its `strtol` (fieldname.cpp:48-53)
    /// reads `value[]` as element 0 and accepts a negative/overflow index,
    /// failing only later at navigation. We reject at build (see the divergence
    /// note on `parse_field_path_checked`); this test pins the stricter Rust
    /// behavior, it is NOT asserting a pvxs match. A non-']'-terminated
    /// component is a literal name.
    #[test]
    fn parse_field_path_checked_bad_subscript() {
        assert!(
            parse_field_path_checked("value[x]").is_err(),
            "non-integer subscript"
        );
        assert!(
            parse_field_path_checked("value[]").is_err(),
            "empty subscript — stricter than pvxs, which reads it as element 0"
        );
        assert!(
            parse_field_path_checked("value[1x]").is_err(),
            "trailing garbage"
        );
        assert!(
            parse_field_path_checked("value[-1]").is_err(),
            "negative subscript — stricter than pvxs, which strtol-accepts then fails at nav"
        );

        // `value[` does not end with ']' → pvxs keeps it as a literal field
        // name (bracket included), no throw. The old parser renamed it to
        // `value`; the canonical grammar preserves the literal.
        let comps = parse_field_path_checked("value[").expect("no trailing ']' → literal name");
        assert_eq!(comps.len(), 1);
        assert_eq!(comps[0].name, "value[");
        assert_eq!(comps[0].index, None);
    }

    /// Empty path → no components, no error (pvxs skips the split for an empty
    /// `fieldName`; the empty-name policy is enforced separately).
    #[test]
    fn parse_field_path_checked_empty_ok() {
        assert_eq!(parse_field_path_checked("").unwrap(), Vec::new());
    }

    // ---- BUG 4: atomic-group PUT serialization ----

    /// A competing owner of `atomic_write_lock` (L33), holding it on a
    /// dedicated thread until this handle is dropped.
    ///
    /// L33 is a blocking lock, so a test's "external holder" cannot be the
    /// task that also drives the assertions: it would own the lock on the very
    /// thread the runtime needs in order to poll the PUT that must block on
    /// it, and "the PUT did not finish" would then be true because nothing
    /// polled it rather than because the lock excluded it.
    struct ExternalLockHolder {
        release: Option<std::sync::mpsc::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl ExternalLockHolder {
        /// Returns once the lock is genuinely held — no sleep-and-hope.
        fn take(
            lock: Arc<epics_base_rs::runtime::sync::PriorityInheritanceMutex<()>>,
        ) -> ExternalLockHolder {
            let (release, release_rx) = std::sync::mpsc::channel();
            let (held, held_rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                let _guard = lock.lock();
                held.send(()).expect("holder announces the lock");
                let _ = release_rx.recv();
            });
            held_rx
                .recv()
                .expect("the external holder must own the lock");
            ExternalLockHolder {
                release: Some(release),
                thread: Some(thread),
            }
        }
    }

    impl Drop for ExternalLockHolder {
        fn drop(&mut self) {
            self.release.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

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
    /// 547-574); a no-`+putorder` proc keeps the sentinel order
    /// (fieldconfig.h:37) and is still processed. Before the fix the
    /// PUT-candidate `filter_map(put_order)` dropped it, so a proc-only
    /// save/apply hook silently never ran. Observable through `has_processed`:
    /// a freshly added AiRecord is still in its INIT phase, and its first
    /// process leaves it.
    #[tokio::test]
    async fn proc_member_without_putorder_is_processed_atomic_and_nonatomic() {
        use epics_base_rs::server::records::ai::AiRecord;

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

            assert!(
                !has_processed(&db, "HOOK:rec").await,
                "fresh record not processed"
            );

            // An empty PUT still fires the proc hook (proc runs regardless
            // of which value fields the client supplied).
            channel
                .put(&PvStructure::new("structure"))
                .await
                .expect("proc-only group PUT must succeed");

            assert!(
                has_processed(&db, "HOOK:rec").await,
                "proc member without +putorder must process its record (atomic={atomic})"
            );
        }
    }

    /// R19-43: a group PUT that decides to process routes the transition
    /// through the database's `put_driven_process` owner, so it splits on PACT
    /// exactly as pvxs `doPostProcessing` does (`iocsource.cpp:404-419`):
    ///
    /// * **record ACTIVE** — `rpro = TRUE`, and `dbProcess` is NOT called.
    ///   `recGblFwdLink` re-queues the record when the device round trip lands,
    ///   so the put still reaches the device one cycle later. The port used to
    ///   call `dbProcess` here, landing in its own PACT guard: LCNT bumped, and
    ///   after MAX_LOCK the record takes a SCAN_ALARM/INVALID that C never
    ///   raises for a client put — while the deferred reprocess was lost.
    /// * **record IDLE** — `putf = TRUE` and the record processes.
    ///
    /// Both gate modes (atomic ⇒ the caller owns the record gates; non-atomic ⇒
    /// the owner takes them) must reach the same owner.
    #[tokio::test]
    async fn group_put_defers_to_rpro_on_an_active_record() {
        use epics_base_rs::server::records::ai::AiRecord;

        for atomic in [false, true] {
            let db = Arc::new(PvDatabase::new());
            db.add_record("PACT:rec", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            let cfg = format!(
                r#"{{ "PACT:GRP": {{ "+atomic": {atomic},
                    "go": {{ "+type": "proc", "+channel": "PACT:rec" }} }} }}"#
            );
            let mut defs = super::super::group_config::parse_group_config(&cfg).unwrap();
            let channel = GroupChannel::new(db.clone(), defs.pop().unwrap());

            // ACTIVE (PACT=1) — the async-device boundary. Driven through the
            // PACT owner's API, not by poking the flag: `processing` is private
            // precisely so no site outside the owner can open or close the
            // window (the wave-16 regression that stranded a parked put-notify).
            {
                let rec = db.get_record("PACT:rec").unwrap();
                let inst = rec.write();
                inst.enter_pact();
            }
            channel
                .put(&PvStructure::new("structure"))
                .await
                .expect("a group PUT onto an active record is not an error");
            {
                let rec = db.get_record("PACT:rec").unwrap();
                let inst = rec.read();
                assert!(
                    inst.common.rpro != 0,
                    "an active record takes the RPRO deferral (atomic={atomic})"
                );
                assert!(
                    !inst.common.putf,
                    "PUTF marks a cycle that actually ran (atomic={atomic})"
                );
                assert_eq!(
                    inst.common.lcnt, 0,
                    "the deferral must not re-enter dbProcess's PACT guard \
                     — an LCNT bump is the SCAN_ALARM path C avoids (atomic={atomic})"
                );
            }

            // IDLE — the other side of the same boundary.
            {
                let rec = db.get_record("PACT:rec").unwrap();
                let mut inst = rec.write();
                // The release carries any put-notify parked on the window; this
                // test parks none (a group PUT, not a put-callback), so there is
                // nothing for the token to hand back.
                let _ = inst.leave_pact();
                inst.common.rpro = 0;
            }
            channel
                .put(&PvStructure::new("structure"))
                .await
                .expect("group PUT on an idle record processes");
            {
                let rec = db.get_record("PACT:rec").unwrap();
                let inst = rec.read();
                assert!(
                    inst.common.rpro == 0,
                    "an idle record processes now, it does not defer (atomic={atomic})"
                );
            }
        }
    }

    /// R18-30: a `+type:"proc"` member does not process its record
    /// unconditionally — it goes through `IOCSource::doPostProcessing`
    /// (`iocsource.cpp:397-403`), which processes only when the bound field is
    /// the record's `PROC`, when the client forced processing, or when the
    /// field is `pp(TRUE)` on a `SCAN=Passive` record.
    ///
    /// Here the backing record is `SCAN=1 second`, so the `pp && Passive` term
    /// is false: a proc member bound to `VAL` must NOT process it, while one
    /// bound to `PROC` must (the first term of C's disjunction ignores both
    /// SCAN and forceProcessing). Pre-fix both processed, on every group PUT.
    #[tokio::test]
    async fn r18_30_proc_member_honors_the_dopostprocessing_gate() {
        use epics_base_rs::server::record::ScanType;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;

        // (bound field, must the record process?)
        for (channel_field, expect_processed) in [("VAL", false), ("PROC", true)] {
            for atomic in [false, true] {
                let db = Arc::new(PvDatabase::new());
                db.add_record("SCANNED:rec", Box::new(AiRecord::new(0.0)))
                    .await
                    .unwrap();
                // Not Passive: the `pp(TRUE) && SCAN==Passive` term is false.
                db.put_pv(
                    "SCANNED:rec.SCAN",
                    EpicsValue::Enum(ScanType::SEC1.to_u16()),
                )
                .await
                .unwrap();

                let cfg = format!(
                    r#"{{ "PROCGATE:GRP": {{ "+atomic": {atomic},
                        "go": {{ "+type": "proc",
                                 "+channel": "SCANNED:rec.{channel_field}" }} }} }}"#
                );
                let mut defs = super::super::group_config::parse_group_config(&cfg).unwrap();
                let def = defs.pop().unwrap();
                let channel = GroupChannel::new(db.clone(), def);

                channel
                    .put(&PvStructure::new("structure"))
                    .await
                    .expect("proc-only group PUT must succeed");

                let processed = has_processed(&db, "SCANNED:rec").await;
                assert_eq!(
                    processed, expect_processed,
                    "+proc member bound to {channel_field} on a SCAN=1s record \
                     (atomic={atomic}): processed={processed}, expected \
                     {expect_processed} (iocsource.cpp:397-403)"
                );
            }
        }
    }

    /// R18-30, the force term: `record._options.process=true` processes the
    /// backing record whatever its SCAN and whatever field the `+proc` member
    /// binds (`forceProcessing == True`, iocsource.cpp:399). `process=false`
    /// (`Inhibit`) suppresses it — except for a PROC-bound member, whose term
    /// in C's disjunction never consults `forceProcessing`.
    #[tokio::test]
    async fn r18_30_proc_member_force_and_inhibit_terms() {
        use super::super::channel::{ProcessMode, PutOptions};
        use epics_base_rs::server::record::ScanType;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;

        // (bound field, process mode, must the record process?)
        let cases = [
            ("VAL", ProcessMode::Force, true),
            ("VAL", ProcessMode::Inhibit, false),
            ("PROC", ProcessMode::Inhibit, true),
        ];
        for (channel_field, process, expect_processed) in cases {
            let db = Arc::new(PvDatabase::new());
            db.add_record("FORCED:rec", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();
            db.put_pv("FORCED:rec.SCAN", EpicsValue::Enum(ScanType::SEC1.to_u16()))
                .await
                .unwrap();

            let cfg = format!(
                r#"{{ "FORCE:GRP": {{
                    "go": {{ "+type": "proc",
                             "+channel": "FORCED:rec.{channel_field}" }} }} }}"#
            );
            let mut defs = super::super::group_config::parse_group_config(&cfg).unwrap();
            let def = defs.pop().unwrap();
            let channel = GroupChannel::new(db.clone(), def);

            channel
                .put_with_options(
                    &PvStructure::new("structure"),
                    PutOptions {
                        process,
                        block: false,
                    },
                    None,
                    &RemoteLog::default(),
                )
                .await
                .expect("proc-only group PUT must succeed");

            let processed = has_processed(&db, "FORCED:rec").await;
            assert_eq!(
                processed, expect_processed,
                "+proc member bound to {channel_field} under {process:?}: \
                 processed={processed}, expected {expect_processed}"
            );
        }
    }

    /// R17-37: a MARKED Meta member with an explicit `+putorder` is `changing`
    /// in pvxs (`changing = marked && putable`, groupsource.cpp:557) — so even
    /// though `IOCSource::put` writes nothing for a Meta mapping
    /// (iocsource.cpp:579-582, "can't write"), `doPostProcessing` runs and the
    /// backing record is processed (groupsource.cpp:568-571).
    ///
    /// The port fused "has no writable leaf" with "does not participate" and
    /// skipped the member outright, so the record never processed and — with no
    /// other member marked — the PUT failed "No fields changed". Both are
    /// asserted here: the record processes, and its VAL is untouched (this is a
    /// post-process, not a write).
    #[tokio::test]
    async fn r17_37_marked_meta_member_with_putorder_post_processes_its_record() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::pvdata::ScalarValue;

        for atomic in [false, true] {
            let db = Arc::new(PvDatabase::new());
            db.add_record("META:rec", Box::new(AiRecord::new(0.0)))
                .await
                .unwrap();

            let cfg = format!(
                r#"{{ "META:GRP": {{ "+atomic": {atomic},
                    "m": {{ "+type": "meta", "+channel": "META:rec.VAL",
                            "+putorder": 0 }} }} }}"#
            );
            let mut defs = super::super::group_config::parse_group_config(&cfg).unwrap();
            let def = defs.pop().unwrap();
            let channel = GroupChannel::new(db.clone(), def);

            // The client marks the meta member (alarm/timeStamp leaves — a Meta
            // mapping carries no value leaf).
            let mut root = PvStructure::new("structure");
            let mut meta = PvStructure::new("structure");
            meta.set("severity", PvField::Scalar(ScalarValue::Int(0)));
            root.set("m", PvField::Structure(meta));

            channel
                .put(&root)
                .await
                .expect("a marked, putable meta member participates: PUT must not fail");

            assert!(
                has_processed(&db, "META:rec").await,
                "a changing meta member must post-process its record \
                 (groupsource.cpp:568, atomic={atomic})"
            );
            let val = match db.get_pv("META:rec.VAL").unwrap() {
                EpicsValue::Double(d) => d,
                other => panic!("unexpected VAL type: {other:?}"),
            };
            assert_eq!(
                val, 0.0,
                "IOCSource::put writes nothing for a Meta mapping \
                 (iocsource.cpp:579-582, atomic={atomic})"
            );
        }
    }

    /// R17-37, the other predicate: a marked Meta member WITHOUT `+putorder` is
    /// not putable, so it is not `changing` and nothing happens — no write, no
    /// post-processing. With no other member participating, the PUT then fails
    /// "No fields changed" (groupsource.cpp:656-659), exactly as in pvxs.
    #[tokio::test]
    async fn r17_37_marked_meta_member_without_putorder_does_nothing() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record("META2:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let cfg = r#"{ "META2:GRP": {
            "m": { "+type": "meta", "+channel": "META2:rec.VAL" }
        } }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let channel = GroupChannel::new(db.clone(), def);

        let mut root = PvStructure::new("structure");
        let mut meta = PvStructure::new("structure");
        meta.set("severity", PvField::Scalar(ScalarValue::Int(0)));
        root.set("m", PvField::Structure(meta));

        let err = channel
            .put(&root)
            .await
            .expect_err("a marked but not-putable member changes nothing");
        assert!(
            err.to_string().contains("No fields changed"),
            "expected pvxs's No-fields-changed reply, got: {err}"
        );

        assert!(
            !has_processed(&db, "META2:rec").await,
            "a member without +putorder is not putable, so it never post-processes"
        );
    }

    /// A group member
    /// mapped under `record.*` (e.g. `record.status`) must survive
    /// alongside the built-in `record._options` branch in both the
    /// composed value (GET/MONITOR share `read_group`) and the
    /// introspected descriptor (GET_FIELD). pvxs adds the built-in
    /// `record._options` to the same member vector and `TypeDef::_append()`
    /// recursively merges matching compound children
    /// (groupconfigprocessor.cpp:499-524, type.cpp:374-389). The prior
    /// Rust path replaced the whole `record` field with one containing
    /// only `_options`, dropping the user member from value, descriptor,
    /// and (via the descriptor) PUT.
    #[tokio::test]
    async fn group_record_member_survives_record_options_merge() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("RM:rec", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();
        // A normal member plus one mapped under `record.status`.
        let cfg = r#"{ "RM:GRP": {
            "v": {"+type": "plain", "+channel": "RM:rec.VAL"},
            "record.status": {"+type": "plain", "+channel": "RM:rec.VAL"}
        } }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let channel = GroupChannel::new(db.clone(), def);

        // value path (GET / MONITOR share read_group): the user
        // `record.status` member and the built-in `record._options`
        // both survive under one `record` branch.
        let value = channel.read_group().await.expect("group read_group");
        let Some(PvField::Structure(record)) = value.get_field("record") else {
            panic!("record branch must be a structure: {value:?}");
        };
        assert!(
            record.get_field("status").is_some(),
            "user record.status member must survive the options merge: {record:?}"
        );
        assert!(
            matches!(record.get_field("_options"), Some(PvField::Structure(_))),
            "built-in record._options must be present: {record:?}"
        );

        // descriptor path (GET_FIELD): the same invariant for the
        // advertised type, so PUT clients can address record.status.
        let desc = channel.get_field().await.expect("group get_field");
        let FieldDesc::Structure { fields, .. } = &desc else {
            panic!("group descriptor must be a structure");
        };
        let record_desc = &fields
            .iter()
            .find(|(n, _)| n == "record")
            .expect("record descriptor present")
            .1;
        let FieldDesc::Structure {
            fields: rfields, ..
        } = record_desc
        else {
            panic!("record descriptor must be a structure");
        };
        assert!(
            rfields.iter().any(|(n, _)| n == "status"),
            "record.status descriptor must survive the options merge: {rfields:?}"
        );
        assert!(
            rfields.iter().any(|(n, _)| n == "_options"),
            "built-in record._options descriptor must be present: {rfields:?}"
        );
    }

    /// The two boundaries of the link refusal. pvxs's link test at
    /// groupsource.cpp:603-604 is dead code — `dbChannelFinalFieldType` is
    /// `DBR_CHAR` for every link field (ioc/channel.cpp:69-74 →
    /// dbChannel.c:579, :621) — so pvxs writes link members and refuses
    /// nothing. The port refuses a link member it is asked to WRITE, and
    /// only that member: a PUT marking an unrelated scalar must go through
    /// even though the group also binds `FLNK`, which is what the port used
    /// to fail.
    #[tokio::test]
    async fn group_put_refuses_a_marked_link_member_and_only_that_member() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record("LNK:rec", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();

        // A scalar value member plus a forward-link member. `FLNK` is a
        // dbCommon link field, so it resolves to a link class for every
        // record type via the canonical `dbf_link_class` classifier.
        let cfg = r#"{ "LNK:GRP": {
            "v":   {"+type": "plain", "+channel": "LNK:rec.VAL", "+putorder": 0},
            "fwd": {"+type": "plain", "+channel": "LNK:rec.FLNK", "+putorder": 1}
        } }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let channel = GroupChannel::new(db.clone(), def);

        // Partial PUT marking ONLY the scalar value member; the link member
        // is left unmarked, so nothing writes it and it must not veto.
        let mut value = PvStructure::new("structure");
        value.set("v", PvField::Scalar(ScalarValue::Double(2.0)));
        channel
            .put(&value)
            .await
            .expect("an untouched link member must not fail a scalar PUT");

        // Marking the link member is the case that still refuses.
        let mut marked = PvStructure::new("structure");
        marked.set(
            "fwd",
            PvField::Scalar(ScalarValue::String("OTHER:REC".into())),
        );
        let res = channel.put(&marked).await;
        assert!(
            matches!(res, Err(BridgeError::PutRejected(_))),
            "writing a link member must be refused: {res:?}"
        );

        // Control: a group with only the scalar member accepts the same
        // partial PUT, proving the acceptance above is not accidental.
        let cfg_ok = r#"{ "OK:GRP": {
            "v": {"+type": "plain", "+channel": "LNK:rec.VAL", "+putorder": 0}
        } }"#;
        let mut defs_ok = super::super::group_config::parse_group_config(cfg_ok).unwrap();
        let def_ok = defs_ok.pop().unwrap();
        let channel_ok = GroupChannel::new(db.clone(), def_ok);
        let mut value_ok = PvStructure::new("structure");
        value_ok.set("v", PvField::Scalar(ScalarValue::Double(3.0)));
        channel_ok
            .put(&value_ok)
            .await
            .expect("a scalar-only group PUT must succeed");
    }

    /// Every group PUT rejection must put pvxs's bare contract text on the
    /// wire: `"Links not supported for put"` (groupsource.cpp:605),
    /// `"No fields changed"` (:658), `"Put not permitted"`
    /// (iocsource.cpp:385) and `"Unable to put value: …"` (:366-368). pvxs
    /// throws those strings and forwards `e.what()` verbatim; it never names
    /// the group, the member, the user/host — and never cites its own source
    /// files. Pre-fix the port emitted `group X PUT: member 'm' targets link
    /// field 'R.FLNK' (pvxs groupsource.cpp:603-606 …)` and friends, plus a
    /// `"put rejected: "` Display prefix, all of which reached the client.
    #[tokio::test]
    async fn group_put_rejection_messages_are_pvxs_contract_text() {
        use super::super::provider::{AccessContext, AccessControl};
        use super::super::put_status::wire_message;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        struct DenyChannel(&'static str);
        #[async_trait::async_trait]
        impl AccessControl for DenyChannel {
            async fn can_write(&self, channel: &str, _: &str, _: &str) -> bool {
                channel != self.0
            }
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record("MSG:rec", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();

        let group_of = |cfg: &str| {
            let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
            defs.pop().unwrap()
        };

        // "Links not supported for put"
        let link = GroupChannel::new(
            db.clone(),
            group_of(
                r#"{ "MSG:LNK": {
                    "v":   {"+type": "plain", "+channel": "MSG:rec.VAL", "+putorder": 0},
                    "fwd": {"+type": "plain", "+channel": "MSG:rec.FLNK", "+putorder": 1}
                } }"#,
            ),
        );
        let mut v = PvStructure::new("structure");
        v.set("v", PvField::Scalar(ScalarValue::Double(2.0)));
        // The refusal follows the member being written, so the link member
        // has to be the marked one.
        let mut link_marked = PvStructure::new("structure");
        link_marked.set(
            "fwd",
            PvField::Scalar(ScalarValue::String("OTHER:REC".into())),
        );
        let err = link
            .put(&link_marked)
            .await
            .expect_err("link member must reject");
        assert_eq!(wire_message(&err), "Links not supported for put");

        // "No fields changed" — a member without `+putorder` is not putable,
        // so a PUT that supplies only it writes nothing.
        let nochange = GroupChannel::new(
            db.clone(),
            group_of(
                r#"{ "MSG:NOP": {
                    "v": {"+type": "plain", "+channel": "MSG:rec.VAL"}
                } }"#,
            ),
        );
        let err = nochange
            .put(&v)
            .await
            .expect_err("a marked but unputable member must reject");
        assert_eq!(wire_message(&err), "No fields changed");

        let putable = r#"{ "MSG:GRP": {
            "v": {"+type": "plain", "+channel": "MSG:rec.VAL", "+putorder": 0}
        } }"#;

        // "Put not permitted" — per-member ACF denial (the group PV itself is
        // writable, the member's backing channel is not).
        let member_denied = GroupChannel::new(db.clone(), group_of(putable)).with_access(
            AccessContext::with_identity(
                Arc::new(DenyChannel("MSG:rec.VAL")),
                "alice".into(),
                "host1".into(),
            ),
        );
        let err = member_denied
            .put(&v)
            .await
            .expect_err("per-member ACF must reject");
        assert_eq!(wire_message(&err), "Put not permitted");

        // "Put not permitted" — group-level ACF denial.
        let group_denied = GroupChannel::new(db.clone(), group_of(putable)).with_access(
            AccessContext::with_identity(
                Arc::new(DenyChannel("MSG:GRP")),
                "alice".into(),
                "host1".into(),
            ),
        );
        let err = group_denied
            .put(&v)
            .await
            .expect_err("group-level ACF must reject");
        assert_eq!(wire_message(&err), "Put not permitted");

        // "Unable to put value: Field Disabled: S_db_putDisabled" — the
        // member's backing record is DISP-disabled (doPreProcessing).
        db.get_record("MSG:rec").unwrap().write().common.disp = 1;
        let disabled = GroupChannel::new(db.clone(), group_of(putable));
        let err = disabled
            .put(&v)
            .await
            .expect_err("DISP=1 member must reject");
        assert_eq!(
            wire_message(&err),
            "Unable to put value: Field Disabled: S_db_putDisabled"
        );
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

    #[tokio::test]
    async fn group_plain_array_member_advertises_scalar_array_descriptor() {
        // BR-1 regression. A `+type:"plain"` member bound to a waveform
        // VAL (an array field) must advertise a scalar-ARRAY leaf, not a
        // bare scalar: pvxs `getChannelValueType` returns `arrayOf()` for
        // `dbChannelFinalElements != 1` (iocsource.cpp:632-643), and the
        // read path serves a `double[]`. A scalar descriptor would
        // disagree with the length-prefixed array on the wire.
        use epics_base_rs::server::records::waveform::WaveformRecord;

        // Pull a Plain member's bare leaf descriptor directly from the
        // group structure (Plain members carry no NT wrapper).
        fn member_leaf<'a>(group: &'a FieldDesc, name: &str) -> &'a FieldDesc {
            let FieldDesc::Structure { fields, .. } = group else {
                panic!("group descriptor must be a structure");
            };
            &fields
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("member '{name}' missing from group descriptor"))
                .1
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "BR1:wf",
            Box::new(WaveformRecord::new(8, DbFieldType::Double)),
        )
        .await
        .unwrap();

        let cfg = r#"{
            "BR1:GRP": {
                "w": {"+type": "plain", "+channel": "BR1:wf.VAL"}
            }
        }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();
        let channel = GroupChannel::new(db.clone(), def);

        // ---- descriptor (introspect_member / build_introspection) ----
        let desc = channel.get_field().await.expect("get_field");
        assert_eq!(
            member_leaf(&desc, "w"),
            &FieldDesc::ScalarArray(ScalarType::Double),
            "plain waveform member must advertise a scalar-ARRAY leaf, not a scalar"
        );

        // ---- runtime value (decode_member) agrees with the descriptor ----
        let val = channel
            .get(&PvStructure::new("structure"))
            .await
            .expect("group GET");
        assert!(
            matches!(val.get_field("w"), Some(PvField::ScalarArray(_))),
            "plain waveform member GET value must be a scalar array, got {:?}",
            val.get_field("w")
        );
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
    /// `groupsource.cpp:429-431`), while non-meta members keep
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
        let plain = db.get_record("R:plain").unwrap();
        {
            let plain_inst = plain.read();
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
        }

        // FR-7: meta member value subscription is ALARM-only; the
        // PROPERTY subscription is retained on the same field.
        let meta = db.get_record("R:meta").unwrap();
        let meta_inst = meta.read();
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
    /// read_checked() at MONITOR INIT, so a forwarded snapshot would
    /// be a duplicate DATA frame for a value that never changes.
    ///
    /// poll() must yield NO snapshot — but it must do so by *parking*, not
    /// by resolving to `None`. The earlier "returns None as soon as the
    /// empty event channel closes" shape made the native server read
    /// source-close and send a premature MONITOR FINISH; pvxs keeps an
    /// all-const subscription open until the client cancels
    /// (`groupsource.cpp:241-298`). The keepalive sender in
    /// [`GroupMonitor::start`] now pins the channel open so poll() parks:
    /// the client sees exactly one DATA frame and the stream stays open.
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

        // poll() must NOT manufacture an initial snapshot, and must NOT
        // resolve to None (which the native server turns into a premature
        // FINISH). With the keepalive sender holding the empty event channel
        // open it parks indefinitely, so a bounded poll() must TIME OUT.
        let polled = tokio::time::timeout(Duration::from_millis(200), mon.poll()).await;
        assert!(
            polled.is_err(),
            "all-const group monitor must park (no snapshot, no FINISH), got {polled:?}"
        );
    }

    /// Regression (Q39): a per-event `read_group()` failure — a member
    /// record removed mid-stream, a value-conversion error, or an ACL
    /// revocation on one member — must drop that single update and leave
    /// the subscription open. pvxs wraps each group value/property refresh
    /// in a try/catch that logs and returns from the callback WITHOUT
    /// posting (`groupsource.cpp:350-352`). Ending the update stream
    /// instead reads as source-close in the forward task and tears the
    /// whole group monitor down with a spurious MONITOR FINISH.
    ///
    /// Two members so the failure is a REAL drain-side one: member `b`'s
    /// record is removed (its subscription ends; the drain drops just that
    /// member stream), then a genuine post on member `a` makes the drain
    /// assemble the group — `read_group()` fails on the missing `b`, the
    /// drain skips the update, and the subscription stays open.
    #[tokio::test]
    async fn q39_group_monitor_member_read_error_skips_event_keeps_open() {
        use crate::qsrv::provider::PvaMonitor;
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("Q39:a", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("Q39:b", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "Q39:GRP": {
                "va": {"+type": "plain", "+channel": "Q39:a.VAL"},
                "vb": {"+type": "plain", "+channel": "Q39:b.VAL"}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let mut mon = GroupMonitor::new(db.clone(), def);
        mon.start().await.expect("group monitor starts");

        // Make the group unreadable: `read_group()` now fails with
        // `RecordNotFound` on `b`.
        assert!(db.remove_record("Q39:b").await, "member record removed");

        // A real post on the surviving member reaches the drain.
        {
            let rec = db.get_record("Q39:a").expect("member a exists");
            rec.write().notify_field(
                "VAL",
                epics_base_rs::server::recgbl::EventMask::VALUE
                    | epics_base_rs::server::recgbl::EventMask::ALARM,
            );
        }

        // The drain must consume the event, hit the read failure, log+skip,
        // and poll() must PARK on the still-open update queue — never
        // return `None` (FINISH). A bounded poll therefore TIMES OUT;
        // pre-fix it returned `None` (Some(None)) immediately.
        let polled = tokio::time::timeout(Duration::from_millis(200), mon.poll()).await;
        assert!(
            polled.is_err(),
            "a member read error must skip the event and keep the \
             subscription open (park), not FINISH — got {polled:?}"
        );
    }

    /// Regression: a group MONITOR's initial seed frame must carry the
    /// same `record._options` stamping (atomic = true, negotiated
    /// queueSize) as every subsequent update. pvxs delivers the first
    /// group post through the monitor-stamped `currentValue`
    /// (`groupsource.cpp:401-405`), not the GET path. The seed used to be
    /// read from the GET path (`Channel::get` → GET-stamped `read_group`),
    /// which stamps the *operation* atomicity and `queueSize = 0`
    /// (`groupsource.cpp:480-485`), so the first frame disagreed with the
    /// stream the client then received. `AnyMonitor::seed` reads through the
    /// monitor's own `group_channel`, so the seed and the deltas share one
    /// stamping by construction.
    #[tokio::test]
    async fn group_monitor_seed_carries_monitor_options_not_get_options() {
        use crate::qsrv::provider::PvaMonitor;
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        fn read_options(pv: &PvStructure) -> (bool, i32) {
            let Some(PvField::Structure(record)) = pv.get_field("record") else {
                panic!("record branch must be a structure: {pv:?}");
            };
            let Some(PvField::Structure(options)) = record.get_field("_options") else {
                panic!("record._options must be a structure: {record:?}");
            };
            let atomic = match options.get_field("atomic") {
                Some(PvField::Scalar(ScalarValue::Boolean(b))) => *b,
                other => panic!("record._options.atomic must be boolean: {other:?}"),
            };
            let queue = match options.get_field("queueSize") {
                Some(PvField::Scalar(ScalarValue::Int(n))) => *n,
                other => panic!("record._options.queueSize must be int: {other:?}"),
            };
            (atomic, queue)
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record("SEED:rec", Box::new(AiRecord::new(2.5)))
            .await
            .unwrap();
        // An explicitly non-atomic group with one backing member, so the
        // GET path would stamp atomic = false / queueSize = 0 — the values
        // the monitor seed must NOT inherit.
        let cfg = r#"{
            "SEED:GRP": {
                "+atomic": false,
                "v": {"+type": "plain", "+channel": "SEED:rec.VAL"}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        assert!(!def.atomic, "fixture group must be non-atomic");

        // MONITOR seed: built with a negotiated queue depth of 32 and the
        // monitor stamp, so the initial frame must report atomic = true and
        // queueSize = 32 — matching the update stream poll() drains from the
        // same group_channel.
        let mut mon = AnyMonitor::Group(Box::new(GroupMonitor::new(db.clone(), def.clone())))
            .with_queue_size(32);
        mon.start().await.expect("group monitor starts");
        let seed = mon
            .seed()
            .await
            .expect("group monitor must expose a stamped seed");
        let PvField::Structure(seed) = seed else {
            panic!("group seed must be a structure: {seed:?}");
        };
        let (seed_atomic, seed_queue) = read_options(&seed);
        assert!(
            seed_atomic,
            "group monitor seed must stamp atomic = true (monitor path)"
        );
        assert_eq!(
            seed_queue, 32,
            "group monitor seed must stamp the negotiated queueSize, got {seed_queue}"
        );

        // GET path (Channel::get / read_group, no monitor stamp): the same
        // group reports the operation atomicity (false here) and
        // queueSize = 0, confirming the two paths legitimately differ and the
        // seed no longer borrows the GET stamping.
        let get_channel = GroupChannel::new(db.clone(), def);
        let get_value = get_channel.read_group().await.expect("group GET read");
        let (get_atomic, get_queue) = read_options(&get_value);
        assert!(
            !get_atomic,
            "non-atomic group GET must stamp atomic = false"
        );
        assert_eq!(get_queue, 0, "group GET must stamp queueSize = 0");
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bug4_atomic_put_serializes_on_group_lock() {
        let (db, def) = atomic_group_fixture().await;
        let channel = GroupChannel::new(db.clone(), def.clone());

        // Hold the group's atomic_write_lock — exactly the guard the
        // atomic PUT branch acquires. While held, a `put` on the same
        // group def must not be able to enter the member-write loop.
        let guard = ExternalLockHolder::take(def.atomic_write_lock.clone());

        let put_fut = tokio::spawn(async move {
            channel.put(&atomic_put_value(11.0, 22.0)).await.unwrap();
        });

        // The PUT must still be blocked on the lock. A real sleep, not a
        // `timeout` around a ready future: the latter never yields, so the
        // spawned PUT would not have been polled at all and the assertion
        // below would hold for the wrong reason.
        tokio::time::sleep(Duration::from_millis(150)).await;
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
        let a = db.get_pv("A:rec.VAL").unwrap();
        let b = db.get_pv("B:rec.VAL").unwrap();
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

    /// H9 boundary: an unconvertible member value must abort the atomic
    /// PUT in the up-front conversion phase, BEFORE `lock_records` (L1)
    /// is ever requested (the hoist this restructure performs).
    ///
    /// Proven by pre-holding, on THIS task, the exact member-record gate
    /// set the atomic PUT would need. If the conversion-failure path ran
    /// AFTER acquiring `lock_records`, `channel.put` would try to
    /// re-acquire a gate this same (un-spawned) task already holds —
    /// which can never be released — and the surrounding timeout would
    /// fire. It returns promptly instead, proving the abort happens
    /// strictly before `lock_records` is reached.
    #[tokio::test]
    async fn h9_atomic_put_conversion_failure_aborts_before_gate() {
        use epics_pva_rs::pvdata::ScalarValue;

        let (db, def) = atomic_group_fixture().await;
        let channel = GroupChannel::new(db.clone(), def.clone());

        let _held = db.lock_records(["A:rec", "B:rec"]);

        // "a" -> a non-numeric string. `A:rec.VAL` is DBF_DOUBLE
        // (`AiRecord`), so `convert_member_value` must reject it.
        let mut value = PvStructure::new("structure");
        value.fields.push((
            "a".into(),
            PvField::Scalar(ScalarValue::String("not-a-number".into())),
        ));
        value
            .fields
            .push(("b".into(), PvField::Scalar(ScalarValue::Double(22.0))));

        let result = tokio::time::timeout(Duration::from_secs(2), channel.put(&value))
            .await
            .expect(
                "atomic PUT with an unconvertible member must abort in the \
                 up-front conversion phase, not block trying to (re-)acquire \
                 the member-record gate this test already holds",
            );

        assert!(
            result.is_err(),
            "an unconvertible member value must reject the whole atomic PUT"
        );

        // Neither member was touched — the write-loop phase never ran.
        match (
            db.get_pv("A:rec.VAL").unwrap(),
            db.get_pv("B:rec.VAL").unwrap(),
        ) {
            (
                epics_base_rs::types::EpicsValue::Double(va),
                epics_base_rs::types::EpicsValue::Double(vb),
            ) => {
                assert_eq!(
                    va, 0.0,
                    "conversion failure must abort before any member write"
                );
                assert_eq!(
                    vb, 0.0,
                    "conversion failure must abort before any member write"
                );
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
    /// 518-523). The prior Rust-only `"structure"` literal changed the
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn bug4_concurrent_atomic_puts_do_not_interleave() {
        let (db, def) = atomic_group_fixture().await;

        let ch1 = GroupChannel::new(db.clone(), def.clone());
        let ch2 = GroupChannel::new(db.clone(), def.clone());

        // Pre-acquire the lock so PUT #1 blocks deterministically;
        // start both PUTs, then release. They must run strictly
        // serially through the shared lock.
        let guard = ExternalLockHolder::take(def.atomic_write_lock.clone());
        let p1 = tokio::spawn(async move {
            ch1.put(&atomic_put_value(1.0, 1.0)).await.unwrap();
        });
        let p2 = tokio::spawn(async move {
            ch2.put(&atomic_put_value(2.0, 2.0)).await.unwrap();
        });
        // Neither PUT can proceed while the lock is held externally. A real
        // sleep so both spawned PUTs are genuinely polled first.
        tokio::time::sleep(Duration::from_millis(120)).await;
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
        let a = db.get_pv("A:rec.VAL").unwrap();
        let b = db.get_pv("B:rec.VAL").unwrap();
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
    /// (`groupsource.cpp:619-630`) over the same per-record locks a plain
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
        let many = db.lock_records(["A:rec", "B:rec"]);

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

        match db.get_pv("A:rec.VAL").unwrap() {
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
        let held = db.lock_record("B:rec");

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

        let a = db.get_pv("A:rec.VAL").unwrap();
        let b = db.get_pv("B:rec.VAL").unwrap();
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

    /// Regression (Q50): the atomic group GET must hold the same
    /// `DBManyLock`-equivalent gate set the atomic PUT holds — pvxs's
    /// `onGet` takes `DBManyLocker G(group.value.lock)`
    /// (`groupsource.cpp:492`), the identical lock its `onPutGroup` takes
    /// (`:621`). Holding one member's gate externally must block the atomic
    /// GET from entering its read loop, and the GET must complete once the
    /// gate is released. Pre-fix the atomic GET took only per-record `RwLock`
    /// read guards incrementally via `lock_group_records_read` and never
    /// `lock_records`, so a concurrent writer could slip a later-sorted
    /// member write between this GET's read of an earlier one — the torn
    /// snapshot the `atomic` flag exists to prevent (GET-side twin of
    /// `br_r15_atomic_put_blocks_on_member_record_gates`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn q50_atomic_get_blocks_on_member_record_gates() {
        let (db, def) = atomic_group_fixture().await;
        let channel = GroupChannel::new(db.clone(), def.clone());

        // Hold one member record's gate. The atomic group GET must block
        // trying to acquire the gate set via `lock_records`.
        let held = db.lock_record("B:rec");

        let get = tokio::spawn(async move { channel.read_group().await.unwrap() });

        // Give the spawned GET real time to run: if it did not take
        // `lock_records`, it would read both members and finish inside this
        // window. Blocked on the held gate, it stays unfinished.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !get.is_finished(),
            "atomic group GET must block while a member-record gate is held"
        );

        drop(held);
        let result = tokio::time::timeout(Duration::from_secs(5), get)
            .await
            .expect("atomic GET must complete once the member gate is free")
            .expect("atomic GET task panicked");

        // Both members read back their fixture values under one consistent
        // snapshot.
        match get_nested_field(&result, "a").as_deref() {
            Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(v))) => {
                assert_eq!(*v, 0.0)
            }
            other => panic!("member a: expected Double(0.0), got {other:?}"),
        }
        match get_nested_field(&result, "b").as_deref() {
            Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(v))) => {
                assert_eq!(*v, 0.0)
            }
            other => panic!("member b: expected Double(0.0), got {other:?}"),
        }
    }

    /// Regression (Q38): a MONITOR over a `+atomic:false` group must still
    /// compose its snapshot atomically — pvxs locks the fired field's whole
    /// trigger-target set (`DBManyLocker G(field.lock)`, `groupsource.cpp:326`)
    /// for every value callback and stamps `atomic=true` unconditionally
    /// (`:401-405`), regardless of the group's `+atomic` setting. The monitor
    /// `read_group()` therefore forces the atomic many-lock path even though
    /// the group is non-atomic. Holding one member's gate externally must block
    /// the monitor read from entering its loop; pre-fix the monitor took the
    /// sequential `read_group_atomic(false)` path (no `lock_records`) and would
    /// finish while a member gate was held, shipping a torn snapshot the wire
    /// still stamps atomic. A plain non-atomic GET (no monitor stamp) still
    /// takes the sequential path — asserted here as the contrast.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn q38_nonatomic_group_monitor_read_composes_atomically() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("A:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("B:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        // A NON-atomic group.
        let cfg = r#"{
            "NONATOMIC:GRP": {
                "+atomic": false,
                "a": {"+type": "plain", "+channel": "A:rec.VAL", "+putorder": 0},
                "b": {"+type": "plain", "+channel": "B:rec.VAL", "+putorder": 1}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        assert!(!def.atomic, "fixture group must be +atomic:false");

        // Hold one member record's gate. A monitor-stamped read must block on
        // `lock_records` despite the group being non-atomic.
        let held = db.lock_record("B:rec");

        let mon_channel = GroupChannel::new(db.clone(), def.clone())
            .with_monitor_stamp(epics_pva_rs::server_native::source::DEFAULT_MONITOR_QUEUE_LIMIT);
        let mon = tokio::spawn(async move { mon_channel.read_group().await.unwrap() });

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !mon.is_finished(),
            "a +atomic:false MONITOR read must block while a member gate is held \
             (it forces the atomic many-lock so its atomic=true stamp is truthful)"
        );

        // Contrast: a plain non-atomic GET (no monitor stamp) does NOT take the
        // many-lock — it reads sequentially and completes with the gate held.
        let get_channel = GroupChannel::new(db.clone(), def.clone());
        let get = tokio::spawn(async move { get_channel.read_group().await.unwrap() });
        let get_done = tokio::time::timeout(Duration::from_secs(5), get)
            .await
            .expect("non-atomic GET must finish without the many-lock")
            .expect("non-atomic GET task panicked");
        assert!(
            get_nested_field(&get_done, "a").is_some(),
            "non-atomic GET returns a snapshot without blocking"
        );

        // Release the gate; the monitor read now completes atomically.
        drop(held);
        let mon_snapshot = tokio::time::timeout(Duration::from_secs(5), mon)
            .await
            .expect("monitor read must complete once the member gate is free")
            .expect("monitor read task panicked");
        match get_nested_field(&mon_snapshot, "a").as_deref() {
            Some(PvField::Scalar(epics_pva_rs::pvdata::ScalarValue::Double(v))) => {
                assert_eq!(*v, 0.0)
            }
            other => panic!("member a: expected Double(0.0), got {other:?}"),
        }
    }

    /// A partial group PUT must not access-check unmarked members. pvxs
    /// builds the per-field SecurityClient over the *changed* fields
    /// (groupsource.cpp:213-226,564-567), so an unwritable member that
    /// the client did not mark cannot reject a PUT to an unrelated
    /// marked member. Whole-value callers still check every member.
    #[tokio::test]
    async fn br120_partial_put_skips_access_check_for_unmarked_member() {
        use super::super::provider::{AccessContext, AccessControl};
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        // Deny writes to member b's backing channel only.
        struct DenyChannel(&'static str);
        #[async_trait::async_trait]
        impl AccessControl for DenyChannel {
            async fn can_write(&self, channel: &str, _user: &str, _host: &str) -> bool {
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
                &RemoteLog::default(),
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
                &RemoteLog::default(),
            )
            .await;
        assert!(
            matches!(res, Err(BridgeError::PutRejected(_))),
            "full PUT including denied member b must be rejected, got {res:?}"
        );
    }

    /// Regression (Q51): a group PUT must NOT enforce per-member write ACF on a
    /// `proc` member. pvxs runs the write-ACF gate `doFieldPreProcessing`
    /// (`canWrite`) only for a `changing` value field (groupsource.cpp:564); a
    /// proc member is never `changing` (no `field.value`), so a client with
    /// group-PUT rights but NO write permission on the proc member's backing
    /// record still triggers processing and gets a normal reply
    /// (`doPostProcessing`, :568). Pre-fix Rust resolved a `write_grant` for
    /// the always-active proc member and a single denial failed the whole PUT.
    #[tokio::test]
    async fn q51_group_put_does_not_write_acf_check_proc_member() {
        use super::super::provider::{AccessContext, AccessControl};
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::ScalarValue;

        // Deny writes to the proc member's backing channel only.
        struct DenyChannel(&'static str);
        #[async_trait::async_trait]
        impl AccessControl for DenyChannel {
            async fn can_write(&self, channel: &str, _user: &str, _host: &str) -> bool {
                channel != self.0
            }
        }

        let db = Arc::new(PvDatabase::new());
        db.add_record("VAL:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.add_record("HOOK:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "PROC:ACF:GRP": {
                "+atomic": false,
                "v":  {"+type":"plain","+channel":"VAL:rec.VAL","+putorder":0},
                "go": {"+type":"proc","+channel":"HOOK:rec"}
            }
        }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();

        // The proc member's channel is write-denied; the value member is writable.
        let access =
            AccessContext::with_identity(Arc::new(DenyChannel("HOOK:rec")), "u".into(), "h".into());
        let channel = GroupChannel::new(db.clone(), def).with_access(access);

        assert!(
            !has_processed(&db, "HOOK:rec").await,
            "proc target unprocessed at start"
        );

        // PUT marking the writable value member; the write-denied proc member
        // must NOT block the PUT and must still be processed.
        let mut put = PvStructure::new("structure");
        put.fields
            .push(("v".into(), PvField::Scalar(ScalarValue::Double(5.0))));
        channel
            .put_with_options(
                &put,
                super::super::channel::PutOptions::default(),
                Some(false),
                &RemoteLog::default(),
            )
            .await
            .expect("group PUT must succeed: a write-denied proc member is not write-ACF checked");

        assert!(
            has_processed(&db, "HOOK:rec").await,
            "the proc member's record was processed despite the write deny"
        );
    }

    /// pvxs's `addMembersForMetaData` wraps `{alarm, timeStamp}` with the
    /// TWO-argument `members::Struct(name, children)` overload
    /// (`groupconfigprocessor.cpp:940-953` → `:1031-1033`,
    /// `include/pvxs/data.h:348-354`), which carries no id — the wire shows
    /// `structure m`, one zero byte, not `meta_t m` and a seven-byte id
    /// string. `groupField.id` is read at exactly one place upstream,
    /// `addMembersForStructureType` (`groupconfigprocessor.cpp:922-931`), so
    /// `+id` names a `+type:"structure"` member and nothing else.
    #[tokio::test]
    async fn meta_member_is_advertised_without_a_struct_id() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_pva_rs::pvdata::FieldDesc;

        let db = Arc::new(PvDatabase::new());
        db.add_record("METAID:rec", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();

        let cfg = r#"{ "METAID:GRP": {
            "m": { "+type": "meta", "+channel": "METAID:rec.VAL", "+id": "ignored/v1" },
            "s": { "+type": "scalar", "+channel": "METAID:rec.VAL", "+id": "alsoignored/v1" },
            "c": { "+type": "structure", "+id": "kept/v1" }
        } }"#;
        let mut defs = super::super::group_config::parse_group_config(cfg).unwrap();
        let def = defs.pop().unwrap();
        let channel = GroupChannel::new(db.clone(), def);

        let desc = channel.get_field().await.expect("get_field");
        let root = match &desc {
            FieldDesc::Structure { fields, .. } => fields,
            other => panic!("group descriptor must be a structure, got {other:?}"),
        };
        let id_of = |name: &str| match root.iter().find(|(n, _)| n == name).map(|(_, d)| d) {
            Some(FieldDesc::Structure { struct_id, fields }) => (struct_id.clone(), fields.clone()),
            other => panic!("{name} must be a structure, got {other:?}"),
        };

        let (meta_id, meta_fields) = id_of("m");
        assert_eq!(meta_id, "", "a meta member's enclosing struct has no id");
        assert!(
            matches!(
                meta_fields.iter().find(|(n, _)| n == "alarm").map(|(_, d)| d),
                Some(FieldDesc::Structure { struct_id, .. }) if struct_id == "alarm_t"
            ),
            "the alarm child keeps the id pvxs gives it, got {meta_fields:?}"
        );

        let (scalar_id, _) = id_of("s");
        assert_eq!(
            scalar_id, "epics:nt/NTScalar:1.0",
            "an NT leaf keeps the id its own type definition gives it"
        );

        let (structure_id, _) = id_of("c");
        assert_eq!(
            structure_id, "kept/v1",
            "+id names a +type:\"structure\" member, and only that"
        );
    }

    /// A group member's `+channel` filter reaches the group GET.
    ///
    /// pvxs reads every member through `dbChannelGet` on that member's own
    /// `dbChannel` (`ioc/iocsource.cpp:79,127,175,268`), and that channel is
    /// built from the full `def.channel` including the suffix
    /// (`ioc/field.cpp:23-26`), so an `arr` slice on a member is served
    /// sliced. This port refused the whole group instead, because the group
    /// paths re-derived `(record, field)` from the raw string per operation
    /// and had nowhere to keep a chain.
    #[tokio::test]
    async fn a_filtered_group_member_serves_the_sliced_array() {
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::EpicsValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "FLT:wf",
            Box::new(WaveformRecord::new(8, DbFieldType::Double)),
        )
        .await
        .unwrap();
        db.put_pv(
            "FLT:wf.VAL",
            EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0]),
        )
        .await
        .unwrap();

        let cfg = r#"{
            "FLT:GRP": {
                "w": {"+type": "plain", "+channel": "FLT:wf.VAL{\"arr\":{\"s\":1,\"e\":2}}"}
            }
        }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();
        // The member bound the suffix once, at `finalize()`.
        assert_eq!(def.channels.len(), 1);
        assert_eq!(def.channels[0].names(), ("FLT:wf", "VAL"));
        assert_eq!(def.channels[0].value_filters.len(), 1);

        let channel = GroupChannel::new(db.clone(), def);
        let pv = channel.read_group().await.expect("filtered group GET");
        match get_nested_field(&pv, "w").as_deref() {
            Some(PvField::ScalarArray(v)) => assert_eq!(
                v.len(),
                2,
                "`[1:2]` must slice the member to 2 elements, got {v:?}"
            ),
            other => panic!("member 'w' must be a scalar array, got {other:?}"),
        }
    }

    /// A `$` `+channel` is served, not refused: the group surface reaches
    /// the same long-string view the single-record one does.
    ///
    /// pvxs has one getter for both surfaces — `groupsource.cpp:344,377` and
    /// `singlesource.cpp:59,289` both call `IOCSource::get`, whose
    /// `final_type == DBR_CHAR && value.type() == TypeCode::String` branch
    /// (`iocsource.cpp:133-136`) collapses a char buffer to a NUL-terminated
    /// string — this port hands a `$` member that leaf type for the
    /// `DBF_STRING` half too, which pvxs does not
    /// (`qsrv::pvif::nt_type_for_channel`). The port had the group path resolve the BARE
    /// field name, which cannot see the view and therefore cannot apply the
    /// eligibility test either, so the only safe thing left was to refuse
    /// `$` at bind time — the parser accepted a name the decode path could
    /// not serve. Asking the view is what restores both halves at once, and
    /// both mappings are checked here because they render through different
    /// builders (`snapshot_to_pv_structure` vs `BareLeaf`) with the NT owner
    /// making them agree.
    #[tokio::test]
    async fn a_dollar_group_member_serves_the_long_string_view() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::ScalarValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record("LS:ai", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.put_pv(
            "LS:ai.DESC",
            EpicsValue::String("a description longer than a byte".into()),
        )
        .await
        .unwrap();

        db.put_record_field_from_ca_no_notify(
            "LS:ai",
            "FLNK",
            EpicsValue::String("LS:other".into()),
        )
        .await
        .expect("seed FLNK");

        // Both eligible C branches: `DBF_STRING` (`dbChannel.c:488-493`) and
        // `DBF_INLINK..DBF_FWDLINK` (`:494-498`), which keeps the original
        // dbfType and only re-views the client's type as `DBR_CHAR`.
        let cfg = r#"{
            "LS:GRP": {
                "d": {"+type": "scalar", "+channel": "LS:ai.DESC$"},
                "p": {"+type": "plain",  "+channel": "LS:ai.DESC$"},
                "f": {"+type": "plain",  "+channel": "LS:ai.FLNK$"}
            }
        }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            def.channels.iter().all(|m| m.string_view),
            "both members bound the `$` view"
        );

        // The bind gate admits it — this is the refusal that used to drop
        // the whole group.
        for m in def.channels.iter() {
            super::super::channel::resolve_db_channel(&db, m)
                .await
                .expect("an eligible `$` member must bind");
        }

        let channel = GroupChannel::new(db.clone(), def);
        let pv = channel.read_group().await.expect("`$` group GET");

        match get_nested_field(&pv, "d").as_deref() {
            Some(PvField::Structure(nt)) => match nt.get_field("value") {
                Some(PvField::Scalar(ScalarValue::String(v))) => {
                    assert_eq!(v, "a description longer than a byte");
                }
                other => panic!("scalar `$` member must carry a string value, got {other:?}"),
            },
            other => panic!("member 'd' must be an NTScalar structure, got {other:?}"),
        }
        match get_nested_field(&pv, "p").as_deref() {
            Some(PvField::Scalar(ScalarValue::String(v))) => {
                assert_eq!(v, "a description longer than a byte");
            }
            other => panic!(
                "plain `$` member must be a bare string leaf, not a byte array, got {other:?}"
            ),
        }
        assert_eq!(
            get_nested_field(&pv, "f").as_deref(),
            Some(&PvField::Scalar(ScalarValue::String("LS:other".into()))),
            "a `$` link member serves the link's textual form"
        );

        // The descriptor has to agree with those bytes. pvxs builds the
        // member descriptor from `getChannelValueType` on the same viewed
        // dbChannel that the getter reads (groupconfigprocessor.cpp:960-974),
        // so the port asks its own `getChannelValueType`
        // (`pvif::nt_type_for_channel`) with the same view the value came
        // from rather than letting the two answers be derived separately.
        let desc = channel.get_field().await.expect("group descriptor");
        let FieldDesc::Structure { fields, .. } = &desc else {
            panic!("group descriptor must be a structure");
        };
        let leaf = |name: &str| -> &FieldDesc {
            &fields
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("member '{name}' missing"))
                .1
        };
        match leaf("d") {
            FieldDesc::Structure { fields, .. } => assert_eq!(
                &fields.iter().find(|(n, _)| n == "value").unwrap().1,
                &FieldDesc::Scalar(ScalarType::String),
                "a scalar `$` member advertises an NTScalar string"
            ),
            other => panic!("member 'd' descriptor must be an NT structure, got {other:?}"),
        }
        assert_eq!(
            leaf("p"),
            &FieldDesc::Scalar(ScalarType::String),
            "a plain `$` member advertises a bare string, not a byte array"
        );
        assert_eq!(
            leaf("f"),
            &FieldDesc::Scalar(ScalarType::String),
            "a plain `$` link member advertises a bare string"
        );
    }

    /// The `$` eligibility rule now lives in the same place the value does.
    /// `dbChannelCreate` returns `S_dbLib_fieldNotFound` for `$` on anything
    /// but a `DBF_STRING` or a `DBF_INLINK..DBF_FWDLINK` link
    /// (`dbChannel.c:488-503`), and the group gate reports that with pvxs's
    /// own `Invalid PV:` text (`ioc/channel.cpp:29-38`).
    ///
    /// This is the half a bind-time refusal could not have given: resolving
    /// the bare field name answers "yes" for `VAL` whatever its type, so a
    /// group that asks the unviewed question admits `REC.VAL$` on a
    /// `DBF_DOUBLE` and serves a double under a `$` name. Measured — with
    /// the three `MemberChannel` accessors passing `false` for the view and
    /// no refusal in `resolve_db_channel`, this case binds.
    #[tokio::test]
    async fn a_dollar_group_member_on_a_numeric_field_is_still_refused() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("LS:num", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();

        let cfg = r#"{
            "LS:BAD": { "v": {"+type": "scalar", "+channel": "LS:num.VAL$"} }
        }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();
        let err = super::super::channel::resolve_db_channel(&db, &def.channels[0])
            .await
            .expect_err("`$` on a DBF_DOUBLE VAL must not bind");
        assert_eq!(err, "Invalid PV: LS:num.VAL$");
    }

    /// A `$` member is writable, and the write goes to the field the view is
    /// of. pvxs puts a group member through the same `IOCSource::put` as a
    /// single-record channel (`groupsource.cpp:564-567`), so the conversion
    /// target must be the VIEWED type — a string — not the raw field's.
    #[tokio::test]
    async fn a_dollar_group_member_round_trips_a_put() {
        use epics_base_rs::server::records::ai::AiRecord;
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::ScalarValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record("LS:rt", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        db.put_pv("LS:rt.DESC", EpicsValue::String("before".into()))
            .await
            .unwrap();

        let cfg = r#"{
            "LS:RT": { "d": {"+type": "plain", "+channel": "LS:rt.DESC$", "+putorder": 1} }
        }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();
        let channel = GroupChannel::new(db.clone(), def);

        let mut top = PvStructure::new("structure");
        top.fields.push((
            "d".into(),
            PvField::Scalar(ScalarValue::String("after".into())),
        ));
        channel
            .put_with_options(
                &top,
                super::super::channel::PutOptions::default(),
                Some(false),
                &RemoteLog::default(),
            )
            .await
            .expect("`$` group PUT");

        assert_eq!(
            db.get_pv("LS:rt.DESC").unwrap(),
            EpicsValue::String("after".into()),
            "the PUT must land on the field the `$` view is of"
        );
        assert_eq!(
            get_nested_field(&channel.read_group().await.unwrap(), "d").as_deref(),
            Some(&PvField::Scalar(ScalarValue::String("after".into()))),
        );
    }

    /// The member's chain is ONE instance for the life of the group, not a
    /// fresh parse per operation — the whole reason the object exists.
    ///
    /// `dbnd` keeps a per-instance baseline. Re-parsing per read would reset
    /// it every time and the filter would never drop anything. Driven here
    /// through the read path, whose `dbnd` short-circuits, so the assertion
    /// is on the chain's identity across two GETs and across a clone of the
    /// channel: pvxs's `Group` is not copyable (`ioc/group.h:52`) and every
    /// client shares one `Group&` (`ioc/groupsource.cpp:43-44`).
    #[tokio::test]
    async fn a_member_chain_is_one_instance_for_the_whole_group() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("DBND:ai", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "DBND:GRP": {
                "a": {"+type": "plain", "+channel": "DBND:ai.VAL{\"dbnd\":{\"d\":10.0}}"}
            }
        }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();

        let first = Arc::as_ptr(&def.channels[0].value_filters);
        let channel = GroupChannel::new(db.clone(), def.clone());
        channel.read_group().await.expect("first GET");
        channel.read_group().await.expect("second GET");
        assert_eq!(
            Arc::as_ptr(&channel.def.channels[0].value_filters),
            first,
            "two GETs must run the SAME chain instance"
        );

        // A second client channel clones the def; pvxs hands it the same
        // `Group&`, so the member's filter state is shared, not re-parsed.
        let other = GroupChannel::new(db.clone(), def.clone());
        assert_eq!(
            Arc::as_ptr(&other.def.channels[0].value_filters),
            first,
            "a second channel on the group must share the member's chain"
        );
        // The value and property chains stay independent instances
        // (`ioc/field.cpp:23-26` calls `Channel(def.channel)` twice).
        assert_ne!(
            Arc::as_ptr(&def.channels[0].value_filters) as *const u8,
            Arc::as_ptr(&def.channels[0].property_filters) as *const u8,
        );
    }

    /// A filtered member's group monitor subscribes THROUGH the member's
    /// value chain — `dbnd` gates this member's events exactly as it gates a
    /// single-record monitor's, because pvxs subscribes the member's own
    /// filtered `dbChannel` (`groupsource.cpp:431,440` off `field.cpp:25`).
    #[tokio::test]
    async fn a_filtered_group_member_subscribes_through_its_chain() {
        use epics_base_rs::server::records::ai::AiRecord;

        let db = Arc::new(PvDatabase::new());
        db.add_record("SUBF:ai", Box::new(AiRecord::new(0.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "SUBF:GRP": {
                "a": {"+type": "plain", "+channel": "SUBF:ai.VAL{\"dbnd\":{\"d\":10.0}}"}
            }
        }"#;
        let def = super::super::group_config::parse_group_config(cfg)
            .unwrap()
            .pop()
            .unwrap();
        use super::super::provider::PvaMonitor;
        let mut mon = GroupMonitor::new(db.clone(), def);
        mon.start().await.expect("filtered group monitor starts");

        let rec = db.get_record("SUBF:ai").expect("record exists");
        let filters_installed = {
            let inst = rec.read();
            inst.subscribers
                .get("VAL")
                .map(|subs| subs.iter().any(|s| !s.filters.is_empty()))
                .unwrap_or(false)
        };
        assert!(
            filters_installed,
            "the member subscription must carry the member's filter chain"
        );

        // A VALUE-only post: C `dbnd` computes
        // `send = pfl->mask & ~(DBE_VALUE|DBE_LOG)` (`filters/dbnd.c:84`), so
        // an event carrying DBE_ALARM passes the deadband unconditionally and
        // would say nothing about the filter.
        let post = async |v: f64| {
            db.put_pv_no_process("SUBF:ai.VAL", epics_base_rs::types::EpicsValue::Double(v))
                .await
                .unwrap();
            let rec = db.get_record("SUBF:ai").expect("record exists");
            rec.write().notify_field("VAL", DbeMask::VALUE);
        };

        // Drain the INIT frame (and anything else already queued) so the
        // assertions below are about the deadband, not about the seed.
        while tokio::time::timeout(Duration::from_millis(200), mon.poll())
            .await
            .is_ok()
        {}

        // The first post sets the deadband baseline and wakes the group.
        post(100.0).await;
        tokio::time::timeout(Duration::from_secs(2), mon.poll())
            .await
            .expect("the first post must reach the group monitor")
            .expect("an update");

        // Inside the band: dropped at the member subscription, so the group
        // never updates.
        post(105.0).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), mon.poll())
                .await
                .is_err(),
            "a sub-deadband member post must not wake the group monitor"
        );

        // Outside the band, measured against the baseline the FIRST post
        // set (100.0, not 105.0): the chain is one instance across events.
        post(120.0).await;
        tokio::time::timeout(Duration::from_secs(2), mon.poll())
            .await
            .expect("a supra-deadband member post must wake the group monitor")
            .expect("an update");

        mon.stop().await;
    }
}
