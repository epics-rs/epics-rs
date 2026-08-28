//! Print the derived census: every nextest binary that spawns an EPICS IOC.
//!
//! `cargo run -p ioc-spawn-gate --bin ioc-spawn-census`. This is what the
//! filters in `.config/nextest.toml` have to cover between them; the gate in
//! `tests/ioc_spawn_gate.rs` is the same scan with the coverage check applied.

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf();
    for s in ioc_spawn_gate::census(&root) {
        println!("{:<44} {}", s.label(), s.evidence);
    }
}
