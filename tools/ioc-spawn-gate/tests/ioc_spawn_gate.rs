//! The live gate: every IOC-spawning binary in this workspace is declared.
//!
//! This runs the scan over the real tree, so a new test binary that spawns an
//! IOC turns this test red on the commit that adds it — which is the whole
//! point. `cargo run -p ioc-spawn-gate --bin ioc-spawn-census` prints what the
//! scan sees, and the panic text names every binary that is missing.
//!
//! No floor lives here any more. The two that did — 30 crates, 15 spawners —
//! guarded a scan that finds nothing, and each is now checked against
//! something measured from the tree instead: the crate set against the
//! workspace manifest's own `members` globs, and the spawner scan against the
//! `ca_softioc` / `ioc_unthrottled` declarations, which name the binaries
//! somebody decided are spawners and which must therefore still scan as
//! spawners.

#[test]
fn every_ioc_spawning_binary_is_declared_in_a_test_group() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    ioc_spawn_gate::assert_every_ioc_spawner_is_declared(root);
}

/// These four reach `softIoc` only through epics-ca-rs's shared test helpers,
/// which is the case the scan follows furthest: through `mod common;` while
/// the helpers sat in `tests/common/`, through the dev-dependency once they
/// moved into one. A scan that stops following either drops them silently,
/// because nothing is left to demand their declaration.
#[test]
fn the_shared_helper_spawners_stay_in_the_census() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let census: Vec<String> = ioc_spawn_gate::census(root)
        .iter()
        .map(ioc_spawn_gate::Spawner::label)
        .collect();
    for binary in [
        "cli_monitor_separator",
        "interop_c_client_rust_ioc",
        "interop_rust_client_c_ioc",
        "stress_load",
    ] {
        let label = format!("epics-ca-rs::{binary}");
        assert!(
            census.contains(&label),
            "{label} left the census: {census:#?}"
        );
    }
}
