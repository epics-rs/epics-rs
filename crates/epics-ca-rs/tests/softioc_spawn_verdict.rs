//! The retry trigger for a C `softIoc` spawn, pinned against C's real console.
//!
//! `named_port::on_a_named_port` spends a fresh candidate whenever the closure
//! returns `None`, so the closure's verdict is the whole safety of the scheme:
//! a `None` on anything but "somebody else has this number" converts a real
//! regression into sixteen slow attempts and a panic that blames the host.
//!
//! `softIoc` is the dangerous case, because it never reports what it bound.
//! The console text below was measured on `bin/linux-x86_64/softIoc`
//! (R7.0.10-146) with a listener already holding the TCP port, which is the
//! race this exists for.

// Only the verdict, deliberately: `mod common;` would drag `softioc_on`'s
// `Command::new("softIoc")` into a binary that spawns nothing, and
// tools/ioc-spawn-gate reads the mod graph, not the call sites.
#[path = "common/softioc_verdict.rs"]
mod softioc_verdict;

use softioc_verdict::{SoftIocVerdict, softioc_verdict};

/// C's stderr when the TCP port it was told to use was already held.
///
/// Measured, verbatim. Note the LAST line: C does not stop. `rsrv_grab_tcp`
/// hands back a different number, RSRV warns, and `iocInit` runs to completion
/// on it (`caservertask.c:582-590` @R7.0.10).
const PORT_HELD: &str = "\
Starting iocInit
cas WARNING: Configured TCP port was unavailable.
cas WARNING: Using dynamically assigned TCP port 43621,
cas WARNING: but now two or more servers share the same UDP port.
cas WARNING: Depending on your IP kernel this server may not be
cas WARNING: reachable with UDP unicast (a host's IP in EPICS_CA_ADDR_LIST)
iocRun: All initialization complete
";

/// C's stderr when the database will not load, measured on the same binary
/// (`softIoc -S -d` on a record of an unknown type). The colour escapes C
/// wraps each `ERROR:` in are elided; nothing else is. The point is the
/// absence: an IOC this broken makes no statement about a port at all, and a
/// truncated `.db` says even less — it segfaults C before errlog gets a line
/// out, which is the same absence with a corpse attached.
const BAD_DATABASE: &str = "\
ERROR: Record type 'nosuchtype' for record 'T:X' not found
ERROR:  at or before ')' in path \".\"  file \"badtype.db\" line 1

 1 | record(nosuchtype, \"T:X\") {

ERROR: syntax error

 1 | record(nosuchtype, \"T:X\") {

ERROR: Failed to load 'badtype.db'
";

/// C's stderr on a clean boot on the number it was given.
const CLEAN_BOOT: &str = "\
Starting iocInit
iocRun: All initialization complete
";

/// The ordering is the whole test: C prints the ready line in BOTH cases, so a
/// verdict that looked for it first would call a fallback IOC a win and hand
/// the test a server on a port it never named — a stranger's number, reachable
/// or not, with every later assertion measuring the wrong process.
#[test]
fn a_fallback_outranks_the_ready_line_c_prints_in_both_cases() {
    assert!(PORT_HELD.contains("iocRun: All initialization complete"));
    assert_eq!(softioc_verdict(PORT_HELD), SoftIocVerdict::PortTaken);
    assert_eq!(softioc_verdict(CLEAN_BOOT), SoftIocVerdict::Up);
}

/// Everything else is `Silent`, which the spawn turns into a panic rather than
/// a retry. These are the consoles a BROKEN IOC leaves, and none of them gets
/// better on a different port — the failure has to reach the test as a failure.
#[test]
fn a_broken_ioc_is_not_a_stolen_port() {
    for console in [
        // Nothing at all: the binary died before errlog opened.
        "",
        // A bad database. Measured, SGR colour escapes elided: softIoc says a
        // great deal and none of it is about a port.
        BAD_DATABASE,
        // The shape the retry must never accept: a client-side symptom with no
        // statement from the IOC about the port. Retrying on this is exactly
        // what would hide a regression as a slow pass.
        "caget: Channel connect timed out: 'T:AI' not found.\n",
        // Partway through init and then stuck: `Starting iocInit` alone is not
        // a report about the server, which `iocRun` prints after `rsrv_run`.
        "Starting iocInit\n",
        // Another server's warning about a DIFFERENT resource. It shares the
        // `cas WARNING:` prefix and must still be Silent, so the discriminator
        // cannot be loosened to that prefix.
        "cas WARNING: Configured UDP port was unavailable.\n",
    ] {
        assert_eq!(
            softioc_verdict(console),
            SoftIocVerdict::Silent,
            "console must not be read as a stolen port: {console:?}"
        );
    }
}
