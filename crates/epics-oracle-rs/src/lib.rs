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
pub mod ntshape;
pub mod pvamonitor;
pub mod pvaread;
pub mod pvatool;
pub mod report;
pub mod runner;
pub mod surface;

pub use allowlist::Allowlist;
pub use dbd::Dbd;
pub use diff::Verdict;
pub use ioc::{CTools, Pair, PvaPair, PvxTools};
pub use pvatool::PvaTools;
pub use report::{Counts, Report};
pub use runner::Runner;
pub use surface::{Coverage, Surface};

/// The asyn port both differential sides attach every asyn record to.
///
/// C's `asynRecord` refuses to `init_record` against an empty PORT
/// (`connectDevice` finds no port and the record errors out), so a bare
/// `record(asyn, "…") {}` cannot be swept. Both sides therefore pin the same
/// port name here: the Rust `oracle_ioc` registers a disconnected
/// `DrvAsynIPPort` under it, and the C st.cmd creates a matching port before
/// `iocInit`.
pub const ORACLE_ASYN_PORT: &str = "ORACLEASYN";

/// One `record(type, "name") { … }` statement for a reproducer `.db`.
///
/// Every record type gets an empty body — the reproducer is deliberately
/// minimal — EXCEPT `asyn`, which pins `field(PORT, "ORACLEASYN")` so the
/// record can attach to a port and `init_record` (see [`ORACLE_ASYN_PORT`]).
/// Both differential sides are handed byte-identical db text, so this is the
/// single owner of that asymmetry.
pub fn record_stmt(record_type: &str, rec_name: &str) -> String {
    record_stmt_fields(record_type, rec_name, &[])
}

/// [`record_stmt`], plus `fields` in the record body.
///
/// The single owner of the reproducer statement, so the `asyn` PORT rule above
/// holds for every reproducer rather than for the one that happened to call the
/// original. [`pvamonitor`] needs a `SCAN` field on its reproducer and the array
/// phase needs `NELM`/`FTVL` on theirs — neither may grow a second, PORT-less
/// spelling to get it.
pub fn record_stmt_fields(record_type: &str, rec_name: &str, fields: &[(&str, &str)]) -> String {
    let mut body = String::new();
    if record_type == "asyn" {
        body.push_str(&format!("    field(PORT, \"{ORACLE_ASYN_PORT}\")\n"));
    }
    for (name, value) in fields {
        body.push_str(&format!("    field({name}, \"{value}\")\n"));
    }
    if body.is_empty() {
        format!("record({record_type}, \"{rec_name}\") {{}}\n")
    } else {
        format!("record({record_type}, \"{rec_name}\") {{\n{body}}}\n")
    }
}

/// Whether the oracle's **put** phase can yield a comparable C-vs-Rust verdict
/// for a record type.
///
/// `asyn` is the sole exception. Its writable fields drive async device I/O
/// whose `ca_put_callback` never completes against the deliberately
/// disconnected `ORACLEASYN` port (see [`ORACLE_ASYN_PORT`]) — on *both* sides —
/// so every asyn put times out and errors instead of producing a comparison.
/// A connected loopback would make them measurable but inject socket-timing
/// nondeterminism into the oracle, so `asyn` is a read/monitor-only record here.
///
/// This is NOT the "exit 0 when you could not look" anti-pattern the harness
/// exists to prevent: the omission is explicit (this predicate), loud (the put
/// dispatch logs each skip by name), and scoped to one record type whose put
/// surface was deliberately placed out of scope — not a silent blind spot.
/// The caller MUST log when this returns `false` so a skipped phase is never
/// mistaken for a measured-clean one.
pub fn puts_are_measurable(record_type: &str) -> bool {
    record_type != "asyn"
}

/// Keep-alive for the process-global registrations
/// [`register_port_ioc_devices`] performs. Dropping it tears the asyn port's
/// runtime down, so it must outlive every IOC built against it.
pub struct PortIocDevices {
    _asyn_manager: asyn_rs::manager::PortManager,
    _asyn_port: asyn_rs::runtime::PortRuntimeHandle,
}

/// The process-global half of the port IOC's configuration: the disconnected
/// `ORACLEASYN` port and asyn's `DTYP` device menus.
///
/// Paired with [`port_ioc_builder`], and the two are the **single owner** of
/// "how the port under test is configured". Both the measured IOC
/// (`oracle-ioc`) and [`crate::surface::probe_supported_record_types`] go
/// through them, because a denominator measured from a *different*
/// configuration than the thing under test is not a denominator. `asyn` is
/// where the two used to diverge: without this, the probe loads
/// epics-base-rs's CNCT-only stub record and answers "implemented" for an IOC
/// nobody runs, while the measured IOC serves asyn-rs's `AsynRecord` attached
/// to this port.
///
/// A `drvAsynIPPort` on `localhost:1`, `noAutoConnect=1`, `noProcessEos=0` —
/// the exact C st.cmd `drvAsynIPPortConfigure("ORACLEASYN","localhost:1",
/// 0,1,0)`. Nothing listens on `localhost:1` and `noAutoConnect` keeps the port
/// from ever dialing, so it stays permanently disconnected on both sides while
/// still answering HOSTINFO / DRTO exactly as C does.
///
/// The menu registration is menu-only on purpose: it does NOT install the
/// universal asyn factory, so no record's read fields or processing change —
/// only the `DTYP` `value.choices` a client reads, matching the fat-C ground
/// truth.
pub fn register_port_ioc_devices() -> Result<PortIocDevices, String> {
    use asyn_rs::drivers::ip_port::DrvAsynIPPort;
    use asyn_rs::manager::PortManager;

    let manager = PortManager::new();
    let port = manager
        .register_port(
            DrvAsynIPPort::new_configured(
                ORACLE_ASYN_PORT,
                "localhost:1",
                true,  // noAutoConnect — never dial; stay permanently disconnected
                false, // noProcessEos=0 — install the default EOS interpose, as C does
            )
            .map_err(|e| format!("configure {ORACLE_ASYN_PORT}: {e}"))?,
        )
        .map_err(|e| format!("register {ORACLE_ASYN_PORT}: {e}"))?;

    asyn_rs::adapter::register_asyn_device_menus();

    Ok(PortIocDevices {
        _asyn_manager: manager,
        _asyn_port: port,
    })
}

/// The per-builder half of the port IOC's configuration: route the `asyn`
/// record type to asyn-rs's full `AsynRecord` rather than epics-base-rs's
/// CNCT-only display stub. See [`register_port_ioc_devices`] for why the two
/// halves must stay together.
pub fn port_ioc_builder() -> epics_base_rs::server::ioc_builder::IocBuilder {
    let (asyn_type, asyn_factory) = asyn_rs::asyn_record::asyn_record_factory();
    epics_base_rs::server::ioc_builder::IocBuilder::new()
        .register_record_type(asyn_type, asyn_factory)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason `record_stmt_fields` exists rather than a second, local
    /// spelling in [`pvamonitor`]: `asyn`'s PORT is what lets the record
    /// `init_record` at all, and a reproducer that asks for a SCAN field must
    /// not lose it.
    #[test]
    fn extra_fields_never_cost_asyn_its_port() {
        let db = record_stmt_fields("asyn", "ORACLE:MON:ASYN", &[("SCAN", ".1 second")]);
        assert!(
            db.contains(&format!("field(PORT, \"{ORACLE_ASYN_PORT}\")")),
            "{db}"
        );
        assert!(db.contains("field(SCAN, \".1 second\")"), "{db}");
    }

    /// A reproducer with no fields keeps the empty body the read phase's db text
    /// already pins.
    #[test]
    fn a_plain_record_is_unchanged_by_the_shared_owner() {
        assert_eq!(
            record_stmt("ai", "ORACLE:AI"),
            "record(ai, \"ORACLE:AI\") {}\n"
        );
        assert_eq!(
            record_stmt_fields("ai", "ORACLE:AI", &[]),
            record_stmt("ai", "ORACLE:AI")
        );
    }

    #[test]
    fn a_scanned_reproducer_names_its_scan_rate() {
        assert_eq!(
            record_stmt_fields("ai", "S:AI", &[("SCAN", ".1 second")]),
            "record(ai, \"S:AI\") {\n    field(SCAN, \".1 second\")\n}\n"
        );
    }
}
