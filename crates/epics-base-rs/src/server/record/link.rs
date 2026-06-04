use crate::types::EpicsValue;

/// Link processing policy for input/output links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkProcessPolicy {
    NoProcess,
    #[default]
    ProcessPassive,
    /// CP (`pvlOptCP`): subscribe to source; when source changes, process
    /// this record unconditionally (`dbCa.c:993` adds `CA_DBPROCESS`
    /// regardless of `precord->scan`).
    ChannelProcess,
    /// CPP (`pvlOptCPP`): like `ChannelProcess`, but on a source change
    /// process this record only when its `SCAN` is `Passive` — C gates the
    /// `CA_DBPROCESS` action on `precord->scan == 0` (`dbCa.c:854,994,1072`).
    ChannelProcessPassive,
}

impl LinkProcessPolicy {
    /// For a CP (`ChannelProcess`) or CPP (`ChannelProcessPassive`) link,
    /// returns `Some(passive_only)`: `false` for CP (always process the
    /// link-holder when the source changes), `true` for CPP (process it
    /// only when it is Passive). Returns `None` for every other policy, so
    /// CP-link registration can filter in one match.
    pub fn cp_passive_only(self) -> Option<bool> {
        match self {
            LinkProcessPolicy::ChannelProcess => Some(false),
            LinkProcessPolicy::ChannelProcessPassive => Some(true),
            _ => None,
        }
    }
}

/// Parsed link address pointing to another record's field.
#[derive(Clone, Debug)]
pub struct LinkAddress {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
}

/// Hardware-link bus kind. Mirrors epics-base `link.h` bus enum.
/// We only carry kinds we can identify from the leading character or
/// a leading `@` token; the actual driver dispatch is by raw arg
/// string so unknown buses still land somewhere useful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HwLinkKind {
    /// `@dev arg1 arg2 ...` — INST_IO. The most common form, used by
    /// asyn-based device support.
    InstIo,
    /// `#Cn Sn @parm` — VME_IO. C/S = card/signal, parm = optional.
    VmeIo,
    /// Other / unrecognized — payload kept verbatim.
    Other,
}

/// Hardware link as parsed from a record's INP/OUT field. Mirrors
/// epics-base PR #213 — accepts the `@dev arg1 ...` and `#C S` forms
/// directly so device-support adapters get a structured handle
/// instead of having to re-parse the raw string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwLink {
    pub kind: HwLinkKind,
    /// Whitespace-tokenized argument list (after the leading `@` or
    /// `#…` discriminator). Empty when the link is just `@`.
    pub args: Vec<String>,
    /// Original verbatim payload (for drivers that prefer to do
    /// their own parsing — `dev arg1 0x1A` etc.).
    pub raw: String,
}

/// Parsed link — distinguishes constants, DB links, CA/PVA links, and empty.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedLink {
    None,
    Constant(String),
    Db(DbLink),
    Ca(CaLink),
    /// PVA (`pvalink`) link whose payload is a verbatim channel name —
    /// the string shorthand `{pva:"name"}` or the `pva://name` scheme
    /// form. Per pvxs `pva_parse_string` (pvalink_jlif.cpp:143-149) a
    /// string pvalink IS the channel name: any `?`/`&` in it is link
    /// DATA, not option syntax. The structured JSON longhand with
    /// options is [`ParsedLink::PvaJson`] instead, so this variant's
    /// `String` has exactly one meaning (a channel name) on every path.
    Pva(String),
    /// PVA (`pvalink`) link parsed from the structured JSON longhand
    /// `{pva:{pv:"name", field:"f", proc:"CP", …}}`. Carries the options
    /// as structured JLink members so the pvalink consumer reconstructs
    /// the `PvaLinkConfig` from map keys (pvalink_jlif.cpp:69-196), not
    /// from a `?key=value` URI query that pvxs never parses. See
    /// [`PvaJsonLink`].
    PvaJson(PvaJsonLink),
    /// `@dev arg1 …` or `#Cn Sn` hardware link (epics-base PR #213).
    Hw(HwLink),
    /// epics-base PR `e3c9d590` / `20404003`: a `lnkCalc` JSON link
    /// computes a result from one or more input PV values + a calc
    /// expression, optionally pulling its timestamp from one of the
    /// inputs. JSON form:
    /// `{calc:{expr:"A+B*2", args:["pv1","pv2.VAL"], time:"A"}}`
    /// — `time` is the input letter (A-L) whose timestamp the result
    /// should carry. `time` may be omitted (no timestamp passthrough).
    Calc(CalcLink),
}

/// A PVA (`pvalink`) external link parsed from the structured JSON
/// longhand `{pva:{pv:"name", field:"f", proc:"CP", …}}`.
///
/// pvxs parses pvalink options only as JLink map keys / typed values
/// (pvalink_jlif.cpp:69-196): booleans (`pipeline`/`time`/`retry`/
/// `local`/`atomic`), integers (`Q`/`monorder`), strings (`field`/
/// `proc`/`sevr`). There is no `?key=value` URI query parser in the
/// JLink callback table (pvalink_jlif.cpp:286-300). Preserving the
/// options as structured pairs here keeps that provenance: the consumer
/// reads JLink members directly instead of re-parsing a synthetic query
/// string (which is exactly the non-pvxs syntax this representation
/// avoids).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PvaJsonLink {
    /// Channel name from the `pv` member.
    pub pv: String,
    /// Non-`pv` JLink options in source order with original key case
    /// (`field`, `proc`, `sevr`, `Q`, `pipeline`, `time`, `retry`,
    /// `local`, `atomic`, `monorder`, …). Empty when the map carried
    /// only `pv` — in that case [`parse_link_v2`] yields a plain
    /// [`ParsedLink::Pva`] rather than this variant.
    pub options: Vec<(String, String)>,
}

/// Configuration for a `lnkCalc` link.
#[derive(Clone, Debug, PartialEq)]
pub struct CalcLink {
    /// Calc expression in epics-base postfix syntax — e.g. `"A+B*2"`,
    /// `"MAX(A,B,C)"`. Variables A..L bind to `args[0..12]`.
    pub expr: String,
    /// Input PV names. Each `args[i]` is fetched at link-read time and
    /// bound to the calc engine's variable slot at index `i` (0→A,
    /// 1→B, …). PV names may include a field suffix (`.VAL`, `.NORD`).
    /// Up to 12 inputs (calc engine A-L slots).
    pub args: Vec<String>,
    /// Input letter ('A'..='L') whose timestamp should be used for
    /// the result. `None` skips timestamp passthrough — the consumer
    /// uses its own `apply_timestamp` time.
    pub time_source: Option<char>,
}

/// Monitor propagation policy for links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MonitorSwitch {
    /// NMS: Do not propagate alarm severity from link source.
    #[default]
    NoMaximize,
    /// MS: Maximize alarm severity from link source into this record.
    Maximize,
    /// MSS: Maximize severity, set status from source.
    MaximizeStatus,
    /// MSI: Maximize severity if source is invalid.
    MaximizeIfInvalid,
}

/// A database link to another record's field.
#[derive(Clone, Debug, PartialEq)]
pub struct DbLink {
    pub record: String,
    pub field: String,
    pub policy: LinkProcessPolicy,
    pub monitor_switch: MonitorSwitch,
}

/// A Channel Access / PV Access external link to a remote PV.
///
/// carries the parsed `MS`/`NMS`/`MSI`/`MSS` maximize-
/// severity policy alongside the PV name, so the alarm gate is applied
/// at the record-processing boundary (uniform with [`DbLink`]) rather
/// than discarded as syntax. Mirrors the C link option parsed by
/// `dbStaticLib.c:2375` and applied by `recGbl.c:264`. The PV name never
/// carries trailing modifier tokens (they are stripped during parse);
/// it may retain a `ca://` scheme prefix, which the resolver strips.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaLink {
    pub pv: String,
    pub monitor_switch: MonitorSwitch,
}

impl CaLink {
    /// CA link with the default (`NoMaximize`) alarm policy — the
    /// shape used by callers that have only a bare PV name.
    pub fn new(pv: impl Into<String>) -> Self {
        Self {
            pv: pv.into(),
            monitor_switch: MonitorSwitch::NoMaximize,
        }
    }
}

/// Discriminated link *type* — the Rust analogue of the C
/// `link.h` `pv_link` / `constantStr` discrimination
/// (`modules/database/src/ioc/dbStatic/link.h:28-39`):
///
/// ```text
/// #define CONSTANT  0   -> LinkType::Constant
/// #define PV_LINK   1   -> (unresolved; resolves to Db or Ca)
/// #define DB_LINK   10  -> LinkType::Db
/// #define CA_LINK   11  -> LinkType::Ca
/// ```
///
/// C device support inspects `prec->inp.type` to decide behaviour —
/// e.g. `devEpidSoft.c:110` (`if (pepid->inp.type == CONSTANT)`),
/// `devEpidSoftCallback.c:116` (`if (ptriglink->type != CA_LINK)`).
/// This enum gives a record's `process()` / its device support the
/// same discrimination on the framework's string link fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkType {
    /// Empty / unset link — C has no value and no target.
    Empty,
    /// `CONSTANT` — the link is a literal numeric/string value, not a
    /// reference to another PV. C `link.h` `#define CONSTANT 0`.
    Constant,
    /// `DB_LINK` — a reference to a record.field in *this* IOC's
    /// database. C `link.h` `#define DB_LINK 10`.
    Db,
    /// `CA_LINK` — a reference to a PV reached over Channel Access /
    /// PV Access (a remote PV). C `link.h` `#define CA_LINK 11`.
    Ca,
    /// A hardware (`@dev …` / `#Cn Sn`) or `lnkCalc` JSON link — not
    /// one of the C `link.h` value-bearing scalar discriminants the
    /// records in this task care about. Kept distinct so a caller is
    /// never forced to mis-classify it.
    Other,
}

impl ParsedLink {
    /// The discriminated [`LinkType`] of this link — the C
    /// `prec->xxx.type` analogue. See [`LinkType`] for the C mapping.
    pub fn link_type(&self) -> LinkType {
        match self {
            ParsedLink::None => LinkType::Empty,
            ParsedLink::Constant(_) => LinkType::Constant,
            ParsedLink::Db(_) => LinkType::Db,
            ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_) => LinkType::Ca,
            ParsedLink::Hw(_) | ParsedLink::Calc(_) => LinkType::Other,
        }
    }

    /// Extract the constant as an EpicsValue (Double if numeric, else String).
    pub fn constant_value(&self) -> Option<EpicsValue> {
        if let ParsedLink::Constant(s) = self {
            if let Ok(v) = s.parse::<f64>() {
                Some(EpicsValue::Double(v))
            } else {
                Some(EpicsValue::String(s.clone().into()))
            }
        } else {
            None
        }
    }

    pub fn is_db(&self) -> bool {
        matches!(self, ParsedLink::Db(_))
    }

    /// True iff this link is a hardware (`@dev …` / `#Cn Sn`) link.
    pub fn is_hw(&self) -> bool {
        matches!(self, ParsedLink::Hw(_))
    }

    /// True iff this link is a writable OUT-link target — a local
    /// `Db` link or an external `Ca`/`Pva` link.
    ///
    /// The OUT-link write stage in `processing.rs` uses this to decide
    /// whether a record's OUT link has a target the value should be
    /// driven into. `Constant`/`Hw`/`Calc`/`None` are not writable
    /// targets (C `dbPutLink` returns `S_db_noLSET` for a link with no
    /// lset). Mirrors C `dbLink.c::dbPutLink` (dbLink.c:434-448), which
    /// dispatches DB *and* CA link writes uniformly through the link
    /// set's `putValue`.
    pub fn is_writable_out_link(&self) -> bool {
        matches!(
            self,
            ParsedLink::Db(_) | ParsedLink::Ca(_) | ParsedLink::Pva(_) | ParsedLink::PvaJson(_)
        )
    }

    /// PV name of an external (`Ca`/`Pva`/`PvaJson`) link, else `None`.
    /// Lets a caller read the remote channel name uniformly without
    /// matching the variants' differing payload shapes (`Ca` carries a
    /// [`CaLink`], `Pva` a bare channel-name `String`, `PvaJson` a
    /// [`PvaJsonLink`] with its `pv` member).
    pub fn external_pv_name(&self) -> Option<&str> {
        match self {
            ParsedLink::Ca(ca) => Some(&ca.pv),
            ParsedLink::Pva(name) => Some(name),
            ParsedLink::PvaJson(j) => Some(&j.pv),
            _ => None,
        }
    }

    /// Maximize-severity policy carried by this link's parsed modifier.
    /// `Db`/`Ca` links carry an explicit [`MonitorSwitch`]; PVA links
    /// keep their `sevr` as link data (a `Pva` channel-name string or a
    /// `PvaJson` `sevr` option) for the pvalink lset to apply, so they
    /// report `None` here (the lset gate stands in). Used by record
    /// processing to apply the MS/NMS/MSI/MSS gate at the fold boundary.
    pub fn monitor_switch(&self) -> Option<MonitorSwitch> {
        match self {
            ParsedLink::Db(db) => Some(db.monitor_switch),
            ParsedLink::Ca(ca) => Some(ca.monitor_switch),
            _ => None,
        }
    }
}

/// The two link-value shapes the pvxs JLink root callbacks accept at parse
/// depth 0: a JSON string (channel-name shorthand) or a JSON object/map
/// (longhand options). Every other root token — null, bool, integer, real,
/// array — installs no channel name in pvxs, so [`classify_pva_root_value`]
/// returns `None` for them rather than coercing the raw token into a PV
/// name. Regression R0604-BRPVALINK-ROOT-NONSTRING-PVA-1.
enum PvaRootValue<'a> {
    /// `{ ... }` longhand options map (handed to the sub-object parser).
    Object(&'a str),
    /// `"name"` / `'name'` string shorthand — exactly one matching quote
    /// pair stripped, contents kept verbatim (no semantic-character trim).
    StringName(&'a str),
}

/// Classify a root `pva`/`ca` link value by JSON shape. Accepts only a JSON
/// object or a (single- or double-) quoted string; rejects bare
/// `null`/`true`/`false`/number/array tokens the way the pvxs root JLink
/// callbacks do — `pva_parse_string` assigns `channelName` only at depth 0
/// while `pva_parse_null`/`bool`/`integer` ignore root-depth values
/// (pvalink_jlif.cpp:74-100,143-154). Regression
/// R0604-BRPVALINK-ROOT-NONSTRING-PVA-1.
fn classify_pva_root_value(value: &str) -> Option<PvaRootValue<'_>> {
    let v = value.trim();
    if v.starts_with('{') {
        return Some(PvaRootValue::Object(v));
    }
    for quote in ['"', '\''] {
        if let Some(rest) = v.strip_prefix(quote) {
            if let Some(inner) = rest.strip_suffix(quote) {
                return Some(PvaRootValue::StringName(inner));
            }
        }
    }
    None
}

/// Try to recognize a JSON-style link option (epics-base PR #86).
///
/// epics-base accepts inline JSON link options like `{ca: {pv: "foo"}}`,
/// `{pva: {pv: "foo"}}`, `{const: 1.5}`. The parser is JSON5-leaning
/// (unquoted keys, single quotes) — we accept that subset here using a
/// lightweight prepass that lowercases the leading key, then hands the
/// inner body to `serde_json::Value`.
///
/// Returns `Some(parsed)` when the string is a recognized JSON link;
/// `None` lets the caller fall through to legacy plain-text parsing.
fn try_parse_json_link(s: &str) -> Option<ParsedLink> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    // First key: scan until ':' or end. Trim outer braces, accept
    // optional whitespace + optional quote around the key.
    let inner = &s[1..s.len() - 1];
    let inner_trim = inner.trim_start();
    let (key_raw, rest) = match inner_trim.split_once(':') {
        Some((k, r)) => (k.trim(), r.trim()),
        None => return None,
    };
    let key = key_raw
        .trim_matches('"')
        .trim_matches('\'')
        .to_ascii_lowercase();
    match key.as_str() {
        "const" => {
            // Constant: bare numeric, quoted string, or array.
            // Strip outer quotes if present.
            let v = rest.trim_end_matches(',').trim();
            let stripped = v.trim_matches('"').trim_matches('\'');
            if stripped.is_empty() {
                Some(ParsedLink::None)
            } else {
                Some(ParsedLink::Constant(stripped.to_string()))
            }
        }
        "ca" | "pva" => {
            // pvxs accepts the link value in two forms (pvalink_jlif.cpp:24-31
            // documents the string shorthand; pva_parse_string at :143-149
            // takes a string value at depth 0 as the channel name and a `pv`
            // string inside a map):
            //   shorthand  { pva: "name" }             — string IS the channel
            //              name verbatim; any `?`/`&` in it is link DATA, not
            //              option syntax (pvalink_jlif.cpp:143-149).
            //   longhand   { pva: { pv: "name", ... } } — map with a `pv`
            //              member; the other keys are STRUCTURED JLink options
            //              (pvalink_jlif.cpp:69-196), preserved as such so the
            //              pvalink bridge reconstructs PvaLinkConfig from map
            //              keys rather than from a synthetic `?key=value` query
            //              (which pvxs has no parser for — :286-300).
            // For CA only the PV name matters (CA links bypass pvalink).
            //
            // Branch on the value's JSON shape so the recognized string
            // shorthand is routed to the PVA/CA resolver instead of falling
            // through to legacy DB parsing (which would treat the raw JSON
            // text as a record name).
            let value = rest.trim_end_matches(',').trim();
            // Classify the root value by JSON shape — only a string
            // (channel-name shorthand) or an object/map (longhand options)
            // is a valid pvalink root; a bare `true`/`5`/`null`/`[..]` token
            // installs no channel name in pvxs and must NOT be coerced into a
            // literal PV name. `?` returns `None` here so a non-string root
            // falls through to legacy parsing instead of dialing a remote PV.
            // Regression R0604-BRPVALINK-ROOT-NONSTRING-PVA-1.
            match classify_pva_root_value(value)? {
                PvaRootValue::Object(obj) => {
                    if key == "ca" {
                        // JSON CA links carry no plain-text MS modifier and
                        // ignore pvalink options; take only the PV name.
                        // Alarm policy defaults to NoMaximize.
                        Some(ParsedLink::Ca(CaLink::new(
                            extract_pv_and_opts_from_subobject(obj)?.0,
                        )))
                    } else {
                        // PVA longhand: keep the options as structured JLink
                        // members.
                        let (pv, options) = extract_pv_and_opts_from_subobject(obj)?;
                        if options.is_empty() {
                            Some(ParsedLink::Pva(pv))
                        } else {
                            Some(ParsedLink::PvaJson(PvaJsonLink { pv, options }))
                        }
                    }
                }
                PvaRootValue::StringName(name) => {
                    // String shorthand: the contents are the verbatim channel
                    // name. Do NOT split `?` — it is link data (pvxs treats a
                    // string pvalink as the channel name in full).
                    let name = name.trim();
                    if name.is_empty() {
                        return None;
                    }
                    if key == "ca" {
                        Some(ParsedLink::Ca(CaLink::new(name.to_string())))
                    } else {
                        Some(ParsedLink::Pva(name.to_string()))
                    }
                }
            }
        }
        "calc" => {
            // Form: { calc: { expr: "...", args: ["pv1","pv2"], time: "A" } }
            //   - expr (required, string)
            //   - args (optional, JSON string array)
            //   - time (optional, single uppercase letter A..L)
            // We use serde_json for proper parsing — the previous
            // permissive substring approach can't handle nested
            // arrays / quoted commas reliably.
            let body = rest.trim();
            // Trim trailing brace-of-outer-object swallowed during the
            // initial split. The body always starts with `{` and the
            // outer brace was already stripped above.
            let body_obj = if body.ends_with('}') {
                body
            } else {
                return None;
            };
            let val: serde_json::Value = serde_json::from_str(body_obj).ok()?;
            let obj = val.as_object()?;
            let expr = obj.get("expr").and_then(|v| v.as_str())?.to_string();
            let args: Vec<String> = obj
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            // 12 input cap — calc engine A..L map.
            if args.len() > 12 {
                return None;
            }
            let time_source = obj
                .get("time")
                .and_then(|v| v.as_str())
                .and_then(|s| s.chars().next())
                .filter(|c| ('A'..='L').contains(c));
            Some(ParsedLink::Calc(CalcLink {
                expr,
                args,
                time_source,
            }))
        }
        _ => None,
    }
}

/// Extract the `pv` name and all other key-value options from a
/// JSON-ish sub-object body. Returns `(pv_name, options)` where
/// `options` is every non-`pv` key as a structured `(key, value)` pair
/// in source order (empty when there are no extra options). Accepts
/// unquoted keys, single or double quotes around values.
///
/// The options are kept STRUCTURED — not flattened into a `?k=v&…`
/// query string — so the pvalink consumer reconstructs `PvaLinkConfig`
/// from JLink map members (pvalink_jlif.cpp:69-196), matching pvxs,
/// which has no URI-query parser in its JLink callback table
/// (pvalink_jlif.cpp:286-300). Key case is preserved so a case-sensitive
/// key like `Q` survives.
fn extract_pv_and_opts_from_subobject(body: &str) -> Option<(String, Vec<(String, String)>)> {
    let body = body.trim_start_matches('{').trim_end_matches('}').trim();
    let mut pv: Option<String> = None;
    let mut opts: Vec<(String, String)> = Vec::new();
    for entry in body.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // split_once splits at the FIRST ':' — PV names like "REC:AI"
        // have ':' inside the quoted value, but since we split on the
        // key separator `:` first (before the opening `"`) the colon
        // inside the quoted value survives (it's the second or later
        // colon in the entry string).
        let (k, v) = entry.split_once(':')?;
        let k_raw = k.trim().trim_matches('"').trim_matches('\'');
        let v_raw = v
            .trim()
            .trim_matches(',')
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if v_raw.is_empty() {
            continue;
        }
        if k_raw.eq_ignore_ascii_case("pv") {
            pv = Some(v_raw.to_string());
        } else {
            // Preserve original key case for PvaLinkConfig::parse
            // (which is case-sensitive for keys like `Q`).
            opts.push((k_raw.to_string(), v_raw.to_string()));
        }
    }
    Some((pv?, opts))
}

/// Recognize a hardware (`@dev …` / `#Cn Sn`) link. Mirrors epics-base
/// PR #213. Hex literals in args are kept as-is — `@dev 0x1A` survives
/// tokenization with `0x1A` as a single arg, since base's #213 was
/// specifically about preserving such literals through the args list.
fn try_parse_hw_link(s: &str) -> Option<ParsedLink> {
    if s.is_empty() {
        return None;
    }
    let first = s.as_bytes()[0];
    if first == b'@' {
        let raw = s[1..].trim().to_string();
        let args: Vec<String> = raw.split_whitespace().map(|t| t.to_string()).collect();
        return Some(ParsedLink::Hw(HwLink {
            kind: HwLinkKind::InstIo,
            args,
            raw,
        }));
    }
    if first == b'#' {
        let raw = s[1..].trim().to_string();
        let args: Vec<String> = raw.split_whitespace().map(|t| t.to_string()).collect();
        return Some(ParsedLink::Hw(HwLink {
            kind: HwLinkKind::VmeIo,
            args,
            raw,
        }));
    }
    None
}

/// Strip trailing link-attribute modifiers (`PP`/`NPP`/`CP`/`CPP`/`CA`/
/// `MS`/`NMS`/`MSI`/`MSS`) from a link string. Returns the remaining
/// `record.field` text plus the parsed process policy, maximize-severity
/// switch, and whether a bare ` CA` modifier forced a CA link. Modifiers
/// may appear in any order (`"REC.FIELD NPP NMS"`, `"REC CP"`, …).
///
/// shared by the legacy plain-text path *and* the `ca://`
/// scheme path so `ca://PV MS` parses the `MS` modifier instead of
/// folding it into the PV name. The bare ` CA` modifier forces a
/// `pv_link` to a CA link (C `dbStaticLib.c:2372`); it may co-occur with
/// `PP`/`MS`-style modifiers, so `force_ca` is recorded while `policy`
/// and `ms` continue to capture the rest.
fn strip_link_modifiers(s: &str) -> (&str, LinkProcessPolicy, MonitorSwitch, bool) {
    // C `dbParseLink` (`dbStaticLib.c:2252,2369-2371`): the modifier set
    // is `memset`-zeroed first, then `pvlOptPP` is set *only* on an
    // explicit ` PP` token (` NPP` clears it back to 0). A modifier-less
    // link is therefore NPP — for an INPUT link this means `dbDbGetValue`
    // (`dbDbLink.c:175`) does NOT process the passive source on read, and
    // for an OUTPUT link `dbDbPutValue` (`dbDbLink.c:387`) does NOT
    // process the target. Default `NoProcess`; the explicit ` PP` arm
    // below promotes it to `ProcessPassive`.
    let mut policy = LinkProcessPolicy::NoProcess;
    let mut ms = MonitorSwitch::NoMaximize;
    let mut force_ca = false;
    let mut link_part = s;
    loop {
        let trimmed = link_part.trim_end();
        if let Some(rest) = trimmed.strip_suffix(" NMS") {
            ms = MonitorSwitch::NoMaximize;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" MSI") {
            ms = MonitorSwitch::MaximizeIfInvalid;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" MSS") {
            ms = MonitorSwitch::MaximizeStatus;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" MS") {
            ms = MonitorSwitch::Maximize;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" NPP") {
            policy = LinkProcessPolicy::NoProcess;
            link_part = rest;
            continue;
        }
        // CPP before CP: a ` CPP` suffix must not be misread as ` CP`
        // leaving a stray `P`. (` CP` never matches a `...CPP` tail, but
        // keep the order explicit.) C distinguishes the two — CP processes
        // the link-holder unconditionally, CPP only when it is Passive.
        if let Some(rest) = trimmed.strip_suffix(" CPP") {
            policy = LinkProcessPolicy::ChannelProcessPassive;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" CP") {
            policy = LinkProcessPolicy::ChannelProcess;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" PP") {
            policy = LinkProcessPolicy::ProcessPassive;
            link_part = rest;
            continue;
        }
        // Bare ` CA` modifier — forces the link to be a CA link.
        // C `dbStaticLib.c:2372`. Stripped here so a combination such
        // as `REC.FIELD CA MS` leaves `link_part == "REC.FIELD"` and
        // both `force_ca` and `ms` are recorded.
        if let Some(rest) = trimmed.strip_suffix(" CA") {
            force_ca = true;
            link_part = rest;
            continue;
        }
        link_part = trimmed;
        break;
    }
    (link_part, policy, ms, force_ca)
}

/// Parse a link string into a ParsedLink (v2 — distinguishes constants from DB links).
pub fn parse_link_v2(s: &str) -> ParsedLink {
    let s = s.trim();
    // JSON-style links (epics-base PR #86) — try first so a leading
    // `{` is not mistaken for a leading-special record-name warning.
    if let Some(parsed) = try_parse_json_link(s) {
        return parsed;
    }
    // Hardware link (epics-base PR #213). `@` starts INST_IO; `#`
    // starts VME_IO. Everything else falls through to legacy parsing.
    if let Some(parsed) = try_parse_hw_link(s) {
        return parsed;
    }
    if s.is_empty() {
        return ParsedLink::None;
    }

    // CA/PVA protocol links. a `ca://PV MS` link carries
    // the same trailing maximize-severity modifiers as the legacy form,
    // so strip them off the scheme body before storing the PV name —
    // otherwise `MS` would be folded into the PV name (`"PV MS"`). The
    // parsed `MS`/`NMS`/`MSI`/`MSS` switch rides in the `CaLink` so the
    // alarm gate is applied at the record-processing boundary.
    if let Some(rest) = s.strip_prefix("ca://") {
        let (pv, _policy, ms, _force_ca) = strip_link_modifiers(rest);
        return ParsedLink::Ca(CaLink {
            pv: pv.to_string(),
            monitor_switch: ms,
        });
    }
    if let Some(rest) = s.strip_prefix("pva://") {
        return ParsedLink::Pva(rest.to_string());
    }

    // Strip trailing link attributes: PP, NPP, CP, CPP, CA, MS, NMS,
    // MSS, MSI (any order) — see [`strip_link_modifiers`].
    let (link_part, policy, ms, force_ca) = strip_link_modifiers(s);

    // A bare ` CA` modifier forces the link to be a CA link. C
    // `dbParseLink` only reaches the modifier scan after the
    // constant test (`dbStaticLib.c:2347`) has already failed — a
    // string carrying a ` CA` suffix never parses as a bare double,
    // so a CA-forced link is always a `PV_LINK`. Honour that here:
    // once ` CA` was stripped, classify as `ParsedLink::Ca` with the
    // remaining `link_part` (the `record.field` PV name) verbatim,
    // never as a Constant or local Db link.
    if force_ca {
        // a bare ` CA`-forced link carries the same
        // maximize-severity policy as any other link — store the parsed
        // `ms` so e.g. `REC.VAL CA MS` keeps its `MS` gate instead of
        // discarding it.
        return ParsedLink::Ca(CaLink {
            pv: link_part.to_string(),
            monitor_switch: ms,
        });
    }

    // Numeric constant
    if link_part.parse::<f64>().is_ok() {
        return ParsedLink::Constant(link_part.to_string());
    }

    // Quoted string constant.
    // C parity (3b484f5): an empty quoted string `""` is equivalent to an
    // unset link — dbConstLoadScalar/Array reject `""` the same as NULL with
    // S_db_badField. Treat it as None here so callers don't see a meaningless
    // empty Constant.
    if link_part.starts_with('"') && link_part.ends_with('"') && link_part.len() >= 2 {
        let inner = &link_part[1..link_part.len() - 1];
        if inner.is_empty() {
            return ParsedLink::None;
        }
        return ParsedLink::Constant(inner.to_string());
    }

    // DB link: try rsplit on '.', validate field part is uppercase alpha 1-4 chars
    if let Some((rec, field)) = link_part.rsplit_once('.') {
        let field_upper = field.to_ascii_uppercase();
        let is_valid_field = !field_upper.is_empty()
            && field_upper.len() <= 4
            && field_upper.chars().all(|c| c.is_ascii_uppercase());
        if is_valid_field {
            return ParsedLink::Db(DbLink {
                record: rec.to_string(),
                field: field_upper,
                policy,
                monitor_switch: ms,
            });
        }
    }

    // No dot or invalid field part → DB link with default field VAL
    ParsedLink::Db(DbLink {
        record: link_part.to_string(),
        field: "VAL".to_string(),
        policy,
        monitor_switch: ms,
    })
}

/// Parse an **output** link.
///
/// Output and input links share [`parse_link_v2`]: C `dbParseLink`
/// zeroes the modifier set for both link types (`dbStaticLib.c:2252`),
/// so a modifier-less link is NPP (`NoProcess`) regardless of
/// direction. The OUT-link target-processing decision — process when
/// the link is explicit ` PP` **or** the destination field is `.PROC`
/// — lives in the write path
/// ([`crate::server::database::Database::write_db_link_value`]),
/// matching C `dbDbPutValue` (`dbDbLink.c:387-390`); it is no longer
/// encoded as a parse-time policy override. This entry point is
/// retained as the OUT-link parse boundary named by `dbPutLink`
/// callers (record `OUT` / dfanout `OUTn` / sseq `LNKn`).
pub fn parse_output_link_v2(s: &str) -> ParsedLink {
    parse_link_v2(s)
}

/// Determine the [`LinkType`] of a record's string link field directly
/// from its raw text — the convenience API a record's `process()` or
/// its device support uses to discriminate one of its `INP` / `OUTL` /
/// `TRIG` link fields without having to match the whole [`ParsedLink`]
/// enum.
///
/// This is the framework's answer to C device support reading
/// `prec->inp.type` (`devEpidSoft.c:110`,
/// `devEpidSoftCallback.c:116`): the existing string link fields are
/// kept as-is, and this query is layered on top.
pub fn link_field_type(s: &str) -> LinkType {
    parse_link_v2(s).link_type()
}

/// Parse a link string into a LinkAddress (legacy wrapper around parse_link_v2).
/// Formats: "REC.FIELD", "REC", "REC.FIELD PP", "REC.FIELD NPP", "" → None
pub fn parse_link(s: &str) -> Option<LinkAddress> {
    match parse_link_v2(s) {
        ParsedLink::Db(db) => Some(LinkAddress {
            record: db.record,
            field: db.field,
            policy: db.policy,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod json_link_tests {
    //! epics-base PR #86 — JSON-style inline link options.
    use super::*;

    #[test]
    fn json_const_numeric() {
        assert_eq!(
            parse_link_v2("{const: 1.5}"),
            ParsedLink::Constant("1.5".to_string())
        );
    }

    #[test]
    fn json_const_quoted_string() {
        assert_eq!(
            parse_link_v2(r#"{const: "hello"}"#),
            ParsedLink::Constant("hello".to_string())
        );
    }

    #[test]
    fn json_const_empty_is_none() {
        // `{const: ""}` matches base's empty-link convention.
        assert_eq!(parse_link_v2(r#"{const: ""}"#), ParsedLink::None);
    }

    #[test]
    fn json_ca_link() {
        assert_eq!(
            parse_link_v2(r#"{ca: { pv: "FOO" }}"#),
            ParsedLink::Ca(CaLink::new("FOO"))
        );
    }

    #[test]
    fn json_pva_link() {
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "FOO:bar" }}"#),
            ParsedLink::Pva("FOO:bar".to_string())
        );
    }

    #[test]
    fn json_ca_link_unquoted_key() {
        assert_eq!(
            parse_link_v2(r#"{ca: { pv: 'BAR' }}"#),
            ParsedLink::Ca(CaLink::new("BAR"))
        );
    }

    // pvxs string shorthand `{ pva: "PV" }` / `{ ca: "PV" }`
    // (pvalink_jlif.cpp:24-31, :143-149). Before the fix these fell through
    // to legacy DB parsing because the value is a string, not a `{pv:...}`
    // sub-object — the link silently became a DB link to a record whose name
    // was the raw JSON text.

    #[test]
    fn json_pva_link_string_shorthand() {
        assert_eq!(
            parse_link_v2(r#"{pva: "TARGET:AI"}"#),
            ParsedLink::Pva("TARGET:AI".to_string())
        );
    }

    #[test]
    fn json_ca_link_string_shorthand() {
        assert_eq!(
            parse_link_v2(r#"{ca: "TARGET:AI"}"#),
            ParsedLink::Ca(CaLink::new("TARGET:AI"))
        );
    }

    #[test]
    fn json_pva_link_string_shorthand_single_quotes() {
        assert_eq!(
            parse_link_v2(r#"{pva: 'invalid:pv:name'}"#),
            ParsedLink::Pva("invalid:pv:name".to_string())
        );
    }

    #[test]
    fn json_pva_link_longhand_preserves_options_structurally() {
        // The longhand map form keeps a `pv` member plus its options as
        // STRUCTURED JLink pairs (not a `?Q=4` query) so the consumer
        // reconstructs PvaLinkConfig from map keys, matching pvxs.
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "FOO:bar", Q: "4" }}"#),
            ParsedLink::PvaJson(PvaJsonLink {
                pv: "FOO:bar".to_string(),
                options: vec![("Q".to_string(), "4".to_string())],
            })
        );
    }

    /// Regression R0604-BRPVALINK-ROOT-NONSTRING-PVA-1.
    ///
    /// pvxs installs root JLink callbacks only for the JSON string (channel
    /// shorthand) and map (longhand) cases; `pva_parse_null`/`bool`/`integer`
    /// ignore root-depth values (pvalink_jlif.cpp:74-100,143-154). A bare
    /// `true`/`5`/`null`/`[..]` root therefore installs no channel name and
    /// must NOT become an external PVA/CA link to a literal token. The old
    /// `value.starts_with('{')` heuristic accepted every non-`{` token as a
    /// PV name after trimming quotes, dialing channels named `"true"`/`"5"`/
    /// `"null"`/`"[1,2]"`.
    #[test]
    fn json_pva_link_root_nonstring_rejected() {
        // Accepted roots: string shorthand and object longhand.
        assert_eq!(
            parse_link_v2(r#"{pva: "TARGET"}"#),
            ParsedLink::Pva("TARGET".to_string())
        );
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "TARGET" }}"#),
            ParsedLink::Pva("TARGET".to_string())
        );

        // Rejected roots: a non-string, non-object value must never produce
        // a PVA link (it falls through to legacy parsing instead).
        for src in [
            r#"{pva: true}"#,
            r#"{pva: false}"#,
            r#"{pva: 5}"#,
            r#"{pva: null}"#,
            r#"{pva: [1,2]}"#,
        ] {
            assert!(
                !matches!(
                    parse_link_v2(src),
                    ParsedLink::Pva(_) | ParsedLink::PvaJson(_)
                ),
                "non-string pva root must not become a PVA link: {src}"
            );
        }

        // Same defect family on the `ca` key: a bare token must not become a
        // CA link to a literal `"true"`/`"5"` channel name.
        for src in [
            r#"{ca: true}"#,
            r#"{ca: 5}"#,
            r#"{ca: null}"#,
            r#"{ca: [1,2]}"#,
        ] {
            assert!(
                !matches!(parse_link_v2(src), ParsedLink::Ca(_)),
                "non-string ca root must not become a CA link: {src}"
            );
        }
    }

    // epics-base PR #213 — hardware-link parsing.

    #[test]
    fn hw_link_inst_io() {
        let parsed = parse_link_v2("@simDriver 0 INPUT");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::InstIo);
                assert_eq!(hw.args, vec!["simDriver", "0", "INPUT"]);
                assert_eq!(hw.raw, "simDriver 0 INPUT");
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    #[test]
    fn hw_link_inst_io_with_hex() {
        // PR #213 specifically: hex literals in HW-link args must
        // survive tokenization intact.
        let parsed = parse_link_v2("@dev 0xFF mask=0x1A");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::InstIo);
                assert_eq!(hw.args, vec!["dev", "0xFF", "mask=0x1A"]);
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    #[test]
    fn hw_link_vme_io() {
        let parsed = parse_link_v2("#C0 S2");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::VmeIo);
                assert_eq!(hw.args, vec!["C0", "S2"]);
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    #[test]
    fn hw_link_inst_io_empty_args() {
        // `@` alone — kind set, args empty, raw empty.
        let parsed = parse_link_v2("@");
        match parsed {
            ParsedLink::Hw(hw) => {
                assert_eq!(hw.kind, HwLinkKind::InstIo);
                assert!(hw.args.is_empty());
                assert!(hw.raw.is_empty());
            }
            other => panic!("expected Hw, got {other:?}"),
        }
    }

    // Link-type discrimination — C `link.h` CONSTANT / DB_LINK /
    // CA_LINK (`dbStatic/link.h:28-39`).

    #[test]
    fn link_type_constant_numeric() {
        assert_eq!(link_field_type("3.14"), LinkType::Constant);
        assert_eq!(link_field_type("{const: 7}"), LinkType::Constant);
    }

    #[test]
    fn link_type_constant_quoted_string() {
        assert_eq!(link_field_type(r#""hello""#), LinkType::Constant);
    }

    #[test]
    fn link_type_empty_is_empty() {
        assert_eq!(link_field_type(""), LinkType::Empty);
        assert_eq!(link_field_type("   "), LinkType::Empty);
        assert_eq!(link_field_type(r#""""#), LinkType::Empty);
    }

    #[test]
    fn link_type_db_link() {
        assert_eq!(link_field_type("REC.VAL"), LinkType::Db);
        assert_eq!(link_field_type("REC"), LinkType::Db);
        assert_eq!(link_field_type("REC.VAL PP"), LinkType::Db);
    }

    #[test]
    fn link_type_ca_link() {
        assert_eq!(link_field_type("ca://REMOTE:PV"), LinkType::Ca);
        assert_eq!(link_field_type("pva://REMOTE:PV"), LinkType::Ca);
        assert_eq!(link_field_type(r#"{ca: { pv: "REMOTE" }}"#), LinkType::Ca);
    }

    #[test]
    fn link_type_hw_and_calc_are_other() {
        assert_eq!(link_field_type("@dev 0 IN"), LinkType::Other);
        assert_eq!(link_field_type("#C0 S2"), LinkType::Other);
        // calc link uses strict JSON (quoted keys) — serde_json parse.
        assert_eq!(
            link_field_type(r#"{calc: {"expr": "A+1", "args": ["pv1"]}}"#),
            LinkType::Other
        );
    }

    // Bare ` CA` modifier — C `dbStaticLib.c:2372` forces a
    // `pv_link` to a CA link (`link.h` `CA_LINK` 11).

    #[test]
    fn ca_modifier_classifies_as_ca() {
        // `REC.FIELD CA` must parse as a CA link carrying the
        // `record.field` PV name — NOT a Db link to field "FIELD CA".
        // No `MS`-class modifier → default NoMaximize.
        assert_eq!(
            parse_link_v2("REC.FIELD CA"),
            ParsedLink::Ca(CaLink::new("REC.FIELD"))
        );
        assert_eq!(link_field_type("REC.FIELD CA"), LinkType::Ca);
    }

    #[test]
    fn ca_modifier_bare_pv_name() {
        // No field suffix — `localPv CA` is still a CA link.
        assert_eq!(
            parse_link_v2("localPv CA"),
            ParsedLink::Ca(CaLink::new("localPv"))
        );
        assert_eq!(link_field_type("localPv CA"), LinkType::Ca);
    }

    #[test]
    fn ca_modifier_combined_with_pp_ms() {
        // `CA` may co-occur with PP/MS-style modifiers in
        // any order. The PV name is stripped clean, AND the `MS`-class
        // modifier is now CARRIED in the CaLink (pre-fix it was
        // discarded, reducing both forms to a bare `Ca("REC.VAL")`).
        assert_eq!(
            parse_link_v2("REC.VAL CA MS"),
            ParsedLink::Ca(CaLink {
                pv: "REC.VAL".to_string(),
                monitor_switch: MonitorSwitch::Maximize,
            })
        );
        // `PP CA` carries no MS-class modifier → NoMaximize.
        assert_eq!(
            parse_link_v2("REC.VAL PP CA"),
            ParsedLink::Ca(CaLink::new("REC.VAL"))
        );
        // `CA NMS` carries the explicit NoMaximize switch.
        assert_eq!(
            parse_link_v2("REC.VAL CA NMS"),
            ParsedLink::Ca(CaLink {
                pv: "REC.VAL".to_string(),
                monitor_switch: MonitorSwitch::NoMaximize,
            })
        );
        assert_eq!(link_field_type("REC.VAL CA NMS"), LinkType::Ca);
    }

    #[test]
    fn ca_scheme_link_parses_ms_modifier() {
        // `ca://PV MS` must strip the modifier off the
        // scheme body — pre-fix the PV name became `"PV MS"`.
        assert_eq!(
            parse_link_v2("ca://SR:DCCT MS"),
            ParsedLink::Ca(CaLink {
                pv: "SR:DCCT".to_string(),
                monitor_switch: MonitorSwitch::Maximize,
            })
        );
        assert_eq!(
            parse_link_v2("ca://SR:DCCT MSI"),
            ParsedLink::Ca(CaLink {
                pv: "SR:DCCT".to_string(),
                monitor_switch: MonitorSwitch::MaximizeIfInvalid,
            })
        );
        assert_eq!(
            parse_link_v2("ca://SR:DCCT MSS"),
            ParsedLink::Ca(CaLink {
                pv: "SR:DCCT".to_string(),
                monitor_switch: MonitorSwitch::MaximizeStatus,
            })
        );
        // Bare `ca://PV` → default NoMaximize, PV name intact.
        assert_eq!(
            parse_link_v2("ca://SR:DCCT"),
            ParsedLink::Ca(CaLink::new("SR:DCCT"))
        );
    }

    #[test]
    fn ca_modifier_does_not_affect_plain_db_link() {
        // A link with no ` CA` modifier stays a Db link — the fix
        // must not over-trigger on record names that merely contain
        // the letters "ca".
        assert_eq!(link_field_type("camera.VAL"), LinkType::Db);
        assert_eq!(link_field_type("REC.VAL PP"), LinkType::Db);
    }

    /// JSON pvalink options survive parse_link_v2 as STRUCTURED JLink
    /// members (pv + ordered (key,value) pairs), not as a `?key=value`
    /// URI query — pvxs has no query parser (pvalink_jlif.cpp:286-300);
    /// options are JLink map keys (:69-196).
    #[test]
    fn br_r10_json_pva_options_preserved_in_parsed_link() {
        // All options present: field, proc (CPP), sevr (MS), Q.
        let link = parse_link_v2(
            r#"{pva: {pv: "TARGET:AI", field: "display.precision", proc: "CPP", sevr: "MS", Q: 8}}"#,
        );
        let j = match link {
            ParsedLink::PvaJson(j) => j,
            other => panic!("expected PvaJson, got {other:?}"),
        };
        assert_eq!(j.pv, "TARGET:AI", "pv must be the bare channel name");
        // No re-encoded query syntax anywhere in the channel name.
        assert!(
            !j.pv.contains('?'),
            "pv must not carry a `?` query: {}",
            j.pv
        );
        // Options preserved in source order with original key case.
        assert_eq!(
            j.options,
            vec![
                ("field".to_string(), "display.precision".to_string()),
                ("proc".to_string(), "CPP".to_string()),
                ("sevr".to_string(), "MS".to_string()),
                ("Q".to_string(), "8".to_string()),
            ]
        );
    }

    /// A PVA string shorthand keeps the channel name verbatim — a `?` in
    /// it is link DATA, not option syntax (pvxs pva_parse_string,
    /// pvalink_jlif.cpp:143-149). It must NOT become a PvaJson with
    /// parsed options.
    #[test]
    fn json_pva_string_shorthand_keeps_query_chars_verbatim() {
        assert_eq!(
            parse_link_v2(r#"{pva: "TARGET:AI?field=x"}"#),
            ParsedLink::Pva("TARGET:AI?field=x".to_string())
        );
    }

    /// The `pva://` scheme form is likewise a verbatim channel name; a
    /// `?` is not split out as options.
    #[test]
    fn pva_scheme_keeps_query_chars_verbatim() {
        assert_eq!(
            parse_link_v2("pva://TARGET:AI?field=x"),
            ParsedLink::Pva("TARGET:AI?field=x".to_string())
        );
    }

    /// `external_pv_name` reads the channel name from a PvaJson link.
    #[test]
    fn pva_json_external_pv_name() {
        let link = parse_link_v2(r#"{pva: {pv: "TARGET:AI", proc: "CP"}}"#);
        assert_eq!(link.external_pv_name(), Some("TARGET:AI"));
        assert_eq!(link.link_type(), LinkType::Ca);
        assert!(link.is_writable_out_link());
    }

    /// Bare pvalink JSON with no extra options is unchanged.
    #[test]
    fn br_r10_json_pva_bare_pv_unchanged() {
        assert_eq!(
            parse_link_v2(r#"{pva: { pv: "FOO:bar" }}"#),
            ParsedLink::Pva("FOO:bar".to_string())
        );
    }

    #[test]
    fn json_unknown_key_falls_through_to_legacy() {
        // Unknown JSON top-level key must NOT be hijacked — leave it
        // for legacy parsing (which will likely produce None or a
        // weird Db link, but not crash).
        let result = parse_link_v2("{unknown: 42}");
        // Not Constant("42"), not Ca/Pva — must be one of the
        // legacy fall-through outcomes.
        assert!(matches!(
            result,
            ParsedLink::None | ParsedLink::Db(_) | ParsedLink::Constant(_)
        ));
    }

    /// BUG 2 — a bare OUT link defaults to NPP (`NoProcess`). C
    /// `dbDbPutValue` (dbDbLink.c:386-389) processes the target only
    /// on an explicit `PP` flag.
    #[test]
    fn parse_output_link_bare_is_noprocess() {
        match parse_output_link_v2("TARGET.VAL") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
                assert_eq!(db.record, "TARGET");
                assert_eq!(db.field, "VAL");
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// BUG 2 — an explicit ` PP` on an OUT link keeps `ProcessPassive`.
    #[test]
    fn parse_output_link_explicit_pp_processes() {
        match parse_output_link_v2("TARGET.VAL PP") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::ProcessPassive);
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// A modifier-less OUT link to a `.PROC` field parses to the uniform
    /// `NoProcess` default like any other bare link; the target-process
    /// decision for `.PROC` lives in the write path
    /// (`write_db_link_value`, C `dbDbPutValue` dbDbLink.c:387-390), not
    /// in a parse-time policy. (Behaviour — that a `.PROC` write still
    /// processes the target — is covered by the database-level
    /// `*_proc_out_link_processes_target` test.)
    #[test]
    fn parse_output_link_proc_field_is_noprocess() {
        match parse_output_link_v2("TARGET.PROC") {
            ParsedLink::Db(db) => {
                assert_eq!(db.field, "PROC");
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// A modifier-less **input** link defaults to NPP (`NoProcess`),
    /// matching C `dbParseLink` (`dbStaticLib.c:2252` memset→0;
    /// `pvlOptPP` set only on an explicit ` PP`). A bare INP must NOT
    /// cause `dbDbGetValue` (`dbDbLink.c:175`) to process the passive
    /// source on read.
    #[test]
    fn parse_input_link_bare_is_noprocess() {
        match parse_link_v2("SRC.VAL") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
                assert_eq!(db.record, "SRC");
                assert_eq!(db.field, "VAL");
            }
            other => panic!("expected Db link, got {other:?}"),
        }
        // An explicit ` PP` input link still promotes to ProcessPassive.
        match parse_link_v2("SRC.VAL PP") {
            ParsedLink::Db(db) => assert_eq!(db.policy, LinkProcessPolicy::ProcessPassive),
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// BUG 2 — an explicit ` NPP` OUT link is `NoProcess` (unchanged
    /// from `parse_link_v2`, but pinned here for completeness).
    #[test]
    fn parse_output_link_explicit_npp_is_noprocess() {
        match parse_output_link_v2("TARGET.VAL NPP") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::NoProcess);
            }
            other => panic!("expected Db link, got {other:?}"),
        }
    }

    /// CP and CPP must parse to distinct policies — collapsing CPP into
    /// `ChannelProcess` loses C's `precord->scan == 0` gate (`dbCa.c:994`).
    #[test]
    fn parse_cp_and_cpp_are_distinct_policies() {
        match parse_link_v2("SRC.VAL CP") {
            ParsedLink::Db(db) => assert_eq!(db.policy, LinkProcessPolicy::ChannelProcess),
            other => panic!("expected Db link for CP, got {other:?}"),
        }
        match parse_link_v2("SRC.VAL CPP") {
            ParsedLink::Db(db) => {
                assert_eq!(db.policy, LinkProcessPolicy::ChannelProcessPassive)
            }
            other => panic!("expected Db link for CPP, got {other:?}"),
        }
    }

    /// `cp_passive_only`: CP → `Some(false)`, CPP → `Some(true)`, others → None.
    #[test]
    fn cp_passive_only_maps_cp_and_cpp() {
        assert_eq!(
            LinkProcessPolicy::ChannelProcess.cp_passive_only(),
            Some(false)
        );
        assert_eq!(
            LinkProcessPolicy::ChannelProcessPassive.cp_passive_only(),
            Some(true)
        );
        assert_eq!(LinkProcessPolicy::ProcessPassive.cp_passive_only(), None);
        assert_eq!(LinkProcessPolicy::NoProcess.cp_passive_only(), None);
    }
}
