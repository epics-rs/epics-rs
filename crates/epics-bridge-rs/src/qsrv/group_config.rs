//! Group PV JSON configuration parser.
//!
//! Parses C++ QSRV-compatible group definitions from JSON.
//! See `~/epics-base/modules/pva2pva/pdbApp/configparse.cpp` for the
//! original C++ parser.
//!
//! # JSON format
//!
//! ```json
//! {
//!   "GROUP:NAME": {
//!     "+id": "some/NT:1.0",
//!     "+atomic": true,
//!     "fieldName": {
//!       "+type": "scalar",
//!       "+channel": "RECORD:FIELD",
//!       "+trigger": "*",
//!       "+putorder": 0
//!     }
//!   }
//! }
//! ```

use serde::Deserialize;
use std::collections::HashMap;

use super::pvif::FieldMapping;
use crate::error::{BridgeError, BridgeResult};

/// Definition of a group PV (multiple records composited into one PvStructure).
#[derive(Debug, Clone)]
pub struct GroupPvDef {
    pub name: String,
    pub struct_id: Option<String>,
    pub atomic: bool,
    pub members: Vec<GroupMember>,
    /// Serializes concurrent PUTs to the same `atomic` group so an
    /// atomic-flagged group cannot be observed half-applied by another
    /// PUT to the same group. Shared (`Arc`) across every `clone` of
    /// this def — `create_channel` clones the def per downstream
    /// channel, and all clones for a given group name come from one
    /// map entry, so they share the same lock instance.
    ///
    /// This closes the group-vs-group interleave: two atomic PUTs to
    /// the same group run strictly serially even during the up-front
    /// value-conversion phase before either reaches `lock_records`.
    ///
    /// BR-R15: the non-group writer gap — a plain CA/PVA PUT to a
    /// record that also backs an atomic-group member interleaving
    /// between member writes — is now closed by the
    /// `DBManyLock`-equivalent `PvDatabase::lock_records` advisory
    /// write gates the atomic PUT acquires over every member record.
    /// This `Mutex` remains only as an internal group-vs-group
    /// serialization aid. See `GroupChannel::put`.
    pub atomic_write_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
}

/// A single member within a group PV.
#[derive(Debug, Clone)]
pub struct GroupMember {
    /// Field path within the group structure (e.g., "temperature").
    pub field_name: String,
    /// Source record and field (e.g., "TEMP:ai.VAL").
    /// Empty for Structure and Const mappings (no backing channel).
    pub channel: String,
    /// How to map the record field to PVA structure.
    pub mapping: FieldMapping,
    /// Which fields to update when this member changes.
    pub triggers: TriggerDef,
    /// Ordering for put operations.
    ///
    /// BR-R30: pvxs defaults the missing-field sentinel to
    /// `i64::MIN` (`fieldconfig.h:37`) and treats that value as
    /// not-putable (`groupsource.cpp:503`). Wire parity therefore
    /// requires `Option<i32>` here, not a defaulted i32 — a
    /// member without an explicit `+putorder` must be silently
    /// dropped from the PUT ordering, NOT written under an
    /// implicit `0`.
    pub put_order: Option<i32>,
    /// Optional structure ID for this member (from `+id`).
    pub struct_id: Option<String>,
    /// Constant value for `Const` mapping. Sourced from `+const`
    /// (pvxs canonical key, `test/qgroup.json`) with a legacy
    /// `+value` fallback for older Rust-authored configs.
    pub const_value: Option<epics_pva_rs::pvdata::PvField>,
    /// Nanosecond mask: lower bits of nsec are extracted as userTag
    /// (pvxs `MappingInfo::nsecMask`, from `+nsecmask` in JSON).
    pub nsec_mask: u32,
}

impl GroupPvDef {
    /// BR-R29: true iff this is a *pure self-trigger* group — every
    /// member with a backing channel uses the default `+trigger`
    /// (self-trigger, [`TriggerDef::SelfOnly`]) or explicit silence
    /// ([`TriggerDef::None`]).
    ///
    /// For such a group each monitor event re-reads only the member
    /// whose record processed, so the wire changed-bitset can be
    /// narrowed to that member's leaves by structurally diffing
    /// consecutive snapshots — exactly the leaf set pvxs marks via
    /// `IOCSource::get` for a self-triggered field
    /// (`groupsource.cpp:288`). A group containing an explicit
    /// `+trigger:"*"` ([`TriggerDef::All`]) or named-field
    /// ([`TriggerDef::Fields`]) member is excluded: pvxs marks the
    /// whole *triggered target set* there (assigned-not-changed
    /// semantics, `dataencode.cpp:425` `store[bit].valid`), which a
    /// value-diff would under-mark, so those groups keep the full
    /// request mask on the wire.
    pub fn is_pure_self_trigger(&self) -> bool {
        self.members.iter().all(|m| {
            m.channel.is_empty() || matches!(m.triggers, TriggerDef::SelfOnly | TriggerDef::None)
        })
    }
}

/// Defines which group fields are updated when a member's source record changes.
#[derive(Debug, Clone)]
pub enum TriggerDef {
    /// `"*"` — update all fields in the group.
    All,
    /// Named fields — update only these fields.
    Fields(Vec<String>),
    /// BR-R29: missing `+trigger` — pvxs default is self-trigger
    /// (`groupconfigprocessor.cpp:323`): the member triggers only
    /// its own field, NOT every other group field. Distinct from
    /// `All` (`"*"`) and from `None` (`""`, explicit silence). The
    /// monitor encoder narrows the changed-bitset accordingly so
    /// downstream clients see a tight delta instead of a full-group
    /// reread.
    SelfOnly,
    /// `""` — never trigger a group update for this member.
    None,
}

/// Parse group definitions from a JSON string.
///
/// The JSON is a top-level object where each key is a group name.
pub fn parse_group_config(json: &str) -> BridgeResult<Vec<GroupPvDef>> {
    let root: HashMap<String, RawGroupDef> =
        serde_json::from_str(json).map_err(|e| BridgeError::GroupConfigError(e.to_string()))?;

    let mut groups = Vec::new();
    for (name, raw) in root {
        groups.push(raw_to_group_def(name, raw)?);
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

/// Parse group definitions from a record's `info(Q:group, ...)` tag.
///
/// In C++ QSRV, records can declare group membership via:
/// ```text
/// record(ai, "TEMP:sensor") {
///     info(Q:group, {
///         "TEMP:group": {
///             "temperature": {"+channel": "VAL", "+type": "plain", "+trigger": "*"}
///         }
///     })
/// }
/// ```
///
/// The `record_name` is used as channel prefix: if `+channel` is a bare field
/// name (no `:` separator), it becomes `"record_name.FIELD"`.
pub fn parse_info_group(record_name: &str, json: &str) -> BridgeResult<Vec<GroupPvDef>> {
    let root: HashMap<String, RawGroupDef> =
        serde_json::from_str(json).map_err(|e| BridgeError::GroupConfigError(e.to_string()))?;

    let mut groups = Vec::new();
    for (name, raw) in root {
        let mut def = raw_to_group_def(name, raw)?;
        // Apply channel prefix: bare field names get record_name prefix
        for member in &mut def.members {
            // Structure/Const have empty channels — skip prefix.
            if !member.channel.is_empty()
                && !member.channel.contains(':')
                && !member.channel.contains('.')
            {
                member.channel = format!("{}.{}", record_name, member.channel);
            }
        }
        groups.push(def);
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

/// Merge additional group definitions into an existing set.
///
/// Members are appended to existing groups; new groups are created.
/// This supports the C++ pattern where multiple records contribute
/// members to the same group via separate info(Q:group) tags.
pub fn merge_group_defs(existing: &mut HashMap<String, GroupPvDef>, new_defs: Vec<GroupPvDef>) {
    for def in new_defs {
        if let Some(existing_def) = existing.get_mut(&def.name) {
            // Merge members into existing group
            existing_def.members.extend(def.members);
            // Update struct_id if newly specified (last wins)
            if def.struct_id.is_some() {
                existing_def.struct_id = def.struct_id;
            }
            // atomic: last-wins with warning on conflict
            // (pvxs groupconfigprocessor.cpp:274-288)
            if existing_def.atomic != def.atomic {
                eprintln!(
                    "warning: group '{}' atomic setting inconsistent, using latest ({})",
                    def.name, def.atomic
                );
            }
            existing_def.atomic = def.atomic;
        } else {
            existing.insert(def.name.clone(), def);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal JSON deserialization types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawGroupDef {
    #[serde(rename = "+id")]
    id: Option<String>,
    #[serde(rename = "+atomic", default = "default_atomic")]
    atomic: bool,
    #[serde(flatten)]
    fields: HashMap<String, serde_json::Value>,
}

fn default_atomic() -> bool {
    true
}

fn raw_to_group_def(name: String, raw: RawGroupDef) -> BridgeResult<GroupPvDef> {
    let mut members = Vec::new();

    for (field_name, value) in &raw.fields {
        // Skip meta-keys (already extracted via named fields)
        if field_name.starts_with('+') {
            continue;
        }

        let member = parse_member(field_name, value)?;
        members.push(member);
    }

    // Sort by put_order for deterministic ordering
    members.sort_by_key(|m| m.put_order);

    // Validate trigger field references against actual member field names.
    // C++ QSRV does this in pdb.cpp:510-533 (trigger resolution phase).
    let member_names: std::collections::HashSet<&str> =
        members.iter().map(|m| m.field_name.as_str()).collect();
    // Members with channels (can actually be triggered at runtime).
    let channeled_names: std::collections::HashSet<&str> = members
        .iter()
        .filter(|m| !m.channel.is_empty())
        .map(|m| m.field_name.as_str())
        .collect();

    for member in &members {
        if let TriggerDef::Fields(targets) = &member.triggers {
            for target in targets {
                if !member_names.contains(target.as_str()) {
                    return Err(BridgeError::GroupConfigError(format!(
                        "group '{}': member '{}' has trigger '{}' which is not a member of this group",
                        name, member.field_name, target
                    )));
                }
                // Warn if trigger target has no channel (Structure/Const) —
                // pvxs silently ignores these (groupconfigprocessor.cpp:405-407).
                if !channeled_names.contains(target.as_str()) {
                    eprintln!(
                        "warning: group '{}': trigger '{}' on member '{}' targets a field without a channel (ignored)",
                        name, target, member.field_name
                    );
                }
            }
        }
    }

    Ok(GroupPvDef {
        name,
        struct_id: raw.id,
        atomic: raw.atomic,
        members,
        atomic_write_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    })
}

fn parse_member(field_name: &str, value: &serde_json::Value) -> BridgeResult<GroupMember> {
    let obj = value.as_object().ok_or_else(|| {
        BridgeError::GroupConfigError(format!("field '{field_name}' must be an object"))
    })?;

    let mapping = match obj.get("+type").and_then(|v| v.as_str()) {
        Some("scalar") | None => FieldMapping::Scalar,
        Some("plain") => FieldMapping::Plain,
        Some("meta") => FieldMapping::Meta,
        Some("any") => FieldMapping::Any,
        Some("proc") => FieldMapping::Proc,
        Some("structure") => FieldMapping::Structure,
        Some("const") => FieldMapping::Const,
        Some(other) => {
            return Err(BridgeError::GroupConfigError(format!(
                "unknown +type '{other}' for field '{field_name}'"
            )));
        }
    };

    // Structure and Const mappings have no backing channel.
    // Warn and ignore +channel if present (pvxs groupconfigprocessor.cpp:148-155).
    let channel = match mapping {
        FieldMapping::Structure | FieldMapping::Const => {
            if obj.get("+channel").is_some() {
                eprintln!(
                    "warning: field '{field_name}' has +type={:?}, ignoring +channel",
                    if mapping == FieldMapping::Structure {
                        "structure"
                    } else {
                        "const"
                    }
                );
            }
            String::new()
        }
        _ => obj
            .get("+channel")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                BridgeError::GroupConfigError(format!("field '{field_name}' missing +channel"))
            })?
            .to_string(),
    };

    // Parse constant value for Const mapping.
    //
    // BR-R26: pvxs uses `+const` (test/qgroup.json:1, test/const.db:2);
    // older Rust drafts accepted only `+value`. Accept both spellings
    // so pvxs-authored configs load without rewriting. When both
    // keys are present `+const` wins (matches pvxs's authoritative
    // key); a deprecation warning surfaces so operators know to
    // migrate.
    let const_value = if mapping == FieldMapping::Const {
        let val = match (obj.get("+const"), obj.get("+value")) {
            (Some(v), _) => v,
            (None, Some(v)) => {
                tracing::warn!(
                    field = field_name,
                    "+value for const mapping is deprecated; use `+const` for pvxs parity"
                );
                v
            }
            (None, None) => {
                return Err(BridgeError::GroupConfigError(format!(
                    "field '{field_name}': +type=const requires +const (or legacy +value)"
                )));
            }
        };
        Some(json_to_pv_field(val).map_err(|e| {
            BridgeError::GroupConfigError(format!("field '{field_name}': invalid const value: {e}"))
        })?)
    } else {
        None
    };

    // Structure and Const have no channel — they cannot trigger or be
    // triggered, so force TriggerDef::None (pvxs groupconfigprocessor.cpp:405-407).
    let triggers = if mapping == FieldMapping::Structure || mapping == FieldMapping::Const {
        TriggerDef::None
    } else {
        match obj.get("+trigger").and_then(|v| v.as_str()) {
            Some("*") => TriggerDef::All,
            // BR-R29: pvxs groupconfigprocessor.cpp:323 defaults a
            // missing `+trigger` to self-trigger (only this member's
            // own field re-emits in the group), not All. The Rust
            // path treated None as All and emitted a full-group
            // changed bitset on every member event — distinct from
            // pvxs's narrow self-trigger delta visible in
            // testqgroup.cpp:220 (NTEnum group: only `value.index`
            // bit set on a VAL update).
            // BR-R58: this per-member default is only provisional. pvxs
            // applies the self-trigger fallback at the GROUP level and only
            // when the whole group declares no triggers
            // (`groupconfigprocessor.cpp:317-339`). In a group where any
            // member has an explicit `+trigger`, a no-`+trigger` member is
            // silent. `GroupPvDef::resolve_self_trigger_default` demotes
            // `SelfOnly` → `None` for such mixed groups after all members
            // are assembled.
            None => TriggerDef::SelfOnly,
            Some("") => TriggerDef::None,
            Some(s) => TriggerDef::Fields(s.split(',').map(|f| f.trim().to_string()).collect()),
        }
    };

    let put_order = obj
        .get("+putorder")
        .and_then(|v| v.as_i64())
        .map(|n| n as i32);

    let struct_id = obj
        .get("+id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let nsec_mask = obj.get("+nsecmask").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    Ok(GroupMember {
        field_name: field_name.to_string(),
        channel,
        mapping,
        triggers,
        put_order,
        struct_id,
        const_value,
        nsec_mask,
    })
}

/// Convert a JSON value to a PvField for Const mapping.
///
/// Const values support the full recursive pvData value space:
///
/// * Scalars (`bool`/number/string) map to [`PvField::Scalar`].
/// * `null` maps to [`PvField::Null`] — pvxs accepts a JSON `null`
///   const (an empty/unset value), so rejecting it was a gap.
/// * Arrays map to the tightest container that fits their elements:
///   an all-scalar array becomes a [`PvField::ScalarArray`], an
///   all-object array becomes a [`PvField::StructureArray`], and an
///   array with nested arrays or mixed element kinds becomes a
///   [`PvField::VariantArray`] (each element kept as a `Variant`).
///   This lifts the prior "nested arrays/structures not supported in
///   const array" restriction.
/// * Objects map to [`PvField::Structure`], recursively.
fn json_to_pv_field(v: &serde_json::Value) -> Result<epics_pva_rs::pvdata::PvField, String> {
    use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue, VariantValue};
    match v {
        serde_json::Value::Bool(b) => Ok(PvField::Scalar(ScalarValue::Boolean(*b))),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(PvField::Scalar(ScalarValue::Int(i as i32)))
            } else if let Some(f) = n.as_f64() {
                Ok(PvField::Scalar(ScalarValue::Double(f)))
            } else {
                Err(format!("unsupported number: {n}"))
            }
        }
        serde_json::Value::String(s) => Ok(PvField::Scalar(ScalarValue::String(s.clone()))),
        serde_json::Value::Array(arr) => {
            // Decode every element first, then pick the tightest
            // container that holds all of them.
            let elems: Vec<PvField> = arr.iter().map(json_to_pv_field).collect::<Result<_, _>>()?;

            if elems.iter().all(|e| matches!(e, PvField::Scalar(_))) {
                let scalars = elems
                    .into_iter()
                    .map(|e| match e {
                        PvField::Scalar(sv) => sv,
                        _ => unreachable!("checked all-scalar above"),
                    })
                    .collect();
                Ok(PvField::ScalarArray(scalars))
            } else if !elems.is_empty() && elems.iter().all(|e| matches!(e, PvField::Structure(_)))
            {
                // PVA-FR-1: JSON array elements are all present, so each
                // maps to a `Some` element.
                let structs = elems
                    .into_iter()
                    .map(|e| match e {
                        PvField::Structure(s) => Some(s),
                        _ => unreachable!("checked all-structure above"),
                    })
                    .collect();
                Ok(PvField::StructureArray(structs))
            } else {
                // Nested arrays, null elements, or mixed kinds — keep
                // each element verbatim inside a Variant. All present
                // (`Some`) here; JSON `null` is handled elsewhere.
                let items = elems
                    .into_iter()
                    .map(|e| {
                        Some(VariantValue {
                            desc: Some(e.descriptor()),
                            value: e,
                        })
                    })
                    .collect();
                Ok(PvField::VariantArray(items))
            }
        }
        serde_json::Value::Object(map) => {
            let mut pv = PvStructure::new("");
            for (key, val) in map {
                pv.fields.push((key.clone(), json_to_pv_field(val)?));
            }
            Ok(PvField::Structure(pv))
        }
        serde_json::Value::Null => Ok(PvField::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_group() {
        let json = r#"{
            "TEST:group": {
                "+id": "epics:nt/NTTable:1.0",
                "+atomic": true,
                "temperature": {
                    "+type": "scalar",
                    "+channel": "TEMP:ai",
                    "+trigger": "*",
                    "+putorder": 0
                },
                "pressure": {
                    "+type": "scalar",
                    "+channel": "PRESS:ai",
                    "+trigger": "temperature,pressure",
                    "+putorder": 1
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups.len(), 1);

        let g = &groups[0];
        assert_eq!(g.name, "TEST:group");
        assert_eq!(g.struct_id.as_deref(), Some("epics:nt/NTTable:1.0"));
        assert!(g.atomic);
        assert_eq!(g.members.len(), 2);

        let temp = &g.members[0];
        assert_eq!(temp.field_name, "temperature");
        assert_eq!(temp.channel, "TEMP:ai");
        assert_eq!(temp.mapping, FieldMapping::Scalar);
        assert!(matches!(temp.triggers, TriggerDef::All));
        assert_eq!(temp.put_order, Some(0));

        let press = &g.members[1];
        assert_eq!(press.field_name, "pressure");
        assert_eq!(press.channel, "PRESS:ai");
        if let TriggerDef::Fields(ref fields) = press.triggers {
            assert_eq!(fields, &["temperature", "pressure"]);
        } else {
            panic!("expected TriggerDef::Fields");
        }
    }

    #[test]
    fn parse_minimal_member() {
        let json = r#"{
            "GRP:min": {
                "val": {
                    "+channel": "REC:val"
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert_eq!(m.mapping, FieldMapping::Scalar); // default
        // BR-R29: pvxs default for a missing `+trigger` is self-trigger
        // (`groupconfigprocessor.cpp:323`), not All.
        assert!(matches!(m.triggers, TriggerDef::SelfOnly));
        // BR-R30: missing `+putorder` → `None` (not putable),
        // mirroring pvxs `fieldconfig.h:37` sentinel.
        assert_eq!(m.put_order, None);
    }

    /// BR-R29 residual: a group whose members all use the default
    /// `+trigger` (self-trigger) is a *pure self-trigger* group — the
    /// PVA server may narrow its monitor changed-bitset. A group with
    /// any explicit `+trigger:"*"` or named-field member is NOT, so
    /// it keeps the full request mask.
    #[test]
    fn br_r29_pure_self_trigger_predicate() {
        // All members default `+trigger` → pure self-trigger.
        let pure = parse_group_config(
            r#"{ "GRP:pure": {
                "a": {"+channel": "R:a"},
                "b": {"+channel": "R:b"}
            }}"#,
        )
        .unwrap();
        assert!(
            pure[0].is_pure_self_trigger(),
            "all-default-trigger group must be pure self-trigger"
        );

        // One member with explicit `+trigger:"*"` → NOT pure.
        let with_star = parse_group_config(
            r#"{ "GRP:star": {
                "a": {"+channel": "R:a"},
                "b": {"+channel": "R:b", "+trigger": "*"}
            }}"#,
        )
        .unwrap();
        assert!(
            !with_star[0].is_pure_self_trigger(),
            "a group with an explicit +trigger:* member must NOT be pure self-trigger"
        );

        // One member with named `+trigger` → NOT pure.
        let with_fields = parse_group_config(
            r#"{ "GRP:fields": {
                "a": {"+channel": "R:a", "+trigger": "a,b"},
                "b": {"+channel": "R:b"}
            }}"#,
        )
        .unwrap();
        assert!(
            !with_fields[0].is_pure_self_trigger(),
            "a group with a named +trigger member must NOT be pure self-trigger"
        );
    }

    /// BR-R29: an explicit `+trigger:"*"` is still parsed as `All`
    /// (distinct from the default self-trigger). Regression guard for
    /// the previous None→All collapse.
    #[test]
    fn parse_trigger_star_is_all() {
        let json = r#"{
            "GRP:star": {
                "val": {
                    "+channel": "REC:val",
                    "+trigger": "*"
                }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert!(matches!(m.triggers, TriggerDef::All));
    }

    #[test]
    fn parse_proc_mapping() {
        let json = r#"{
            "GRP:proc": {
                "trigger": {
                    "+type": "proc",
                    "+channel": "REC:proc",
                    "+trigger": ""
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert_eq!(m.mapping, FieldMapping::Proc);
        assert!(matches!(m.triggers, TriggerDef::None));
    }

    #[test]
    fn parse_error_missing_channel() {
        let json = r#"{
            "GRP:bad": {
                "val": {
                    "+type": "scalar"
                }
            }
        }"#;

        assert!(parse_group_config(json).is_err());
    }

    #[test]
    fn parse_multiple_groups() {
        let json = r#"{
            "GRP:b": {
                "x": { "+channel": "B:x" }
            },
            "GRP:a": {
                "y": { "+channel": "A:y" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups.len(), 2);
        // Sorted by name
        assert_eq!(groups[0].name, "GRP:a");
        assert_eq!(groups[1].name, "GRP:b");
    }

    #[test]
    fn parse_member_id() {
        let json = r#"{
            "GRP:id": {
                "sensor": {
                    "+channel": "SENSOR:ai",
                    "+id": "epics:nt/NTScalar:1.0"
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert_eq!(m.struct_id.as_deref(), Some("epics:nt/NTScalar:1.0"));
    }

    #[test]
    fn parse_member_no_id() {
        let json = r#"{
            "GRP:noid": {
                "val": { "+channel": "REC:val" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert!(groups[0].members[0].struct_id.is_none());
    }

    #[test]
    fn parse_info_group_prefix() {
        let json = r#"{
            "TEMP:group": {
                "temperature": {
                    "+channel": "VAL",
                    "+type": "plain",
                    "+trigger": "*"
                }
            }
        }"#;

        let groups = parse_info_group("TEMP:sensor", json).unwrap();
        // Bare field "VAL" should become "TEMP:sensor.VAL"
        assert_eq!(groups[0].members[0].channel, "TEMP:sensor.VAL");
    }

    #[test]
    fn parse_info_group_absolute_channel() {
        let json = r#"{
            "TEMP:group": {
                "pressure": {
                    "+channel": "PRESS:ai",
                    "+type": "scalar"
                }
            }
        }"#;

        let groups = parse_info_group("TEMP:sensor", json).unwrap();
        // Absolute channel (contains ':') should be kept as-is
        assert_eq!(groups[0].members[0].channel, "PRESS:ai");
    }

    #[test]
    fn parse_info_group_structure_keeps_empty_channel() {
        let json = r#"{
            "TEMP:group": {
                "container": { "+type": "structure" },
                "val": { "+channel": "VAL", "+type": "plain" }
            }
        }"#;

        let groups = parse_info_group("TEMP:sensor", json).unwrap();
        let container = groups[0]
            .members
            .iter()
            .find(|m| m.field_name == "container")
            .unwrap();
        // Structure has no channel — must stay empty, not become "TEMP:sensor."
        assert!(container.channel.is_empty());
        // Normal member gets prefix
        let val = groups[0]
            .members
            .iter()
            .find(|m| m.field_name == "val")
            .unwrap();
        assert_eq!(val.channel, "TEMP:sensor.VAL");
    }

    #[test]
    fn merge_groups() {
        let mut existing = HashMap::new();
        let defs1 = parse_group_config(
            r#"{
            "GRP:a": {
                "x": { "+channel": "R1:x" }
            }
        }"#,
        )
        .unwrap();
        merge_group_defs(&mut existing, defs1);

        let defs2 = parse_group_config(
            r#"{
            "GRP:a": {
                "y": { "+channel": "R2:y" }
            }
        }"#,
        )
        .unwrap();
        merge_group_defs(&mut existing, defs2);

        let grp = existing.get("GRP:a").unwrap();
        assert_eq!(grp.members.len(), 2);
    }

    #[test]
    fn trigger_validation_unknown_field() {
        let json = r#"{
            "GRP:bad": {
                "x": {
                    "+channel": "R:x",
                    "+trigger": "y,z"
                },
                "y": { "+channel": "R:y" }
            }
        }"#;

        // y exists but z doesn't — should fail
        let result = parse_group_config(json);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("'z'"), "expected error about 'z': {err}");
    }

    #[test]
    fn trigger_validation_self_reference() {
        let json = r#"{
            "GRP:ok": {
                "a": { "+channel": "R:a", "+trigger": "a,b" },
                "b": { "+channel": "R:b", "+trigger": "a" }
            }
        }"#;

        // Self-reference and cross-reference are both valid
        let result = parse_group_config(json);
        assert!(result.is_ok());
    }

    #[test]
    fn trigger_validation_star_passes() {
        let json = r#"{
            "GRP:ok": {
                "a": { "+channel": "R:a", "+trigger": "*" }
            }
        }"#;

        // "*" doesn't go through field validation
        assert!(parse_group_config(json).is_ok());
    }

    #[test]
    fn parse_structure_mapping() {
        let json = r#"{
            "GRP:struct": {
                "container": {
                    "+type": "structure",
                    "+id": "my:container/v1"
                },
                "val": { "+channel": "R:val" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let members = &groups[0].members;
        let container = members
            .iter()
            .find(|m| m.field_name == "container")
            .unwrap();
        assert_eq!(container.mapping, FieldMapping::Structure);
        assert!(container.channel.is_empty());
        assert_eq!(container.struct_id.as_deref(), Some("my:container/v1"));
    }

    #[test]
    fn parse_const_mapping_scalar() {
        let json = r#"{
            "GRP:const": {
                "version": {
                    "+type": "const",
                    "+value": 42
                },
                "val": { "+channel": "R:val" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let members = &groups[0].members;
        let version = members.iter().find(|m| m.field_name == "version").unwrap();
        assert_eq!(version.mapping, FieldMapping::Const);
        assert!(version.channel.is_empty());
        assert!(version.const_value.is_some());
        if let Some(epics_pva_rs::pvdata::PvField::Scalar(
            epics_pva_rs::pvdata::ScalarValue::Int(v),
        )) = &version.const_value
        {
            assert_eq!(*v, 42);
        } else {
            panic!("expected Int(42), got {:?}", version.const_value);
        }
    }

    /// BR-R26: pvxs's canonical key is `+const` (test/qgroup.json).
    #[test]
    fn parse_const_mapping_pvxs_const_key() {
        let json = r#"{
            "GRP:const": {
                "version": {
                    "+type": "const",
                    "+const": 7
                },
                "val": { "+channel": "R:val" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let version = groups[0]
            .members
            .iter()
            .find(|m| m.field_name == "version")
            .unwrap();
        assert_eq!(version.mapping, FieldMapping::Const);
        if let Some(epics_pva_rs::pvdata::PvField::Scalar(
            epics_pva_rs::pvdata::ScalarValue::Int(v),
        )) = &version.const_value
        {
            assert_eq!(*v, 7);
        } else {
            panic!("expected Int(7) via +const, got {:?}", version.const_value);
        }
    }

    /// BR-R26: `+const` wins when both keys are present.
    #[test]
    fn parse_const_mapping_const_key_wins_over_value() {
        let json = r#"{
            "GRP:both": {
                "k": {
                    "+type": "const",
                    "+const": 100,
                    "+value": 999
                },
                "v": { "+channel": "R:val" }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let k = groups[0]
            .members
            .iter()
            .find(|m| m.field_name == "k")
            .unwrap();
        if let Some(epics_pva_rs::pvdata::PvField::Scalar(
            epics_pva_rs::pvdata::ScalarValue::Int(v),
        )) = &k.const_value
        {
            assert_eq!(*v, 100, "+const should take precedence over +value");
        } else {
            panic!("expected Int(100), got {:?}", k.const_value);
        }
    }

    #[test]
    fn parse_const_mapping_string() {
        let json = r#"{
            "GRP:const": {
                "label": {
                    "+type": "const",
                    "+value": "hello"
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert_eq!(m.mapping, FieldMapping::Const);
        if let Some(epics_pva_rs::pvdata::PvField::Scalar(
            epics_pva_rs::pvdata::ScalarValue::String(s),
        )) = &m.const_value
        {
            assert_eq!(s, "hello");
        } else {
            panic!("expected String(\"hello\")");
        }
    }

    /// B8: a const array of scalars stays a `ScalarArray`.
    #[test]
    fn parse_const_scalar_array() {
        use epics_pva_rs::pvdata::{PvField, ScalarValue};
        let json = r#"{
            "GRP:c": {
                "list": { "+type": "const", "+value": [1, 2, 3] }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        match &m.const_value {
            Some(PvField::ScalarArray(items)) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items[0], ScalarValue::Int(1)));
            }
            other => panic!("expected ScalarArray, got {other:?}"),
        }
    }

    /// B8: a const array containing nested arrays is accepted as a
    /// `VariantArray` instead of being rejected.
    #[test]
    fn parse_const_nested_array() {
        use epics_pva_rs::pvdata::PvField;
        let json = r#"{
            "GRP:c": {
                "matrix": { "+type": "const", "+value": [[1, 2], [3, 4]] }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        match &m.const_value {
            Some(PvField::VariantArray(items)) => {
                assert_eq!(items.len(), 2, "two nested rows");
                assert!(
                    matches!(items[0].as_ref().unwrap().value, PvField::ScalarArray(_)),
                    "each nested element is a scalar array"
                );
            }
            other => panic!("expected VariantArray of nested arrays, got {other:?}"),
        }
    }

    /// B8: a const array of objects is accepted as a `StructureArray`.
    #[test]
    fn parse_const_array_of_structures() {
        use epics_pva_rs::pvdata::PvField;
        let json = r#"{
            "GRP:c": {
                "rows": {
                    "+type": "const",
                    "+value": [{"a": 1}, {"a": 2}]
                }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        match &m.const_value {
            Some(PvField::StructureArray(items)) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].as_ref().unwrap().fields[0].0, "a");
            }
            other => panic!("expected StructureArray, got {other:?}"),
        }
    }

    /// B8: a nested structure const value is accepted recursively.
    #[test]
    fn parse_const_nested_structure() {
        use epics_pva_rs::pvdata::PvField;
        let json = r#"{
            "GRP:c": {
                "cfg": {
                    "+type": "const",
                    "+value": {"limits": {"low": 0, "high": 10}}
                }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        match &m.const_value {
            Some(PvField::Structure(s)) => {
                assert_eq!(s.fields[0].0, "limits");
                match &s.fields[0].1 {
                    PvField::Structure(inner) => assert_eq!(inner.fields.len(), 2),
                    other => panic!("expected nested structure, got {other:?}"),
                }
            }
            other => panic!("expected Structure, got {other:?}"),
        }
    }

    /// B8: JSON `null` is accepted as a const value (maps to
    /// `PvField::Null`), where it was previously rejected.
    #[test]
    fn parse_const_null_value() {
        use epics_pva_rs::pvdata::PvField;
        let json = r#"{
            "GRP:c": {
                "unset": { "+type": "const", "+value": null }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        assert!(
            matches!(m.const_value, Some(PvField::Null)),
            "JSON null const must map to PvField::Null, got {:?}",
            m.const_value
        );
    }

    /// B8: a `null` element inside a const array forces the
    /// `VariantArray` container (mixed kinds), not a rejection.
    #[test]
    fn parse_const_array_with_null_element() {
        use epics_pva_rs::pvdata::PvField;
        let json = r#"{
            "GRP:c": {
                "mixed": { "+type": "const", "+value": [1, null, 3] }
            }
        }"#;
        let groups = parse_group_config(json).unwrap();
        let m = &groups[0].members[0];
        match &m.const_value {
            Some(PvField::VariantArray(items)) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items[1].as_ref().unwrap().value, PvField::Null));
            }
            other => panic!("expected VariantArray with a Null element, got {other:?}"),
        }
    }

    #[test]
    fn parse_const_missing_value_is_error() {
        let json = r#"{
            "GRP:bad": {
                "label": {
                    "+type": "const"
                }
            }
        }"#;

        let result = parse_group_config(json);
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("+value"), "expected error about +value: {err}");
    }

    #[test]
    fn const_and_structure_default_trigger_none() {
        let json = r#"{
            "GRP:t": {
                "node": { "+type": "structure" },
                "fixed": { "+type": "const", "+value": 1 },
                "val": { "+channel": "R:val" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        let node = groups[0]
            .members
            .iter()
            .find(|m| m.field_name == "node")
            .unwrap();
        let fixed = groups[0]
            .members
            .iter()
            .find(|m| m.field_name == "fixed")
            .unwrap();
        assert!(matches!(node.triggers, TriggerDef::None));
        assert!(matches!(fixed.triggers, TriggerDef::None));
    }

    #[test]
    fn parse_nsecmask() {
        let json = r#"{
            "GRP:ns": {
                "val": {
                    "+channel": "R:val",
                    "+nsecmask": 255
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups[0].members[0].nsec_mask, 255);
    }

    #[test]
    fn nsecmask_defaults_to_zero() {
        let json = r#"{
            "GRP:ns": {
                "val": { "+channel": "R:val" }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups[0].members[0].nsec_mask, 0);
    }

    #[test]
    fn structure_ignores_channel() {
        // +channel on structure type should be silently ignored
        let json = r#"{
            "GRP:s": {
                "node": {
                    "+type": "structure",
                    "+channel": "SHOULD:IGNORE"
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert!(groups[0].members[0].channel.is_empty());
    }
}
