//! The per-client send path's transient-failure policy — the one place a CA
//! server decides whether a failed write means "this client is gone" or "the
//! host ran out of network buffers for a moment".
//!
//! The receive side's counterpart is [`super::recv`]: that module owns how
//! bytes coming *off* a client socket may grow memory, this one owns how bytes
//! going *onto* one may fail.
//!
//! # Invariant
//!
//! **A transient send failure retries the same bytes; only a real error
//! disconnects the client.**
//!
//! C states the same rule as a loop rather than a type. `cas_send_bs_msg`
//! (`rsrv/caserverio.c:65-101`) keeps `send()`ing from `pclient->send.buf`
//! until the frame is gone, and inside that loop exactly two errno values are
//! *not* failures:
//!
//! * `SOCK_EINTR` — `continue`, retry immediately;
//! * `SOCK_ENOBUFS` — `errlogPrintf("CAS: Out of network buffers, retrying
//!   send in 15 seconds")`, `epicsThreadSleep(15.0)`, `continue`.
//!
//! Everything else falls through to the hangup/error path that ends the
//! circuit. A partial `send()` is not an error at all: C `memmove`s the
//! remainder down and loops, so a retry never re-sends a byte the peer already
//! has.
//!
//! Both port drivers wrote frames with `write_all`, whose contract is the
//! opposite — *any* `Err` ends the call, and `?` then ended the client. An
//! ENOBUFS burst that C rides out by sleeping disconnected every CA client
//! this IOC was serving.
//!
//! # Why an adapter and not a retry at each call site
//!
//! The blocking driver has two write sites and the hosted driver three, and
//! the hosted driver's own doc comment claimed `drain_and_flush` was "the ONLY
//! place server-produced bytes reach the socket" while the unsolicited
//! `CA_PROTO_VERSION` greeting and the out-of-band monitor frame wrote the
//! socket directly beside it. A retry bolted onto the drain would have left
//! those two exactly as they were, and the next write site added would start
//! out broken again.
//!
//! So the policy lives *under* every writer instead: [`RetryTransient`] and
//! `RetryTransientAsync` wrap the socket itself, so `write_all`, `flush`,
//! and `BufWriter`'s internal spill all inherit it and no call site carries a
//! branch. Resumption is exact for the same reason C's is — the adapter sits
//! at the `write`/`poll_write` level, where a retry re-offers only the bytes
//! the kernel did not take.
//!
//! (`RetryTransientAsync` is deliberately not an intra-doc link: it is
//! `cfg(not(epics_embedded_target))`, so on an embedded target the link has no
//! target and `rustdoc -D warnings` fails the build.)

use std::io;
use std::time::Duration;

/// C `caserverio.c:99` — `epicsThreadSleep(15.0)` between ENOBUFS retries.
const OUT_OF_BUFFERS_RETRY: Duration = Duration::from_secs(15);

/// This host's "out of network buffers" code for a socket send. On Unix that
/// is `ENOBUFS`; Winsock reports the same condition as `WSAENOBUFS` (10055),
/// which is what `SOCKERRNO` resolves to in C's `#ifdef` for this branch.
///
/// Named rather than inlined so the tests can assert against the very constant
/// the classifier reads. A test that spelled the number itself would be
/// asserting POSIX errno on Windows, where `from_raw_os_error` takes Win32
/// codes and 105 is not `WSAENOBUFS`.
#[cfg(windows)]
pub(crate) const OUT_OF_BUFFERS: i32 = 10055;
#[cfg(not(windows))]
pub(crate) const OUT_OF_BUFFERS: i32 = libc::ENOBUFS;

/// Did the host refuse this write for want of network buffers?
fn is_out_of_buffers(e: &io::Error) -> bool {
    e.raw_os_error() == Some(OUT_OF_BUFFERS)
}

/// One console record per retry, on C's sink and with C's wording
/// (`caserverio.c:97-98`).
fn announce_out_of_buffers() {
    epics_base_rs::runtime::log::errlog_sev_printf(
        epics_base_rs::runtime::log::ErrlogSevEnum::Major,
        "CAS: Out of network buffers, retrying send in 15 seconds",
    );
}

/// A blocking writer that applies the module invariant: `EINTR` retries at
/// once, `ENOBUFS` sleeps [`OUT_OF_BUFFERS_RETRY`] and retries, anything else
/// is returned to the caller as the error it is.
pub(crate) struct RetryTransient<W>(pub(crate) W);

impl<W: io::Write> io::Write for RetryTransient<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.0.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if is_out_of_buffers(&e) => {
                    announce_out_of_buffers();
                    std::thread::sleep(OUT_OF_BUFFERS_RETRY);
                }
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// The hosted driver's equivalent. The backoff is a `Sleep` held across polls
/// rather than a blocking sleep, so a buffer-starved circuit parks its own
/// task and leaves the rest of the reactor running — where the blocking
/// driver, like C, parks the one thread that owns this client.
///
/// `Poll::Pending` is the whole trick: it consumes no bytes, so `BufWriter`
/// and `write_all` above resume from exactly where the kernel stopped.
#[cfg(not(epics_embedded_target))]
pub(crate) struct RetryTransientAsync<W> {
    inner: W,
    backoff: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

#[cfg(not(epics_embedded_target))]
impl<W> RetryTransientAsync<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            backoff: None,
        }
    }
}

#[cfg(not(epics_embedded_target))]
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for RetryTransientAsync<W> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        use std::future::Future;
        use std::pin::Pin;
        use std::task::Poll;

        let me = self.get_mut();
        if let Some(backoff) = me.backoff.as_mut() {
            std::task::ready!(backoff.as_mut().poll(cx));
            me.backoff = None;
        }
        loop {
            match Pin::new(&mut me.inner).poll_write(cx, buf) {
                Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Poll::Ready(Err(e)) if is_out_of_buffers(&e) => {
                    announce_out_of_buffers();
                    let mut backoff = Box::pin(tokio::time::sleep(OUT_OF_BUFFERS_RETRY));
                    if backoff.as_mut().poll(cx).is_pending() {
                        me.backoff = Some(backoff);
                        return Poll::Pending;
                    }
                    // The delay was already over; retry without yielding.
                }
                other => return other,
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// How the fixture should spell its failure.
    ///
    /// The two discriminators in this module read different things — the EINTR
    /// branch reads `io::ErrorKind`, the ENOBUFS branch reads the raw OS code —
    /// so a fixture has to be able to speak both. Spelling every failure as a
    /// POSIX errno instead is what made these tests pass on Unix and fail on
    /// Windows, where `from_raw_os_error` takes Win32 codes and `libc::EINTR`
    /// (4) decodes as "cannot open the file", not `Interrupted`.
    #[derive(Clone, Copy)]
    enum Fail {
        Kind(io::ErrorKind),
        Code(i32),
    }

    impl Fail {
        fn error(self) -> io::Error {
            match self {
                Fail::Kind(k) => io::Error::from(k),
                Fail::Code(c) => io::Error::from_raw_os_error(c),
            }
        }
    }

    /// A writer that fails a caller-chosen way for the first `fails` calls,
    /// then behaves. Records what it was actually offered so a test can prove
    /// no byte was written twice.
    struct FlakyWriter {
        fails: usize,
        how: Fail,
        offered: Vec<Vec<u8>>,
        accepted: Vec<u8>,
        /// Take only this many bytes per successful `write`, to exercise the
        /// partial-write path `write_all` drives.
        chunk: usize,
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.offered.push(buf.to_vec());
            if self.fails > 0 {
                self.fails -= 1;
                return Err(self.how.error());
            }
            let n = self.chunk.min(buf.len());
            self.accepted.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn flaky(fails: usize, how: Fail, chunk: usize) -> FlakyWriter {
        FlakyWriter {
            fails,
            how,
            offered: Vec::new(),
            accepted: Vec::new(),
            chunk,
        }
    }

    /// `EINTR` is retried in place, as C's `SOCK_EINTR: continue` does, and the
    /// caller never sees it. Written as the `ErrorKind` the retry loop reads,
    /// not as an errno, so it exercises the same branch on every platform.
    #[test]
    fn eintr_is_retried_not_returned() {
        let mut w = RetryTransient(flaky(3, Fail::Kind(io::ErrorKind::Interrupted), 64));
        w.write_all(b"hello").expect("EINTR must not surface");
        assert_eq!(w.0.accepted, b"hello");
    }

    /// A partial write followed by a retryable failure re-offers only the
    /// bytes the kernel did not take — C's `memmove` of the remainder. A retry
    /// that restarted the frame would show a duplicated prefix here.
    #[test]
    fn a_retry_after_a_partial_write_does_not_duplicate_bytes() {
        // Take 2 bytes per successful write, and fail the very first call.
        let mut w = RetryTransient(flaky(1, Fail::Kind(io::ErrorKind::Interrupted), 2));
        w.write_all(b"abcdef").expect("write_all");
        assert_eq!(w.0.accepted, b"abcdef", "each byte delivered exactly once");
        // "abcdef" (failed), then "abcdef", "cdef", "ef".
        assert_eq!(
            w.0.offered,
            vec![
                b"abcdef".to_vec(),
                b"abcdef".to_vec(),
                b"cdef".to_vec(),
                b"ef".to_vec()
            ],
            "a retry re-offers the remainder, never the whole frame"
        );
    }

    /// A non-transient failure is the caller's to handle — that is what still
    /// ends a circuit whose peer really is gone.
    #[test]
    fn a_real_error_is_returned_unchanged() {
        let mut w = RetryTransient(flaky(1, Fail::Kind(io::ErrorKind::ConnectionReset), 64));
        let e = w.write_all(b"x").expect_err("a reset must surface");
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
    }

    /// The classifier is what separates the two, and it must fire on exactly
    /// one code. Asserted against [`OUT_OF_BUFFERS`] itself and its immediate
    /// neighbours, so it holds on every platform's numbering rather than on
    /// POSIX errno — and so an off-by-one in that constant fails here.
    #[test]
    fn only_out_of_buffers_counts_as_out_of_buffers() {
        assert!(is_out_of_buffers(&io::Error::from_raw_os_error(
            OUT_OF_BUFFERS
        )));
        for other in [OUT_OF_BUFFERS - 1, OUT_OF_BUFFERS + 1] {
            assert!(
                !is_out_of_buffers(&io::Error::from_raw_os_error(other)),
                "os error {other} must not be treated as out-of-buffers"
            );
        }
        // A failure carrying no OS code at all must not sweep in either.
        assert!(!is_out_of_buffers(&io::Error::from(
            io::ErrorKind::ConnectionReset
        )));
    }

    /// ENOBUFS retries the same bytes. Driven with a zero delay would be a
    /// different function, so this asserts the classification and the loop
    /// through a single failure and measures that the 15 s sleep happened.
    #[test]
    #[ignore = "sleeps 15s — C's caserverio.c:99 delay, run with --ignored"]
    fn enobufs_sleeps_then_retries_the_same_bytes() {
        let start = std::time::Instant::now();
        let mut w = RetryTransient(flaky(1, Fail::Code(OUT_OF_BUFFERS), 64));
        w.write_all(b"frame").expect("ENOBUFS must not surface");
        assert_eq!(w.0.accepted, b"frame");
        assert!(
            start.elapsed() >= OUT_OF_BUFFERS_RETRY,
            "the retry must wait C's 15 seconds"
        );
    }
}
