//! The standard converters byte for byte against C: each line of
//! `tests/oracle/conv/<converter>.in` runs here as `oracle.cc` runs it on
//! C StreamDevice, and must print the same line as `<converter>.out`
//! (see `tests/oracle/conv/run-c.sh`).

use std::fmt::Write;

use epics_base_rs::runtime::stdlib::strtod;
use stream_rs::StreamBuffer;
use stream_rs::converter::{find, parse_format};
use stream_rs::cstr::CPtr;
use stream_rs::error::{
    capture_stderr, set_get_thread_name_function, set_stream_debug_colored,
    set_stream_msg_time_stamped, take_captured,
};
use stream_rs::format::{
    ALT_FLAG, LEFT_FLAG, SIGN_FLAG, SIGNED_FORMAT, STRING_FORMAT, StreamFormat, UNSIGNED_FORMAT,
    ZERO_FLAG,
};
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

/// C's `strtol(arg, NULL, 0)`.
fn arg_long(a: &str) -> i64 {
    let (neg, a) = a.strip_prefix('-').map_or((false, a), |a| (true, a));
    let v = match a.strip_prefix("0x") {
        Some(h) => i128::from_str_radix(h, 16).unwrap(),
        None => a.parse().unwrap(),
    };
    // strtol saturates.
    (if neg { -v } else { v }).clamp(i64::MIN.into(), i64::MAX.into()) as i64
}

/// One `oracle.cc` line.
fn run(case: &str) -> String {
    let f: Vec<&str> = case.split('\t').collect();
    let (mode, kind) = (f[0].as_bytes()[0], f[0].as_bytes()[1]);
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
    let format_type = if mode == b'p' {
        FormatType::PrintFormat
    } else {
        FormatType::ScanFormat
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
        let mut out = StreamBuffer::new();
        let input = || [unhex(f[2]).as_slice(), &[0]].concat();
        match (mode, kind) {
            (b'p' | b'c', b'l') => {
                let ok = c.print_long(&fmt, &mut out, arg_long(f[2]));
                write!(line, " ok={} out={}", ok as i32, hex(out.bytes())).unwrap();
            }
            (b'p' | b'c', b'd') => {
                let ok = c.print_double(&fmt, &mut out, strtod(f[2].as_bytes()).value);
                write!(line, " ok={} out={}", ok as i32, hex(out.bytes())).unwrap();
            }
            (b's', b'd') => {
                let mut val = 1234.5;
                let n = c.scan_double(&fmt, CPtr::new(&input()), &mut val);
                write!(line, " n={n} val={:016x}", val.to_bits()).unwrap();
            }
            (b's', b'l') => {
                let mut val = 0x5a5a;
                let n = c.scan_long(&fmt, CPtr::new(&input()), &mut val);
                write!(line, " n={n} val={val}").unwrap();
            }
            (b'p' | b'c', b's') => {
                let ok = c.print_string(&fmt, &mut out, &input());
                write!(line, " ok={} out={}", ok as i32, hex(out.bytes())).unwrap();
            }
            (b's', b's') => {
                let size: usize = f.get(3).map_or(16, |s| s.parse().unwrap());
                let mut buf = [0x5au8; 256];
                let mut sz = size;
                let n = c.scan_string(&fmt, CPtr::new(&input()), &mut buf[..size], &mut sz);
                write!(line, " n={n} size={sz} buf={}", hex(&buf[..size])).unwrap();
            }
            _ => panic!("mode {}", f[0]),
        }
    }
    write!(line, " err={}", hex(&take_captured())).unwrap();
    line
}

fn check(converter: &str) {
    check_with(converter, |_, c_line| c_line.to_string());
}

/// `fixup` turns a C line into the port's where they differ by design.
fn check_with(converter: &str, fixup: impl Fn(&str, &str) -> String) {
    check_lines(converter, |_| true, fixup, str::to_string);
}

/// Run the cases `keep` selects; `part` is what of each line is compared.
fn check_lines(
    converter: &str,
    keep: impl Fn(&str) -> bool,
    fixup: impl Fn(&str, &str) -> String,
    part: impl Fn(&str) -> String,
) {
    set_stream_debug_colored(false);
    set_stream_msg_time_stamped(0);
    set_get_thread_name_function(None);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}{converter}.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}{converter}.out")).unwrap();
    assert_eq!(cases.lines().count(), expected.lines().count());
    let mut failed = Vec::new();
    let mut n = 0;
    for (case, want) in cases.lines().zip(expected.lines()) {
        if !keep(case) {
            continue;
        }
        let want = part(&fixup(case, want));
        let got = part(&run(case));
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

#[test]
fn std_long_converter() {
    check("long");
}

/// The `%D` cases where the port fixes a C defect, keyed by their `.in`
/// line, with the line the port prints. Their `.out` line stays C's.
fn bcd_fixed() -> Vec<(&'static str, &'static str)> {
    vec![
        // BCDConverter.cc:50 appends the field before :51 widens it to the
        // width, so a width over the precision pads nothing.
        (
            "pl\t%6.4D\t1234",
            "parse=1 rest= infolen=0 info= ok=1 out=000000001234 err=",
        ),
        (
            "pl\t%+6.4D\t1234",
            "parse=2 rest= infolen=0 info= ok=1 out=000000001234 err=",
        ),
        (
            "pl\t%1.0D\t9",
            "parse=1 rest= infolen=0 info= ok=1 out=00 err=",
        ),
        // BCDConverter.cc:101-103 shift the upper half byte out twice, so
        // with # C reads the tens digits and the sign as 0.
        (
            "sl\t%#2D\t3412",
            "parse=1 rest= infolen=0 info= n=2 val=1234 err=",
        ),
        (
            "sl\t%#+2D\t04f1",
            "parse=2 rest= infolen=0 info= n=2 val=-104 err=",
        ),
        (
            "sl\t%#+2D\t0481",
            "parse=2 rest= infolen=0 info= n=2 val=-104 err=",
        ),
        (
            "sl\t%#+D\tf5",
            "parse=2 rest= infolen=0 info= n=1 val=-5 err=",
        ),
        // BCDConverter.cc:97's int shift overflows past 10^8 (undefined; gcc
        // wraps it), where the port's long reaches 10^10.
        (
            "sl\t%#+6D\t0000000000f1",
            "parse=2 rest= infolen=0 info= n=6 val=-10000000000 err=",
        ),
        (
            "sl\t%#6D\t000000000001",
            "parse=1 rest= infolen=0 info= n=6 val=10000000000 err=",
        ),
        (
            "sl\t%#6D\t000000000002",
            "parse=1 rest= infolen=0 info= n=6 val=20000000000 err=",
        ),
        (
            "sl\t%#6D\t000000000009",
            "parse=1 rest= infolen=0 info= n=6 val=90000000000 err=",
        ),
    ]
}

/// `BCDConverter.cc`'s `%D`, through the same oracle.
#[test]
fn bcd_converter() {
    let fixed = bcd_fixed();
    check_with("bcd", |case, c_line| {
        match fixed.iter().find(|(c, _)| *c == case) {
            Some((_, port)) => {
                assert_ne!(c_line, *port, "{case}: C no longer shows its defect");
                port.to_string()
            }
            None => c_line.to_string(),
        }
    });
}

fn bcd_print(flags: u16, prec: i64, width: u64, value: i64) -> Vec<u8> {
    let fmt = StreamFormat {
        conv: b'D',
        type_: SIGNED_FORMAT,
        flags,
        prec,
        width,
        infolen: 0,
        info: b"\0",
    };
    let mut out = StreamBuffer::from_bytes(b"x=");
    assert!(
        find(b'D')
            .unwrap()
            .converter
            .print_long(&fmt, &mut out, value)
    );
    out.bytes().to_vec()
}

/// Where C writes before its field (out of the buffer when nothing was
/// printed before): formats.html, "precision decimal digits are printed in
/// at least width output bytes", the sign in the most significant byte.
#[test]
fn bcd_print_width_over_the_precision_pads_the_field() {
    assert_eq!(bcd_print(ALT_FLAG, 4, 6, 1234), b"x=\x34\x12\0\0\0\0");
    assert_eq!(bcd_print(SIGN_FLAG, 4, 6, -1234), b"x=\xf0\0\0\0\x12\x34");
    assert_eq!(
        bcd_print(ALT_FLAG | SIGN_FLAG, 4, 6, -1234),
        b"x=\x34\x12\0\0\0\xf0"
    );
}

#[test]
fn std_double_converter() {
    check("double");
}

#[test]
fn std_string_converter() {
    check("string");
}

#[test]
fn std_chars_converter() {
    check("chars");
}

/// `BinaryConverter.cc`'s parse: what it returned, left of the format and
/// wrote as info, and the error of the formats it refused.
#[test]
fn binary_parse() {
    check_lines(
        "binary",
        |_| true,
        |_, c_line| c_line.to_string(),
        |line| match line.find(" ok=").or_else(|| line.find(" n=")) {
            Some(end) => line[..end].to_string(),
            None => line.to_string(),
        },
    );
}

/// `BinaryConverter::printLong`, for output formats and the input ones
/// that '=' turns into output.
#[test]
fn binary_print_long() {
    check_lines(
        "binary",
        |case| case.starts_with("pl\t") || case.starts_with("cl\t"),
        |_, c_line| c_line.to_string(),
        str::to_string,
    );
}

/// `BinaryConverter::scanLong`.
#[test]
fn binary_scan_long() {
    check_lines(
        "binary",
        |case| case.starts_with("sl\t"),
        |_, c_line| c_line.to_string(),
        str::to_string,
    );
}

/// C's `memset(info(1), 255, 32)` leaves the map's first byte clear where
/// the port fills it (the cases keep bytes 0 to 7 out of a set that is not
/// negated, and out of the input).
#[test]
fn std_charset_converter() {
    check_with("charset", |case, c_line| {
        let format = case.split('\t').nth(1).unwrap();
        let negated = format
            .split_once('[')
            .is_some_and(|(_, set)| set.starts_with('^'));
        match c_line.split_once(" info=00") {
            Some((head, tail)) if !negated && head.ends_with("infolen=32") => {
                format!("{head} info=ff{tail}")
            }
            _ => c_line.to_string(),
        }
    });
}

/// C crashes here: with the space flag its `size_t` width wraps when more
/// space was skipped than the width. The port takes the width as used up.
#[test]
fn std_long_space_flag_past_the_width_reads_nothing() {
    let r = run("sl\t% 2d\t20202035");
    assert_eq!(r, "parse=2 rest= infolen=3 info=000000 n=-1 val=23130 err=");
}

/// C's `%0Ns` takes `width - strlen` as a count that goes negative for a
/// longer string, and `append(c, count)` then deletes bytes from the end of
/// the output: before the string it eats what was printed earlier, after it
/// (`%-0Ns`) the string's tail. The port pads with nothing there.
#[test]
fn std_string_zero_width_shorter_than_the_string_deletes_nothing() {
    let c = &find(b's').unwrap().converter;
    for (flags, info) in [
        (ZERO_FLAG, &b"%03s\0"[..]),
        (ZERO_FLAG | LEFT_FLAG, b"%-03s\0"),
    ] {
        let fmt = StreamFormat {
            conv: b's',
            type_: STRING_FORMAT,
            flags,
            prec: -1,
            width: 3,
            infolen: info.len() as u64 - 1,
            info,
        };
        let mut out = StreamBuffer::from_bytes(b"x=");
        assert!(c.print_string(&fmt, &mut out, b"hello\0"));
        assert_eq!(out.bytes(), b"x=hello");
    }
}

/// Where C reads past its map: bytes 1 to 7 never end a set that is not
/// negated (the clear first byte), and a byte from 0x80 indexes the map
/// as a signed char, 16 bytes before it. The port reads the map's bit.
#[test]
fn std_charset_ends_on_low_and_high_bytes_by_the_map() {
    set_stream_debug_colored(false);
    let tail = |r: String| r.split_once(" n=").unwrap().1.to_string();
    assert_eq!(
        tail(run("ss\t%[a-z]\t610162")),
        "1 size=2 buf=61005a5a5a5a5a5a5a5a5a5a5a5a5a5a err="
    );
    assert_eq!(
        tail(run("ss\t%[^\\x80]\t61ff8062")),
        "2 size=3 buf=61ff005a5a5a5a5a5a5a5a5a5a5a5a5a err="
    );
    assert_eq!(
        tail(run("ss\t%[a-\\x7f]\t61ff")),
        "1 size=2 buf=61005a5a5a5a5a5a5a5a5a5a5a5a5a5a err="
    );
}

/// formats.html: "With the # flag, the byte order is changed to little
/// endian"; C reads 78 56 34 12 as 2040608.
#[test]
fn bcd_scan_little_endian_reads_the_tens_digits() {
    let fmt = StreamFormat {
        conv: b'D',
        type_: UNSIGNED_FORMAT,
        flags: ALT_FLAG,
        prec: -1,
        width: 4,
        infolen: 0,
        info: b"\0",
    };
    let mut val = 0;
    let c = &find(b'D').unwrap().converter;
    assert_eq!(
        c.scan_long(&fmt, CPtr::new(b"\x78\x56\x34\x12\0"), &mut val),
        4
    );
    assert_eq!(val, 12345678);
}

/// The cap of BCDConverter.cc:113 allows 12 digits, which C's int shift
/// cannot hold.
#[test]
fn bcd_scan_little_endian_reads_12_digits() {
    let fmt = StreamFormat {
        conv: b'D',
        type_: UNSIGNED_FORMAT,
        flags: ALT_FLAG,
        prec: -1,
        width: 6,
        infolen: 0,
        info: b"\0",
    };
    let mut val = 0;
    let c = &find(b'D').unwrap().converter;
    let input = b"\x12\x90\x78\x56\x34\x12\0";
    assert_eq!(c.scan_long(&fmt, CPtr::new(input), &mut val), 6);
    assert_eq!(val, 123456789012);
}

/// glibc 2.39 drops `%#g`'s zeros when rounding carries the value up to
/// 10^P: C prints "1.e+06" and "1.E+06" here. The port prints C99's.
#[test]
fn std_double_alt_g_keeps_its_zeros_on_a_rounding_carry() {
    set_stream_debug_colored(false);
    let out = |r: String| r.split_once(" out=").unwrap().1.to_string();
    assert_eq!(out(run("pd\t%#g\t999999.5")), "312e3030303030652b3036 err=");
    assert_eq!(out(run("pd\t%#G\t999999.5")), "312e3030303030452b3036 err=");
}

/// `print_double` against glibc's `snprintf` over every flag set, a range of
/// widths and precisions, and values around each rounding edge.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn std_double_print_matches_snprintf() {
    let mut values = vec![
        0.0,
        -0.0,
        f64::MIN_POSITIVE,
        5e-324,
        f64::MAX,
        f64::NAN,
        -f64::NAN,
    ];
    values.extend([f64::INFINITY, f64::NEG_INFINITY]);
    for e in -30..=30 {
        for m in [
            1.0,
            0.5,
            9.5,
            9.95,
            9.995,
            9.9999995,
            1.25,
            2.675,
            3.0 / 7.0,
            0.95,
        ] {
            let v = m * 10f64.powi(e);
            values.extend([
                v,
                -v,
                f64::from_bits(v.to_bits() + 1),
                f64::from_bits(v.to_bits() - 1),
            ]);
        }
    }
    let mut specs = Vec::new();
    for flags in 0..32u32 {
        let f: String = ["#", "+", " ", "-", "0"]
            .iter()
            .enumerate()
            .filter(|(i, _)| flags & 1 << i != 0)
            .map(|(_, s)| *s)
            .collect();
        for width in ["", "1", "12"] {
            for prec in ["", ".0", ".1", ".3", ".17", ".40"] {
                for conv in ["f", "e", "E", "g", "G"] {
                    specs.push(format!("%{f}{width}{prec}{conv}"));
                }
            }
        }
    }
    let mut buf = vec![0u8; 4096];
    for spec in &specs {
        let cspec = std::ffi::CString::new(spec.as_str()).unwrap();
        let info = [spec.as_bytes(), &[0]].concat();
        for &v in &values {
            // SAFETY: one double for one conversion, into a buffer of its length.
            let n =
                unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), cspec.as_ptr(), v) };
            let c = buf[..n as usize].to_vec();
            let mut out = StreamBuffer::new();
            stream_rs::cprintf::print_double(&mut out, &info, v);
            let rs = out.bytes();
            if rs == c.as_slice() {
                continue;
            }
            // The glibc carry bug above: the port prints what `%#.{P-1}e`
            // prints, where C's mantissa is a bare "1.".
            let cs = String::from_utf8_lossy(&c).into_owned();
            let (head, conv) = spec.split_at(spec.len() - 1);
            let (head, p) = match head.split_once('.') {
                Some((h, p)) => (h, p.parse::<usize>().unwrap().max(1)),
                None => (head, 6),
            };
            let e = if conv == "g" { "e" } else { "E" };
            let espec = std::ffi::CString::new(format!("{head}.{}{e}", p - 1)).unwrap();
            // SAFETY: as above.
            let n =
                unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), espec.as_ptr(), v) };
            assert!(
                head.contains('#')
                    && conv.eq_ignore_ascii_case("g")
                    && cs.contains(&format!("1.{e}"))
                    && rs == &buf[..n as usize],
                "{spec} {v:e}: C {cs:?} rs {:?}",
                String::from_utf8_lossy(rs)
            );
        }
    }
}
