//! `%{` byte for byte against C: each line of
//! `tests/oracle/conv/enum_<group>.in` runs here as `oracle.cc` runs it on
//! C StreamDevice and must print the same line as `enum_<group>.out`;
//! `enum_debug` with streamDebug 2, its `file:line:` prefixes left out.

use std::fmt::Write;

use stream_rs::StreamBuffer;
use stream_rs::converter::{find, parse_format};
use stream_rs::cstr::CPtr;
use stream_rs::error::{
    capture_stderr, set_get_thread_name_function, set_stream_debug, set_stream_debug_colored,
    set_stream_msg_time_stamped, take_captured,
};
use stream_rs::format::StreamFormat;
use stream_rs::protocol::FormatType;

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(h: &str) -> Vec<u8> {
    if h.starts_with('.') {
        return Vec::new();
    }
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// One `oracle.cc` line.
fn run(case: &str) -> String {
    let f: Vec<&str> = case.split('\t').collect();
    // A \xNN in the format is that byte.
    let mut src = Vec::new();
    let mut rest = f[1];
    while let Some((head, tail)) = rest.split_once("\\x") {
        src.extend_from_slice(head.as_bytes());
        src.push(u8::from_str_radix(&tail[..2], 16).unwrap());
        rest = &tail[2..];
    }
    src.extend_from_slice(rest.as_bytes());
    src.push(0);
    let mut source = CPtr::new(&src);
    let mut fmt = StreamFormat::default();
    let mut info = StreamBuffer::new();
    capture_stderr();
    // `cl` is an input format that its `=` turns into an output one.
    let format_type = if f[0] != "pl" {
        FormatType::ScanFormat
    } else {
        FormatType::PrintFormat
    };
    let t = parse_format(&mut source, format_type, &mut fmt, &mut info);
    let mut line = format!(
        "parse={t} rest={} infolen={} info={}",
        hex(source.cstr()),
        info.length(),
        hex(info.bytes())
    );
    if t != 0 {
        let mut info_bytes = info.bytes().to_vec();
        info_bytes.push(0);
        fmt.type_ = t;
        fmt.infolen = info.length() as u64;
        fmt.info = &info_bytes;
        let c = &find(fmt.conv).unwrap().converter;
        if f[0] == "sl" {
            let mut val = 0x5a5a;
            let input = [unhex(f[2]).as_slice(), &[0]].concat();
            let n = c.scan_long(&fmt, CPtr::new(&input), &mut val);
            write!(line, " n={n} val={val}").unwrap();
        } else {
            let mut out = StreamBuffer::new();
            let ok = c.print_long(&fmt, &mut out, f[2].parse().unwrap());
            write!(line, " ok={} out={}", ok as i32, hex(out.bytes())).unwrap();
        }
    }
    write!(line, " err={}", hex(&take_captured())).unwrap();
    line
}

/// The debug lines without their `file:line: ` prefix.
fn unprefixed(line: &str) -> String {
    let (head, err) = line.split_once(" err=").unwrap();
    let text = String::from_utf8(unhex(err)).unwrap();
    let text: String = text
        .split_inclusive('\n')
        .map(|l| match l.split_once(": ") {
            Some((p, rest)) if p.contains(".cc:") || p.contains(".rs:") => rest,
            _ => l,
        })
        .collect();
    format!("{head} err={}", hex(text.as_bytes()))
}

fn check(group: &str, debug: i32) {
    set_stream_debug_colored(false);
    set_stream_msg_time_stamped(0);
    set_get_thread_name_function(None);
    set_stream_debug(debug);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}enum_{group}.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}enum_{group}.out")).unwrap();
    let mut failed = Vec::new();
    for (case, want) in cases.lines().zip(expected.lines()) {
        let (want, got) = (unprefixed(want), unprefixed(&run(case)));
        if got != want {
            failed.push(format!(
                "{}\n   C: {want}\n  rs: {got}",
                case.replace('\t', " ")
            ));
        }
    }
    assert_eq!(cases.lines().count(), expected.lines().count());
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// `parse`, and `printLong` on what it built.
#[test]
fn enum_parse_and_print() {
    check("parse", 0);
}

#[test]
fn enum_scan() {
    check("scan", 0);
}

#[test]
fn enum_debug_lines() {
    check("debug", 2);
}
