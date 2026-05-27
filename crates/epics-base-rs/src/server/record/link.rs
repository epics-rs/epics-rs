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
    Pva(String),
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
/// BRIDGE-FR-3: carries the parsed `MS`/`NMS`/`MSI`/`MSS` maximize-
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
            ParsedLink::Ca(_) | ParsedLink::Pva(_) => LinkType::Ca,
            ParsedLink::Hw(_) | ParsedLink::Calc(_) => LinkType::Other,
        }
    }

    /// Extract the constant as an EpicsValue (Double if numeric, else String).
    pub fn constant_value(&self) -> Option<EpicsValue> {
        if let ParsedLink::Constant(s) = self {
            if let Ok(v) = s.parse::<f64>() {
                Some(EpicsValue::Double(v))
            } else {
                Some(EpicsValue::String(s.clone()))
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
            ParsedLink::Db(_) | ParsedLink::Ca(_) | ParsedLink::Pva(_)
        )
    }

    /// PV name of an external (`Ca`/`Pva`) link, else `None`. Lets a
    /// caller read the remote name uniformly without matching the two
    /// variants' differing payload shapes (`Ca` carries a [`CaLink`],
    /// `Pva` a bare `String`). BRIDGE-FR-3.
    pub fn external_pv_name(&self) -> Option<&str> {
        match self {
            ParsedLink::Ca(ca) => Some(&ca.pv),
            ParsedLink::Pva(name) => Some(name),
            _ => None,
        }
    }

    /// Maximize-severity policy carried by this link's parsed modifier.
    /// `Db`/`Ca` links carry an explicit [`MonitorSwitch`]; `Pva` links
    /// encode it in a stripped `?sevr=` query the lset retains, so they
    /// report `None` here (the lset gate stands in). Used by record
    /// processing to apply the MS/NMS/MSI/MSS gate at the fold boundary.
    /// BRIDGE-FR-3.
    pub fn monitor_switch(&self) -> Option<MonitorSwitch> {
        match self {
            ParsedLink::Db(db) => Some(db.monitor_switch),
            ParsedLink::Ca(ca) => Some(ca.monitor_switch),
            _ => None,
        }
    }
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
            // Form: { pva: { pv: "name", field: "F", proc: "CP", ... } }
            // For CA: only the PV name is needed (CA links bypass pvalink).
            // For PVA: preserve all pvxs JSON options as a query string so
            // the pvalink bridge can reconstruct the full PvaLinkConfig.
            // pvxs parity: pvalink_jlif.cpp:24-41, :69-196.
            let (pv, query) = extract_pv_and_opts_from_subobject(rest)?;
            if key == "ca" {
                // JSON CA links carry no plain-text MS modifier; the
                // alarm policy defaults to NoMaximize (BRIDGE-FR-3).
                Some(ParsedLink::Ca(CaLink::new(pv)))
            } else if query.is_empty() {
                Some(ParsedLink::Pva(pv))
            } else {
                Some(ParsedLink::Pva(format!("{pv}?{query}")))
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
/// JSON-ish sub-object body. Returns `(pv_name, query_string)` where
/// `query_string` encodes every non-pv key as `k=v&…` (empty when
/// there are no extra options). Accepts unquoted keys, single or
/// double quotes around values.
///
/// pvxs parity: pvalink_jlif.cpp:24-41 (supported keys),
/// :69-196 (per-key parsing). Key case is preserved so
/// `PvaLinkConfig::parse` can parse `Q` (uppercase).
fn extract_pv_and_opts_from_subobject(body: &str) -> Option<(String, String)> {
    let body = body.trim_start_matches('{').trim_end_matches('}').trim();
    let mut pv: Option<String> = None;
    let mut opts: Vec<String> = Vec::new();
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
            opts.push(format!("{k_raw}={v_raw}"));
        }
    }
    Some((pv?, opts.join("&")))
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
/// BRIDGE-FR-3: shared by the legacy plain-text path *and* the `ca://`
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

    // CA/PVA protocol links. BRIDGE-FR-3: a `ca://PV MS` link carries
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
        // BRIDGE-FR-3: a bare ` CA`-forced link carries the same
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
        // BRIDGE-FR-3: `CA` may co-occur with PP/MS-style modifiers in
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
        // BRIDGE-FR-3: `ca://PV MS` must strip the modifier off the
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

    /// BR-R10: JSON pvalink options survive parse_link_v2.
    /// Upstream parity: pvxs pvalink_jlif.cpp:24-41, :69-196.
    #[test]
    fn br_r10_json_pva_options_preserved_in_parsed_link() {
        // All options present: field, proc (CPP), sevr (MS), Q, pipeline.
        let link = parse_link_v2(
            r#"{pva: {pv: "TARGET:AI", field: "display.precision", proc: "CPP", sevr: "MS", Q: 8}}"#,
        );
        let stored = match link {
            ParsedLink::Pva(ref s) => s.as_str(),
            other => panic!("expected Pva, got {other:?}"),
        };
        assert!(
            stored.starts_with("TARGET:AI?"),
            "options must be encoded as query: {stored}"
        );
        assert!(
            stored.contains("field=display.precision"),
            "field option lost: {stored}"
        );
        assert!(stored.contains("proc=CPP"), "proc option lost: {stored}");
        assert!(stored.contains("sevr=MS"), "sevr option lost: {stored}");
        assert!(stored.contains("Q=8"), "Q option lost: {stored}");
    }

    /// BR-R10: bare pvalink JSON with no extra options is unchanged.
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
