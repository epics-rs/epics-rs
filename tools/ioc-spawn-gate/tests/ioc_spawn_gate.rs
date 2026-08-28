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
