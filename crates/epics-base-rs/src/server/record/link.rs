use crate::types::EpicsValue;

/// Link processing policy for input/output links.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkProcessPolicy {
    NoProcess,
    #[default]
    ProcessPassive,
    /// CP: subscribe to source; when source changes, process this record.
    ChannelProcess,
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
    Ca(String),
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

impl ParsedLink {
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
            // Form: { ca: { pv: "name", proc: ... } } — extract pv
            // value via a permissive substring scan. Full JSON parser
            // would be cleaner but pulls in pv-name validation that
            // belongs at parse_db level.
            let pv = extract_pv_from_subobject(rest)?;
            if key == "ca" {
                Some(ParsedLink::Ca(pv))
            } else {
                Some(ParsedLink::Pva(pv))
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

/// Extract `pv: "name"` from a sub-object's body. Permissive: accepts
/// quoted/unquoted keys, single or double quotes around the value.
fn extract_pv_from_subobject(body: &str) -> Option<String> {
    let body = body.trim_start_matches('{').trim_end_matches('}').trim();
    for entry in body.split(',') {
        let entry = entry.trim();
        let (k, v) = entry.split_once(':')?;
        let k = k.trim().trim_matches('"').trim_matches('\'');
        if k.eq_ignore_ascii_case("pv") {
            let v = v
                .trim()
                .trim_matches(',')
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if v.is_empty() {
                return None;
            }
            return Some(v);
        }
    }
    None
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

    // CA/PVA protocol links
    if let Some(rest) = s.strip_prefix("ca://") {
        return ParsedLink::Ca(rest.to_string());
    }
    if let Some(rest) = s.strip_prefix("pva://") {
        return ParsedLink::Pva(rest.to_string());
    }

    // Strip trailing link attributes: PP, NPP, CP, CPP, MS, NMS, MSS, MSI
    // They can appear in any order: "REC.FIELD NPP NMS", "REC CP", etc.
    let mut policy = LinkProcessPolicy::ProcessPassive;
    let mut ms = MonitorSwitch::NoMaximize;
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
        if let Some(rest) = trimmed
            .strip_suffix(" CP")
            .or_else(|| trimmed.strip_suffix(" CPP"))
        {
            policy = LinkProcessPolicy::ChannelProcess;
            link_part = rest;
            continue;
        }
        if let Some(rest) = trimmed.strip_suffix(" PP") {
            policy = LinkProcessPolicy::ProcessPassive;
            link_part = rest;
            continue;
        }
        link_part = trimmed;
        break;
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
            ParsedLink::Ca("FOO".to_string())
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
            ParsedLink::Ca("BAR".to_string())
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
}
