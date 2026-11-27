//! RegexpConverter.cc byte for byte against C: each line of
//! `tests/oracle/regexp/<group>.in` runs here as `oracle.cc` runs it on C
//! StreamDevice built with PCRE, and must print the same line as
//! `<group>.out` (see `tests/oracle/regexp/run-c.sh`).

use std::fmt::Write;

use stream_rs::StreamBuffer;
use stream_rs::converter::{find, parse_format};
use stream_rs::cstr::CPtr;
use stream_rs::error::{
    capture_stderr, set_get_thread_name_function, set_stream_debug_colored,
    set_stream_msg_time_stamped, take_captured,
};
use stream_rs::format::{PSEUDO_FORMAT, StreamFormat};
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

/// One `oracle.cc` line; without `call`, only what `parse` did.
fn run(case: &str, call: bool) -> String {
    let f: Vec<&str> = case.split('\t').collect();
    let scan = f[0] == "s";
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
    let format_type = if scan {
        FormatType::ScanFormat
    } else {
        FormatType::PrintFormat
    };
    let t = parse_format(&mut source, format_type, &mut fmt, &mut info);
    // The compiled pattern's pointer differs run to run.
    let skip = if info.length() >= size_of::<usize>() {
        size_of::<usize>()
    } else {
        0
    };
    let mut line = format!(
        "parse={t} rest={} infolen={} info={}",
        hex(source.cstr()),
        info.length(),
        hex(&info.bytes()[skip..])
    );
    if t != 0 && call {
        let mut info_bytes = info.bytes().to_vec();
        info_bytes.push(0);
        fmt.type_ = t;
        fmt.infolen = info.length() as u64;
        fmt.info = &info_bytes;
        let c = &find(fmt.conv).unwrap().converter;
        let v = unhex(f[2]);
        if t == PSEUDO_FORMAT {
            let mut b = StreamBuffer::from_bytes(&v);
            if scan {
                let mut cursor: usize = f.get(3).map_or(0, |s| s.parse().unwrap());
                let n = c.scan_pseudo(&fmt, &mut b, &mut cursor);
                write!(line, " n={n} cursor={cursor} out=").unwrap();
            } else {
                let ok = c.print_pseudo(&fmt, &mut b);
                write!(line, " ok={} out=", ok as i32).unwrap();
            }
            line.push_str(&hex(b.bytes()));
        } else {
            let mut input = [0u8; 512];
            input[..v.len()].copy_from_slice(&v);
            let size: usize = f.get(3).map_or(16, |s| s.parse().unwrap());
            let mut buf = [0x5au8; 256];
            let mut sz = size;
            let n = c.scan_string(&fmt, CPtr::new(&input), &mut buf[..size], &mut sz);
            write!(line, " n={n} size={sz} buf={}", hex(&buf[..size])).unwrap();
        }
    }
    write!(line, " err={}", hex(&take_captured())).unwrap();
    line
}

/// A C line without the call's result. The parse cases' calls print
/// nothing to err.
fn parse_part(line: &str) -> String {
    let (head, err) = line.rsplit_once(" err=").unwrap();
    let head = head
        .split(" n=")
        .next()
        .unwrap()
        .split(" ok=")
        .next()
        .unwrap();
    format!("{head} err={err}")
}

/// The line a parsed `%#/` case prints: the subst in info, then the call.
fn pseudo(subst: &str, call: &str, out: &str) -> String {
    format!(
        "parse=6 rest= infolen={} info={}00 {call} out={} err=",
        size_of::<usize>() + subst.len() + 1,
        hex(subst.as_bytes()),
        hex(out.as_bytes())
    )
}

/// The `%#/` cases where the port fixes a C defect, keyed by their `.in`
/// line, with the line the port prints. Their `.out` line stays C's.
fn fixed() -> Vec<(&'static str, String)> {
    vec![
        // RegexpConverter.cc:62 and :103 check prec as a sub-expression
        // index, but for %#/ it counts matches (formats.html: "precision is
        // the maximum number of matches to replace" / "the index ... of the
        // match to replace"), so C refuses its own example %#+-10.2/ab/X/.
        (
            "s\t%#.3/(a)(b)/x/\t.",
            "parse=6 rest= infolen=10 info=7800 err=".into(),
        ),
        (
            "p\t%#+-10.2/ab/X/\t616263616263616263616263",
            pseudo("X", "ok=1", "abcXcXcabc"),
        ),
        ("p\t%#.2/a/X/\t613161326133", pseudo("X", "ok=1", "a1X2a3")),
        ("p\t%#.3/a/X/\t613161326133", pseudo("X", "ok=1", "a1a2X3")),
        ("p\t%#+.2/a/X/\t613161326133", pseudo("X", "ok=1", "X1X2a3")),
        ("p\t%#.5/a/X/\t613161326133", pseudo("X", "ok=1", "a1a2a3")),
        (
            "s\t%#.2/a/X/\t61616161\t1",
            pseudo("X", "n=0 cursor=1", "aaXa"),
        ),
        ("p\t%#.12/a/X/\t613161326133", pseudo("X", "ok=1", "a1a2a3")),
        (
            "p\t%#+.10/a/X/\t616161616161616161616161",
            pseudo("X", "ok=1", "XXXXXXXXXXaa"),
        ),
        // RegexpConverter.cc:287 moves past a match it does not replace by
        // its start only, so without the + flag C replaces the first match
        // prec times over, where formats.html says "precision is the index
        // (counting from 1) of the match to replace".
        (
            "p\t%#.2/(a)(1?)/X/\t613161326133",
            pseudo("X", "ok=1", "a1X2a3"),
        ),
        ("p\t%#.2/(a)(b?)/X/\t61616161", pseudo("X", "ok=1", "aXaa")),
        (
            "s\t%#.2/(a)(b?)/X/\t61616161\t1",
            pseudo("X", "n=0 cursor=1", "aaXa"),
        ),
    ]
}

fn check(group: &str, call: bool) {
    set_stream_debug_colored(false);
    set_stream_msg_time_stamped(0);
    set_get_thread_name_function(None);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/regexp/");
    let cases = std::fs::read_to_string(format!("{dir}{group}.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}{group}.out")).unwrap();
    let fixed = fixed();
    let mut failed = Vec::new();
    let mut n = 0;
    for (case, want) in cases.lines().zip(expected.lines()) {
        let got = run(case, call);
        let mut want = if call {
            want.to_string()
        } else {
            parse_part(want)
        };
        if let Some((_, port)) = fixed.iter().find(|(c, _)| *c == case) {
            assert_ne!(&want, port, "{case}: C no longer shows its defect");
            want = port.clone();
        }
        if got != want {
            failed.push(format!(
                "{}\n  want: {want}\n    rs: {got}",
                case.replace('\t', " ")
            ));
        }
        n += 1;
    }
    assert_eq!(n, expected.lines().count());
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn regexp_parse() {
    check("parse", false);
}

#[test]
fn regexp_scan_string() {
    check("scan", true);
}

fn scan_one(format: &[u8], input: &[u8], value: &mut [u8], size: &mut usize) -> isize {
    let src = [format, &[0]].concat();
    let mut source = CPtr::new(&src);
    let mut fmt = StreamFormat::default();
    let mut info = StreamBuffer::new();
    let t = parse_format(&mut source, FormatType::ScanFormat, &mut fmt, &mut info);
    assert_eq!(t, stream_rs::format::STRING_FORMAT);
    let info_bytes = info.bytes().to_vec();
    fmt.type_ = t;
    fmt.info = &info_bytes;
    let c = &find(b'/').unwrap().converter;
    c.scan_string(&fmt, CPtr::new(input), value, size)
}

/// StreamCore hands over the input line's data without the zero tail C's
/// buffer has; a width past its end still matches 0x00 as the oracle's
/// `%5/c./` line does in C.
#[test]
fn regexp_scan_string_width_past_the_data_reads_zeros() {
    let mut buf = [0x5a; 4];
    let mut size = buf.len();
    assert_eq!(scan_one(b"%5/c./", b"abc", &mut buf, &mut size), 4);
    assert_eq!((size, buf), (3, [b'c', 0, 0, 0x5a]));
}

/// C's `size-1` wraps with the + flag and no buffer and copies past it;
/// the port writes nothing and still consumes the match.
#[test]
fn regexp_scan_string_plus_flag_into_no_buffer_writes_nothing() {
    let mut size = 0;
    assert_eq!(scan_one(b"%+/b+/", b"abbc", &mut [], &mut size), 3);
    assert_eq!(size, 0);
}

#[test]
fn regexp_subst() {
    check("subst", true);
}

/// formats.html's example: "%#+-10.2/ab/X/ replaces the string ab with X
/// maximal 2 times in the last 10 characters". C refuses to parse it.
#[test]
fn regexp_subst_prec_counts_matches() {
    let line = run("p\t%#+-10.2/ab/X/\t616263616263616263616263", true);
    assert_eq!(line, pseudo("X", "ok=1", "abcXcXcabc"));
}

/// formats.html: "precision is the index (counting from 1) of the match to
/// replace". C replaces the first one.
#[test]
fn regexp_subst_prec_without_plus_replaces_that_match() {
    let line = run("p\t%#.3/a/X/\t613161326133", true);
    assert_eq!(line, pseudo("X", "ok=1", "a1a2X3"));
}
