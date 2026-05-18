//! Parser for `@pva://...` link strings.
//!
//! Accepted forms (matches pvxs `pvalink_jlif.cpp`):
//!
//! ```text
//! pva://PV:NAME                              — bare PV name, default options
//! pva://PV:NAME?field=value                  — explicit value field
//! pva://PV:NAME?proc=PASSIVE&monitor=true    — multiple options
//! pva://PV:NAME pp                           — legacy "process passive" suffix
//! ```
//!
//! INP vs OUT direction is determined by the record field, not the link
//! string itself; callers pass [`LinkDirection`] when constructing a link.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkDirection {
    /// Record reads from the remote PV (INP-style).
    Inp,
    /// Record writes to the remote PV (OUT-style).
    Out,
}

/// Maximize-severity mode for a pvalink (the `sevr` JSON option and the
/// legacy `MS`/`NMS`/`MSI`/`MSS` bare modifiers).
///
/// Mirrors pvxs `pvaLinkConfig::sevr` (`pvalink.h` enum
/// `NMS`/`MS`/`MSI`/`MSS`). Controls whether a non-`NO_ALARM` severity
/// observed on the *remote* NT `alarm.severity` field propagates into
/// the owning record's `LINK_ALARM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SevrMode {
    /// `NMS` — never maximize. Remote severity is dropped. Default,
    /// matching pvxs (`sevr` defaults to `NMS`).
    #[default]
    Nms,
    /// `MS` — maximize severity: any non-`NO_ALARM` remote severity
    /// propagates into the record's `LINK_ALARM`.
    Ms,
    /// `MSI` — maximize *invalid* only: propagate solely when the
    /// remote severity is `INVALID_ALARM`.
    Msi,
}

impl SevrMode {
    /// Decide whether a remote NT `alarm.severity` value should raise
    /// `LINK_ALARM` on the owning record.
    ///
    /// `remote_severity` follows the EPICS alarm severity numbering
    /// (`0 = NO_ALARM`, `1 = MINOR`, `2 = MAJOR`, `3 = INVALID`),
    /// which is also the pvData NT `alarm.severity` encoding.
    ///
    /// Mirrors pvxs `pvalink_lset.cpp:418`:
    /// ```text
    /// (snap_severity != NO_ALARM && sevr == MS) ||
    /// (snap_severity == INVALID_ALARM && sevr == MSI)
    /// ```
    pub fn propagates(self, remote_severity: i32) -> bool {
        const NO_ALARM: i32 = 0;
        const INVALID_ALARM: i32 = 3;
        match self {
            SevrMode::Nms => false,
            SevrMode::Ms => remote_severity != NO_ALARM,
            SevrMode::Msi => remote_severity == INVALID_ALARM,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvaLinkConfig {
    pub pv_name: String,
    /// Sub-field selector inside the remote NT structure (default `"value"`).
    pub field: String,
    /// True iff the link should keep an active monitor open instead of
    /// re-reading on each access (INP only).
    pub monitor: bool,
    /// True iff PUT should call `process()` on the remote record (OUT only).
    pub process: bool,
    /// True iff the link reports DBE_VALUE notifications back to the local
    /// record (INP, monitor mode).
    pub notify: bool,
    /// When true, every monitor update triggers `process()` on the
    /// owning record (INP-side equivalent of the legacy
    /// `record(... CP)` flag). Mirrors pvxs `pvaLink::scanOnUpdate`
    /// (pvalink_link.cpp:122). Default `false` — record must be
    /// scanned externally.
    pub scan_on_update: bool,
    /// When true, `scan_on_update` processing fires on *every* monitor
    /// event even if the linked field value did not change. Mirrors
    /// pvxs `pvaLinkConfig::always` — without it, `CP`/`CPP` scans are
    /// suppressed for no-op updates. INP only.
    pub always: bool,
    /// Maximize-severity mode (`MS`/`NMS`/`MSI`). Mirrors pvxs
    /// `pvaLinkConfig::sevr`.
    pub sevr: SevrMode,
    /// Monitor queue size — pvxs `Q` (and the `queueSize` pvRequest
    /// option). `< 1` is clamped to `1`. Default `4`, matching the
    /// pvxs default monitor queue depth. INP+monitor only.
    pub queue_size: usize,
    /// Pipeline (windowed flow-control) mode — pvxs `pipeline`. When
    /// true the monitor request carries `record[pipeline=true]`.
    /// INP+monitor only.
    pub pipeline: bool,
    /// Defer the actual Put: when true a `write` only queues the value
    /// locally and the caller must call `flush_deferred` to push it.
    /// Mirrors pvxs `pvaLinkConfig::defer`. OUT only.
    pub defer: bool,
    /// Retry queued Puts across disconnects: when true a `write` issued
    /// while the upstream is unreachable is queued and replayed once
    /// the link reconnects. Mirrors pvxs `pvaLinkConfig::retry`.
    /// Without it, a Put on a disconnected link fails immediately.
    /// OUT only.
    pub retry: bool,
    /// Require a *local* (same-IOC) channel. When true the link only
    /// resolves PVs served by the local QSRV instance; remote PVs are
    /// rejected. Mirrors pvxs `pvaLinkConfig::local`.
    pub local: bool,
    /// Atomic multi-link processing. When true, `scan_on_update`
    /// processing for this link is grouped with other `atomic` links
    /// sharing the same monitor batch so they scan under one lock
    /// epoch. Mirrors pvxs `pvaLinkConfig::atomic`.
    pub atomic: bool,
    /// Processing order during a `CP` scan batch, clamped to
    /// `-1024..=1024`. Lower values process first. Mirrors pvxs
    /// `pvaLinkConfig::monorder`.
    pub monorder: i32,
    /// BR-R19: when true, the owning record's TIME is adopted from
    /// the linked PV's NT `timeStamp` on each read. Mirrors pvxs
    /// `pvaLinkConfig::time` (`pvalink_jlif.cpp:35` / parsing at
    /// `:104`; consumer at `pvalink_lset.cpp:427`). Default `false`
    /// — the owning record keeps its locally-stamped processing
    /// time.
    pub time: bool,
    /// Direction inferred from caller, not parsed.
    pub direction: LinkDirection,
}

/// pvxs default monitor queue depth (`pvaLinkConfig::queueSize`).
pub const DEFAULT_QUEUE_SIZE: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum PvaLinkParseError {
    #[error("not a pva link: {0:?}")]
    NotPvaLink(String),
    #[error("empty PV name")]
    EmptyPv,
    #[error("invalid option: {0:?}")]
    BadOption(String),
}

impl PvaLinkConfig {
    /// Parse a link string into a config. The caller passes the direction
    /// explicitly — INP for record input fields, OUT for outputs.
    pub fn parse(s: &str, direction: LinkDirection) -> Result<Self, PvaLinkParseError> {
        // Strip leading `@` if present (DBD parsers strip this; tests may not).
        let s = s.trim();
        let s = s.strip_prefix('@').unwrap_or(s);
        // pvxs accepts both `pva://` and bare `pva:` prefixes.
        let body = s
            .strip_prefix("pva://")
            .or_else(|| s.strip_prefix("pva:"))
            .ok_or_else(|| PvaLinkParseError::NotPvaLink(s.to_string()))?;

        // Strip legacy "PP" / "NPP" / "MS" / "NMS" suffixes (DBD-style mods).
        let (body, legacy_mods) = strip_legacy_mods(body);

        // Split off ?key=value&key=value
        let (pv_name, opts) = match body.split_once('?') {
            Some((pv, q)) => (pv, parse_query(q)?),
            None => (body, HashMap::new()),
        };
        if pv_name.is_empty() {
            return Err(PvaLinkParseError::EmptyPv);
        }

        let mut cfg = PvaLinkConfig {
            pv_name: pv_name.to_string(),
            direction,
            ..PvaLinkConfig::defaults_for(pv_name, direction)
        };

        if let Some(v) = opts.get("field") {
            cfg.field = v.clone();
        }
        if let Some(v) = opts.get("monitor") {
            cfg.monitor = parse_bool(v)?;
        }
        if let Some(v) = opts.get("proc") {
            // pvxs `proc`: false/true/NPP/PP/CP/CPP. CP / CPP imply
            // monitor + scan-on-update; PP / true imply put-side
            // process.
            match v.as_str() {
                "TRUE" | "true" | "1" | "PP" | "pp" | "PASSIVE" | "passive" => cfg.process = true,
                "FALSE" | "false" | "0" | "NPP" | "npp" => cfg.process = false,
                "CP" | "cp" => {
                    cfg.monitor = true;
                    cfg.scan_on_update = true;
                }
                "CPP" | "cpp" => {
                    cfg.monitor = true;
                    cfg.scan_on_update = true;
                }
                other => return Err(PvaLinkParseError::BadOption(other.to_string())),
            }
        }
        if let Some(v) = opts.get("notify") {
            cfg.notify = parse_bool(v)?;
        }
        if let Some(v) = opts.get("scan_on_update") {
            cfg.scan_on_update = parse_bool(v)?;
        }
        if let Some(v) = opts.get("sevr") {
            cfg.sevr = parse_sevr(v)?;
        }
        if let Some(v) = opts.get("always") {
            cfg.always = parse_bool(v)?;
        }
        if let Some(v) = opts.get("Q").or_else(|| opts.get("queueSize")) {
            let n: i64 = v
                .parse()
                .map_err(|_| PvaLinkParseError::BadOption(format!("Q={v}")))?;
            // pvxs clamps `Q < 1` to 1.
            cfg.queue_size = if n < 1 { 1 } else { n as usize };
        }
        if let Some(v) = opts.get("pipeline") {
            cfg.pipeline = parse_bool(v)?;
        }
        if let Some(v) = opts.get("defer") {
            cfg.defer = parse_bool(v)?;
        }
        if let Some(v) = opts.get("retry") {
            cfg.retry = parse_bool(v)?;
        }
        if let Some(v) = opts.get("local") {
            cfg.local = parse_bool(v)?;
        }
        if let Some(v) = opts.get("atomic") {
            cfg.atomic = parse_bool(v)?;
        }
        if let Some(v) = opts.get("monorder") {
            let n: i64 = v
                .parse()
                .map_err(|_| PvaLinkParseError::BadOption(format!("monorder={v}")))?;
            // pvxs clamps to [-1024, 1024].
            cfg.monorder = n.clamp(-1024, 1024) as i32;
        }
        // BR-R19: `time` adopts the linked PV's NT timestamp on read.
        if let Some(v) = opts.get("time") {
            cfg.time = parse_bool(v)?;
        }

        // Apply legacy bare modifiers
        for m in legacy_mods {
            match m.as_str() {
                "PP" | "pp" => cfg.process = true,
                "NPP" | "npp" => cfg.process = false,
                "CP" | "cp" => {
                    cfg.monitor = true;
                    cfg.scan_on_update = true;
                }
                "CPP" | "cpp" => {
                    cfg.monitor = true;
                    cfg.scan_on_update = true;
                }
                "MS" | "ms" => cfg.sevr = SevrMode::Ms,
                "MSI" | "msi" => cfg.sevr = SevrMode::Msi,
                "MSS" | "mss" => cfg.sevr = SevrMode::Ms, // MSS == MS at our granularity
                "NMS" | "nms" => cfg.sevr = SevrMode::Nms,
                _ => {}
            }
        }

        Ok(cfg)
    }

    /// Construct a config with pvxs-default option values for the
    /// given PV name + direction. Used by the parser and by callers
    /// (the resolver) that build configs programmatically rather than
    /// parsing a link string.
    pub fn defaults_for(pv_name: &str, direction: LinkDirection) -> Self {
        PvaLinkConfig {
            pv_name: pv_name.to_string(),
            field: "value".to_string(),
            monitor: false,
            process: false,
            notify: false,
            scan_on_update: false,
            always: false,
            sevr: SevrMode::Nms,
            queue_size: DEFAULT_QUEUE_SIZE,
            pipeline: false,
            defer: false,
            retry: false,
            local: false,
            atomic: false,
            monorder: 0,
            time: false,
            direction,
        }
    }
}

fn strip_legacy_mods(body: &str) -> (&str, Vec<String>) {
    // Legacy DBD links can have whitespace-separated trailing tokens like
    // "PV:NAME PP MS". Detect and split those off.
    let mut parts: Vec<&str> = body.split_whitespace().collect();
    if parts.len() <= 1 {
        return (body, Vec::new());
    }
    let head = parts.remove(0);
    let mods: Vec<String> = parts.into_iter().map(|s| s.to_string()).collect();
    (head, mods)
}

fn parse_query(q: &str) -> Result<HashMap<String, String>, PvaLinkParseError> {
    let mut out = HashMap::new();
    for chunk in q.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = chunk
            .split_once('=')
            .ok_or_else(|| PvaLinkParseError::BadOption(chunk.to_string()))?;
        out.insert(k.to_string(), v.to_string());
    }
    Ok(out)
}

/// Parse a `sevr` option value. Accepts the pvxs string forms
/// (`NMS`/`MS`/`MSI`/`MSS`) plus boolean shorthands — pvxs maps
/// `sevr:true` → `MS` and `sevr:false` → `NMS`.
fn parse_sevr(v: &str) -> Result<SevrMode, PvaLinkParseError> {
    match v {
        "NMS" | "nms" | "false" | "FALSE" | "0" | "no" | "NO" => Ok(SevrMode::Nms),
        // pvxs treats MSS as a maximize-severity variant; at our
        // granularity (no separate status propagation) it equals MS.
        "MS" | "ms" | "MSS" | "mss" | "true" | "TRUE" | "1" | "yes" | "YES" => Ok(SevrMode::Ms),
        "MSI" | "msi" => Ok(SevrMode::Msi),
        other => Err(PvaLinkParseError::BadOption(format!("sevr={other}"))),
    }
}

fn parse_bool(v: &str) -> Result<bool, PvaLinkParseError> {
    match v {
        "true" | "TRUE" | "1" | "yes" | "YES" => Ok(true),
        "false" | "FALSE" | "0" | "no" | "NO" => Ok(false),
        other => Err(PvaLinkParseError::BadOption(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_pv_name() {
        let c = PvaLinkConfig::parse("pva://OTHER:PV", LinkDirection::Inp).unwrap();
        assert_eq!(c.pv_name, "OTHER:PV");
        assert_eq!(c.field, "value");
        assert!(!c.monitor);
        assert!(!c.process);
        // BR-R19: `time` defaults to false to match pvxs.
        assert!(!c.time);
    }

    /// BR-R19: `?time=true` enables remote-timestamp adoption.
    #[test]
    fn time_option_parses_true() {
        let c = PvaLinkConfig::parse("pva://X?time=true", LinkDirection::Inp).unwrap();
        assert!(c.time, "time=true must parse");
        let c = PvaLinkConfig::parse("pva://X?time=false", LinkDirection::Inp).unwrap();
        assert!(!c.time, "time=false must parse");
    }

    #[test]
    fn at_prefix_accepted() {
        let c = PvaLinkConfig::parse("@pva://X", LinkDirection::Out).unwrap();
        assert_eq!(c.pv_name, "X");
    }

    #[test]
    fn query_options() {
        let c = PvaLinkConfig::parse(
            "pva://A?field=alarm.severity&monitor=true&proc=PASSIVE",
            LinkDirection::Inp,
        )
        .unwrap();
        assert_eq!(c.field, "alarm.severity");
        assert!(c.monitor);
        assert!(c.process);
    }

    #[test]
    fn legacy_pp_modifier() {
        let c = PvaLinkConfig::parse("pva://X PP", LinkDirection::Out).unwrap();
        assert_eq!(c.pv_name, "X");
        assert!(c.process);
    }

    #[test]
    fn empty_pv_rejected() {
        assert!(matches!(
            PvaLinkConfig::parse("pva://", LinkDirection::Inp),
            Err(PvaLinkParseError::EmptyPv)
        ));
    }

    #[test]
    fn non_pva_rejected() {
        assert!(matches!(
            PvaLinkConfig::parse("ca://X", LinkDirection::Inp),
            Err(PvaLinkParseError::NotPvaLink(_))
        ));
    }

    // ---- B2: MS / NMS / MSI severity flags ----

    #[test]
    fn sevr_defaults_to_nms() {
        let c = PvaLinkConfig::parse("pva://X", LinkDirection::Inp).unwrap();
        assert_eq!(c.sevr, SevrMode::Nms);
    }

    #[test]
    fn sevr_legacy_modifiers() {
        assert_eq!(
            PvaLinkConfig::parse("pva://X MS", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Ms
        );
        assert_eq!(
            PvaLinkConfig::parse("pva://X MSI", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Msi
        );
        assert_eq!(
            PvaLinkConfig::parse("pva://X NMS", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Nms
        );
        // MSS folds to MS at our granularity.
        assert_eq!(
            PvaLinkConfig::parse("pva://X MSS", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Ms
        );
    }

    #[test]
    fn sevr_query_option() {
        assert_eq!(
            PvaLinkConfig::parse("pva://X?sevr=MS", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Ms
        );
        assert_eq!(
            PvaLinkConfig::parse("pva://X?sevr=MSI", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Msi
        );
        // pvxs boolean shorthand: true→MS, false→NMS.
        assert_eq!(
            PvaLinkConfig::parse("pva://X?sevr=true", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Ms
        );
        assert_eq!(
            PvaLinkConfig::parse("pva://X?sevr=false", LinkDirection::Inp)
                .unwrap()
                .sevr,
            SevrMode::Nms
        );
    }

    #[test]
    fn sevr_propagation_semantics() {
        // NMS: never propagates.
        for sev in 0..=3 {
            assert!(!SevrMode::Nms.propagates(sev));
        }
        // MS: any non-NO_ALARM severity propagates.
        assert!(!SevrMode::Ms.propagates(0)); // NO_ALARM
        assert!(SevrMode::Ms.propagates(1)); // MINOR
        assert!(SevrMode::Ms.propagates(2)); // MAJOR
        assert!(SevrMode::Ms.propagates(3)); // INVALID
        // MSI: only INVALID propagates.
        assert!(!SevrMode::Msi.propagates(0));
        assert!(!SevrMode::Msi.propagates(1));
        assert!(!SevrMode::Msi.propagates(2));
        assert!(SevrMode::Msi.propagates(3));
    }

    // ---- B4: link options ----

    #[test]
    fn proc_cp_implies_monitor_and_scan() {
        let c = PvaLinkConfig::parse("pva://X?proc=CP", LinkDirection::Inp).unwrap();
        assert!(c.monitor);
        assert!(c.scan_on_update);
        let c2 = PvaLinkConfig::parse("pva://X CPP", LinkDirection::Inp).unwrap();
        assert!(c2.monitor);
        assert!(c2.scan_on_update);
    }

    #[test]
    fn queue_size_parsing_and_clamp() {
        assert_eq!(
            PvaLinkConfig::parse("pva://X?Q=8", LinkDirection::Inp)
                .unwrap()
                .queue_size,
            8
        );
        // pvxs clamps Q < 1 to 1.
        assert_eq!(
            PvaLinkConfig::parse("pva://X?Q=0", LinkDirection::Inp)
                .unwrap()
                .queue_size,
            1
        );
        // default
        assert_eq!(
            PvaLinkConfig::parse("pva://X", LinkDirection::Inp)
                .unwrap()
                .queue_size,
            DEFAULT_QUEUE_SIZE
        );
        // `queueSize` alias also accepted.
        assert_eq!(
            PvaLinkConfig::parse("pva://X?queueSize=16", LinkDirection::Inp)
                .unwrap()
                .queue_size,
            16
        );
    }

    #[test]
    fn monorder_parsing_and_clamp() {
        assert_eq!(
            PvaLinkConfig::parse("pva://X?monorder=5", LinkDirection::Inp)
                .unwrap()
                .monorder,
            5
        );
        assert_eq!(
            PvaLinkConfig::parse("pva://X?monorder=99999", LinkDirection::Inp)
                .unwrap()
                .monorder,
            1024
        );
        assert_eq!(
            PvaLinkConfig::parse("pva://X?monorder=-99999", LinkDirection::Inp)
                .unwrap()
                .monorder,
            -1024
        );
    }

    #[test]
    fn boolean_options_parsed() {
        let c = PvaLinkConfig::parse(
            "pva://X?pipeline=true&defer=true&retry=true&local=true&atomic=true&always=true",
            LinkDirection::Out,
        )
        .unwrap();
        assert!(c.pipeline);
        assert!(c.defer);
        assert!(c.retry);
        assert!(c.local);
        assert!(c.atomic);
        assert!(c.always);
    }

    #[test]
    fn boolean_options_default_false() {
        let c = PvaLinkConfig::parse("pva://X", LinkDirection::Out).unwrap();
        assert!(!c.pipeline);
        assert!(!c.defer);
        assert!(!c.retry);
        assert!(!c.local);
        assert!(!c.atomic);
        assert!(!c.always);
        assert_eq!(c.monorder, 0);
    }

    #[test]
    fn bad_option_value_rejected() {
        assert!(matches!(
            PvaLinkConfig::parse("pva://X?Q=notanumber", LinkDirection::Inp),
            Err(PvaLinkParseError::BadOption(_))
        ));
        assert!(matches!(
            PvaLinkConfig::parse("pva://X?sevr=BOGUS", LinkDirection::Inp),
            Err(PvaLinkParseError::BadOption(_))
        ));
    }
}
