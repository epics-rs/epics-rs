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

use epics_base_rs::reference::{ReferenceTree, reference_path};
use epics_base_rs::server::db_loader::{apply_fields, create_record};
use epics_base_rs::server::record::FieldDeclaration;
use epics_base_rs::types::EpicsValue;
use std::path::PathBuf;

/// Base records the Rust port models that also have a C dbd. synApps records
/// (busy, swait, scalcout, sseq, transform, asyn) have no epics-base dbd and are
/// absent here by design.
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
    "permissive",
    "state",
];

/// Common fields (shared `CommonFields`, not in any record's `field_list`) that
/// legitimately carry a dbd initial. Verified outside this test:
///   - `SSCN`: `CommonFields.sscn` (=65535, `SimModeScan::DoNotUse`) — covered by
///     the SSCN serve/round-trip tests and `test_new_common_fields_get_put`.
///
/// `SDLY` ("Sim. Mode Async Delay") is no longer here: the port now models it as
/// a per-record `field_list` entry on the 15 record types that carry the SIMM
/// simulation-field group, so its `-1.0` default is verified by the regular
/// default-check path. Record types that do not model that group route SDLY
/// through `unmodeled_feature_reason` instead.
const KNOWN_NON_FIELDLIST: &[&str] = &["SSCN"];

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

/// A record field the port deliberately does not model: it is absent from
/// `field_list()`, so a `.db` assignment lands in `common_fields` and has no
/// effect. This is a *feature* gap, not a default divergence — the field has no
/// modeled storage to diverge — so the dbd-initial check records it separately
/// rather than flagging a mismatch. Returns the documented reason, or `None`.
fn unmodeled_feature_reason(record: &str, field: &str) -> Option<&'static str> {
    match record {
        // aSub models the subroutine I/O arguments (`FTx`/`FTVx` element types,
        // `NOx`/`NOVx` max counts, `NEx`/`NEVx` used counts, `INPx`/`OUTx`
        // links, `A..U`/`VALx` values, plus `EFLG`/`PREC`/`LFLG`/`SUBL`/`ONAM`),
        // all present in `field_list()`. It does not model the output-monitoring
        // bookkeeping below.
        "aSub" if is_asub_onv(field) => Some(
            "ONVx (Num. elements in OVLx) is SPC_NOMOD bookkeeping of the previous output-array \
             length used to detect an output-size change; the port models neither the old \
             output buffers OVLx nor their length, so ONVx carries no state.",
        ),
        // SDLY ("Sim. Mode Async Delay") is part of the SIMM simulation-field
        // group. The port models that group — and SDLY's async-defer behavior —
        // on the 15 record types that declare SIML/SIMM/SIOL/SIMS in
        // `field_list()`. These record types do not model the SIMM group at
        // all, so SDLY carries no state here (a documented feature gap, the same
        // status the rest of the group has on these records).
        "longin" | "int64in" | "event" | "waveform" | "aai" | "aao" if field == "SDLY" => Some(
            "SDLY is part of the SIMM simulation-field group, which the port does not model on \
             this record type (no SIML/SIMM/SIOL/SIMS in field_list); SDLY carries no state.",
        ),
        _ => None,
    }
}

/// `ONVA`..`ONVU` (single trailing letter A..U): aSub old-output-element counts.
fn is_asub_onv(f: &str) -> bool {
    f.len() == 4 && f.starts_with("ONV") && (b'A'..=b'U').contains(&f.as_bytes()[3])
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

/// Only an EMPTY initial is trivial — it is the absence of a directive.
///
/// `"0"` is NOT: on a `DBF_STRING` field it is the one-character string `"0"`,
/// which a zeroed Rust default (the empty string) does not equal. Skipping it as
/// "type-zero" is what let `calc`/`calcout`/`swait` ship with an EMPTY `CALC`
/// where C's `.dbd` says `initial("0")` — an empty expression base's `postfix()`
/// refuses, so a plain `record(calc,"X") {}` alarmed CALC/INVALID on its first
/// process (R19-122). The rule now looks at the value, not at its resemblance to
/// a zero.
fn is_trivial_initial(v: &str) -> bool {
    v.is_empty()
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

/// Locate the C dbd record directory. A previous round wrote the lesson here
/// — "a guard that skips verifies nothing while still reporting green" — and
/// then fixed only the path list, leaving the skip in place, so the guard was
/// still vacuous on any machine without the checkout. The resolver now fails
/// instead; see [`epics_base_rs::reference`].
fn dbd_dir() -> PathBuf {
    reference_path(ReferenceTree::Base, "modules/database/src/std/rec")
}

#[test]
fn rust_record_defaults_match_c_dbd_initials() {
    let dir = dbd_dir();

    let mut checked = 0usize;
    let mut deviations: Vec<String> = Vec::new();
    let mut skipped_common: Vec<String> = Vec::new();
    let mut skipped_unmodeled: Vec<String> = Vec::new();
    // Fields whose default was verified by direct value comparison because the
    // loader's apply path could not reach them (not in field_list, or read-only
    // put_field). Logged so the loadability quirk stays visible.
    let mut value_fallback: Vec<String> = Vec::new();
    let mut mismatches: Vec<String> = Vec::new();

    for &record in BASE_RECORDS {
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

            // Record fields the port does not model at all (no field_list entry,
            // no storage). A documented feature gap, not a default divergence.
            if let Some(reason) = unmodeled_feature_reason(record, &field) {
                skipped_unmodeled.push(format!(
                    "{record}.{field} (C initial {initial:?}): {reason}"
                ));
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
                    &[(field.clone(), initial.as_str().into())],
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
         {} documented deviations; {} common fields skipped; \
         {} unmodeled-feature fields skipped; \
         {} verified by value (loader can't set them: {value_fallback:?})",
        deviations.len(),
        skipped_common.len(),
        skipped_unmodeled.len(),
        value_fallback.len(),
    );
    for d in &deviations {
        eprintln!("  deviation: {d}");
    }
    for u in &skipped_unmodeled {
        eprintln!("  unmodeled feature: {u}");
    }

    assert!(
        mismatches.is_empty(),
        "Rust record defaults diverge from C dbd initials:\n  {}",
        mismatches.join("\n  ")
    );
}
