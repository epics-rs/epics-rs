//! Guard: every `device()` row in EPICS base's `devSoft.dbd` must be accounted
//! for by the Rust port — served by one of base-rs's device-support mechanisms,
//! or an explicitly tracked known gap.
//!
//! Background — base-rs maps a record's DTYP to its device support through
//! THREE mechanisms, and nothing keeps them in sync with the C
//! `device(recordType, linkType, dset, "DTYP")` rows. A new base device row, or
//! a dropped/renamed registration, silently means a record's device never runs
//! and VAL is wrong — the exact `is_soft_dtyp` / stdio-completeness family this
//! port just closed (stdio, then Db State). This test closes that family
//! structurally for base: `devSoft.dbd` is the single source of truth, and
//! every device row is classified against the live Rust coverage.
//!
//! base's coverage is split across three mechanisms, not one registry (unlike
//! the std module's `std_device_supports()` guard), so the "served" set is the
//! union of all three — checked by calling the real functions, not a hardcoded
//! list, so the guard self-updates as builtins are added:
//!   1. `is_soft_dtyp(dtyp)` — the soft-channel short-circuit (Soft Channel /
//!      Raw Soft Channel / Async Soft Channel never attach a device).
//!   2. `builtin_dynamic_factory(ctx)` — the dynamic builtins that need the
//!      record's INP/OUT (Soft Timestamp, stdio, Db State).
//!   3. The static auto-registered builtins (`getenv`), which both
//!      `IocBuilder::new` and `IocApplication::new` register via `.n(dtyp, …)`
//!      and which neither (1) nor (2) reports — the one small explicit set.
//!
//! Pair-keyed allowlist, by design and for consistency with the std guard:
//! EPICS selects device support by the (recordType, DTYP) PAIR. base's
//! `devSoft.dbd` does not reuse a DTYP string across record types for differing
//! behavior the way `epid` does, so a DTYP-level entry would suffice — but the
//! per-record reasons genuinely differ (General Time's ai `"TIME"` is a clean
//! system-clock read; its bo/longin/stringin need the unmodeled error-counter /
//! provider-registry subsystem), so the pair shape carries more information.
//!
//! Source of truth: `devSoft.dbd` under `$EPICS_BASE/modules/database/src/std/
//! dev` (env `EPICS_BASE`), falling back to the machine-local reference
//! checkout. If absent the test skips with a printed notice, so CI without the
//! base C source is not blocked.

use std::collections::BTreeSet;
use std::path::PathBuf;

use epics_base_rs::server::builtin_devices::builtin_dynamic_factory;
use epics_base_rs::server::device_support::is_soft_dtyp;
use epics_base_rs::server::ioc_app::DeviceSupportContext;

/// Statically auto-registered builtins that `is_soft_dtyp` and
/// `builtin_dynamic_factory` do not report. `getenv` (base `devSiEnviron` /
/// `devLsiEnviron` for stringin/lsi) is registered by `IocBuilder::new` /
/// `IocApplication::new` via `.n("getenv", …)` — a `Fn() -> Box<dyn
/// DeviceSupport>` that needs no INP/OUT, so it is not in the dynamic factory.
/// The only static builtin; listed so a renamed/removed registration surfaces.
const STATIC_BUILTINS: &[&str] = &["getenv"];

/// (recordType, DTYP) device rows base-rs does NOT serve, each a tracked known
/// gap with the reason it is acceptable to leave unported (for now). Keyed on
/// the PAIR for consistency with the std guard and because the per-record
/// reasons differ.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "ai",
        "General Time",
        "KNOWN GAP (General Time, clean channel): devAiGeneralTime read_ai = \
         epicsTimeGetCurrent → VAL = secPastEpoch + nsec*1e-9, return 2 — a \
         plain system-clock read with no subsystem. Portable at effort S, but \
         redundant with the existing 'Soft Timestamp' ai / 'Sec Past Epoch' \
         current-time support; deferred to a concrete consumer.",
    ),
    (
        "bo",
        "General Time",
        "KNOWN GAP (General Time): devBoGeneralTime write_bo channel RSTERRCNT \
         = generalTimeResetErrorCounts(). Needs the epicsGeneralTime \
         error-counter subsystem, which base-rs does not model (single \
         system-clock source, no time-provider chain). Stubbing to a no-op \
         would be fake parity; deferred until the subsystem is modeled.",
    ),
    (
        "longin",
        "General Time",
        "KNOWN GAP (General Time): devLiGeneralTime read_li channel GETERRCNT \
         = VAL = generalTimeGetErrorCounts(). Needs the same epicsGeneralTime \
         error-counter subsystem; stubbing to a constant 0 would be fake \
         parity. Deferred.",
    ),
    (
        "stringin",
        "General Time",
        "KNOWN GAP (General Time): devSiGeneralTime read_si channels \
         BESTTCP/TOPTCP/BESTTEP = provider names from \
         generalTimeCurrentProviderName / generalTimeHighestCurrentName / \
         generalTimeEventProviderName. Needs the epicsGeneralTime PROVIDER \
         REGISTRY (registered providers with priorities + an event-time \
         provider), which base-rs does not model. Stubbing a fixed name would \
         be fake parity. Deferred.",
    ),
];

/// Locate `devSoft.dbd`, or `None` to skip the test.
fn dbd_path() -> Option<PathBuf> {
    if let Ok(base) = std::env::var("EPICS_BASE") {
        let p = PathBuf::from(base).join("modules/database/src/std/dev/devSoft.dbd");
        if p.is_file() {
            return Some(p);
        }
    }
    let reference =
        PathBuf::from("/Users/stevek/codes/epics-base/modules/database/src/std/dev/devSoft.dbd");
    reference.is_file().then_some(reference)
}

/// Parse `device(recordType, linkType, dset, "DTYP")` rows → (recordType, DTYP).
/// Spacing varies in the dbd (`device(bi, INST_IO, …)` vs
/// `device(ai,      INST_IO,…)`), so split on `,` and trim each field.
fn parse_device_rows(dbd: &str) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for line in dbd.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("device(") else {
            continue;
        };
        let Some(inner) = rest.strip_suffix(')') else {
            continue;
        };
        let fields: Vec<&str> = inner.split(',').map(str::trim).collect();
        if fields.len() != 4 {
            continue;
        }
        let record_type = fields[0].to_string();
        let dtyp = fields[3].trim_matches('"').to_string();
        rows.push((record_type, dtyp));
    }
    rows
}

/// Is `dtyp` served by one of base-rs's three device-support mechanisms?
fn served_by_base(dtyp: &str) -> bool {
    if is_soft_dtyp(dtyp) || STATIC_BUILTINS.contains(&dtyp) {
        return true;
    }
    // The dynamic builtins need INP/OUT; probing with empty strings only
    // exercises the DTYP-match arm (the factory returns Some on a recognized
    // DTYP regardless of INP/OUT content).
    let ctx = DeviceSupportContext {
        dtyp,
        inp: "",
        out: "",
    };
    builtin_dynamic_factory(&ctx).is_some()
}

#[test]
fn base_devsoft_device_rows_are_all_accounted_for() {
    let Some(path) = dbd_path() else {
        eprintln!(
            "SKIP base_devsoft_device_rows_are_all_accounted_for: devSoft.dbd \
             not found (set EPICS_BASE or place the reference checkout). No \
             device rows verified."
        );
        return;
    };
    let dbd = std::fs::read_to_string(&path).expect("read devSoft.dbd");
    let rows = parse_device_rows(&dbd);
    assert!(
        !rows.is_empty(),
        "parsed zero device() rows from {} — parser or file format changed",
        path.display()
    );

    let mut failures: Vec<String> = Vec::new();
    let mut tracked: Vec<String> = Vec::new();
    // Allowlist entries actually matched by a row — to catch a stale allowlist
    // entry that no longer corresponds to any devSoft.dbd row.
    let mut allowlist_hit: BTreeSet<(String, String)> = BTreeSet::new();

    for (record_type, dtyp) in &rows {
        if served_by_base(dtyp) {
            continue;
        }
        if let Some((_, _, reason)) = ALLOWLIST
            .iter()
            .find(|(rt, dt, _)| *rt == record_type.as_str() && *dt == dtyp.as_str())
        {
            allowlist_hit.insert((record_type.clone(), dtyp.clone()));
            tracked.push(format!("({record_type}, {dtyp:?}) — {reason}"));
        } else {
            failures.push(format!(
                "({record_type}, {dtyp:?}) is served by none of base-rs's \
                 device-support mechanisms (is_soft_dtyp / builtin_dynamic_\
                 factory / static builtins) and is not on the (recordType, \
                 DTYP) allowlist — a base device row with no Rust coverage (the \
                 record's device never runs; VAL silently wrong). Add the \
                 device support or allowlist it with a reason."
            ));
        }
    }

    // No stale allowlist entry: every ALLOWLIST pair must match a real row.
    for (rt, dtyp, _) in ALLOWLIST {
        if !allowlist_hit.contains(&(rt.to_string(), dtyp.to_string())) {
            failures.push(format!(
                "allowlist entry ({rt}, {dtyp:?}) matches no devSoft.dbd row — \
                 stale entry; the gap was closed or the row was renamed/removed."
            ));
        }
    }

    // No stale static builtin: every STATIC_BUILTINS entry must back a real row
    // (a renamed/removed getenv DTYP would otherwise pass silently).
    let dtyps: BTreeSet<&str> = rows.iter().map(|(_, d)| d.as_str()).collect();
    for b in STATIC_BUILTINS {
        if !dtyps.contains(b) {
            failures.push(format!(
                "static builtin {b:?} backs no devSoft.dbd row — phantom \
                 registration or a renamed DTYP."
            ));
        }
    }

    // Surface the tracked known gaps so General Time stays visible.
    if !tracked.is_empty() {
        eprintln!(
            "base_devsoft_device_rows_are_all_accounted_for: {} device row(s) \
             not yet served by base-rs (tracked known gaps, with reasons):",
            tracked.len()
        );
        for t in &tracked {
            eprintln!("  - {t}");
        }
    }

    assert!(
        failures.is_empty(),
        "devSoft.dbd device rows unaccounted for:\n{}",
        failures.join("\n")
    );
}
