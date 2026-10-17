//! Parser behaviour the C oracle cannot show: where the port deliberately
//! differs from C, and the values a record reads from a compiled protocol.

mod common;

use std::io::Write;

use stream_rs::converter::ESC;
use stream_rs::core::commands;
use stream_rs::protocol::{codes, print_string};

fn proto_file(text: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.proto");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let path = path.to_str().unwrap().to_string();
    (dir, path)
}

#[test]
fn nul_in_an_output_string_is_escaped() {
    common::setup();
    let (_dir, path) = proto_file("p { out \"a\" nul \"b\"; }\n");
    let c = common::load(&path, "p", &mut common::Rec::default()).unwrap();
    // C leaves it bare, so the string ended after "a" and "b" was read as
    // the next command.
    assert_eq!(
        &c.commands.bytes()[..6],
        &[commands::OUT, b'a', ESC, 0, b'b', codes::EOS]
    );
}

#[test]
fn print_string_shows_a_compiled_string_as_it_was_written() {
    common::setup();
    let (_dir, path) = proto_file("p { in \"x\\r\" ? \"%3d\" \"\\_\\\\\" 1; }\n");
    let c = common::load(&path, "p", &mut common::Rec::default()).unwrap();
    let code = c.commands.bytes();
    assert_eq!(code[0], commands::IN);
    let mut shown = stream_rs::StreamBuffer::new();
    let next = print_string(&mut shown, stream_rs::cstr::CPtr::at_offset(code, 1));
    // Escaped bytes compile to ESC + byte, which C shows as \xhh.
    assert_eq!(shown.bytes(), b"x\\x0d\\?%3d\\_\\x5c\\x01");
    assert_eq!(next.offset(), code.len() - 1);
}

#[test]
fn a_record_reads_the_protocol_variables() {
    common::setup();
    stream_rs::protocol::set_path(Some("tests/oracle"));
    let rec = &mut common::Rec::default();
    let c = common::load("features.proto", "locals", rec).unwrap();
    assert!(c.ignore_extra_input);
    assert_eq!(
        (
            c.lock_timeout,
            c.read_timeout,
            c.reply_timeout,
            c.write_timeout,
            c.max_input
        ),
        (0x100, 50, 2000, 100, 10)
    );
    // pollperiod defaults to replytimeout
    assert_eq!(c.poll_period, 2000);
    assert_eq!(c.in_terminator.bytes(), b"");
    assert_eq!(c.out_terminator.bytes(), b"\n");
    assert_eq!(c.separator.bytes(), b",");
    let c = common::load("features.proto", "reference", rec).unwrap();
    assert_eq!(c.in_terminator.bytes(), b"\r\n");
    assert_eq!(c.out_terminator.bytes(), b"\r\n");
}
