//! `%R` byte for byte against C: each line of
//! `tests/oracle/conv/rawfloat.in` runs here as `oracle.cc` runs it on C
//! StreamDevice and must print the same line as `rawfloat.out`.

use std::fmt::Write;
use std::sync::Once;

use stream_rs::StreamBuffer;
use stream_rs::converter::{find, parse_format};
use stream_rs::cstr::CPtr;
use stream_rs::error::{
    capture_stderr, set_get_thread_name_function, set_stream_debug_colored,
    set_stream_msg_time_stamped, take_captured,
};
use stream_rs::format::StreamFormat;
use stream_rs::protocol::FormatType;

fn setup() {
    static SETUP: Once = Once::new();
    SETUP.call_once(|| {
        set_stream_debug_colored(false);
        set_stream_msg_time_stamped(0);
        set_get_thread_name_function(None);
    });
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
    let src = [f[1].as_bytes(), &[0]].concat();
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
            let n = c.scan_double(&fmt, CPtr::new(&input), &mut val);
            write!(line, " n={n} val={:016x}", val.to_bits()).unwrap();
        }
    }
    write!(line, " err={}", hex(&take_captured())).unwrap();
    line
}

/// The `rawfloat.in` lines `keep` selects, each with C's line cut by `cut`.
fn check(keep: impl Fn(&str) -> bool, cut: impl Fn(&str) -> String) {
    setup();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}rawfloat.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}rawfloat.out")).unwrap();
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
fn parse_takes_width_0_4_or_8() {
    check(|_| true, parse_part);
}

#[test]
fn print_double_matches_c() {
    check(|case| case.starts_with("pd\t"), |line| line.to_string());
}

#[test]
fn scan_double_matches_c() {
    check(|case| case.starts_with("sd\t"), |line| line.to_string());
}

/// C reads past a short input; the port reads zeros and still reports the
/// full width, which StreamCore refuses.
#[test]
fn a_short_input_reads_zeros() {
    setup();
    let line = run("sd\t%8R\t3ff8");
    assert!(line.contains(" n=8 val=3ff8000000000000 "), "{line}");
    let line = run("sd\t%#R\t.");
    assert!(line.contains(" n=4 val=0000000000000000 "), "{line}");
}
