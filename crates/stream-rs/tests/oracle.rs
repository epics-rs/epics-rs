//! Protocol loading, byte for byte against C StreamDevice: each
//! `tests/oracle/<case>.stderr` is what C's parser and `StreamCore::parse`
//! printed for the case's records, and for the cases that compile,
//! `<case>.compiled` and `<case>.report` are C's command lists and
//! `printProtocol` output (see `tests/oracle/run-c.sh`). The `real_*` cases
//! load every protocol of the device support files in `tests/proto`, where
//! C prints nothing except for the two files whose protocols C rejects too.

mod common;

use stream_rs::StreamBuffer;
use stream_rs::converter::ESC;
use stream_rs::core::commands;
use stream_rs::error::{capture_stderr, take_captured};
use stream_rs::format::StreamFormat;
use stream_rs::protocol::{self, codes};

/// Where C's compiled code and the port's differ by design: the
/// `StreamFormat` layout (C's x86_64 struct, padding and `info` pointer
/// included), the field address (C's `DBADDR`), and the info and type
/// the converters return (stand-ins here). What is left is the parser's:
/// the commands, literal bytes, escapes, field names, format strings,
/// conversion characters, flags, widths and precisions.
#[derive(Clone, Copy)]
enum Layout {
    C,
    Rs,
}

fn read<const N: usize>(code: &[u8], at: usize) -> [u8; N] {
    code[at..at + N].try_into().unwrap()
}

fn normalize(code: &[u8], layout: Layout) -> String {
    let mut out = String::new();
    let mut lit = StreamBuffer::new();
    let flush = |lit: &mut StreamBuffer, out: &mut String| {
        if lit.length() > 0 {
            out.push_str(&format!(
                " \"{}\"",
                String::from_utf8_lossy(lit.expand().bytes())
            ));
            lit.clear();
        }
    };
    let cstr = |at: usize| {
        let n = code[at..].iter().position(|&b| b == 0).unwrap();
        (
            String::from_utf8_lossy(&code[at..at + n]).into_owned(),
            at + n + 1,
        )
    };
    let mut i = 0;
    while i < code.len() {
        let cmd = code[i];
        i += 1;
        out.push_str(&format!("{} ", commands::to_str(cmd)));
        match cmd {
            commands::IN | commands::OUT | commands::EXEC => loop {
                let b = code[i];
                i += 1;
                match b {
                    codes::EOS => break,
                    ESC => {
                        lit.append_byte(code[i]);
                        i += 1;
                    }
                    codes::SKIP | codes::WHITESPACE => {
                        flush(&mut lit, &mut out);
                        out.push_str(if b == codes::SKIP {
                            " skip"
                        } else {
                            " whitespace"
                        });
                    }
                    codes::FORMAT | codes::FORMAT_FIELD => {
                        flush(&mut lit, &mut out);
                        if b == codes::FORMAT_FIELD {
                            let (name, next) = cstr(i);
                            let len = u16::from_ne_bytes(read(code, next)) as usize;
                            i = next + 2 + len;
                            out.push_str(&format!(" field({name})"));
                        }
                        // The format string can hold escaped bytes
                        // (StreamCore.cc formatOutput reads it so).
                        let start = i;
                        while code[i] != 0 {
                            i += if code[i] == ESC { 2 } else { 1 };
                        }
                        let fs = StreamBuffer::from_bytes(&code[start..i]).expand();
                        let fs = String::from_utf8_lossy(fs.bytes()).into_owned();
                        i += 1;
                        let (conv, flags, prec, width, infolen) = match layout {
                            Layout::C => {
                                let f = (
                                    code[i],
                                    u16::from_ne_bytes(read(code, i + 8)),
                                    i64::from_ne_bytes(read(code, i + 16)),
                                    u64::from_ne_bytes(read(code, i + 24)),
                                    u64::from_ne_bytes(read(code, i + 32)),
                                );
                                i += 48;
                                f
                            }
                            Layout::Rs => {
                                let f = StreamFormat::decode(&read(code, i));
                                i += StreamFormat::SIZE;
                                (f.conv, f.flags, f.prec, f.width, f.infolen)
                            }
                        };
                        // The stand-ins write no info.
                        if matches!(layout, Layout::C) {
                            i += infolen as usize;
                        }
                        out.push_str(&format!(
                            " format(\"{fs}\" {} flags={flags:#x} prec={prec} width={width})",
                            conv as char
                        ));
                    }
                    _ => {
                        lit.append_byte(b);
                    }
                }
                if i >= code.len() {
                    break;
                }
            },
            commands::WAIT | commands::CONNECT => {
                out.push_str(&u64::from_ne_bytes(read(code, i)).to_string());
                i += 8;
            }
            commands::EVENT => {
                let mask = u64::from_ne_bytes(read(code, i));
                let timeout = u64::from_ne_bytes(read(code, i + 8));
                out.push_str(&format!("{mask:#x} {timeout}"));
                i += 16;
            }
            _ => {}
        }
        flush(&mut lit, &mut out);
        out.push_str("; ");
    }
    out
}

/// compileNumber reports the line of the token after the number, which
/// past the last argument C reads from beyond the value (StreamProtocol.cc
/// compileNumber, getLineNumber(source)): stack garbage, as in the compiled
/// code. Its line field is masked on both sides.
fn mask_number_lines(text: &[u8]) -> Vec<u8> {
    const MESSAGES: [&[u8]; 3] = [
        b"Unsigned numeric value expected: ",
        b"Garbage after numeric value: ",
        b"Value ",
    ];
    let mut out = Vec::new();
    for line in text.split_inclusive(|&b| b == b'\n') {
        let masked = (|| {
            let at = line.windows(6).position(|w| w == b" line ")? + 6;
            let digits = line[at..].iter().take_while(|b| b.is_ascii_digit()).count();
            let rest = line[at + digits..].strip_prefix(b": ")?;
            let number = MESSAGES.iter().any(|m| rest.starts_with(m))
                && (!rest.starts_with(b"Value ")
                    || rest.windows(14).any(|w| w == b" out of range "));
            number.then(|| [&line[..at], b"?", &line[at + digits..]].concat())
        })();
        out.extend(masked.unwrap_or_else(|| line.to_vec()));
    }
    out
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

/// The `printProtocol` block of each record in C's `streamReportRecord`
/// output, by record number.
fn c_reports(report: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    for line in report.split_inclusive('\n') {
        match line.strip_prefix("rec").and_then(|l| l.split_once(": ")) {
            Some((n, _)) if !line.starts_with(' ') && n.parse::<usize>().is_ok() => {
                out.push((n.parse().unwrap(), String::new()))
            }
            _ => out.last_mut().unwrap().1.push_str(line),
        }
    }
    out
}

#[test]
fn parser_errors_match_c() {
    common::setup();
    protocol::set_path(Some("tests/oracle:tests/none;tests/proto"));
    let cases = std::fs::read_to_string("tests/oracle/cases.txt").unwrap();
    let mut order: Vec<&str> = Vec::new();
    let mut records: Vec<(&str, &str, &str)> = Vec::new();
    for line in cases
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        let mut w = line.split_whitespace();
        let (case, file, proto) = (w.next().unwrap(), w.next().unwrap(), w.next().unwrap());
        if order.last() != Some(&case) {
            order.push(case);
        }
        records.push((case, file, proto));
    }
    let mut failed = Vec::new();
    let mut compiled_lists = 0;
    let mut reported = 0;
    for case in &order {
        protocol::free();
        let others: Vec<String> = records
            .iter()
            .filter(|r| r.0 == *case && r.1 == "@record")
            .map(|r| r.2.to_string())
            .collect();
        capture_stderr();
        let mut loaded = 0;
        let mut lists = Vec::new();
        let mut reports = Vec::new();
        for (n, (_, file, proto)) in records
            .iter()
            .filter(|r| r.0 == *case && r.1 != "@record")
            .enumerate()
        {
            let (core, ok) = common::load(file, proto, &format!("rec{n}"), &others);
            if ok {
                loaded += 1;
                lists.extend(common::compiled(&core));
            }
            // C reports a record whose protocol failed too, with what
            // compile left in its lists.
            let mut out = Vec::new();
            core.lock().print_protocol(&mut out).unwrap();
            reports.push((n, String::from_utf8(out).unwrap()));
        }
        let got = take_captured();
        let want = std::fs::read(format!("tests/oracle/{case}.stderr")).unwrap();
        // Every bad case fails; a real file's protocols compile but for
        // those C rejects, each named once at the end of its error.
        let total = records
            .iter()
            .filter(|r| r.0 == *case && r.1 != "@record")
            .count();
        let rejected = if case.starts_with("real_") || case.starts_with("ok_") {
            String::from_utf8_lossy(&want)
                .matches(": in protocol '")
                .count()
        } else {
            total
        };
        if loaded != total - rejected {
            failed.push(format!(
                "{case}: {loaded} of {total} protocols compiled, C rejected {rejected}"
            ));
        }
        if case.starts_with("real_") || case.starts_with("ok_") {
            let c_compiled =
                std::fs::read_to_string(format!("tests/oracle/{case}.compiled")).unwrap();
            let want: Vec<String> = c_compiled
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .filter(|(_, code)| !code.is_empty())
                .map(|(h, code)| format!("{h}: {}", normalize(&unexpand(code), Layout::C)))
                .collect();
            let got: Vec<String> = lists
                .iter()
                .map(|(h, code)| format!("{h}: {}", normalize(code, Layout::Rs)))
                .collect();
            compiled_lists += got.len();
            if got.len() != want.len() {
                failed.push(format!(
                    "{case}: {} command lists compiled, C {}",
                    got.len(),
                    want.len()
                ));
            }
            for (g, w) in got.iter().zip(&want) {
                if g != w {
                    failed.push(format!("{case} compiled:\n  C:  {w}\n  rs: {g}"));
                }
            }
            let c_report = std::fs::read_to_string(format!("tests/oracle/{case}.report")).unwrap();
            let mut want = c_reports(&c_report);
            // C's StreamBuffer::print leaves the `\` of a truncated
            // vsnprintf past `len`, grow's in-place move clears only `offs`
            // bytes after it, and printProtocol reads up to a zero
            // (StreamBuffer.cc print and grow): one stray byte the port
            // does not copy. run-c.sh's block ends at the next `}` line,
            // so this one also takes the blank line after it.
            if *case == "real_PTC10" {
                let r = &mut want[26].1;
                *r = r.replacen("\";\n\\}\n\n", "\";\n}\n", 1);
            }
            reported += reports.len();
            if reports.iter().map(|r| r.0).ne(want.iter().map(|r| r.0)) {
                failed.push(format!("{case}: reported records differ from C's"));
            }
            for ((n, g), (_, w)) in reports.iter().zip(&want) {
                if g != w {
                    failed.push(format!(
                        "{case} rec{n} printProtocol:\n  C:\n{w}\n  rs:\n{g}"
                    ));
                }
            }
        }
        if mask_number_lines(&got) != mask_number_lines(&want) {
            failed.push(format!(
                "{case}:\n  C:  {:?}\n  rs: {:?}",
                String::from_utf8_lossy(&want),
                String::from_utf8_lossy(&got)
            ));
        }
    }
    assert!(
        compiled_lists > 700,
        "{compiled_lists} command lists compared"
    );
    assert!(reported > 700, "{reported} printProtocol outputs compared");
    assert!(
        failed.is_empty(),
        "{} of {} cases differ:\n{}",
        failed.len(),
        order.len(),
        failed.join("\n")
    );
}
