//! C `epicsStdio.c`: each thread's own stdin, stdout and stderr.
//!
//! An iocsh redirect sets them for the length of one line
//! (`iocsh.cpp:401-451`), and C's `epicsStdio.h` turns every `printf`,
//! `puts` and `stdout`/`stderr`/`stdin` of a file that includes it into
//! these (`epicsStdio.h:71-101`). So whatever a command calls, at any depth,
//! prints into the line's `> file`; a thread that set none prints to the
//! process's streams. [`epics_print!`](crate::epics_print!) is that `printf`.

use std::cell::RefCell;
use std::io::{BufRead, Write};

use crate::runtime::log::{StagedStderr, write_whole};

thread_local! {
    static STDIN: RefCell<Option<Box<dyn BufRead>>> = const { RefCell::new(None) };
    static STDOUT: RefCell<Option<Box<dyn Write>>> = const { RefCell::new(None) };
    static STDERR: RefCell<Option<Box<dyn Write>>> = const { RefCell::new(None) };
}

/// `f` with the thread's stdin set to `r`: C `startRedirect`'s
/// `epicsSetThreadStdin`, undone after as `stopRedirect` does.
pub fn redirect_stdin<T>(r: impl BufRead + 'static, f: impl FnOnce() -> T) -> T {
    scoped(&STDIN, Box::new(r), f)
}

/// `f` with the thread's stdout set to `w` (`epicsSetThreadStdout`).
pub fn redirect_stdout<T>(w: impl Write + 'static, f: impl FnOnce() -> T) -> T {
    scoped(&STDOUT, Box::new(w), f)
}

/// `f` with the thread's stderr set to `w` (`epicsSetThreadStderr`).
pub fn redirect_stderr<T>(w: impl Write + 'static, f: impl FnOnce() -> T) -> T {
    scoped(&STDERR, Box::new(w), f)
}

/// The stream that was set before is back when `f` returns or unwinds.
fn scoped<S: ?Sized + 'static, T>(
    key: &'static std::thread::LocalKey<RefCell<Option<Box<S>>>>,
    stream: Box<S>,
    f: impl FnOnce() -> T,
) -> T {
    struct Restore<S: ?Sized + 'static> {
        key: &'static std::thread::LocalKey<RefCell<Option<Box<S>>>>,
        prev: Option<Box<S>>,
    }
    impl<S: ?Sized + 'static> Drop for Restore<S> {
        fn drop(&mut self) {
            let prev = self.prev.take();
            let _ = self.key.try_with(|cell| cell.replace(prev));
        }
    }
    let _restore = Restore {
        key,
        prev: key.with(|cell| cell.replace(Some(stream))),
    };
    f()
}

/// C `epicsGetStdin`: the thread's stdin, else the process's.
pub fn with_stdin<R>(f: impl FnOnce(&mut dyn BufRead) -> R) -> R {
    on_thread(&STDIN, |r: &mut (dyn BufRead + 'static)| f(r))
        .unwrap_or_else(|f| f(&mut std::io::stdin().lock()))
}

/// C `epicsGetStdout`: the thread's stdout, else the process's.
pub fn with_stdout<R>(f: impl FnOnce(&mut dyn Write) -> R) -> R {
    on_thread(&STDOUT, |w: &mut (dyn Write + 'static)| f(w))
        .unwrap_or_else(|f| f(&mut std::io::stdout()))
}

/// C `epicsGetStderr`: the thread's stderr, else the process's.
pub fn with_stderr<R>(f: impl FnOnce(&mut dyn Write) -> R) -> R {
    on_thread(&STDERR, |w: &mut (dyn Write + 'static)| f(w))
        .unwrap_or_else(|f| f(&mut StagedStderr))
}

/// `f` run on the thread's stream; `f` back when none is set, the thread is
/// exiting, or a print on this thread is already writing to it.
fn on_thread<S: ?Sized + 'static, R, F: FnOnce(&mut S) -> R>(
    key: &'static std::thread::LocalKey<RefCell<Option<Box<S>>>>,
    f: F,
) -> Result<R, F> {
    let mut f = Some(f);
    let ran = key.try_with(|cell| {
        let mut slot = cell.try_borrow_mut().ok()?;
        let stream = slot.as_deref_mut()?;
        f.take().map(|f| f(stream))
    });
    match ran {
        Ok(Some(r)) => Ok(r),
        _ => Err(f.expect("f runs only when it returns")),
    }
}

/// [`epics_print!`](crate::epics_print!)'s body: the text staged whole and
/// written to the thread's stdout in one write.
#[doc(hidden)]
pub fn stdout_whole(args: std::fmt::Arguments<'_>) {
    let _ = with_stdout(|out| write_whole(out, args));
}

/// C `printf("%s", bytes)`: raw bytes to the thread's stdout.
pub fn stdout_bytes(bytes: &[u8]) {
    let _ = with_stdout(|out| out.write_all(bytes));
}

/// C `printf` in a file that includes `epicsStdio.h`: to the thread's stdout.
#[macro_export]
macro_rules! epics_print {
    ($($arg:tt)*) => {
        $crate::runtime::stdio::stdout_whole(::core::format_args!($($arg)*))
    };
}

/// [`epics_print!`](crate::epics_print!) with a newline.
#[macro_export]
macro_rules! epics_println {
    () => {
        $crate::runtime::stdio::stdout_whole(::core::format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::runtime::stdio::stdout_whole(::core::format_args!(
            "{}\n",
            ::core::format_args!($($arg)*)
        ))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Sink(std::rc::Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_print_follows_the_thread_stdout_and_its_restore() {
        let sink = Sink::default();
        let inner = Sink::default();
        redirect_stdout(sink.clone(), || {
            crate::epics_println!("a {}", 1);
            redirect_stdout(inner.clone(), || crate::epics_println!("i"));
            stdout_bytes(b"\xff\n");
            let other = std::thread::spawn(|| {
                let sink = Sink::default();
                redirect_stdout(sink.clone(), || crate::epics_println!("b"));
                sink.0.take()
            });
            assert_eq!(other.join().unwrap(), b"b\n");
        });
        assert_eq!(*inner.0.borrow(), b"i\n");
        crate::epics_println!("c");
        assert_eq!(*sink.0.borrow(), b"a 1\n\xff\n");
    }

    #[test]
    fn an_unwinding_redirect_is_restored() {
        let outer = Sink::default();
        redirect_stdout(outer.clone(), || {
            let unwound = std::panic::catch_unwind(|| {
                redirect_stdout(Sink::default(), || panic!("inside"));
            });
            assert!(unwound.is_err());
            crate::epics_println!("after");
        });
        assert_eq!(*outer.0.borrow(), b"after\n");
    }

    #[test]
    fn a_print_inside_a_print_reaches_the_process_stream() {
        let sink = Sink::default();
        redirect_stdout(sink.clone(), || {
            with_stdout(|out| {
                crate::epics_println!("nested");
                out.write_all(b"outer\n")
            })
            .unwrap();
        });
        assert_eq!(*sink.0.borrow(), b"outer\n");
    }

    #[test]
    fn the_thread_stdin_and_stderr_are_swapped_alike() {
        let mut buf = String::new();
        redirect_stdin(&b"line\nrest"[..], || with_stdin(|r| r.read_line(&mut buf))).unwrap();
        assert_eq!(buf, "line\n");

        let sink = Sink::default();
        redirect_stderr(sink.clone(), || with_stderr(|w| w.write_all(b"e\n"))).unwrap();
        assert_eq!(*sink.0.borrow(), b"e\n");
    }
}
