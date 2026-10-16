//! The errlog worker strips ANSI escapes from a console line exactly when C's
//! `ttyConsole` is false (`errlog.c:789-795`): `isATTY` holds only for a
//! terminal whose `$TERM` is non-empty (`:222-238`). R7.0.10 libCom,
//! `errlogPrintf("rec \033[31;1mERROR\033[0m: bad \033[?25lput\n")` then
//! `errlogFlush()`, bytes as written:
//!
//! ```text
//! stderr a pipe:              rec ERROR: bad put\n
//! a raw pty, TERM=xterm:      rec \033[31;1mERROR\033[0m: bad \033[?25lput\n
//! a raw pty, TERM empty:      rec ERROR: bad put\n
//! ```
#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use epics_libcom_rs::runtime::log::{
    errlog_flush, errlog_init, errlog_printf, errlog_set_console, errlog_set_console_to_stderr,
};

const POSTED: &str = "rec \x1b[31;1mERROR\x1b[0m: bad \x1b[?25lput\n";
const STRIPPED: &[u8] = b"rec ERROR: bad put\n";

/// Everything readable from `fd` right now.
fn drain(fd: &OwnedFd) -> Vec<u8> {
    // SAFETY: O_NONBLOCK on a descriptor this test owns.
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        // SAFETY: reads into a local buffer of the stated length.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
}

/// A raw pty: `(master, slave)`, no newline translation on the way through.
fn raw_pty() -> (OwnedFd, std::fs::File) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty fills both descriptors; cfmakeraw on the slave's own
    // termios.
    unsafe {
        assert_eq!(
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null()
            ),
            0
        );
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(slave, &mut t);
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(slave, libc::TCSANOW, &t);
        (
            OwnedFd::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    }
}

fn post_and_flush() {
    errlog_printf(POSTED);
    errlog_flush();
}

#[test]
fn the_console_keeps_escapes_only_for_a_terminal_that_names_itself() {
    errlog_init(0);

    // The default console, stderr, piped.
    let (mut reader, writer) = std::io::pipe().unwrap();
    // SAFETY: fd 2 is pointed at the pipe and put back before anything can
    // panic; nextest runs this test in its own process.
    let saved = unsafe {
        let saved = libc::dup(2);
        libc::dup2(writer.as_raw_fd(), 2);
        saved
    };
    drop(writer);
    post_and_flush();
    // SAFETY: restores the descriptor replaced above.
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    let mut piped = Vec::new();
    reader.read_to_end(&mut piped).unwrap();
    assert_eq!(piped, STRIPPED, "stderr a pipe");

    // SAFETY: this test is the only thread reading the environment here.
    unsafe { std::env::set_var("TERM", "xterm") };
    let (master, slave) = raw_pty();
    errlog_set_console(slave);
    post_and_flush();
    assert_eq!(drain(&master), POSTED.as_bytes(), "a pty, TERM=xterm");

    // SAFETY: as above.
    unsafe { std::env::set_var("TERM", "") };
    let (master, slave) = raw_pty();
    errlog_set_console(slave);
    post_and_flush();
    assert_eq!(drain(&master), STRIPPED, "a pty, TERM empty");

    errlog_set_console_to_stderr();
}
