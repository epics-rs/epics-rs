//! `StreamError.cc` / `StreamError.h`: error and debug output.
//!
//! An error goes to stderr as
//! `<red> [timestamp ][threadname ][file line N: ]message<reset>`, and,
//! when a debug file is set, to that file as `[timestamp ][threadname ]message`.
//! Messages are byte strings: a token quoted in a message can hold any byte
//! the protocol file held.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicI32, Ordering};

use parking_lot::{Mutex, RwLock};

/// `streamDebug`: debug level, 0 is off.
pub static STREAM_DEBUG: AtomicI32 = AtomicI32::new(0);
/// `streamError`: 0 disables error messages (unless debugging).
pub static STREAM_ERROR: AtomicI32 = AtomicI32::new(1);
/// `streamMsgTimeStamped`: 0 disables timestamps on errors and debug output.
pub static STREAM_MSG_TIME_STAMPED: AtomicI32 = AtomicI32::new(1);
/// `streamDebugColored`; -1 until first read, then `isatty(stderr)`.
static STREAM_DEBUG_COLORED: AtomicI32 = AtomicI32::new(-1);

/// `StreamDebugFile`
static STREAM_DEBUG_FILE: Mutex<Option<Box<dyn Write + Send>>> = Mutex::new(None);

type TimestampFn = fn() -> String;
type ThreadNameFn = fn() -> Option<String>;

/// `StreamPrintTimestampFunction`
static PRINT_TIMESTAMP: RwLock<TimestampFn> = RwLock::new(print_timestamp);
/// `StreamGetThreadNameFunction`
static GET_THREAD_NAME: RwLock<Option<ThreadNameFn>> = RwLock::new(None);

thread_local! {
    /// Where this thread's stderr output goes instead, when set (tests).
    static CAPTURE: std::cell::RefCell<Option<Vec<u8>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn stream_debug() -> i32 {
    STREAM_DEBUG.load(Ordering::Relaxed)
}

pub fn set_stream_debug(level: i32) {
    STREAM_DEBUG.store(level, Ordering::Relaxed);
}

pub fn set_stream_error(on: i32) {
    STREAM_ERROR.store(on, Ordering::Relaxed);
}

pub fn set_stream_msg_time_stamped(on: i32) {
    STREAM_MSG_TIME_STAMPED.store(on, Ordering::Relaxed);
}

pub fn stream_debug_colored() -> bool {
    match STREAM_DEBUG_COLORED.load(Ordering::Relaxed) {
        -1 => {
            let tty = std::io::stderr().is_terminal();
            STREAM_DEBUG_COLORED.store(tty as i32, Ordering::Relaxed);
            tty
        }
        v => v != 0,
    }
}

pub fn set_stream_debug_colored(on: bool) {
    STREAM_DEBUG_COLORED.store(on as i32, Ordering::Relaxed);
}

/// Set `StreamDebugFile`; `None` closes it.
pub fn set_stream_debug_file(file: Option<Box<dyn Write + Send>>) {
    *STREAM_DEBUG_FILE.lock() = file;
}

pub fn set_print_timestamp_function(f: TimestampFn) {
    *PRINT_TIMESTAMP.write() = f;
}

pub fn set_get_thread_name_function(f: Option<ThreadNameFn>) {
    *GET_THREAD_NAME.write() = f;
}

/// Send this thread's stderr output to a buffer until [`take_captured`].
pub fn capture_stderr() {
    CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
}

/// End the capture and return what was written.
pub fn take_captured() -> Vec<u8> {
    CAPTURE.with(|c| c.borrow_mut().take()).unwrap_or_default()
}

/// Where every thread's stderr output goes while a [`ProcessCapture`] is
/// alive, but for a thread's own [`capture_stderr`].
static PROCESS_CAPTURE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// One [`ProcessCapture`] at a time.
static PROCESS_CAPTURE_OWNER: Mutex<()> = Mutex::new(());

/// The stderr output of all threads, the bus's and the timers' included,
/// for a test to compare; [`capture_process_stderr`] makes it. Any
/// thread of the process writes into it, a test's that holds none
/// included, so it is made only under nextest's process-per-test mode.
pub struct ProcessCapture {
    _owner: parking_lot::MutexGuard<'static, ()>,
}

/// Send the stderr output of every thread to one buffer until the
/// capture is dropped, waiting until no other capture is alive.
///
/// # Panics
///
/// Outside `NEXTEST_EXECUTION_MODE=process-per-test`, where another
/// test's threads would write into the capture.
pub fn capture_process_stderr() -> ProcessCapture {
    assert_eq!(
        std::env::var("NEXTEST_EXECUTION_MODE").as_deref(),
        Ok("process-per-test"),
        "capture_process_stderr needs a process per test: run under cargo nextest"
    );
    let owner = PROCESS_CAPTURE_OWNER.lock();
    *PROCESS_CAPTURE.lock() = Some(Vec::new());
    ProcessCapture { _owner: owner }
}

impl ProcessCapture {
    /// What was written since the capture began or the last take.
    pub fn take(&self) -> Vec<u8> {
        PROCESS_CAPTURE
            .lock()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}

impl Drop for ProcessCapture {
    fn drop(&mut self) {
        *PROCESS_CAPTURE.lock() = None;
    }
}

fn write_stderr(bytes: &[u8]) {
    let captured = CAPTURE.with(|c| match c.borrow_mut().as_mut() {
        Some(buf) => {
            buf.extend_from_slice(bytes);
            true
        }
        None => false,
    }) || match PROCESS_CAPTURE.lock().as_mut() {
        Some(buf) => {
            buf.extend_from_slice(bytes);
            true
        }
        None => false,
    };
    if !captured {
        let _ = std::io::stderr().write_all(bytes);
    }
}

/// The default `StreamPrintTimestampFunction`: local time.
fn print_timestamp() -> String {
    chrono::Local::now().format("%Y/%m/%d %H:%M:%S").to_string()
}

/// `enum AnsiMode`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnsiMode {
    ReverseVideo,
    NotReverseVideo,
    BgWhite,
    Reset,
    RedBold,
}

/// `ansiEscape`: the escape sequence, or "" when output is not coloured.
pub fn ansi_escape(mode: AnsiMode) -> &'static str {
    const ESCAPES: [&str; 5] = ["\x1b[7m", "\x1b[27m", "\x1b[47m", "\x1b[0m", "\x1b[31;1m"];
    if stream_debug_colored() {
        ESCAPES[mode as usize]
    } else {
        ""
    }
}

/// `StreamVError(line, file, fmt, args)` with the message already formatted.
pub fn stream_verror(line: i32, file: Option<&[u8]>, msg: &[u8]) {
    if STREAM_ERROR.load(Ordering::Relaxed) == 0 && stream_debug() == 0 {
        return; // Error logging disabled
    }
    let timestamp =
        (STREAM_MSG_TIME_STAMPED.load(Ordering::Relaxed) != 0).then(|| (*PRINT_TIMESTAMP.read())());
    let threadname = GET_THREAD_NAME.read().and_then(|f| f());
    let mut prefix = Vec::new();
    if let Some(t) = &timestamp {
        prefix.extend_from_slice(t.as_bytes());
        prefix.push(b' ');
    }
    if let Some(t) = &threadname {
        prefix.extend_from_slice(t.as_bytes());
        prefix.push(b' ');
    }
    if let Some(f) = STREAM_DEBUG_FILE.lock().as_mut() {
        let _ = f.write_all(&prefix);
        let _ = f.write_all(msg);
        let _ = f.flush();
    }
    let mut out = Vec::new();
    out.extend_from_slice(ansi_escape(AnsiMode::RedBold).as_bytes());
    out.extend_from_slice(&prefix);
    if let Some(file) = file {
        out.extend_from_slice(file);
        out.extend_from_slice(format!(" line {line}: ").as_bytes());
    }
    out.extend_from_slice(msg);
    out.extend_from_slice(ansi_escape(AnsiMode::Reset).as_bytes());
    write_stderr(&out);
}

/// `StreamError(fmt, ...)`
pub fn stream_error(msg: &[u8]) {
    stream_verror(0, None, msg);
}

/// `StreamError(line, file, fmt, ...)`
pub fn stream_error_at(line: i32, file: &[u8], msg: &[u8]) {
    stream_verror(line, Some(file), msg);
}

/// `StreamDebugClass::print`: `[timestamp ][threadname ][file:[line:] ]msg`
/// to the debug file, or stderr.
pub fn stream_debug_print(file: Option<&str>, line: u32, msg: &[u8]) {
    let mut out = Vec::new();
    if STREAM_MSG_TIME_STAMPED.load(Ordering::Relaxed) != 0 {
        out.extend_from_slice((*PRINT_TIMESTAMP.read())().as_bytes());
        out.push(b' ');
    }
    if let Some(f) = *GET_THREAD_NAME.read() {
        // C prints whatever the function returns, NULL included.
        out.extend_from_slice(f().as_deref().unwrap_or("(null)").as_bytes());
        out.push(b' ');
    }
    if let Some(file) = file {
        let base = file.rsplit('/').next().unwrap_or(file);
        out.extend_from_slice(base.as_bytes());
        out.push(b':');
        if line != 0 {
            out.extend_from_slice(format!("{line}:").as_bytes());
        }
        out.push(b' ');
    }
    out.extend_from_slice(msg);
    let mut debug_file = STREAM_DEBUG_FILE.lock();
    match debug_file.as_mut() {
        Some(f) => {
            let _ = f.write_all(&out);
            let _ = f.flush();
        }
        None => write_stderr(&out),
    }
}

/// A message piece: raw bytes, text, or a `%c` byte.
pub trait MsgPart {
    fn put(&self, out: &mut Vec<u8>);
}

impl MsgPart for [u8] {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
}

impl<const N: usize> MsgPart for [u8; N] {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
}

impl MsgPart for Vec<u8> {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self);
    }
}

impl MsgPart for str {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.as_bytes());
    }
}

impl MsgPart for String {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.as_bytes());
    }
}

impl MsgPart for crate::buffer::StreamBuffer {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.bytes());
    }
}

/// printf `%c`: one byte (an `int` argument is cut to `unsigned char`).
pub struct Ch(pub i32);

impl MsgPart for Ch {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(self.0 as u8);
    }
}

impl<T: MsgPart + ?Sized> MsgPart for &T {
    fn put(&self, out: &mut Vec<u8>) {
        (**self).put(out);
    }
}

/// Concatenate message parts into one byte string.
#[macro_export]
macro_rules! cmsg {
    ($($part:expr),* $(,)?) => {{
        let mut out: Vec<u8> = Vec::new();
        $( $crate::error::MsgPart::put(&$part, &mut out); )*
        out
    }};
}

/// `debug(...)`: print when `streamDebug` is on.
#[macro_export]
macro_rules! debug {
    ($($part:expr),* $(,)?) => {
        if $crate::error::stream_debug() != 0 {
            $crate::error::stream_debug_print(Some(file!()), line!(), &$crate::cmsg!($($part),*));
        }
    };
}

/// `debug2(...)`: print when `streamDebug` is 2 or more.
#[macro_export]
macro_rules! debug2 {
    ($($part:expr),* $(,)?) => {
        if $crate::error::stream_debug() >= 2 {
            $crate::error::stream_debug_print(Some(file!()), line!(), &$crate::cmsg!($($part),*));
        }
    };
}

/// The switches are process-wide: a unit test that sets or reads them holds
/// this for its whole run.
#[cfg(test)]
pub(crate) static TEST_SWITCHES: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_carries_colour_timestamp_thread_and_location() {
        let _switches = TEST_SWITCHES.lock();
        set_stream_debug_colored(true);
        set_stream_msg_time_stamped(1);
        set_print_timestamp_function(|| "2026/10/18 12:00:00".into());
        set_get_thread_name_function(Some(|| Some("_main_".into())));
        capture_stderr();
        stream_error_at(7, b"x.proto", b"Unexpected 'a'\n");
        stream_error(b"plain\n");
        assert_eq!(
            take_captured(),
            b"\x1b[31;1m2026/10/18 12:00:00 _main_ x.proto line 7: Unexpected 'a'\n\x1b[0m\
              \x1b[31;1m2026/10/18 12:00:00 _main_ plain\n\x1b[0m"
                .to_vec()
        );
    }

    #[test]
    fn errors_are_silent_only_when_both_switches_are_off() {
        let _switches = TEST_SWITCHES.lock();
        set_stream_debug_colored(false);
        set_stream_msg_time_stamped(0);
        set_get_thread_name_function(None);
        capture_stderr();
        set_stream_error(0);
        stream_error(b"a\n");
        set_stream_debug(1);
        stream_error(b"b\n");
        set_stream_debug(0);
        set_stream_error(1);
        stream_error(b"c\n");
        assert_eq!(take_captured(), b"b\nc\n".to_vec());
    }

    #[test]
    fn a_process_capture_takes_a_spawned_threads_lines() {
        let _switches = TEST_SWITCHES.lock();
        set_stream_debug_colored(false);
        set_stream_msg_time_stamped(0);
        set_get_thread_name_function(None);
        let capture = capture_process_stderr();
        std::thread::spawn(|| {
            stream_error(b"bus\n");
            stream_debug_print(Some("asyn.rs"), 3, b"lock\n");
        })
        .join()
        .unwrap();
        stream_error(b"main\n");
        assert_eq!(capture.take(), b"bus\nasyn.rs:3: lock\nmain\n".to_vec());
        // a thread's own capture keeps its lines
        capture_stderr();
        stream_error(b"own\n");
        assert_eq!(take_captured(), b"own\n".to_vec());
        assert_eq!(capture.take(), Vec::<u8>::new());
        drop(capture);

        // the next capture waits for the one alive, and starts empty
        let first = capture_process_stderr();
        stream_error(b"first\n");
        let (tx, rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            let capture = capture_process_stderr();
            tx.send(()).unwrap();
            capture.take()
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(rx.try_recv().is_err(), "a second capture began early");
        assert_eq!(first.take(), b"first\n".to_vec());
        drop(first);
        assert_eq!(second.join().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn debug_shows_the_file_basename_and_line() {
        let _switches = TEST_SWITCHES.lock();
        set_stream_msg_time_stamped(0);
        set_get_thread_name_function(None);
        capture_stderr();
        stream_debug_print(Some("src/protocol.rs"), 12, b"x\n");
        stream_debug_print(Some("a.rs"), 0, b"y\n");
        assert_eq!(take_captured(), b"protocol.rs:12: x\na.rs: y\n".to_vec());
    }
}
