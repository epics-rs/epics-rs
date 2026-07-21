//! Blocking, thread-per-connection PVA server driver (RTEMS phase 6 item 5,
//! stage 3 — `doc/pva-rtems-item5-design.md` §1, §3).
//!
//! The second driver, beside [`super::accept`]. It serves one connection with
//! three threads and **no reactor**:
//!
//! ```text
//!   socket --read--> reader thread --Vec<u8> chunks--> ChannelReader
//!                                                         |
//!                                    operation thread: block_on_sync(
//!                                        handle_connection_io(..))
//!                                                         |
//!   socket <--write-- writer thread <--framed bytes-- ChannelWriter
//! ```
//!
//! # The seam is the byte source, not the frame pipeline
//!
//! `handle_connection_io` already takes its reader and writer as
//! `Box<dyn AsyncRead/AsyncWrite>` (§1.2). So this driver adds *implementors*,
//! not a second protocol: every byte still reaches the same parser, the same
//! `select!`, the same handlers. Nothing in the 21,000-line protocol module is
//! `cfg`-ed, and the hosted driver is not touched.
//!
//! Crucially the threads hand over **bytes, not frames** (§1.3). The inbound
//! type cache, the segment buffer and the channel map stay exactly where they
//! are — inside the one loop that dispatches frames in wire order — so a
//! client that defines a descriptor with `0xFD <slot>` in one frame and
//! references it with `0xFE <slot>` in a later one still resolves. That
//! invariant holds *by construction* here rather than by a new protocol
//! between two threads.
//!
//! # Where the async goes on each target
//!
//! `block_on_sync` (`epics_base_rs::runtime::task`) is the single bridge. On a
//! bare thread it parks; on a multi-thread runtime worker it hands the worker
//! off first. That difference matters, because on a **hosted** build the
//! operation future still needs a tokio runtime underneath it: `runtime::task`
//! aliases `spawn` and `interval` to tokio's, and tokio's need a reactor. So
//! [`serve_connection_blocking`] must be called from a multi-thread runtime
//! worker on the host. On RTEMS the `exec_backend` supplies both the spawn
//! pool and the timer, so the very same call runs on a bare thread. This
//! module is therefore host-compiled and host-tested — the only way to show
//! hosted behaviour is unchanged — and the reader/writer threads and both
//! adapters are exercised for real either way.
//!
//! # Not here
//!
//! Server-wide shutdown (a socket registry walked by `PvaServer::stop`) and
//! the writer-exit `oneshot` arm are stage 4 (§4.2b, §4.3). This module tears
//! down only its own connection, and does it without either.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::block_on_sync;
use tokio::io::ReadBuf;
use tokio::sync::mpsc;
use tracing::debug;

use super::config::PvaServerConfig;
use super::source::DynSource;
use super::tcp::{ConnInit, handle_connection_io};
use crate::error::{PvaError, PvaResult};

/// Depth of the reader → operation byte-chunk channel.
///
/// **One** (§1.4, resolved). The hosted read is strictly demand-driven —
/// `read_frame` issues one read per poll and the loop dispatches a frame fully
/// before reading again — so a depth of 1 reproduces it with at most one chunk
/// of read-ahead, which the kernel receive buffer already provides. A larger
/// depth would let a fast client queue chunks while a slow source blocks the
/// dispatcher: a behaviour change, not an optimisation.
const CHUNK_QUEUE_DEPTH: usize = 1;

/// Depth of the operation → writer frame channel. Same reasoning: the producer
/// (the connection's writer task) emits one frame at a time and waits for it,
/// so depth 1 gives the task the same backpressure a blocking socket write
/// would.
const FRAME_QUEUE_DEPTH: usize = 1;

/// One blocking read, sized to match `read_frame`'s own chunk so the byte
/// arrival pattern is the hosted one.
const READ_CHUNK: usize = 4096;

/// How many SO_SNDTIMEO ticks fit inside one send deadline. The socket
/// timeout only exists to return control to the deadline loop; the deadline
/// is the real bound (§3.3).
const SEND_TICKS_PER_DEADLINE: u32 = 4;

/// A blocking socket op hit its `SO_RCVTIMEO`/`SO_SNDTIMEO`.
///
/// Same classification CA's blocking driver uses (`epics-ca-rs`
/// `server/blocking.rs`, `is_read_timeout`): Unix reports the expiry as
/// `WouldBlock`, some platforms as `TimedOut`.
fn is_socket_timeout(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

// ---------------------------------------------------------------------------
// Reader side
// ---------------------------------------------------------------------------

/// `AsyncRead` over a channel of byte chunks — the blocking driver's stand-in
/// for the tokio socket read half.
///
/// **Cancel-safety** is the whole point of the `cur`/`pos` pair and is why
/// this type exists at all rather than a channel being read inline. The
/// hosted `read_frame` is used directly as a `select!` arm and survives losing
/// that race because its accumulated bytes live *outside* it. This adapter has
/// the same property: a chunk leaves the channel only when `poll_recv` returns
/// `Ready`, and a partially-copied chunk stays in `cur`/`pos` across as many
/// dropped `poll_read` futures as the caller likes. A lost race consumes
/// nothing.
pub(super) struct ChannelReader {
    rx: mpsc::Receiver<Vec<u8>>,
    /// The chunk currently being handed out, and how much of it has gone.
    cur: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            cur: Vec::new(),
            pos: 0,
        }
    }
}

impl tokio::io::AsyncRead for ChannelReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // No room offered: report "nothing filled" without taking anything
        // out of the channel. Consuming here would be the one way this
        // adapter could lose bytes.
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let me = &mut *self;
        loop {
            if me.pos < me.cur.len() {
                let n = (me.cur.len() - me.pos).min(buf.remaining());
                buf.put_slice(&me.cur[me.pos..me.pos + n]);
                me.pos += n;
                if me.pos == me.cur.len() {
                    me.cur.clear();
                    me.pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            match me.rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    // An empty chunk is not an EOF marker; skip it rather
                    // than letting it read as one.
                    if chunk.is_empty() {
                        continue;
                    }
                    me.cur = chunk;
                    me.pos = 0;
                }
                // Every sender gone = the reader thread ended (EOF, read
                // error, or RCVTIMEO). Zero bytes filled is what `read_frame`
                // turns into `Protocol("client closed")` — the existing hosted
                // EOF path, unchanged (§4.2a).
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Blocking read loop. Ends on EOF, read error, or `SO_RCVTIMEO`; dropping
/// `tx` on the way out is the EOF signal to the operation thread.
fn reader_thread(mut sock: TcpStream, tx: mpsc::Sender<Vec<u8>>, peer: SocketAddr) {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if is_socket_timeout(e.kind()) => {
                debug!(?peer, "blocking reader: receive timeout, ending connection");
                break;
            }
            Err(e) => {
                debug!(?peer, error = %e, "blocking reader: read failed");
                break;
            }
        };
        // The house sync-over-async primitive: parks this thread (no runtime
        // entered) or hands the worker off (hosted). NOT `blocking_send`,
        // which panics inside a runtime context and would make this same
        // file unusable from a hosted worker.
        if !matches!(block_on_sync(tx.send(chunk[..n].to_vec())), Ok(Ok(()))) {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Writer side
// ---------------------------------------------------------------------------

/// Wake slot for a `poll_write` that found the frame channel full. The writer
/// thread wakes it after each frame it takes, which is the moment room
/// appears.
#[derive(Default)]
struct WriteRoom {
    waker: Mutex<Option<Waker>>,
}

impl WriteRoom {
    fn park(&self, cx: &Context<'_>) {
        *self.waker.lock().expect("write-room waker poisoned") = Some(cx.waker().clone());
    }

    fn wake(&self) {
        let waker = self.waker.lock().expect("write-room waker poisoned").take();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// `AsyncWrite` over a channel of frames — the blocking driver's stand-in for
/// the tokio socket write half.
///
/// Holds a [`mpsc::WeakSender`], never a strong one, and that is load-bearing
/// rather than tidiness: this adapter is owned by the connection's writer
/// task, which is aborted (not joined) when the connection ends, so the moment
/// its last strong sender drops is not a moment the driver controls. With only
/// a weak handle here, the driver's own sender is the sole thing keeping the
/// channel open, and dropping it ends the writer thread deterministically
/// instead of whenever the runtime gets round to reaping an aborted task.
pub(super) struct ChannelWriter {
    tx: mpsc::WeakSender<Vec<u8>>,
    room: Arc<WriteRoom>,
}

impl ChannelWriter {
    fn new(tx: mpsc::WeakSender<Vec<u8>>, room: Arc<WriteRoom>) -> Self {
        Self { tx, room }
    }
}

fn write_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "PVA writer thread has ended")
}

impl tokio::io::AsyncWrite for ChannelWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let Some(tx) = self.tx.upgrade() else {
            return Poll::Ready(Err(write_closed()));
        };
        // Register interest BEFORE trying, so a take that happens between the
        // try and the return cannot be missed: either `try_send` sees the room
        // that take created, or the take's `wake()` finds this waker.
        self.room.park(cx);
        match tx.try_send(buf.to_vec()) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Full(_)) => Poll::Pending,
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(write_closed())),
        }
        // `tx` drops here. Nothing in this adapter holds a strong sender
        // across a suspension, which is what makes the driver's drop decisive.
    }

    /// Frames are flushed by the writer thread as it takes them; there is no
    /// buffer here to push.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Write one whole frame under a **deadline**, not merely a per-syscall
/// timeout (§3.3).
///
/// The hosted writer bounds `write_all(&frame)` as a unit. Plain SO_SNDTIMEO
/// bounds each `write` syscall instead, so a client that accepts one byte per
/// tick never trips it and holds the writer thread indefinitely — the exact
/// stuck-client hazard the hosted timeout exists to prevent, on a resource (an
/// OS thread) that is scarcer on RTEMS than a task is on the host. The socket
/// timeout here is only what returns control to this loop; the deadline is the
/// bound.
///
/// A partial write on expiry needs no repair: the caller ends the writer and
/// the connection is torn down, so nothing is ever written to this socket
/// again.
fn write_frame_deadline(
    sock: &mut TcpStream,
    frame: &[u8],
    send_timeout: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + send_timeout;
    let mut off = 0;
    while off < frame.len() {
        // Checked at the top so every way round the loop is bounded,
        // including an `Interrupted` storm.
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "send deadline expired with the frame incomplete",
            ));
        }
        match sock.write(&frame[off..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "peer accepted no bytes",
                ));
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // A tick with no progress: fall through to the deadline check.
            Err(e) if is_socket_timeout(e.kind()) => {}
            Err(e) => return Err(e),
        }
    }
    sock.flush()
}

/// Drain frames to the socket in order. Ends when the driver drops the last
/// strong sender, or on the first write error / send-deadline expiry.
fn writer_thread(
    mut sock: TcpStream,
    mut rx: mpsc::Receiver<Vec<u8>>,
    room: Arc<WriteRoom>,
    send_timeout: Duration,
    peer: SocketAddr,
) {
    // `Ok(None)` = the driver let go of its sender; `Err(_)` = this thread
    // cannot block here at all. Both end the writer.
    while let Ok(Some(frame)) = block_on_sync(rx.recv()) {
        // A slot just opened; let a parked `poll_write` retry.
        room.wake();
        if let Err(e) = write_frame_deadline(&mut sock, &frame, send_timeout) {
            debug!(?peer, error = %e, "blocking writer: send failed, ending connection");
            break;
        }
    }
    // Whatever parked the producer, it must not stay parked on a dead writer.
    room.wake();
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Serve one PVA connection over a blocking [`TcpStream`], on this thread.
///
/// This call *is* the operation thread: it runs `handle_connection_io` to
/// completion and returns the connection's result, having joined both child
/// threads. The reader and writer are its children and neither decides the
/// connection is over — they can only report (§4.1).
///
/// `peer_entry` is this connection's row in the server report (`pvxsr`) and
/// `channel_invalidator` is the server-wide out-of-band disconnect channel;
/// both come from whatever accepts the socket. There is no TLS parameter: the
/// blocking driver is plain-TCP only (§6), so the connection carries no x509
/// identity and authenticates through CONNECTION_VALIDATION alone.
///
/// **Hosted callers must run this on a multi-thread runtime worker**; see the
/// module docs. On RTEMS a bare thread is correct.
pub fn serve_connection_blocking(
    stream: TcpStream,
    peer: SocketAddr,
    source: DynSource,
    config: PvaServerConfig,
    peer_entry: Arc<super::peers::PeerEntry>,
    channel_invalidator: super::source::ChannelInvalidator,
) -> PvaResult<()> {
    let init = ConnInit {
        peer_entry,
        x509_identity: None,
        channel_invalidator,
    };
    let _ = stream.set_nodelay(true);
    // Portable SO_RCVTIMEO / SO_SNDTIMEO (both valid on RTEMS), the same move
    // CA's blocking driver makes. Note the receive timeout is NOT a shutdown
    // mechanism here: `op_timeout` defaults to ~64,000 s, so it is effectively
    // infinite (§1.6). What ends a parked reader is the `shutdown` below.
    stream
        .set_read_timeout(Some(config.op_timeout))
        .map_err(PvaError::Io)?;
    let send_timeout = config.send_timeout;
    let send_tick = (send_timeout / SEND_TICKS_PER_DEADLINE).max(Duration::from_millis(1));
    stream
        .set_write_timeout(Some(send_tick))
        .map_err(PvaError::Io)?;

    // One socket, three handles: this thread keeps the original for teardown,
    // each child thread gets a dup'd fd.
    let read_sock = stream.try_clone().map_err(PvaError::Io)?;
    let write_sock = stream.try_clone().map_err(PvaError::Io)?;

    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>(CHUNK_QUEUE_DEPTH);
    let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
    let room = Arc::new(WriteRoom::default());
    let writer_adapter = ChannelWriter::new(frame_tx.downgrade(), room.clone());

    let reader = thread::Builder::new()
        .name(format!("PVA-reader {peer}"))
        .spawn(move || reader_thread(read_sock, chunk_tx, peer))
        .map_err(PvaError::Io)?;
    let writer_room = room.clone();
    let writer = thread::Builder::new()
        .name(format!("PVA-writer {peer}"))
        .spawn(move || writer_thread(write_sock, frame_rx, writer_room, send_timeout, peer))
        .map_err(PvaError::Io)?;

    let outcome = block_on_sync(handle_connection_io(
        source,
        Box::new(ChannelReader::new(chunk_rx)),
        Box::new(writer_adapter),
        peer,
        config,
        init,
    ));

    // Teardown, in §4.3's order and for its reason: the writer goes down
    // first so frames the connection emitted on its way out (MONITOR FINISH,
    // DESTROY_CHANNEL) reach the wire before the socket is torn down.
    //
    // Dropping this sender is decisive because it is the only strong one —
    // the adapter holds a weak handle. The writer drains what is queued, then
    // sees `None` and exits.
    drop(frame_tx);
    let _ = writer.join();
    // Now the reader. Its `read` is parked with an effectively-infinite
    // SO_RCVTIMEO, so the socket has to be shut to return it (§4.2c's
    // mechanism, applied to our own fd — the server-wide registry that does
    // this from outside is stage 4).
    let _ = stream.shutdown(Shutdown::Both);
    let _ = reader.join();
    // `room` outlives both, so a producer parked on a full queue is released
    // by the writer's exit wake rather than left holding a dead waker.
    drop(room);

    match outcome {
        Ok(result) => result,
        Err(_) => Err(PvaError::Protocol(
            "blocking PVA driver cannot run on a current-thread runtime".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ByteOrder, Command, PvaHeader, ReadExt, WriteExt, encode_string_into};
    use crate::pvdata::encode::encode_type_desc;
    use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};
    use crate::server_native::peers::PeerEntry;
    use crate::server_native::source::ChannelInvalidator;
    use crate::server_native::{SharedPV, SharedSource};
    use std::io::Cursor;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;

    /// Production scope of a source file: everything before the first
    /// column-0 `#[cfg(test)]`. Same helper the accept-driver guard uses.
    fn production_scope(src: &str) -> &str {
        match src.find("\n#[cfg(test)]") {
            Some(i) => &src[..i],
            None => src,
        }
    }

    /// The RTEMS constraint this whole driver exists to satisfy: it must not
    /// reach for tokio's async net/timer/spawn machinery, none of which builds
    /// for `armv7-rtems-eabihf`, and it must not suspend a future directly —
    /// every await goes through `block_on_sync`. `tokio::sync` and
    /// `tokio::io`'s traits ARE allowed and are what the two adapters are
    /// built from. Modelled on the CA blocking driver's guard
    /// (`epics-ca-rs` `server/blocking.rs`), and scoped to the production
    /// slice so the tests below may use the async machinery freely.
    #[test]
    fn blocking_driver_has_no_async_runtime_symbols() {
        let prod = production_scope(include_str!("blocking.rs"));
        // Fail closed: if the driver is no longer in the slice, the slice is
        // wrong and every assertion below would pass vacuously.
        assert!(
            prod.contains("fn serve_connection_blocking"),
            "production slice no longer covers the driver"
        );
        // Assembled with `concat!` so the forbidden literals never appear
        // contiguously in this file — otherwise this test body would match
        // itself under `include_str!`.
        let forbidden = [
            concat!("tokio", "::net"),
            concat!("tokio", "::time"),
            concat!("tokio", "::", "spawn"),
            concat!("block", "_in_place"),
            concat!(".", "await"),
        ];
        for token in forbidden {
            assert_eq!(
                prod.matches(token).count(),
                0,
                "the blocking PVA driver must not reference `{token}`: it has no async \
                 net/timer/spawn on RTEMS, and every await goes through `block_on_sync`"
            );
        }
    }

    // ── adapter: cancel-safety ──────────────────────────────────────────

    /// §1.5's claim, which the design doc flags as the one thing it reasoned
    /// about but did not execute: losing a `select!` race must consume
    /// nothing. `read_frame` is used directly as a `select!` arm, so if this
    /// adapter dropped bytes on a lost race the failure would be silent and
    /// intermittent — a truncated frame long after the fact.
    ///
    /// Both boundaries of "what was in flight when the race was lost":
    ///
    /// * **mid-chunk** — part of a chunk has been handed out and the rest is
    ///   parked in `cur`/`pos`;
    /// * **pending** — no chunk has arrived at all, so the poll registered a
    ///   waker and returned `Pending`.
    #[tokio::test]
    async fn channel_reader_loses_no_bytes_when_a_select_race_is_lost() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(CHUNK_QUEUE_DEPTH);
        let mut reader = ChannelReader::new(rx);

        // Boundary 1: a partially-consumed chunk survives.
        tx.send(b"ABCDEFGH".to_vec()).await.expect("chunk queued");
        let mut small = [0u8; 3];
        let n = reader.read(&mut small).await.expect("first read");
        assert_eq!(&small[..n], b"ABC");
        for _ in 0..4 {
            let mut buf = [0u8; 8];
            tokio::select! {
                biased;
                // This arm always wins, so the read future below is created
                // and dropped without ever completing.
                _ = std::future::ready(()) => {}
                _ = reader.read(&mut buf) => unreachable!("the ready arm wins under `biased`"),
            }
        }
        let mut rest = [0u8; 8];
        let n = reader.read(&mut rest).await.expect("read after lost races");
        assert_eq!(
            &rest[..n],
            b"DEFGH",
            "a lost race must not eat the parked tail of the chunk"
        );

        // Boundary 2: a poll that returned Pending consumed nothing either.
        for _ in 0..4 {
            let mut buf = [0u8; 8];
            tokio::select! {
                biased;
                _ = std::future::ready(()) => {}
                _ = reader.read(&mut buf) => unreachable!("the ready arm wins under `biased`"),
            }
        }
        tx.send(b"IJKL".to_vec())
            .await
            .expect("second chunk queued");
        let mut after = [0u8; 8];
        let n = reader
            .read(&mut after)
            .await
            .expect("read after pending races");
        assert_eq!(
            &after[..n],
            b"IJKL",
            "a chunk must not be taken out of the channel by a poll that returned Pending"
        );

        // And EOF still reads as EOF once every sender is gone.
        drop(tx);
        let mut eof = [0u8; 8];
        assert_eq!(
            reader.read(&mut eof).await.expect("eof read"),
            0,
            "all senders dropped must surface as a zero-length read"
        );
    }

    // ── end-to-end over a real loopback socket ──────────────────────────

    fn scalar_intro() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        }
    }

    fn scalar_value(v: f64) -> PvField {
        PvField::Structure(PvStructure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), PvField::Scalar(ScalarValue::Double(v)))],
        })
    }

    fn test_source() -> DynSource {
        let pv = SharedPV::new();
        pv.open(scalar_intro(), scalar_value(1.5)).expect("open pv");
        let shared = SharedSource::new();
        shared.add("dut", pv);
        Arc::new(shared)
    }

    /// A connection served by the blocking driver, plus the client's end of
    /// the socket. Dropping the client end ends the connection.
    struct Harness {
        client: TcpStream,
        conn: Option<tokio::task::JoinHandle<PvaResult<()>>>,
    }

    impl Harness {
        /// Bind loopback on an ephemeral port (never 5075), connect, and hand
        /// the accepted socket to the blocking driver.
        ///
        /// The driver runs inside a spawned task rather than a bare thread
        /// because on a hosted build the connection future needs tokio's
        /// timer and spawner underneath it (module docs); `block_on_sync`
        /// then takes its worker-handoff arm, which is the arm a hosted
        /// caller is supposed to take.
        fn start(config: PvaServerConfig) -> Harness {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
            let addr = listener.local_addr().expect("local addr");
            let client = TcpStream::connect(addr).expect("connect");
            let (server_sock, peer) = listener.accept().expect("accept");
            let source = test_source();
            let conn = tokio::spawn(async move {
                serve_connection_blocking(
                    server_sock,
                    peer,
                    source,
                    config,
                    PeerEntry::new(false),
                    ChannelInvalidator::new(),
                )
            });
            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("client read timeout");
            Harness {
                client,
                conn: Some(conn),
            }
        }

        fn send(&mut self, bytes: &[u8]) {
            self.client.write_all(bytes).expect("client write");
        }

        /// Read exactly one PVA frame (header + declared body).
        fn read_frame(&mut self) -> (PvaHeader, Vec<u8>) {
            let mut head = [0u8; PvaHeader::SIZE];
            self.client.read_exact(&mut head).expect("frame header");
            let header =
                PvaHeader::decode(&mut Cursor::new(&head[..])).expect("decode frame header");
            let mut body = vec![0u8; header.payload_length as usize];
            if !body.is_empty() {
                self.client.read_exact(&mut body).expect("frame body");
            }
            (header, body)
        }

        /// Read frames until one carries `command` as an application message.
        fn read_until(&mut self, command: Command) -> Vec<u8> {
            for _ in 0..32 {
                let (header, body) = self.read_frame();
                if !header.flags.is_control() && header.command == command.code() {
                    return body;
                }
            }
            panic!("no {command:?} frame arrived within 32 frames");
        }

        /// Close the client end and collect the connection's result.
        async fn finish(mut self) -> PvaResult<()> {
            let _ = self.client.shutdown(Shutdown::Both);
            self.conn
                .take()
                .expect("connection task present")
                .await
                .expect("connection task joined")
        }
    }

    fn isolated_config() -> PvaServerConfig {
        PvaServerConfig {
            wire_byte_order: ByteOrder::Little,
            ..PvaServerConfig::isolated()
        }
    }

    fn app_frame(command: Command, order: ByteOrder, payload: Vec<u8>) -> Vec<u8> {
        // Client → server: the Server direction bit stays clear.
        let header = PvaHeader::application(false, order, command.code(), payload.len() as u32);
        let mut out = Vec::new();
        header.write_into(&mut out);
        out.extend_from_slice(&payload);
        out
    }

    fn create_channel_payload(cid: u32, name: &str, order: ByteOrder) -> Vec<u8> {
        let mut payload = Vec::new();
        // `count` is a plain u16, not a Size (pvxs serverchan.cpp:269).
        payload.put_u16(1, order);
        payload.put_u32(cid, order);
        encode_string_into(name, order, &mut payload);
        payload
    }

    /// The header and the body each arrive split across several TCP segments,
    /// with a pause in between so the kernel really does deliver them as
    /// separate chunks. The chunk boundaries land inside the 8-byte header and
    /// inside the payload — the two places a byte-level seam could corrupt a
    /// frame — and the server must still dispatch one whole CREATE_CHANNEL.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_header_and_partial_body_frames_reassemble() {
        let order = ByteOrder::Little;
        let mut h = Harness::start(isolated_config());

        let frame = app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(7, "dut", order),
        );
        // 3 bytes of header, then 5 more (completing it), then the body in
        // two pieces.
        let split = [3usize, PvaHeader::SIZE, PvaHeader::SIZE + 2, frame.len()];
        let mut from = 0;
        for to in split {
            h.send(&frame[from..to]);
            from = to;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let body = h.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        assert_eq!(cur.get_u32(order).expect("cid"), 7, "cid echoed");
        let _sid = cur.get_u32(order).expect("sid");
        let status = cur.get_u8().expect("status");
        assert_eq!(status, 0xFF, "CREATE_CHANNEL must succeed (status OK)");

        let _ = h.finish().await;
    }

    /// A segmented application message whose FIRST and LAST segments are
    /// written in separate TCP chunks. Segment reassembly lives in the
    /// operation loop's `seg_buf`, on the far side of the thread boundary —
    /// this proves handing over *bytes* rather than frames left it intact.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn segmented_message_reassembles_across_a_chunk_boundary() {
        let order = ByteOrder::Little;
        let mut h = Harness::start(isolated_config());

        let payload = create_channel_payload(11, "dut", order);
        let cut = payload.len() / 2;
        // Segment flags live in the header's flag byte: bit 4 = FIRST,
        // bit 5 = LAST (an unsegmented frame has neither).
        let mut first =
            PvaHeader::application(false, order, Command::CreateChannel.code(), cut as u32);
        first.flags.0 |= 0x10;
        let mut last = PvaHeader::application(
            false,
            order,
            Command::CreateChannel.code(),
            (payload.len() - cut) as u32,
        );
        last.flags.0 |= 0x20;

        let mut chunk_a = Vec::new();
        first.write_into(&mut chunk_a);
        chunk_a.extend_from_slice(&payload[..cut]);
        h.send(&chunk_a);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut chunk_b = Vec::new();
        last.write_into(&mut chunk_b);
        chunk_b.extend_from_slice(&payload[cut..]);
        h.send(&chunk_b);

        let body = h.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        assert_eq!(cur.get_u32(order).expect("cid"), 11, "cid echoed");
        let _sid = cur.get_u32(order).expect("sid");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "a segmented CREATE_CHANNEL must reassemble and succeed"
        );

        let _ = h.finish().await;
    }

    /// The TypeCache invariant (§1.3), end to end through the blocking driver.
    ///
    /// A client may define a descriptor with `0xFD <slot> <desc>` in one frame
    /// and reference it with `0xFE <slot>` in a **different** frame on the
    /// same connection; pvxs keeps one connection-scoped `rxRegistry`
    /// (`conn.h:23`). This is the invariant that decided the seam: had the
    /// reader thread parsed frames and shipped them across a channel, the
    /// cache would have needed a protocol between two threads. Handing bytes
    /// keeps it a plain local of the one loop that dispatches in wire order —
    /// and this test is what proves the thread boundary did not disturb it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn type_cache_define_and_reference_survive_in_different_frames() {
        let order = ByteOrder::Little;
        let mut h = Harness::start(isolated_config());

        h.send(&app_frame(
            Command::CreateChannel,
            order,
            create_channel_payload(1, "dut", order),
        ));
        let body = h.read_until(Command::CreateChannel);
        let mut cur = Cursor::new(&body[..]);
        let _cid = cur.get_u32(order).expect("cid");
        let sid = cur.get_u32(order).expect("sid");
        assert_eq!(cur.get_u8().expect("status"), 0xFF, "channel created");

        // The empty pvRequest structure: a real, cacheable descriptor whose
        // only job here is to be defined once and referenced later.
        let req_desc = FieldDesc::Structure {
            struct_id: String::new(),
            fields: Vec::new(),
        };
        const SLOT: u16 = 0x0001;
        const INIT: u8 = 0x08;

        // GET INIT #1 — pvRequest carried as `0xFD <slot> <inline desc>`.
        let mut define = Vec::new();
        define.put_u32(sid, order);
        define.put_u32(801, order);
        define.put_u8(INIT);
        define.put_u8(0xFD);
        define.put_u16(SLOT, order);
        encode_type_desc(&req_desc, order, &mut define);
        h.send(&app_frame(Command::Get, order, define));
        let reply1 = h.read_until(Command::Get);
        let mut cur = Cursor::new(&reply1[..]);
        assert_eq!(cur.get_u32(order).expect("ioid"), 801);
        let _sub = cur.get_u8().expect("subcommand");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "the defining GET INIT must succeed"
        );

        // GET INIT #2 — a bare `0xFE <slot>` reference, in its own frame,
        // resolved only by the connection-scoped cache the first frame filled.
        let mut reference = Vec::new();
        reference.put_u32(sid, order);
        reference.put_u32(802, order);
        reference.put_u8(INIT);
        reference.put_u8(0xFE);
        reference.put_u16(SLOT, order);
        h.send(&app_frame(Command::Get, order, reference));
        let reply2 = h.read_until(Command::Get);
        let mut cur = Cursor::new(&reply2[..]);
        assert_eq!(cur.get_u32(order).expect("ioid"), 802);
        let _sub = cur.get_u8().expect("subcommand");
        assert_eq!(
            cur.get_u8().expect("status"),
            0xFF,
            "a 0xFE reference in a later frame must resolve against the cache the \
             earlier frame filled — the invariant the byte-level seam preserves"
        );

        let _ = h.finish().await;
    }

    /// The hosted precondition, stated as a boundary rather than left to the
    /// module docs: a **current-thread** runtime cannot host this driver,
    /// because parking its only thread would stop the very tasks that must
    /// wake the connection. `block_on_sync` reports that instead of
    /// deadlocking, and the driver must surface it as an error and tear down
    /// rather than pass it off as a normal connection close.
    #[tokio::test]
    async fn a_current_thread_runtime_is_refused_rather_than_deadlocked() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let _client = TcpStream::connect(addr).expect("connect");
        let (server_sock, peer) = listener.accept().expect("accept");

        // `#[tokio::test]` without a flavor IS a current-thread runtime, so
        // this call is already on the thread the guard is about.
        let err = serve_connection_blocking(
            server_sock,
            peer,
            test_source(),
            isolated_config(),
            PeerEntry::new(false),
            ChannelInvalidator::new(),
        )
        .expect_err("a current-thread runtime must be refused");
        match err {
            PvaError::Protocol(msg) => assert!(
                msg.contains("current-thread"),
                "the error must name the reason, got: {msg}"
            ),
            other => panic!("expected a Protocol error naming the runtime, got {other:?}"),
        }
    }

    // ── writer: the deadline loop ───────────────────────────────────────

    /// The property §3.3 exists for, stated as the case bare SO_SNDTIMEO gets
    /// wrong.
    ///
    /// The peer here is not silent — it accepts a trickle of bytes, one small
    /// read per socket-timeout tick, forever. Under plain SO_SNDTIMEO every
    /// `write` syscall makes progress, so no timeout ever fires and the writer
    /// thread is held indefinitely. The deadline loop bounds the *frame*, so
    /// it gives up at `send_timeout` regardless of the dribble.
    ///
    /// Mutation-checked: delete the loop's top-of-loop deadline check, leaving
    /// only the socket timeout, and this test never returns (the run was
    /// killed at 120 s). That is the whole argument of §3.3, executed.
    #[test]
    fn writer_deadline_loop_ends_a_trickling_client() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");

        const SEND_TIMEOUT: Duration = Duration::from_millis(400);
        let tick = SEND_TIMEOUT / SEND_TICKS_PER_DEADLINE;
        server.set_write_timeout(Some(tick)).expect("SO_SNDTIMEO");
        // Small enough that a multi-MiB frame cannot be absorbed by the
        // kernel buffers, so the writer really does have to wait on the peer.
        let _ = server.set_nodelay(true);

        let read_total = Arc::new(AtomicUsize::new(0));
        let counted = read_total.clone();
        // The trickle keeps a whole 8 MiB frame's worth of bytes in flight, so
        // it must be told to stop rather than left to drain the socket a byte
        // at a time once the writer has given up.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = stop.clone();
        let trickle = thread::spawn(move || {
            // One byte per tick: always progress, never a stall long enough
            // for SO_SNDTIMEO to fire on its own.
            let mut byte = [0u8; 1];
            while !stop_reader.load(Ordering::Relaxed) {
                match client.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        counted.fetch_add(n, Ordering::Relaxed);
                    }
                }
                thread::sleep(tick / 2);
            }
        });

        let frame = vec![0xA5u8; 8 * 1024 * 1024];
        let started = Instant::now();
        let err = write_frame_deadline(&mut server, &frame, SEND_TIMEOUT)
            .expect_err("a trickling peer must not be allowed to hold the writer");
        let elapsed = started.elapsed();

        assert_eq!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "the frame deadline is what fires, not a socket error"
        );
        assert!(
            elapsed < SEND_TIMEOUT * 5,
            "the deadline must bound the whole frame; took {elapsed:?} for a \
             {SEND_TIMEOUT:?} deadline"
        );
        assert!(
            elapsed >= SEND_TIMEOUT,
            "the writer must not give up before its deadline; took {elapsed:?}"
        );
        assert!(
            read_total.load(Ordering::Relaxed) > 0,
            "the peer really was accepting bytes — this is the trickle case, \
             not a dead-socket case"
        );

        stop.store(true, Ordering::Relaxed);
        drop(server);
        let _ = trickle.join();
    }

    /// A complete frame to a peer that reads normally still goes out, and the
    /// deadline does not fire. The negative control for the test above: a
    /// deadline loop that failed everything would pass that one.
    #[test]
    fn writer_deadline_loop_delivers_a_frame_to_a_reading_client() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let mut client = TcpStream::connect(addr).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");
        server
            .set_write_timeout(Some(Duration::from_millis(50)))
            .expect("SO_SNDTIMEO");

        let frame: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
        let expected = frame.clone();
        let echo = thread::spawn(move || {
            let mut got = vec![0u8; expected.len()];
            client.read_exact(&mut got).expect("client reads the frame");
            assert_eq!(got, expected, "bytes arrive intact and in order");
        });

        write_frame_deadline(&mut server, &frame, Duration::from_secs(5))
            .expect("a reading peer takes the frame well inside the deadline");
        echo.join().expect("client thread");
    }

    // ── writer adapter backpressure ─────────────────────────────────────

    /// `ChannelWriter` holds only a weak sender, so the driver's own sender is
    /// what keeps the frame channel alive. That is what makes teardown
    /// deterministic rather than dependent on when an aborted writer task is
    /// reaped, and it is worth pinning: a future edit that stores a strong
    /// `Sender` here would still pass every other test in this file while
    /// reintroducing a teardown that can hang.
    #[tokio::test]
    async fn channel_writer_does_not_keep_the_frame_channel_open() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
        let room = Arc::new(WriteRoom::default());
        let mut writer = ChannelWriter::new(tx.downgrade(), room.clone());

        {
            use tokio::io::AsyncWriteExt;
            writer
                .write_all(b"frame")
                .await
                .expect("first frame queued");
        }
        assert_eq!(rx.recv().await.as_deref(), Some(&b"frame"[..]));

        // The adapter is still alive; the driver lets go of its sender.
        drop(tx);
        assert!(
            rx.recv().await.is_none(),
            "a live ChannelWriter must not keep the channel open once the driver \
             drops its sender — teardown depends on this"
        );
        // And the adapter reports the closure rather than parking forever.
        {
            use tokio::io::AsyncWriteExt;
            let err = writer
                .write_all(b"late")
                .await
                .expect_err("writing after the channel closed must fail");
            assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        }
    }
}
