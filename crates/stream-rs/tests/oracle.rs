//! Parser errors, byte for byte against C StreamDevice: each
//! `tests/oracle/<case>.stderr` is what C printed for the case's records
//! (see `tests/oracle/run-c.sh`). The `real_*` cases load every protocol
//! of the device support files in `tests/proto`, where C prints nothing
//! except for the two files whose protocols C rejects too.

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
    for case in &order {
        protocol::free();
        let mut rec = common::Rec {
            records: records
                .iter()
                .filter(|r| r.0 == *case && r.1 == "@record")
                .map(|r| r.2.to_string())
                .collect(),
            ..Default::default()
        };
        capture_stderr();
        let mut loaded = 0;
        for (_, file, proto) in records.iter().filter(|r| r.0 == *case && r.1 != "@record") {
            let compiled = common::load(file, proto, &mut rec).is_some();
            loaded += compiled as usize;
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
            let got: Vec<String> = rec
                .compiled
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
        }
        if got != want {
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
    assert!(
        failed.is_empty(),
        "{} of {} cases differ:\n{}",
        failed.len(),
        order.len(),
        failed.join("\n")
    );
}
