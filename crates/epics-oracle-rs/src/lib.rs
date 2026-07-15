//! # Differential oracle harness
//!
//! Boots the **compiled C `softIoc`** and the **Rust IOC** on the same `.db`,
//! drives both with identical CA operations through the same C client tools,
//! and diffs only what a client can observe. It exists to make "clean" a
//! *measurement* instead of an opinion.
//!
//! ## Why this replaces reading C and Rust side by side
//!
//! Nineteen audit rounds never converged (`doc/strategy-2026-07-13.md`): the
//! discovery rate never fell, there was no denominator, and "verified clean"
//! verdicts kept turning out false. Two structural fixes follow, and this crate
//! is the second:
//!
//! - the surface is enumerated **from the `.dbd`**, so coverage is a percentage
//!   of a stated denominator rather than a feeling ([`surface`], [`dbd`]);
//! - a case that could not run is an **ERROR**, never an agreement ([`diff`],
//!   [`report`]). That single rule is what makes a clean verdict mean something.
//!
//! ## The three-way verdict
//!
//! Under the product policy, a C-vs-port difference is *not* automatically a
//! port bug — the port deliberately refuses to reproduce C's bugs. So the
//! expected-deviation allowlist **is** `doc/upstream-c-bugs.md`, transcribed
//! into [`allowlist`]:
//!
//! | outcome | meaning |
//! |---|---|
//! | AGREED | both sides read the same |
//! | EXPECTED DEVIATION | they differ, and a NOT-REPRODUCED CBUG entry justifies it |
//! | DEFECT | they differ and nothing justifies it |
//! | ERROR | no reading was obtained — **never** scored as agreement |
//!
//! A row that stops firing is reported too: the deviation vanished, which is
//! either a port regression or an upstream fix. That makes the harness and the
//! catalogue check each other.
//!
//! ## Usage
//!
//! As a binary:
//!
//! ```text
//! cargo run -p epics-oracle-rs --bin oracle -- --phase all --json out.json
//! ```
//!
//! From a test: see `tests/oracle.rs`, which boots the pair and asserts the
//! counts reconcile.

pub mod allowlist;
pub mod cases;
pub mod catool;
pub mod dbd;
pub mod diff;
pub mod ioc;
pub mod report;
pub mod runner;
pub mod surface;

pub use allowlist::Allowlist;
pub use dbd::Dbd;
pub use diff::Verdict;
pub use ioc::{CTools, Pair};
pub use report::{Counts, Report};
pub use runner::Runner;
pub use surface::{Coverage, Surface};

/// The asyn port both differential sides attach every asyn record to.
///
/// C's `asynRecord` refuses to `init_record` against an empty PORT
/// (`connectDevice` finds no port and the record errors out), so a bare
/// `record(asyn, "…") {}` cannot be swept. Both sides therefore pin the same
/// port name here: the Rust `oracle_ioc` registers a `NullOctetPort` under it,
/// and the C st.cmd creates a matching port before `iocInit`.
pub const ORACLE_ASYN_PORT: &str = "ORACLEASYN";

/// One `record(type, "name") { … }` statement for a reproducer `.db`.
///
/// Every record type gets an empty body — the reproducer is deliberately
/// minimal — EXCEPT `asyn`, which pins `field(PORT, "ORACLEASYN")` so the
/// record can attach to a port and `init_record` (see [`ORACLE_ASYN_PORT`]).
/// Both differential sides are handed byte-identical db text, so this is the
/// single owner of that asymmetry.
pub fn record_stmt(record_type: &str, rec_name: &str) -> String {
    if record_type == "asyn" {
        format!(
            "record({record_type}, \"{rec_name}\") {{\n    field(PORT, \"{ORACLE_ASYN_PORT}\")\n}}\n"
        )
    } else {
        format!("record({record_type}, \"{rec_name}\") {{}}\n")
    }
}
