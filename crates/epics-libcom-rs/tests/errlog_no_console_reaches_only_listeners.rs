//! C `errlogVprintfNoConsole` (`errlog.c:356-366`) commits with `localEcho`
//! false, so the worker never writes the line to the console
//! (`errlog.c:790-796`); listeners hear it as they hear every message.
#![cfg(unix)]

use std::io::Read;
use std::sync::{Arc, Mutex};

use epics_libcom_rs::runtime::log::{
    errlog_add_listener, errlog_flush, errlog_init, errlog_printf_no_console, errlog_set_console,
    errlog_vprintf,
};

#[test]
fn a_no_console_post_reaches_a_listener_and_not_the_console() {
    errlog_init(0);
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    errlog_set_console(std::fs::File::from(std::os::fd::OwnedFd::from(writer)));
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.to_string()));

    errlog_printf_no_console(format_args!("listener only\n"));
    // A console line after it, so the read below ends on a known byte.
    errlog_vprintf(format_args!("console end\n"));
    errlog_flush();

    let mut console = String::new();
    let mut byte = [0u8; 1];
    while !console.ends_with("console end\n") {
        reader.read_exact(&mut byte).expect("console pipe");
        console.push(byte[0] as char);
    }
    assert!(
        !console.contains("listener only"),
        "a NoConsole post was written to the console: {console:?}"
    );
    assert!(
        heard.lock().unwrap().iter().any(|m| m == "listener only\n"),
        "the listener must still receive it: {:?}",
        heard.lock().unwrap()
    );
}
