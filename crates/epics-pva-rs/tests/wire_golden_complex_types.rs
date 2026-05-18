//! Default-suite golden replay for complex PVA structures.
//!
//! For each `PvBuild` in the shared matrix, re-encode the same
//! descriptor + value via the current Rust encoder and assert
//! byte-equality against `tests/fixtures/pvxs/<pv>.bin`. The
//! fixtures were captured during a `--profile interop` run with
//! `UPDATE_GOLDENS=1`, after pvxget verified pvxs accepts the
//! bytes (see `interop_pvxs_mods/complex_types.rs`).
//!
//! Trust chain:
//!   - interop test: pvxs CLIENT parses Rust SERVER's bytes
//!   - golden capture: bytes frozen as fixtures
//!   - this test: Rust encoder must keep producing the same
//!     bytes — runs on every push, no external dep
//!
//! If a fixture is missing or differs, EITHER:
//!   (a) the Rust encoder regressed (most common — fix it), OR
//!   (b) pvxs's wire format changed (rare — re-run interop with
//!       UPDATE_GOLDENS=1, review the diff, commit if intentional).

#[path = "interop_helpers/pv_builders.rs"]
mod pv_builders;

use pv_builders::{complex_pv_matrix, encode_pv_fixture, split_fixture};

use std::path::PathBuf;

fn fixture_path(pv: &str) -> PathBuf {
    let stem = pv.replace([':', '/'], "_");
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pvxs")
        .join(format!("{stem}.bin"))
}

#[test]
fn wire_golden_complex_types_byte_exact() {
    let pvs = complex_pv_matrix();
    let mut failures: Vec<String> = Vec::new();
    let mut missing_fixtures: Vec<String> = Vec::new();

    for build in &pvs {
        let path = fixture_path(build.name);
        let Ok(golden) = std::fs::read(&path) else {
            missing_fixtures.push(format!(
                "{} → {:?} (run with `UPDATE_GOLDENS=1 cargo nextest run --profile interop \
                 -p epics-pva-rs -E 'test(interop_complex_types_pvxget_against_rust_server)'` \
                 to regenerate)",
                build.name, path,
            ));
            continue;
        };
        let actual = encode_pv_fixture(build);
        if actual != golden {
            // Split into desc/value halves for a more useful diff
            // when the failure lands.
            let (golden_desc, golden_val) = split_fixture(&golden).unwrap_or((&[], &[]));
            let (actual_desc, actual_val) = split_fixture(&actual).unwrap_or((&[], &[]));
            failures.push(format!(
                "[{}] fixture mismatch.\n  \
                 golden desc ({:>3}B): {}\n  \
                 actual desc ({:>3}B): {}\n  \
                 golden val  ({:>3}B): {}\n  \
                 actual val  ({:>3}B): {}",
                build.name,
                golden_desc.len(),
                hex(golden_desc),
                actual_desc.len(),
                hex(actual_desc),
                golden_val.len(),
                hex(golden_val),
                actual_val.len(),
                hex(actual_val),
            ));
        }
    }

    if !missing_fixtures.is_empty() {
        panic!(
            "{} fixture file(s) missing:\n  {}",
            missing_fixtures.len(),
            missing_fixtures.join("\n  "),
        );
    }
    assert!(
        failures.is_empty(),
        "{} fixture mismatch(es):\n{}",
        failures.len(),
        failures.join("\n----\n"),
    );
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        use std::fmt::Write;
        let _ = write!(&mut s, "{byte:02x}");
    }
    s
}
