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
/// Returns the empty suffix when no `{` appears.
pub fn split_channel_name(raw: &str) -> ParsedChannelName {
    let Some(brace_pos) = raw.find('{') else {
        return ParsedChannelName {
            record_path: raw.to_string(),
            json_suffix: None,
        };
    };
    // Strip an optional `.` separator immediately before the brace
    // so `RECORD.{...}` and `RECORD.FIELD.{...}` produce a clean
    // `record_path` without the dangling dot.
    let path_end = if brace_pos > 0 && raw.as_bytes()[brace_pos - 1] == b'.' {
        brace_pos - 1
    } else {
        brace_pos
    };
    let record_path = raw[..path_end].to_string();
    let json = raw[brace_pos..].to_string();
    ParsedChannelName {
        record_path,
        json_suffix: Some(json),
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

/// Rewrite the documented EPICS JSON5 channel-filter forms into the
/// strict JSON that `serde_json` accepts.
///
/// EPICS base parses filter suffixes with a JSON5-capable yajl, and its
/// own documentation and shipped examples use unquoted object keys,
/// e.g. `{"arr":{s:2,i:2,e:8}}` (`filters.dbd.pod:73-99, 415-419`). The
/// only JSON5 extension the documented filter grammar relies on is
/// unquoted identifier keys, so this quotes bareword keys — an
/// identifier token in key position (the next non-whitespace char is
/// `:`) that is not already quoted — and leaves string contents,
/// numbers, and bareword values (`true` / `false` / `null`) untouched.
/// Already-strict JSON round-trips unchanged.
fn json5_filter_to_json(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + 16);
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                // Copy a string literal verbatim, honouring `\`-escapes
                // so an embedded `"` or `:` is never mistaken for
                // structure.
                out.push('"');
                while let Some(sc) = chars.next() {
                    out.push(sc);
                    if sc == '\\' {
                        if let Some(esc) = chars.next() {
                            out.push(esc);
                        }
                    } else if sc == '"' {
                        break;
                    }
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                let mut word = String::new();
                word.push(c);
                while let Some(&pc) = chars.peek() {
                    if pc.is_ascii_alphanumeric() || pc == '_' || pc == '$' {
                        word.push(pc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Buffer trailing whitespace so a `key :` form still
                // resolves as a key without dropping the spacing.
                let mut ws = String::new();
                while let Some(&pc) = chars.peek() {
                    if pc.is_whitespace() {
                        ws.push(pc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&':') {
                    out.push('"');
                    out.push_str(&word);
                    out.push('"');
                } else {
                    out.push_str(&word);
                }
                out.push_str(&ws);
            }
            other => out.push(other),
        }
    }
    out
}

/// Parse the JSON suffix into a [`FilterChain`], rejecting any
/// syntactically-present-but-unparseable filter request.
///
/// This is the channel-creation contract: it mirrors EPICS base
/// `dbChannelCreate()` / `chf_parse()`, which abort channel creation on
/// malformed JSON, a non-object body, an unknown filter name, or a
/// filter whose own parser rejects its configuration. An empty object
/// (`{}`) is a valid no-filter request and yields an empty chain. The
/// documented JSON5 unquoted-key forms are accepted via
/// [`json5_filter_to_json`].
pub fn try_parse_filter_chain(json: &str) -> Result<FilterChain, FilterParseError> {
    let normalized = json5_filter_to_json(json);
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
    let normalized = json5_filter_to_json(json);
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

/// Build one filter from its name + config, distinguishing an unknown
/// filter name from a known filter whose configuration is rejected so
/// the strict path can mirror EPICS `S_db_notFound` vs `parse_stop`.
fn build_filter(
    name: &str,
    cfg: &serde_json::Value,
) -> Result<Arc<dyn SubscriptionFilter>, FilterParseError> {
    let built = match name {
        "dbnd" => build_dbnd(cfg),
        "arr" => build_arr(cfg),
        "ts" => build_ts(cfg),
        "dec" => build_decimate(cfg),
        "sync" => build_sync(cfg),
        _ => {
            return Err(FilterParseError::UnknownFilter {
                name: name.to_string(),
            });
        }
    };
    built.ok_or_else(|| FilterParseError::BadConfig {
        name: name.to_string(),
        config: cfg.to_string(),
    })
}

/// C `ts.c` JSON schema:
/// * `{"ts":{}}` — default Generate mode.
/// * `{"ts":{"num":"dbl"|"sec"|"nsec"|"ts","epoch":"epics"|"unix"}}` —
///   numeric / array output, optional epoch override.
/// * `{"ts":{"str":"epics"}}` — formatted string output.
fn build_ts(cfg: &serde_json::Value) -> Option<Arc<dyn SubscriptionFilter>> {
    use super::ts::{TimestampFilter, TsEpoch, TsMode};
    let Some(obj) = cfg.as_object() else {
        // Bare `{"ts":{}}` decodes as an empty Map → build_filter
        // routes here with an object. `as_object()` only fails on
        // non-object cfg (e.g. `{"ts":42}`) which we skip with warn.
        return None;
    };
    // String mode wins over numeric — C `parse_finished` sets
    // `mode = tsModeString` whenever `str` is provided.
    if let Some(s) = obj.get("str").and_then(|v| v.as_str()) {
        match s {
            "epics" => return Some(Arc::new(TimestampFilter::with_mode(TsMode::StringEpics))),
            // `iso` is documented in C but its tsStringIso path is
            // gated on MSVC and treated as identical to epics on
            // non-Windows. Fall back to the EPICS format.
            "iso" => return Some(Arc::new(TimestampFilter::with_mode(TsMode::StringEpics))),
            _ => {
                tracing::warn!(value = %s, "unknown ts `str` value; ignoring");
                return None;
            }
        }
    }
    let epoch = match obj.get("epoch").and_then(|v| v.as_str()) {
        Some("unix") => TsEpoch::Unix,
        Some("epics") | None => TsEpoch::Epics,
        Some(other) => {
            tracing::warn!(value = %other, "unknown ts `epoch` value; defaulting to epics");
            TsEpoch::Epics
        }
    };
    let mode = match obj.get("num").and_then(|v| v.as_str()) {
        None => TsMode::Generate,
        Some("dbl") => TsMode::Double,
        Some("sec") => TsMode::Seconds,
        Some("nsec") => TsMode::Nanoseconds,
        Some("ts") => TsMode::Array,
        Some(other) => {
            tracing::warn!(value = %other, "unknown ts `num` value; defaulting to Generate");
            TsMode::Generate
        }
    };
    Some(Arc::new(TimestampFilter::with_mode_epoch(mode, epoch)))
}

fn build_dbnd(cfg: &serde_json::Value) -> Option<Arc<dyn SubscriptionFilter>> {
    let obj = cfg.as_object()?;
    if let Some(d) = obj.get("d").and_then(|v| v.as_f64()) {
        return Some(Arc::new(DeadbandFilter::new(d, DeadbandMode::Absolute)));
    }
    if let Some(r) = obj.get("r").and_then(|v| v.as_f64()) {
        // C `dbnd.c`: `my->hyst = val * my->cval/100.` — JSON `r` is
        // interpreted as a percent (e.g. `{"r":50}` means 50%). The
        // internal `DeadbandFilter` stores the relative threshold as
        // a fraction (`threshold * |last|` per call), so divide by
        // 100 at the wire boundary to match C semantics.
        return Some(Arc::new(DeadbandFilter::new(
            r / 100.0,
            DeadbandMode::Relative,
        )));
    }
    None
}

fn build_arr(cfg: &serde_json::Value) -> Option<Arc<dyn SubscriptionFilter>> {
    let obj = cfg.as_object()?;
    let start = obj.get("s").and_then(|v| v.as_i64()).unwrap_or(0);
    let incr = obj.get("i").and_then(|v| v.as_i64()).unwrap_or(1);
    let end = obj.get("e").and_then(|v| v.as_i64()).unwrap_or(-1);
    // `ArrayFilterConfig::new` clamps `incr` to `>= 1`, so a malicious
    // `{"i":0}` / `{"i":-3}` cannot reach the slice divisor.
    Some(Arc::new(ArrayFilter::new(ArrayFilterConfig::new(
        start, incr, end,
    ))))
}

fn build_decimate(cfg: &serde_json::Value) -> Option<Arc<dyn SubscriptionFilter>> {
    let obj = cfg.as_object()?;
    let n = obj.get("n").and_then(|v| v.as_u64())?;
    let offset = obj.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
    Some(Arc::new(DecimateFilter::new(n, offset)))
}

/// `sync` filter — six gating modes on a named [`super::DbState`].
///
/// Long form (matches upstream `chfPlugin` field tags):
/// `{"sync":{"m":"after","s":"STATE_NAME"}}`
///
/// Mode-tagged shorthand (also upstream-supported):
/// `{"sync":{"after":"STATE_NAME"}}` — the key acts as both the mode
/// keyword and the value carries the state name.
///
/// Missing mode or state → entry is skipped with a warn rather than
/// rejecting the whole chain.
fn build_sync(cfg: &serde_json::Value) -> Option<Arc<dyn SubscriptionFilter>> {
    let obj = cfg.as_object()?;
    // Long form: {"m": "after", "s": "STATE"}
    if let (Some(m), Some(s)) = (
        obj.get("m").and_then(|v| v.as_str()),
        obj.get("s").and_then(|v| v.as_str()),
    ) {
        if let Some(mode) = super::SyncMode::from_keyword(m) {
            if !s.is_empty() {
                return Some(Arc::new(super::SyncFilter::new(mode, s)));
            }
        }
    }
    // Mode-tagged shorthand: {"after": "STATE"} / {"while": "STATE"} / ...
    for (key, val) in obj {
        if let Some(mode) = super::SyncMode::from_keyword(key) {
            if let Some(state) = val.as_str() {
                if !state.is_empty() {
                    return Some(Arc::new(super::SyncFilter::new(mode, state)));
                }
            }
        }
    }
    None
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

    #[test]
    fn parse_dbnd_absolute() {
        let chain = parse_filter_chain(r#"{"dbnd":{"d":0.5}}"#);
        assert_eq!(chain.len(), 1);
        let names: Vec<&'static str> = chain.iter().map(|f| f.name()).collect();
        assert_eq!(names, vec!["dbnd"]);
    }

    #[test]
    fn parse_dbnd_relative_uses_r_key() {
        let chain = parse_filter_chain(r#"{"dbnd":{"r":0.01}}"#);
        assert_eq!(chain.len(), 1);
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
        let chain = parse_filter_chain(r#"{"sync":{"m":"after","s":"SYS:TRIG"}}"#);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain.iter().next().unwrap().name(), "sync");
    }

    #[test]
    fn parse_sync_tagged_shorthand_while_mode() {
        // `{"while":"STATE"}` — upstream-supported shorthand where
        // the mode keyword doubles as the JSON key and the value is
        // the state name. epics-base sync.c uses `chfTagString` for
        // exactly this case.
        let chain = parse_filter_chain(r#"{"sync":{"while":"SYS:READY"}}"#);
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn parse_sync_all_six_modes_via_shorthand() {
        for mode in ["before", "first", "last", "after", "while", "unless"] {
            let json = format!(r#"{{"sync":{{"{mode}":"STATE"}}}}"#);
            let chain = parse_filter_chain(&json);
            assert_eq!(chain.len(), 1, "mode {mode} must parse to one filter");
            assert_eq!(chain.iter().next().unwrap().name(), "sync");
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

    #[test]
    fn json5_normalizer_leaves_quoted_json_untouched() {
        let strict = r#"{"arr":{"s":2,"i":2,"e":8}}"#;
        assert_eq!(json5_filter_to_json(strict), strict);
    }

    #[test]
    fn json5_normalizer_does_not_quote_string_values() {
        // Only bareword KEYS are quoted; quoted string values (and the
        // `:` inside them) are preserved verbatim.
        let src = r#"{"sync":{"m":"after","s":"SYS:TRIG"}}"#;
        assert_eq!(json5_filter_to_json(src), src);
    }
}
