//! What the sources say about every target's declared cfg.
//!
//! `cargo run -p target-cfg-gate --bin target-cfg-census`. Prints one line per
//! (target file, gated thing it names) pair with the requirement and the gate
//! the target declares, so a breach can be read without re-deriving it by hand.

fn main() {
    let root = target_cfg_gate::workspace_root();
    print!("{}", target_cfg_gate::audit(&root).census());
}
