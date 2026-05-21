//! Parser for `@pva://...` link strings.
//!
//! Accepted forms (matches pvxs `pvalink_jlif.cpp`):
//!
//! ```text
//! pva://PV:NAME                              — bare PV name, default options
//! pva://PV:NAME?field=value                  — explicit value field
//! pva://PV:NAME?proc=NPP&monitor=true        — multiple options
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

/// pvxs pvalink `proc` mode — the five-state enum the JSON / legacy
/// parser preserves (`pvalink_jlif.cpp:69-166`).
///
/// BRIDGE-FR-16: the OUT-side process behaviour and the INP-side
/// scan-on-update behaviour are *related but distinct* (pvxs derives
/// INP scan at `pvalink_link.cpp:122` and the PUT process request at
/// `pvalink_channel.cpp:237-263` from the same enum). Collapsing this
/// into a single `bool` lost two distinctions: `Default` vs `Npp`
/// (both wrote the wire value `"passive"` instead of `"passive"` vs
/// `"false"`), and `Cp`/`Cpp` (which request remote processing on PUT,
/// `"true"`, but were stored as scan-only flags leaving the bool
/// `false`). This enum keeps all five states; the two outputs are
/// derived from it via [`Self::put_process_request`] and
/// [`Self::inp_scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ProcMode {
    /// No `proc` given — pvxs `Default`. PUT request → `"passive"`
    /// (process the target only if it is Passive-scanned).
    #[default]
    Default,
    /// `proc=PP` / `proc=true` — process the target on PUT. PUT
    /// request → `"true"`.
    Pp,
    /// `proc=NPP` / `proc=false` — explicit no-process. PUT request →
    /// `"false"` (distinct from `Default`'s `"passive"`).
    Npp,
    /// `proc=CP` — INP scan-on-update on every monitor event; on PUT,
    /// requests remote processing (`"true"`).
    Cp,
    /// `proc=CPP` — INP scan-on-update only when the owning record is
    /// Passive; on PUT, requests remote processing (`"true"`).
    Cpp,
}

impl ProcMode {
    /// The `record._options.process` wire value for an OUT PUT.
    /// Mirrors pvxs `pvalink_channel.cpp:237-263`:
    /// `Default → "passive"`, `Npp → "false"`,
    /// `Pp` / `Cp` / `Cpp → "true"`.
    pub fn put_process_request(self) -> &'static str {
        match self {
            ProcMode::Default => "passive",
            ProcMode::Npp => "false",
            ProcMode::Pp | ProcMode::Cp | ProcMode::Cpp => "true",
        }
    }

    /// INP scan-on-update derivation: `(scan_on_update, scan_on_passive)`.
    /// Only `Cp` / `Cpp` scan; `Cpp` gates on the owning record being
    /// Passive. Mirrors pvxs `pvalink_link.cpp:122`.
    pub fn inp_scan(self) -> (bool, bool) {
        match self {
            ProcMode::Cp => (true, false),
            ProcMode::Cpp => (true, true),
            ProcMode::Default | ProcMode::Pp | ProcMode::Npp => (false, false),
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
    /// BRIDGE-FR-16: pvxs `proc` mode (`Default`/`PP`/`NPP`/`CP`/`CPP`),
    /// preserved as a five-state enum. Drives the OUT-side PUT process
    /// request ([`ProcMode::put_process_request`]) and, for `CP`/`CPP`,
    /// the INP scan-on-update flags below (set at parse time via
    /// [`ProcMode::inp_scan`]).
    pub proc: ProcMode,
    /// True iff the link reports DBE_VALUE notifications back to the local
    /// record (INP, monitor mode).
    pub notify: bool,
    /// When true, every monitor update triggers `process()` on the
    /// owning record (INP-side equivalent of the legacy
    /// `record(... CP)` flag). Mirrors pvxs `pvaLink::scanOnUpdate`
    /// (pvalink_link.cpp:122). Default `false` — record must be
    /// scanned externally.
    pub scan_on_update: bool,
    /// BR-R28: when true, the `scan_on_update` processing fires only
    /// when the owning record's `SCAN` is `Passive`. Distinguishes
    /// pvxs `CPP` (`scanOnUpdatePassive`, pvalink_link.cpp:122 →
    /// pvalink_channel.cpp:313) from `CP` (`scanOnUpdateYes`, which
    /// always fires). Without this flag, `CPP` collapses to `CP` and
    /// can trigger processing on non-Passive records — changing
    /// FLNK/output-link cascades vs pvxs.
    pub scan_on_passive: bool,
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
            // BRIDGE-FR-16: pvxs `proc` is a five-state enum
            // (`pvalink_jlif.cpp:69-166`): boolean true→PP / false→NPP,
            // plus the strings NPP/PP/CP/CPP. `CP`/`CPP` additionally
            // imply an open monitor and INP scan-on-update. Store the
            // enum and derive the scan flags from it; `PASSIVE` is NOT
            // a pvxs `proc` enum value (it is only the later wire
            // request string for `Default`), so reject it rather than
            // silently treating it as process-on-PUT.
            cfg.proc = match v.as_str() {
                "TRUE" | "true" | "1" | "PP" | "pp" => ProcMode::Pp,
                "FALSE" | "false" | "0" | "NPP" | "npp" => ProcMode::Npp,
                "CP" | "cp" => ProcMode::Cp,
                "CPP" | "cpp" => ProcMode::Cpp,
                other => return Err(PvaLinkParseError::BadOption(other.to_string())),
            };
            let (sou, sop) = cfg.proc.inp_scan();
            if sou {
                cfg.monitor = true;
                cfg.scan_on_update = true;
                cfg.scan_on_passive = sop;
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
                // BRIDGE-FR-16: legacy bare modifiers map to the same
                // five-state `ProcMode`; CP/CPP additionally drive the
                // INP scan flags via `inp_scan`.
                "PP" | "pp" => cfg.proc = ProcMode::Pp,
                "NPP" | "npp" => cfg.proc = ProcMode::Npp,
                "CP" | "cp" | "CPP" | "cpp" => {
                    cfg.proc = if m.eq_ignore_ascii_case("CPP") {
                        ProcMode::Cpp
                    } else {
                        ProcMode::Cp
                    };
                    let (_sou, sop) = cfg.proc.inp_scan();
                    cfg.monitor = true;
                    cfg.scan_on_update = true;
                    cfg.scan_on_passive = sop;
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
            proc: ProcMode::Default,
            notify: false,
            scan_on_update: false,
            scan_on_passive: false,
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
        assert_eq!(c.proc, ProcMode::Default);
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

    /// BR-R28: `CP` sets scan_on_update without scan_on_passive;
    /// `CPP` sets both. The legacy bare `CP` / `CPP` modifier path
    /// follows the same rule.
    #[test]
    fn cp_vs_cpp_distinguished() {
        let cp = PvaLinkConfig::parse("pva://X?proc=CP", LinkDirection::Inp).unwrap();
        assert!(cp.scan_on_update, "CP must enable scan_on_update");
        assert!(
            !cp.scan_on_passive,
            "CP must NOT set scan_on_passive (pvxs scanOnUpdateYes)"
        );

        let cpp = PvaLinkConfig::parse("pva://X?proc=CPP", LinkDirection::Inp).unwrap();
        assert!(cpp.scan_on_update, "CPP must enable scan_on_update");
        assert!(
            cpp.scan_on_passive,
            "CPP must set scan_on_passive (pvxs scanOnUpdatePassive)"
        );

        // Legacy bare modifier
        let bare_cpp = PvaLinkConfig::parse("pva://X CPP", LinkDirection::Inp).unwrap();
        assert!(
            bare_cpp.scan_on_passive,
            "bare CPP must set scan_on_passive"
        );
        let bare_cp = PvaLinkConfig::parse("pva://X CP", LinkDirection::Inp).unwrap();
        assert!(
            !bare_cp.scan_on_passive,
            "bare CP must not set scan_on_passive"
        );
    }

    #[test]
    fn at_prefix_accepted() {
        let c = PvaLinkConfig::parse("@pva://X", LinkDirection::Out).unwrap();
        assert_eq!(c.pv_name, "X");
    }

    #[test]
    fn query_options() {
        let c = PvaLinkConfig::parse(
            "pva://A?field=alarm.severity&monitor=true&proc=PP",
            LinkDirection::Inp,
        )
        .unwrap();
        assert_eq!(c.field, "alarm.severity");
        assert!(c.monitor);
        assert_eq!(c.proc, ProcMode::Pp);
    }

    #[test]
    fn legacy_pp_modifier() {
        let c = PvaLinkConfig::parse("pva://X PP", LinkDirection::Out).unwrap();
        assert_eq!(c.pv_name, "X");
        assert_eq!(c.proc, ProcMode::Pp);
        assert_eq!(c.proc.put_process_request(), "true");
    }

    /// BRIDGE-FR-16: the five pvxs `proc` states are preserved through
    /// parsing and each derives the correct PUT `process` wire value.
    /// `Default` (`"passive"`) and `NPP` (`"false"`) are distinct;
    /// `CP`/`CPP` request remote processing (`"true"`) on PUT even
    /// though they are primarily INP scan modes.
    #[test]
    fn fr16_proc_enum_preserved_and_put_request_derived() {
        let cases = [
            ("pva://X", ProcMode::Default, "passive"),
            ("pva://X?proc=PP", ProcMode::Pp, "true"),
            ("pva://X?proc=NPP", ProcMode::Npp, "false"),
            ("pva://X?proc=CP", ProcMode::Cp, "true"),
            ("pva://X?proc=CPP", ProcMode::Cpp, "true"),
            // Boolean shorthands map to PP / NPP.
            ("pva://X?proc=true", ProcMode::Pp, "true"),
            ("pva://X?proc=false", ProcMode::Npp, "false"),
        ];
        for (link, want_mode, want_wire) in cases {
            let c = PvaLinkConfig::parse(link, LinkDirection::Out).unwrap();
            assert_eq!(c.proc, want_mode, "{link}: proc mode");
            assert_eq!(
                c.proc.put_process_request(),
                want_wire,
                "{link}: PUT process wire value"
            );
        }
    }

    /// BRIDGE-FR-16: `Default` and `NPP` must NOT collapse — pre-fix
    /// both produced `"passive"`; an explicit `NPP` must send `"false"`.
    #[test]
    fn fr16_default_and_npp_are_distinct_on_the_wire() {
        let default = PvaLinkConfig::parse("pva://X", LinkDirection::Out).unwrap();
        let npp = PvaLinkConfig::parse("pva://X?proc=NPP", LinkDirection::Out).unwrap();
        assert_ne!(default.proc, npp.proc);
        assert_eq!(default.proc.put_process_request(), "passive");
        assert_eq!(npp.proc.put_process_request(), "false");
    }

    /// BRIDGE-FR-16: `CP`/`CPP` derive INP scan-on-update from the same
    /// enum (`inp_scan`), independent of the PUT process request — they
    /// are related but distinct outputs (pvxs `pvalink_link.cpp:122` vs
    /// `pvalink_channel.cpp:237`).
    #[test]
    fn fr16_cp_cpp_derive_scan_flags_and_put_true() {
        let cp = PvaLinkConfig::parse("pva://X?proc=CP", LinkDirection::Inp).unwrap();
        assert_eq!(cp.proc.inp_scan(), (true, false));
        assert!(cp.scan_on_update && !cp.scan_on_passive && cp.monitor);
        assert_eq!(cp.proc.put_process_request(), "true");

        let cpp = PvaLinkConfig::parse("pva://X?proc=CPP", LinkDirection::Inp).unwrap();
        assert_eq!(cpp.proc.inp_scan(), (true, true));
        assert!(cpp.scan_on_update && cpp.scan_on_passive && cpp.monitor);
        assert_eq!(cpp.proc.put_process_request(), "true");
    }

    /// BRIDGE-FR-16: `PASSIVE` is the wire request string for `Default`,
    /// not a pvxs `proc` enum value, so it must be rejected — pre-fix it
    /// was silently accepted as process-on-PUT.
    #[test]
    fn fr16_passive_is_not_a_valid_proc_value() {
        assert!(matches!(
            PvaLinkConfig::parse("pva://X?proc=PASSIVE", LinkDirection::Out),
            Err(PvaLinkParseError::BadOption(_))
        ));
        assert!(matches!(
            PvaLinkConfig::parse("pva://X?proc=passive", LinkDirection::Out),
            Err(PvaLinkParseError::BadOption(_))
        ));
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
