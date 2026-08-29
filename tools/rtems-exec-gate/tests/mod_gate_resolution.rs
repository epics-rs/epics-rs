//! The gate shape a reading of the removed file cannot see.
//!
//! A `#[cfg(<gate>)] mod x;` empties `x.rs` from another file entirely, so
//! every case here is about resolving a declaration to the files it removes:
//! one per way the resolution can be wrong, since a resolution that silently
//! finds nothing reports a cost of zero and looks exactly like a gate that cost
//! nothing.

use rtems_exec_gate::audit_crate;
use std::fs;
use std::path::{Path, PathBuf};

fn tokio_test() -> &'static str {
    concat!("#[tokio", "::test]\nasync fn t() {}\n")
}

/// A file holding one reactor-dependent site and `plain` reactor-free tests.
fn source(plain: usize) -> String {
    let mut s = String::from("#![allow(dead_code)]\n");
    s.push_str(tokio_test());
    for i in 0..plain {
        s.push_str(&format!("\n#[test]\nfn free_{i}() {{}}\n"));
    }
    s
}

fn crate_at(name: &str, files: &[(&str, String)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    for (rel, text) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        fs::write(&path, text).expect("write fixture");
    }
    dir
}

/// `(module, files removed, sites, collateral)` for each declaration.
fn shape(dir: &Path) -> Vec<(String, usize, usize, usize)> {
    audit_crate(dir.to_str().expect("utf-8"))
        .mod_gated
        .into_iter()
        .map(|m| {
            (
                m.module,
                m.removes.len(),
                m.removes.iter().map(|c| c.sites).sum(),
                m.removes.iter().map(|c| c.collateral).sum(),
            )
        })
        .collect()
}

#[test]
fn a_gated_declaration_costs_the_file_it_names() {
    let dir = crate_at(
        "modgate_flat",
        &[
            (
                "src/lib.rs",
                format!("{}\nmod x;\n", rtems_exec_gate::GATE_ATTR),
            ),
            ("src/x.rs", source(3)),
        ],
    );
    assert_eq!(shape(&dir), [("x".to_owned(), 1, 1, 3)]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_directory_module_resolves_through_its_mod_rs() {
    let dir = crate_at(
        "modgate_dir",
        &[
            (
                "src/lib.rs",
                format!("{}\nmod x;\n", rtems_exec_gate::GATE_ATTR),
            ),
            ("src/x/mod.rs", source(2)),
        ],
    );
    assert_eq!(shape(&dir), [("x".to_owned(), 1, 1, 2)]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_gated_module_takes_its_whole_subtree() {
    let dir = crate_at(
        "modgate_subtree",
        &[
            (
                "src/lib.rs",
                format!("{}\nmod x;\n", rtems_exec_gate::GATE_ATTR),
            ),
            ("src/x.rs", source(1)),
            ("src/x/deep.rs", source(4)),
            ("src/x/deeper/leaf.rs", source(2)),
        ],
    );
    assert_eq!(shape(&dir), [("x".to_owned(), 3, 3, 7)]);
    let _ = fs::remove_dir_all(&dir);
}

/// `mod y;` inside `x.rs` resolves under `x/`, not beside `x.rs` — the rule
/// that differs between a mod-rs file and any other.
#[test]
fn a_declaration_in_a_non_mod_rs_file_resolves_under_its_own_directory() {
    let dir = crate_at(
        "modgate_nonmodrs",
        &[
            ("src/lib.rs", "mod x;\n".to_owned()),
            (
                "src/x.rs",
                format!("{}\nmod y;\n", rtems_exec_gate::GATE_ATTR),
            ),
            ("src/x/y.rs", source(5)),
            ("src/y.rs", source(9)),
        ],
    );
    assert_eq!(
        shape(&dir),
        [("y".to_owned(), 1, 1, 5)],
        "the sibling src/y.rs is a different module and must not be charged"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// `#[path]` is relative to the DECLARING FILE's directory. Ignoring it would
/// resolve to nothing, and a resolution that finds nothing is indistinguishable
/// from a gate that cost nothing.
#[test]
fn a_path_override_is_followed_from_the_declaring_files_directory() {
    let dir = crate_at(
        "modgate_path",
        &[
            (
                "src/lib.rs",
                format!(
                    "{}\n#[path = \"../shared/helper.rs\"]\nmod x;\n",
                    rtems_exec_gate::GATE_ATTR
                ),
            ),
            ("shared/helper.rs", source(6)),
        ],
    );
    assert_eq!(shape(&dir), [("x".to_owned(), 1, 1, 6)]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_ungated_declaration_costs_nothing() {
    let dir = crate_at(
        "modgate_ungated",
        &[
            (
                "src/lib.rs",
                "#[cfg(feature = \"ioc\")]\nmod x;\n".to_owned(),
            ),
            ("src/x.rs", source(3)),
        ],
    );
    assert!(shape(&dir).is_empty(), "{:?}", shape(&dir));
    let _ = fs::remove_dir_all(&dir);
}

/// The two shapes are disjoint by construction, so their costs add. A file that
/// gates itself is reported once, by the shape that can see it in place.
#[test]
fn a_file_that_gates_itself_is_not_also_charged_to_its_declaration() {
    let dir = crate_at(
        "modgate_selfgated",
        &[
            (
                "src/lib.rs",
                format!("{}\nmod x;\n", rtems_exec_gate::GATE_ATTR),
            ),
            (
                "src/x.rs",
                format!("{}\n{}", rtems_exec_gate::FILE_GATE_ATTR, source(3)),
            ),
        ],
    );
    let census = audit_crate(dir.to_str().expect("utf-8"));
    assert!(census.mod_gated.is_empty(), "{:?}", census.mod_gated);
    assert_eq!(census.scope_gated.len(), 1);
    assert_eq!(census.scope_gated[0].collateral, 3);
    let _ = fs::remove_dir_all(&dir);
}

/// A gate inside a subtree another gate already removed adds nothing: the files
/// are gone once, and charging them twice would inflate the total the sign-off
/// rests on.
#[test]
fn a_nested_gated_declaration_does_not_charge_the_subtree_twice() {
    let dir = crate_at(
        "modgate_nested",
        &[
            (
                "src/lib.rs",
                format!("{}\nmod x;\n", rtems_exec_gate::GATE_ATTR),
            ),
            (
                "src/x.rs",
                format!("{}\nmod y;\n", rtems_exec_gate::GATE_ATTR),
            ),
            ("src/x/y.rs", source(4)),
        ],
    );
    let total: usize = audit_crate(dir.to_str().expect("utf-8"))
        .mod_gated
        .iter()
        .flat_map(|m| &m.removes)
        .map(|c| c.collateral)
        .sum();
    assert_eq!(total, 4, "src/x/y.rs is removed once, not once per gate");
    let _ = fs::remove_dir_all(&dir);
}

/// Cost, not accounting. The removed file is still scanned normally, and that
/// is where its census markers are judged; judging it a second time as gated
/// would read every marker in it as stale.
#[test]
fn a_removed_files_census_markers_are_not_re_judged() {
    let dir = crate_at(
        "modgate_markers",
        &[
            (
                "src/lib.rs",
                format!("{}\nmod x;\n", rtems_exec_gate::GATE_ATTR),
            ),
            (
                "src/x.rs",
                format!(
                    "// {}1): needs a reactor\n{}",
                    rtems_exec_gate::CENSUS_MARKER,
                    tokio_test()
                ),
            ),
        ],
    );
    let census = audit_crate(dir.to_str().expect("utf-8"));
    assert!(census.violations.is_empty(), "{:?}", census.violations);
    assert_eq!(census.mod_gated.len(), 1);
    assert_eq!(census.mod_gated[0].removes[0].sites, 1);
    let _ = fs::remove_dir_all(&dir);
}
