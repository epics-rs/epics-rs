//! Guard: every `device()` row in the std module's `stdSupport.dbd` must be
//! accounted for by the Rust port — served by a registered device-support
//! factory (`std_device_supports()`), handled by the record body itself,
//! app-parameterized, or an explicitly tracked known gap.
//!
//! Background — the Rust port hand-registers device support (which DTYP maps to
//! which `DeviceSupport`), and nothing keeps that registry in sync with the C
//! dbd `device(recordType, linkType, dset, "DTYP")` rows. A new std device row,
//! or a dropped registration, silently means a record's device never runs and
//! VAL is wrong — the exact `is_soft_dtyp` / "Sec Past Epoch" family this port
//! just closed. This test closes that family structurally for the std module:
//! the C `stdSupport.dbd` is the single source of truth, and every device row
//! is classified against the live Rust coverage.
//!
//! Pair-keyed by design: EPICS selects device support by the (recordType, DTYP)
//! PAIR, and `epid` deliberately REUSES the base soft-channel DTYP strings
//! ("Soft Channel" / "Async Soft Channel") for record-type-specific PID
//! behavior. A DTYP-string-only guard would false-pass those rows (they look
//! like generic soft channels) and hide the epid async-callback gap. So the
//! allowlist is keyed on the (recordType, DTYP) pair, each entry with a reason.
//!
//! Source of truth: `stdSupport.dbd` under `$EPICS_MODULES/std/stdApp/src`
//! (env `EPICS_MODULES`), falling back to the machine-local reference checkout.
//! If absent the test skips with a printed notice, so CI without the synApps
//! source is not blocked.

use epics_base_rs::reference::{ReferenceTree, reference_path};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// (recordType, DTYP) device rows the Rust port covers WITHOUT a registered
/// device-support factory, each carrying the reason it is nonetheless correct
/// (or a tracked gap). Keyed on the PAIR because `epid` reuses base
/// soft-channel DTYP strings for its own behavior.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    (
        "epid",
        "Soft Channel",
        "record-internal: is_soft_dtyp short-circuits device attach, so epid \
         process() runs epid_soft::do_pid directly (epid.rs). The \
         EpidSoftDeviceSupport DeviceSupport impl is vestigial for this row.",
    ),
    (
        "epid",
        "Async Soft Channel",
        "record-internal: is_soft_dtyp short-circuits the device, so the sync \
         do_pid runs from the record body. The TRIG readback (both DB and CA \
         link types) is fired by EpidRecord::pre_input_link_actions, gated on \
         EpidRecord::is_async_callback_dtyp, which matches THIS dbd string. \
         Closed record-internally, not by registration.",
    ),
    (
        "epid",
        "Fast Epid",
        "app-parameterized: EpidFastDeviceSupport::new() builds an un-wired \
         device; the asyn Float64 output port must be installed per-record via \
         set_output_port(sync_io, reason) (an asyn handle, app-specific), like \
         the Asyn Scaler / motor device support. Not a std_device_supports() \
         drop-in.",
    ),
];

/// Locate `stdSupport.dbd`. Fails the test if the reference tree is absent —
/// see `epics_base_rs::reference` for why this must not skip.
fn dbd_path() -> PathBuf {
    reference_path(ReferenceTree::Modules, "std/stdApp/src/stdSupport.dbd")
}

/// Parse `device(recordType, linkType, dset, "DTYP")` rows → (recordType, DTYP).
/// Spacing varies in the dbd (`device(epid,CONSTANT,...)` vs
/// `device(stringin, CONSTANT, ...)`), so split on `,` and trim each field.
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

#[test]
fn stdsupport_device_rows_are_all_accounted_for() {
    let path = dbd_path();
    let dbd = std::fs::read_to_string(&path).expect("read stdSupport.dbd");
    let rows = parse_device_rows(&dbd);
    assert!(
        !rows.is_empty(),
        "parsed zero device() rows from {} — parser or file format changed",
        path.display()
    );

    // Live Rust registry: the DTYPs served by std_device_supports().
    let registered: BTreeSet<String> = std_rs::std_device_supports()
        .into_iter()
        .map(|(dtyp, _)| dtyp.to_string())
        .collect();

    let mut failures: Vec<String> = Vec::new();
    let mut tracked: Vec<String> = Vec::new();
    // DTYPs whose dbd row required (and found) a registered factory — used to
    // catch phantom registrations (a factory with no dbd basis).
    let mut covered_registered: BTreeSet<String> = BTreeSet::new();

    for (record_type, dtyp) in &rows {
        if let Some((_, _, reason)) = ALLOWLIST
            .iter()
            .find(|(rt, dt, _)| *rt == record_type.as_str() && *dt == dtyp.as_str())
        {
            tracked.push(format!("({record_type}, {dtyp:?}) — {reason}"));
        } else if registered.contains(dtyp) {
            covered_registered.insert(dtyp.clone());
        } else {
            failures.push(format!(
                "({record_type}, {dtyp:?}) is neither registered in \
                 std_device_supports() nor on the (recordType, DTYP) allowlist \
                 — a std device row with no Rust coverage (the record's device \
                 never runs; VAL silently wrong). Register a factory or \
                 allowlist it with a reason."
            ));
        }
    }

    // No phantom registration: every std_device_supports() key must have a dbd
    // basis (a non-allowlisted device row of that DTYP).
    for dtyp in &registered {
        if !covered_registered.contains(dtyp) {
            failures.push(format!(
                "std_device_supports() registers {dtyp:?} but no stdSupport.dbd \
                 device() row uses it — phantom registration or a renamed DTYP."
            ));
        }
    }

    // Surface the tracked (record-internal / app-param / known-gap) rows so the
    // epid async-callback gap and the Fast Epid app-wiring stay visible.
    if !tracked.is_empty() {
        eprintln!(
            "stdsupport_device_rows_are_all_accounted_for: {} device row(s) \
             covered outside std_device_supports() (tracked, with reasons):",
            tracked.len()
        );
        for t in &tracked {
            eprintln!("  - {t}");
        }
    }

    assert!(
        failures.is_empty(),
        "stdSupport.dbd device rows unaccounted for:\n{}",
        failures.join("\n")
    );
}
