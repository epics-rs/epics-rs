//! R18-16: a duplicated address-list entry is DISCARDED and REPORTED.
//!
//! Every EPICS address list is built the same way — `addAddrToChannelAccess
//! AddressList` then `removeDuplicateAddresses(…, silent=0)` — and the port had
//! neither half of the second call: duplicates survived (doubling the search or
//! beacon traffic to that destination for the life of the process) and nothing
//! was printed.
//!
//! The expected byte string is the one the compiled tools write. Captured from
//! `/home/stevek/work/epics-base/bin/linux-x86_64`:
//!
//! ```text
//! $ EPICS_CA_ADDR_LIST="127.0.0.1:15099 127.0.0.1:15099" caget TST:X
//! Warning: Duplicate EPICS CA Address list entry "127.0.0.1:15099" discarded
//! $ EPICS_CAS_INTF_ADDR_LIST="127.0.0.1 127.0.0.1" softIoc -x tst
//! Warning: Duplicate EPICS CA Address list entry "127.0.0.1:15264" discarded
//! ```
//!
//! Both tools are driven at a dead address here: the diagnostic is emitted while
//! the address list is being built, long before anything is searched for, so no
//! IOC and no port of our own is needed.

use std::process::Command;

/// The line, byte for byte, that C's `removeDuplicateAddresses` writes for a
/// repeat of `127.0.0.1:15099`.
const DUP: &str = "Warning: Duplicate EPICS CA Address list entry \"127.0.0.1:15099\" discarded\n";

/// `EPICS_CA_ADDR_LIST` — C `iocinf.cpp:227`.
#[test]
fn a_repeated_ca_addr_list_entry_is_reported_once_per_repeat() {
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .arg("-w")
        .arg("0.2")
        .arg("TST:NOSUCHPV")
        .env(
            "EPICS_CA_ADDR_LIST",
            "127.0.0.1:15099 127.0.0.1:15099 127.0.0.1:15098",
        )
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .output()
        .expect("run caget-rs");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        stderr.matches(DUP).count(),
        1,
        "one repeat of one address ⇒ exactly one warning; stderr: {stderr:?}"
    );
    // `:15098` is a different port, so it is a different entry — C says nothing
    // about it, and neither may we.
    assert!(!stderr.contains("15098"), "stderr: {stderr:?}");
}

/// Three copies discard two, and C reports each discard — the warning is
/// per-dropped-entry, not per-duplicated-address.
#[test]
fn every_repeat_after_the_first_is_reported() {
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .arg("-w")
        .arg("0.2")
        .arg("TST:NOSUCHPV")
        .env(
            "EPICS_CA_ADDR_LIST",
            "127.0.0.1:15099 127.0.0.1:15099 127.0.0.1:15099",
        )
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .output()
        .expect("run caget-rs");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        stderr.matches(DUP).count(),
        2,
        "three copies discard two; stderr: {stderr:?}"
    );
}

/// `EPICS_CA_NAME_SERVERS` — C `cac.cpp:260` hands this list to the SAME
/// `removeDuplicateAddresses`, with the same `silent=0`. The port's name-server
/// parser was a second hand-rolled copy that had lost the dedup too.
#[test]
fn a_repeated_name_server_entry_is_reported() {
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .arg("-w")
        .arg("0.2")
        .arg("TST:NOSUCHPV")
        .env("EPICS_CA_NAME_SERVERS", "127.0.0.1:15099 127.0.0.1:15099")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .output()
        .expect("run caget-rs");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        stderr.matches(DUP).count(),
        1,
        "the name-server list is deduped by the same helper; stderr: {stderr:?}"
    );
}

/// A list with no repeats says nothing at all — the warning is not a
/// "this list was processed" trace.
#[test]
fn a_clean_list_is_silent() {
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .arg("-w")
        .arg("0.2")
        .arg("TST:NOSUCHPV")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1:15099 127.0.0.1:15098")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .output()
        .expect("run caget-rs");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !stderr.contains("Duplicate EPICS CA Address list entry"),
        "stderr: {stderr:?}"
    );
}
