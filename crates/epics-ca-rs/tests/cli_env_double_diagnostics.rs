//! Regression tests (R15-17): a `double`-valued EPICS env var that
//! `epicsScanDouble` rejects is DIAGNOSED, not silently defaulted.
//!
//! C prints two lines from the call site on top of the one
//! `envGetDoubleConfigParam` itself prints (`envSubr.c:205-206`):
//!
//! ```text
//! Unable to find a real number in EPICS_CA_CONN_TMO=abc     <- envSubr.c
//! EPICS "EPICS_CA_CONN_TMO" double fetch failed             <- cac.cpp:192
//! Defaulting "EPICS_CA_CONN_TMO" = 30.000000                <- cac.cpp:193
//! ```
//!
//! and, for the search period (`udpiiu.cpp:86-89`):
//!
//! ```text
//! Unable to find a real number in EPICS_CA_MAX_SEARCH_PERIOD=abc
//! EPICS "EPICS_CA_MAX_SEARCH_PERIOD" wasn't a real number
//! Setting "EPICS_CA_MAX_SEARCH_PERIOD" = 300.000000 seconds
//! ```
//!
//! The port resolved both silently. The values themselves (default on
//! reject, `0x10` → 16, `1e400` → reject) are pinned by the per-boundary
//! unit tests in `client::transport` and `server::tcp`.

mod common;

use std::process::Command;

// Both are used only by the one `softioc-rs` case below, which the
// reactor-free backend does not build.
#[cfg(tokio_backend)]
use common::LineCollector;
#[cfg(tokio_backend)]
use std::process::Stdio;

/// stderr of `caget-rs` for a PV that does not exist — the tool still
/// builds its client, which is where both variables are resolved. `-w 0.1`
/// keeps the failed search short.
fn caget_stderr(var: &str, value: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_caget-rs"))
        .args(["-w", "0.1", "R15-17:NO-SUCH-PV"])
        .env(var, value)
        // Keep the run off any real network: no broadcast, no name server.
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
        .output()
        .expect("spawn caget-rs");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn conn_tmo_reject_prints_cs_three_lines() {
    let err = caget_stderr("EPICS_CA_CONN_TMO", "abc");
    for line in [
        "Unable to find a real number in EPICS_CA_CONN_TMO=abc",
        "EPICS \"EPICS_CA_CONN_TMO\" double fetch failed",
        "Defaulting \"EPICS_CA_CONN_TMO\" = 30.000000",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr:\n{err}");
    }
}

/// An ERANGE value is a reject too — `strtod("1e400")` sets `errno`, so
/// `epicsParseDouble` fails where the bare `inf` word succeeds.
#[test]
fn conn_tmo_erange_prints_the_same_lines() {
    let err = caget_stderr("EPICS_CA_CONN_TMO", "1e400");
    assert!(
        err.contains("Unable to find a real number in EPICS_CA_CONN_TMO=1e400")
            && err.contains("EPICS \"EPICS_CA_CONN_TMO\" double fetch failed"),
        "stderr:\n{err}"
    );
}

/// `inf` PARSES — `strtod` leaves errno clear, so neither parse
/// diagnostic is due — but it fails C's `finite()` guard
/// (`cac.cpp:189-195` @`a8180003e`), so C defaults and says so, and so do
/// we. It must also not abort the tool, which is what it used to do.
#[test]
fn conn_tmo_inf_parses_but_is_refused_like_c() {
    let err = caget_stderr("EPICS_CA_CONN_TMO", "inf");
    assert!(
        !err.contains("double fetch failed") && !err.contains("Unable to find a real number"),
        "inf is a valid double for strtod; no parse diagnostic is due:\n{err}"
    );
    assert!(
        err.contains("\"EPICS_CA_CONN_TMO\" = inf is not a positive period"),
        "a non-finite period is refused out loud, as C refuses it:\n{err}"
    );
    assert!(
        !err.contains("cannot convert float seconds to Duration"),
        "the from_secs_f64 panic must be gone:\n{err}"
    );
}

/// Finite and enormous passes C's `finite()` guard, so C obeys it: the
/// watchdog simply never fires. No diagnostic, no panic, no overflow when
/// the deadline is built from `Duration::MAX`.
#[test]
fn conn_tmo_finite_enormous_is_obeyed_silently() {
    let err = caget_stderr("EPICS_CA_CONN_TMO", "1e300");
    assert!(
        !err.contains("is not a positive period") && !err.contains("double fetch failed"),
        "1e300 is finite and positive, so C obeys it:\n{err}"
    );
    assert!(
        !err.contains("overflow when adding duration to instant"),
        "the deadline built from Duration::MAX must saturate:\n{err}"
    );
}

/// Hex floats are `strtod`'s to accept: no diagnostic for `0x10`.
#[test]
fn conn_tmo_hex_float_is_accepted_silently() {
    let err = caget_stderr("EPICS_CA_CONN_TMO", "0x10");
    assert!(
        !err.contains("Unable to find a real number"),
        "strtod(\"0x10\") is 16.0:\n{err}"
    );
}

#[test]
fn max_search_period_reject_prints_cs_lines() {
    let err = caget_stderr("EPICS_CA_MAX_SEARCH_PERIOD", "abc");
    for line in [
        "Unable to find a real number in EPICS_CA_MAX_SEARCH_PERIOD=abc",
        "EPICS \"EPICS_CA_MAX_SEARCH_PERIOD\" wasn't a real number",
        "Setting \"EPICS_CA_MAX_SEARCH_PERIOD\" = 300.000000 seconds",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr:\n{err}");
    }
}

/// C `getNTimers` (`udpiiu.cpp:96-111`): a period past the 18-rung search-timer
/// ladder is named and clamped to `(1 << 17) * 32e-3` seconds. The compiled
/// `caget` prints exactly these two lines for `=100000`.
#[test]
fn max_search_period_above_the_timer_ladder_is_named() {
    let err = caget_stderr("EPICS_CA_MAX_SEARCH_PERIOD", "100000");
    for line in [
        "\"EPICS_CA_MAX_SEARCH_PERIOD\" out of range (high)",
        "Setting \"EPICS_CA_MAX_SEARCH_PERIOD\" = 4194.304000 seconds",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr:\n{err}");
    }
}

/// The rung below the boundary: `8388.607 < 0.032 * 2^18`, so the ladder still
/// holds it and the compiled `caget` says nothing. One tick under the boundary
/// must not gain a diagnostic C does not print.
#[test]
fn max_search_period_just_inside_the_ladder_is_silent() {
    let err = caget_stderr("EPICS_CA_MAX_SEARCH_PERIOD", "8388.607");
    assert!(
        !err.contains("out of range"),
        "8388.607 s fits C's 18-rung ladder; no diagnostic is due:\n{err}"
    );
}

/// C `udpiiu.cpp:78-83`: a value under the 60 s lower limit is named and
/// clamped, not silently raised.
#[test]
fn max_search_period_below_lower_limit_is_named() {
    let err = caget_stderr("EPICS_CA_MAX_SEARCH_PERIOD", "30");
    for line in [
        "\"EPICS_CA_MAX_SEARCH_PERIOD\" out of range (low)",
        "Setting \"EPICS_CA_MAX_SEARCH_PERIOD\" = 60.000000 seconds",
    ] {
        assert!(err.contains(line), "missing {line:?} in stderr:\n{err}");
    }
}

/// R16-18: the beacon-period diagnostic is printed ONCE, as C prints it.
///
/// C reads `EPICS_CAS_BEACON_PERIOD` in the single beacon thread
/// (`online_notify.c:52-64`), so the compiled `softIoc` prints one
/// "float fetch failed" pair for a bad value. The port resolved the server's
/// UDP config from three points in startup (`bind_tcp_listeners`,
/// `bind_sockets`, `CaServer::run`) and so printed the pair three times.
// The only case here that spawns `softioc-rs` rather than `caget-rs`; that
// binary refuses on `exec_backend`, where the async CA server it starts does
// not exist. The other twelve read the client's own diagnostics and stay.
#[cfg(tokio_backend)]
#[test]
fn beacon_period_reject_is_diagnosed_once_per_process() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["-S", "--pv", "R16-18:PV:double:1.0"])
        // Keep the IOC off the network: loopback interface, loopback beacon
        // target, and a port the harness picks nothing else on.
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_SERVER_PORT", "0")
        .env("EPICS_CAS_BEACON_PERIOD", "garbage")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softioc-rs");

    // Startup is what prints, and the "CA server:" banner is emitted after
    // the one-shot env resolution (`addr_list::from_env` in `run()`) — every
    // startup point that historically duplicated the diagnostic precedes it.
    // Wait for the banner itself (a fixed sleep races startup under load),
    // then take the IOC down and judge everything it wrote.
    let err_lines = LineCollector::spawn(child.stderr.take().expect("piped stderr"));
    assert!(
        err_lines.wait_for(budget::FACT_BUDGET, |t| {
            t.contains("CA server: UDP search on port")
        }),
        "softioc-rs never printed its startup banner; stderr: {:?}",
        err_lines.text()
    );
    let _ = child.kill();
    let _ = child.wait();
    let err = err_lines.into_text();

    for line in [
        "Unable to find a real number in EPICS_CAS_BEACON_PERIOD=garbage",
        "EPICS \"EPICS_CAS_BEACON_PERIOD\" float fetch failed",
        "Setting \"EPICS_CAS_BEACON_PERIOD\" = 15.000000",
    ] {
        assert_eq!(
            err.matches(line).count(),
            1,
            "{line:?} must appear exactly once (C prints it once); stderr:\n{err}"
        );
    }
}

/// R16-19: a non-positive `EPICS_CA_CONN_TMO` is refused, and SAID so.
///
/// This is the port's one documented deviation on this knob. C stores the raw
/// value (`cac.cpp:188-194`) and its connection watchdog then fires on every
/// check — the compiled `camonitor` wrote 177_182 stderr lines in 3 s with
/// `=-5`. The port keeps the 30 s default instead, and tells the operator that
/// the value they set is not the one in force. The test stays bounded: `caget`
/// with `-w 0.1` never reaches a circuit, so nothing can flood.
#[test]
fn conn_tmo_non_positive_is_refused_out_loud() {
    for value in ["-5", "0"] {
        let err = caget_stderr("EPICS_CA_CONN_TMO", value);
        assert!(
            err.contains("\"EPICS_CA_CONN_TMO\"") && err.contains("not a positive period"),
            "EPICS_CA_CONN_TMO={value} must be diagnosed, not silently overridden:\n{err}"
        );
    }
}

/// The flip side: a period C accepts and the port honours is silent.
#[test]
fn conn_tmo_positive_period_is_silent() {
    let err = caget_stderr("EPICS_CA_CONN_TMO", "10.5");
    assert!(
        !err.contains("not a positive period") && !err.contains("double fetch failed"),
        "10.5 s is a valid period; no diagnostic is due:\n{err}"
    );
}

#[cfg(tokio_backend)]
use common::budget;
