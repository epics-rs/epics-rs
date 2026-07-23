//! Parser for `pva://...` link strings.
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

use epics_base_rs::server::record::JlinkValue;

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
/// the OUT-side process behaviour and the INP-side
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
    /// Only `Cp` and `Cpp` are monitor scan modes; `Cp` always fires and
    /// `Cpp` gates on the owning record being Passive. `Pp` is *not* a
    /// scan mode for an INP link.
    /// Mirrors pvxs/ioc `pvaLink::scanOnUpdate()`
    /// (`pvalink_link.cpp:122-134`): it returns `scanOnUpdateYes` only for
    /// `CP`, `scanOnUpdatePassive` only for `CPP`, and `scanOnUpdateNo`
    /// for every other mode (including `PP`/`NPP`/`Default`), so a `PP`
    /// INP link never registers in the monitor-event scan lists and only
    /// drives the OUT-side `record._options.process` request.
    pub fn inp_scan(self) -> (bool, bool) {
        match self {
            ProcMode::Cp => (true, false),
            ProcMode::Cpp => (true, true),
            ProcMode::Pp | ProcMode::Default | ProcMode::Npp => (false, false),
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
    /// pvxs `proc` mode (`Default`/`PP`/`NPP`/`CP`/`CPP`),
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
    /// when true, the `scan_on_update` processing fires only
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
    /// Defer the actual Put: when true a `write` only stages the value
    /// on the shared OUT channel and a non-deferred sibling — or the
    /// `LinkSet::flush_puts` production drain — flushes it, so several
    /// fields combine into one PUT. Mirrors pvxs `pvaLinkConfig::defer`.
    /// OUT only.
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
    /// when true, the owning record's TIME is adopted from
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
            Some((pv, q)) => (pv, parse_query(pv, q)),
            None => (body, HashMap::new()),
        };
        if pv_name.is_empty() {
            return Err(PvaLinkParseError::EmptyPv);
        }

        // `opts` is the convenience-URI option map split from the
        // `?key=value` query above. This is the Rust-extension path:
        // string values, lenient `yes`/`no` booleans. It is applied
        // through [`Self::apply_options`]. The structured-JSON JLink path
        // ([`Self::from_jlink_options`]) does NOT come through here — it
        // dispatches by JSON value KIND to stay pvxs-faithful, so a
        // string on a boolean key is ignored rather than coerced.
        let mut cfg = Self::apply_options(pv_name, &opts, direction);

        // Apply legacy bare modifiers
        for m in legacy_mods {
            // legacy bare modifiers map to the same five-state
            // `ProcMode`; CP/CPP additionally drive the INP scan flags
            // via `inp_scan`. These are CASE-SENSITIVE uppercase,
            // matching EPICS base's link-modifier parser, which
            // `strstr`s the raw modifier string against the literal
            // uppercase tokens `NPP`/`CPP`/`PP`/`CA`/`CP` and
            // `NMS`/`MSI`/`MSS`/`MS` (`epics-base/modules/database/src/
            // ioc/dbStatic/dbStaticLib.c:2369-2378`). A lowercase
            // modifier like `pp`/`ms` is not recognized by base, so
            // accepting it here would diverge from how the same link
            // parses under a C IOC.
            match m.as_str() {
                "PP" | "NPP" | "CP" | "CPP" => {
                    // Set the five-state mode, then derive the INP scan
                    // flags from it via the single `inp_scan` owner — the
                    // same uniform derivation used for the query `proc=`
                    // path above. `PP` is NOT special-cased to skip scan:
                    // pvxs's scan-list builder
                    // (`pva2pva/pdbApp/pvalink_channel.cpp:393-397`)
                    // includes `PP`/`CP`/`CPP` INP links and gates `PP`
                    // (like `CPP`) on the owning record being Passive, so
                    // a legacy `PP` suffix on an INP link must register a
                    // passive-only scan target, not stay PUT-only.
                    cfg.proc = match m.as_str() {
                        "PP" => ProcMode::Pp,
                        "NPP" => ProcMode::Npp,
                        "CP" => ProcMode::Cp,
                        _ => ProcMode::Cpp,
                    };
                    let (sou, sop) = cfg.proc.inp_scan();
                    if sou {
                        cfg.monitor = true;
                        cfg.scan_on_update = true;
                        cfg.scan_on_passive = sop;
                    }
                }
                "MS" => cfg.sevr = SevrMode::Ms,
                "MSI" => cfg.sevr = SevrMode::Msi,
                // pvxs aliases MSS onto MS itself, not a Rust limitation:
                // its `sevr` enum has no MSS variant (pvalink.h:83-86) and
                // the string parser maps it explicitly (pvalink_jlif.cpp:179-183,
                // "not sure how to handle mapping severity for MSS … handling as
                // alias for MS until then"). Exact parity, not coarse granularity.
                "MSS" => cfg.sevr = SevrMode::Ms,
                "NMS" => cfg.sevr = SevrMode::Nms,
                _ => {}
            }
        }

        Ok(cfg)
    }

    /// Build a config from the structured JLink members of a
    /// `{pva:{pv:"name", field:"f", proc:"CP", …}}` longhand
    /// (`epics_base_rs` `PvaJsonLink.options`), WITHOUT round-tripping
    /// through a `?key=value` query string.
    ///
    /// pvxs parses pvalink options only as JLink map keys / typed values
    /// (`pvalink_jlif.cpp:69-196`) — there is no `?key=value` URI query
    /// parser in the JLink callback table (`:286-300`). pvxs dispatches
    /// each option STRICTLY by its JSON value KIND through distinct
    /// callbacks (`pva_parse_{null,bool,integer,string}`, wired at
    /// `:286-300`). This path reproduces that dispatch via
    /// [`Self::apply_jlink_option`]; it does NOT share the
    /// convenience-URI [`Self::apply_options`], which collapses every
    /// value to text and accepts lenient `yes`/`no` booleans. Keeping the
    /// two paths separate is the structural reason a JSON string on a
    /// boolean key (`pipeline:"yes"`) is IGNORED here, exactly as pvxs
    /// ignores it (`pva_parse_string` unknown-key branch, `:189-191`),
    /// instead of being honored as a Rust extension. [`Self::parse`]
    /// remains the Rust convenience-URI path.
    pub fn from_jlink_options(
        pv_name: &str,
        options: &[(String, JlinkValue)],
        direction: LinkDirection,
    ) -> Result<Self, PvaLinkParseError> {
        let pv_name = pv_name.trim();
        if pv_name.is_empty() {
            return Err(PvaLinkParseError::EmptyPv);
        }
        let mut cfg = PvaLinkConfig {
            pv_name: pv_name.to_string(),
            direction,
            ..PvaLinkConfig::defaults_for(pv_name, direction)
        };
        for (key, val) in options {
            Self::apply_jlink_option(&mut cfg, pv_name, key, val);
        }
        // `proc` CP/CPP imply an open monitor + INP scan-on-update.
        // Derive the scan flags from the FINAL `proc` value (pvxs builds
        // its scan list at open() time from the parsed config,
        // `pvalink_channel.cpp:393-397`, not per-option), so a `proc:"CP"`
        // forces the monitor on regardless of option order.
        let (sou, sop) = cfg.proc.inp_scan();
        if sou {
            cfg.monitor = true;
            cfg.scan_on_update = true;
            cfg.scan_on_passive = sop;
        }
        Ok(cfg)
    }

    /// Apply ONE JLink option, dispatching by its JSON value KIND exactly
    /// as pvxs's pvalink callback table does (pvxs `ioc/pvalink_jlif.cpp`):
    /// `pva_parse_bool` (`:90-122`) for booleans, `pva_parse_integer`
    /// (`:124-141`) for integers, `pva_parse_string` (`:143-197`) for
    /// strings, `pva_parse_null` (`:69-88`) for null.
    ///
    /// A value whose kind does not match the key's accepting callback —
    /// a string `pipeline:"yes"` (boolean-only key), or a string
    /// `proc:"true"` that is not one of `CP`/`CPP`/`PP`/`NPP` — falls
    /// through that callback's unknown branch and is IGNORED, never
    /// coerced. The boolean shorthands `proc:true`/`proc:false`
    /// (→`PP`/`NPP`) and `sevr:true`/`sevr:false` (→`MS`/`NMS`) come from
    /// `pva_parse_bool` (`:96-99`), so they are accepted only as JSON
    /// booleans, not as the strings `"true"`/`"false"`. Options that are
    /// not pvxs JLink keys at all (`monitor`, `notify`, `scan_on_update`,
    /// `queueSize`) are likewise ignored — those exist only on the
    /// convenience-URI [`Self::apply_options`] path.
    fn apply_jlink_option(cfg: &mut PvaLinkConfig, pv_name: &str, key: &str, val: &JlinkValue) {
        match val {
            JlinkValue::Bool(b) => match key {
                "proc" => cfg.proc = if *b { ProcMode::Pp } else { ProcMode::Npp },
                "sevr" => cfg.sevr = if *b { SevrMode::Ms } else { SevrMode::Nms },
                "defer" => cfg.defer = *b,
                "pipeline" => cfg.pipeline = *b,
                "time" => cfg.time = *b,
                "retry" => cfg.retry = *b,
                "local" => cfg.local = *b,
                "always" => cfg.always = *b,
                "atomic" => cfg.atomic = *b,
                _ => warn_ignored_jlink(pv_name, key, val),
            },
            JlinkValue::Int(n) => match key {
                // pvxs clamps `Q < 1` to 1 (pvalink_jlif.cpp:129-130).
                "Q" => cfg.queue_size = if *n < 1 { 1 } else { *n as usize },
                // pvxs clamps monorder to [-1024, 1024] (`:131-132`).
                "monorder" => cfg.monorder = (*n).clamp(-1024, 1024) as i32,
                _ => warn_ignored_jlink(pv_name, key, val),
            },
            JlinkValue::Str(s) => match key {
                "field" => cfg.field = s.clone(),
                // CASE-SENSITIVE enum strings (pva_parse_string
                // :156-170): only CP/CPP/PP/NPP and the empty string
                // (→Default) are recognized; anything else (incl. a
                // lowercase typo, or the literal "true") is ignored.
                "proc" => match s.as_str() {
                    "" => cfg.proc = ProcMode::Default,
                    "CP" => cfg.proc = ProcMode::Cp,
                    "CPP" => cfg.proc = ProcMode::Cpp,
                    "PP" => cfg.proc = ProcMode::Pp,
                    "NPP" => cfg.proc = ProcMode::Npp,
                    _ => warn_ignored_jlink(pv_name, key, val),
                },
                // pva_parse_string :172-187: NMS/MS/MSI, MSS aliased to MS.
                "sevr" => match s.as_str() {
                    "NMS" => cfg.sevr = SevrMode::Nms,
                    "MS" => cfg.sevr = SevrMode::Ms,
                    "MSI" => cfg.sevr = SevrMode::Msi,
                    "MSS" => cfg.sevr = SevrMode::Ms,
                    _ => warn_ignored_jlink(pv_name, key, val),
                },
                _ => warn_ignored_jlink(pv_name, key, val),
            },
            JlinkValue::Null => match key {
                // pva_parse_null :74-83.
                "proc" => cfg.proc = ProcMode::Default,
                "sevr" => cfg.sevr = SevrMode::Nms,
                "local" => cfg.local = false,
                _ => warn_ignored_jlink(pv_name, key, val),
            },
        }
    }

    /// The single owner of "convenience-URI option map →
    /// [`PvaLinkConfig`]".
    ///
    /// Used ONLY by [`Self::parse`] (after splitting the `?key=value`
    /// query). This is the Rust-extension path: every value is a string
    /// and booleans accept lenient `yes`/`no`. The pvxs-parity
    /// structured-JSON path is [`Self::from_jlink_options`] /
    /// [`Self::apply_jlink_option`], which dispatches by JSON value KIND
    /// and does NOT come through here. Legacy bare modifiers (`PP`/`MS`/…)
    /// are NOT handled here — they exist only on the convenience-URI path
    /// and are applied by `parse`.
    fn apply_options(
        pv_name: &str,
        opts: &HashMap<String, String>,
        direction: LinkDirection,
    ) -> Self {
        let mut cfg = PvaLinkConfig {
            pv_name: pv_name.to_string(),
            direction,
            ..PvaLinkConfig::defaults_for(pv_name, direction)
        };

        if let Some(v) = opts.get("field") {
            cfg.field = v.clone();
        }
        // `monitor` is handled before `proc` so a `proc=CP`/`CPP` (which
        // forces an open monitor below) takes precedence over an explicit
        // `monitor=false` on the same link, matching the prior ordering.
        apply_or_warn(pv_name, "monitor", opts, &mut cfg.monitor, parse_bool);
        if let Some(v) = opts.get("proc") {
            // pvxs `proc` is a five-state enum
            // (`pvalink_jlif.cpp:69-166`): boolean true→PP / false→NPP,
            // plus the strings NPP/PP/CP/CPP. `CP`/`CPP` additionally
            // imply an open monitor and INP scan-on-update. Store the
            // enum and derive the scan flags from it. `PASSIVE` is NOT
            // a pvxs `proc` enum value (it is only the later wire
            // request string for `Default`); an unknown value is warned
            // and the prior (default) mode is kept — pvxs does the same
            // (`log_warn_printf` + `jlif_continue`, pvalink_jlif.cpp:156-170)
            // rather than failing the whole link.
            //
            // The enum string forms are CASE-SENSITIVE uppercase, matching
            // pvxs `pva_parse_string` (`pvalink_jlif.cpp:156-170`), which
            // compares `sval` byte-for-byte against `CP`/`CPP`/`PP`/`NPP`
            // and warns "unknown proc" on anything else. A lowercase typo
            // like `proc=cp` must therefore be ignored (kept at default),
            // not silently activated — otherwise the same link behaves
            // differently under Rust vs pvxs/QSRV. The boolean shorthands
            // (`true`/`false`/`1`/`0`) are a distinct path: pvxs accepts a
            // JSON boolean `proc:true`→PP / `proc:false`→NPP via
            // `pva_parse_bool` (`pvalink_jlif.cpp:96-98`), so they are kept.
            match v.as_str() {
                "TRUE" | "true" | "1" => cfg.proc = ProcMode::Pp,
                "FALSE" | "false" | "0" => cfg.proc = ProcMode::Npp,
                "PP" => cfg.proc = ProcMode::Pp,
                "NPP" => cfg.proc = ProcMode::Npp,
                "CP" => cfg.proc = ProcMode::Cp,
                "CPP" => cfg.proc = ProcMode::Cpp,
                _ => warn_ignored_option(pv_name, "proc", v),
            }
            let (sou, sop) = cfg.proc.inp_scan();
            if sou {
                cfg.monitor = true;
                cfg.scan_on_update = true;
                cfg.scan_on_passive = sop;
            }
        }
        apply_or_warn(pv_name, "notify", opts, &mut cfg.notify, parse_bool);
        apply_or_warn(
            pv_name,
            "scan_on_update",
            opts,
            &mut cfg.scan_on_update,
            parse_bool,
        );
        apply_or_warn(pv_name, "sevr", opts, &mut cfg.sevr, parse_sevr);
        apply_or_warn(pv_name, "always", opts, &mut cfg.always, parse_bool);
        if let Some(v) = opts.get("Q").or_else(|| opts.get("queueSize")) {
            match v.parse::<i64>() {
                // pvxs clamps `Q < 1` to 1.
                Ok(n) => cfg.queue_size = if n < 1 { 1 } else { n as usize },
                Err(_) => warn_ignored_option(pv_name, "Q", v),
            }
        }
        apply_or_warn(pv_name, "pipeline", opts, &mut cfg.pipeline, parse_bool);
        apply_or_warn(pv_name, "defer", opts, &mut cfg.defer, parse_bool);
        apply_or_warn(pv_name, "retry", opts, &mut cfg.retry, parse_bool);
        apply_or_warn(pv_name, "local", opts, &mut cfg.local, parse_bool);
        apply_or_warn(pv_name, "atomic", opts, &mut cfg.atomic, parse_bool);
        if let Some(v) = opts.get("monorder") {
            match v.parse::<i64>() {
                // pvxs clamps to [-1024, 1024].
                Ok(n) => cfg.monorder = n.clamp(-1024, 1024) as i32,
                Err(_) => warn_ignored_option(pv_name, "monorder", v),
            }
        }
        // `time` adopts the linked PV's NT timestamp on read.
        apply_or_warn(pv_name, "time", opts, &mut cfg.time, parse_bool);

        cfg
    }

    /// Construct a config with pvxs-default option values for the
    /// given PV name + direction. Used by the parser and by callers
    /// (the resolver) that build configs programmatically rather than
    /// parsing a link string.
    pub fn defaults_for(pv_name: &str, direction: LinkDirection) -> Self {
        PvaLinkConfig {
            pv_name: pv_name.to_string(),
            // pvxs's default `field` is the empty string, which selects
            // the top-level structure (`pvalink.rst:13-30`,
            // `pvalink_link.cpp:90-110`): if that root is a structure
            // its `.value` is used, otherwise the root itself is the
            // value. Defaulting to `"value"` instead conflated the
            // default with an explicit `field=value` and could not
            // represent a top-level (non-NT) value. The selected-root
            // rule lives in `link::select_link_value`.
            field: String::new(),
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

fn parse_query(pv: &str, q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for chunk in q.split('&').filter(|s| !s.is_empty()) {
        match chunk.split_once('=') {
            Some((k, v)) => {
                out.insert(k.to_string(), v.to_string());
            }
            // pvxs warns on a malformed token and keeps parsing the rest
            // (`pvalink_jlif.cpp` parse callbacks return `jlif_continue`).
            // A segment without `=` is skipped so the link's valid
            // `key=value` siblings are still applied rather than the whole
            // link being discarded.
            None => tracing::warn!(
                target: "pvalink",
                pv = %pv,
                segment = %chunk,
                "pvalink: ignoring malformed query segment (no '='); \
                 remaining options on the link are still applied"
            ),
        }
    }
    out
}

/// Apply a parsed option value to `slot`, or warn-and-keep-default when
/// the value does not parse. Reads `key` from `opts`; a missing key
/// leaves the default untouched. pvxs parity: an unparseable option value
/// is logged and ignored, never failing the whole link
/// (`pvalink_jlif.cpp:90-194`, `log_warn_printf` + `jlif_continue`).
fn apply_or_warn<T>(
    pv: &str,
    key: &str,
    opts: &HashMap<String, String>,
    slot: &mut T,
    parse: impl Fn(&str) -> Result<T, PvaLinkParseError>,
) {
    if let Some(v) = opts.get(key) {
        match parse(v) {
            Ok(val) => *slot = val,
            Err(_) => warn_ignored_option(pv, key, v),
        }
    }
}

/// Warn that an option value could not be parsed and was ignored, leaving
/// the field at its default. pvxs parity (`pvalink_jlif.cpp:90-194`):
/// each parse callback `log_warn_printf`s an unknown/unparseable key or
/// value and returns `jlif_continue`, so one bad option never discards
/// the rest of the link.
/// Warn that a JLink option was ignored, rendering its JSON value KIND
/// in the log so a kind mismatch (`pipeline:"yes"` vs `pipeline:true`)
/// is visible. Delegates to [`warn_ignored_option`].
fn warn_ignored_jlink(pv: &str, key: &str, val: &JlinkValue) {
    let rendered = match val {
        JlinkValue::Null => "null".to_string(),
        JlinkValue::Bool(b) => b.to_string(),
        JlinkValue::Int(n) => n.to_string(),
        JlinkValue::Str(s) => format!("\"{s}\""),
    };
    warn_ignored_option(pv, key, &rendered);
}

fn warn_ignored_option(pv: &str, key: &str, value: &str) {
    tracing::warn!(
        target: "pvalink",
        pv = %pv,
        key = %key,
        value = %value,
        "pvalink: ignoring unparseable option value (keeping default); \
         remaining options on the link are still applied"
    );
}

/// Parse a `sevr` option value.
///
/// The enum string forms are CASE-SENSITIVE uppercase
/// (`NMS`/`MS`/`MSI`/`MSS`), matching pvxs `pva_parse_string`
/// (`pvalink_jlif.cpp:172-187`), which compares `sval` byte-for-byte and
/// warns "unknown sevr" on anything else (so a lowercase typo like
/// `sevr=msi` is ignored, keeping the default NMS — not silently turned
/// into INVALID-only propagation). `MSS` is accepted as an alias for
/// `MS` exactly as pvxs does (`pvalink_jlif.cpp:179-183`).
///
/// The boolean shorthands (`true`/`false`/`1`/`0`) are a distinct path:
/// pvxs maps a JSON boolean `sevr:true` → `MS` and `sevr:false` → `NMS`
/// via `pva_parse_bool` (`pvalink_jlif.cpp:99`), so they are kept.
fn parse_sevr(v: &str) -> Result<SevrMode, PvaLinkParseError> {
    match v {
        "NMS" | "false" | "FALSE" | "0" | "no" | "NO" => Ok(SevrMode::Nms),
        // pvxs treats MSS as a maximize-severity variant; at our
        // granularity (no separate status propagation) it equals MS.
        "MS" | "MSS" | "true" | "TRUE" | "1" | "yes" | "YES" => Ok(SevrMode::Ms),
        "MSI" => Ok(SevrMode::Msi),
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
        // pvxs default field is the empty string (top-level selection),
        // not "value".
        assert_eq!(c.field, "");
        assert!(!c.monitor);
        assert_eq!(c.proc, ProcMode::Default);
        // `time` defaults to false to match pvxs.
        assert!(!c.time);
    }

    // ---- JLink (structured `{pva:{...}}`) option kind dispatch ----
    //
    // pvxs routes each pvalink option strictly by its JSON value KIND
    // (pvalink_jlif.cpp:286-300): a boolean key reached with a string,
    // or a string key reached with a non-enum string, is IGNORED. These
    // assert `from_jlink_options` reproduces that, NOT the lenient
    // convenience-URI behavior (which accepts `yes`/`no` and `"true"`).

    fn jlink(pv: &str, opts: Vec<(&str, JlinkValue)>) -> PvaLinkConfig {
        let owned: Vec<(String, JlinkValue)> =
            opts.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        PvaLinkConfig::from_jlink_options(pv, &owned, LinkDirection::Inp).unwrap()
    }

    #[test]
    fn jlink_bool_pipeline_is_honored() {
        let c = jlink("X", vec![("pipeline", JlinkValue::Bool(true))]);
        assert!(
            c.pipeline,
            "JSON boolean pipeline:true must enable pipeline"
        );
    }

    #[test]
    fn jlink_string_on_bool_key_is_ignored() {
        // `pipeline:"yes"` is a JSON string on a boolean-only key. pvxs's
        // pva_parse_string ignores it (pvalink_jlif.cpp:189-191); the
        // convenience-URI path would have accepted "yes" as true.
        let c = jlink("X", vec![("pipeline", JlinkValue::Str("yes".to_string()))]);
        assert!(
            !c.pipeline,
            "JSON string pipeline:\"yes\" must be ignored, not coerced to true"
        );
    }

    #[test]
    fn jlink_bool_proc_true_is_pp() {
        let c = jlink("X", vec![("proc", JlinkValue::Bool(true))]);
        assert_eq!(c.proc, ProcMode::Pp, "proc:true → PP (pva_parse_bool)");
    }

    #[test]
    fn jlink_string_proc_true_is_ignored() {
        // pva_parse_string accepts only CP/CPP/PP/NPP/empty for `proc`;
        // "true" is unknown and ignored (pvalink_jlif.cpp:156-170). The
        // convenience-URI applier mapped the string "true" → PP.
        let c = jlink("X", vec![("proc", JlinkValue::Str("true".to_string()))]);
        assert_eq!(
            c.proc,
            ProcMode::Default,
            "proc:\"true\" (string) must be ignored, not mapped to PP"
        );
    }

    #[test]
    fn jlink_string_proc_cp_sets_scan() {
        let c = jlink("X", vec![("proc", JlinkValue::Str("CP".to_string()))]);
        assert_eq!(c.proc, ProcMode::Cp);
        assert!(c.scan_on_update, "CP → scan_on_update");
        assert!(c.monitor, "CP → open monitor");
    }

    #[test]
    fn jlink_int_q_sets_queue_but_string_q_ignored() {
        let c = jlink("X", vec![("Q", JlinkValue::Int(8))]);
        assert_eq!(c.queue_size, 8, "Q:8 (integer) sets the queue size");

        let default_q = PvaLinkConfig::defaults_for("X", LinkDirection::Inp).queue_size;
        let c = jlink("X", vec![("Q", JlinkValue::Str("8".to_string()))]);
        assert_eq!(
            c.queue_size, default_q,
            "Q:\"8\" (string) is ignored — pvxs Q accepts only an integer"
        );
    }

    #[test]
    fn jlink_bool_sevr_true_is_ms() {
        let c = jlink("X", vec![("sevr", JlinkValue::Bool(true))]);
        assert_eq!(c.sevr, SevrMode::Ms, "sevr:true → MS (pva_parse_bool)");
    }

    #[test]
    fn jlink_null_proc_resets_to_default() {
        let c = jlink("X", vec![("proc", JlinkValue::Null)]);
        assert_eq!(
            c.proc,
            ProcMode::Default,
            "proc:null → Default (pva_parse_null)"
        );
    }

    /// `?time=true` enables remote-timestamp adoption.
    #[test]
    fn time_option_parses_true() {
        let c = PvaLinkConfig::parse("pva://X?time=true", LinkDirection::Inp).unwrap();
        assert!(c.time, "time=true must parse");
        let c = PvaLinkConfig::parse("pva://X?time=false", LinkDirection::Inp).unwrap();
        assert!(!c.time, "time=false must parse");
    }

    /// `CP` sets scan_on_update without scan_on_passive;
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

    /// The parser tolerates a leading `@`, but nothing in an IOC can hand
    /// it one: `try_parse_hw_link` (`epics-base-rs` `link.rs:1074-1086`)
    /// claims any field starting with `@` as INST_IO before the scheme arm
    /// runs, and `iocInit` then refuses it on a soft record
    /// (`doc/pvalink-rtems-design.md` §12.2, measured on target). This is
    /// therefore leniency on a path with no producer, kept because
    /// removing it would be a behaviour change, not a spelling fix. Pinned
    /// so the tolerance is a decision rather than an accident.
    #[test]
    fn at_prefix_accepted_though_no_record_can_deliver_one() {
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

    /// the five pvxs `proc` states are preserved through
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

    /// `Default` and `NPP` must NOT collapse — pre-fix
    /// both produced `"passive"`; an explicit `NPP` must send `"false"`.
    #[test]
    fn fr16_default_and_npp_are_distinct_on_the_wire() {
        let default = PvaLinkConfig::parse("pva://X", LinkDirection::Out).unwrap();
        let npp = PvaLinkConfig::parse("pva://X?proc=NPP", LinkDirection::Out).unwrap();
        assert_ne!(default.proc, npp.proc);
        assert_eq!(default.proc.put_process_request(), "passive");
        assert_eq!(npp.proc.put_process_request(), "false");
    }

    /// `CP`/`CPP` derive INP scan-on-update from the same
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

    /// `PP` (`proc=PP`, `proc=true`, and the legacy bare `PP` suffix) is
    /// NOT an INP monitor scan mode under the pvxs/ioc pvalink dialect:
    /// `pvaLink::scanOnUpdate()` (`pvalink_link.cpp:122-134`) returns
    /// `scanOnUpdateNo` for everything except `CP`/`CPP`, so a `PP` INP
    /// link never registers a scan target and only carries the OUT-side
    /// `record._options.process` request.
    #[test]
    fn pp_inp_does_not_register_scan() {
        // PP is not a scan mode for INP links.
        assert_eq!(ProcMode::Pp.inp_scan(), (false, false));
        // still requests remote processing on PUT
        assert_eq!(ProcMode::Pp.put_process_request(), "true");

        for s in ["pva://X?proc=PP", "pva://X?proc=true", "pva://X PP"] {
            let c = PvaLinkConfig::parse(s, LinkDirection::Inp).unwrap();
            assert_eq!(c.proc, ProcMode::Pp, "{s}");
            assert!(
                !c.scan_on_update && !c.scan_on_passive,
                "{s}: PP must not register an INP scan target"
            );
        }

        // CP/CPP remain the only INP scan modes.
        let cp = PvaLinkConfig::parse("pva://X?proc=CP", LinkDirection::Inp).unwrap();
        assert!(cp.scan_on_update && !cp.scan_on_passive && cp.monitor);
        let cpp = PvaLinkConfig::parse("pva://X?proc=CPP", LinkDirection::Inp).unwrap();
        assert!(cpp.scan_on_update && cpp.scan_on_passive && cpp.monitor);

        // NPP / Default never scan.
        let npp = PvaLinkConfig::parse("pva://X?proc=NPP", LinkDirection::Inp).unwrap();
        assert!(!npp.scan_on_update);
        let def = PvaLinkConfig::parse("pva://X", LinkDirection::Inp).unwrap();
        assert!(!def.scan_on_update);
    }

    /// `PASSIVE` is the wire request string for `Default`, not a pvxs
    /// `proc` enum value. pvxs warns on an unknown `proc` value and keeps
    /// the prior (default) mode rather than failing the link
    /// (pvalink_jlif.cpp:156-170), so the link parses Ok with
    /// `proc == Default` and the bad value ignored — not as
    /// process-on-PUT.
    #[test]
    fn fr16_passive_proc_ignored_keeps_default() {
        for link in ["pva://X?proc=PASSIVE", "pva://X?proc=passive"] {
            let c = PvaLinkConfig::parse(link, LinkDirection::Out).unwrap();
            assert_eq!(
                c.proc,
                ProcMode::Default,
                "{link}: unknown proc ignored, default kept"
            );
        }
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

    /// pvxs enum string forms are case-sensitive uppercase. A lowercase
    /// typo such as `proc=cp` or `sevr=msi` is "unknown" to pvxs
    /// `pva_parse_string` (`pvalink_jlif.cpp:156-187`) and to EPICS
    /// base's modifier parser (`dbStaticLib.c:2369-2378`): it is warned
    /// and the prior/default mode is kept, never silently activated.
    /// The same link must therefore behave identically under Rust and
    /// pvxs/QSRV.
    #[test]
    fn proc_sevr_enum_strings_are_case_sensitive_uppercase() {
        // Lowercase `proc=` enum strings are ignored → default (not CP).
        for typo in ["cp", "cpp", "pp", "npp"] {
            let c =
                PvaLinkConfig::parse(&format!("pva://X?proc={typo}"), LinkDirection::Inp).unwrap();
            assert_eq!(
                c.proc,
                ProcMode::Default,
                "proc={typo} (lowercase) must be ignored, keeping default"
            );
            assert!(
                !c.scan_on_update,
                "lowercase proc={typo} must not turn on CP-style scan-on-update"
            );
        }

        // Lowercase `sevr=` enum strings are ignored → default NMS.
        for typo in ["ms", "msi", "mss", "nms"] {
            let c =
                PvaLinkConfig::parse(&format!("pva://X?sevr={typo}"), LinkDirection::Inp).unwrap();
            assert_eq!(
                c.sevr,
                SevrMode::Nms,
                "sevr={typo} (lowercase) must be ignored, keeping default NMS"
            );
        }

        // Lowercase legacy bare modifiers are likewise ignored.
        let c = PvaLinkConfig::parse("pva://X cp ms", LinkDirection::Inp).unwrap();
        assert_eq!(c.proc, ProcMode::Default, "legacy `cp` ignored");
        assert_eq!(c.sevr, SevrMode::Nms, "legacy `ms` ignored");

        // Uppercase forms still work (regression guard).
        let up = PvaLinkConfig::parse("pva://X?proc=CP&sevr=MSI", LinkDirection::Inp).unwrap();
        assert_eq!(up.proc, ProcMode::Cp);
        assert_eq!(up.sevr, SevrMode::Msi);
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

    /// pvxs parity: an unparseable option value is warned and ignored,
    /// the field keeps its default, and the link still parses (it is NOT
    /// rejected). Mirrors `pvalink_jlif.cpp:90-194` (`log_warn_printf` +
    /// `jlif_continue`).
    #[test]
    fn bad_option_value_ignored_keeps_default() {
        let c = PvaLinkConfig::parse("pva://X?Q=notanumber", LinkDirection::Inp).unwrap();
        assert_eq!(
            c.queue_size, DEFAULT_QUEUE_SIZE,
            "unparseable Q ignored → default queue size kept"
        );
        let c = PvaLinkConfig::parse("pva://X?sevr=BOGUS", LinkDirection::Inp).unwrap();
        assert_eq!(
            c.sevr,
            SevrMode::Nms,
            "unparseable sevr ignored → default NMS kept"
        );
    }

    /// The core of the fix: a single bad option must not discard the
    /// valid siblings on the same link. Pre-fix, `parse` returned `Err`
    /// on the bad option and the call site's `if let Ok(cfg)`
    /// (integration.rs) dropped the *entire* link, defaulting every
    /// option. pvxs parity — the bad option is warned+ignored and every
    /// valid option is still applied.
    #[test]
    fn bad_option_keeps_valid_siblings() {
        let c = PvaLinkConfig::parse(
            "pva://X?proc=BOGUS&field=alarm.severity&Q=8&monitor=true",
            LinkDirection::Inp,
        )
        .expect("a link with one bad option must still parse");
        assert_eq!(c.proc, ProcMode::Default, "bad proc ignored → default");
        assert_eq!(c.field, "alarm.severity", "valid field applied");
        assert_eq!(c.queue_size, 8, "valid Q applied");
        assert!(c.monitor, "valid monitor applied");
    }

    /// A malformed query segment (no `=`) is warned and skipped while the
    /// valid `key=value` siblings are still applied (pvxs warn+continue
    /// parity). Pre-fix the missing `=` returned `Err(BadOption)` and the
    /// whole link was discarded.
    #[test]
    fn malformed_query_segment_skipped_keeps_valid_siblings() {
        let c = PvaLinkConfig::parse("pva://X?garbage&field=value.index&Q=2", LinkDirection::Inp)
            .expect("a malformed query segment must not fail the whole link");
        assert_eq!(c.field, "value.index", "valid field after bad segment");
        assert_eq!(c.queue_size, 2, "valid Q after bad segment");
    }
}
