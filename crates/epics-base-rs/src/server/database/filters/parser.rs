//! Parse the channel-filter suffix from a CA / PVA channel name into
//! a populated [`FilterChain`].
//!
//! Wire syntax (epics-base 3.15.7 / pvxs `dbAccessChannelFilter`):
//!
//! * `TEMP` — no filter; equivalent to `(base, "VAL", None)`.
//! * `TEMP.VAL` — base record + explicit field.
//! * `TEMP.{"dbnd":{"d":0.5}}` — filter chain on `VAL` of `TEMP`.
//! * `TEMP.VAL.{"arr":{"s":0,"i":2,"e":-1}}` — explicit field +
//!   filter chain.
//! * Chained filters live in one JSON object — `TEMP.{"dec":{"n":3},
//!   "dbnd":{"d":1.0}}` decimates first, then deadbands the
//!   survivors. Order follows JSON object iteration (preserved by
//!   `serde_json`'s default `Map`, which is insertion-ordered when
//!   the `preserve_order` feature is off — pvxs callers should use
//!   one filter per chain entry in the literal order they want).
//!
//! Two parse entry points with different contracts:
//!
//! * [`try_parse_filter_chain`] is the channel-creation contract — it
//!   mirrors EPICS `dbChannelCreate()` and returns a
//!   [`FilterParseError`] (so the caller rejects the channel) on
//!   malformed JSON, a non-object body, an unknown filter name, or a
//!   filter whose configuration its own parser rejects.
//! * [`parse_filter_chain`] is the permissive forward-compatibility
//!   path retained for the CA server: it downgrades an unparseable
//!   suffix to an unfiltered subscription, skipping bad entries with a
//!   `tracing::warn!`.

use std::sync::Arc;

use super::{
    ArrayFilter, ArrayFilterConfig, DeadbandFilter, DeadbandMode, DecimateFilter, FilterChain,
    SubscriptionFilter,
};

/// Result of splitting a raw CA/PVA channel name into its three
/// components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedChannelName {
    /// Record + optional `.FIELD` portion (everything before the
    /// JSON suffix). Caller routes this through the existing
    /// `parse_pv_name` to get the (base, field) split.
    pub record_path: String,
    /// Raw JSON suffix from the channel name, or `None` if there
    /// wasn't one. Returned separately so callers can record /
    /// audit the original suffix even when the filter parse fails.
    pub json_suffix: Option<String>,
}

/// Split a raw channel name into the `record_path` + JSON suffix.
///
/// pvxs `test:ai.VAL{"dbnd":{"d":0.0}}`: the suffix
/// begins at the first `{` regardless of whether a separating `.`
/// precedes it. EPICS PV names never contain `{`, so the first
/// `{` is unambiguous. Accepts every pvxs-compatible form:
///
/// - `RECORD`            → no suffix.
/// - `RECORD.FIELD`      → no suffix.
/// - `RECORD{...}`       → suffix without field separator.
/// - `RECORD.{...}`      → suffix after bare separator (legacy CA).
/// - `RECORD.FIELD{...}` → suffix directly after field (pvxs).
/// - `RECORD.FIELD.{...}` → suffix after explicit separator.
///
/// A trailing legacy array-range modifier (`RECORD.VAL[start:incr:end]`)
/// is normalised into a canonical `arr` channel filter, matching EPICS
/// `dbChannel.c` `parseArrayRange` (dbChannel.c:351-446, 507-510): base
/// translates `[N]`, `[s:e]`, `[s:i:e]` into an `arr` filter inserted
/// *before* any JSON filters, so the slice applies first. Because every
/// CA/PVA consumer already builds its [`FilterChain`] from `json_suffix`,
/// emitting the range as `arr` here gives the legacy syntax full support
/// with no consumer changes — `split_channel_name` is the single owner of
/// "channel name → record_path + filters".
///
/// Returns the empty suffix when no `{` and no `[range]` appear.
pub fn split_channel_name(raw: &str) -> ParsedChannelName {
    // 1. Peel the JSON filter suffix at the first `{`. EPICS PV names
    //    never contain `{`, so the first `{` is unambiguous.
    let (name_part, json_suffix) = match raw.find('{') {
        Some(brace_pos) => {
            // Strip an optional `.` separator immediately before the
            // brace so `RECORD.{...}` and `RECORD.FIELD.{...}` produce a
            // clean record path without the dangling dot.
            let path_end = if brace_pos > 0 && raw.as_bytes()[brace_pos - 1] == b'.' {
                brace_pos - 1
            } else {
                brace_pos
            };
            (&raw[..path_end], Some(raw[brace_pos..].to_string()))
        }
        None => (raw, None),
    };

    // 2. Peel a trailing `[range]` array-range modifier off the name and
    //    fold it into the filter chain as a leading `arr` entry. Array
    //    range is channel syntax (base parses it after field modifiers,
    //    before JSON filters), not part of the field name.
    if let Some((record_path, arr_inner)) = peel_array_range(name_part) {
        let json_suffix = Some(match json_suffix {
            Some(existing) => merge_arr_into_json(&arr_inner, &existing),
            None => format!("{{{arr_inner}}}"),
        });
        return ParsedChannelName {
            record_path,
            json_suffix,
        };
    }

    ParsedChannelName {
        record_path: name_part.to_string(),
        json_suffix,
    }
}

/// A client-supplied channel name resolved into everything C
/// `dbChannelCreate` derives from one, in C's own order.
///
/// See [`parse_channel_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelName {
    /// `record[.FIELD]` with the filter suffix and the `$` modifier
    /// removed — the key [`PvDatabase::find_entry`](crate::server::database::PvDatabase::find_entry)
    /// and the CA CREATE_CHANNEL lookup use.
    pub record_path: String,
    /// The record (or alias) [`Self::record_path`] names, without the
    /// field.
    pub record: String,
    /// The addressed field, uppercased, defaulting to `VAL` when the
    /// client named none (`parse_pv_name`).
    pub field: String,
    /// The `$` long-string modifier was requested. Eligibility is the
    /// record's answer, not this parser's — see
    /// [`RecordInstance::resolve_string_view_field`](crate::server::record::RecordInstance::resolve_string_view_field).
    pub string_view: bool,
    /// Raw JSON filter suffix, or `None`. A `[range]` modifier arrives
    /// here already folded into a leading `arr` entry by
    /// [`split_channel_name`].
    pub json_suffix: Option<String>,
}

/// Resolve a raw CA/PVA channel name the way C `dbChannelCreate` does
/// (`dbChannel.c:440-530`): peel the `{json}` / `[range]` filter suffix,
/// then the `$` long-string modifier, then split `record.FIELD`.
///
/// The single owner of that three-step order, which matters: the `$` is
/// innermost (`REC.FIELD$[range]{json}`), so peeling it before the suffix
/// would miss it, and splitting on the last `.` before peeling the suffix
/// tears a `{"dbnd":{"d":0.5}}` apart at the `0.5`. Both CA CREATE_CHANNEL
/// and the two PVA sources resolve names through here, so the answer to
/// "which record and field does this channel address" cannot differ by
/// protocol.
///
/// Honouring the filter chain is the CALLER's half — this returns the raw
/// suffix. A caller that cannot honour it must refuse the channel rather
/// than serve it unfiltered: C's `chf_parse` failure path runs
/// `dbChannelDelete(chan); chan = NULL` at `finish:`
/// (`dbChannel.c:514-527`), so base never connects a channel whose filter
/// it could not build.
pub fn parse_channel_name(raw: &str) -> ChannelName {
    let parsed = split_channel_name(raw);
    let (core, string_view) = match parsed.record_path.strip_suffix('$') {
        Some(core) => (core, true),
        None => (parsed.record_path.as_str(), false),
    };
    let (record, field) = crate::server::database::parse_pv_name(core);
    ChannelName {
        record: record.to_string(),
        field: field.to_ascii_uppercase(),
        record_path: core.to_string(),
        string_view,
        json_suffix: parsed.json_suffix,
    }
}

/// Peel a trailing legacy array-range modifier (`[...]`) off a channel
/// name part, returning the stripped record path and the synthesised
/// inner `"arr":{...}` filter fragment. Returns `None` when there is no
/// trailing `[...]` or it does not parse as a valid range — the name is
/// then left untouched so the unresolved field still fails downstream
/// exactly as before, matching base rejecting `dbChannelCreate`.
fn peel_array_range(name_part: &str) -> Option<(String, String)> {
    let without_close = name_part.strip_suffix(']')?;
    let open = without_close.rfind('[')?;
    let inner = &without_close[open + 1..];
    let (start, incr, end) = parse_array_range(inner)?;
    Some((
        name_part[..open].to_string(),
        build_arr_inner(start, incr, end),
    ))
}

/// Parse the interior of a `[...]` array-range modifier into
/// `(start, incr, end)`, mirroring EPICS `dbChannel.c` `parseArrayRange`
/// (dbChannel.c:351-399). Accepts `[N]`, `[start:end]`, and
/// `[start:incr:end]`; any numeric position may be omitted
/// (`[:end]`, `[start:]`, `[start::end]`, `[:]`) and falls back to the
/// base defaults start `0`, incr `1`, end `-1` (full array). Returns
/// `None` for forms base also rejects: empty `[]`, a non-numeric
/// position, or more than three colon-separated parts.
fn parse_array_range(inner: &str) -> Option<(i64, i64, i64)> {
    // Outer `None` ⇒ non-numeric (reject); inner `None` ⇒ empty (use
    // the position default).
    let part = |s: &str| -> Option<Option<i64>> {
        let t = s.trim();
        if t.is_empty() {
            Some(None)
        } else {
            t.parse::<i64>().ok().map(Some)
        }
    };
    match inner.split(':').collect::<Vec<_>>().as_slice() {
        // `[N]`: single element start==end==N. Base requires a number
        // here — `[]` is rejected.
        [one] => {
            let n = part(one)??;
            Some((n, 1, n))
        }
        [s, e] => Some((part(s)?.unwrap_or(0), 1, part(e)?.unwrap_or(-1))),
        [s, i, e] => Some((
            part(s)?.unwrap_or(0),
            part(i)?.unwrap_or(1),
            part(e)?.unwrap_or(-1),
        )),
        _ => None,
    }
}

/// Build the inner `"arr":{...}` filter fragment, emitting only the
/// non-default positions (`s`≠0, `i`≠1, `e`≠-1) exactly as base's
/// `parseArrayRange` does (dbChannel.c:422-433). A full-array range
/// (`[:]`) yields `"arr":{}`, the identity slice.
fn build_arr_inner(start: i64, incr: i64, end: i64) -> String {
    let mut keys: Vec<String> = Vec::new();
    if start != 0 {
        keys.push(format!("\"s\":{start}"));
    }
    if incr != 1 {
        keys.push(format!("\"i\":{incr}"));
    }
    if end != -1 {
        keys.push(format!("\"e\":{end}"));
    }
    format!("\"arr\":{{{}}}", keys.join(","))
}

/// Prepend a synthesised `arr` filter into an existing JSON filter
/// suffix so the slice applies first (base inserts the array-range
/// filter before JSON filters; `serde_json`'s `preserve_order` keeps the
/// leading `arr` first in iteration). The existing suffix is spliced as
/// raw text rather than re-parsed, so JSON5 forms survive untouched for
/// the downstream chain parser.
fn merge_arr_into_json(arr_inner: &str, existing: &str) -> String {
    let body = existing
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .map(str::trim)
        .unwrap_or("");
    if body.is_empty() {
        format!("{{{arr_inner}}}")
    } else {
        format!("{{{arr_inner},{body}}}")
    }
}

/// Error returned by [`try_parse_filter_chain`] when a channel-filter
/// suffix is syntactically present but cannot be turned into the
/// requested chain.
///
/// EPICS base `dbChannelCreate()` aborts channel creation whenever its
/// filter parser reports a status (`dbChannel.c:512-529`): a malformed
/// body, an unknown filter name (`parse_stop` → `S_db_notFound`,
/// `dbChannel.c:176-182`), or a filter whose own parser rejects its
/// configuration (`chf_value` turns `parse_end` failure into
/// `parse_stop`, `dbChannel.c:72-85`). This type names which of those
/// hard failures occurred so the channel-creation boundary can reject
/// rather than silently downgrade to an unfiltered stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterParseError {
    /// The suffix was not valid JSON (after JSON5 key normalization).
    InvalidJson { suffix: String, message: String },
    /// The suffix parsed but its top-level value is not an object.
    NotObject { suffix: String },
    /// A requested filter name is not implemented (EPICS `parse_stop`
    /// → `S_db_notFound`).
    UnknownFilter { name: String },
    /// A known filter was handed a key its `chfPluginArgDef opts[]`
    /// table does not define (EPICS `parse_map_key` → `parse_stop`,
    /// `chfPlugin.c:472-474`).
    UnknownOption { filter: String, key: String },
    /// A known filter rejected its configuration body (EPICS
    /// `chf_value` / `parse_end` failure → `parse_stop`).
    BadConfig { name: String, config: String },
}

impl std::fmt::Display for FilterParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson { suffix, message } => {
                write!(f, "invalid channel-filter JSON `{suffix}`: {message}")
            }
            Self::NotObject { suffix } => {
                write!(f, "channel-filter body `{suffix}` is not a JSON object")
            }
            Self::UnknownFilter { name } => write!(f, "unknown channel filter `{name}`"),
            Self::UnknownOption { filter, key } => {
                write!(f, "channel filter `{filter}` has no option `{key}`")
            }
            Self::BadConfig { name, config } => {
                write!(
                    f,
                    "channel filter `{name}` rejected its configuration `{config}`"
                )
            }
        }
    }
}

impl std::error::Error for FilterParseError {}

/// Parse the JSON suffix into a [`FilterChain`], rejecting any
/// syntactically-present-but-unparseable filter request.
///
/// This is the channel-creation contract: it mirrors EPICS base
/// `dbChannelCreate()` / `chf_parse()`, which abort channel creation on
/// malformed JSON, a non-object body, an unknown filter name, or a
/// filter whose own parser rejects its configuration. An empty object
/// (`{}`) is a valid no-filter request and yields an empty chain. The
/// documented JSON5 unquoted-key forms are accepted via
/// [`crate::json5::relaxed_to_strict`].
pub fn try_parse_filter_chain(json: &str) -> Result<FilterChain, FilterParseError> {
    let normalized =
        crate::json5::relaxed_to_strict(json).map_err(|e| FilterParseError::InvalidJson {
            suffix: json.to_string(),
            message: e.to_string(),
        })?;
    let value: serde_json::Value =
        serde_json::from_str(&normalized).map_err(|e| FilterParseError::InvalidJson {
            suffix: json.to_string(),
            message: e.to_string(),
        })?;
    let obj = value
        .as_object()
        .ok_or_else(|| FilterParseError::NotObject {
            suffix: json.to_string(),
        })?;
    let mut chain = FilterChain::new();
    for (key, cfg) in obj {
        chain.push(build_filter(key, cfg)?);
    }
    Ok(chain)
}

/// Permissive parse retained for the CA-server forward-compatibility
/// path (`epics-ca-rs` server `tcp.rs`), which historically downgrades
/// an unparseable suffix to an unfiltered subscription rather than
/// failing the request. Returns an empty chain on JSON / non-object
/// failure and skips individual unparseable entries with a
/// `tracing::warn!`. New channel-creation boundaries must use
/// [`try_parse_filter_chain`] so a bad suffix rejects the channel
/// (EPICS `dbChannelCreate` parity) instead of silently changing the
/// requested semantics. JSON5 unquoted keys are accepted here too.
pub fn parse_filter_chain(json: &str) -> FilterChain {
    let mut chain = FilterChain::new();
    let normalized = match crate::json5::relaxed_to_strict(json) {
        Ok(normalized) => normalized,
        Err(e) => {
            tracing::warn!(
                json = %json,
                error = %e,
                "channel filter JSON parse failed; subscription proceeds without filters",
            );
            return chain;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&normalized) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                json = %json,
                error = %e,
                "channel filter JSON parse failed; subscription proceeds without filters",
            );
            return chain;
        }
    };
    let Some(obj) = value.as_object() else {
        tracing::warn!(json = %json, "channel filter JSON must be an object");
        return chain;
    };
    for (key, cfg) in obj {
        match build_filter(key, cfg) {
            Ok(filt) => chain.push(filt),
            Err(e) => {
                tracing::warn!(error = %e, "channel filter skipped");
            }
        }
    }
    chain
}

/// The `chfPluginEnumType` maps of the three enum-typed plugins, and the
/// single owner of every filter-option keyword the port recognises.
///
/// `sync`'s six mode-tagged string options carry exactly the `modeEnum`
/// names and choices (`sync.c:36-45`, `:61-66`), so one table serves the
/// long form and the shorthand.
const DBND_MODES: &[(&str, i32)] = &[("abs", 0), ("rel", 1)];
const SYNC_MODES: &[(&str, i32)] = &[
    ("before", 0),
    ("first", 1),
    ("last", 2),
    ("after", 3),
    ("while", 4),
    ("unless", 5),
];
/// `ts.c:46-47` / `:54` / `:62` — three separate enums, so the same
/// choice number means different things under `num`, `epoch` and `str`.
const TS_NUM: &[(&str, i32)] = &[("dbl", 2), ("sec", 3), ("nsec", 4), ("ts", 5)];
const TS_EPOCH: &[(&str, i32)] = &[("epics", 0), ("unix", 1)];
const TS_STR: &[(&str, i32)] = &[("epics", 1), ("iso", 2)];

/// The `optType` half of a `chfPluginArgDef` row (`chfPlugin.h:221-227`).
/// No shipped filter declares a Boolean option, so there is no variant
/// for one.
#[derive(Clone, Copy)]
enum ArgType {
    Int32,
    Double,
    Str,
    Enum(&'static [(&'static str, i32)]),
}

/// One row of a plugin's `chfPluginArgDef opts[]`: the key, the type its
/// value is stored as, and C's per-option `convert` flag — the last
/// positional argument of every `chf*` macro (`chfPlugin.h:249-291`).
///
/// `convert` is not decoration. `store_integer_value` opens with
/// `if (!opt->convert && opt->optType != chfPluginArgInt32) return -1;`
/// (`chfPlugin.c:80-82`) and its string twin with the `String`/`Enum`
/// exemption (`:248-251`), so the flag alone decides whether a JSON
/// value of the "wrong" kind converts into the option's type or stops
/// the parse. The port had no representation of it, and the two
/// directions it got wrong point opposite ways: `{"ts":{"num":2}}`
/// connected here and is refused by C, while `{"sync":{"m":2,"s":"S"}}`
/// was refused here and connects in C.
struct ArgDef {
    name: &'static str,
    ty: ArgType,
    convert: bool,
}

const fn arg(name: &'static str, ty: ArgType, convert: bool) -> ArgDef {
    ArgDef { name, ty, convert }
}

/// One option's stored value, in the type its [`ArgDef`] names.
#[derive(Clone, Debug, PartialEq)]
enum ArgValue {
    Int(i32),
    Double(f64),
    Str(String),
    /// The `chfPluginEnumType` *choice*, never the spelling that selected
    /// it — C stores `emap->value` into an `int` member, so `"last"` and
    /// `2` reach the plugin identically.
    Enum(i32),
}

impl ArgValue {
    fn as_int(&self) -> Option<i32> {
        match self {
            Self::Int(v) => Some(*v),
            _ => None,
        }
    }
    fn as_double(&self) -> Option<f64> {
        match self {
            Self::Double(v) => Some(*v),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(v) => Some(v),
            _ => None,
        }
    }
    fn as_choice(&self) -> Option<i32> {
        match self {
            Self::Enum(v) => Some(*v),
            _ => None,
        }
    }
}

/// The options one filter body stored, in JSON order.
///
/// A builder never sees raw JSON, so there is no second place where a
/// value's kind is interpreted — and no accessor here can observe a kind
/// other than the one its [`ArgDef`] names, because [`store`] produced it
/// from that same row. Order is preserved because C's tagged options
/// (`dbnd`'s `abs`/`rel`, `sync`'s six mode names) set the mode when the
/// KEY is seen (`chfPlugin.c:476-479`), so a later key overrides an
/// earlier one.
struct Opts(Vec<(&'static str, ArgValue)>);

impl Opts {
    fn iter(&self) -> impl Iterator<Item = (&'static str, &ArgValue)> {
        self.0.iter().map(|(k, v)| (*k, v))
    }
    fn get(&self, key: &str) -> Option<&ArgValue> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
    fn int(&self, key: &str) -> Option<i32> {
        self.get(key)?.as_int()
    }
    fn choice(&self, key: &str) -> Option<i32> {
        self.get(key)?.as_choice()
    }
}

/// C `epicsParseInt32(val, ival, 0, &end)` as `store_string_value` calls
/// it (`chfPlugin.c:253`): leading whitespace skipped, `strtol` with base
/// 0 so `0x`/`0` prefixes are honoured, at least one digit required, and
/// trailing text ALLOWED because the `units` out-parameter is non-NULL
/// and then ignored (`epicsStdlib.c:33-53`).
fn epics_parse_int32(s: &str) -> Option<i32> {
    let s = s.trim_start();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (radix, digits) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(rest) => (16, rest),
        None if s.starts_with('0') => (8, s),
        None => (10, s),
    };
    let len = digits.chars().take_while(|c| c.is_digit(radix)).count();
    if len == 0 {
        // `strtol` consumed the leading `0` of a bare `0x` and stopped.
        return (radix == 16).then_some(0);
    }
    let mag = i64::from_str_radix(&digits[..len], radix).ok()?;
    i32::try_from(if neg { -mag } else { mag }).ok()
}

/// C `epicsParseDouble(val, dval, &end)` (`chfPlugin.c:273`): the longest
/// numeric prefix wins and the remainder lands in the ignored `units`
/// pointer, so `"0.5 V"` stores 0.5 exactly as `"0.5"` does.
///
/// `strtod` reports an out-of-range result through `errno`, and
/// `epicsParseDouble` turns that into `S_stdlib_overflow` /
/// `S_stdlib_underflow` (`epicsStdlib.c:159-165`) — an error return, so
/// the store fails and the channel is refused. Rust's `f64::from_str`
/// saturates instead, silently storing `inf` for `"1e400"` and `0` for
/// `"1e-400"`, which is the class of silent wrong value this whole
/// convert stage exists to prevent. `"inf"` / `"nan"` are word forms
/// `strtod` accepts without setting `errno`, so they still store.
fn epics_parse_double(s: &str) -> Option<f64> {
    let s = s.trim_start();
    let (v, token) = longest_f64_prefix(s)?;
    let digits = token.trim_start_matches(['+', '-']);
    if digits.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Some(v);
    }
    let mantissa = digits.split(['e', 'E']).next().unwrap_or("");
    let significant = mantissa.chars().any(|c| c.is_ascii_digit() && c != '0');
    if v.is_infinite() || (significant && v == 0.0) {
        return None;
    }
    Some(v)
}

/// The prefix of `s` that `strtod` would consume, and its value.
fn longest_f64_prefix(s: &str) -> Option<(f64, &str)> {
    let mut longest = None;
    for (i, _) in s.char_indices().skip(1) {
        if let Ok(v) = s[..i].parse::<f64>() {
            longest = Some((v, &s[..i]));
        }
    }
    if let Ok(v) = s.parse::<f64>() {
        longest = Some((v, s));
    }
    longest
}

/// C `store_integer_value` (`chfPlugin.c:64-121`).
fn store_integer(def: &ArgDef, val: i32) -> Option<ArgValue> {
    if !def.convert && !matches!(def.ty, ArgType::Int32) {
        return None;
    }
    Some(match def.ty {
        ArgType::Int32 => ArgValue::Int(val),
        ArgType::Double => ArgValue::Double(f64::from(val)),
        ArgType::Str => ArgValue::Str(val.to_string()),
        // The Enum arm matches the numeric VALUE against the map, never
        // a name (`:104-115`) — this is the arm `{"sync":{"m":2}}` needs.
        ArgType::Enum(map) => {
            let (_, choice) = map.iter().find(|(_, c)| *c == val)?;
            ArgValue::Enum(*choice)
        }
    })
}

/// C `store_double_value` (`chfPlugin.c:170-222`).
fn store_double(def: &ArgDef, val: f64) -> Option<ArgValue> {
    if !def.convert && !matches!(def.ty, ArgType::Double) {
        return None;
    }
    Some(match def.ty {
        // Range-checked against INT_MIN..INT_MAX and then truncated by
        // the `(epicsInt32)` cast (`:194-199`). A NaN passes C's pair of
        // `<`/`>` comparisons into an undefined cast; it is rejected here.
        ArgType::Int32 => {
            if !(f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&val) {
                return None;
            }
            ArgValue::Int(val as i32)
        }
        ArgType::Double => ArgValue::Double(val),
        ArgType::Str => ArgValue::Str(format!("{val}")),
        // C has no Enum arm here — it returns -1 (`:218-220`).
        ArgType::Enum(_) => return None,
    })
}

/// C `store_string_value` (`chfPlugin.c:228-290`). The `String`/`Enum`
/// exemption is why a non-converting `chfEnum` such as `ts`'s `num` still
/// accepts `"dbl"` while refusing `2`.
fn store_string(def: &ArgDef, val: &str) -> Option<ArgValue> {
    if !def.convert && !matches!(def.ty, ArgType::Str | ArgType::Enum(_)) {
        return None;
    }
    Some(match def.ty {
        ArgType::Int32 => ArgValue::Int(epics_parse_int32(val)?),
        ArgType::Double => ArgValue::Double(epics_parse_double(val)?),
        ArgType::Str => ArgValue::Str(val.to_string()),
        ArgType::Enum(map) => {
            let (_, choice) = map.iter().find(|(n, _)| *n == val)?;
            ArgValue::Enum(*choice)
        }
    })
}

/// C `store_boolean_value` (`chfPlugin.c:127-165`). Its guard exempts
/// `chfPluginArgBoolean`, which no shipped filter declares, so a JSON
/// `true`/`false` reaches an option only when that option converts.
fn store_boolean(def: &ArgDef, val: bool) -> Option<ArgValue> {
    if !def.convert {
        return None;
    }
    let v = i32::from(val);
    Some(match def.ty {
        ArgType::Int32 => ArgValue::Int(v),
        ArgType::Double => ArgValue::Double(f64::from(v)),
        ArgType::Str => ArgValue::Str(if val { "true" } else { "false" }.to_string()),
        // C returns -1 for Enum (`:160-162`).
        ArgType::Enum(_) => return None,
    })
}

/// Store one option value, dispatching on the JSON value's kind exactly
/// as yajl's `parse_integer` / `parse_double` / `parse_string` /
/// `parse_boolean` callbacks do (`chfPlugin.c:392-448`).
///
/// `None` is C's non-zero store return, which every one of those
/// callbacks turns into `parse_stop` — and `parse_stop` fails
/// `dbChannelCreate`, so the channel is refused rather than built with a
/// defaulted option.
fn store(def: &ArgDef, value: &serde_json::Value) -> Option<ArgValue> {
    use serde_json::Value;
    match value {
        Value::Number(n) if n.is_f64() => store_double(def, n.as_f64()?),
        Value::Number(n) => {
            // `parse_integer` range-checks yajl's `long` into epicsInt32
            // before any store, whatever the option's type (`:409-413`).
            store_integer(def, i32::try_from(n.as_i64()?).ok()?)
        }
        Value::String(s) => store_string(def, s),
        Value::Bool(b) => store_boolean(def, *b),
        // yajl registers no null callback, and a nested map or array is
        // not an option value; all three end the parse.
        _ => None,
    }
}

/// The filter registry `chfPluginRegister` builds in C: one row per
/// plugin, carrying its whole `chfPluginArgDef opts[]` table beside its
/// parser.
///
/// Keeping the two together is what makes C's `parse_map_key` rejection
/// (`chfPlugin.c:472-474`) and its `store_*_value` conversion rules
/// (`:64-290`) unforgettable when a filter is added. The port previously
/// had a bare key list and no types at all: every `build_*` read raw JSON
/// with `as_i64`/`as_str`/`as_f64`, so a value of the wrong kind became
/// the option's default instead of failing the channel.
const PLUGINS: &[(&str, &[ArgDef], FilterBuilder)] = &[
    // `dbnd.c:38-43`.
    (
        "dbnd",
        &[
            arg("d", ArgType::Double, true),
            arg("m", ArgType::Enum(DBND_MODES), true),
            arg("abs", ArgType::Double, true),
            arg("rel", ArgType::Double, true),
        ],
        build_dbnd,
    ),
    // `arr.c:35-38`.
    (
        "arr",
        &[
            arg("s", ArgType::Int32, true),
            arg("i", ArgType::Int32, true),
            arg("e", ArgType::Int32, true),
        ],
        build_arr,
    ),
    // `ts.c:70-75` — every option `convert = 0`, so a numeric `num` /
    // `epoch` / `str` stops the parse.
    (
        "ts",
        &[
            arg("num", ArgType::Enum(TS_NUM), false),
            arg("epoch", ArgType::Enum(TS_EPOCH), false),
            arg("str", ArgType::Enum(TS_STR), false),
        ],
        build_ts,
    ),
    // `decimate.c:33-36` — `n` alone, `convert = 0`.
    ("dec", &[arg("n", ArgType::Int32, false)], build_decimate),
    // `sync.c:57-67`: `m` is the enum and converts; `s` and the six
    // mode-tagged names are plain strings that do not.
    (
        "sync",
        &[
            arg("m", ArgType::Enum(SYNC_MODES), true),
            arg("s", ArgType::Str, false),
            arg("before", ArgType::Str, false),
            arg("first", ArgType::Str, false),
            arg("last", ArgType::Str, false),
            arg("after", ArgType::Str, false),
            arg("while", ArgType::Str, false),
            arg("unless", ArgType::Str, false),
        ],
        build_sync,
    ),
    // `utag.c:20-25`.
    (
        "utag",
        &[
            arg("M", ArgType::Int32, true),
            arg("V", ArgType::Int32, true),
        ],
        build_utag,
    ),
];

type FilterBuilder = fn(&Opts) -> Option<Arc<dyn SubscriptionFilter>>;

/// Build one filter from its name + config, running the three stages C
/// has: `parse_map_key`'s unknown-key stop (`chfPlugin.c:472-474`), the
/// `store_*_value` conversion its `convert` flag governs (`:64-290`), and
/// the plugin's own `parse_ok` (each `build_*` answering `None`).
/// Distinguishing an unknown filter name from a rejected configuration
/// mirrors EPICS `S_db_notFound` vs `parse_stop`.
fn build_filter(
    name: &str,
    cfg: &serde_json::Value,
) -> Result<Arc<dyn SubscriptionFilter>, FilterParseError> {
    let (_, defs, build) = PLUGINS.iter().find(|(n, _, _)| *n == name).ok_or_else(|| {
        FilterParseError::UnknownFilter {
            name: name.to_string(),
        }
    })?;
    let bad_config = || FilterParseError::BadConfig {
        name: name.to_string(),
        config: cfg.to_string(),
    };
    let obj = cfg.as_object().ok_or_else(bad_config)?;
    let mut stored = Vec::with_capacity(obj.len());
    for (key, value) in obj {
        let def =
            defs.iter()
                .find(|d| d.name == key)
                .ok_or_else(|| FilterParseError::UnknownOption {
                    filter: name.to_string(),
                    key: key.clone(),
                })?;
        stored.push((def.name, store(def, value).ok_or_else(bad_config)?));
    }
    build(&Opts(stored)).ok_or_else(bad_config)
}

/// C `ts.c` JSON schema:
/// * `{"ts":{}}` — default Generate mode.
/// * `{"ts":{"num":"dbl"|"sec"|"nsec"|"ts","epoch":"epics"|"unix"}}` —
///   numeric / array output, optional epoch override.
/// * `{"ts":{"str":"epics"}}` — formatted string output.
fn build_ts(opts: &Opts) -> Option<Arc<dyn SubscriptionFilter>> {
    use super::ts::{TimestampFilter, TsEpoch, TsMode};
    let epoch = match opts.choice("epoch") {
        // `ts_epoch_enum` (`ts.c:54`).
        Some(1) => TsEpoch::Unix,
        _ => TsEpoch::Epics,
    };
    // `parse_finished` (`ts.c:77-90`): any `str` choice overrides `num`
    // outright, and only when there is none does `num` apply.
    if let Some(choice) = opts.choice("str") {
        // `ts_string_enum` (`ts.c:62`). C `ts.c:250` gives `iso` its own
        // format ("%Y-%m-%dT%H:%M:%S.%06f%z"); only VS2012 rejects it.
        let mode = if choice == 2 {
            TsMode::StringIso
        } else {
            TsMode::StringEpics
        };
        return Some(Arc::new(TimestampFilter::with_mode(mode)));
    }
    // `ts_numeric_enum` (`ts.c:46-47`); absent leaves `tsModeInvalid`,
    // which `parse_finished` turns into `tsModeGenerate`.
    let mode = match opts.choice("num") {
        None => TsMode::Generate,
        Some(2) => TsMode::Double,
        Some(3) => TsMode::Seconds,
        Some(4) => TsMode::Nanoseconds,
        _ => TsMode::Array,
    };
    Some(Arc::new(TimestampFilter::with_mode_epoch(mode, epoch)))
}

/// C `dbnd.c:36-45` schema: `modeEnum {"abs"=>0,"rel"=>1}`; `d` sets the
/// delta and `m` the mode, while the tagged `abs`/`rel` keys set both at
/// once. There is NO `r` key. The delta reaches [`DeadbandFilter`] in C's
/// own wire units — a percent in `rel` mode — because C's band refresh is
/// written in them (`my->hyst = val * my->cval/100.`, `dbnd.c:87`).
fn build_dbnd(opts: &Opts) -> Option<Arc<dyn SubscriptionFilter>> {
    let mut cval = None;
    let mut mode = DeadbandMode::Absolute;
    for (name, value) in opts.iter() {
        match name {
            "d" => cval = Some(value.as_double()?),
            "m" => mode = dbnd_mode(value.as_choice()?),
            // `chfTagDouble`: `parse_map_key` writes the tag's choice into
            // `mode` as soon as the KEY is seen (`chfPlugin.c:476-479`),
            // so whichever of `m` / `abs` / `rel` comes last in the JSON
            // decides the mode.
            "abs" => {
                mode = DeadbandMode::Absolute;
                cval = Some(value.as_double()?);
            }
            _ => {
                mode = DeadbandMode::Relative;
                cval = Some(value.as_double()?);
            }
        }
    }
    Some(Arc::new(DeadbandFilter::new(cval?, mode)))
}

fn dbnd_mode(choice: i32) -> DeadbandMode {
    if choice == 1 {
        DeadbandMode::Relative
    } else {
        DeadbandMode::Absolute
    }
}

/// C `utag.c:20-25`: `M` and `V` are both optional, and `allocPvt`
/// pre-seeds `mask` to `0xffffffff` before parsing (`:27-33`), so
/// `{"utag":{}}` is a legal filter that passes only a zero user tag.
fn build_utag(opts: &Opts) -> Option<Arc<dyn SubscriptionFilter>> {
    use super::utag::UserTagFilter;
    Some(Arc::new(UserTagFilter::new(
        opts.int("M").unwrap_or(UserTagFilter::DEFAULT_MASK),
        opts.int("V").unwrap_or(0),
    )))
}

fn build_arr(opts: &Opts) -> Option<Arc<dyn SubscriptionFilter>> {
    // `arr.c:22-26` calloc's the private struct and `parse_ok` leaves an
    // absent bound alone, so the defaults are C's: whole array, stride 1.
    let start = opts.int("s").unwrap_or(0);
    let incr = opts.int("i").unwrap_or(1);
    let end = opts.int("e").unwrap_or(-1);
    // `ArrayFilterConfig::new` clamps `incr` to `>= 1`, so a malicious
    // `{"i":0}` / `{"i":-3}` cannot reach the slice divisor.
    Some(Arc::new(ArrayFilter::new(ArrayFilterConfig::new(
        i64::from(start),
        i64::from(incr),
        i64::from(end),
    ))))
}

fn build_decimate(opts: &Opts) -> Option<Arc<dyn SubscriptionFilter>> {
    // `n` is the only key and it is required (`decimate.c:34`,
    // `chfInt32(myStruct, n, "n", 1, 0)`); the `n < 1` rejection is
    // `decimate.c`'s own `parse_ok`, owned by `DecimateFilter::new`, and
    // a negative `n` cannot become a `u64` at all.
    let n = u64::try_from(opts.int("n")?).ok()?;
    DecimateFilter::new(n).map(|f| Arc::new(f) as Arc<dyn SubscriptionFilter>)
}

/// `sync` filter — six gating modes on a named [`super::DbState`].
///
/// Long form (matches upstream `chfPlugin` field tags):
/// `{"sync":{"m":"after","s":"STATE_NAME"}}`
///
/// Mode-tagged shorthand (also upstream-supported):
/// `{"sync":{"after":"STATE_NAME"}}` — the key acts as the mode keyword
/// and the value carries the state name, and one such key satisfies both
/// required options because `parse_map_key` marks every option sharing
/// its data and tag offsets as found (`chfPlugin.c:481-487`).
///
/// `sync.c`'s `parse_ok` additionally rejects a state name `dbStateFind`
/// does not know (`sync.c:87-93`), so a filter naming a state no record
/// has declared fails channel creation instead of gating on an invented
/// one.
fn build_sync(opts: &Opts) -> Option<Arc<dyn SubscriptionFilter>> {
    let mut mode = None;
    let mut state = None;
    for (name, value) in opts.iter() {
        match name {
            "m" => mode = Some(value.as_choice()?),
            "s" => state = Some(value.as_str()?),
            // `chfTagString`: the tag names the mode and the value names
            // the state, both written when the key is seen.
            tag => {
                let (_, choice) = SYNC_MODES.iter().find(|(n, _)| *n == tag)?;
                mode = Some(*choice);
                state = Some(value.as_str()?);
            }
        }
    }
    let state = super::db_state_registry().find(state?)?;
    Some(Arc::new(super::SyncFilter::new(sync_mode(mode?)?, state)))
}

/// `modeEnum` choice to [`super::SyncMode`] (`sync.c:27-34`). The
/// keyword half of that map lives in [`SYNC_MODES`], which is where the
/// wire spelling — long form or mode tag — is resolved.
fn sync_mode(choice: i32) -> Option<super::SyncMode> {
    use super::SyncMode;
    Some(match choice {
        0 => SyncMode::Before,
        1 => SyncMode::First,
        2 => SyncMode::Last,
        3 => SyncMode::After,
        4 => SyncMode::While,
        5 => SyncMode::Unless,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_plain_record_name() {
        let p = split_channel_name("TEMP");
        assert_eq!(p.record_path, "TEMP");
        assert!(p.json_suffix.is_none());
    }

    #[test]
    fn split_record_dot_field() {
        let p = split_channel_name("TEMP.VAL");
        assert_eq!(p.record_path, "TEMP.VAL");
        assert!(p.json_suffix.is_none());
    }

    #[test]
    fn split_filter_only_no_field() {
        let p = split_channel_name(r#"TEMP.{"dbnd":{"d":0.5}}"#);
        assert_eq!(p.record_path, "TEMP");
        assert_eq!(p.json_suffix.as_deref(), Some(r#"{"dbnd":{"d":0.5}}"#));
    }

    #[test]
    fn split_field_then_filter() {
        let p = split_channel_name(r#"TEMP.VAL.{"dbnd":{"d":0.5}}"#);
        assert_eq!(p.record_path, "TEMP.VAL");
        assert_eq!(p.json_suffix.as_deref(), Some(r#"{"dbnd":{"d":0.5}}"#));
    }

    /// pvxs `test:ai.VAL{...}` (no separating `.`): the
    /// first `{` always begins the suffix, the optional `.`
    /// immediately before it is consumed so the record_path is
    /// clean.
    #[test]
    fn split_pvxs_field_directly_followed_by_filter() {
        let p = split_channel_name(r#"test:ai.VAL{"dbnd":{"d":0.0}}"#);
        assert_eq!(p.record_path, "test:ai.VAL");
        assert_eq!(p.json_suffix.as_deref(), Some(r#"{"dbnd":{"d":0.0}}"#));
    }

    /// `RECORD{...}` without any field component.
    #[test]
    fn split_record_directly_followed_by_filter() {
        let p = split_channel_name(r#"TEMP{"arr":{"s":0,"i":1,"e":4}}"#);
        assert_eq!(p.record_path, "TEMP");
        assert_eq!(
            p.json_suffix.as_deref(),
            Some(r#"{"arr":{"s":0,"i":1,"e":4}}"#)
        );
    }

    /// Legacy `[N]` single-element range → `arr` with start==end==N
    /// (base `parseArrayRange` sets `end=start` for the one-number form).
    #[test]
    fn split_array_range_single_element() {
        let p = split_channel_name("WF.VAL[3]");
        assert_eq!(p.record_path, "WF.VAL");
        assert_eq!(p.json_suffix.as_deref(), Some(r#"{"arr":{"s":3,"e":3}}"#));
    }

    /// `[start:end]` → `arr` with the bounds, default stride.
    #[test]
    fn split_array_range_start_end() {
        let p = split_channel_name("WF.VAL[2:5]");
        assert_eq!(p.record_path, "WF.VAL");
        assert_eq!(p.json_suffix.as_deref(), Some(r#"{"arr":{"s":2,"e":5}}"#));
    }

    /// `[start:incr:end]` → `arr` carrying the stride.
    #[test]
    fn split_array_range_start_incr_end() {
        let p = split_channel_name("WF.VAL[0:2:10]");
        assert_eq!(p.record_path, "WF.VAL");
        assert_eq!(p.json_suffix.as_deref(), Some(r#"{"arr":{"i":2,"e":10}}"#));
    }

    /// Range with no field component defaults the field to `VAL`
    /// downstream; only the record path is stripped here.
    #[test]
    fn split_array_range_no_field() {
        let p = split_channel_name("WF[1:4]");
        assert_eq!(p.record_path, "WF");
        assert_eq!(p.json_suffix.as_deref(), Some(r#"{"arr":{"s":1,"e":4}}"#));
    }

    /// Default positions are omitted; a full-array `[:]` is the identity
    /// `arr` filter.
    #[test]
    fn split_array_range_open_forms() {
        assert_eq!(
            split_channel_name("WF.VAL[:5]").json_suffix.as_deref(),
            Some(r#"{"arr":{"e":5}}"#)
        );
        assert_eq!(
            split_channel_name("WF.VAL[2:]").json_suffix.as_deref(),
            Some(r#"{"arr":{"s":2}}"#)
        );
        assert_eq!(
            split_channel_name("WF.VAL[:]").json_suffix.as_deref(),
            Some(r#"{"arr":{}}"#)
        );
    }

    /// A range combined with a JSON filter folds the `arr` in first so
    /// the slice applies before the stream filter (base inserts the
    /// array-range filter ahead of JSON filters; `preserve_order` keeps
    /// `arr` leading).
    #[test]
    fn split_array_range_with_json_filter() {
        let p = split_channel_name(r#"WF.VAL[2:5]{"dbnd":{"d":0.5}}"#);
        assert_eq!(p.record_path, "WF.VAL");
        assert_eq!(
            p.json_suffix.as_deref(),
            Some(r#"{"arr":{"s":2,"e":5},"dbnd":{"d":0.5}}"#)
        );
        // The merged suffix is a valid chain with arr first.
        let chain = parse_filter_chain(p.json_suffix.as_deref().unwrap());
        let names: Vec<&'static str> = chain.iter().map(|f| f.name()).collect();
        assert_eq!(names, vec!["arr", "dbnd"]);
    }

    /// Malformed or empty ranges are left in the name untouched (base
    /// rejects them at channel creation; here they fall through to a
    /// field-not-found downstream).
    #[test]
    fn split_array_range_invalid_left_intact() {
        for raw in ["WF.VAL[]", "WF.VAL[a:b]", "WF.VAL[1:2:3:4]"] {
            let p = split_channel_name(raw);
            assert_eq!(p.record_path, raw, "invalid range must stay in the name");
            assert!(p.json_suffix.is_none());
        }
    }

    #[test]
    fn parse_dbnd_absolute() {
        let chain = parse_filter_chain(r#"{"dbnd":{"d":0.5}}"#);
        assert_eq!(chain.len(), 1);
        let names: Vec<&'static str> = chain.iter().map(|f| f.name()).collect();
        assert_eq!(names, vec!["dbnd"]);
    }

    #[test]
    fn parse_dbnd_relative_rel_key_and_d_m() {
        // C `dbnd.c` accepts `{"rel":X}` and `{"d":X,"m":"rel"}` for
        // relative mode; both build a one-filter chain.
        assert_eq!(parse_filter_chain(r#"{"dbnd":{"rel":50}}"#).len(), 1);
        assert_eq!(
            parse_filter_chain(r#"{"dbnd":{"d":50,"m":"rel"}}"#).len(),
            1
        );
        // `{"abs":X}` and `{"d":X,"m":"abs"}` build absolute mode.
        assert_eq!(parse_filter_chain(r#"{"dbnd":{"abs":0.5}}"#).len(), 1);
        assert_eq!(
            parse_filter_chain(r#"{"dbnd":{"d":0.5,"m":"abs"}}"#).len(),
            1
        );
    }

    #[test]
    fn parse_dbnd_rejects_fabricated_r_key_and_bad_mode() {
        // The fabricated `r` key C never had now matches nothing — the
        // filter is dropped (empty chain).
        assert_eq!(parse_filter_chain(r#"{"dbnd":{"r":0.01}}"#).len(), 0);
        // An `m` value outside modeEnum fails the parse like C chfEnum.
        assert_eq!(
            parse_filter_chain(r#"{"dbnd":{"d":1.0,"m":"nope"}}"#).len(),
            0
        );
    }

    #[test]
    fn parse_arr_with_defaults() {
        // Empty `arr` object is valid — defaults to full-array
        // identity per pvxs.
        let chain = parse_filter_chain(r#"{"arr":{}}"#);
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn parse_ts() {
        let chain = parse_filter_chain(r#"{"ts":{}}"#);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.iter().next().unwrap().name(), "ts");
    }

    #[test]
    fn parse_dec() {
        let chain = parse_filter_chain(r#"{"dec":{"n":3}}"#);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.iter().next().unwrap().name(), "dec");
    }

    #[test]
    fn parse_dec_requires_n() {
        // No `n` → skip (warn).
        let chain = parse_filter_chain(r#"{"dec":{}}"#);
        assert!(chain.is_empty());
    }

    #[test]
    fn parse_chained_filters_in_order() {
        let chain = parse_filter_chain(r#"{"dec":{"n":2},"dbnd":{"d":0.1},"ts":{}}"#);
        let names: Vec<&'static str> = chain.iter().map(|f| f.name()).collect();
        assert_eq!(names, vec!["dec", "dbnd", "ts"]);
    }

    #[test]
    fn parse_sync_mode_without_state_is_skipped() {
        // Long form requires both `m` and `s`; missing `s` skips.
        let chain = parse_filter_chain(r#"{"sync":{"m":"unless"},"dbnd":{"d":1.0}}"#);
        let names: Vec<&'static str> = chain.iter().map(|f| f.name()).collect();
        assert_eq!(names, vec!["dbnd"]);
    }

    #[test]
    fn parse_sync_long_form_after_mode() {
        // The state must already exist: `sync.c`'s `parse_ok` resolves it
        // with `dbStateFind` and fails the parse when it is unknown. A real
        // IOC declares it with a `Db State` device-support record.
        super::super::db_state_registry().get_or_create("SYS:TRIG");
        let chain = parse_filter_chain(r#"{"sync":{"m":"after","s":"SYS:TRIG"}}"#);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.iter().next().unwrap().name(), "sync");
    }

    /// C `sync.c:87-93`: `if (!(my->id = dbStateFind(my->state))) return -1;`
    /// — a filter naming a state no record declared fails channel creation
    /// rather than gating on a freshly invented one that nothing will ever
    /// set.
    #[test]
    fn parse_sync_unknown_state_is_rejected() {
        let err = try_parse_filter_chain(r#"{"sync":{"m":"after","s":"SYS:NEVER:DECLARED"}}"#)
            .expect_err("an unknown dbState must fail channel creation");
        assert!(matches!(err, FilterParseError::BadConfig { .. }), "{err}");
    }

    #[test]
    fn parse_sync_tagged_shorthand_while_mode() {
        // `{"while":"STATE"}` — upstream-supported shorthand where
        // the mode keyword doubles as the JSON key and the value is
        // the state name. epics-base sync.c uses `chfTagString` for
        // exactly this case.
        super::super::db_state_registry().get_or_create("SYS:READY");
        let chain = parse_filter_chain(r#"{"sync":{"while":"SYS:READY"}}"#);
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn parse_sync_all_six_modes_via_shorthand() {
        super::super::db_state_registry().get_or_create("STATE");
        for mode in ["before", "first", "last", "after", "while", "unless"] {
            let json = format!(r#"{{"sync":{{"{mode}":"STATE"}}}}"#);
            let chain = parse_filter_chain(&json);
            assert_eq!(chain.len(), 1, "mode {mode} must parse to one filter");
            assert_eq!(chain.iter().next().unwrap().name(), "sync");
        }
    }

    /// `utag` is a registered plugin (`utag.c:93`), so a channel naming it
    /// connects instead of answering CREATE_CH_FAIL.
    #[test]
    fn parse_utag_builds_a_filter() {
        for json in [
            r#"{"utag":{}}"#,
            r#"{"utag":{"M":15}}"#,
            r#"{"utag":{"V":2}}"#,
            r#"{"utag":{"M":15,"V":2}}"#,
        ] {
            let chain = try_parse_filter_chain(json).expect("utag must parse");
            assert_eq!(chain.len(), 1, "{json}");
            assert_eq!(chain.iter().next().unwrap().name(), "utag", "{json}");
        }
    }

    /// C `parse_map_key` returns `parse_stop` for a key outside the
    /// plugin's `chfPluginArgDef opts[]` table (`chfPlugin.c:472-474`), so
    /// `dbChannelCreate` fails. `dec` defines `n` and nothing else
    /// (`decimate.c:32-36`) — the `offset` key this port used to accept was
    /// never a C option.
    #[test]
    fn parse_rejects_a_key_the_plugin_does_not_define() {
        for (json, filter, key) in [
            (r#"{"dec":{"n":2,"offset":1}}"#, "dec", "offset"),
            (r#"{"arr":{"s":1,"stride":2}}"#, "arr", "stride"),
            (r#"{"dbnd":{"d":1,"r":2}}"#, "dbnd", "r"),
            (r#"{"ts":{"fmt":"iso"}}"#, "ts", "fmt"),
            (r#"{"sync":{"m":"after","state":"S"}}"#, "sync", "state"),
        ] {
            let err = try_parse_filter_chain(json).expect_err("{json} must be rejected");
            assert_eq!(
                err,
                FilterParseError::UnknownOption {
                    filter: filter.to_string(),
                    key: key.to_string(),
                },
                "{json}"
            );
        }
    }

    /// C `decimate.c`'s `parse_ok` returns -1 for `n < 1` (`:49-56`). The
    /// port used to clamp with `.max(1)`, turning a rejected channel into a
    /// silently unfiltered one.
    #[test]
    fn parse_rejects_a_decimation_count_below_one() {
        let err = try_parse_filter_chain(r#"{"dec":{"n":0}}"#)
            .expect_err("n < 1 must fail channel creation");
        assert!(matches!(err, FilterParseError::BadConfig { .. }), "{err}");
    }

    /// `chfEnum` stores through `store_enum_value`, which returns -1 for a
    /// name outside the enum map (`chfPlugin.c:279-300`). `ts` used to warn
    /// and fall back to its default mode, which hands the client a
    /// different filter from the one it asked for.
    #[test]
    fn parse_rejects_an_enum_value_outside_the_map() {
        for json in [
            r#"{"ts":{"num":"bogus"}}"#,
            r#"{"ts":{"epoch":"tai"}}"#,
            r#"{"ts":{"str":"rfc3339"}}"#,
        ] {
            let err = try_parse_filter_chain(json).expect_err("{json} must be rejected");
            assert!(matches!(err, FilterParseError::BadConfig { .. }), "{json}");
        }
    }

    #[test]
    fn parse_sync_empty_config_is_skipped() {
        let chain = parse_filter_chain(r#"{"sync":{}}"#);
        assert!(chain.is_empty());
    }

    #[test]
    fn parse_sync_unknown_mode_is_skipped() {
        let chain = parse_filter_chain(r#"{"sync":{"m":"nonsense","s":"STATE"}}"#);
        assert!(chain.is_empty());
    }

    #[test]
    fn parse_malformed_json_returns_empty_chain() {
        let chain = parse_filter_chain("{not json");
        assert!(chain.is_empty());
    }

    #[test]
    fn parse_non_object_returns_empty_chain() {
        let chain = parse_filter_chain("[1,2,3]");
        assert!(chain.is_empty());
    }

    #[test]
    fn empty_object_yields_empty_chain() {
        let chain = parse_filter_chain("{}");
        assert!(chain.is_empty());
    }

    // ---- try_parse_filter_chain: dbChannelCreate-parity reject contract ----

    #[test]
    fn try_parse_rejects_malformed_json() {
        let err = try_parse_filter_chain("{not json").unwrap_err();
        assert!(matches!(err, FilterParseError::InvalidJson { .. }));
    }

    #[test]
    fn try_parse_rejects_non_object_body() {
        let err = try_parse_filter_chain("[1,2,3]").unwrap_err();
        assert!(matches!(err, FilterParseError::NotObject { .. }));
    }

    #[test]
    fn try_parse_rejects_unknown_filter() {
        // EPICS `dbChannel.c:176-182` `parse_stop` → `S_db_notFound`.
        let err = try_parse_filter_chain(r#"{"no_such":{}}"#).unwrap_err();
        assert!(matches!(err, FilterParseError::UnknownFilter { .. }));
    }

    #[test]
    fn try_parse_rejects_bad_dec_config() {
        // `dec` without `n` is a configuration the filter parser rejects
        // (EPICS `chf_value` / `parse_end` failure → `parse_stop`).
        let err = try_parse_filter_chain(r#"{"dec":{}}"#).unwrap_err();
        assert!(matches!(err, FilterParseError::BadConfig { .. }));
    }

    #[test]
    fn try_parse_empty_object_is_valid_empty_chain() {
        // `{}` is a valid no-filter request, not a parse failure.
        let chain = try_parse_filter_chain("{}").expect("empty object is valid");
        assert!(chain.is_empty());
    }

    #[test]
    fn try_parse_accepts_documented_json5_unquoted_keys() {
        // `filters.dbd.pod:415-419` ships `{"arr":{s:2,i:2,e:8}}`.
        let chain = try_parse_filter_chain(r#"{"arr":{s:2,i:2,e:8}}"#)
            .expect("documented JSON5 array filter must parse");
        let names: Vec<&'static str> = chain.iter().map(|f| f.name()).collect();
        assert_eq!(names, vec!["arr"]);
    }

    #[test]
    fn try_parse_accepts_fully_unquoted_keys() {
        let chain = try_parse_filter_chain(r#"{dbnd:{d:0.5}}"#)
            .expect("unquoted filter + parameter keys must parse");
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn try_parse_chained_filters_preserve_order() {
        let chain = try_parse_filter_chain(r#"{"dec":{"n":2},"dbnd":{"d":0.1},"ts":{}}"#).unwrap();
        let names: Vec<&'static str> = chain.iter().map(|f| f.name()).collect();
        assert_eq!(names, vec!["dec", "dbnd", "ts"]);
    }

    #[test]
    fn try_parse_rejects_chain_with_one_unknown_entry() {
        // A mixed chain with one unknown filter is a hard reject of the
        // whole channel — matching `dbChannelCreate` aborting on the
        // first `parse_stop`, not a silently-reduced chain.
        let err = try_parse_filter_chain(r#"{"dbnd":{"d":0.1},"no_such":{}}"#).unwrap_err();
        assert!(matches!(err, FilterParseError::UnknownFilter { .. }));
    }

    // ── parse_channel_name: the three-step dbChannelCreate order ──

    fn cn(raw: &str) -> (String, String, String, bool, Option<String>) {
        let c = parse_channel_name(raw);
        (
            c.record_path,
            c.record,
            c.field,
            c.string_view,
            c.json_suffix,
        )
    }

    #[test]
    fn channel_name_bare_record_binds_val() {
        assert_eq!(
            cn("REC"),
            ("REC".into(), "REC".into(), "VAL".into(), false, None)
        );
    }

    #[test]
    fn channel_name_field_is_uppercased() {
        assert_eq!(
            cn("REC.desc"),
            ("REC.desc".into(), "REC".into(), "DESC".into(), false, None)
        );
    }

    /// The `$` is innermost, so it comes off after the suffix and before the
    /// `record.FIELD` split — on a bare `REC$` too, which binds `VAL`.
    #[test]
    fn channel_name_peels_the_dollar_modifier() {
        assert_eq!(
            cn("REC.DESC$"),
            ("REC.DESC".into(), "REC".into(), "DESC".into(), true, None)
        );
        assert_eq!(
            cn("REC$"),
            ("REC".into(), "REC".into(), "VAL".into(), true, None)
        );
    }

    /// The case that made the old callers' last-dot split a coin flip: a
    /// suffix with no `.` left the field looking like a clean `VAL`, while
    /// one containing `0.5` tore the JSON apart. Both resolve identically
    /// here, and the suffix comes back whole.
    #[test]
    fn channel_name_peels_the_json_suffix_dotted_or_not() {
        assert_eq!(
            cn(r#"REC{"arr":{"s":0}}"#),
            (
                "REC".into(),
                "REC".into(),
                "VAL".into(),
                false,
                Some(r#"{"arr":{"s":0}}"#.into())
            )
        );
        assert_eq!(
            cn(r#"REC{"dbnd":{"d":0.5}}"#),
            (
                "REC".into(),
                "REC".into(),
                "VAL".into(),
                false,
                Some(r#"{"dbnd":{"d":0.5}}"#.into())
            )
        );
    }

    /// A `[range]` arrives as an `arr` filter, so a caller that refuses
    /// filters refuses the legacy syntax by the same rule.
    #[test]
    fn channel_name_folds_a_range_into_an_arr_filter() {
        let c = parse_channel_name("REC.VAL[0:2]");
        assert_eq!(
            (c.record_path.as_str(), c.field.as_str()),
            ("REC.VAL", "VAL")
        );
        assert_eq!(c.json_suffix.as_deref(), Some(r#"{"arr":{"e":2}}"#));
    }

    /// All three modifiers at once, in the order the name carries them.
    #[test]
    fn channel_name_peels_all_three_modifiers() {
        let c = parse_channel_name(r#"REC.DESC$[0:2]{"dbnd":{"d":0.5}}"#);
        assert_eq!(c.record, "REC");
        assert_eq!(c.field, "DESC");
        assert!(c.string_view);
        assert!(c.json_suffix.as_deref().unwrap().contains("\"arr\""));
        assert!(c.json_suffix.as_deref().unwrap().contains("\"dbnd\""));
    }

    // ── chfPluginArgDef `convert` (chfPlugin.c:64-290) ─────────────

    fn conv_ev(v: crate::types::EpicsValue) -> super::super::FilteredMonitorEvent {
        use super::super::FilteredMonitorEvent;
        use crate::server::pv::MonitorEvent;
        use crate::server::recgbl::EventMask;
        use crate::server::snapshot::Snapshot;
        FilteredMonitorEvent::new(MonitorEvent {
            snapshot: Arc::new(Snapshot::new(v, 0, 0, std::time::SystemTime::UNIX_EPOCH)),
            origin: 0,
            mask: EventMask::VALUE,
        })
    }

    fn conv_doubles(json: &str, input: Vec<f64>) -> Vec<f64> {
        use crate::types::EpicsValue;
        let chain = try_parse_filter_chain(json).expect("{json} must parse");
        let out = chain
            .apply(conv_ev(EpicsValue::DoubleArray(input)))
            .expect("value event must survive an arr filter");
        match Arc::unwrap_or_clone(out.event.snapshot).value {
            EpicsValue::DoubleArray(v) => v,
            other => panic!("expected DoubleArray, got {other:?}"),
        }
    }

    /// `arr`'s `s`/`i`/`e` are `chfInt32(..., Conv = 1)` (`arr.c:36-38`),
    /// so `store_string_value` runs them through `epicsParseInt32`
    /// (`chfPlugin.c:253`). The port read them with `as_i64()`, which is
    /// `None` for a JSON string, and both bounds silently fell back to
    /// their defaults — ten elements where C sends four.
    #[test]
    fn arr_string_bounds_convert_through_epics_parse_int32() {
        let all: Vec<f64> = (0..10).map(f64::from).collect();
        assert_eq!(
            conv_doubles(r#"{"arr":{"s":"2","e":"5"}}"#, all.clone()),
            vec![2.0, 3.0, 4.0, 5.0]
        );
        // `strtol` base 0 and the ignored `units` out-parameter: a `0x`
        // prefix and trailing text are both C-legal.
        assert_eq!(
            conv_doubles(r#"{"arr":{"s":"0x2","e":" 5 elements"}}"#, all),
            vec![2.0, 3.0, 4.0, 5.0]
        );
    }

    /// The same `Conv = 1` reached from the other side:
    /// `store_double_value` range-checks INT_MIN..INT_MAX and then
    /// truncates through the `(epicsInt32)` cast (`chfPlugin.c:194-199`).
    /// `as_i64()` is `None` for `2.0`, so the stride fell back to 1.
    #[test]
    fn arr_double_stride_truncates_after_the_int_range_check() {
        let all: Vec<f64> = (0..10).map(f64::from).collect();
        assert_eq!(
            conv_doubles(r#"{"arr":{"i":2.0}}"#, all),
            vec![0.0, 2.0, 4.0, 6.0, 8.0]
        );
        // Outside epicsInt32 the store returns -1, which is `parse_stop`.
        assert!(try_parse_filter_chain(r#"{"arr":{"i":3e9}}"#).is_err());
        // `parse_integer` applies the same range check to a JSON integer
        // before any store at all (`chfPlugin.c:409-413`).
        assert!(try_parse_filter_chain(r#"{"arr":{"s":3000000000}}"#).is_err());
    }

    /// `dbnd`'s `m` is `chfEnum(..., Conv = 1)` (`dbnd.c:41`), and
    /// `store_integer_value`'s Enum arm matches the numeric *value*
    /// against `modeEnum` (`chfPlugin.c:104-115`). Reading `m` with
    /// `as_str()` made `{"m":1}` fall back to absolute, so a 50% band
    /// became a band of 50.
    #[test]
    fn dbnd_integer_mode_selects_relative_by_enum_value() {
        use crate::types::EpicsValue;
        let chain = try_parse_filter_chain(r#"{"dbnd":{"d":50,"m":1}}"#).expect("must parse");
        assert!(
            chain.apply(conv_ev(EpicsValue::Double(10.0))).is_some(),
            "the first event always passes (hyst starts at cval)"
        );
        assert!(
            chain.apply(conv_ev(EpicsValue::Double(20.0))).is_some(),
            "rel mode refreshed hyst to 10 * 50/100 = 5, and 20 - 10 > 5"
        );
        // Absolute mode with the same numbers suppresses the second one,
        // which is what the port used to do with `{"m":1}`.
        let abs = try_parse_filter_chain(r#"{"dbnd":{"d":50,"m":"abs"}}"#).expect("must parse");
        assert!(abs.apply(conv_ev(EpicsValue::Double(10.0))).is_some());
        assert!(abs.apply(conv_ev(EpicsValue::Double(20.0))).is_none());
    }

    /// Every `ts` option is `chfEnum(..., Conv = 0)` (`ts.c:71-73`), so
    /// `store_integer_value` stops at its opening guard
    /// (`chfPlugin.c:80-82`) and `dbChannelCreate` returns NULL. The port
    /// read `num` with `as_str()`, saw `None`, and connected the channel
    /// in the default Generate mode instead.
    #[test]
    fn ts_numeric_enum_value_is_refused_because_the_option_does_not_convert() {
        for json in [
            r#"{"ts":{"num":2}}"#,
            r#"{"ts":{"epoch":1}}"#,
            r#"{"ts":{"str":2}}"#,
        ] {
            let err = try_parse_filter_chain(json).expect_err("{json} must be refused");
            assert!(matches!(err, FilterParseError::BadConfig { .. }), "{json}");
        }
        // The same options still take their keyword spellings, because
        // `store_string_value` exempts Enum from the convert guard
        // (`chfPlugin.c:248-251`).
        assert_eq!(
            try_parse_filter_chain(r#"{"ts":{"num":"dbl","epoch":"unix"}}"#)
                .expect("keywords must parse")
                .len(),
            1
        );
    }

    /// `sync`'s `m` is `chfEnum(..., Conv = 1)` (`sync.c:59`), so an
    /// integer selects the mode by `modeEnum` value and the channel
    /// connects — the port refused it. Each numeric choice must land on
    /// the same mode as its keyword, so both forms are driven through one
    /// state transition sequence that separates all six modes.
    #[test]
    fn sync_integer_mode_matches_the_keyword_of_the_same_enum_choice() {
        use crate::types::EpicsValue;
        for (choice, keyword, expected) in [
            (0, "before", vec![1.0]),
            (1, "first", vec![2.0]),
            (2, "last", vec![3.0]),
            (3, "after", vec![4.0]),
            (4, "while", vec![2.0, 3.0]),
            (5, "unless", vec![1.0, 4.0]),
        ] {
            let name = format!("UNIT:CONV:SYNC:{choice}");
            let state = super::super::db_state_registry().get_or_create(&name);
            let numeric =
                try_parse_filter_chain(&format!(r#"{{"sync":{{"m":{choice},"s":"{name}"}}}}"#))
                    .expect("an integer mode must build the filter");
            let worded =
                try_parse_filter_chain(&format!(r#"{{"sync":{{"m":"{keyword}","s":"{name}"}}}}"#))
                    .expect("the keyword form must build the filter");
            let mut got_numeric = Vec::new();
            let mut got_worded = Vec::new();
            // state F: 1.0 │ state T: 2.0, 3.0 │ state F: 4.0
            for (set, value) in [(false, 1.0), (true, 2.0), (true, 3.0), (false, 4.0)] {
                state.set(set);
                for (chain, got) in [(&numeric, &mut got_numeric), (&worded, &mut got_worded)] {
                    if let Some(out) = chain.apply(conv_ev(EpicsValue::Double(value))) {
                        match Arc::unwrap_or_clone(out.event.snapshot).value {
                            EpicsValue::Double(v) => got.push(v),
                            other => panic!("expected Double, got {other:?}"),
                        }
                    }
                }
            }
            assert_eq!(
                got_numeric, expected,
                "m = {choice} must gate like {keyword}"
            );
            assert_eq!(got_worded, expected, "m = \"{keyword}\"");
        }
        // A choice outside `modeEnum` still stops the parse
        // (`chfPlugin.c:112-115`).
        assert!(try_parse_filter_chain(r#"{"sync":{"m":6,"s":"UNIT:CONV:SYNC:0"}}"#).is_err());
    }

    /// `dbnd`'s tagged `abs` / `rel` are `chfTagDouble(..., Conv = 1)`
    /// (`dbnd.c:42-43`), so a string converts through `epicsParseDouble`
    /// and the tag still selects the mode. An out-of-range magnitude is
    /// an ERANGE error return there (`epicsStdlib.c:164-165`), not a
    /// saturated `inf` or `0`.
    #[test]
    fn dbnd_tagged_doubles_convert_from_a_string_but_not_out_of_range() {
        use crate::types::EpicsValue;
        let abs = try_parse_filter_chain(r#"{"dbnd":{"abs":"0.5"}}"#).expect("abs converts");
        assert!(abs.apply(conv_ev(EpicsValue::Double(1.0))).is_some());
        assert!(
            abs.apply(conv_ev(EpicsValue::Double(1.4))).is_none(),
            "a 0.5 absolute deadband must swallow a 0.4 step"
        );
        assert!(abs.apply(conv_ev(EpicsValue::Double(1.6))).is_some());

        let rel = try_parse_filter_chain(r#"{"dbnd":{"rel":"50"}}"#).expect("rel converts");
        assert!(rel.apply(conv_ev(EpicsValue::Double(10.0))).is_some());
        assert!(
            rel.apply(conv_ev(EpicsValue::Double(20.0))).is_some(),
            "the tag selected relative mode, so hyst is 10 * 50/100 = 5"
        );

        for json in [
            r#"{"dbnd":{"abs":"abc"}}"#,
            r#"{"dbnd":{"abs":"1.5e400"}}"#,
            r#"{"dbnd":{"d":"1e-400"}}"#,
        ] {
            assert!(
                try_parse_filter_chain(json).is_err(),
                "{json} must stop the parse"
            );
        }
        // `strtod` sets no `errno` for the word forms, so they store.
        assert!(try_parse_filter_chain(r#"{"dbnd":{"abs":"inf"}}"#).is_ok());
    }

    /// `sync`'s `s` and `decimate`'s `n` are the two `Conv = 0` options
    /// that are not enums (`sync.c:60`, `decimate.c:34`), so a value of
    /// any other kind stops the parse rather than converting.
    #[test]
    fn non_converting_options_refuse_a_value_of_another_kind() {
        super::super::db_state_registry().get_or_create("UNIT:CONV:NOCONV");
        assert!(try_parse_filter_chain(r#"{"dec":{"n":"3"}}"#).is_err());
        // Enum is exempt from the string guard, but the string must still
        // name a choice — `"1"` is not one of `ts`'s (`ts.c:46-47`).
        assert!(try_parse_filter_chain(r#"{"ts":{"num":"1"}}"#).is_err());
        assert!(try_parse_filter_chain(r#"{"dec":{"n":3.0}}"#).is_err());
        assert!(try_parse_filter_chain(r#"{"sync":{"m":"last","s":7}}"#).is_err());
        // `utag`'s `M`/`V` convert (`utag.c:22-23`), so the same string
        // shape is accepted there.
        assert_eq!(
            try_parse_filter_chain(r#"{"utag":{"M":"0xff","V":"3"}}"#)
                .expect("utag converts")
                .len(),
            1
        );
    }
}
