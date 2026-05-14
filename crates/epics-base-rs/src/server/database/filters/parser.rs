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
//! Unrecognised filter names are silently skipped with a tracing
//! warning so a forward-compatible client that emits a `sync` filter
//! today doesn't error out the whole subscription.

use std::sync::Arc;

use super::{
    ArrayFilter, ArrayFilterConfig, DeadbandFilter, DeadbandMode, DecimateFilter, FilterChain,
    SubscriptionFilter, TimestampFilter,
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
/// The suffix begins at the first unescaped `{` that follows the
/// last `.` separator (so `REC.{...}` and `REC.FIELD.{...}` both
/// parse). Returns the empty suffix when the name is just a normal
/// `RECORD` or `RECORD.FIELD`.
pub fn split_channel_name(raw: &str) -> ParsedChannelName {
    // Find the FIRST `.{` — the JSON suffix always starts that way
    // because the previous `.` separates field from filter.
    if let Some(brace_pos) = raw.find(".{") {
        let record_path = raw[..brace_pos].to_string();
        let json = raw[brace_pos + 1..].to_string();
        ParsedChannelName {
            record_path,
            json_suffix: Some(json),
        }
    } else {
        ParsedChannelName {
            record_path: raw.to_string(),
            json_suffix: None,
        }
    }
}

/// Parse the JSON suffix into a [`FilterChain`]. Returns an empty
/// chain on parse failure (invalid JSON, unknown filter keys, etc.)
/// — the caller's subscription proceeds with no filter rather than
/// failing outright. Per-filter parse failures inside a valid
/// object are logged via `tracing::warn!` and skipped.
pub fn parse_filter_chain(json: &str) -> FilterChain {
    let mut chain = FilterChain::new();
    let value: serde_json::Value = match serde_json::from_str(json) {
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
            Some(filt) => chain.push(filt),
            None => {
                tracing::warn!(
                    filter = %key,
                    config = %cfg,
                    "unrecognised channel filter; skipped",
                );
            }
        }
    }
    chain
}

fn build_filter(name: &str, cfg: &serde_json::Value) -> Option<Arc<dyn SubscriptionFilter>> {
    match name {
        "dbnd" => build_dbnd(cfg),
        "arr" => build_arr(cfg),
        "ts" => Some(Arc::new(TimestampFilter::new())),
        "dec" => build_decimate(cfg),
        "sync" => build_sync(cfg),
        _ => None,
    }
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
    Some(Arc::new(ArrayFilter::new(ArrayFilterConfig {
        start,
        incr,
        end,
    })))
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
}
