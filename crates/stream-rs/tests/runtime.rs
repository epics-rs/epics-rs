//! Protocol execution against C StreamDevice: each `tests/oracle/rt/<case>.log`
//! is what C's StreamCore printed (debug lines and errors) while a record
//! ran the case's protocol on a device (see `tests/oracle/rt/run-rt.sh`).
//!
//! The log is replayed: a line naming an entry into the core from outside
//! (the record layer starting a protocol, the bus answering a request,
//! input arriving)
//! makes the test call the port's entry, when the port has not printed that
//! line itself; every line is then compared with the port's output.

mod common;

use common::{Core, MOCK, MockConfig, TestHooks, Value};
use std::sync::atomic::Ordering;
use stream_rs::bus::{Client, ReadInput, StreamIoStatus};

use stream_rs::core::{STREAM_ERROR_DEAD_TIME, StartMode, StreamCoreLock};
use stream_rs::error::{capture_stderr, set_stream_debug, take_captured};
use stream_rs::protocol;

fn status(s: &str) -> StreamIoStatus {
    use StreamIoStatus::*;
    [
        StreamIoSuccess,
        StreamIoTimeout,
        StreamIoNoReply,
        StreamIoEnd,
        StreamIoFault,
    ]
    .into_iter()
    .find(|x| x.to_str() == s)
    .unwrap_or_else(|| panic!("status {s}"))
}

/// The entry a C log line names, played on `core`; false when the line
/// names none.
fn play(core: &Core, line: &str) -> bool {
    let Some(call) = line.strip_prefix("StreamCore::") else {
        return false;
    };
    let Some((entry, args)) = call.split_once("(rec") else {
        return false;
    };
    let arg = args
        .strip_prefix(", ")
        .and_then(|a| a.split([')', ' ']).next())
        .unwrap_or("");
    match entry {
        "startProtocol" => {
            let mode = match arg {
                "startMode=StartNormal" => StartMode::StartNormal,
                "startMode=StartInit" => StartMode::StartInit,
                "startMode=StartAsync" => StartMode::StartAsync,
                _ => panic!("{line}"),
            };
            core.start_protocol(mode);
        }
        "lockCallback" => core.lock_callback(status(arg)),
        "writeCallback" => core.write_callback(status(arg)),
        "timerCallback" => core.timer_callback(),
        "execCallback" => core.exec_callback(status(arg)),
        "eventCallback" => core.event_callback(status(arg)),
        "connectCallback" => core.connect_callback(status(arg)),
        "disconnectCallback" => core.disconnect_callback(status(arg)),
        "readCallback" => {
            // readCallback(rec, S input="...", size=N)
            let input = args
                .split_once(" input=\"")
                .and_then(|(_, a)| a.rsplit_once("\", size="))
                .unwrap_or_else(|| panic!("{line}"));
            let input = unexpand(input.0);
            // C's size_t: a take-back of 1 prints as 18446744073709551615
            let size = input_size(line).strip_suffix(')').unwrap_or("");
            let size = size.parse::<u64>().unwrap_or_else(|_| panic!("{line}")) as i64;
            if size < 0 {
                assert!(input.is_empty(), "{line}");
                core.read_callback(
                    status(arg),
                    ReadInput::TakeBack(size.unsigned_abs() as usize),
                );
            } else {
                assert_eq!(size as usize, input.len(), "{line}");
                core.read_callback(status(arg), ReadInput::Bytes(&input));
            }
        }
        _ => return false,
    }
    true
}

/// The `N)` of a line ending `size=N)`.
fn input_size(line: &str) -> &str {
    line.rsplit_once("size=").map_or("", |(_, n)| n)
}

/// Undo `StreamBuffer::expand`: `<hh>` is a byte.
fn unexpand(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'<'
            && i + 3 < b.len()
            && b[i + 3] == b'>'
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// C's log lines, where the port differs by design. C's
/// `scanValue(long&)` debug format ends `failed\\n` (StreamCore.cc), so
/// the line runs on into the next debug line, with that line's thread name
/// and file:line (run-rt.sh cuts it when it is not StreamCore.cc's); the
/// port ends the line.
fn c_log(log: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in log.lines() {
        match line.split_once(" failed\\n") {
            Some((head, next)) if head.starts_with("StreamCore::scanValue(") => {
                out.push(format!("{head} failed"));
                // run-rt.sh has cut a line of another file
                if let Some((_, next)) = next.split_once(' ') {
                    match next.split_once(": ") {
                        Some((at, rest)) if at.starts_with("StreamCore.cc:") => {
                            out.push(rest.to_string())
                        }
                        _ => out.push(next.to_string()),
                    }
                }
            }
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// The port's output as the C log keeps it: core.rs's debug lines and the
/// error lines, without the thread name and file:line prefix.
fn keep(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| {
            let l = l.split_once(' ')?.1;
            match l.split_once(": ") {
                Some((at, rest)) if at.contains(".rs:") => {
                    at.starts_with("core.rs:").then(|| rest.to_string())
                }
                _ => Some(l.to_string()),
            }
        })
        .collect()
}

fn run_case(case: &str, rtype: &str, val: &str, opts: &str) -> Result<(), String> {
    let value = match rtype {
        "longout" => Value::Longout(val.parse().unwrap()),
        "ao" => Value::Ao(val.parse().unwrap()),
        "stringout" => Value::Stringout(val.as_bytes().to_vec()),
        "longin" => Value::Longin(val.parse().unwrap()),
        "ai" => Value::Ai,
        "stringin" => Value::Stringin,
        _ => panic!("record type {rtype}"),
    };
    let opts: Vec<&str> = opts.split(',').collect();
    *MOCK.lock() = MockConfig {
        lock_ok: !opts.contains(&"lock=fail"),
        events: opts.contains(&"events"),
        async_read: opts.contains(&"intr"),
    };
    let core: Core = StreamCoreLock::new("rec", |_| TestHooks {
        value,
        ..Default::default()
    });
    // Stream::initRecord attaches the bus before it parses.
    if !core.attach_bus("P", 0, "") || !common::parse(&core, &format!("{case}.proto"), b"p") {
        return Err(format!("{case}: protocol not loaded"));
    }
    let want = c_log(&std::fs::read_to_string(format!("tests/oracle/rt/{case}.log")).unwrap());
    let mut got = Vec::new();
    set_stream_debug(1);
    capture_stderr();
    let dead_time = opts
        .iter()
        .find_map(|o| o.strip_prefix("deadtime="))
        .map_or(0, |n| n.parse().unwrap());
    STREAM_ERROR_DEAD_TIME.store(dead_time, Ordering::Relaxed);
    let mut starts = 0;
    let mut result = Ok(());
    for (i, line) in want.iter().enumerate() {
        if got.len() <= i {
            if line.starts_with("StreamCore::startProtocol(") {
                starts += 1;
                // run-rt.sh's "thrice": the third run is 2.1 s later.
                if starts == 3 && opts.contains(&"thrice") {
                    std::thread::sleep(std::time::Duration::from_millis(2100));
                }
            }
            if !play(&core, line) {
                result = Err(format!("{case}: rs printed nothing for line {i}: {line}"));
                break;
            }
            got.extend(keep(&take_captured()));
            capture_stderr();
        }
        if got.get(i) != Some(line) {
            result = Err(format!(
                "{case} line {i}:\n  C:  {line}\n  rs: {}",
                got.get(i).map_or("(nothing)", String::as_str)
            ));
            break;
        }
    }
    if result.is_ok() && got.len() > want.len() {
        result = Err(format!("{case}: rs printed more: {:?}", &got[want.len()..]));
    }
    set_stream_debug(0);
    take_captured();
    result
}

/// Run the cases of `tests/oracle/rt/<list>`; the number run.
fn run_cases(list: &str) -> usize {
    common::setup();
    protocol::set_path(Some("tests/oracle/rt"));
    let cases = std::fs::read_to_string(format!("tests/oracle/rt/{list}")).unwrap();
    let mut failed = Vec::new();
    let mut n = 0;
    for line in cases
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let f: Vec<&str> = line.split('\t').collect();
        n += 1;
        if let Err(e) = run_case(f[0], f[1], f[2], f[4]) {
            failed.push(e);
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {n} cases differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
    n
}

#[test]
fn protocols_run_as_c() {
    let n = run_cases("cases.txt");
    assert!(n >= 48, "{n} cases");
}

#[test]
fn events_run_as_c_prints_them() {
    let n = run_cases("nocrun.txt");
    assert!(n >= 3, "{n} cases");
}
