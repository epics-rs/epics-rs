//! Guards: every `device()` row in EPICS base's `devSoft.dbd` must be accounted
//! for by the Rust port on TWO axes — REGISTRATION (the DTYP is served by a
//! device-support mechanism, or a tracked known gap) and ROUTING (a served
//! record reaches its device in the correct I/O direction). `devSoft.dbd` is
//! the single source of truth for both.
//!
//! Background — base-rs maps a record's DTYP to its device support through
//! TWO mechanisms, and nothing keeps them in sync with the C
//! `device(recordType, linkType, dset, "DTYP")` rows. Two distinct failure
//! families result, each closed by one test here:
//!   - `base_devsoft_device_rows_are_all_accounted_for` (REGISTRATION): a new
//!     base device row, or a dropped/renamed registration, silently means a
//!     record's DTYP resolves to no device — VAL stays at its default.
//!   - `base_devsoft_device_routing_matches_c_dset_direction` (ROUTING): even a
//!     registered device is silently dropped if it is routed the wrong way. The
//!     processing loop derives `is_output = can_device_write()`, so an output
//!     record absent from `can_device_write()` (the stdio printf bug) is treated
//!     as an input — its device write never runs. The registration test is
//!     blind to this (printf's DTYP still resolved); the routing test catches it
//!     by asserting `can_device_write()` matches the C dset's read/write
//!     direction for every served, device-attaching row.
//!
//! base's coverage is split across two mechanisms, not one registry (unlike
//! the std module's `std_device_supports()` guard), so the "served" set is the
//! union of both — each computed by calling the real function base-rs itself
//! uses, never a guard-local hardcoded copy, so the guard self-updates as
//! builtins are added and goes RED if one is dropped:
//!   1. `is_soft_dtyp(dtyp)` — the soft-channel short-circuit (Soft Channel /
//!      Raw Soft Channel / Async Soft Channel never attach a device).
//!   2. `builtin_dynamic_factory(ctx)` — the dynamic builtins, every one of
//!      which needs the record's INST_IO `INP`/`OUT` (Soft Timestamp, stdio, Db
//!      State, getenv). The inner typed record handed to a device's `init` /
//!      `read` does NOT expose `INP`/`OUT` (those live on the `RecordInstance`
//!      common header), so there is no context-free static builtin: every base
//!      builtin is dispatched through this factory.
//!
//! Residual: there is no phantom-DYNAMIC check — a `builtin_dynamic_factory`
//! arm for a DTYP with no devSoft.dbd row passes silently, because the match
//! arms are not enumerable. The allowlist side IS phantom-checked.
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

use epics_base_rs::reference::{ReferenceTree, reference_path};
use epics_base_rs::server::builtin_devices::builtin_dynamic_factory;
use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::device_support::is_soft_dtyp;
use epics_base_rs::server::ioc_app::DeviceSupportContext;

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

/// Locate `devSoft.dbd`. Fails the test if the reference tree is absent —
/// see [`epics_base_rs::reference`] for why this must not skip.
fn dbd_path() -> PathBuf {
    reference_path(
        ReferenceTree::Base,
        "modules/database/src/std/dev/devSoft.dbd",
    )
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

/// Is `dtyp` served by one of base-rs's two device-support mechanisms?
fn served_by_base(dtyp: &str) -> bool {
    if is_soft_dtyp(dtyp) {
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
    let path = dbd_path();
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
                "({record_type}, {dtyp:?}) is served by neither of base-rs's \
                 device-support mechanisms (is_soft_dtyp / builtin_dynamic_\
                 factory) and is not on the (recordType, DTYP) allowlist — a \
                 base device row with no Rust coverage (the record's device \
                 never runs; VAL silently wrong). Add the device support or \
                 allowlist it with a reason."
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

/// C dset I/O direction for every served, device-attaching (non-soft,
/// non-allowlisted) devSoft.dbd row: `true` = the C dset writes (the record's
/// device support IS its output, a `write_xxx` entry), `false` = it reads (a
/// `read_xxx` entry). This is C REFERENCE data — it cannot be probed from Rust
/// (matching it is the whole point) — so each entry cites its C dset.
///
/// The processing loop sets `is_output = record.can_device_write()`
/// (processing.rs), so the routing is correct iff `can_device_write()` equals
/// the C direction: an output dset whose record is missing from
/// `can_device_write()` is misrouted to the no-op read path (the printf bug),
/// and an input dset whose record is wrongly in it would skip its `dev.read`.
const DEVICE_IO_DIRECTION: &[(&str, &str, bool)] = &[
    ("ai", "Soft Timestamp", false),       // devTimestampAI read_ai
    ("stringin", "Soft Timestamp", false), // devTimestampSI read_stringin
    ("lso", "stdio", true),                // devLsoStdio write_string
    ("printf", "stdio", true),             // devPrintfStdio write_string
    ("stringout", "stdio", true),          // devSoStdio write_string
    ("lsi", "getenv", false),              // devLsiEnviron read
    ("stringin", "getenv", false),         // devSiEnviron read
    ("bi", "Db State", false),             // devBiDbState read_bi
    ("bo", "Db State", true),              // devBoDbState write_bo
];

#[test]
fn base_devsoft_device_routing_matches_c_dset_direction() {
    let path = dbd_path();
    let dbd = std::fs::read_to_string(&path).expect("read devSoft.dbd");
    let rows = parse_device_rows(&dbd);
    assert!(
        !rows.is_empty(),
        "parsed zero device() rows from {}",
        path.display()
    );

    let mut failures: Vec<String> = Vec::new();
    // Direction entries actually matched by a served row — to catch a stale
    // entry that no longer corresponds to any served device-attaching row.
    let mut direction_hit: BTreeSet<(String, String)> = BTreeSet::new();

    for (record_type, dtyp) in &rows {
        // Soft channels short-circuit the device (the soft-link path, not
        // dev.write/dev.read), and allowlisted rows are unserved — neither has a
        // device direction to route.
        if is_soft_dtyp(dtyp)
            || ALLOWLIST
                .iter()
                .any(|(rt, dt, _)| *rt == record_type.as_str() && *dt == dtyp.as_str())
        {
            continue;
        }
        // A served, device-attaching row. It MUST be classified, or its routing
        // cannot be checked (the printf-class blind spot).
        let Some((_, _, expected_output)) = DEVICE_IO_DIRECTION
            .iter()
            .find(|(rt, dt, _)| *rt == record_type.as_str() && *dt == dtyp.as_str())
        else {
            assert!(
                served_by_base(dtyp),
                "({record_type}, {dtyp:?}) is neither soft, allowlisted, nor \
                 served — the registration guard should have caught this first"
            );
            failures.push(format!(
                "({record_type}, {dtyp:?}) is a served device-attaching row with \
                 no DEVICE_IO_DIRECTION entry — classify its C dset as read \
                 (false) or write (true) so its routing can be checked."
            ));
            continue;
        };
        direction_hit.insert((record_type.clone(), dtyp.clone()));

        let record = match create_record(record_type) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!(
                    "({record_type}, {dtyp:?}): create_record failed ({e}) — \
                     cannot verify routing for a record type devSoft.dbd binds."
                ));
                continue;
            }
        };
        let actual_output = record.can_device_write();
        if actual_output != *expected_output {
            let (c_dir, mis) = if *expected_output {
                (
                    "write (output)",
                    "absent from can_device_write() → routed to \
                 the no-op read path; its device write never runs (the printf \
                 bug)",
                )
            } else {
                (
                    "read (input)",
                    "present in can_device_write() → treated as an \
                 output; its dev.read never runs",
                )
            };
            failures.push(format!(
                "({record_type}, {dtyp:?}) routing mismatch: C dset is {c_dir} \
                 but can_device_write()={actual_output} — {mis}."
            ));
        }
    }

    // No stale direction entry: every DEVICE_IO_DIRECTION pair must match a
    // served row (else the classification drifted from devSoft.dbd).
    for (rt, dtyp, _) in DEVICE_IO_DIRECTION {
        if !direction_hit.contains(&(rt.to_string(), dtyp.to_string())) {
            failures.push(format!(
                "DEVICE_IO_DIRECTION entry ({rt}, {dtyp:?}) matches no served \
                 devSoft.dbd row — stale entry; the row was renamed/removed or \
                 became soft/allowlisted."
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "devSoft.dbd device routing mismatches:\n{}",
        failures.join("\n")
    );
}
