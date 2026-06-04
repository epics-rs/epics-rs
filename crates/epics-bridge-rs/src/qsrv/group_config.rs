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
    /// Resolved runtime atomicity. pvxs builds the group with
    /// `atomicPutGet = (tristate != False)` (groupconfigprocessor.cpp:436),
    /// so an unspecified `+atomic` resolves to atomic (`true`) — the
    /// default is applied here at materialization, not during parse/merge.
    pub atomic: bool,
    /// pvxs `atomicIsSet` presence bit (groupconfig.h:25). `true` iff some
    /// fragment explicitly carried `+atomic`. Used only by
    /// [`merge_group_defs`] so a later `+atomic`-less fragment cannot
    /// overwrite an earlier explicit setting; not consumed at runtime.
    pub atomic_is_set: bool,
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
    /// the non-group writer gap — a plain CA/PVA PUT to a
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
    /// pvxs defaults the missing-field sentinel to
    /// `i64::MIN` (`fieldconfig.h:37`) and treats that value as
    /// not-putable (`groupsource.cpp:503`). Wire parity therefore
    /// requires `Option<i64>` here, not a defaulted value — a
    /// member without an explicit `+putorder` must be silently
    /// dropped from the PUT ordering, NOT written under an
    /// implicit `0`. The width is the full pvxs `int64_t`
    /// (`fieldconfig.h:37`): a config may use `+putorder` values
    /// outside the `i32` range, and narrowing them would silently
    /// re-order record processing.
    pub put_order: Option<i64>,
    /// Optional structure ID for this member (from `+id`).
    pub struct_id: Option<String>,
    /// Constant value for `Const` mapping. Sourced from `+const`
    /// (pvxs canonical key, `test/qgroup.json`) with a legacy
    /// `+value` fallback for older Rust-authored configs.
    pub const_value: Option<epics_pva_rs::pvdata::PvField>,
}

impl GroupPvDef {
    /// true iff this is a *pure self-trigger* group — every
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

    /// resolve the provisional self-trigger default with
    /// group-level context, mirroring pvxs `resolveTriggerReferences`
    /// (`groupconfigprocessor.cpp:317-339`). pvxs applies the self-trigger
    /// fallback to every channeled field ONLY when the whole group
    /// declares no triggers (`!hasTriggers`); when any member carries an
    /// explicit `+trigger` (`"*"`/named), a member without one keeps an
    /// EMPTY trigger set (`defineTriggers`, `:300-309`) and posts nothing
    /// on its own change.
    ///
    /// `parse_member` cannot make this decision — it sees one member at a
    /// time. Resolve it here so a `SelfOnly` member can exist only in a
    /// pure self-trigger group (invariant held by construction), instead
    /// of leaking the dual meaning into the monitor mark path.
    ///
    /// Idempotent and monotonic: a group only gains members via
    /// [`merge_group_defs`], so "has an explicit trigger" never reverts;
    /// re-running after a merge can only demote more `SelfOnly` members.
    pub fn resolve_self_trigger_default(&mut self) {
        let has_explicit = self
            .members
            .iter()
            .any(|m| matches!(m.triggers, TriggerDef::All | TriggerDef::Fields(_)));
        if has_explicit {
            for m in &mut self.members {
                if matches!(m.triggers, TriggerDef::SelfOnly) {
                    m.triggers = TriggerDef::None;
                }
            }
        }
    }
}

/// Defines which group fields are updated when a member's source record changes.
#[derive(Debug, Clone)]
pub enum TriggerDef {
    /// `"*"` — update all fields in the group.
    All,
    /// Named fields — update only these fields.
    Fields(Vec<String>),
    /// missing `+trigger` — pvxs default is self-trigger
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

/// Normalize the relaxed JSON dialect that upstream QSRV accepts into
/// the strict JSON `serde_json` requires.
///
/// pva2pva enables YAJL `allow_comments` for both external group files
/// and record `info(Q:group, ...)` bodies (pva2pva
/// `pdbApp/configparse.cpp:224-254`), and EPICS-base db-file
/// `info(Q:group, ...)` bodies additionally use unquoted `+`-prefixed
/// option keys — e.g. `{+channel:"VAL", +putorder:0}` (pva2pva
/// `testApp/testpdb-groups.db:4-5`) and `+id`/`+type`/`+trigger`
/// (`iocBoot/iocimagedemo/image.db:38-44`). The shipped `image.json`
/// example also opens with a C-style block comment
/// (`iocBoot/iocimagedemo/image.json:1`). All of these are valid for the
/// reference parser but rejected by strict `serde_json`, so the same
/// configuration text that loads under pva2pva fails to load here before
/// any group semantics are reached.
///
/// Two string-literal-aware transformations (quoted payloads such as a
/// channel value `"a/*b*/c"` or `"+notakey"` are never rewritten):
///   1. replace `/* block */` and `// line` comments with whitespace;
///   2. wrap bare `+ident` object keys in double quotes.
///
/// YAJL treats a comment as a single run of whitespace, so it SEPARATES
/// the tokens on either side and an unterminated `/* ... */` is a parse
/// error. The comment is therefore replaced with whitespace rather than
/// deleted (deleting it with no separator would splice `1/*x*/2` into the
/// single token `12`, turning input YAJL rejects into a different valid
/// document), and an unterminated block comment returns an error instead
/// of being silently stripped to EOF (`R0604-BRQSRV-GROUP-JSON-COMMENT-CONCAT-1`).
///
/// Group names and member field names are always quoted in the upstream
/// dialect, so only `+`-prefixed option keys are ever unquoted; this
/// keeps the transform minimal rather than guessing at arbitrary bare
/// keys. The existing typed validation runs unchanged on the parsed
/// result, so parser leniency never hides an invalid group field.
fn normalize_relaxed_group_json(src: &str) -> BridgeResult<String> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            // String literal: copy verbatim, honoring backslash escapes,
            // so comment markers or `+` inside a quoted value are untouched.
            '"' => {
                out.push(c);
                i += 1;
                while i < n {
                    let d = chars[i];
                    out.push(d);
                    i += 1;
                    if d == '\\' {
                        if i < n {
                            out.push(chars[i]);
                            i += 1;
                        }
                    } else if d == '"' {
                        break;
                    }
                }
            }
            // Block comment `/* ... */`. YAJL treats it as whitespace, so it
            // SEPARATES the surrounding tokens; emit one space in its place
            // and fail on a `/*` that never closes (matching YAJL, which
            // reports a parse error rather than accepting the truncated text).
            '/' if i + 1 < n && chars[i + 1] == '*' => {
                i += 2;
                let mut closed = false;
                while i + 1 < n {
                    if chars[i] == '*' && chars[i + 1] == '/' {
                        i += 2;
                        closed = true;
                        break;
                    }
                    i += 1;
                }
                if !closed {
                    return Err(BridgeError::GroupConfigError(
                        "unterminated block comment '/* ... */' in QSRV group JSON".into(),
                    ));
                }
                out.push(' ');
            }
            // Line comment `// ...`.
            '/' if i + 1 < n && chars[i + 1] == '/' => {
                i += 2;
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            // Bare `+ident` option key → `"+ident"`. Only quoted when an
            // identifier follows and the next non-space token is `:` (an
            // object-key position); otherwise emitted verbatim — the
            // dialect uses a leading `+` exclusively for option keys, so
            // the else branch is defensive.
            '+' => {
                let mut j = i + 1;
                while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let mut k = j;
                while k < n && chars[k].is_whitespace() {
                    k += 1;
                }
                if j > i + 1 && k < n && chars[k] == ':' {
                    out.push('"');
                    out.extend(chars[i..j].iter());
                    out.push('"');
                    i = j;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Parse group definitions from a JSON string.
///
/// The JSON is a top-level object where each key is a group name.
pub fn parse_group_config(json: &str) -> BridgeResult<Vec<GroupPvDef>> {
    let normalized = normalize_relaxed_group_json(json)?;
    let root: HashMap<String, RawGroupDef> = serde_json::from_str(&normalized)
        .map_err(|e| BridgeError::GroupConfigError(e.to_string()))?;

    let mut groups = Vec::new();
    for (name, raw) in root {
        // pvxs validates groups one by one and catches per-group
        // exceptions, printing "ignoring invalid group" while preserving
        // sibling groups (ioc/groupconfigprocessor.cpp:128-163, :170-201).
        // A semantic error in one group must not hide valid siblings in
        // the same file; the JSON *syntax* error above still rejects the
        // whole file (matching the strict-parse boundary).
        match raw_to_group_def(name.clone(), raw) {
            Ok(def) => groups.push(def),
            Err(e) => tracing::warn!(group = %name, error = %e, "ignoring invalid QSRV group"),
        }
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
    let normalized = normalize_relaxed_group_json(json)?;
    let root: HashMap<String, RawGroupDef> = serde_json::from_str(&normalized)
        .map_err(|e| BridgeError::GroupConfigError(e.to_string()))?;

    let mut groups = Vec::new();
    for (name, raw) in root {
        // Per-group recovery, as in `parse_group_config` — one invalid
        // group in an info(Q:group) body must not drop its siblings
        // (pvxs groupconfigprocessor.cpp:128-163).
        let mut def = match raw_to_group_def(name.clone(), raw) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(group = %name, error = %e, "ignoring invalid QSRV info(Q:group) group");
                continue;
            }
        };
        // pvxs prefixes EVERY `+channel` in a record's info(Q:group) with
        // `"{record}."` — `channelPrefix = dbRecordName + "."` is applied
        // unconditionally (groupprocessorcontext.cpp:65-66,
        // groupconfigprocessor.cpp:810-818); it does NOT skip values that
        // already contain ':' or '.'. Record-info group JSON is always
        // record-relative; absolute PV names are reachable only through the
        // file-based dbLoadGroup path (the empty-prefix case). The earlier
        // ':'/'.' scan let an info(Q:group) member reference an unrelated
        // absolute PV that pvxs would not model. Skip only the channel-less
        // Structure/Const members (empty channel).
        for member in &mut def.members {
            if !member.channel.is_empty() {
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
            // Merge members, keyed by group field name. pvxs holds one
            // field per group field name (`groupDefinition.fieldMap`,
            // groupdefinition.h:36-37); when a later fragment maps a field
            // name already defined, `defineFields` warns "ignoring
            // duplicate mapping" and skips it (groupconfigprocessor.cpp:
            // 221-225) — first definition wins. A blind `extend` instead
            // kept BOTH members, so GET composed the field twice and an
            // atomic PUT could drive two backing writes for one client
            // field. Skip duplicates here so the runtime group has exactly
            // one member per field name, matching pvxs.
            let mut seen: std::collections::HashSet<String> = existing_def
                .members
                .iter()
                .map(|m| m.field_name.clone())
                .collect();
            for member in def.members {
                if !seen.insert(member.field_name.clone()) {
                    eprintln!(
                        "warning: group '{}': ignoring duplicate mapping for field '{}'",
                        def.name, member.field_name
                    );
                    continue;
                }
                existing_def.members.push(member);
            }
            // re-sort the combined member list — the merge appends
            // a second source's members, so the canonical (put_order,
            // field_name) order must be re-established over the union.
            sort_members_canonical(&mut existing_def.members);
            // the merge may have turned a previously pure
            // self-trigger group into a mixed-trigger group (a new member
            // carries `+trigger`). Re-resolve so any `SelfOnly` member is
            // demoted to silent, matching pvxs's group-level resolution.
            existing_def.resolve_self_trigger_default();
            // Update struct_id if newly specified (last wins)
            if def.struct_id.is_some() {
                existing_def.struct_id = def.struct_id;
            }
            // atomic: pvxs runs `defineAtomicity` ONLY when the incoming
            // fragment explicitly set `+atomic` (groupconfigprocessor.cpp:
            // 194 gates on `atomicIsSet`); a fragment that omits `+atomic`
            // leaves the merged TriState untouched. The conflict warning
            // fires only when the group's atomicity was ALREADY explicitly
            // set and the new explicit value differs (`atomic != Unset &&
            // atomic != atomicity`, :279). A plain last-wins assignment let
            // an `+atomic`-less later fragment revert an earlier explicit
            // `+atomic:false`.
            if def.atomic_is_set {
                if existing_def.atomic_is_set && existing_def.atomic != def.atomic {
                    eprintln!(
                        "warning: group '{}' atomic setting inconsistent, using latest ({})",
                        def.name, def.atomic
                    );
                }
                existing_def.atomic = def.atomic;
                existing_def.atomic_is_set = true;
            }
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
    // pvxs assigns `+id` with `value.as<std::string>()`
    // (groupprocessorcontext.cpp:33), which coerces a bool / integer /
    // real / string JSON scalar to its string form (data.cpp:402-461).
    // Capturing the raw `serde_json::Value` (not `Option<String>`) keeps a
    // non-string `+id` from failing the whole-file `serde_json::from_str`;
    // the `as<string>` coercion and its NoConvert/per-group-skip outcome
    // run in `raw_to_group_def`, inside the per-group recovery boundary.
    #[serde(rename = "+id", default)]
    id: Option<serde_json::Value>,
    // pvxs tracks `+atomic` as a value plus a presence bit
    // (`groupconfig.h:24-31`: `atomic` defaults true, `atomicIsSet`
    // defaults false; groupprocessorcontext.cpp:27-30 sets the bit only
    // when the JSON actually carries `+atomic`) and assigns it with
    // `value.as<bool>()`, coercing bool / integer / unsigned / real
    // (nonzero ⇒ true) and the exact strings "true"/"false"
    // (data.cpp:402-461). Capturing the raw `serde_json::Value` (not
    // `Option<bool>`) keeps a numeric/real/string `+atomic` from failing
    // the whole-file parse and preserves the presence bit: `None` == not
    // specified by THIS fragment, so a later fragment that omits `+atomic`
    // must not clobber an earlier explicit setting during merge. The
    // `as<bool>` coercion and its NoConvert/per-group-skip outcome run in
    // `raw_to_group_def`.
    #[serde(rename = "+atomic", default)]
    atomic: Option<serde_json::Value>,
    #[serde(flatten)]
    fields: HashMap<String, serde_json::Value>,
}

/// canonical group-member ordering — `put_order` primary,
/// `field_name` secondary. `put_order` is `Option<i64>`; `None`
/// (no `+putorder`, "not putable") sorts before any `Some`, matching
/// pvxs's `i64::MIN` sentinel ordering (`fieldconfig.h:37`). The
/// field-name tiebreak makes the order a pure function of the config,
/// independent of the HashMap iteration order the members were collected
/// in.
fn sort_members_canonical(members: &mut [GroupMember]) {
    members.sort_by(|a, b| {
        a.put_order
            .cmp(&b.put_order)
            .then_with(|| a.field_name.cmp(&b.field_name))
    });
}

/// Coerce a JSON `+atomic` value to a bool exactly as pvxs
/// `Value::as<bool>` coerces the parsed group-config scalar
/// (groupprocessorcontext.cpp:29 → data.cpp:402-461): a JSON bool maps
/// directly; a JSON number is true when nonzero (integer and real alike,
/// matching `copyOutScalar`'s `bool(src)` cast); a JSON string accepts
/// only the exact tokens `"true"`/`"false"` (no trim, case-sensitive).
/// Any other value (other string, array, object, null) is unconvertible
/// — `None` mirrors pvxs's NoConvert, which the caller turns into a
/// per-group skip. JSON-value sibling of [`crate::qsrv::channel`]'s
/// `scalar_as_bool`, which applies the same `as<bool>` rule to a runtime
/// pvRequest `ScalarValue`.
fn json_value_as_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i != 0)
            } else if let Some(u) = n.as_u64() {
                Some(u != 0)
            } else {
                n.as_f64().map(|f| f != 0.0)
            }
        }
        serde_json::Value::String(s) => match s.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Coerce a JSON `+id` value to a string exactly as pvxs
/// `Value::as<std::string>()` coerces the parsed scalar
/// (groupprocessorcontext.cpp:33 → data.cpp:402-461): a JSON string maps
/// directly; a JSON bool becomes `"true"`/`"false"` (the Bool→String
/// store branch, data.cpp:436); a JSON number becomes its base-10 form
/// (`copyOutScalar`'s `SB()<<src`, data.cpp:409). Any other value (array,
/// object, null) is unconvertible — `None` mirrors pvxs's NoConvert,
/// which the caller turns into a per-group skip.
fn json_value_as_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn raw_to_group_def(name: String, raw: RawGroupDef) -> BridgeResult<GroupPvDef> {
    let mut members = Vec::new();

    for (field_name, value) in &raw.fields {
        // Skip meta-keys (already extracted via named fields)
        if field_name.starts_with('+') {
            continue;
        }

        let member = parse_member(field_name, value)?;
        // pvxs `defineFields` only permits an empty top-level field name
        // for a metadata mapping: `+type:"meta"` merges alarm/timeStamp at
        // the struct root, every other mapping must be named
        // (groupconfigprocessor.cpp:215-231 logs `only +type:"meta" can be
        // mapped at struct top` and skips that field). Without this guard
        // an empty-key scalar/plain/any/const/structure/proc member was
        // accepted, then reached `set_nested_field` with an empty path
        // (group.rs) and silently produced no runtime value AND no
        // descriptor member — a misconfigured group that loaded clean.
        if member.field_name.is_empty() && member.mapping != FieldMapping::Meta {
            eprintln!(
                "warning: group '{}': only +type:\"meta\" can be mapped at struct top; ignoring empty-named field with mapping {:?}",
                name, member.mapping
            );
            continue;
        }
        members.push(member);
    }

    // total (put_order, field_name) order. The `fields` source is
    // a `#[serde(flatten)]` HashMap (randomized iteration), so a
    // put_order-only stable sort left equal-put_order members (the common
    // case — only writable members carry +putorder) in arbitrary order,
    // making the wire field/bit layout non-deterministic. pvxs derives a
    // deterministic putOrder-then-name order from a name-sorted std::map +
    // stable_sort (groupconfig.h:28, groupconfigprocessor.cpp:253-262); the
    // field-name tiebreak reproduces it independent of HashMap order.
    sort_members_canonical(&mut members);

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
                    // pvxs `defineGroupTriggers` (groupconfigprocessor.cpp:
                    // 396-397) logs the bad reference and `continue`s,
                    // dropping just this trigger target — the group and
                    // every sibling group still load. Do NOT fail the
                    // whole config. The unknown ref is then filtered out
                    // at runtime in `value_event_mark`.
                    eprintln!(
                        "Error: group '{}': member '{}' defines trigger to nonexistent field '{}' (ignored)",
                        name, member.field_name, target
                    );
                    continue;
                }
                // A channel-less target (Structure/Const) is dropped too —
                // pvxs ignores these (groupconfigprocessor.cpp:405-407).
                if !channeled_names.contains(target.as_str()) {
                    eprintln!(
                        "warning: group '{}': trigger '{}' on member '{}' targets a field without a channel (ignored)",
                        name, target, member.field_name
                    );
                }
            }
        }
    }

    // pvxs assigns `+atomic` via `value.as<bool>()` and `+id` via
    // `value.as<std::string>()` (groupprocessorcontext.cpp:29,33). A value
    // that cannot coerce raises NoConvert, which pvxs catches per group —
    // the bad group is dropped and its siblings survive
    // (groupconfigprocessor.cpp:128-163). Run the coercion here, inside the
    // per-group recovery boundary, so a numeric/real/string `+atomic` (or a
    // non-string `+id`) no longer fails the whole-file `serde_json::from_str`.
    // pvxs assigns `+atomic` via `value.as<bool>()` and `+id` via
    // `value.as<std::string>()` (groupprocessorcontext.cpp:29,33). A value
    // that cannot coerce raises NoConvert, which pvxs catches per group —
    // the bad group is dropped and its siblings survive
    // (groupconfigprocessor.cpp:128-163). Run the coercion here, inside the
    // per-group recovery boundary, so a numeric/real/string `+atomic` (or a
    // non-string `+id`) no longer fails the whole-file `serde_json::from_str`.
    let (atomic, atomic_is_set) = match &raw.atomic {
        // Unset: pvxs's runtime rule `atomic != False`
        // (groupconfigprocessor.cpp:436) resolves atomic (`true`) but
        // leaves the presence bit clear.
        None => (true, false),
        Some(v) => match json_value_as_bool(v) {
            Some(b) => (b, true),
            None => {
                return Err(BridgeError::GroupConfigError(format!(
                    "group '{name}': +atomic value {v} is not a bool, number, or \"true\"/\"false\""
                )));
            }
        },
    };
    let struct_id = match &raw.id {
        None => None,
        Some(v) => match json_value_as_string(v) {
            Some(s) => Some(s),
            None => {
                return Err(BridgeError::GroupConfigError(format!(
                    "group '{name}': +id value {v} is not a string, number, or bool"
                )));
            }
        },
    };

    let mut def = GroupPvDef {
        name,
        struct_id,
        atomic,
        atomic_is_set,
        members,
        atomic_write_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    };
    // resolve the per-member self-trigger default now that every
    // member of this (single-source) group is known. Re-run after merge.
    def.resolve_self_trigger_default();
    Ok(def)
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
            // pvxs logs an unknown mapping +type and keeps the default
            // (scalar) mapping rather than rejecting the config
            // (ioc/groupprocessorcontext.cpp:43-63; the default mapping
            // type is Scalar, fieldconfig.h:24-37). Warn and fall back to
            // Scalar — the normal +channel validation below then decides
            // whether the member (and so the group) is usable.
            tracing::warn!(
                field = field_name,
                bad_type = other,
                "unknown QSRV group member +type; defaulting to scalar"
            );
            FieldMapping::Scalar
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
    // pvxs uses `+const` (test/qgroup.json:1, test/const.db:2);
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
            // pvxs groupconfigprocessor.cpp:323 defaults a
            // missing `+trigger` to self-trigger (only this member's
            // own field re-emits in the group), not All. The Rust
            // path treated None as All and emitted a full-group
            // changed bitset on every member event — distinct from
            // pvxs's narrow self-trigger delta visible in
            // testqgroup.cpp:220 (NTEnum group: only `value.index`
            // bit set on a VAL update).
            // this per-member default is only provisional. pvxs
            // applies the self-trigger fallback at the GROUP level and only
            // when the whole group declares no triggers
            // (`groupconfigprocessor.cpp:317-339`). In a group where any
            // member has an explicit `+trigger`, a no-`+trigger` member is
            // silent. `GroupPvDef::resolve_self_trigger_default` demotes
            // `SelfOnly` → `None` for such mixed groups after all members
            // are assembled.
            // An explicit `+trigger:""` is NOT distinct from a missing
            // `+trigger`: pvxs stores both as the empty string
            // (`fieldconfig.h:50-54`) and sets `hasTriggers` only for a
            // NON-empty trigger string (`groupconfigprocessor.cpp:297-309`).
            // So an empty trigger gets the same provisional self-trigger
            // default and is resolved at group scope — a one-member
            // `"+trigger":""` group, or an all-empty group, still
            // self-triggers (`:317-339`). Materializing `None` here made an
            // explicit `""` permanent silence even with no non-empty sibling
            // trigger, diverging from pvxs. `None` (silence) is produced
            // only by `resolve_self_trigger_default` (mixed groups) and for
            // channel-less Structure/Const members above.
            None | Some("") => TriggerDef::SelfOnly,
            // pvxs `defineTriggers` (groupconfigprocessor.cpp:299-309)
            // splits a non-empty `+trigger` with `std::getline(.,',')` and
            // inserts each substring VERBATIM — it does not trim whitespace.
            // Trigger resolution (`:394-408`) then looks each name up exactly
            // in `fieldMap`, so `"a, b"` keeps the target `" b"` and reports
            // it as a nonexistent field rather than triggering `b`. Trimming
            // here would make Rust trigger `b`, diverging from pvxs and
            // changing the group's changed-bitset/monitor fanout.
            // Regression R0604-QSRV-TRIGGER-WHITESPACE-1.
            Some(s) => TriggerDef::Fields(s.split(',').map(|f| f.to_string()).collect()),
        }
    };

    // pvxs reads `+putorder` as the full `int64_t`
    // (groupprocessorcontext.cpp:74-78); when the explicit value equals
    // the absent-sentinel `i64::MIN` it increments to `i64::MIN + 1` so
    // an explicit minimum order is never confused with "no +putorder".
    // We keep the value at full width and apply the same sentinel bump.
    let put_order = obj
        .get("+putorder")
        .and_then(|v| v.as_i64())
        .map(|n| if n == i64::MIN { i64::MIN + 1 } else { n });

    let struct_id = obj
        .get("+id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(GroupMember {
        field_name: field_name.to_string(),
        channel,
        mapping,
        triggers,
        put_order,
        struct_id,
        const_value,
    })
}

/// Convert a JSON value to a PvField for a Const mapping.
///
/// pvxs's group JSON parser can only assign a SCALAR or `null` to a
/// `+const`. It registers no array callbacks — "arrays are not used"
/// (groupconfigprocessor.cpp:772-790) — and rejects object nesting below
/// a field-definition object with "Group field def. can't contain Object
/// (too deep)" (:733-739). The const template then builds a `TypeDef`
/// directly from the parsed scalar/null `Value` (:596-604).
///
/// The earlier Rust port accepted the full recursive pvData space
/// (scalar/structure/variant arrays and nested objects). Those forms
/// produced const descriptors and payloads that pvxs rejects at IOC
/// startup, so a group authored against Rust was not portable upstream.
/// Restrict const values to the node kinds pvxs's parser can actually
/// assign; an array or nested object is a hard error so the group is
/// skipped (per-group recovery), matching pvxs's startup rejection.
///
/// * Scalars (`bool`/number/string) map to [`PvField::Scalar`].
/// * `null` maps to [`PvField::Null`] (pvxs accepts an empty/unset const).
fn json_to_pv_field(v: &serde_json::Value) -> Result<epics_pva_rs::pvdata::PvField, String> {
    use epics_pva_rs::pvdata::{PvField, ScalarValue};
    match v {
        serde_json::Value::Bool(b) => Ok(PvField::Scalar(ScalarValue::Boolean(*b))),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                // pvxs builds a JSON integer const as `TypeDef(TypeCode::
                // Int64)` and assigns the full `int64_t`
                // (groupconfigprocessor.cpp:680-686); the group field
                // descriptor is then derived from that const's type
                // (596-603), so a JSON integer const is a PVA `long`, not
                // `int`. The prior `i as i32` truncated large constants
                // (version IDs, table labels) and advertised the wrong
                // field type. Keep the full width as `Long`.
                Ok(PvField::Scalar(ScalarValue::Long(i)))
            } else if let Some(f) = n.as_f64() {
                Ok(PvField::Scalar(ScalarValue::Double(f)))
            } else {
                Err(format!("unsupported number: {n}"))
            }
        }
        serde_json::Value::String(s) => Ok(PvField::Scalar(ScalarValue::String(s.clone().into()))),
        serde_json::Value::Null => Ok(PvField::Null),
        serde_json::Value::Array(_) => Err(
            "group +const cannot be an array: pvxs registers no array callbacks \
             (groupconfigprocessor.cpp:772-790)"
                .to_string(),
        ),
        serde_json::Value::Object(_) => Err(
            "group +const cannot be a nested object: pvxs rejects object nesting below a \
             field definition (groupconfigprocessor.cpp:733-739)"
                .to_string(),
        ),
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

    /// A semantically invalid group (here: a plain member with no
    /// +channel) is skipped with a warning while valid sibling groups in
    /// the same JSON survive — pvxs groupconfigprocessor.cpp:128-163.
    #[test]
    fn parse_group_config_skips_invalid_group_keeps_siblings() {
        let json = r#"{
            "BADGRP": { "v": { "+type": "plain" } },
            "OKGRP":  { "v": { "+channel": "X.VAL", "+type": "plain" } }
        }"#;
        let defs = parse_group_config(json).expect("syntactically valid JSON must parse");
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["OKGRP"],
            "the valid sibling must survive a bad group"
        );
    }

    /// An unknown member +type warns and falls back to the default
    /// scalar mapping rather than aborting the load — pvxs
    /// groupprocessorcontext.cpp:43-63.
    #[test]
    fn parse_member_unknown_type_defaults_to_scalar() {
        let json = r#"{ "G": { "v": { "+channel": "X.VAL", "+type": "bogus" } } }"#;
        let defs = parse_group_config(json).expect("unknown +type must not abort the load");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].members[0].mapping, FieldMapping::Scalar);
    }

    /// A syntactically malformed JSON body still rejects the whole file
    /// (the strict-parse boundary is preserved). The relaxed-JSON
    /// normalizer must not turn invalid input into something that parses.
    #[test]
    fn parse_group_config_rejects_malformed_json() {
        assert!(parse_group_config("{ not json").is_err());
    }

    /// pva2pva accepts EPICS db-file `info(Q:group, ...)` bodies that
    /// carry unquoted `+`-prefixed option keys — pva2pva
    /// `testApp/testpdb-groups.db:4-5`
    /// (`{+channel:"VAL", +putorder:0}`) and `image.db:38-44`
    /// (`+id`/`+type`/`+trigger`). The relaxed-JSON normalizer must quote
    /// them so the same text loads here.
    #[test]
    fn parse_group_config_accepts_unquoted_plus_keys() {
        let json = r#"{
            "grp1": {
                +id: "epics:nt/NTGroup:1.0",
                +atomic: true,
                "fld1": {+channel:"VAL", +putorder:0},
                "fld2": {+channel:"RVAL", +trigger:"fld1,fld2"}
            }
        }"#;
        let defs = parse_group_config(json).expect("unquoted +keys must parse");
        assert_eq!(defs.len(), 1);
        let g = &defs[0];
        assert_eq!(g.name, "grp1");
        assert_eq!(g.struct_id.as_deref(), Some("epics:nt/NTGroup:1.0"));
        assert!(g.atomic);
        let f1 = g.members.iter().find(|m| m.field_name == "fld1").unwrap();
        assert_eq!(f1.channel, "VAL");
        assert_eq!(f1.put_order, Some(0));
        let f2 = g.members.iter().find(|m| m.field_name == "fld2").unwrap();
        assert_eq!(f2.channel, "RVAL");
        match &f2.triggers {
            TriggerDef::Fields(t) => assert_eq!(t, &vec!["fld1".to_string(), "fld2".to_string()]),
            other => panic!("expected named trigger fields, got {other:?}"),
        }
    }

    /// Regression R0604-QSRV-TRIGGER-WHITESPACE-1: pvxs `defineTriggers`
    /// (groupconfigprocessor.cpp:299-309) splits `+trigger` on commas with
    /// `std::getline` and inserts each substring VERBATIM — no trim. So
    /// `"a, b"` yields targets `a` and `" b"`, and exact `fieldMap`
    /// resolution drops `" b"` as a nonexistent field rather than triggering
    /// `b`. The earlier `.trim()` collapsed `" b"` to `b`, diverging from
    /// pvxs and silently widening the group's trigger fanout.
    #[test]
    fn trigger_targets_preserve_whitespace_like_pvxs() {
        // Spaces after the comma are part of the target name (verbatim),
        // matching pvxs `getline`. `" b"` must NOT collapse to `b`.
        let json = r#"{
            "G": {
                "a": {"+channel": "A.VAL"},
                "b": {"+channel": "B.VAL"},
                "c": {"+channel": "C.VAL", "+trigger": "a, b"}
            }
        }"#;
        let defs = parse_group_config(json).expect("must parse");
        let c = defs[0]
            .members
            .iter()
            .find(|m| m.field_name == "c")
            .expect("member c");
        match &c.triggers {
            TriggerDef::Fields(t) => assert_eq!(
                t,
                &vec!["a".to_string(), " b".to_string()],
                "pvxs keeps the post-comma space; ' b' must not collapse to 'b'"
            ),
            other => panic!("expected named trigger fields, got {other:?}"),
        }

        // No spaces → exact field names, unaffected by the no-trim change.
        let json_tight = r#"{
            "G": {
                "a": {"+channel": "A.VAL"},
                "b": {"+channel": "B.VAL"},
                "c": {"+channel": "C.VAL", "+trigger": "a,b"}
            }
        }"#;
        let defs = parse_group_config(json_tight).expect("must parse");
        let c = defs[0]
            .members
            .iter()
            .find(|m| m.field_name == "c")
            .expect("member c");
        match &c.triggers {
            TriggerDef::Fields(t) => {
                assert_eq!(t, &vec!["a".to_string(), "b".to_string()])
            }
            other => panic!("expected named trigger fields, got {other:?}"),
        }
    }

    /// pva2pva enables YAJL `allow_comments` (pva2pva
    /// `configparse.cpp:231/249`); the shipped `image.json` example opens
    /// with a C-style block comment (`image.json:1`). Both `/* */` and
    /// `//` comments must be stripped before strict parsing.
    #[test]
    fn parse_group_config_strips_comments() {
        let json = r#"/* leading block comment, as in image.json:1 */
        {
            // line comment
            "grp": {
                "v": { "+channel": "X.VAL" /* trailing */ }
            }
        }"#;
        let defs = parse_group_config(json).expect("comments must be stripped");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].members[0].channel, "X.VAL");
    }

    /// The normalizer is string-literal aware: comment markers and a
    /// leading `+` inside a quoted value must survive verbatim, never
    /// mistaken for a comment or a bare key.
    #[test]
    fn parse_group_config_preserves_comment_markers_inside_strings() {
        let json = r#"{
            "grp": {
                "v": { "+channel": "A/*not a comment*/B//x", "+id": "+literal" }
            }
        }"#;
        let defs = parse_group_config(json).expect("strings with markers must parse");
        assert_eq!(defs[0].members[0].channel, "A/*not a comment*/B//x");
    }

    /// Regression R0604-BRQSRV-GROUP-JSON-COMMENT-CONCAT-1: YAJL treats a
    /// comment as whitespace (`allow_comments`, configparse.cpp:231/249), so
    /// it SEPARATES the tokens it stands between. A block comment wedged
    /// between two numbers (`1/*x*/2`) leaves two number tokens where a comma
    /// or `}` is required, which YAJL rejects. Deleting the comment with no
    /// separator spliced them into the single token `12`, turning input YAJL
    /// rejects into a different valid document.
    #[test]
    fn group_json_block_comment_separates_tokens() {
        let json = r#"{ "g": { "f": {+channel:"VAL", +putorder:1/*typo*/2} } }"#;
        assert!(
            parse_info_group("rec", json).is_err(),
            "block comment must separate 1 and 2, leaving invalid JSON (not splice into 12)"
        );
        // The same comment placed where a separator is legal still parses, and
        // the number before it keeps its value (`1`, not `12`).
        let json = r#"{ "g": { "f": {+channel:"VAL", +putorder:1/*typo*/, +type:"plain"} } }"#;
        let groups = parse_info_group("rec", json).expect("comment before ',' is valid");
        assert_eq!(
            groups[0].members[0].put_order,
            Some(1),
            "putorder is 1, not 12"
        );
    }

    /// Regression R0604-BRQSRV-GROUP-JSON-COMMENT-CONCAT-1: an unterminated
    /// `/* ... */` is a YAJL parse error, not silently stripped to EOF. The
    /// previous normalizer ran the strip past the end and accepted the
    /// preceding text if it happened to be valid.
    #[test]
    fn group_json_unterminated_block_comment_is_rejected() {
        let json = r#"{ "g": { "f": {+channel:"VAL"} } } /* never closed"#;
        assert!(
            parse_info_group("rec", json).is_err(),
            "unterminated block comment must be rejected, matching YAJL"
        );
        // Also rejected mid-document, before any otherwise-valid close.
        let json = r#"{ "g": { "f": {+channel:"VAL" /* unterminated } } }"#;
        assert!(parse_info_group("rec", json).is_err());
    }

    /// The relaxed normalizer applies to the record `info(Q:group, ...)`
    /// path too, not only standalone files — pvxs routes record info tags
    /// through the same relaxed parser, and the Rust bridge feeds them via
    /// `parse_info_group` (provider.rs).
    #[test]
    fn parse_info_group_accepts_unquoted_plus_keys() {
        let json = r#"{
            "grp1": {
                "fld1": {+channel:"VAL", +putorder:0}
            }
        }"#;
        let groups = parse_info_group("rec3", json).expect("unquoted +keys must parse");
        assert_eq!(groups.len(), 1);
        // bare field-name channel gets the record-name prefix applied.
        assert_eq!(groups[0].members[0].channel, "rec3.VAL");
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
        // pvxs default for a missing `+trigger` is self-trigger
        // (`groupconfigprocessor.cpp:323`), not All.
        assert!(matches!(m.triggers, TriggerDef::SelfOnly));
        // missing `+putorder` → `None` (not putable),
        // mirroring pvxs `fieldconfig.h:37` sentinel.
        assert_eq!(m.put_order, None);
    }

    /// `+putorder` is stored at full pvxs `int64_t` width
    /// (fieldconfig.h:37): a value outside the `i32` range must not wrap,
    /// and members order by the full `i64` value. Pre-fix the parser cast
    /// `n as i32`, silently re-ordering record processing.
    #[test]
    fn putorder_preserves_int64_width_and_orders_by_full_value() {
        let big = i32::MAX as i64 + 1; // 2147483648, wraps to negative as i32
        let json = format!(
            r#"{{ "G": {{
                "hi":  {{ "+channel": "R:hi.VAL",  "+putorder": {big} }},
                "lo":  {{ "+channel": "R:lo.VAL",  "+putorder": -5 }},
                "mid": {{ "+channel": "R:mid.VAL", "+putorder": 0 }}
            }} }}"#
        );
        let defs = parse_group_config(&json).unwrap();
        let g = &defs[0];
        let po = |name: &str| {
            g.members
                .iter()
                .find(|m| m.field_name == name)
                .unwrap()
                .put_order
        };
        assert_eq!(po("hi"), Some(big), "i32::MAX+1 must not wrap to negative");
        assert_eq!(po("lo"), Some(-5));
        // Canonical order is by full i64 put_order: lo(-5), mid(0), hi(big).
        let names: Vec<&str> = g.members.iter().map(|m| m.field_name.as_str()).collect();
        assert_eq!(names, vec!["lo", "mid", "hi"]);
    }

    /// An explicit `+putorder` equal to the absent-sentinel `i64::MIN` is
    /// bumped to `i64::MIN + 1` so it stays distinct from "no +putorder"
    /// (`None`), matching pvxs `groupprocessorcontext.cpp:74-78`.
    #[test]
    fn putorder_explicit_min_is_bumped_off_the_sentinel() {
        let json = format!(
            r#"{{ "G": {{ "x": {{ "+channel": "R:x.VAL", "+putorder": {} }} }} }}"#,
            i64::MIN
        );
        let defs = parse_group_config(&json).unwrap();
        assert_eq!(defs[0].members[0].put_order, Some(i64::MIN + 1));
    }

    /// A group whose members all use the default
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

    /// in a group where any member carries an explicit
    /// `+trigger`, a member WITHOUT one is silent in pvxs (empty trigger
    /// set), not self-triggering. The provisional `SelfOnly` default must
    /// be demoted to `None` for such mixed groups; a pure self-trigger
    /// group keeps `SelfOnly`.
    #[test]
    fn br_r58_mixed_trigger_group_demotes_self_to_silent() {
        // Mixed group: `a` no trigger, `b` explicit "*".
        let mixed = parse_group_config(
            r#"{ "GRP:mixed": {
                "a": {"+channel": "R:a"},
                "b": {"+channel": "R:b", "+trigger": "*"}
            }}"#,
        )
        .unwrap();
        let a = mixed[0]
            .members
            .iter()
            .find(|m| m.field_name == "a")
            .unwrap();
        assert!(
            matches!(a.triggers, TriggerDef::None),
            "no-+trigger member in a mixed group must be silent (None), got {:?}",
            a.triggers
        );

        // Mixed group with named trigger: `x` no trigger, `y` -> "x".
        let named = parse_group_config(
            r#"{ "GRP:named": {
                "x": {"+channel": "R:x"},
                "y": {"+channel": "R:y", "+trigger": "x"}
            }}"#,
        )
        .unwrap();
        let x = named[0]
            .members
            .iter()
            .find(|m| m.field_name == "x")
            .unwrap();
        assert!(
            matches!(x.triggers, TriggerDef::None),
            "no-+trigger member in a named-trigger group must be silent (None)"
        );

        // Pure group: both default -> both keep SelfOnly.
        let pure = parse_group_config(
            r#"{ "GRP:pure2": {
                "a": {"+channel": "R:a"},
                "b": {"+channel": "R:b"}
            }}"#,
        )
        .unwrap();
        for m in &pure[0].members {
            assert!(
                matches!(m.triggers, TriggerDef::SelfOnly),
                "pure self-trigger group must keep SelfOnly for member {}",
                m.field_name
            );
        }
    }

    /// pvxs treats an explicit `+trigger:""` identically to a missing
    /// `+trigger`: when a group has NO non-empty trigger string, every
    /// channeled member self-triggers (groupconfigprocessor.cpp:317-339).
    /// A one-member `""` group and an all-empty multi-member group must
    /// both self-trigger; an empty `""` alongside a non-empty sibling
    /// stays silent.
    #[test]
    fn br_64_empty_trigger_self_triggers_without_nonempty_sibling() {
        // One member, explicit empty trigger → self-trigger (not silent).
        let one =
            parse_group_config(r#"{ "GRP:e1": { "a": {"+channel": "R:a", "+trigger": ""} } }"#)
                .unwrap();
        assert!(
            matches!(one[0].members[0].triggers, TriggerDef::SelfOnly),
            "single empty-trigger member must self-trigger, got {:?}",
            one[0].members[0].triggers
        );
        assert!(one[0].is_pure_self_trigger());

        // All-empty two-member group (one explicit "", one missing) → both
        // self-trigger.
        let all_empty = parse_group_config(
            r#"{ "GRP:e2": {
                "a": {"+channel": "R:a", "+trigger": ""},
                "b": {"+channel": "R:b"}
            }}"#,
        )
        .unwrap();
        for m in &all_empty[0].members {
            assert!(
                matches!(m.triggers, TriggerDef::SelfOnly),
                "all-empty group member {} must self-trigger, got {:?}",
                m.field_name,
                m.triggers
            );
        }

        // Mixed group: explicit "" alongside a non-empty trigger → the ""
        // member is silent (pvxs empty-trigger inside a hasTriggers group).
        let mixed = parse_group_config(
            r#"{ "GRP:e3": {
                "a": {"+channel": "R:a", "+trigger": ""},
                "b": {"+channel": "R:b", "+trigger": "*"}
            }}"#,
        )
        .unwrap();
        let a = mixed[0]
            .members
            .iter()
            .find(|m| m.field_name == "a")
            .unwrap();
        assert!(
            matches!(a.triggers, TriggerDef::None),
            "explicit empty trigger alongside a non-empty sibling stays silent, got {:?}",
            a.triggers
        );
    }

    /// the same demotion must happen when a group is assembled
    /// across multiple sources via `merge_group_defs` — a later source
    /// adding an explicit-trigger member demotes the earlier no-trigger
    /// member.
    #[test]
    fn br_r58_merge_demotes_self_to_silent() {
        let mut existing = std::collections::HashMap::new();
        let first = parse_group_config(r#"{ "GRP:m": { "a": {"+channel": "R:a"} } }"#).unwrap();
        merge_group_defs(&mut existing, first);
        // After the first (pure) source, `a` is self-trigger.
        assert!(matches!(
            existing["GRP:m"].members[0].triggers,
            TriggerDef::SelfOnly
        ));

        let second =
            parse_group_config(r#"{ "GRP:m": { "b": {"+channel": "R:b", "+trigger": "*"} } }"#)
                .unwrap();
        merge_group_defs(&mut existing, second);
        let a = existing["GRP:m"]
            .members
            .iter()
            .find(|m| m.field_name == "a")
            .unwrap();
        assert!(
            matches!(a.triggers, TriggerDef::None),
            "merge that introduces an explicit-trigger member must demote the earlier no-trigger member to silent"
        );
    }

    /// An unspecified `+atomic` resolves to atomic (`true`) but leaves
    /// the presence bit clear — pvxs `atomicPutGet = (TriState != False)`
    /// with `atomicIsSet=false` (groupconfig.h:25, groupconfigprocessor
    /// .cpp:436).
    #[test]
    fn group_without_atomic_resolves_true_but_unset() {
        let g = &parse_group_config(r#"{ "G:d": { "a": { "+channel": "R.A" } } }"#).unwrap()[0];
        assert!(
            g.atomic,
            "omitted +atomic resolves atomic (TriState != False)"
        );
        assert!(
            !g.atomic_is_set,
            "presence bit must stay clear when +atomic omitted"
        );
    }

    /// Regression R0604-BRQSRV-GROUP-ATOMIC-CONFIG-1. pvxs assigns group
    /// `+atomic` with `value.as<bool>()` (groupprocessorcontext.cpp:29),
    /// so a numeric, real, or `"true"`/`"false"` scalar sets atomicity by
    /// nonzero truthiness (data.cpp:402-461). A numeric `+atomic` must
    /// parse, not fail the whole-file `serde_json::from_str`.
    #[test]
    fn config_atomic_numeric_and_string_coerce_like_as_bool() {
        // integer 0 / 1, real 0.0, exact strings "true"/"false".
        for (json, want, label) in [
            (
                r#"{ "G": { "+atomic": 0, "a": { "+channel": "R.A" } } }"#,
                false,
                "int 0",
            ),
            (
                r#"{ "G": { "+atomic": 1, "a": { "+channel": "R.A" } } }"#,
                true,
                "int 1",
            ),
            (
                r#"{ "G": { "+atomic": 2, "a": { "+channel": "R.A" } } }"#,
                true,
                "int 2 (nonzero)",
            ),
            (
                r#"{ "G": { "+atomic": 0.0, "a": { "+channel": "R.A" } } }"#,
                false,
                "real 0.0",
            ),
            (
                r#"{ "G": { "+atomic": "false", "a": { "+channel": "R.A" } } }"#,
                false,
                "\"false\"",
            ),
            (
                r#"{ "G": { "+atomic": "true", "a": { "+channel": "R.A" } } }"#,
                true,
                "\"true\"",
            ),
        ] {
            let groups = parse_group_config(json)
                .unwrap_or_else(|e| panic!("{label}: numeric/string +atomic must parse: {e}"));
            assert_eq!(groups.len(), 1, "{label}: group must be present");
            assert_eq!(groups[0].atomic, want, "{label}: +atomic coercion");
            assert!(groups[0].atomic_is_set, "{label}: presence bit set");
        }
    }

    /// Regression R0604-BRQSRV-GROUP-ATOMIC-CONFIG-1. A `+atomic` value
    /// that `as<bool>` cannot coerce (a non-`"true"`/`"false"` string,
    /// array, object, null) raises NoConvert in pvxs, which is caught per
    /// group — the bad group is dropped, its valid siblings survive
    /// (groupconfigprocessor.cpp:128-163). It must NOT fail the whole file.
    #[test]
    fn config_bad_atomic_skips_only_that_group() {
        let json = r#"{
            "G:bad":  { "+atomic": "yes", "a": { "+channel": "R.A" } },
            "G:good": { "b": { "+channel": "R.B" } }
        }"#;
        let groups = parse_group_config(json).expect("whole file must still parse");
        assert_eq!(groups.len(), 1, "only the valid sibling survives");
        assert_eq!(groups[0].name, "G:good", "the bad +atomic group is dropped");
    }

    /// Regression R0604-BRQSRV-GROUP-ATOMIC-CONFIG-1 (same defect family,
    /// `+id`). pvxs assigns `+id` with `value.as<std::string>()`
    /// (groupprocessorcontext.cpp:33), coercing a numeric `+id` to its
    /// base-10 string. A numeric `+id` must parse, not fail the whole file.
    #[test]
    fn config_id_numeric_coerces_to_string() {
        let groups = parse_group_config(r#"{ "G": { "+id": 5, "a": { "+channel": "R.A" } } }"#)
            .expect("numeric +id must parse");
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].struct_id.as_deref(),
            Some("5"),
            "numeric +id coerces to its base-10 string"
        );
    }

    /// Regression R0604-BRQSRV-GROUP-ATOMIC-CONFIG-1 (same defect family,
    /// `+id`). A `+id` that `as<std::string>` cannot coerce (array,
    /// object, null) is a per-group NoConvert, not a whole-file failure.
    #[test]
    fn config_bad_id_skips_only_that_group() {
        let json = r#"{
            "G:bad":  { "+id": [1, 2], "a": { "+channel": "R.A" } },
            "G:good": { "b": { "+channel": "R.B" } }
        }"#;
        let groups = parse_group_config(json).expect("whole file must still parse");
        assert_eq!(groups.len(), 1, "only the valid sibling survives");
        assert_eq!(groups[0].name, "G:good", "the bad +id group is dropped");
    }

    /// A later `info(Q:group)` fragment that omits `+atomic` must not
    /// revert an earlier explicit `+atomic:false`. pvxs only runs
    /// `defineAtomicity` when the incoming fragment set `atomicIsSet`
    /// (groupconfigprocessor.cpp:194); an omitting fragment leaves the
    /// merged TriState untouched. Verified in both merge orders.
    #[test]
    fn br_63_omitted_atomic_preserves_explicit_false() {
        use std::collections::HashMap;
        let explicit_false = parse_group_config(
            r#"{ "G:split": { "+atomic": false, "a": { "+channel": "R.A" } } }"#,
        )
        .unwrap();
        let omits_atomic =
            parse_group_config(r#"{ "G:split": { "b": { "+channel": "R.B" } } }"#).unwrap();

        // Order 1: explicit-false first, omitting fragment merged on top.
        let mut map: HashMap<String, GroupPvDef> = explicit_false
            .iter()
            .cloned()
            .map(|d| (d.name.clone(), d))
            .collect();
        merge_group_defs(&mut map, omits_atomic.clone());
        assert!(
            !map["G:split"].atomic,
            "omitted +atomic must not revert explicit +atomic:false (explicit-first)"
        );

        // Order 2: omitting fragment first (resolves to default atomic),
        // explicit-false merged on top — the explicit setting wins.
        let mut map2: HashMap<String, GroupPvDef> = omits_atomic
            .iter()
            .cloned()
            .map(|d| (d.name.clone(), d))
            .collect();
        merge_group_defs(&mut map2, explicit_false.clone());
        assert!(
            !map2["G:split"].atomic,
            "explicit +atomic:false must win over an earlier default (omit-first)"
        );
        assert!(
            map2["G:split"].atomic_is_set,
            "presence bit set after an explicit +atomic merges in"
        );
    }

    /// member order must be a deterministic (put_order,
    /// field_name) total order, not HashMap-iteration order. Members with
    /// no `+putorder` (`None`) sort first, then by put_order; ties broken
    /// by field name — matching pvxs's name-sorted-then-stable-putOrder
    /// layout.
    #[test]
    fn br_r59_member_order_is_canonical() {
        let groups = parse_group_config(
            r#"{ "GRP:ord": {
                "zebra": {"+channel": "R:z"},
                "alpha": {"+channel": "R:a"},
                "mid":   {"+channel": "R:m", "+putorder": 5},
                "first": {"+channel": "R:f", "+putorder": 1}
            }}"#,
        )
        .unwrap();
        let order: Vec<&str> = groups[0]
            .members
            .iter()
            .map(|m| m.field_name.as_str())
            .collect();
        // None-put_order members first (alpha, zebra by name), then
        // put_order 1 (first), then 5 (mid).
        assert_eq!(order, vec!["alpha", "zebra", "first", "mid"]);
    }

    /// the canonical order is re-established over the union after
    /// a cross-source merge (members appended, then re-sorted).
    #[test]
    fn br_r59_merge_reestablishes_canonical_order() {
        let mut existing = std::collections::HashMap::new();
        merge_group_defs(
            &mut existing,
            parse_group_config(r#"{ "GRP:mo": { "yankee": {"+channel": "R:y"} } }"#).unwrap(),
        );
        merge_group_defs(
            &mut existing,
            parse_group_config(r#"{ "GRP:mo": { "bravo": {"+channel": "R:b"} } }"#).unwrap(),
        );
        let order: Vec<&str> = existing["GRP:mo"]
            .members
            .iter()
            .map(|m| m.field_name.as_str())
            .collect();
        // Both no-put_order → name-sorted union: bravo before yankee.
        assert_eq!(order, vec!["bravo", "yankee"]);
    }

    /// an explicit `+trigger:"*"` is still parsed as `All`
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
        // `+trigger:""` in a group with no non-empty trigger is treated
        // exactly like a missing `+trigger`: pvxs defaults every channeled
        // field to self-trigger (groupconfigprocessor.cpp:317-339), so this
        // resolves to `SelfOnly`, not permanent silence.
        assert!(matches!(m.triggers, TriggerDef::SelfOnly));
    }

    #[test]
    fn parse_error_missing_channel() {
        // A scalar/plain member with no +channel is invalid: the group
        // is skipped (with a warning), not an all-or-nothing file abort
        // — pvxs groupconfigprocessor.cpp:128-163.
        let json = r#"{
            "GRP:bad": {
                "val": {
                    "+type": "scalar"
                }
            }
        }"#;

        let defs = parse_group_config(json).expect("file parses; invalid group skipped");
        assert!(defs.is_empty(), "the only (invalid) group must be skipped");
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

    /// pvxs prefixes the owning record name onto EVERY info(Q:group)
    /// `+channel`, including values that contain ':' or '.' — record-info
    /// group JSON is always record-relative (groupprocessorcontext.cpp:
    /// 65-66). A ':'-bearing value is NOT treated as an absolute PV here.
    #[test]
    fn br_65_info_group_prefixes_colon_bearing_channel() {
        let json = r#"{
            "TEMP:group": {
                "pressure": {
                    "+channel": "PRESS:ai",
                    "+type": "scalar"
                }
            }
        }"#;

        let groups = parse_info_group("TEMP:sensor", json).unwrap();
        assert_eq!(
            groups[0].members[0].channel, "TEMP:sensor.PRESS:ai",
            "info(Q:group) channel must be record-relative even when it contains ':'"
        );
    }

    /// The prefix rule must not depend on '.' scanning either: a dotted
    /// relative field string is still prefixed with the owning record.
    #[test]
    fn br_65_info_group_prefixes_dotted_channel() {
        let json = r#"{
            "TEMP:group": {
                "sub": {
                    "+channel": "A.B",
                    "+type": "scalar"
                }
            }
        }"#;

        let groups = parse_info_group("TEMP:sensor", json).unwrap();
        assert_eq!(groups[0].members[0].channel, "TEMP:sensor.A.B");
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

    /// Two info(Q:group) fragments contributing the SAME group field
    /// name collapse to one member — pvxs keeps one field per name
    /// (`groupdefinition.h:36-37`) and `defineFields`
    /// (groupconfigprocessor.cpp:221-225) skips the duplicate with
    /// "ignoring duplicate mapping" (first definition wins). A blind
    /// member append would compose the field twice on GET and double the
    /// backing write on an atomic PUT.
    #[test]
    fn merge_collapses_duplicate_field_name_first_wins() {
        let mut existing = HashMap::new();
        let defs1 =
            parse_group_config(r#"{ "GRP:d": { "x": { "+channel": "R1:x", "+putorder": 0 } } }"#)
                .unwrap();
        merge_group_defs(&mut existing, defs1);

        // A second fragment maps the same field name `x` to a different
        // channel — pvxs ignores it; the first definition stands.
        let defs2 =
            parse_group_config(r#"{ "GRP:d": { "x": { "+channel": "R2:x", "+putorder": 1 } } }"#)
                .unwrap();
        merge_group_defs(&mut existing, defs2);

        let grp = existing.get("GRP:d").unwrap();
        assert_eq!(grp.members.len(), 1, "duplicate field must collapse");
        assert_eq!(grp.members[0].field_name, "x");
        assert_eq!(
            grp.members[0].channel, "R1:x",
            "first definition must win (pvxs ignores the duplicate)"
        );
    }

    #[test]
    fn trigger_validation_unknown_field_drops_ref_not_config() {
        // pvxs (groupconfigprocessor.cpp:396-397) logs an unknown
        // trigger target and `continue`s — it drops only that reference,
        // never the group or its siblings. Pre-fix the Rust parser
        // returned Err, aborting the whole blob and dropping EVERY group.
        let json = r#"{
            "GRP:bad": {
                "x": { "+channel": "R:x", "+trigger": "y,z" },
                "y": { "+channel": "R:y" }
            },
            "GRP:sibling": {
                "a": { "+channel": "R:a" }
            }
        }"#;

        // 'z' does not exist, but the config must still parse and BOTH
        // groups must load.
        let groups =
            parse_group_config(json).expect("unknown trigger ref must not fail the config");
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert!(
            names.contains(&"GRP:bad"),
            "the group with the bad ref still loads: {names:?}"
        );
        assert!(
            names.contains(&"GRP:sibling"),
            "the sibling group must not be dropped by the bad ref: {names:?}"
        );
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
            epics_pva_rs::pvdata::ScalarValue::Long(v),
        )) = &version.const_value
        {
            assert_eq!(*v, 42);
        } else {
            panic!("expected Long(42), got {:?}", version.const_value);
        }
    }

    /// pvxs's canonical key is `+const` (test/qgroup.json).
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
            epics_pva_rs::pvdata::ScalarValue::Long(v),
        )) = &version.const_value
        {
            assert_eq!(*v, 7);
        } else {
            panic!("expected Long(7) via +const, got {:?}", version.const_value);
        }
    }

    /// `+const` wins when both keys are present.
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
            epics_pva_rs::pvdata::ScalarValue::Long(v),
        )) = &k.const_value
        {
            assert_eq!(*v, 100, "+const should take precedence over +value");
        } else {
            panic!("expected Long(100), got {:?}", k.const_value);
        }
    }

    /// A `+const` integer above the int32 range must survive at its
    /// full int64 width — pvxs builds it as `TypeCode::Int64`
    /// (groupconfigprocessor.cpp:680-686), so narrowing to i32 (the
    /// prior bug) corrupted large constants such as version IDs.
    #[test]
    fn parse_const_large_integer_preserved_at_int64() {
        use epics_pva_rs::pvdata::{PvField, ScalarValue};
        // i32::MAX + 1, i64::MIN, and a plain in-range value.
        let big = i64::from(i32::MAX) + 1;
        let json = format!(
            r#"{{
                "GRP:c": {{
                    "hi": {{ "+type": "const", "+const": {big} }},
                    "lo": {{ "+type": "const", "+const": {} }},
                    "mid": {{ "+type": "const", "+const": 5 }}
                }}
            }}"#,
            i64::MIN
        );
        let groups = parse_group_config(&json).unwrap();
        let members = &groups[0].members;
        let get = |name: &str| {
            members
                .iter()
                .find(|m| m.field_name == name)
                .and_then(|m| m.const_value.clone())
        };
        // Every JSON integer const is a `Long`, regardless of magnitude,
        // and retains its exact value (no i32 truncation/wraparound).
        assert!(
            matches!(get("hi"), Some(PvField::Scalar(ScalarValue::Long(v))) if v == big),
            "i32::MAX+1 const must stay Long({big}), got {:?}",
            get("hi")
        );
        assert!(
            matches!(get("lo"), Some(PvField::Scalar(ScalarValue::Long(v))) if v == i64::MIN),
            "i64::MIN const must stay Long, got {:?}",
            get("lo")
        );
        assert!(
            matches!(get("mid"), Some(PvField::Scalar(ScalarValue::Long(5)))),
            "in-range const is still Long (not Int), got {:?}",
            get("mid")
        );
    }

    /// pvxs `defineFields` only allows an empty top-level field name for a
    /// metadata mapping (groupconfigprocessor.cpp:215-231). Empty-keyed
    /// scalar / plain / const mappings must be skipped, not silently
    /// accepted as zero-value members.
    #[test]
    fn parse_empty_field_name_non_meta_is_skipped() {
        // Default (scalar) mapping with an empty key, plus a valid sibling.
        for empty_member in [
            r#""": { "+channel": "REC.VAL" }"#,
            r#""": { "+type": "plain", "+channel": "REC.VAL" }"#,
            r#""": { "+type": "any", "+channel": "REC.VAL" }"#,
            r#""": { "+type": "const", "+const": 1 }"#,
            r#""": { "+type": "structure" }"#,
            r#""": { "+type": "proc", "+channel": "REC.PROC" }"#,
        ] {
            let json =
                format!(r#"{{ "GRP:e": {{ {empty_member}, "ok": {{ "+channel": "R.VAL" }} }} }}"#);
            let groups = parse_group_config(&json).unwrap();
            let names: Vec<&str> = groups[0]
                .members
                .iter()
                .map(|m| m.field_name.as_str())
                .collect();
            assert!(
                !names.contains(&""),
                "empty-named non-meta member must be skipped for `{empty_member}`, got {names:?}"
            );
            assert!(
                names.contains(&"ok"),
                "valid sibling must survive for `{empty_member}`, got {names:?}"
            );
        }
    }

    /// The one allowed empty key: `+type:"meta"` is preserved so its
    /// alarm/timeStamp members flatten to the struct root (pvxs
    /// groupconfigprocessor.cpp:940-952).
    #[test]
    fn parse_empty_field_name_meta_is_kept() {
        let json = r#"{ "GRP:m": { "": { "+type": "meta", "+channel": "REC" } } }"#;
        let groups = parse_group_config(json).unwrap();
        let m = groups[0]
            .members
            .iter()
            .find(|m| m.field_name.is_empty())
            .expect("empty-named meta member must be kept");
        assert_eq!(m.mapping, FieldMapping::Meta);
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

    /// BR-81: pvxs's group JSON parser can only assign a scalar or `null`
    /// to a `+const` — it has no array callbacks
    /// (groupconfigprocessor.cpp:772-790) and rejects nested objects
    /// (:733-739). A const whose value is an array (scalar, nested,
    /// object-element, or null-element) or a nested object is rejected; the
    /// group is skipped via per-group recovery, matching pvxs's startup
    /// rejection. The cited values were previously accepted as Rust-only
    /// `ScalarArray` / `VariantArray` / `StructureArray` / `Structure`
    /// const forms that pvxs cannot represent.
    #[test]
    fn br_81_const_array_and_nested_object_rejected() {
        for body in [
            r#""list": { "+type": "const", "+const": [1, 2, 3] }"#,
            r#""matrix": { "+type": "const", "+const": [[1, 2], [3, 4]] }"#,
            r#""rows": { "+type": "const", "+const": [{"a": 1}, {"a": 2}] }"#,
            r#""mixed": { "+type": "const", "+const": [1, null, 3] }"#,
            r#""cfg": { "+type": "const", "+const": {"limits": {"low": 0}} }"#,
        ] {
            let json = format!(r#"{{ "GRP:c": {{ {body} }} }}"#);
            let groups =
                parse_group_config(&json).expect("file still parses; the invalid group is skipped");
            assert!(
                groups.is_empty(),
                "array / nested-object const must be rejected (group skipped) for `{body}`, got {groups:?}"
            );
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

    #[test]
    fn parse_const_missing_value_is_skipped() {
        // A const member without +const/+value is invalid: the group is
        // skipped rather than aborting the load.
        let json = r#"{
            "GRP:bad": {
                "label": {
                    "+type": "const"
                }
            }
        }"#;

        let defs = parse_group_config(json).expect("file parses; invalid group skipped");
        assert!(
            defs.is_empty(),
            "const member without +const/+value → group skipped"
        );
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

    /// `+nsecmask` is not an upstream pvxs group-JSON key — pvxs derives
    /// the nanosecond split solely from the record's `info(Q:time:tag)`
    /// (`ioc/typeutils.cpp:79-87`, `ioc/groupprocessorcontext.cpp:43-83`
    /// has no `+nsecmask` branch). A config still carrying it must load
    /// cleanly with the key silently ignored, leaving timestamp behaviour
    /// driven entirely by the already-masked record snapshot.
    #[test]
    fn nsecmask_key_is_ignored() {
        let json = r#"{
            "GRP:ns": {
                "val": {
                    "+channel": "R:val",
                    "+nsecmask": 255
                }
            }
        }"#;

        let groups = parse_group_config(json).unwrap();
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[0].members[0].field_name, "val");
        assert_eq!(groups[0].members[0].channel, "R:val");
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
