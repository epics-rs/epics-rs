//! `%T` byte for byte against C: each line of
//! `tests/oracle/conv/timestamp.in` runs here as `oracle.cc` runs it on C
//! StreamDevice, in the zone and on the "today" `run-c.sh` gives it, and
//! must print the same line as `timestamp.out`.

use std::fmt::Write;
use std::sync::Once;

use parking_lot::{Mutex, MutexGuard};

use stream_rs::StreamBuffer;
use stream_rs::converter::{find, parse_format};
use stream_rs::cstr::CPtr;
use stream_rs::error::{
    capture_stderr, set_get_thread_name_function, set_stream_debug_colored,
    set_stream_msg_time_stamped, take_captured,
};
use stream_rs::format::StreamFormat;
use stream_rs::protocol::FormatType;
use stream_rs::timestamp_converter::scan_double_at;

/// `run-c.sh`'s `ORACLE_NOW`: the "today" a scan takes the fields its
/// format leaves out from, 2023-11-14 23:13:20 CET.
const ORACLE_NOW: libc::time_t = 1700000000;

/// `run-c.sh`'s zone, and the time functions to this test alone until the
/// guard drops: glibc's mktime picks the side of a repeated DST hour from
/// what earlier calls left process-wide, which in `oracle.cc` are the
/// previous lines only. Every test calls this before any time function
/// runs.
fn zone() -> MutexGuard<'static, ()> {
    static TIME: Mutex<()> = Mutex::new(());
    let time = TIME.lock();
    static ZONE: Once = Once::new();
    ZONE.call_once(|| {
        // SAFETY: every test of this binary waits here before it reads the
        // environment or calls a libc time function.
        unsafe { std::env::set_var("TZ", "CET-1CEST,M3.5.0,M10.5.0/3") };
        set_stream_debug_colored(false);
        set_stream_msg_time_stamped(0);
        set_get_thread_name_function(None);
    });
    time
}

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
    let format_type = match f[0] {
        "pd" => FormatType::PrintFormat,
        "sd" => FormatType::ScanFormat,
        mode => panic!("mode {mode}"),
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
        if f[0] == "pd" {
            let mut out = StreamBuffer::new();
            let ok = c.print_double(&fmt, &mut out, f[2].parse().unwrap());
            write!(line, " ok={} out={}", ok as i32, hex(out.bytes())).unwrap();
        } else {
            let input = [unhex(f[2]).as_slice(), &[0]].concat();
            let mut val = 1234.5f64;
            let n = scan_double_at(ORACLE_NOW, &fmt, CPtr::new(&input), &mut val);
            write!(line, " n={n} val={:016x}", val.to_bits()).unwrap();
        }
    }
    write!(line, " err={}", hex(&take_captured())).unwrap();
    line
}

/// The `timestamp.in` lines `keep` selects, each with C's line cut by `cut`.
fn check(keep: impl Fn(&str) -> bool, cut: impl Fn(&str) -> String) {
    let _time = zone();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}timestamp.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}timestamp.out")).unwrap();
    assert_eq!(cases.lines().count(), expected.lines().count());
    let mut failed = Vec::new();
    let mut n = 0;
    for (case, want) in cases.lines().zip(expected.lines()) {
        if !keep(case) {
            continue;
        }
        let (want, got) = (cut(want), cut(&run(case)));
        if got != want {
            failed.push(format!(
                "{}\n   C: {want}\n  rs: {got}",
                case.replace('\t', " ")
            ));
        }
        n += 1;
    }
    assert!(n > 0);
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// What `parse` left: the line up to the print or scan result, or all of a
/// refused format's line.
fn parse_part(line: &str) -> String {
    match line.find(" ok=").or_else(|| line.find(" n=")) {
        Some(i) => line[..i].to_string(),
        None => line.to_string(),
    }
}

#[test]
fn parse_builds_cs_info_string() {
    check(|_| true, parse_part);
}

/// `timestamp.in` print lines where the port leaves a C bug out: the
/// output C printed and the port's.
const PRINT_BUGS_NOT_COPIED: &[(&str, &str, &str)] = &[
    // glibc pads the unknown "%0<n>f" strftime gets to its width.
    ("%T(%12f)\t0", "0000000000000000000", "000000000000"),
    ("%T(%N)\t0", "00000000000000", "000000000"),
    ("%T(%f)\t0", "00000000000000", "000000000"),
    ("%T(%.S)\t0", "00.00000000000000", "00.000000000"),
    // The pad does not fit C's 40 bytes; the precision C's (int) cast
    // turns negative prints as printf's default 6.
    ("%T(%2147483648f)\t1700000000.5", "", "500000"),
    (
        "%T(%H:%M:%.S)\t1700000000.123456789",
        "23:13:20.00000123456717",
        "23:13:20.123456717",
    ),
    (
        "%T(%3f|%N|%f|%6f)\t1700000000.987654321",
        "988|00000987654209|00000987654209|00987654",
        "988|987654209|987654209|987654",
    ),
    (
        "%T(%12f)\t1700000000.5",
        "0000000500000000000",
        "500000000000",
    ),
    // Nothing carries into the second when the finest field rounds up.
    ("%T(%S.%3f)\t1700000000.9996", "20.000", "21.000"),
    ("%T(%S.%1f|%3f)\t1700000000.9996", "20.0|000", "21.0|000"),
    ("%T(%M:%S.%1f)\t1700000039.96", "13:59.0", "14:00.0"),
    (
        "%T(%H:%M:%S.%3f|%6f)\t1700000059.9999996",
        "23:14:19.000|00000000",
        "23:14:20.000|000000",
    ),
    (
        "%T(%S.%3f|%6f)\t1700000000.9996",
        "20.000|00999600",
        "20.000|999600",
    ),
    // Only the fields before one C refuses count towards the rounding.
    (
        "%T(%M:%S.%1f\\x1b%05|%6f)\t1700000039.96",
        "13:59.0%05|00%06f",
        "14:00.0%05|%06f",
    ),
    // (time_t) truncates toward zero.
    (
        "%T(%F %T.%3f)\t-1.25",
        "1970-01-01 00:59:59.250",
        "1970-01-01 00:59:58.750",
    ),
    (
        "%T(%F %T.%3f)\t-0.25",
        "1970-01-01 01:00:00.250",
        "1970-01-01 00:59:59.750",
    ),
];

fn print_bug(case: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    PRINT_BUGS_NOT_COPIED
        .iter()
        .find(|(c, _, _)| case.strip_prefix("pd\t") == Some(c))
}

fn out_text(line: &str) -> String {
    let out = &line[line.find(" out=").unwrap() + 5..line.find(" err=").unwrap()];
    String::from_utf8(unhex(out)).unwrap()
}

#[test]
fn print_double_matches_c() {
    check(
        |case| case.starts_with("pd\t") && print_bug(case).is_none(),
        |line| line.to_string(),
    );
}

#[test]
fn print_double_leaves_out_cs_bugs() {
    let _time = zone();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}timestamp.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}timestamp.out")).unwrap();
    let mut seen = 0;
    for (case, want) in cases.lines().zip(expected.lines()) {
        let Some(&(_, c_out, rs_out)) = print_bug(case) else {
            continue;
        };
        assert_eq!(out_text(want), c_out, "{case}");
        assert_eq!(out_text(&run(case)), rs_out, "{case}");
        seen += 1;
    }
    assert_eq!(seen, PRINT_BUGS_NOT_COPIED.len());
}

/// C's `strchr(fracbuffer, '.') + 1` is NULL + 1 for `%0f`.
#[test]
fn a_zero_digit_fraction_prints_nothing() {
    let _time = zone();
    assert_eq!(out_text(&run("pd\t%T(%S.%0f)\t1700000000.5")), "20.");
}

/// `timestamp.in` scan lines where the port leaves a C bug out: the input
/// as text, then the value C scanned and the port's.
const SCAN_BUGS_NOT_COPIED: &[(&str, &str, f64, f64)] = &[
    // A 2-digit year: C reads 00..68 as 19xx and 69..99 as 20xx.
    ("%T(%d %b %Y)", "05 janx 2023", -2208646800.0, 947026800.0),
    ("%T(%D)", "11/14/23", -1455843600.0, 1699916400.0),
    ("%T(%x)", "05/14/99", 4082392800.0, 926632800.0),
    // %C adds 100 * century to a year counted from 1900.
    (
        "%T(%y %C %m %d)",
        "23 20 05 14",
        61642159200.0,
        1684015200.0,
    ),
    // The month table lacks "october": oct fails, nov and dec read a month
    // early.
    ("%T(%d %b %Y)", "05 oct 2023", f64::NAN, 1696456800.0),
    ("%T(%d %b %Y)", "05 nov 2023", 1696456800.0, 1699138800.0),
    ("%T(%d %b %Y)", "05 dec 2023", 1699138800.0, 1701730800.0),
    ("%T(%d %B %Y)", "05 october 2023", f64::NAN, 1696456800.0),
    // %z is applied the wrong way round: 12:00 +0100 is 11:00Z, not 13:00Z.
    // The last three also carry the hour past midnight either way.
    (
        "%T(%F %H:%M %z)",
        "2023-11-14 12:00 +0100",
        1699966800.0,
        1699959600.0,
    ),
    (
        "%T(%F %H:%M %z)",
        "2023-11-14 23:30 -0100",
        1700001000.0,
        1700008200.0,
    ),
    (
        "%T(%F %H:%M %z)",
        "2023-11-14 23:30 -0200",
        1699997400.0,
        1700011800.0,
    ),
    (
        "%T(%F %H:%M %z)",
        "2023-11-14 00:30 +0200",
        1699929000.0,
        1699914600.0,
    ),
    // A zone in the format is applied the same wrong way.
    (
        "%T(%F %H:%M%-0200)",
        "2023-07-14 12:00",
        1689328800.0,
        1689343200.0,
    ),
    (
        "%T(%F %H:%M%+0130)",
        "2023-07-14 12:00",
        1689341400.0,
        1689330600.0,
    ),
    // A fraction's leading zeros are dropped: .050 read as .5.
    ("%T(%s.%3f)", "1700000000.050", 1700000000.5, 1700000000.05),
    // Digits past nanoseconds are kept and overflow them.
    (
        "%T(%s.%12f)",
        "0.123456789123",
        1.9122772780000001,
        123456789.0 * 1e-9,
    ),
    // %s is cut to int.
    ("%T(%s)", "4102444800", -192522496.0, 4102444800.0),
];

fn scan_bug(case: &str) -> Option<&'static (&'static str, &'static str, f64, f64)> {
    let f: Vec<&str> = case.split('\t').collect();
    if f[0] != "sd" {
        return None;
    }
    SCAN_BUGS_NOT_COPIED
        .iter()
        .find(|(fmt, input, _, _)| f[1] == *fmt && unhex(f[2]) == input.as_bytes())
}

/// A scan line's value, NaN when it failed.
fn scanned(line: &str) -> f64 {
    let n: isize = line[line.find(" n=").unwrap() + 3..line.find(" val=").unwrap()]
        .parse()
        .unwrap();
    let bits = &line[line.find(" val=").unwrap() + 5..line.find(" err=").unwrap()];
    match n {
        ..0 => f64::NAN,
        _ => f64::from_bits(u64::from_str_radix(bits, 16).unwrap()),
    }
}

fn same(a: f64, b: f64) -> bool {
    a == b || a.is_nan() && b.is_nan()
}

#[test]
fn scan_double_matches_c() {
    check(
        |case| case.starts_with("sd\t") && scan_bug(case).is_none(),
        |line| line.to_string(),
    );
}

#[test]
fn scan_double_leaves_out_cs_bugs() {
    let _time = zone();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}timestamp.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}timestamp.out")).unwrap();
    let mut seen = 0;
    for (case, want) in cases.lines().zip(expected.lines()) {
        let Some(&(_, _, c_val, rs_val)) = scan_bug(case) else {
            continue;
        };
        let got = run(case);
        assert!(same(scanned(want), c_val), "{case}: C {want}");
        assert!(same(scanned(&got), rs_val), "{case}: rs {got}");
        assert_eq!(parse_part(&got), parse_part(want), "{case}");
        seen += 1;
    }
    assert_eq!(seen, SCAN_BUGS_NOT_COPIED.len());
}

/// C's loop `while (i < 100000000) i *= 10` never ends on an all-zero
/// fraction, so there is no oracle line.
#[test]
fn an_all_zero_fraction_scans() {
    let _time = zone();
    let line = run(&format!("sd\t%T(%s.%3f)\t{}", hex(b"1700000000.000")));
    assert_eq!(scanned(&line), 1700000000.0, "{line}");
    let line = run(&format!("sd\t%T(%s.%f)\t{}", hex(b"0.000")));
    assert_eq!(scanned(&line), 0.0, "{line}");
}

/// A rounded fraction carries through every field of the time.
#[test]
fn a_rounded_fraction_carries_into_the_second() {
    let _time = zone();
    // 2023-12-31 23:59:59.9996 CET
    let line = run("pd\t%T(%Y-%m-%d %H:%M:%S.%3f)\t1704063599.9996");
    assert_eq!(out_text(&line), "2024-01-01 00:00:00.000");
    let line = run("pd\t%T(%F %T.%3f)\t-0.0004");
    assert_eq!(out_text(&line), "1970-01-01 01:00:00.000");
    let line = run("pd\t%T(%S.%2f)\t1704063599.994");
    assert_eq!(out_text(&line), "59.99");
}

/// More than 12 fraction digits overflow C's buffer; rs prints 12.
#[test]
fn fraction_digits_stop_at_12() {
    let _time = zone();
    let line = run("pd\t%T(%13f)\t1700000000.5");
    assert_eq!(out_text(&line), "500000000000");
    let line = run("pd\t%T(%2147483647f|%S)\t1700000000.5");
    assert_eq!(out_text(&line), "500000000000|20");
}
