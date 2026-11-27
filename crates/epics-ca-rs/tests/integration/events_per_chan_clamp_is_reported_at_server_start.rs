//! `EPICS_CAS_MAX_EVENTS_PER_CHAN` is rs-only (C's `EVENTSPERQUE` is a
//! compile-time 36, `dbEvent.c:61`), so its out-of-range report takes the
//! shape of `rsrv_init`'s `EPICS_CA_MAX_ARRAY_BYTES` line
//! (`caservertask.c:526`): once, at CA server start, through the errlog.

// `softioc-rs` serves through the async CA front-end and refuses at startup
// on `exec_backend`.
#![cfg(tokio_backend)]

use std::process::{Command, Stdio};

/// Boot `softioc-rs` on a one-record st.cmd under `eltc {eltc}` and return
/// (stdout, stderr). stdin is closed, so the shell exits right after the CA
/// server is up and its prompt is printed. The silent cases are silent for a
/// server that never started too; `casr` prints nothing until one has bound
/// (`caservertask.c:911-913`), so its header is the start they need.
fn boot(eltc: u8, events_per_chan: &str) -> (String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("a.db");
    std::fs::write(&db, "record(ai, \"EPQ:AI\") {}\n").expect("write db");
    let st = dir.path().join("st.cmd");
    std::fs::write(
        &st,
        format!(
            "eltc {eltc}\ndbLoadRecords(\"{}\")\niocInit\ncasr\n",
            db.display()
        ),
    )
    .expect("write st.cmd");
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0"])
        .arg(&st)
        .env("EPICS_CAS_MAX_EVENTS_PER_CHAN", events_per_chan)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .output()
        .expect("run softioc-rs");
    assert!(out.status.success(), "{out:?}");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    assert!(
        stdout.contains("Channel Access Server V4.13\n"),
        "the CA server never started:\n{stdout}\n{stderr}"
    );
    (stdout, stderr)
}

fn assert_reported_once(events_per_chan: &str, line: &str) {
    let (_, err) = boot(1, events_per_chan);
    assert_eq!(err.matches(line).count(), 1, "{line:?} once in:\n{err}");
    let at = err.find(line).unwrap();
    let banner = err
        .find("CA server: UDP search on port")
        .expect("CA banner");
    assert!(
        err.find("Starting iocInit").unwrap() < at && at < banner,
        "the line belongs to CA server start:\n{err}"
    );
}

fn assert_silent(events_per_chan: &str) {
    let (out, err) = boot(0, events_per_chan);
    assert!(
        out.contains("epics> "),
        "the boot reached the shell:\n{out}"
    );
    assert!(
        !err.contains("EPICS_CAS_MAX_EVENTS_PER_CHAN"),
        "eltc 0 silences it:\n{err}"
    );
}

#[test]
fn above_the_slot_width_is_rounded_down_out_loud() {
    assert_reported_once(
        "65536",
        "CAS: EPICS_CAS_MAX_EVENTS_PER_CHAN was rounded down to 65535\n",
    );
}

#[test]
fn below_event_entries_is_rounded_up_out_loud() {
    assert_reported_once(
        "3",
        "CAS: EPICS_CAS_MAX_EVENTS_PER_CHAN was rounded up to 4\n",
    );
}

#[test]
fn a_non_integer_is_named_not_silently_defaulted() {
    assert_reported_once(
        "many",
        "CAS: EPICS_CAS_MAX_EVENTS_PER_CHAN was not a positive integer\n",
    );
}

#[test]
fn eltc_0_silences_the_rounded_down_line() {
    assert_silent("65536");
}

#[test]
fn eltc_0_silences_the_rounded_up_line() {
    assert_silent("3");
}

#[test]
fn an_in_range_value_is_silent() {
    let (_, err) = boot(1, "4");
    assert!(!err.contains("EPICS_CAS_MAX_EVENTS_PER_CHAN"), "{err}");
}
