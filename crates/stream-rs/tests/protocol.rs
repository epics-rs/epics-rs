//! Parser behaviour the C oracle cannot show: where the port deliberately
//! differs from C, and the values a record reads from a compiled protocol.

mod common;

use std::io::Write;

use stream_rs::converter::ESC;
use stream_rs::core::{IGNORE_EXTRA_INPUT, commands};
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
    let (core, ok) = common::load(&path, "p", "rec", &[]);
    assert!(ok);
    // C leaves it bare, so the string ended after "a" and "b" was read as
    // the next command.
    assert_eq!(
        &core.lock().commands.borrow().bytes()[..6],
        &[commands::OUT, b'a', ESC, 0, b'b', codes::EOS]
    );
}

#[test]
fn print_string_shows_a_compiled_string_as_it_was_written() {
    common::setup();
    let (_dir, path) = proto_file("p { in \"x\\r\" ? \"%3d\" \"\\_\\\\\" 1; }\n");
    let (core, ok) = common::load(&path, "p", "rec", &[]);
    assert!(ok);
    let code = core.lock().commands.borrow().clone();
    let code = code.bytes();
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
    let (core, ok) = common::load("features.proto", "locals", "rec", &[]);
    assert!(ok);
    let c = core.lock();
    assert_eq!(c.flags.get() & IGNORE_EXTRA_INPUT, IGNORE_EXTRA_INPUT);
    assert_eq!(
        (
            c.lock_timeout.get(),
            c.read_timeout.get(),
            c.reply_timeout.get(),
            c.write_timeout.get(),
            c.max_input.get()
        ),
        (0x100, 50, 2000, 100, 10)
    );
    // pollperiod defaults to replytimeout
    assert_eq!(c.poll_period.get(), 2000);
    assert_eq!(c.in_terminator.borrow().bytes(), b"");
    assert_eq!(c.out_terminator.borrow().bytes(), b"\n");
    assert_eq!(c.separator.borrow().bytes(), b",");
    // An empty interminator is defined: the bus is told so.
    assert_eq!(c.get_in_terminator(), Some(Vec::new()));
    let (core, ok) = common::load("features.proto", "reference", "rec", &[]);
    assert!(ok);
    let c = core.lock();
    assert_eq!(c.in_terminator.borrow().bytes(), b"\r\n");
    assert_eq!(c.out_terminator.borrow().bytes(), b"\r\n");
}
