//! `%m` byte for byte against C: each line of
//! `tests/oracle/conv/mantexp_<group>.in` runs here as `oracle.cc` runs it
//! on C StreamDevice and must print the same line as `mantexp_<group>.out`;
//! `mantexp_debug` with streamDebug 2, its `file:line:` prefixes left out.

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
    // `cd` is an input format that its `=` turns into an output one.
    let format_type = if f[0] != "pd" {
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
        if f[0] == "sd" {
            let mut val = 1234.5f64;
            let input = [unhex(f[2]).as_slice(), &[0]].concat();
            let n = c.scan_double(&fmt, CPtr::new(&input), &mut val);
            write!(line, " n={n} val={:016x}", val.to_bits()).unwrap();
        } else {
            let mut out = StreamBuffer::new();
            let ok = c.print_double(&fmt, &mut out, f[2].parse().unwrap());
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
    check_with(group, debug, |_, c_line| c_line.to_string());
}

/// `fixup` turns a C line into the port's where they differ by design.
fn check_with(group: &str, debug: i32, fixup: impl Fn(&str, &str) -> String) {
    set_stream_debug_colored(false);
    set_stream_msg_time_stamped(0);
    set_get_thread_name_function(None);
    set_stream_debug(debug);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}mantexp_{group}.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}mantexp_{group}.out")).unwrap();
    let mut failed = Vec::new();
    for (case, want) in cases.lines().zip(expected.lines()) {
        let (want, got) = (unprefixed(&fixup(case, want)), unprefixed(&run(case)));
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

/// C removes byte 1 and the 'e' (or, with none, the last byte) of
/// `%.*e`: at precision 1 there is no '.', so it drops the 'e' and the
/// exponent's last digit, and inf and nan lose all but a letter. The port
/// removes the '.' and the 'e' only where they are.
fn printed_as_meant(case: &str, c_line: &str) -> String {
    let fixed: &[u8] = match case.split_once('\t').unwrap().1 {
        "%.1m\t5" => b"5+00",
        "%.1m\t15" | "%.1m\t25" => b"2+01",
        "%.1m\t35" => b"4+01",
        "%.1m\t9.5" => b"1+01",
        "%.1m\t0" => b"0+00",
        "%.1m\t-7" => b"-7+00",
        "%.1m\t1e300" => b"1+300",
        "%8.1m\t1" => b"    1+00",
        "%.1m\tinf" | "%m\tinf" => b"inf",
        "%m\t-inf" => b"-inf",
        "%.1m\tnan" | "%m\tnan" | "%m\t-nan" => b"nan",
        _ => return c_line.to_string(),
    };
    let (head, tail) = c_line.split_once(" out=").unwrap();
    let err = tail.split_once(" err=").unwrap().1;
    format!("{head} out={} err={err}", hex(fixed))
}

/// The C line as this libm makes it. 10^23 is a tie between two doubles:
/// glibc's pow rounds it up, so C prints 1/pow(10,23) below 1e-23;
/// macOS's `__exp10`, which clang calls for C's `pow(10.0, x)` as rustc
/// does for `powf`, rounds it to even.
fn as_on_this_libm(case: &str, c_line: &str) -> String {
    if case == "pd\t%.24m\t1" && 10f64.powf(std::hint::black_box(23.0)) == 1e23 {
        let (head, tail) = c_line.split_once(" out=").unwrap();
        let err = tail.split_once(" err=").unwrap().1;
        return format!(
            "{head} out={} err={err}",
            hex(b"100000000000000010737149-23")
        );
    }
    c_line.to_string()
}

/// `parse`, and `printDouble` on what it built.
#[test]
fn mantexp_parse_and_print() {
    check_with("print", 0, |case, c_line| {
        printed_as_meant(case, &as_on_this_libm(case, c_line))
    });
}

#[test]
fn mantexp_scan() {
    check("scan", 0);
}

#[test]
fn mantexp_debug_lines() {
    check_with("debug", 2, printed_as_meant);
}
