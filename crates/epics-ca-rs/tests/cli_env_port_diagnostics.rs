//! Regression tests (R17-16): `EPICS_CA_SERVER_PORT` /
//! `EPICS_CA_REPEATER_PORT` resolve through C `envGetInetPortConfigParam`
//! (`envSubr.c:397-424` on top of `envGetLongConfigParam`, `envSubr.c:303`)
//! — not `parse::<u16>()`.
//!
//! Transcript of the compiled C `caget` (epics-base bin/linux-x86_64),
//! `EPICS_CA_ADDR_LIST=127.0.0.1 EPICS_CA_AUTO_ADDR_LIST=NO caget -w 0.3 pv`:
//!
//! ```text
//! $ EPICS_CA_SERVER_PORT=3000 caget …          # also 70000, -1, 0
//! EPICS Environment "EPICS_CA_SERVER_PORT" out of range
//! Setting "EPICS_CA_SERVER_PORT" = 5064
//!
//! $ EPICS_CA_SERVER_PORT=abc caget …
//! Unable to find an integer in EPICS_CA_SERVER_PORT=abc
//! EPICS Environment "EPICS_CA_SERVER_PORT" integer fetch failed
//! setting "EPICS_CA_SERVER_PORT" = 5064
//!
//! $ EPICS_CA_SERVER_PORT= caget …              # empty: no "Unable to find" line
//! EPICS Environment "EPICS_CA_SERVER_PORT" integer fetch failed
//! setting "EPICS_CA_SERVER_PORT" = 5064
//!
//! $ EPICS_CA_SERVER_PORT='  5065' caget …      # sscanf leniency: silent
//! $ EPICS_CA_SERVER_PORT=5070x caget …         # sscanf leniency: silent
//! ```
//!
//! Pre-fix Rust took all of `3000`, `70000` (as a parse failure), `-1`, `0`
//! silently — a port below `IPPORT_USERRESERVED` was *honoured* — and
//! rejected the two lenient forms C accepts. Nothing was ever printed.
//!
//! The effective port is observed by pointing `EPICS_CA_ADDR_LIST` at a
//! bare `127.0.0.1` (so the resolved default port is what the SEARCH is
//! addressed to) and watching a UDP probe socket on an ephemeral port.
//! Never bind 5064 here: the rejected cases are proven by the SEARCH *not*
//! arriving at the probe.

use std::net::UdpSocket;
use std::process::Command;
use std::time::Duration;

/// UDP probe on an ephemeral port, with a short read timeout.
fn probe() -> (UdpSocket, u16) {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind probe socket");
    let port = sock.local_addr().unwrap().port();
    sock.set_read_timeout(Some(Duration::from_millis(1500)))
        .unwrap();
    // The kernel's ephemeral range is well above IPPORT_USERRESERVED (5000),
    // so the probe port is always a legal EPICS port.
    assert!(port > 5000, "ephemeral probe port {port} must be > 5000");
    (sock, port)
}

/// Run `caget-rs` for a PV nobody serves, with `EPICS_CA_SERVER_PORT=value`,
/// and return its stderr. The SEARCH goes to `127.0.0.1:<resolved port>`.
fn caget_stderr(var: &str, value: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .args(["-w", "0.5", "R17-16:NO-SUCH-PV"])
        .env(var, value)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .output()
        .expect("spawn caget-rs");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Discard datagrams left over from an earlier case — `caget-rs` retries
/// the SEARCH within its wait window, so the probe holds a backlog.
fn drain(sock: &UdpSocket) {
    sock.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 1024];
    while sock.recv_from(&mut buf).is_ok() {}
    sock.set_nonblocking(false).unwrap();
}

/// Same, but the addr list is the bare probe host: the resolved default
/// port decides where the SEARCH lands. Returns (stderr, search_seen).
fn caget_search_lands_on(value: &str, probe: &UdpSocket) -> (String, bool) {
    drain(probe);
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .args(["-w", "0.5", "R17-16:NO-SUCH-PV"])
        .env("EPICS_CA_SERVER_PORT", value)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .output()
        .expect("spawn caget-rs");
    let mut buf = [0u8; 1024];
    let seen = probe.recv_from(&mut buf).is_ok();
    (String::from_utf8_lossy(&out.stderr).into_owned(), seen)
}

#[test]
fn out_of_range_server_port_prints_cs_two_lines_and_defaults() {
    // 3000 (below the 5000 floor), 70000 (above USHRT_MAX), -1 and 0.
    for bad in ["3000", "70000", "-1", "0"] {
        let err = caget_stderr("EPICS_CA_SERVER_PORT", bad);
        for line in [
            "EPICS Environment \"EPICS_CA_SERVER_PORT\" out of range",
            "Setting \"EPICS_CA_SERVER_PORT\" = 5064",
        ] {
            assert!(
                err.contains(line),
                "EPICS_CA_SERVER_PORT={bad}: missing {line:?} in stderr:\n{err}"
            );
        }
    }
}

#[test]
fn non_numeric_server_port_prints_cs_three_lines() {
    let err = caget_stderr("EPICS_CA_SERVER_PORT", "abc");
    for line in [
        "Unable to find an integer in EPICS_CA_SERVER_PORT=abc",
        "EPICS Environment \"EPICS_CA_SERVER_PORT\" integer fetch failed",
        "setting \"EPICS_CA_SERVER_PORT\" = 5064",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr:\n{err}");
    }
}

/// An empty value is NULL to `envGetConfigParamPtr`, so C skips the
/// "Unable to find an integer" line and prints only the fetch-failed pair.
#[test]
fn empty_server_port_skips_the_unable_to_find_line() {
    let err = caget_stderr("EPICS_CA_SERVER_PORT", "");
    assert!(
        !err.contains("Unable to find an integer"),
        "empty value must not print the sscanf line:\n{err}"
    );
    for line in [
        "EPICS Environment \"EPICS_CA_SERVER_PORT\" integer fetch failed",
        "setting \"EPICS_CA_SERVER_PORT\" = 5064",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr:\n{err}");
    }
}

/// C `udpiiu` resolves the repeater port once, in its constructor
/// (`udpiiu.cpp:168`), and every registration retry sends to the stored
/// `this->repeaterPort` — so a client prints the diagnostics exactly once,
/// not once per registration attempt.
///
/// `feature = "client"` only, unlike its `EPICS_CA_SERVER_PORT` siblings above,
/// and that is the whole condition: `CaClient::new_with_config` resolves the
/// repeater port under the feature alone, so an `exec_backend` client prints
/// this pair exactly as a `tokio_backend` one does — measured directly, not
/// inferred. `client-core` is the build that really has no repeater port to
/// resolve, because it binds no UDP socket for a repeater to fan beacons into,
/// and it still builds `caget-rs` and this file.
///
/// The gate read `ca_beacon_monitor` while the resolution was gated that way.
/// That cfg also asserts `tokio_backend`, which is a statement about the
/// executor rather than about whether an operator configured a repeater port,
/// so it silently excused the one target where a rejected value is hardest to
/// notice. Per-test rather than per-file: the server-port cases here hold on
/// every build.
#[cfg(feature = "client")]
#[test]
fn out_of_range_repeater_port_prints_cs_two_lines_once() {
    let err = caget_stderr("EPICS_CA_REPEATER_PORT", "3000");
    for line in [
        "EPICS Environment \"EPICS_CA_REPEATER_PORT\" out of range",
        "Setting \"EPICS_CA_REPEATER_PORT\" = 5065",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr:\n{err}");
        assert_eq!(
            err.matches(line).count(),
            1,
            "one resolution per client process, C prints {line:?} once:\n{err}"
        );
    }
}

/// `sscanf("%ld")` leniency: leading whitespace and a trailing garbage
/// suffix are accepted silently, and the port they yield is the one the
/// SEARCH is actually addressed to.
#[test]
fn lenient_values_are_silent_and_take_effect() {
    let (sock, port) = probe();

    for value in [format!("  {port}"), format!("{port}x")] {
        let (err, seen) = caget_search_lands_on(&value, &sock);
        assert!(
            !err.contains("out of range") && !err.contains("integer fetch failed"),
            "EPICS_CA_SERVER_PORT={value:?} must resolve silently, stderr:\n{err}"
        );
        assert!(
            seen,
            "EPICS_CA_SERVER_PORT={value:?} must address the SEARCH to {port}"
        );
    }
}

/// The rejected values do not reach the wire: the SEARCH is addressed to
/// the compiled default (5064), not to the rejected port.
#[test]
fn rejected_values_do_not_reach_the_probe_port() {
    let (sock, port) = probe();

    // Sanity: the probe port itself is honoured, so a miss below is a
    // rejection and not a broken probe.
    let (_, seen) = caget_search_lands_on(&port.to_string(), &sock);
    assert!(seen, "probe port {port} must receive the SEARCH");

    // A value that parses to `port` but is out of range cannot exist, so
    // rejection is shown with values whose *lenient* parse would have been
    // honoured pre-fix — the SEARCH must go to 5064 instead of {port}.
    for bad in ["3000", "0"] {
        let (err, seen) = caget_search_lands_on(bad, &sock);
        assert!(
            err.contains("out of range"),
            "EPICS_CA_SERVER_PORT={bad} must be diagnosed, stderr:\n{err}"
        );
        assert!(
            !seen,
            "EPICS_CA_SERVER_PORT={bad} must not address the SEARCH to the probe port {port}"
        );
    }
}
