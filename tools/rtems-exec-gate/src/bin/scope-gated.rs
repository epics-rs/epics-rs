//! Print what `SCOPE_GATED` should say about a tree, and why each entry moved.
//!
//! ```text
//! cargo run -q -p rtems-exec-gate --bin scope-gated -- <workspace-root>
//! ```
//!
//! The constant is a name-set rather than a count precisely so that it merges:
//! two branches that each gate a different file compose into the union instead
//! of into a wrong total. What does NOT compose is the pin itself — neither
//! branch has the other's entry, so the merged tree names fewer files than it
//! gates, and the census test fails on a tree where no branch was wrong. Filling
//! it in is therefore a merge-time step, on the merged tree, and it needs the
//! measurement to be readable from a tree nobody has built yet.
//!
//! Hence a binary rather than the test's panic message. The test can only speak
//! when it fails, only about the tree it is compiled inside, and only after
//! every declaring crate has passed its marker check — so a single unmarked
//! site anywhere aborts the run before the pin's text is ever printed. This
//! prints the text first and asserts nothing.
//!
//! Exit status is 0 whether or not the tree matches its pin: the gate is the
//! test, this is the instrument you read before editing.

use rtems_exec_gate::{
    Dropped, declaring_crates, pinned_scope_gated, scope_gated_literal, scope_gated_set,
    why_dropped,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".to_owned()));
    assert!(
        root.join("Cargo.toml").is_file(),
        "{} is not a workspace root: no Cargo.toml there",
        root.display()
    );

    let measured = scope_gated_set(&root);
    let pinned = pinned_scope_gated(&root);
    let dirs: BTreeMap<String, PathBuf> = declaring_crates(&root).into_iter().collect();

    println!(
        "tree {}\n  declaring crates: {}\n  measured: {} file(s)\n  pinned:   {} file(s)\n",
        root.display(),
        dirs.len(),
        measured.len(),
        pinned.len()
    );

    let started: Vec<_> = measured.difference(&pinned).collect();
    let stopped: Vec<_> = pinned.difference(&measured).collect();

    // Both directions report their count even when it is zero. An absent
    // section reads the same as a section that failed to print, and the
    // question this instrument is read for — did anything leave the pin —
    // is answered by a zero as much as by a list.
    if started.is_empty() {
        println!("started gating (0): nothing to add.\n");
    } else {
        println!("started gating ({}), add:", started.len());
        for (c, f) in &started {
            println!("  + (\"{c}\", \"{f}\")");
        }
        println!();
    }

    if stopped.is_empty() {
        println!(
            "stopped gating (0): nothing to remove, so no file stopped gating \
             and no sites came back into the census.\n"
        );
    } else {
        println!(
            "stopped gating ({}), remove — only `GATE LOST` returns sites to the census:",
            stopped.len()
        );
        for (c, f) in &stopped {
            let why = match dirs.get(c.as_str()) {
                None => "crate is no longer in the census (dropped its declaration, or left the workspace)".to_owned(),
                Some(dir) => match why_dropped(dir, f) {
                    Dropped::Gone => "file is gone — renamed, moved or deleted".to_owned(),
                    Dropped::StillGated { collateral } => format!(
                        "still gated; no reactor-dependent site left to take ({collateral} reactor-free test(s) still removed)"
                    ),
                    Dropped::ModGated { declared_in, line } => format!(
                        "now removed whole by `{declared_in}:{line}`'s gated `mod`; its sites did not come back"
                    ),
                    Dropped::GateLost { sites } => format!(
                        "GATE LOST — {sites} reactor-dependent site(s) back in the census and now needing markers"
                    ),
                },
            };
            println!("  - (\"{c}\", \"{f}\")\n      {why}");
        }
        println!();
    }

    println!(
        "paste over the constant in {} — this is the layout `cargo fmt` writes, \
         so a paste needs no reformat:\n\n{}",
        Path::new(rtems_exec_gate::PIN_FILE).display(),
        scope_gated_literal(&measured)
    );
}
