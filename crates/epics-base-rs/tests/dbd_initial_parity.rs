//! Guard: every record's Rust `Default` must agree with the upstream C dbd
//! `initial("…")` directive for that field.
//!
//! Background — the Rust port's `db_loader` never parses dbd `initial(...)`
//! values; each record hand-codes its struct `Default` instead. Nothing keeps
//! the two in sync, so any record whose `Default` drifts from the C dbd silently
//! diverges from a real IOC's load-time field values. This test closes that
//! family structurally: the *actual* C dbd files are the single source of truth,
//! and every modeled record's live `Default` is checked against them. A new
//! record, a changed C initial, or a drifted Rust default fails here.
//!
//! Mechanism: for each `initial("V")` in `<record>Record.dbd.pod`, construct the
//! record's default instance (`create_record`), read the field, then apply `V`
//! through the loader's own coercion (`apply_fields`, which resolves menu labels
//! and parses numerics exactly as a `.db` load would) to a second instance and
//! compare. Equal ⇒ the hand-coded default matches C; unequal ⇒ divergence.
//!
//! Source of truth: the C dbd at `$EPICS_BASE/modules/database/src/std/rec`
//! (env `EPICS_BASE`), falling back to the machine-local reference checkout. If
//! neither is present the test skips with a printed notice rather than failing,
//! so CI without the C source is not blocked.

use epics_base_rs::server::db_loader::{apply_fields, create_record};
use epics_base_rs::types::EpicsValue;
use std::path::PathBuf;

/// Base records the Rust port models that also have a C dbd. `aSub` is handled
/// separately (see `SKIP_RECORDS`). synApps records (busy, swait, scalcout,
/// sseq, transform, asyn) have no epics-base dbd and are absent here by design.
const BASE_RECORDS: &[&str] = &[
    "ai",
    "ao",
    "bi",
    "bo",
    "stringin",
    "stringout",
    "longin",
    "longout",
    "int64in",
    "int64out",
    "lsi",
    "lso",
    "mbbi",
    "mbbo",
    "mbbiDirect",
    "mbboDirect",
    "event",
    "printf",
    "waveform",
    "aai",
    "aao",
    "subArray",
    "calc",
    "fanout",
    "seq",
    "calcout",
    "dfanout",
    "compress",
    "histogram",
    "sel",
    "sub",
    "aSub",
];

/// `aSub` carries ~148 per-argument initials (FTx=DOUBLE, NEx=1, NOx=1, ONVx,
/// NEVx, NOVx, FTVx, EFLG) as fixed-size struct arrays (`[FTYPE_DOUBLE; N]`,
/// `[1; N]`). Those array elements are not individually present in
/// `field_list()`, so the loader's scalar field path cannot reach them and this
/// dbd-parse loop cannot verify them. They are covered by `asub_record.rs` unit
/// tests and their array `Default`. Skipped here, logged below — not silently
/// dropped.
const SKIP_RECORDS: &[&str] = &["aSub"];

/// Common fields (shared `CommonFields`, not in any record's `field_list`) that
/// legitimately carry a dbd initial. Verified outside this test:
///   - `SSCN`: `CommonFields.sscn` (=65535, `SimModeScan::DoNotUse`) — covered by
///     the SSCN serve/round-trip tests and `test_new_common_fields_get_put`.
///   - `SDLY`: scan-delay (SPC_NOMOD), unmodeled in the port.
const KNOWN_NON_FIELDLIST: &[&str] = &["SSCN", "SDLY"];

/// `(record, field)` whose Rust `Default` deliberately differs from the C dbd
/// `initial(...)`. Returns the documented reason, or `None` if not a deviation.
fn deviation_reason(record: &str, field: &str) -> Option<&'static str> {
    match (record, field) {
        ("lsi" | "lso" | "printf", "SIZV") => Some(
            "Rust default string capacity 256 vs C dbd 41 — intentional larger default \
             (lsi/lso/printf hold a longer string than C's initial size hint).",
        ),
        ("calcout", f) if is_calcout_in_status(f) => Some(
            "INAV..INUV are SPC_NOMOD link-status fields; C init_record recomputes them per \
             link type (CON for an unset/constant link). The Rust default models the post-init \
             value CON(3); C's static initial 1 (EXT) is overwritten at init and never observed.",
        ),
        _ => None,
    }
}

/// `INAV`..`INUV` (single middle letter A..U): calcout per-input link-status.
fn is_calcout_in_status(f: &str) -> bool {
    let b = f.as_bytes();
    f.len() == 4 && f.starts_with("IN") && f.ends_with('V') && (b'A'..=b'U').contains(&b[2])
}

/// Extract `(FIELD, initial_value)` for every `field(NAME,DBF_*) { … initial("V") … }`
/// in a `.dbd.pod`, keeping only non-trivial initials (anything other than the
/// type-zero `0`/`0.0`/empty — those cannot diverge from a zeroed Rust default).
fn parse_dbd_initials(dbd: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = dbd;
    while let Some(pos) = rest.find("field(") {
        rest = &rest[pos + "field(".len()..];
        let Some(close) = rest.find(')') else { break };
        let header = &rest[..close];
        let mut parts = header.split(',');
        let name = parts.next().unwrap_or("").trim().to_string();
        let dbf = parts.next().unwrap_or("").trim();
        let after = &rest[close + 1..];
        // Only real `field(NAME,DBF_*)` declarations; skip POD prose that happens
        // to contain "field(".
        if !dbf.starts_with("DBF") {
            continue;
        }
        let Some(brace) = after.find('{') else {
            continue;
        };
        let body = &after[brace + 1..];
        // Field bodies contain no nested braces, so the first `}` closes the field.
        let Some(end) = body.find('}') else {
            continue;
        };
        let body = &body[..end];
        if let Some(v) = extract_initial(body).filter(|v| !is_trivial_initial(v)) {
            out.push((name, v));
        }
        rest = &after[brace + 1 + end..];
    }
    out
}

fn extract_initial(body: &str) -> Option<String> {
    let i = body.find("initial(\"")?;
    let s = &body[i + "initial(\"".len()..];
    let j = s.find('"')?;
    Some(s[..j].to_string())
}

/// A type-zero initial (`0`/`0.0`/empty) cannot diverge from a zeroed Rust
/// default, so it is not worth verifying.
fn is_trivial_initial(v: &str) -> bool {
    v.is_empty() || v == "0" || v == "0.0" || v == "0.00"
}

enum ValueCmp {
    Match,
    Differ,
    /// Cannot compare automatically (non-numeric initial on a field the loader
    /// path can't reach) — surface for manual triage rather than pass silently.
    Untestable,
}

/// Compare a record's default field value against a C dbd initial string when the
/// loader's apply path is unavailable (field absent from `field_list`, or
/// `put_field` read-only). Numeric initials compare through `to_f64`, which
/// covers every integer/menu/float variant.
fn compare_default_to_initial(default: &EpicsValue, initial: &str) -> ValueCmp {
    if let Ok(want) = initial.parse::<f64>() {
        match default.to_f64() {
            Some(got) if got == want => ValueCmp::Match,
            Some(_) => ValueCmp::Differ,
            None => ValueCmp::Untestable,
        }
    } else {
        ValueCmp::Untestable
    }
}

/// Locate the C dbd record directory, or `None` to skip the test.
fn dbd_dir() -> Option<PathBuf> {
    if let Ok(base) = std::env::var("EPICS_BASE") {
        let p = PathBuf::from(base).join("modules/database/src/std/rec");
        if p.is_dir() {
            return Some(p);
        }
    }
    let reference = PathBuf::from("/Users/stevek/codes/epics-base/modules/database/src/std/rec");
    reference.is_dir().then_some(reference)
}

#[test]
fn rust_record_defaults_match_c_dbd_initials() {
    let Some(dir) = dbd_dir() else {
        eprintln!(
            "SKIP rust_record_defaults_match_c_dbd_initials: C dbd not found \
             (set EPICS_BASE or place the reference checkout). No defaults verified."
        );
        return;
    };

    let mut checked = 0usize;
    let mut deviations: Vec<String> = Vec::new();
    let mut skipped_common: Vec<String> = Vec::new();
    let mut skipped_records: Vec<String> = Vec::new();
    // Fields whose default was verified by direct value comparison because the
    // loader's apply path could not reach them (not in field_list, or read-only
    // put_field). Logged so the loadability quirk stays visible.
    let mut value_fallback: Vec<String> = Vec::new();
    let mut mismatches: Vec<String> = Vec::new();

    for &record in BASE_RECORDS {
        if SKIP_RECORDS.contains(&record) {
            skipped_records.push(record.to_string());
            continue;
        }

        let path = dir.join(format!("{record}Record.dbd.pod"));
        let Ok(dbd) = std::fs::read_to_string(&path) else {
            mismatches.push(format!("{record}: dbd file missing at {}", path.display()));
            continue;
        };

        for (field, initial) in parse_dbd_initials(&dbd) {
            if let Some(reason) = deviation_reason(record, &field) {
                deviations.push(format!(
                    "{record}.{field} (C initial {initial:?}): {reason}"
                ));
                continue;
            }

            // Common fields (shared CommonFields, not on the record) verified
            // elsewhere — see KNOWN_NON_FIELDLIST.
            if KNOWN_NON_FIELDLIST.contains(&field.as_str()) {
                skipped_common.push(format!("{record}.{field} (C initial {initial:?})"));
                continue;
            }

            let pristine = create_record(record).expect("create_record");
            let default_value = pristine.get_field(&field);
            let in_field_list = pristine.field_list().iter().any(|f| f.name == field);

            // Preferred path: apply the C initial through the loader's own
            // coercion (menu-label + numeric parsing) to a fresh instance and
            // compare the read-back to the default. Exercises the real load path.
            if in_field_list {
                let mut applied = create_record(record).expect("create_record");
                let mut common = Vec::new();
                // On read-only put_field this is Err — fall through to the value
                // comparison below.
                if apply_fields(
                    &mut applied,
                    &[(field.clone(), initial.clone())],
                    &mut common,
                )
                .is_ok()
                {
                    checked += 1;
                    let dbd_value = applied.get_field(&field);
                    if default_value != dbd_value {
                        mismatches.push(format!(
                            "{record}.{field}: Rust default {default_value:?} != C dbd \
                             initial {initial:?} (coerces to {dbd_value:?})"
                        ));
                    }
                    continue;
                }
            }

            // Fallback: the loader cannot set this field (absent from field_list,
            // or read-only put_field), so compare the default value directly.
            let Some(dv) = &default_value else {
                mismatches.push(format!(
                    "{record}.{field}: C dbd has initial {initial:?} but the field is neither \
                     modeled (get_field None) nor a known common field — triage needed"
                ));
                continue;
            };
            value_fallback.push(format!("{record}.{field}"));
            match compare_default_to_initial(dv, &initial) {
                ValueCmp::Match => checked += 1,
                ValueCmp::Differ => mismatches.push(format!(
                    "{record}.{field}: Rust default {dv:?} != C dbd initial {initial:?}"
                )),
                ValueCmp::Untestable => mismatches.push(format!(
                    "{record}.{field}: C dbd initial {initial:?} not auto-comparable to default \
                     {dv:?} (non-numeric initial on a field the loader can't set) — verify manually"
                )),
            }
        }
    }

    eprintln!(
        "dbd-initial parity: {checked} field defaults verified against C; \
         {} documented deviations; {} common/unmodeled fields skipped; \
         {} verified by value (loader can't set them: {value_fallback:?}); \
         records skipped (array-backed, see SKIP_RECORDS): {skipped_records:?}",
        deviations.len(),
        skipped_common.len(),
        value_fallback.len(),
    );
    for d in &deviations {
        eprintln!("  deviation: {d}");
    }

    assert!(
        mismatches.is_empty(),
        "Rust record defaults diverge from C dbd initials:\n  {}",
        mismatches.join("\n  ")
    );
}
