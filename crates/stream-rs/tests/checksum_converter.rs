//! `ChecksumConverter.cc` byte for byte against C: each line of
//! `tests/oracle/conv/checksum.in` runs here as `oracle.cc` runs it on C
//! StreamDevice, and must print the same line as `checksum.out` (see
//! `tests/oracle/conv/run-c.sh`). The calls run with `streamDebug` on, so
//! the converter's debug lines are compared too, less their `file:line: `
//! prefix, which names the C or the Rust source.

use std::fmt::Write;
use std::sync::Mutex;

use stream_rs::StreamBuffer;
use stream_rs::converter::{find, parse_format};
use stream_rs::cstr::{CPtr, c_sscanf_hex};
use stream_rs::error::{
    capture_stderr, set_get_thread_name_function, set_stream_debug, set_stream_debug_colored,
    set_stream_msg_time_stamped, take_captured,
};
use stream_rs::format::StreamFormat;
use stream_rs::protocol::FormatType;

/// `streamDebug` is process-wide.
static SWITCHES: Mutex<()> = Mutex::new(());

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

/// One `oracle.cc` line of mode `pp` or `sp`.
fn run(case: &str) -> String {
    let f: Vec<&str> = case.split('\t').collect();
    let mut src = f[1].as_bytes().to_vec();
    src.push(0);
    let mut source = CPtr::new(&src);
    let mut fmt = StreamFormat::default();
    let mut info = StreamBuffer::new();
    capture_stderr();
    let format_type = if f[0] == "pp" {
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
        match f[0] {
            "pp" => {
                let mut out = StreamBuffer::from_bytes(&unhex(f[2]));
                set_stream_debug(1);
                let ok = c.print_pseudo(&fmt, &mut out);
                set_stream_debug(0);
                write!(line, " ok={} out={}", ok as i32, hex(out.bytes())).unwrap();
            }
            "sp" => {
                let mut input = StreamBuffer::from_bytes(&unhex(f[2]));
                let mut cursor: usize = f.get(3).map_or(0, |s| s.parse().unwrap());
                set_stream_debug(1);
                let n = c.scan_pseudo(&fmt, &mut input, &mut cursor);
                set_stream_debug(0);
                write!(line, " n={n} cursor={cursor}").unwrap();
            }
            _ => panic!("mode {}", f[0]),
        }
    }
    write!(line, " err={}", hex(&take_captured())).unwrap();
    line
}

/// The line with each debug line's `file:line: ` prefix taken out of `err`.
fn without_debug_location(line: &str) -> String {
    let (head, err) = line.split_once(" err=").unwrap();
    let err = unhex(err);
    let mut out = Vec::new();
    for l in err.split_inclusive(|&b| b == b'\n') {
        let located = l
            .iter()
            .position(|&b| b == b':')
            .filter(|&i| {
                let file = &l[..i];
                (file.ends_with(b".cc") || file.ends_with(b".rs"))
                    && file
                        .iter()
                        .all(|b| b.is_ascii_alphanumeric() || b"._".contains(b))
            })
            .and_then(|i| {
                let rest = &l[i + 1..];
                let n = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                (n > 0 && rest[n..].starts_with(b": ")).then_some(i + 1 + n + 2)
            });
        out.extend_from_slice(&l[located.unwrap_or(0)..]);
    }
    format!("{head} err={}", hex(&out))
}

/// Run the cases `keep` selects; `part` is what of each line is compared.
fn check(keep: impl Fn(&str) -> bool, part: impl Fn(&str) -> String) {
    check_with(keep, |_, c_line| c_line.to_string(), part);
}

/// `fixup` turns a C line into the port's where they differ by design.
fn check_with(
    keep: impl Fn(&str) -> bool,
    fixup: impl Fn(&str, &str) -> String,
    part: impl Fn(&str) -> String,
) {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    set_stream_debug_colored(false);
    set_stream_msg_time_stamped(0);
    set_get_thread_name_function(None);
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/conv/");
    let cases = std::fs::read_to_string(format!("{dir}checksum.in")).unwrap();
    let expected = std::fs::read_to_string(format!("{dir}checksum.out")).unwrap();
    assert_eq!(cases.lines().count(), expected.lines().count());
    let mut failed = Vec::new();
    let mut n = 0;
    for (case, want) in cases.lines().zip(expected.lines()) {
        if !keep(case) {
            continue;
        }
        let (want, got) = (part(&fixup(case, want)), part(&run(case)));
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

/// What `parse` returned, left of the format and wrote as info, for every
/// case, and the error of the ones it refused.
#[test]
fn checksum_parse() {
    check(
        |_| true,
        |line| {
            let line = without_debug_location(line);
            if line.starts_with("parse=0 ") {
                return line;
            }
            let end = line.find(" ok=").or_else(|| line.find(" n=")).unwrap();
            line[..end].to_string()
        },
    );
}

/// `printPseudo`: the output and its debug lines.
#[test]
fn checksum_print_pseudo() {
    check(|case| case.starts_with("pp\t"), without_debug_location);
}

/// `scanPseudo`: what it returned and its debug lines.
#[test]
fn checksum_scan_pseudo() {
    check_with(
        |case| case.starts_with("sp\t"),
        |case, c_line| {
            // Big endian ASCII: C leaves the sscanf of each checksum byte
            // unchecked, so a byte that is not hex compares the previous
            // byte's inchar (see checksum_scan_refuses_big_endian_non_hex).
            let f: Vec<&str> = case.split('\t').collect();
            let little = f[1].starts_with("%#0");
            if !little && (!f[1].starts_with("%0") || f[1].contains('#')) {
                return c_line.to_string();
            }
            let (head, err) = c_line.split_once(" err=").unwrap();
            let err = String::from_utf8(unhex(err)).unwrap();
            let Some((kept, sum)) = err
                .split_once("input checksum is 0x")
                .map(|(_, tail)| (&err[..err.len() - tail.len()], tail))
                .and_then(|(kept, tail)| Some((kept, tail.split_once('\n')?.0)))
            else {
                return c_line.to_string();
            };
            let input = unhex(f[2]);
            let cursor: usize = f[3].parse().unwrap();
            let value = u32::from_str_radix(sum, 16).unwrap();
            let refused = |at: usize| {
                let name = &f[1][f[1].find('<').unwrap() + 1..f[1].len() - 1];
                let refused = format!(
                    "{kept}{sum}\nChecksumConverter.cc:0: ChecksumConverter {name}: \
                     Input byte '{}' is not a hex byte\n",
                    String::from_utf8_lossy(&input[at..at + 2])
                );
                format!("{head} err={}", hex(refused.as_bytes()))
            };
            if little {
                // glibc 2.43 refuses a bare "0x" as `%2x` (BZ #12701); the
                // 2.39 that recorded checksum.out read it as 0.
                for i in 0..sum.len() / 2 {
                    let at = cursor + 2 * i;
                    if input
                        .get(at..at + 2)
                        .is_some_and(|b| b.eq_ignore_ascii_case(b"0x"))
                    {
                        return refused(at);
                    }
                    match c_sscanf_hex(CPtr::new(&[&input[at..], &[0]].concat())) {
                        Some((byte, _)) if byte == (value >> (8 * i)) & 0xff => {}
                        _ => break,
                    }
                }
                return c_line.to_string();
            }
            for (j, i) in (0..sum.len() / 2).rev().enumerate() {
                let at = cursor + 2 * i;
                let Some((byte, _)) = c_sscanf_hex(CPtr::new(&[&input[at..], &[0]].concat()))
                else {
                    return refused(at);
                };
                if byte != (value >> (8 * j)) & 0xff {
                    break;
                }
            }
            c_line.to_string()
        },
        without_debug_location,
    );
}

/// A big endian ASCII checksum that is not hex is refused like a little
/// endian one; C compares whatever `inchar` held (see `scan_pseudo`).
#[test]
fn checksum_scan_refuses_big_endian_non_hex() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    set_stream_debug_colored(false);
    set_stream_msg_time_stamped(0);
    set_get_thread_name_function(None);
    for (format, input, bad) in [
        ("%0<sum>", &b"123456789zz"[..], "zz"),
        ("%0<sum16>", b"12345678901zz", "zz"),
        ("%0<sum16>", b"123456789zzDD", "zz"),
    ] {
        let case = format!("sp\t{format}\t{}\t9", hex(input));
        let got = without_debug_location(&run(&case));
        let want_err = format!(
            "ChecksumConverter {name}: input to check: \"123456789\"\n\
             ChecksumConverter {name}: input checksum is 0x{sum}\n\
             ChecksumConverter {name}: Input byte '{bad}' is not a hex byte\n",
            name = &format[3..format.len() - 1],
            sum = if format.contains("16") { "01DD" } else { "DD" },
        );
        assert_eq!(
            got.split_once(" n=").unwrap().1,
            format!("-1 cursor=9 err={}", hex(want_err.as_bytes())),
            "{format} {}",
            String::from_utf8_lossy(input)
        );
    }
}
