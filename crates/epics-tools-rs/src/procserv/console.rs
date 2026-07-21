//! Foreground console client — the launching terminal as a party-line peer.
//!
//! C `procServ.cc:566-569`:
//!
//! ```c
//! if (inFgMode && !(logFile && strcmp(logFile, "-")==0)) {
//!     ttySetCharNoEcho(true);
//!     AddConnection(clientFactory(0));
//! }
//! ```
//!
//! In foreground mode procServ turns fd 0 into an ordinary client: the
//! operator types straight into the IOC shell, uses the command keys
//! (`^X` kill, `^R` restart-mode toggle, `^Q` logout), and sees child
//! output inline. It is a `clientItem` like any other — same greeting,
//! same telnet handling, same party-line — which is why this module
//! produces an [`IncomingClient`] and lets the supervisor's existing
//! client path do the rest, rather than adding a second console path.
//!
//! Two details are terminal-specific:
//!
//! * **Raw-ish termios** ([`TtyGuard`], C `ttySetCharNoEcho`,
//!   `procServ.cc:955-974`): `ICANON` and `ECHO` off so keystrokes reach
//!   the child immediately and are echoed by the child rather than the
//!   line discipline, `IXON` off so `^S`/`^Q` are ordinary bytes, and
//!   `VMIN = 1`. Restored on drop — C restores after its select loop
//!   (`procServ.cc:684`).
//! * **Blocking I/O on dedicated threads** ([`ConsoleStream`]): a
//!   terminal read parks indefinitely, so it must not sit on tokio's
//!   blocking pool (dropping the runtime waits for those, and the
//!   process would hang at exit until the operator pressed a key).
//!   Detached OS threads do not delay process exit.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use crate::procserv::client::{ClientPeer, ClientStream, IncomingClient};

/// Terminal fd procServ attaches in foreground mode. C passes literal
/// `0` to `clientFactory` and writes its output there too — on a tty,
/// fd 0 is open read/write, so the console is one fd, not a
/// stdin/stdout pair. Keeping that means a redirected stdout does not
/// swallow the console session.
const CONSOLE_FD: i32 = 0;

/// Saved terminal settings, restored when this guard drops.
///
/// C `ttySetCharNoEcho(bool)` (`procServ.cc:955-974`) — a no-op when
/// `isatty(0)` is false, which is why a `procserv-rs -f` with a piped
/// stdin still attaches the console client but leaves no termios to
/// restore.
pub struct TtyGuard {
    saved: Option<libc::termios>,
}

impl TtyGuard {
    /// Put the terminal into the character-at-a-time, no-echo mode the
    /// console client needs. Returns a guard that restores the previous
    /// settings on drop.
    pub fn set_char_no_echo() -> Self {
        // SAFETY: `isatty` only inspects the fd.
        if unsafe { libc::isatty(CONSOLE_FD) } != 1 {
            return Self { saved: None };
        }
        // SAFETY: `tcgetattr` fills a caller-owned termios; on failure it
        // leaves it untouched, and we then decline to change anything.
        let saved = unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(CONSOLE_FD, &mut mode) != 0 {
                tracing::debug!(
                    error = %io::Error::last_os_error(),
                    "procserv-rs: tcgetattr failed; leaving the terminal alone"
                );
                return Self { saved: None };
            }
            let original = mode;
            mode.c_iflag &= !libc::IXON;
            mode.c_lflag &= !(libc::ICANON | libc::ECHO);
            mode.c_cc[libc::VMIN] = 1;
            if libc::tcsetattr(CONSOLE_FD, libc::TCSANOW, &mode) != 0 {
                tracing::debug!(
                    error = %io::Error::last_os_error(),
                    "procserv-rs: tcsetattr failed; leaving the terminal alone"
                );
                return Self { saved: None };
            }
            original
        };
        Self { saved: Some(saved) }
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        if let Some(saved) = self.saved {
            // SAFETY: restoring the exact struct `tcgetattr` produced.
            unsafe {
                libc::tcsetattr(CONSOLE_FD, libc::TCSANOW, &saved);
            }
        }
    }
}

/// The console as an `AsyncRead + AsyncWrite` stream, so it can go
/// through the same [`crate::procserv::client::spawn_client`] path as an
/// accepted socket.
///
/// Backed by two detached OS threads doing blocking `read`/`write` on
/// fd 0 (see the module doc for why not the tokio blocking pool).
pub struct ConsoleStream {
    /// Bytes read from the terminal. Closed when the reader thread sees
    /// EOF or an error — which the client read task reports as a
    /// disconnect, exactly like a closed socket.
    rx: mpsc::Receiver<Vec<u8>>,
    /// Tail of a chunk that did not fit the caller's `ReadBuf`.
    pending: Vec<u8>,
    /// Bytes to write to the terminal. Unbounded: the writer thread is
    /// the only consumer and a tty write cannot be back-pressured
    /// meaningfully — C writes to fd 0 blocking and ignores the result
    /// (`clientFactory.cc:153-174`).
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// Attach the launching terminal as a client connection.
///
/// C `AddConnection(clientFactory(0))` — the caller supplies the
/// `inFgMode && logFile != "-"` gate, and must keep the returned
/// [`TtyGuard`] alive for the lifetime of the session.
pub fn attach_console() -> io::Result<(IncomingClient, TtyGuard)> {
    let guard = TtyGuard::set_char_no_echo();
    let stream = ConsoleStream::spawn()?;
    Ok((
        IncomingClient {
            stream: ClientStream::Console(stream),
            peer: ClientPeer::Console,
            // C `clientFactory(0)` takes the default `readonly = false`
            // (`procServ.h:94`): the console is a full user client.
            readonly: false,
        },
        guard,
    ))
}

impl ConsoleStream {
    /// Build a console stream over caller-supplied channels instead of
    /// fd 0, so the supervisor's console path can be driven without a
    /// terminal. The fd plumbing (blocking threads, termios) is what
    /// [`attach_console`] owns and is not exercised here.
    #[cfg(test)]
    pub(crate) fn from_channels(
        rx: mpsc::Receiver<Vec<u8>>,
        tx: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Self {
        Self {
            rx,
            pending: Vec::new(),
            tx,
        }
    }

    fn spawn() -> io::Result<Self> {
        // SAFETY: fd 0 is a valid fd for the lifetime of the process;
        // `try_clone_to_owned` dups it, so the threads keep working even
        // if something later replaces fd 0.
        // NOT RTEMS-SAFE if this crate is ever built for RTEMS: `F_DUPFD`
        // there calls the file's `open_h` (`libcsupport/src/fcntl.c:47-77`).
        // A console fd survives that; a libbsd SOCKET does not — see
        // `epics-ca-rs/src/server/blocking.rs::handle_client_blocking`.
        let fd: Arc<OwnedFd> =
            Arc::new(unsafe { BorrowedFd::borrow_raw(CONSOLE_FD) }.try_clone_to_owned()?);

        let (read_tx, rx) = mpsc::channel::<Vec<u8>>(16);
        let (tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let read_fd = fd.clone();
        std::thread::Builder::new()
            .name("procserv-console-read".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    // SAFETY: `read_fd` owns the fd; `buf` is valid for `len`.
                    let n = unsafe {
                        libc::read(
                            read_fd.as_raw_fd(),
                            buf.as_mut_ptr().cast(),
                            buf.len() as libc::size_t,
                        )
                    };
                    match n {
                        0 => break, // EOF — terminal closed
                        n if n < 0 => {
                            let err = io::Error::last_os_error();
                            if err.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            tracing::debug!(error = %err, "procserv-rs: console read error");
                            break;
                        }
                        n => {
                            if read_tx.blocking_send(buf[..n as usize].to_vec()).is_err() {
                                break; // supervisor gone
                            }
                        }
                    }
                }
            })?;

        let write_fd = fd;
        std::thread::Builder::new()
            .name("procserv-console-write".into())
            .spawn(move || {
                while let Some(chunk) = write_rx.blocking_recv() {
                    let mut written = 0;
                    while written < chunk.len() {
                        // SAFETY: `write_fd` owns the fd; the slice is live.
                        let n = unsafe {
                            libc::write(
                                write_fd.as_raw_fd(),
                                chunk[written..].as_ptr().cast(),
                                (chunk.len() - written) as libc::size_t,
                            )
                        };
                        if n < 0 {
                            let err = io::Error::last_os_error();
                            if err.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            // C ignores console write failures
                            // (`ignore_result(write(_fd, ...))`) and keeps the
                            // connection; so do we.
                            tracing::debug!(error = %err, "procserv-rs: console write error");
                            break;
                        }
                        written += n as usize;
                    }
                }
            })?;

        Ok(Self {
            rx,
            pending: Vec::new(),
            tx,
        })
    }
}

impl AsyncRead for ConsoleStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.pending.is_empty() {
            let n = self.pending.len().min(buf.remaining());
            let tail = self.pending.split_off(n);
            buf.put_slice(&self.pending);
            self.pending = tail;
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            // Channel closed ⇒ EOF ⇒ the client read task reports a
            // disconnect, same as a closed socket.
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Ready(Some(chunk)) => {
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                self.pending = chunk[n..].to_vec();
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for ConsoleStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.tx.send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // The writer thread drains the queue; there is no buffer here to
        // flush, and a tty write is not batched.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Never close fd 0 — it is the operator's terminal, not a socket
        // we own. Dropping `self` drops the write channel, which ends the
        // writer thread.
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The console stream must behave like the sockets the supervisor
    /// already speaks to: bytes in one end come out the other, a
    /// short `ReadBuf` keeps the remainder, and a closed input reads as
    /// EOF (which the client read task turns into a disconnect).
    ///
    /// Driven through the channel ends directly — a unit test has no
    /// terminal on fd 0, and the fd plumbing is the part `attach_console`
    /// owns.
    #[tokio::test]
    async fn console_stream_reads_chunks_and_reports_eof() {
        let (read_tx, rx) = mpsc::channel::<Vec<u8>>(4);
        let (tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut console = ConsoleStream {
            rx,
            pending: Vec::new(),
            tx,
        };

        read_tx.send(b"help".to_vec()).await.unwrap();
        let mut two = [0u8; 2];
        console.read_exact(&mut two).await.unwrap();
        assert_eq!(&two, b"he", "a short read takes only what fits");
        let mut rest = [0u8; 2];
        console.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"lp", "the remainder is kept, not dropped");

        console.write_all(b"@@@ banner\r\n").await.unwrap();
        assert_eq!(
            write_rx.try_recv().unwrap(),
            b"@@@ banner\r\n".to_vec(),
            "writes reach the terminal thread verbatim"
        );

        drop(read_tx);
        let mut eof = [0u8; 1];
        assert_eq!(
            console.read(&mut eof).await.unwrap(),
            0,
            "a closed terminal reads as EOF, so the client disconnects"
        );
    }

    /// `TtyGuard` on a non-tty (the test harness's stdin) must be inert:
    /// C's `ttySetCharNoEcho` returns early when `isatty(0) != 1`
    /// (`procServ.cc:959`), and the console client still attaches.
    #[test]
    fn tty_guard_is_inert_without_a_terminal() {
        let guard = TtyGuard::set_char_no_echo();
        // Under `cargo nextest`, fd 0 is not a tty; nothing was saved, so
        // the drop below restores nothing.
        if unsafe { libc::isatty(CONSOLE_FD) } != 1 {
            assert!(
                guard.saved.is_none(),
                "no terminal ⇒ no saved termios ⇒ nothing to restore"
            );
        }
    }
}
